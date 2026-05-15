//! RateLimit types — v4.2 types-only stub.
//!
//! sqlx async impl lives in [`crate::repo::rate_limits_async`].

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowCount {
    pub scope_key: String,
    pub window_start_ts: String,
    pub count: u64,
}
