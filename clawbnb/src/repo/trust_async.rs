//! AsyncTrustRepo — v4.1 K4 sqlx 版本。

use chrono::Utc;
use serde_json::json;
use sqlx::SqlitePool;

use crate::repo::trust::TrustSnapshot;
use crate::storage::db::DbError;
use crate::tenancy::trust::{TrustInputs, TrustTier};

pub struct SqlxTrustRepo {
    pool: SqlitePool,
}

impl SqlxTrustRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get(&self, user_hash: &str) -> Result<Option<TrustSnapshot>, DbError> {
        let row: Option<(String, String, f64, f64, f64, f64, f64, String, String, String)> = sqlx::query_as(
            "SELECT user_hash, tenant_id, success_rate, uptime, threat, integrity,
                    score, tier, updated_at, tier_since
             FROM user_trust_inputs WHERE user_hash = ?",
        )
        .bind(user_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx trust get: {e}")))?;
        Ok(row.map(|r| TrustSnapshot {
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
            updated_at: r.8,
            tier_since: r.9,
        }))
    }

    pub async fn upsert(&self, snap: &TrustSnapshot) -> Result<(), DbError> {
        let prev: Option<(String,)> =
            sqlx::query_as("SELECT tier FROM user_trust_inputs WHERE user_hash = ?")
                .bind(&snap.user_hash)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DbError::Pool(format!("sqlx trust prev: {e}")))?;
        let prev_tier = prev.as_ref().map(|t| t.0.clone());
        let tier_changed = prev_tier.as_deref() != Some(snap.tier.as_str());
        let tier_since = if tier_changed {
            snap.updated_at.clone()
        } else {
            snap.tier_since.clone()
        };

        sqlx::query(
            "INSERT INTO user_trust_inputs
                 (user_hash, tenant_id, success_rate, uptime, threat, integrity,
                  score, tier, updated_at, tier_since)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
        .bind(&snap.updated_at)
        .bind(&tier_since)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx trust upsert: {e}")))?;

        if tier_changed {
            let inputs_json = json!({
                "success_rate": snap.inputs.success_rate,
                "uptime": snap.inputs.uptime,
                "threat": snap.inputs.threat,
                "integrity": snap.inputs.integrity,
            })
            .to_string();
            sqlx::query(
                "INSERT INTO user_trust_history
                     (user_hash, tenant_id, ts, inputs_json, score, tier, prev_tier)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&snap.user_hash)
            .bind(&snap.tenant_id)
            .bind(Utc::now().to_rfc3339())
            .bind(&inputs_json)
            .bind(snap.score)
            .bind(snap.tier.as_str())
            .bind(prev_tier)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx trust history: {e}")))?;
        }
        Ok(())
    }

    pub async fn list_by_tier(
        &self,
        tenant_id: &str,
        tier: TrustTier,
    ) -> Result<Vec<TrustSnapshot>, DbError> {
        let rows: Vec<(String, String, f64, f64, f64, f64, f64, String, String, String)> = sqlx::query_as(
            "SELECT user_hash, tenant_id, success_rate, uptime, threat, integrity,
                    score, tier, updated_at, tier_since
             FROM user_trust_inputs
             WHERE tenant_id = ? AND tier = ?
             ORDER BY score ASC",
        )
        .bind(tenant_id)
        .bind(tier.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx trust list_by_tier: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|r| TrustSnapshot {
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
                updated_at: r.8,
                tier_since: r.9,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    async fn repo() -> SqlxTrustRepo {
        SqlxTrustRepo::new(db_async::open_in_memory().await.unwrap())
    }

    fn sample(hash: &str, tier: TrustTier, score: f64) -> TrustSnapshot {
        TrustSnapshot {
            user_hash: hash.into(),
            tenant_id: "default".into(),
            inputs: TrustInputs {
                success_rate: score,
                uptime: score,
                threat: 1.0 - score,
                integrity: score,
            },
            score,
            tier,
            updated_at: "2026-05-15T00:00:00Z".into(),
            tier_since: "2026-05-15T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn upsert_then_get_async() {
        let r = repo().await;
        let s = sample("u-aaaaaaaaaaaa", TrustTier::Standard, 0.7);
        r.upsert(&s).await.unwrap();
        let got = r.get(&s.user_hash).await.unwrap().unwrap();
        assert_eq!(got.tier, TrustTier::Standard);
    }

    #[tokio::test]
    async fn nonexistent_returns_none_async() {
        let r = repo().await;
        assert!(r.get("u-xxxxxxxxxxxx").await.unwrap().is_none());
    }
}
