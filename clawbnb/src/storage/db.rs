//! Shared DB error type — v7.0 minimal stub after rusqlite removal.
//!
//! Real sqlx pool + migrations live in [`crate::storage::db_async`]. This
//! module survives only to host `DbError` (used by all repo trait
//! signatures + the `?` glue between sqlx and the domain error layer).

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
}
