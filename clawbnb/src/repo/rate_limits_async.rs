//! AsyncRateLimitRepo — v7.0 window_start_ts TIMESTAMPTZ.

use crate::storage::db_async::AsyncDbPool;
use crate::storage::ts;

use crate::storage::db::DbError;

pub struct SqlxRateLimitRepo {
    pool: AsyncDbPool,
}

impl SqlxRateLimitRepo {
    pub fn new(pool: AsyncDbPool) -> Self {
        Self { pool }
    }

    pub async fn increment(
        &self,
        scope_key: &str,
        window_start_ts: &str,
    ) -> Result<u64, DbError> {
        let ts = ts::parse_rfc3339(window_start_ts);
        sqlx::query(
            "INSERT INTO rate_limits (scope_key, window_start_ts, count)
             VALUES ($1, $2, 1)
             ON CONFLICT(scope_key, window_start_ts) DO UPDATE SET
                 count = rate_limits.count + 1",
        )
        .bind(scope_key)
        .bind(ts)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx rate inc: {e}")))?;
        let row: (i64,) = sqlx::query_as(
            "SELECT count FROM rate_limits WHERE scope_key = $1 AND window_start_ts = $2",
        )
        .bind(scope_key)
        .bind(ts)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx rate get-after-inc: {e}")))?;
        Ok(row.0.max(0) as u64)
    }

    // v7.0 housekeeping: `get`, `sum_for_scope`, `list_scope` removed —
    // production `monitor::rate_limit` only calls `increment` (atomic
    // get-after-increment) + `prune_older_than` (1% lazy prune).

    pub async fn prune_older_than(&self, cutoff: &str) -> Result<u64, DbError> {
        let res = sqlx::query("DELETE FROM rate_limits WHERE window_start_ts < $1")
            .bind(ts::parse_rfc3339(cutoff))
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx rate prune: {e}")))?;
        Ok(res.rows_affected())
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

    // v7.0 housekeeping: `sum_for_scope_aggregates_async` removed alongside
    // the `sum_for_scope` method.
}
