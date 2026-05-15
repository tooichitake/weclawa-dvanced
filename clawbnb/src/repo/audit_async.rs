//! AsyncAuditRepo — v4.1 K6 sqlx 版本。

use chrono::Utc;
use serde_json::Value;
use crate::storage::db_async::AsyncDbPool;

use crate::repo::audit::{AuditEntry, AuditInput};
use crate::storage::db::DbError;

pub struct SqlxAuditRepo {
    pool: AsyncDbPool,
}

impl SqlxAuditRepo {
    pub fn new(pool: AsyncDbPool) -> Self {
        Self { pool }
    }

    pub async fn record(&self, input: AuditInput<'_>) -> Result<i64, DbError> {
        let ts = Utc::now().to_rfc3339();
        let before = input.before.map(serde_json::to_string).transpose()?;
        let after = input.after.map(serde_json::to_string).transpose()?;
        // v5.3: 用 `RETURNING id` 替代 SQLite-only `last_insert_rowid()` ──
        // RETURNING 在 SQLite 3.35+ (2021-03-12) 和 Postgres 9.1+ 都支持，
        // 是跨 backend portable 的拿 generated id 方法。
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO audit_log
                 (ts, actor_key_id, action, target, before_json, after_json, ip)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING id",
        )
        .bind(&ts)
        .bind(input.actor_key_id)
        .bind(input.action)
        .bind(input.target)
        .bind(&before)
        .bind(&after)
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
        let rows: Vec<(i64, String, Option<String>, String, Option<String>, Option<String>, Option<String>, Option<String>)> =
            sqlx::query_as(
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
            .map(|(id, ts, actor, action, target, before_raw, after_raw, ip)| AuditEntry {
                id,
                ts,
                actor_key_id: actor,
                action,
                target,
                before: before_raw.and_then(|s| serde_json::from_str::<Value>(&s).ok()),
                after: after_raw.and_then(|s| serde_json::from_str::<Value>(&s).ok()),
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
        r.record(AuditInput {
            actor_key_id: Some("key-1"),
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
