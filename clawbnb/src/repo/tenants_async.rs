//! AsyncTenantRepo — v4 J2 hot path 切 sqlx。
//!
//! 跟 [`crate::repo::tenants::SqliteTenantRepo`] (rusqlite) **接口对等**，
//! 但所有方法是 `async fn`，axum handler 直接 `.await` 不再走
//! `spawn_blocking`。
//!
//! ## 使用规则
//!
//! - **新代码**：直接用 `AsyncTenantRepo`
//! - **遗留 CLI / startup**：仍走 sync 版（CLI 是单线程不在乎阻塞）
//! - **handler.rs hot path**：切 async（每条 inbound 都走 is_active）

use crate::storage::db_async::AsyncDbPool;

use crate::storage::db::DbError;
use crate::tenancy::{TenantId, TenantStatus};

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct AsyncTenant {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub status: String,
    pub stripe_customer_id: Option<String>,
    pub deleted_at: Option<String>,
    pub billing_status: String,
    pub billing_period_end: Option<String>,
    pub last_billing_event: Option<String>,
    pub last_billing_event_at: Option<String>,
}

impl AsyncTenant {
    pub fn status_enum(&self) -> TenantStatus {
        TenantStatus::from_str(&self.status).unwrap_or(TenantStatus::Active)
    }
}

pub struct SqlxTenantRepo {
    pool: AsyncDbPool,
}

impl SqlxTenantRepo {
    pub fn new(pool: AsyncDbPool) -> Self {
        Self { pool }
    }

    pub async fn get(&self, id: &TenantId) -> Result<Option<AsyncTenant>, DbError> {
        sqlx::query_as::<_, AsyncTenant>(
            "SELECT id, name, created_at, status, stripe_customer_id, deleted_at,
                    billing_status, billing_period_end, last_billing_event, last_billing_event_at
             FROM tenants WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx get tenant: {e}")))
    }

    /// 是否允许处理 inbound — `status='active'` AND (`billing_status='active'`
    /// OR (`billing_status='pending'` AND `grace_until > now`))。
    ///
    /// 跟 sync `SqliteTenantRepo::is_active` 行为 1:1 一致。
    pub async fn is_active(&self, id: &TenantId) -> Result<bool, DbError> {
        let now_rfc = chrono::Utc::now().to_rfc3339();
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT CASE
                WHEN status != 'active' THEN 0
                WHEN billing_status = 'active' THEN 1
                WHEN billing_status = 'pending'
                     AND grace_until IS NOT NULL
                     AND grace_until > ? THEN 1
                ELSE 0
             END
             FROM tenants WHERE id = ?",
        )
        .bind(&now_rfc)
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx is_active: {e}")))?;
        Ok(row.map(|r| r.0 == 1).unwrap_or(false))
    }

    /// Stripe webhook 调 — 更新 billing 字段 + 记 event 类型/时间。
    pub async fn update_billing_status(
        &self,
        id: &TenantId,
        billing_status: &str,
        event_type: &str,
    ) -> Result<bool, DbError> {
        let now = chrono::Utc::now().to_rfc3339();
        let res = sqlx::query(
            "UPDATE tenants
             SET billing_status = ?,
                 last_billing_event = ?,
                 last_billing_event_at = ?
             WHERE id = ?",
        )
        .bind(billing_status)
        .bind(event_type)
        .bind(&now)
        .bind(id.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx update_billing: {e}")))?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn find_by_stripe_customer(
        &self,
        customer_id: &str,
    ) -> Result<Option<AsyncTenant>, DbError> {
        sqlx::query_as::<_, AsyncTenant>(
            "SELECT id, name, created_at, status, stripe_customer_id, deleted_at,
                    billing_status, billing_period_end, last_billing_event, last_billing_event_at
             FROM tenants WHERE stripe_customer_id = ?",
        )
        .bind(customer_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx find_by_customer: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    async fn repo() -> SqlxTenantRepo {
        SqlxTenantRepo::new(db_async::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn default_tenant_is_active_via_sqlx() {
        let r = repo().await;
        assert!(r.is_active(&TenantId::default_tenant()).await.unwrap());
    }

    #[tokio::test]
    async fn suspended_is_inactive_via_sqlx() {
        let r = repo().await;
        r.update_billing_status(
            &TenantId::default_tenant(),
            "suspended",
            "customer.subscription.deleted",
        )
        .await
        .unwrap();
        assert!(!r.is_active(&TenantId::default_tenant()).await.unwrap());
    }

    #[tokio::test]
    async fn pending_without_grace_is_blocked_via_sqlx() {
        let r = repo().await;
        r.update_billing_status(
            &TenantId::default_tenant(),
            "pending",
            "invoice.payment_failed",
        )
        .await
        .unwrap();
        assert!(!r.is_active(&TenantId::default_tenant()).await.unwrap());
    }

    #[tokio::test]
    async fn pending_with_future_grace_is_active_via_sqlx() {
        let r = repo().await;
        r.update_billing_status(
            &TenantId::default_tenant(),
            "pending",
            "invoice.payment_failed",
        )
        .await
        .unwrap();
        let future = (chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339();
        sqlx::query("UPDATE tenants SET grace_until = ? WHERE id = 'default'")
            .bind(&future)
            .execute(&r.pool)
            .await
            .unwrap();
        assert!(r.is_active(&TenantId::default_tenant()).await.unwrap());
    }
}
