//! WeChat ↔ AI message-handling pipeline.
//!
//! Responsibilities split across submodules:
//!
//! - `poller`   — long-poll iLink `getUpdates`, deliver to `handler`.
//! - `handler`  — orchestrator: dedup, sandbox, console route, AI dispatch,
//!                reply, file forwarding.
//! - `dedup`    — process-local SEEN_MSGS guard.
//! - `reply`    — build + send a plain text bot reply.
//! - `webhook`  — outbound webhook reply path (config.webhook.url).
//! - `forward`  — MCP `attach` files + filesystem-diff fallback + URL forward.

pub mod common;
pub mod dedup;
pub mod forward;
pub mod handler;
pub mod telegram_poller;
pub mod poller;
pub mod rate_limit;
pub mod reply;
pub mod typing;
pub mod webhook;
