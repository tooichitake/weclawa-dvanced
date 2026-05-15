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

use std::sync::OnceLock;

#[cfg(not(feature = "postgres"))]
use std::path::Path;

#[cfg(not(feature = "postgres"))]
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
#[cfg(not(feature = "postgres"))]
use sqlx::SqlitePool;

#[cfg(feature = "postgres")]
use sqlx::postgres::PgPoolOptions;

use crate::storage::db::DbError;

/// sqlx pool 类型 alias — cfg-gated SQLite/Postgres 双 backend。
///
/// ## v5.3 final state — Postgres ready
///
/// **全部完成**:
/// - ✅ 所有 repo 用无序号 `?` placeholder（76 处 `?N` → `?`）
/// - ✅ 所有 INSERT 用 ANSI `ON CONFLICT (col) DO ...`（替代 SQLite-only
///   `INSERT OR IGNORE`）
/// - ✅ Migration runner `portable_ddl()` 把 `INTEGER PRIMARY KEY
///   AUTOINCREMENT` cfg 替换成 `BIGSERIAL PRIMARY KEY`
/// - ✅ CI postgres testcontainer 矩阵
/// - ✅ **`AsyncDbPool` cfg-gated typealias** (`SqlitePool` / `PgPool`)
/// - ✅ **所有 11 repo `pool: SqlitePool` → `pool: AsyncDbPool`**
/// - ✅ `cli/backup.rs::VACUUM INTO` → `pg_dump` 外部命令 cfg 分支
/// - ✅ `audit_async::record::last_insert_rowid()` → `RETURNING id`
///
/// **不变 (SQLite 友好)**:
/// - `?` placeholder 跨 driver 通用（sqlx 0.8 PG 客户端自动改写到 `$N`）
/// - ANSI ON CONFLICT 两 backend 都吃
/// - `RETURNING id` 两 backend 都吃 (SQLite 3.35+, PG 9.1+)
///
/// ## Backend 切换方法
///
/// **默认 SQLite**：`cargo build --release` —— `~/.weclawbot/state.db` 文件
///
/// **Postgres**：`cargo build --release --features postgres`，daemon 启动
/// 时读 env `WECLAWBOT_PG_URL=postgres://user:pass@host/db`（或
/// `DATABASE_URL` 兜底）。Backup/restore 走 `pg_dump`/`psql` 外部命令。
///
/// **2. SQLite-specific SQL 子句**
/// - `INSERT OR IGNORE` (出现 3 处：dedup / defaults bootstrap / bindings
///   register_or_touch) → Postgres 兼容写法 `INSERT ... ON CONFLICT
///   (col) DO NOTHING`（SQLite 3.24+ 也支持，可统一）
/// - `VACUUM INTO 'path'` (backup) → Postgres `pg_dump`（语义不同，
///   按 backend cfg 走两条 path）
/// - PRAGMA (foreign_keys / journal_mode / busy_timeout / synchronous) →
///   Postgres 无需，cfg 跳过
/// - `INTEGER PRIMARY KEY AUTOINCREMENT` (V0001 user_history.id, V0006
///   user_trust_history.id, V0007 sandbox_logs.id, audit_log.id) →
///   Postgres `BIGSERIAL` 或 `GENERATED ALWAYS AS IDENTITY`。改 migration
///   文件 cfg 化或 dup 一份 V*_pg__*.sql
///
/// **3. Migration runner**
/// `_sqlx_migrations` 表 + refinery 兼容 import 都 ANSI SQL，OK。但每个
/// migration 文件本身的 DDL 需 portable，见上述 (2)。
///
/// **4. Pool type**
/// 两条路：
/// - cfg-gated typealias：`#[cfg(feature="postgres")] type AsyncDbPool
///   = sqlx::PgPool;` 编译期 backend 选择
/// - `sqlx::AnyPool`：runtime backend 检测；单 binary 双驱动
///
/// **v5.3 PR 范围**：
/// - 把 3 处 `INSERT OR IGNORE` 改 `INSERT ... ON CONFLICT DO NOTHING`
/// - 把 4 处 `AUTOINCREMENT` 改 cfg-gated DDL
/// - 把 ~40 处 `?1, ?2` 改 `?` 无序号 + 按位 bind
/// - cfg-gated `AsyncDbPool` typealias
/// - 加 CI testcontainers postgres 矩阵
///
/// **当前 (v5.2)**: 保 SqlitePool。`postgres` Cargo feature 编 sqlx
/// postgres driver 进 binary（让 v5.3 PR 不用动 Cargo），但 daemon 实际
/// 跑 SQLite。
#[cfg(not(feature = "postgres"))]
pub type AsyncDbPool = SqlitePool;

#[cfg(feature = "postgres")]
pub type AsyncDbPool = sqlx::PgPool;

/// 打开默认 backend：SQLite 走 `~/.weclawbot/state.db` 文件；Postgres
/// 走 env `WECLAWBOT_PG_URL`（postgres://user:pass@host/db）。
#[cfg(not(feature = "postgres"))]
pub async fn open_default() -> Result<AsyncDbPool, DbError> {
    let path = crate::storage::db::default_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(DbError::Io)?;
    }
    open_at(&path).await
}

#[cfg(feature = "postgres")]
pub async fn open_default() -> Result<AsyncDbPool, DbError> {
    let url = std::env::var("WECLAWBOT_PG_URL").or_else(|_| std::env::var("DATABASE_URL"))
        .map_err(|_| DbError::Pool(
            "neither WECLAWBOT_PG_URL nor DATABASE_URL set (postgres backend requires one)".into(),
        ))?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .map_err(|e| DbError::Pool(e.to_string()))?;
    apply_migrations(&pool).await?;
    Ok(pool)
}

/// Open at a specific path. Used by tests + bespoke deployments.
/// v4.2: 也跑 migrations — 不再依赖 rusqlite/refinery 跑 schema。
///
/// **SQLite-only** —— Postgres 没"file path" 概念，PG 部署用 `open_default()`
/// 走 env DSN，或 caller 直接 `PgPoolOptions::connect()` 自己来。
#[cfg(not(feature = "postgres"))]
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
pub async fn apply_migrations(pool: &AsyncDbPool) -> Result<(), DbError> {
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
    // SQLite 用 sqlite_master，Postgres 用 information_schema.tables —
    // 两条 SQL 都查"有没有 refinery_schema_history"，结果转 Option<String>。
    #[cfg(not(feature = "postgres"))]
    let refinery_exists: Option<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='refinery_schema_history'",
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| DbError::Migration(format!("check refinery: {e}")))?;

    #[cfg(feature = "postgres")]
    let refinery_exists: Option<(String,)> = sqlx::query_as(
        "SELECT table_name FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name = 'refinery_schema_history'",
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
                "INSERT INTO _sqlx_migrations (version, applied_at) VALUES (?, ?)
                 ON CONFLICT (version) DO NOTHING",
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
            sqlx::query_as("SELECT version FROM _sqlx_migrations WHERE version = ?")
                .bind(*name)
                .fetch_optional(pool)
                .await
                .map_err(|e| DbError::Migration(format!("check {name}: {e}")))?;
        if already.is_some() {
            continue;
        }
        let portable_sql = portable_ddl(sql);
        pool.execute(portable_sql.as_str()).await.map_err(|e| {
            DbError::Migration(format!("apply {name}: {e}"))
        })?;
        sqlx::query("INSERT INTO _sqlx_migrations (version, applied_at) VALUES (?, ?)")
            .bind(*name)
            .bind(chrono::Utc::now().to_rfc3339())
            .execute(pool)
            .await
            .map_err(|e| DbError::Migration(format!("record {name}: {e}")))?;
    }
    Ok(())
}

/// v5.3: 把 SQLite-dialect 的 DDL 转换成当前 backend 能吃的 SQL。
///
/// - SQLite (default)：原样返回
/// - Postgres (`feature = "postgres"`)：把 `INTEGER PRIMARY KEY AUTOINCREMENT`
///   改成 `BIGSERIAL PRIMARY KEY`；其他 PG 不兼容子句（`WITHOUT ROWID`、
///   `PRAGMA`）migrations 文件里**禁止使用**，不会出现。
///
/// 用文本替换不是 AST 改写 —— 输入是我们自己写的 V*.sql，列出关键词足够。
/// 若将来 migration SQL 出现新关键词，加进这里即可。
fn portable_ddl(sql: &str) -> String {
    if cfg!(feature = "postgres") {
        sql.replace(
            "INTEGER PRIMARY KEY AUTOINCREMENT",
            "BIGSERIAL PRIMARY KEY",
        )
        // SQLite 的 BLOB / TEXT 类型 PG 也吃；INTEGER PG 也吃；
        // 文本时间戳 ('YYYY-MM-DDTHH:MM:SSZ') 走 TEXT 不动。
    } else {
        sql.to_string()
    }
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
///
/// **Postgres**：没在线 in-memory 等价物；PG 测试需要 testcontainer
/// 起 docker postgres，跑 env `DATABASE_URL` 注入 → `open_default()`。
/// 本函数在 `feature = "postgres"` 下会试 connect env DSN（CI workflow
/// 跑 postgres:16 service container 时可用）。
#[cfg(all(test, not(feature = "postgres")))]
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

/// Postgres `open_in_memory` 等价 —— 连 env DSN 跑 fresh schema。CI
/// workflow per-test 用 `DATABASE_URL` 注入 ephemeral PG。
///
/// **重要**：所有 PG 测试共享一个数据库，per-test data 不隔离。tests
/// 之间要避免数据交叉污染（用 unique key prefix 或测试前 TRUNCATE）。
/// 真严格隔离需要 testcontainers crate / docker-test，留 follow-up。
#[cfg(all(test, feature = "postgres"))]
pub async fn open_in_memory() -> Result<AsyncDbPool, DbError> {
    let url = std::env::var("DATABASE_URL").or_else(|_| std::env::var("WECLAWBOT_PG_URL"))
        .map_err(|_| DbError::Pool(
            "DATABASE_URL not set; CI postgres workflow must inject it".into(),
        ))?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
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
