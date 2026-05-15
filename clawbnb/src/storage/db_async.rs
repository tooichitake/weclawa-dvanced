//! sqlx async Postgres pool — v5.6 PG-only.
//!
//! ## v5.6 architecture
//!
//! - Single backend: **Postgres** (13+).
//! - Connection via env `WECLAWBOT_PG_URL` (preferred) or `DATABASE_URL`.
//! - All `?` placeholders eliminated v5.6 → `$1, $2, ...` native PG.
//! - Migrations are PG-native (no SQLite `AUTOINCREMENT`, `PRAGMA`,
//!   `VACUUM INTO`, `INSERT OR IGNORE` — all gone).
//!
//! ## Migration policy
//!
//! `apply_migrations()` is idempotent. Tracks applied versions in the
//! `_sqlx_migrations` table. Brand-new DBs run all V*.sql files in
//! order. Existing PG DBs from prior daemon runs skip already-applied
//! ones. Legacy SQLite users run `weclawbot import-sqlite ./state.db`
//! once to copy their data into a fresh PG.
//!
//! ## Why drop SQLite
//!
//! Dual-backend complexity caused real bugs:
//! - v4→v5: `apply_migrations` "refinery compat" path incorrectly
//!   marked V0009 as applied → accounts table missing `platform_id`
//!   → daemon couldn't enumerate accounts → WeChat clients saw
//!   "暂无法连接 openclaw".
//! - sqlx 0.8 PG driver doesn't auto-translate `?` → `$N` (despite
//!   common belief). The 76 callsites had to be either changed twice
//!   (once for each backend) or wrapped in a macro.
//! - SQLite-only SQL (`AUTOINCREMENT`, `VACUUM INTO`, `last_insert_rowid()`,
//!   PRAGMAs) needed cfg-gated branches everywhere.
//!
//! PG-only removes ~400 lines of cfg complexity, kills a class of
//! migration bugs, and aligns dev/prod with the same engine.

use std::sync::OnceLock;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;

use crate::storage::db::DbError;

/// The one and only async pool type. Was previously cfg-gated between
/// SqlitePool / PgPool; v5.6 settled on PG.
pub type AsyncDbPool = sqlx::PgPool;

/// Open the daemon's main pool. Reads env `WECLAWBOT_PG_URL` (preferred,
/// weclawbot-specific) or `DATABASE_URL` (sqlx convention) for the DSN.
pub async fn open_default() -> Result<AsyncDbPool, DbError> {
    let url = std::env::var("WECLAWBOT_PG_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .map_err(|_| {
            DbError::Pool(
                "neither WECLAWBOT_PG_URL nor DATABASE_URL is set; \
                 weclawbot daemon requires a Postgres DSN. Example:\n  \
                 export WECLAWBOT_PG_URL='postgres://weclawbot:pw@127.0.0.1:5432/weclawbot'"
                    .into(),
            )
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&url)
        .await
        .map_err(|e| DbError::Pool(e.to_string()))?;
    apply_migrations(&pool).await?;
    Ok(pool)
}

/// Test-only: open at an explicit DSN. Used by integration tests that
/// spin up a `postgres:16` testcontainer. NOT for production — prod
/// uses `open_default()` so credentials come from env.
#[cfg(test)]
pub async fn open_at_url(url: &str) -> Result<AsyncDbPool, DbError> {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .map_err(|e| DbError::Pool(e.to_string()))?;
    apply_migrations(&pool).await?;
    Ok(pool)
}

/// v5.6 PG-native migration runner. Idempotent. Tracks applied versions
/// in `_sqlx_migrations`; runs each file in order; skips already-applied
/// migrations.
///
/// Refinery / `pragma_table_info` / `portable_ddl` self-heal — all gone.
/// PG-native DDL is portable as-is.
pub async fn apply_migrations(pool: &AsyncDbPool) -> Result<(), DbError> {
    use sqlx::Executor;
    pool.execute(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version    TEXT PRIMARY KEY,
            applied_at TEXT NOT NULL
         )",
    )
    .await
    .map_err(|e| DbError::Migration(format!("create _sqlx_migrations: {e}")))?;

    for (name, sql) in MIGRATION_FILES {
        let already: Option<(String,)> =
            sqlx::query_as("SELECT version FROM _sqlx_migrations WHERE version = $1")
                .bind(*name)
                .fetch_optional(pool)
                .await
                .map_err(|e| DbError::Migration(format!("check {name}: {e}")))?;
        if already.is_some() {
            continue;
        }
        pool.execute(*sql)
            .await
            .map_err(|e| DbError::Migration(format!("apply {name}: {e}")))?;
        sqlx::query("INSERT INTO _sqlx_migrations (version, applied_at) VALUES ($1, $2)")
            .bind(*name)
            .bind(chrono::Utc::now().to_rfc3339())
            .execute(pool)
            .await
            .map_err(|e| DbError::Migration(format!("record {name}: {e}")))?;
        tracing::info!("applied migration {name}");
    }
    Ok(())
}

/// Embedded migration files — compile-time `include_str!` of every
/// `V*.sql` in the migrations directory. Append new files to this list
/// when adding migrations; the runner does the rest.
const MIGRATION_FILES: &[(&str, &str)] = &[
    (
        "V0001__init.sql",
        include_str!("migrations/V0001__init.sql"),
    ),
    (
        "V0002__token_ciphertext.sql",
        include_str!("migrations/V0002__token_ciphertext.sql"),
    ),
    (
        "V0003__dedup.sql",
        include_str!("migrations/V0003__dedup.sql"),
    ),
    (
        "V0004__settings_version.sql",
        include_str!("migrations/V0004__settings_version.sql"),
    ),
    (
        "V0005__tenancy.sql",
        include_str!("migrations/V0005__tenancy.sql"),
    ),
    (
        "V0006__trust_inputs.sql",
        include_str!("migrations/V0006__trust_inputs.sql"),
    ),
    (
        "V0007__billing.sql",
        include_str!("migrations/V0007__billing.sql"),
    ),
    (
        "V0008__billing_periods.sql",
        include_str!("migrations/V0008__billing_periods.sql"),
    ),
    (
        "V0009__account_platform.sql",
        include_str!("migrations/V0009__account_platform.sql"),
    ),
];

// --- Process-global async pool handle --------------------------------------

static GLOBAL_ASYNC_POOL: OnceLock<AsyncDbPool> = OnceLock::new();

/// Install the process-global async pool. Called once during daemon boot.
pub fn set_global_async_pool(pool: AsyncDbPool) {
    let _ = GLOBAL_ASYNC_POOL.set(pool);
}

/// Borrow the process-global async pool. Returns `None` if not yet
/// initialized — caller decides whether to lazy-init via `open_default()`
/// or to fail.
pub fn try_global_async_pool() -> Option<AsyncDbPool> {
    GLOBAL_ASYNC_POOL.get().cloned()
}

/// Test helper: connect to a **separate** test-database, wiping it on
/// each call to give each test a fresh schema. Approximates SQLite's
/// `:memory:` per-call freshness.
///
/// Critical safety: this function REFUSES to operate against the
/// daemon's production DB. The DSN's database name must end in
/// `_test` (e.g., `weclawbot_test`) — guards against tests
/// accidentally wiping live data. CI sets `POSTGRES_DB=weclawbot_test`.
///
/// For local dev, create the test DB once:
/// ```sh
/// podman exec weclawbot-pg createdb -U weclawbot weclawbot_test
/// ```
///
/// Then run tests with:
/// ```sh
/// export DATABASE_URL='postgres://weclawbot:weclawbot-local@127.0.0.1:5432/weclawbot_test'
/// cargo test --release -- --test-threads=1
/// ```
#[cfg(test)]
pub async fn open_in_memory() -> Result<AsyncDbPool, DbError> {
    use sqlx::Executor;
    let url = std::env::var("DATABASE_URL")
        .or_else(|_| std::env::var("WECLAWBOT_PG_URL"))
        .map_err(|_| {
            DbError::Pool(
                "tests need DATABASE_URL pointing at a *_test postgres DB \
                 (NOT the production DB; tests wipe it on each call)"
                    .into(),
            )
        })?;
    // Safety: extract DB name from DSN, refuse anything that isn't `_test`-suffixed.
    let db_name = url.rsplit('/').next().unwrap_or("");
    let db_name_clean = db_name.split('?').next().unwrap_or("");
    if !db_name_clean.ends_with("_test") {
        return Err(DbError::Pool(format!(
            "REFUSING to wipe non-test DB '{db_name_clean}': test DSN must end in '_test' \
             (e.g., postgres://.../weclawbot_test). Set DATABASE_URL accordingly."
        )));
    }
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .map_err(|e| DbError::Pool(e.to_string()))?;
    pool.execute("DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public;")
        .await
        .map_err(|e| DbError::Migration(format!("test reset schema: {e}")))?;
    apply_migrations(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only runs when CI / dev has DATABASE_URL set. Otherwise marked
    /// ignore so `cargo test` on a vanilla machine doesn't fail to find
    /// a Postgres.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL pointing at a postgres:16 instance"]
    async fn pool_opens_with_migrations() {
        let pool = open_in_memory().await.unwrap();
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM tenants WHERE id = 'default'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, 1);
    }
}
