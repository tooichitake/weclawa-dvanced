//! Operator-facing `weclawbot config show/set/echo/webhook` CLI.
//!
//! This is the **escape-hatch** for poking arbitrary JSON fields in
//! `~/.weclawbot/config.json`. For typed access from inside the daemon use
//! `crate::config::Config` instead — that path is type-safe and partial-
//! schema-tolerant. This CLI deliberately stays untyped so an operator can
//! reach experimental / unreleased fields without us having to add a typed
//! struct for each one.
//!
//! BLOCKED_KEYS guards specific paths that even the operator's CLI must not
//! freely poke (auth tokens, the daemon's own MCP registration, etc.).
//! Phase 0.3 of the v2 hardening plan.

use std::fs;

use serde_json::{json, Value};

use crate::storage::atomic_write::write_json_atomic;
use crate::storage::json_path::set_at_pointer;
use crate::storage::state_dir::{config_path, ensure_dirs};

/// JSON-pointer prefixes the `weclawbot config set <path> ...` CLI refuses
/// to touch. Aligned with `wechat_menu::apply::BLOCKED_KEYS` plus operator-
/// specific guards. Anything starting with one of these prefixes is rejected.
///
/// To change these paths you go through a dedicated subcommand (e.g. account
/// rotation goes through `weclawbot login`, not `config set`).
const BLOCKED_PATH_PREFIXES: &[&str] = &[
    // API credentials — change via dedicated `weclawbot ai` or `login` cmds.
    "/ai/apiKey",
    "/ai/api_key",
    "/env/ANTHROPIC_API_KEY",
    "/env/ANTHROPIC_AUTH_TOKEN",
    "/env/CLAUDE_API_KEY",
    "/env/CLAUDE_TOKEN",
    "/env/OPENAI_API_KEY",
    // Account state — managed by QR login + auth/accounts/, not the CLI.
    "/accounts",
    "/oauthAccount",
    // weclawbot's own MCP server registration — modifying breaks the bot.
    "/mcpServers/weclawbot",
    // Defaults file path / DB path / etc. — internal layout, not config.
    "/storage",
    "/_weclawbotManaged",
];

fn check_path_allowed(pointer: &str) -> Result<(), String> {
    for blocked in BLOCKED_PATH_PREFIXES {
        if pointer == *blocked || pointer.starts_with(&format!("{blocked}/")) {
            return Err(format!(
                "path '{pointer}' is protected and cannot be set via `config set`. \
                 Use the appropriate dedicated subcommand (`weclawbot login`, \
                 `weclawbot ai`, GUI Accounts tab, etc.) — or, if you absolutely \
                 must, edit ~/.weclawbot/config.json by hand."
            ));
        }
    }
    Ok(())
}

pub async fn show() -> Result<(), String> {
    ensure_dirs().map_err(|e| format!("init dirs: {e}"))?;
    // Make sure the file at least exists with default contents — so a fresh
    // host shows non-empty output.
    let _ = crate::config::Config::ensure_exists();

    let path = config_path();
    let raw = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let parsed: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("parse {}: {e}", path.display()))?;
    println!("Config file: {}", path.display());
    println!(
        "{}",
        serde_json::to_string_pretty(&parsed).map_err(|e| e.to_string())?
    );
    Ok(())
}

pub async fn set(key: &str, value: &str) -> Result<(), String> {
    ensure_dirs().map_err(|e| format!("init dirs: {e}"))?;
    let _ = crate::config::Config::ensure_exists();

    let path = config_path();
    let raw = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut cfg: Value =
        serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;

    let parsed = parse_value(value);
    let pointer = key_to_pointer(key);

    check_path_allowed(&pointer)?;

    set_at_pointer(&mut cfg, &pointer, parsed)
        .map_err(|e| format!("invalid path '{pointer}': {e}"))?;
    write_json_atomic(&path, &cfg).map_err(|e| format!("write {}: {e}", path.display()))?;

    println!("Updated {key} in {}", path.display());
    println!(
        "{}",
        serde_json::to_string_pretty(cfg.pointer(&pointer).unwrap_or(&Value::Null))
            .unwrap_or_default()
    );
    Ok(())
}

pub async fn echo(on: bool) -> Result<(), String> {
    set("echo.enabled", &on.to_string()).await
}

pub async fn webhook(url: &str) -> Result<(), String> {
    set("webhook.url", url).await
}

fn parse_value(raw: &str) -> Value {
    if raw == "true" {
        return json!(true);
    }
    if raw == "false" {
        return json!(false);
    }
    if let Ok(n) = raw.parse::<i64>() {
        return json!(n);
    }
    if let Ok(f) = raw.parse::<f64>() {
        return json!(f);
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
        return parsed;
    }
    json!(raw)
}

fn key_to_pointer(key: &str) -> String {
    if key.starts_with('/') {
        key.to_string()
    } else {
        format!("/{}", key.replace('.', "/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_api_key_paths() {
        assert!(check_path_allowed("/ai/apiKey").is_err());
        assert!(check_path_allowed("/env/ANTHROPIC_API_KEY").is_err());
        assert!(check_path_allowed("/env/OPENAI_API_KEY").is_err());
    }

    #[test]
    fn blocks_account_writes() {
        assert!(check_path_allowed("/accounts").is_err());
        assert!(check_path_allowed("/accounts/foo-im-bot").is_err());
        assert!(check_path_allowed("/oauthAccount").is_err());
    }

    #[test]
    fn blocks_weclawbot_mcp_self_registration() {
        assert!(check_path_allowed("/mcpServers/weclawbot").is_err());
        assert!(check_path_allowed("/mcpServers/weclawbot/command").is_err());
        // But operator can configure OTHER mcp servers:
        assert!(check_path_allowed("/mcpServers/some-other-server").is_ok());
    }

    #[test]
    fn allows_normal_config_paths() {
        assert!(check_path_allowed("/ai/model").is_ok());
        assert!(check_path_allowed("/ai/timeoutMs").is_ok());
        assert!(check_path_allowed("/webhook/url").is_ok());
        assert!(check_path_allowed("/echo/enabled").is_ok());
        assert!(check_path_allowed("/sandbox/image").is_ok());
        assert!(check_path_allowed("/agentBinding/maxAgents").is_ok());
    }

    #[test]
    fn prefix_match_does_not_overreach() {
        // "/ai" alone is fine; the blocklist only triggers on /ai/apiKey.
        assert!(check_path_allowed("/ai").is_ok());
        // "/accountsX" (a hypothetical sibling key) is fine — only "/accounts"
        // exact + "/accounts/..." trigger.
        assert!(check_path_allowed("/accountsXyz").is_ok());
    }

    #[test]
    fn key_to_pointer_dot_path() {
        assert_eq!(key_to_pointer("ai.model"), "/ai/model");
        assert_eq!(key_to_pointer("/ai/model"), "/ai/model");
        assert_eq!(key_to_pointer("echo.enabled"), "/echo/enabled");
    }
}
