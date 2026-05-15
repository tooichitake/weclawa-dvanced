//! Tenant types — v4.2 types-only stub.
//!
//! sqlx async impl lives in [`crate::repo::tenants_async`].

use crate::tenancy::TenantStatus;

#[derive(Debug, Clone, PartialEq)]
pub struct Tenant {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub status: TenantStatus,
    pub stripe_customer_id: Option<String>,
    pub deleted_at: Option<String>,
    pub billing_status: String,
    pub billing_period_end: Option<String>,
    pub last_billing_event: Option<String>,
    pub last_billing_event_at: Option<String>,
}
