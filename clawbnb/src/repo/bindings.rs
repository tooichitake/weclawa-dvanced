//! Binding types — v4.2 types-only stub.
//!
//! sqlx async impl lives in [`crate::repo::bindings_async`].

use crate::ids::{AccountId, WeixinUserId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub weixin_user_id: WeixinUserId,
    pub active_account_id: AccountId,
    pub agent_id: String,
    pub updated_at: String,
}
