//! Audit types — v4.2 types-only stub.
//!
//! sqlx async impl lives in [`crate::repo::audit_async`].

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub id: i64,
    pub ts: String,
    pub actor_key_id: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub ip: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AuditInput<'a> {
    pub actor_key_id: Option<&'a str>,
    pub action: &'a str,
    pub target: Option<&'a str>,
    pub before: Option<&'a Value>,
    pub after: Option<&'a Value>,
    pub ip: Option<&'a str>,
}
