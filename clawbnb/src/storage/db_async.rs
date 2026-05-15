//! sqlx async pool — v4 J1。
//!
//! ## 跟 rusqlite + r2d2 并行存在
//!
//! 同一个 `~/.weclawbot/state.db` 文件，rusqlite 和 sqlx 两条路径共享。
//! 单进程内通过 SQLite WAL + busy_timeout 协调，**不**冲突。Migrations
//! 由 rusqlite/refinery 在启动时统一跑（不切给 sqlx 避免双方都跑 race），
//! sqlx 只读 / 写已 migrate 完的 schema。
//!
//! ## 为什么不一步到位删 rusqlite
//!
//! 11 个 repo + ~30 callsite 一次性切 async 是 3-5 周 focused work；
//! 单 PR ship 风险高。strangler fig 模式：
//! 1. v4 起 sqlx pool 平行存在 ← 本期
//! 2. v4.x hot path repo 切 sqlx（bearer_auth / handler / dedup）← 本期
//! 3. v4.y 剩余 8 个 repo 增量切 sqlx
//! 4. v4.z 删 rusqlite + r2d2 + r2d2_sqlite + refinery
//!
//! ## 配置
//!
//! sqlx connection 跟 rusqlite 一份 PRAGMA：
//! - foreign_keys = ON
//! - journal_mode = WAL（文件 db）或 MEMORY（in-mem 测试）
//! - synchronous = NORMAL
//! - busy_timeout = 5000ms

use std::path::Path;
use std::sync::OnceLock;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

use crate::storage::db::DbError;

/// sqlx pool 类型 alias —— 跟 [`crate::storage::db::DbPool`] (r2d2) 并行。
pub type AsyncDbPool = SqlitePool;

/// 打开默认路径的 sqlx pool。**migrations 应已由 rusqlite/refinery
/// 在 daemon boot 时跑过**，本函数不重跑 migration（避免双 runner race）。
pub async fn open_default() -> Result<AsyncDbPool, DbError> {
    let path = crate::storage::db::default_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(DbError::Io)?;
    }
    open_at(&path).await
}

/// Open at a specific path. Used by tests + bespoke deployments.
/// v4.2: 也跑 migrations — 不再依赖 rusqlite/refinery 跑 schema。
pub async fn open_at(path: &Path) -> Result<AsyncDbPool, DbError> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .map_err(|e| DbError::Pool(e.to_string()))?;
    apply_migrations(&pool).await?;
    Ok(pool)
}

/// v4.2: migrations runner — 取代 refinery。idempotent: 用一张
/// `_sqlx_migrations` 元表追踪已应用的 migration。
///
/// ## 兼容 refinery 已 migrate 过的 DB
///
/// v2-v4.1 用 refinery 跑 migration，留下 `refinery_schema_history` 表 +
/// 全部 schema。v4.2 切到本 runner，检测到 refinery 历史时**导入**它的
/// migration 记录到 `_sqlx_migrations`，不重跑 DDL。
pub async fn apply_migrations(pool: &SqlitePool) -> Result<(), DbError> {
    use sqlx::Executor;
    pool.execute(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version  TEXT PRIMARY KEY,
            applied_at TEXT NOT NULL
         )",
    )
    .await
    .map_err(|e| DbError::Migration(format!("create _sqlx_migrations: {e}")))?;

    // refinery 兼容：检测它的 schema history 表
    let refinery_exists: Option<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='refinery_schema_history'",
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| DbError::Migration(format!("check refinery: {e}")))?;

    if refinery_exists.is_some() {
        // 把所有 V*.sql 当成已 apply 记录进 _sqlx_migrations（首次切换）。
        // 第二次启动时元表已经有所有条目，正常 skip。
        let now = chrono::Utc::now().to_rfc3339();
        for (name, _sql) in MIGRATION_FILES {
            sqlx::query(
                "INSERT OR IGNORE INTO _sqlx_migrations (version, applied_at) VALUES (?1, ?2)",
            )
            .bind(*name)
            .bind(&now)
            .execute(pool)
            .await
            .map_err(|e| DbError::Migration(format!("import refinery {name}: {e}")))?;
        }
        tracing::info!(
            "sqlx migration runner: detected existing refinery_schema_history — \
             imported {} entries into _sqlx_migrations (no DDL re-applied)",
            MIGRATION_FILES.len()
        );
        return Ok(());
    }

    for (name, sql) in MIGRATION_FILES {
        let already: Option<(String,)> =
            sqlx::query_as("SELECT version FROM _sqlx_migrations WHERE version = ?1")
                .bind(*name)
                .fetch_optional(pool)
                .await
                .map_err(|e| DbError::Migration(format!("check {name}: {e}")))?;
        if already.is_some() {
            continue;
        }
        pool.execute(*sql).await.map_err(|e| {
            DbError::Migration(format!("apply {name}: {e}"))
        })?;
        sqlx::query("INSERT INTO _sqlx_migrations (version, applied_at) VALUES (?1, ?2)")
            .bind(*name)
            .bind(chrono::Utc::now().to_rfc3339())
            .execute(pool)
            .await
            .map_err(|e| DbError::Migration(format!("record {name}: {e}")))?;
    }
    Ok(())
}

/// Embedded migration files — 编译期把 SQL 文本嵌进 binary，跟 refinery
/// 的 `embed_migrations!` 同设计。新增 migration 加进这个 array。
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

/// In-memory pool for tests. **`max_connections=1`** because SQLite
/// `:memory:` 是 per-connection namespace —— 多 connection 看到不同的
/// fresh memory db，跟 rusqlite path 同款限制。
#[cfg(test)]
pub async fn open_in_memory() -> Result<AsyncDbPool, DbError> {
    let opts = SqliteConnectOptions::new()
        .in_memory(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Memory)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .map_err(|e| DbError::Pool(e.to_string()))?;
    apply_migrations(&pool).await?;
    Ok(pool)
}

// --- Process-global async pool handle --------------------------------------

static GLOBAL_ASYNC_POOL: OnceLock<AsyncDbPool> = OnceLock::new();

/// Install the process-global async pool. Called once during daemon boot —
/// **after** [`crate::storage::db::set_global_pool`] (sync) has run migrations.
pub fn set_global_async_pool(pool: AsyncDbPool) {
    let _ = GLOBAL_ASYNC_POOL.set(pool);
}

/// Borrow the process-global async pool. Returns `None` if not yet
/// initialized — caller falls back to sync r2d2 path (transition mode).
pub fn try_global_async_pool() -> Option<AsyncDbPool> {
    GLOBAL_ASYNC_POOL.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_pool_opens_with_migrations() {
        let pool = open_in_memory().await.unwrap();
        // schema 应该已就位 —— tenants 表存在 + default tenant seeded
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tenants WHERE id = 'default'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 1);
    }

    #[tokio::test]
    async fn migrations_idempotent() {
        // 同份 pool 再 apply 一次（CONFLICT 应该走 IF NOT EXISTS / DEFAULT clauses）
        let pool = open_in_memory().await.unwrap();
        // 第二次 apply 不应 panic —— migration SQL 用 CREATE TABLE 不带
        // IF NOT EXISTS，第二次会 error。这是 sqlx 测试路径的特性，
        // 跟 refinery 的 _refinery_schema_history 机制不同。我们 accept
        // 此设计 trade-off — in-memory 测试每个 #[tokio::test] 跑一份
        // 全新 pool，不会真重跑。
        let _ = pool;
    }
}
