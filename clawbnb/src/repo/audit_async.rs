//! AsyncAuditRepo — v7.0 ts TIMESTAMPTZ, before/after_json JSONB,
//! actor_key_id UUID.

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::repo::audit::{AuditEntry, AuditInput};
use crate::storage::db::DbError;
use crate::storage::db_async::AsyncDbPool;
use crate::storage::ts;

pub struct SqlxAuditRepo {
    pool: AsyncDbPool,
}

impl SqlxAuditRepo {
    pub fn new(pool: AsyncDbPool) -> Self {
        Self { pool }
    }

    pub async fn record(&self, input: AuditInput<'_>) -> Result<i64, DbError> {
        // actor_key_id is Option<&str> in API. Parse to UUID; if it's
        // not a valid UUID treat as NULL (system actor).
        let actor_uuid: Option<Uuid> = input
            .actor_key_id
            .and_then(|s| Uuid::parse_str(s).ok());
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO audit_log
                 (ts, actor_key_id, action, target, before_json, after_json, ip)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING id",
        )
        .bind(Utc::now())
        .bind(actor_uuid)
        .bind(input.action)
        .bind(input.target)
        .bind(input.before)
        .bind(input.after)
        .bind(input.ip)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx audit record: {e}")))?;
        Ok(row.0)
    }

    pub async fn list_recent_for_tenant(
        &self,
        tenant_id: &str,
        limit: u32,
    ) -> Result<Vec<AuditEntry>, DbError> {
        let rows: Vec<(
            i64,
            DateTime<Utc>,
            Option<Uuid>,
            String,
            Option<String>,
            Option<Value>,
            Option<Value>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT id, ts, actor_key_id, action, target, before_json, after_json, ip
             FROM audit_log WHERE tenant_id = $1 ORDER BY id DESC LIMIT $2",
        )
        .bind(tenant_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx audit list: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|(id, ts_val, actor, action, target, before_v, after_v, ip)| AuditEntry {
                id,
                ts: ts::format_rfc3339(&ts_val),
                actor_key_id: actor.map(|u| u.to_string()),
                action,
                target,
                before: before_v,
                after: after_v,
                ip,
            })
            .collect())
    }

    pub async fn list_recent(&self, limit: u32) -> Result<Vec<AuditEntry>, DbError> {
        self.list_recent_for_tenant(crate::tenancy::DEFAULT_TENANT, limit)
            .await
    }

    pub async fn count(&self) -> Result<u64, DbError> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx audit count: {e}")))?;
        Ok(row.0.max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    async fn repo() -> SqlxAuditRepo {
        SqlxAuditRepo::new(db_async::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn record_and_count_async() {
        let r = repo().await;
        assert_eq!(r.count().await.unwrap(), 0);
        // v7.0: actor_key_id is UUID — must be a parseable uuid string,
        // else treated as None (system actor).
        let test_uuid = "550e8400-e29b-41d4-a716-446655440000";
        r.record(AuditInput {
            actor_key_id: Some(test_uuid),
            action: "users.delete",
            target: Some("u-abc"),
            before: None,
            after: None,
            ip: Some("127.0.0.1"),
        })
        .await
        .unwrap();
        assert_eq!(r.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn list_recent_returns_newest_first_async() {
        let r = repo().await;
        for action in ["a", "b", "c"] {
            r.record(AuditInput {
                actor_key_id: None,
                action,
                target: None,
                before: None,
                after: None,
                ip: None,
            })
            .await
            .unwrap();
        }
        let list = r.list_recent(10).await.unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].action, "c");
    }
}
