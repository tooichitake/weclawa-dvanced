//! User-related types — v4.2 types-only stub.
//!
//! Sync repo trait + rusqlite impl removed; see [`crate::repo::users_async`]
//! for the production sqlx async repo. This file now only defines the
//! data types that callsites pass around — they're orthogonal to the
//! impl choice.

use crate::ids::UserHash;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserProfile {
    pub hash: UserHash,
    pub user_id_hint: Option<String>,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub message_count: u64,
    pub sync_state: String,
    pub last_sync_at: Option<String>,
    pub last_sync_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HistoryTurn {
    pub role: String, // "user" | "assistant"
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConsoleSession {
    pub in_menu: bool,
    pub current_path: Vec<String>,
    pub last_input_at: String,
}
