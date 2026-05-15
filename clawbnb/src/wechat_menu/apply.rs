//! Settings.json mutation primitives used by command actions.
//!
//! Every write goes through the existing atomic-write infrastructure
//! (`crate::storage::atomic_write::write_json_atomic`) so a partial write
//! never leaves the file half-corrupt. After the write, the **next inbound
//! message** for this user will trigger `Sandbox::ensure` →
//! `sync_user_settings_into_sandbox` which mirrors the updated settings
//! into the sandbox — no manual rematerialize needed here.
//!
//! `BLOCKED_KEYS` enumerates JSON paths the console must never write. These
//! are operator-managed (or otherwise sensitive: API keys, oauth state,
//! our own MCP self-registration). Any write attempt to a blocked path
//! returns `Err`, surfaced as a friendly menu error.

use serde_json::Value;

use crate::storage::json_path;

/// JSON paths the WeChat console is forbidden from touching. Each entry is
/// a sequence of object keys, matched as a prefix: any write whose target
/// path starts with one of these is rejected.
pub const BLOCKED_KEYS: &[&[&str]] = &[
    // Our own MCP server registration — see materialize.rs. Removing it
    // would break the attach/attach_url tools.
    &["mcpServers", "weclawbot"],
    // API keys and auth tokens — never modifiable from the WeChat surface.
    &["env", "ANTHROPIC_API_KEY"],
    &["env", "CLAUDE_TOKEN"],
    &["env", "CLAUDE_API_KEY"],
    &["env", "ANTHROPIC_AUTH_TOKEN"],
    // Account state (.claude.json has oauth fields too — we don't edit
    // .claude.json at all from the console, but listing here for clarity).
    &["oauthAccount"],
    &["apiKey"],
];

fn path_is_blocked(path: &[&str]) -> bool {
    for blocked in BLOCKED_KEYS {
        if path.len() >= blocked.len() && path[..blocked.len()] == **blocked {
            return true;
        }
    }
    false
}

fn read_settings(user_hash: &str) -> Result<Value, String> {
    // Phase 1d: backed by `user_settings`. `None` means the user has
    // never been seeded — return an empty object so callers can write
    // their first field without a stat error.
    Ok(crate::defaults::load_user_settings(user_hash)
        .unwrap_or_else(|| serde_json::json!({})))
}

fn write_settings(user_hash: &str, v: &Value) -> Result<(), String> {
    crate::defaults::save_user_settings(user_hash, v)
}

/// Set (or overwrite) a scalar at a nested path. Intermediate objects are
/// created as needed. Existing siblings are preserved.
pub fn set_field(user_hash: &str, path: &[&str], value: Value) -> Result<(), String> {
    if path_is_blocked(path) {
        return Err(format!("path {:?} is protected and cannot be edited via the console", path));
    }
    let mut root = read_settings(user_hash)?;
    json_path::set_at(&mut root, path, value)?;
    write_settings(user_hash, &root)
}

/// Read a scalar/object at a nested path. Returns `None` if the path doesn't
/// resolve. This is read-only so it bypasses the blocked-key check (used by
/// `show` commands).
pub fn get_field(user_hash: &str, path: &[&str]) -> Option<Value> {
    let mut cur = read_settings(user_hash).ok()?;
    for key in path {
        let next = cur.as_object_mut()?.remove(*key)?;
        cur = next;
    }
    Some(cur)
}

// v7.0 housekeeping: `array_add_unique` / `array_remove` removed —
// the WeChat menu doesn't currently expose any list-management commands,
// so the wrappers had zero callers. The lower-level helpers in
// `storage::json_path` were also removed for the same reason. If a
// future menu command needs list-add/remove, re-add the wrapper here
// (the `path_is_blocked` check must come back too) and the underlying
// `serde_json::Value::as_array_mut` is enough — no need to revive a
// dedicated helper module.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_keys_match_prefix() {
        assert!(path_is_blocked(&["mcpServers", "weclawbot"]));
        assert!(path_is_blocked(&["env", "ANTHROPIC_API_KEY"]));
        assert!(path_is_blocked(&["oauthAccount"]));
        assert!(!path_is_blocked(&["mcpServers", "other-server"]));
        assert!(!path_is_blocked(&["env", "DEBUG"]));
        assert!(!path_is_blocked(&["model"]));
    }
}
