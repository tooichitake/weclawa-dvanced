//! AsyncRateLimitRepo — v4.1 K3 sqlx 版本。

use sqlx::SqlitePool;

use crate::repo::rate_limits::WindowCount;
use crate::storage::db::DbError;

pub struct SqlxRateLimitRepo {
    pool: SqlitePool,
}

impl SqlxRateLimitRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn increment(
        &self,
        scope_key: &str,
        window_start_ts: &str,
    ) -> Result<u64, DbError> {
        sqlx::query(
            "INSERT INTO rate_limits (scope_key, window_start_ts, count)
             VALUES (?, ?, 1)
             ON CONFLICT(scope_key, window_start_ts) DO UPDATE SET
                 count = count + 1",
        )
        .bind(scope_key)
        .bind(window_start_ts)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx rate inc: {e}")))?;
        let row: (i64,) = sqlx::query_as(
            "SELECT count FROM rate_limits WHERE scope_key = ? AND window_start_ts = ?",
        )
        .bind(scope_key)
        .bind(window_start_ts)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx rate get-after-inc: {e}")))?;
        Ok(row.0.max(0) as u64)
    }

    pub async fn get(&self, scope_key: &str, window_start_ts: &str) -> Result<u64, DbError> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT count FROM rate_limits WHERE scope_key = ? AND window_start_ts = ?",
        )
        .bind(scope_key)
        .bind(window_start_ts)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx rate get: {e}")))?;
        Ok(row.map(|r| r.0.max(0) as u64).unwrap_or(0))
    }

    pub async fn sum_for_scope(&self, scope_key: &str) -> Result<u64, DbError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(count), 0) FROM rate_limits WHERE scope_key = ?",
        )
        .bind(scope_key)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx rate sum: {e}")))?;
        Ok(row.0.max(0) as u64)
    }

    pub async fn prune_older_than(&self, cutoff: &str) -> Result<u64, DbError> {
        let res = sqlx::query("DELETE FROM rate_limits WHERE window_start_ts < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx rate prune: {e}")))?;
        Ok(res.rows_affected())
    }

    pub async fn list_scope(&self, scope_key: &str) -> Result<Vec<WindowCount>, DbError> {
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT scope_key, window_start_ts, count FROM rate_limits
             WHERE scope_key = ? ORDER BY window_start_ts DESC",
        )
        .bind(scope_key)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx rate list: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|(s, w, c)| WindowCount {
                scope_key: s,
                window_start_ts: w,
                count: c.max(0) as u64,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    async fn repo() -> SqlxRateLimitRepo {
        SqlxRateLimitRepo::new(db_async::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn first_increment_seeds_to_one_async() {
        let r = repo().await;
        assert_eq!(
            r.increment("inbound:u-1", "2026-05-13T16:48:00Z")
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn repeated_increments_accumulate_async() {
        let r = repo().await;
        for expected in 1..=5 {
            assert_eq!(
                r.increment("inbound:u-1", "2026-05-13T16:48:00Z")
                    .await
                    .unwrap(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn sum_for_scope_aggregates_async() {
        let r = repo().await;
        r.increment("ai:acct-1", "2026-05-13T16:48:00Z").await.unwrap();
        r.increment("ai:acct-1", "2026-05-13T16:48:00Z").await.unwrap();
        r.increment("ai:acct-1", "2026-05-13T16:49:00Z").await.unwrap();
        assert_eq!(r.sum_for_scope("ai:acct-1").await.unwrap(), 3);
    }
}
