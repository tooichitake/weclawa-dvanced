//! Shared DB error type — v4.2 minimal stub after rusqlite removal.
//!
//! Real sqlx pool + migrations are in [`crate::storage::db_async`]. This
//! module survives only to host `DbError` (used by all repo trait
//! signatures) + `default_path()` (used by tooling).
//!
//! v4.1 之前 rusqlite + r2d2 + refinery 走这个 module；v4.2 sqlx 全面铺
//! 开后这层不再需要 connection pool / migration runner —— 全 stub。

use std::path::PathBuf;

use crate::storage::state_dir::state_dir;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("connection pool: {0}")]
    Pool(String),
    #[error("schema migration: {0}")]
    Migration(String),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
}

/// Resolve DB file path. Honors `WECLAWBOT_DB_PATH` env var.
pub fn default_path() -> PathBuf {
    if let Ok(val) = std::env::var("WECLAWBOT_DB_PATH") {
        if !val.is_empty() {
            return PathBuf::from(val);
        }
    }
    state_dir().join("state.db")
}
