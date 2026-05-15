//! PTY-driven automation of the interactive `claude` CLI.
//!
//! `claude -p` (non-interactive print mode) cannot run slash commands like
//! `/plugin install` — those only exist inside the TUI. To install/uninstall
//! plugins programmatically we spawn `claude` under a real pseudoterminal,
//! feed it slash commands, and watch the output for completion markers.
//!
//! The automation runs **inside the per-user gVisor container** by going
//! through `podman run` (no `-p`), so any plugin install lands in that
//! user's writable `plugins/cache/` directory.

pub mod claude_cli;
