//! Trust types — v4.2 types-only stub.
//!
//! sqlx async impl lives in [`crate::repo::trust_async`].

use crate::tenancy::trust::{TrustInputs, TrustTier};

#[derive(Debug, Clone, PartialEq)]
pub struct TrustSnapshot {
    pub user_hash: String,
    pub tenant_id: String,
    pub inputs: TrustInputs,
    pub score: f64,
    pub tier: TrustTier,
    pub updated_at: String,
    pub tier_since: String,
}
