//! WeChat-driven slash-command console.
//!
//! A WeChat user sends `/menu` to enter "console mode". While in this mode
//! every subsequent message is dispatched to the command tree below
//! (`tree::root()`); the AI / claude pipeline is short-circuited. The user
//! navigates by typing menu names, numbers, or fully-qualified paths
//! (`/model set sonnet`); they leave by typing `/exit`.
//!
//! Scope is intentionally narrow:
//!   - **Claude Code / Codex CLI settings only** — model, system-prompt,
//!     permissions, plugins, MCP servers, output-style, history-limit,
//!     timeout.
//!   - **NEVER** account-level commands (login, logout, switch-account,
//!     API key, OAuth, credentials). These are not in the tree.
//!   - **NEVER** host-level commands (restart daemon, view logs, podman,
//!     sandbox image, runsc). These are not in the tree.
//!   - **NEVER** spawn the Claude binary interactively. All effects are
//!     plain JSON edits to `~/.weclawbot/users/<u-hash>/settings.json`.
//!     The next inbound message lets `materialize.rs` mirror those changes
//!     into the sandbox.
//!
//! See `tree.rs` for the literal command list and `apply.rs` for the write
//! primitives. `session.rs` owns per-user state (mode, current path,
//! last activity). `dispatch.rs` is the input → reply state machine.

pub mod apply;
pub mod dispatch;
pub mod session;
pub mod tree;

use tracing::debug;

/// Result of routing a WeChat inbound through the console. Used by
/// `monitor::handler` to decide whether the message was a console interaction
/// (in which case the AI pipeline should NOT run) or fell through to chat.
pub enum ConsoleOutcome {
    /// We handled the input. Caller sends `reply` to the WeChat user as a
    /// regular text message and does NOT run the AI pipeline. Caller also
    /// does NOT run the filesystem-diff fallback.
    Handled { reply: String },
    /// Input was not a console command and the user is not in menu mode.
    /// Caller should run the normal AI pipeline.
    PassThrough,
}

/// Route a single inbound text. Returns `Handled` when we owned the input,
/// `PassThrough` when the AI pipeline should continue.
///
/// Entry triggers:
///   - `text == "/menu"` regardless of state → enter menu mode at root.
///   - User is already in menu mode → every message is a command, even if
///     it doesn't start with `/`.
pub fn route(user_hash: &str, text: &str) -> ConsoleOutcome {
    let trimmed = text.trim();
    session::gc_expired(user_hash);
    let in_menu = session::is_in_menu_mode(user_hash);

    if !in_menu && trimmed != "/menu" {
        return ConsoleOutcome::PassThrough;
    }

    let reply = dispatch::dispatch(user_hash, trimmed);
    debug!("console reply ({}): {}", user_hash, reply.lines().next().unwrap_or(""));
    ConsoleOutcome::Handled { reply }
}
