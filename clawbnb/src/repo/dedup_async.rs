//! AsyncDedupRepo — v4 J3 hot path 切 sqlx。
//!
//! handler.rs 每条 inbound 都打一次 dedup check 跟 mark_seen，是真热
//! 路径。切到 async 后省去 spawn_blocking 跳板。

use sqlx::SqlitePool;

use crate::storage::db::DbError;

pub struct SqlxDedupRepo {
    pool: SqlitePool,
}

impl SqlxDedupRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// scoped variant (preferred): mark_seen with explicit tenant_id +
    /// account_id + msg_id triplet。返回 true = 已存在（duplicate），caller
    /// skip 处理。
    pub async fn mark_seen_scoped(
        &self,
        tenant_id: &str,
        account_id: &str,
        msg_id: i64,
    ) -> Result<bool, DbError> {
        let now = chrono::Utc::now().to_rfc3339();
        // v5.2 O2: ANSI ON CONFLICT (SQLite 3.24+ / Postgres)
        let res = sqlx::query(
            "INSERT INTO seen_messages
                 (msg_id, first_seen_at, tenant_id, account_id)
             VALUES (?, ?, ?, ?)
             ON CONFLICT (msg_id) DO NOTHING",
        )
        .bind(msg_id)
        .bind(&now)
        .bind(tenant_id)
        .bind(account_id)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx mark_seen: {e}")))?;
        Ok(res.rows_affected() == 0)
    }

    /// 兼容入口 — v2.2 single tenant callers
    pub async fn mark_seen(&self, msg_id: i64) -> Result<bool, DbError> {
        self.mark_seen_scoped(crate::tenancy::DEFAULT_TENANT, "default", msg_id)
            .await
    }

    /// 撤销 mark — sandbox::ensure 失败后 unmark 让 redeliver 重试。
    pub async fn unmark(&self, msg_id: i64) -> Result<(), DbError> {
        sqlx::query("DELETE FROM seen_messages WHERE msg_id = ?")
            .bind(msg_id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx unmark: {e}")))?;
        Ok(())
    }

    /// 削峰 — 删 first_seen_at < cutoff 的老行。1% lazy prune 同 sync 版本。
    pub async fn prune_older_than(&self, cutoff: &str) -> Result<u64, DbError> {
        let res = sqlx::query("DELETE FROM seen_messages WHERE first_seen_at < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx prune: {e}")))?;
        Ok(res.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    async fn repo() -> SqlxDedupRepo {
        SqlxDedupRepo::new(db_async::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn first_mark_not_duplicate_async() {
        let r = repo().await;
        assert!(!r.mark_seen(42).await.unwrap());
    }

    #[tokio::test]
    async fn second_mark_duplicate_async() {
        let r = repo().await;
        assert!(!r.mark_seen(42).await.unwrap());
        assert!(r.mark_seen(42).await.unwrap());
    }

    #[tokio::test]
    async fn unmark_lets_redeliver_retry_async() {
        let r = repo().await;
        assert!(!r.mark_seen(42).await.unwrap());
        r.unmark(42).await.unwrap();
        assert!(!r.mark_seen(42).await.unwrap());
    }
}
