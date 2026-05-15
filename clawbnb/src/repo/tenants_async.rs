//! AsyncTenantRepo — v7.0 TIMESTAMPTZ for created_at / deleted_at /
//! last_billing_event_at.

use chrono::{DateTime, Utc};

use crate::storage::db::DbError;
use crate::storage::db_async::AsyncDbPool;
use crate::tenancy::{TenantId, TenantStatus};

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AsyncTenant {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub status: String,
    pub stripe_customer_id: Option<String>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub billing_status: String,
    pub billing_period_end: Option<DateTime<Utc>>,
    pub last_billing_event: Option<String>,
    pub last_billing_event_at: Option<DateTime<Utc>>,
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
             FROM tenants WHERE id = $1",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx get tenant: {e}")))
    }

    /// 是否允许处理 inbound — `status='active'` AND
    /// (`billing_status='active'` OR
    ///  (`billing_status='pending'` AND `grace_until > now`)).
    pub async fn is_active(&self, id: &TenantId) -> Result<bool, DbError> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT CASE
                WHEN status != 'active' THEN 0
                WHEN billing_status = 'active' THEN 1
                WHEN billing_status = 'pending'
                     AND grace_until IS NOT NULL
                     AND grace_until > $1 THEN 1
                ELSE 0
             END::BIGINT
             FROM tenants WHERE id = $2",
        )
        .bind(Utc::now())
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx is_active: {e}")))?;
        Ok(row.map(|r| r.0 == 1).unwrap_or(false))
    }

    pub async fn update_billing_status(
        &self,
        id: &TenantId,
        billing_status: &str,
        event_type: &str,
    ) -> Result<bool, DbError> {
        let res = sqlx::query(
            "UPDATE tenants
             SET billing_status = $1,
                 last_billing_event = $2,
                 last_billing_event_at = $3
             WHERE id = $4",
        )
        .bind(billing_status)
        .bind(event_type)
        .bind(Utc::now())
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
             FROM tenants WHERE stripe_customer_id = $1",
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
        let future = Utc::now() + chrono::Duration::days(7);
        sqlx::query("UPDATE tenants SET grace_until = $1 WHERE id = 'default'")
            .bind(future)
            .execute(&r.pool)
            .await
            .unwrap();
        assert!(r.is_active(&TenantId::default_tenant()).await.unwrap());
    }
}
