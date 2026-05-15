//! Repository layer — typed CRUD over the SQLite state.
//!
//! Each table in `src/storage/migrations/V0001__init.sql` gets a
//! `Repo` trait + one `Sqlite<X>Repo` implementation here. Higher layers
//! (handler, routes, CLI) call through the trait, which keeps the
//! database backend swappable (future PostgreSQL or pure-mock for tests).
//!
//! The trait surfaces are intentionally narrow — each method maps to one
//! SQL statement. Cross-table queries get their own method (no leaking
//! `Connection` to callers).

pub mod accounts;
pub mod accounts_async;
pub mod admin_keys;
pub mod admin_keys_async;
pub mod audit;
pub mod audit_async;
pub mod bindings;
pub mod bindings_async;
pub mod dedup;
pub mod dedup_async;
pub mod defaults;
pub mod defaults_async;
pub mod rate_limits_async;
pub mod tenants_async;
pub mod trust;
pub mod trust_async;
pub mod users;
pub mod users_async;
