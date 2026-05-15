//! AsyncTrustRepo — v7.0 timestamps TIMESTAMPTZ, inputs_json JSONB.
//!
//! v7.4 — trust scoring driver now LIVE (see `crate::tenancy::trust_driver`).
//! `upsert` restored to support the periodic writer. `tool_policy::for_user`
//! reads tier overlays via `get`; in v7.0-7.3 nothing wrote so tier was
//! always Standard. Now the driver computes scores from history every
//! 15min and the overlay actually fires on misbehaving users.

use chrono::{DateTime, Utc};
use serde_json::json;

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

    /// v7.4 — restored. Atomic upsert of a TrustSnapshot. If the tier
    /// changed since last upsert, also records a row in
    /// `user_trust_history` so the GUI/audit can show "user A went
    /// from Standard → Restricted at T".
    pub async fn upsert(&self, snap: &TrustSnapshot) -> Result<(), DbError> {
        let prev: Option<(String,)> =
            sqlx::query_as("SELECT tier FROM user_trust_inputs WHERE user_hash = $1")
                .bind(&snap.user_hash)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DbError::Pool(format!("sqlx trust prev: {e}")))?;
        let prev_tier = prev.as_ref().map(|t| t.0.clone());
        let tier_changed = prev_tier.as_deref() != Some(snap.tier.as_str());
        let tier_since_str = if tier_changed {
            snap.updated_at.clone()
        } else {
            snap.tier_since.clone()
        };

        sqlx::query(
            "INSERT INTO user_trust_inputs
                 (user_hash, tenant_id, success_rate, uptime, threat, integrity,
                  score, tier, updated_at, tier_since)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT(user_hash) DO UPDATE SET
                 tenant_id    = excluded.tenant_id,
                 success_rate = excluded.success_rate,
                 uptime       = excluded.uptime,
                 threat       = excluded.threat,
                 integrity    = excluded.integrity,
                 score        = excluded.score,
                 tier         = excluded.tier,
                 updated_at   = excluded.updated_at,
                 tier_since   = excluded.tier_since",
        )
        .bind(&snap.user_hash)
        .bind(&snap.tenant_id)
        .bind(snap.inputs.success_rate)
        .bind(snap.inputs.uptime)
        .bind(snap.inputs.threat)
        .bind(snap.inputs.integrity)
        .bind(snap.score)
        .bind(snap.tier.as_str())
        .bind(ts::parse_rfc3339(&snap.updated_at))
        .bind(ts::parse_rfc3339(&tier_since_str))
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx trust upsert: {e}")))?;

        if tier_changed {
            let inputs_value = json!({
                "success_rate": snap.inputs.success_rate,
                "uptime": snap.inputs.uptime,
                "threat": snap.inputs.threat,
                "integrity": snap.inputs.integrity,
            });
            sqlx::query(
                "INSERT INTO user_trust_history
                     (user_hash, tenant_id, ts, inputs_json, score, tier, prev_tier)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(&snap.user_hash)
            .bind(&snap.tenant_id)
            .bind(Utc::now())
            .bind(&inputs_value)
            .bind(snap.score)
            .bind(snap.tier.as_str())
            .bind(prev_tier)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx trust history: {e}")))?;
        }
        Ok(())
    }
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
