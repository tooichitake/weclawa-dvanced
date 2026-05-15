//! Sync the user-owned `claude-cli settings.json` into the sandbox AND inject
//! the weclawbot MCP server registration into the sandbox's `.claude.json`.
//!
//! In the per-user-autonomous model, the file at
//! `~/.weclawbot/users/<u-hash>/settings.json` is the canonical source for
//! claude-cli runtime settings (permissions, plugins, theme, env). We mirror
//! it verbatim into `<sandbox>/home/.claude/settings.json`.
//!
//! Separately, Claude Code reads MCP server registrations from
//! `<sandbox>/home/.claude.json` (the *user-scope* MCP config — NOT
//! `settings.json`, which the CLI ignores for MCP). Empirically verified via
//! `claude mcp add -s user ... → File modified: /home/claude/.claude.json`.
//! We merge our entry into whatever else is in that file (oauth state etc.).

use std::fs;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::sandbox::layout;
use crate::storage::atomic_write::write_json_atomic;

/// Path to the `weclawbot-mcp` binary inside the sandbox image. Baked there
/// by `build/Dockerfile`'s mcp-builder stage. Must match the COPY target.
const MCP_BINARY_IN_SANDBOX: &str = "/usr/local/bin/weclawbot-mcp";

pub fn sync_user_settings_into_sandbox(user_hash: &str) -> Result<(), String> {
    // Phase 1d: the canonical settings now live in the DB
    // (`user_settings` table). The sandbox copy at
    // `<sandbox>/home/.claude/settings.json` is materialized from DB on
    // every Sandbox::ensure call so claude-cli still reads a real file.
    let mut parsed: Value = crate::defaults::load_user_settings(user_hash)
        .ok_or_else(|| format!("no settings row for {user_hash}"))?;

    // Tag for inspectability — never read back.
    if let Some(obj) = parsed.as_object_mut() {
        obj.insert("_weclawbotManaged".into(), Value::Bool(true));
        obj.insert(
            "_weclawbotSyncedAt".into(),
            Value::String(chrono::Utc::now().to_rfc3339()),
        );

        // Allow the MCP tools without per-message permission prompts so Claude
        // can call them mid-turn without the host having to grant interactive
        // approval. The MCP server validates inputs server-side anyway.
        let mut permissions = obj
            .remove("permissions")
            .and_then(|v| match v {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default();
        let mut allow: Vec<Value> = permissions
            .remove("allow")
            .and_then(|v| match v {
                Value::Array(arr) => Some(arr),
                _ => None,
            })
            .unwrap_or_default();
        for tool in [
            "mcp__weclawbot__attach",
            "mcp__weclawbot__attach_url",
        ] {
            let needle = Value::String(tool.to_string());
            if !allow.contains(&needle) {
                allow.push(needle);
            }
        }
        permissions.insert("allow".into(), Value::Array(allow));
        obj.insert("permissions".into(), Value::Object(permissions));
    }

    let home_claude = layout::user_sandbox_root(user_hash)
        .join("home")
        .join(".claude");
    let settings_dest = home_claude.join("settings.json");
    fs::create_dir_all(&home_claude).map_err(|e| format!("mkdir {}: {e}", home_claude.display()))?;
    write_json_atomic(&settings_dest, &parsed)
        .map_err(|e| format!("write {}: {e}", settings_dest.display()))?;

    // Inject the MCP registration into `.claude.json`. This is the file
    // Claude Code actually consults for user-scope MCP servers; settings.json
    // is ignored for MCP.
    let claude_json_path = layout::user_sandbox_root(user_hash)
        .join("home")
        .join(".claude.json");
    inject_mcp_registration(&claude_json_path)
        .map_err(|e| format!("inject mcp into {}: {e}", claude_json_path.display()))?;

    Ok(())
}

/// Merge `mcpServers.weclawbot` into the sandbox's `.claude.json`. If the
/// file doesn't exist yet (first run before claude has written it), we seed
/// it with a minimal object. If it does exist, we preserve every other field
/// (oauth state, growthbook cache, onboarding flags, etc.) and replace only
/// our entry under `mcpServers`.
fn inject_mcp_registration(path: &Path) -> Result<(), String> {
    let mut root: Value = if path.exists() {
        let raw = fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
        if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&raw).map_err(|e| format!("parse: {e}"))?
        }
    } else {
        json!({})
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| ".claude.json is not a JSON object".to_string())?;

    let mut servers: Map<String, Value> = obj
        .remove("mcpServers")
        .and_then(|v| match v {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default();

    // The `type: "stdio"` field is what `claude mcp add -s user` writes —
    // matching the exact shape avoids any chance of schema-strict validation
    // rejecting the entry on Claude Code upgrades.
    servers.insert(
        "weclawbot".into(),
        json!({
            "type": "stdio",
            "command": MCP_BINARY_IN_SANDBOX,
            "args": [],
            "env": {}
        }),
    );
    obj.insert("mcpServers".into(), Value::Object(servers));

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    write_json_atomic(path, &root).map_err(|e| format!("write: {e}"))
}
