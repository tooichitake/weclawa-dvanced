//! Sync ↔ async bridge — single survivor `block_on_async`.
//!
//! `block_on_async` lets sync code (CLI subcommands, `defaults.rs`,
//! `Sandbox::ensure`, etc.) call sqlx async repos. Implementation:
//!
//! - If there's a tokio runtime on the current thread (daemon hot path),
//!   we hand the worker's reactor to another worker via `block_in_place`
//!   and safely block the current thread waiting on the future.
//! - If there's no runtime (cold CLI invocation), we spin up a one-shot
//!   `current_thread` runtime and drive the future on it.
//!
//! v7.0 housekeeping: removed `run` / `try_run` wrappers around
//! `spawn_blocking` — there were zero callers left after the async-first
//! refactor. If you need them back, use `tokio::task::spawn_blocking`
//! directly; the wrapper was only saving one line of `JoinError` mapping.

pub fn block_on_async<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build oneshot runtime")
            .block_on(fut),
    }
}
