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
        // v7.0: applied_at column starts TEXT (V0001 schema), becomes
        // TIMESTAMPTZ after V0012. sqlx binds chrono::DateTime<Utc> to
        // either correctly (TEXT cast: ISO 8601; TIMESTAMPTZ: native).
        sqlx::query("INSERT INTO _sqlx_migrations (version, applied_at) VALUES ($1, $2)")
            .bind(*name)
            .bind(chrono::Utc::now())
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
    (
        "V0010__jsonb.sql",
        include_str!("migrations/V0010__jsonb.sql"),
    ),
    (
        "V0011__uuid.sql",
        include_str!("migrations/V0011__uuid.sql"),
    ),
    (
        "V0012__timestamptz.sql",
        include_str!("migrations/V0012__timestamptz.sql"),
    ),
    (
        "V0013__tenant_sso_config.sql",
        include_str!("migrations/V0013__tenant_sso_config.sql"),
    ),
    (
        "V0014__sla_rollup.sql",
        include_str!("migrations/V0014__sla_rollup.sql"),
    ),
    (
        "V0015__stripe_subscription_item.sql",
        include_str!("migrations/V0015__stripe_subscription_item.sql"),
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

/// v7.0 — per-test isolated PG via `testcontainers`. Each call:
///   1. Reuses a process-global postgres:16 container (started lazily on
///      first call, kept alive for the rest of the process).
///   2. Creates a fresh database with a random name (`t_<uuid>`).
///   3. Connects + applies all migrations.
///   4. Returns the pool. Caller's `Drop` disconnects; the DB stays in the
///      container until process exit (cheap; tens of MB total per run).
///
/// Tests can run in parallel (default `cargo test` behavior) because each
/// gets its own DB. The `--test-threads=1` workaround from v5.6 is gone.
///
/// Requires Docker (or podman with the docker socket bridge) on the
/// runner. CI's Ubuntu image has Docker pre-installed; WSL devs need
/// `podman system service --time 0 unix:///tmp/podman.sock` exposed as
/// `DOCKER_HOST=unix:///tmp/podman.sock` (one-time setup).
#[cfg(test)]
pub async fn open_in_memory() -> Result<AsyncDbPool, DbError> {
    let pg = test_pg_container().await;
    let db_name = format!("t_{}", uuid::Uuid::new_v4().simple());
    create_test_database(pg, &db_name).await?;
    let url = format!(
        "postgres://{}:{}@{}:{}/{}",
        pg.user, pg.password, pg.host, pg.port, db_name
    );
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .map_err(|e| DbError::Pool(e.to_string()))?;
    apply_migrations(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
struct TestPgContainer {
    // Kept alive (Drop stops the container) by the OnceCell.
    _container: testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    user: String,
    password: String,
    host: String,
    port: u16,
}

#[cfg(test)]
static TEST_PG: tokio::sync::OnceCell<TestPgContainer> = tokio::sync::OnceCell::const_new();

#[cfg(test)]
async fn test_pg_container() -> &'static TestPgContainer {
    TEST_PG
        .get_or_init(|| async {
            use testcontainers::runners::AsyncRunner;
            use testcontainers_modules::postgres::Postgres;

            let user = "postgres".to_string();
            let password = "postgres".to_string();
            let container = Postgres::default()
                .start()
                .await
                .expect("start postgres testcontainer (need docker/podman)");
            let port = container
                .get_host_port_ipv4(5432)
                .await
                .expect("testcontainer port");
            let host = container
                .get_host()
                .await
                .expect("testcontainer host")
                .to_string();
            TestPgContainer {
                _container: container,
                user,
                password,
                host,
                port,
            }
        })
        .await
}

#[cfg(test)]
async fn create_test_database(pg: &TestPgContainer, name: &str) -> Result<(), DbError> {
    use sqlx::Executor;
    let admin_url = format!(
        "postgres://{}:{}@{}:{}/postgres",
        pg.user, pg.password, pg.host, pg.port
    );
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .map_err(|e| DbError::Pool(format!("admin connect: {e}")))?;
    // Identifier safe: name is `t_<32 hex chars>`, no quoting risk, but
    // wrap anyway since CREATE DATABASE doesn't accept placeholders.
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .map_err(|e| DbError::Migration(format!("create test db: {e}")))?;
    admin.close().await;
    Ok(())
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
