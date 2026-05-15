//! AsyncTrustRepo — v7.0 timestamps TIMESTAMPTZ, inputs_json JSONB.
//!
//! Trust scoring is shipped as a scaffold: the table + `get` lookup are
//! wired (so `tool_policy::for_user` can read tier overlays), but nothing
//! in production currently writes scores. `upsert` / `list_by_tier` and
//! the score-compute helpers were removed in v7.0 housekeeping; revive
//! them when the trust pipeline gets wired into `monitor::handler` (v3).

use chrono::{DateTime, Utc};

use crate::repo::trust::TrustSnapshot;
use crate::storage::db::DbError;
use crate::storage::db_async::AsyncDbPool;
use crate::storage::ts;
use crate::tenancy::trust::{TrustInputs, TrustTier};

pub struct SqlxTrustRepo {
    pool: AsyncDbPool,
}

type TrustRow = (
    String,
    String,
    f64,
    f64,
    f64,
    f64,
    f64,
    String,
    DateTime<Utc>,
    DateTime<Utc>,
);

impl SqlxTrustRepo {
    pub fn new(pool: AsyncDbPool) -> Self {
        Self { pool }
    }

    pub async fn get(&self, user_hash: &str) -> Result<Option<TrustSnapshot>, DbError> {
        let row: Option<TrustRow> = sqlx::query_as(
            "SELECT user_hash, tenant_id, success_rate, uptime, threat, integrity,
                    score, tier, updated_at, tier_since
             FROM user_trust_inputs WHERE user_hash = $1",
        )
        .bind(user_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx trust get: {e}")))?;
        Ok(row.map(materialize))
    }

    // v7.0 housekeeping: `upsert` + `list_by_tier` removed — nothing
    // in production writes scores yet, so a fresh table just stays empty
    // and `get()` returns None (which `tool_policy::for_user` already
    // handles as "use Standard tier"). v3 will revive both alongside
    // a scoring driver loop.
}

fn materialize(r: TrustRow) -> TrustSnapshot {
    TrustSnapshot {
        user_hash: r.0,
        tenant_id: r.1,
        inputs: TrustInputs {
            success_rate: r.2,
            uptime: r.3,
            threat: r.4,
            integrity: r.5,
        },
        score: r.6,
        tier: TrustTier::from_score(r.6),
        updated_at: ts::format_rfc3339(&r.8),
        tier_since: ts::format_rfc3339(&r.9),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    async fn repo() -> SqlxTrustRepo {
        SqlxTrustRepo::new(db_async::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn nonexistent_returns_none_async() {
        let r = repo().await;
        assert!(r.get("u-xxxxxxxxxxxx").await.unwrap().is_none());
    }
}
