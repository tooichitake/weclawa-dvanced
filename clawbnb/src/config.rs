//! Typed schema for `~/.weclawbot/config.json` — the **operator-managed**
//! global daemon configuration (not the per-user `settings.json` that
//! Claude reads inside each sandbox).
//!
//! Previously this file was parsed as `serde_json::Value` and accessed via
//! ~13 hardcoded `.pointer("/ai/timeoutMs")` strings scattered across the
//! codebase. A schema change in one place silently broke all the others.
//! The struct here makes the schema explicit:
//!
//! ```ignore
//! cfg.ai.timeout_ms        // typed u64, default 300_000
//! cfg.webhook.url          // typed String, default ""
//! cfg.agent_binding.enabled
//! ```
//!
//! ## Forward / backward compatibility
//!
//! - `#[serde(default)]` on every nested struct + every field — missing
//!   keys parse cleanly and pick up the type-level defaults. Operators
//!   never need to add new fields manually after an upgrade.
//! - `#[serde(rename_all = "camelCase")]` matches the existing on-disk
//!   format so this is a drop-in replacement for the JSON layout.
//! - `Config::load()` always returns a valid struct; corrupted JSON is
//!   logged at warn level and replaced with defaults.
//!
//! ## Migrations
//!
//! `Config::load_and_migrate()` runs all idempotent one-time bumps before
//! returning the struct. Currently:
//! - `ai.timeoutMs` 60_000 → 300_000 (operators on the old short window)
//!
//! Add new migrations in `apply_migrations()` below.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::storage::atomic_write::write_json_atomic;
use crate::storage::state_dir::config_path;

const DEFAULT_MAX_AGENTS: usize = 20;
const DEFAULT_AI_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_WEBHOOK_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_HISTORY_LIMIT: usize = 20;

const OLD_AI_TIMEOUT_MS: u64 = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub ai: AiConfig,
    pub echo: EchoConfig,
    pub webhook: WebhookConfig,
    pub sandbox: SandboxConfig,
    pub agent_binding: AgentBindingConfig,
    pub rate_limit: RateLimitConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RateLimitConfig {
    /// 每个 WeChat 用户每分钟入站消息上限。超过的用户直接收节流提示，
    /// 不进 sandbox。0 = 不限。daemon 每次 inbound 都从 config 读，
    /// 改完写入 config.json 立刻生效，不用重启。
    pub user_per_minute: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            user_per_minute: 30,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiProvider {
    /// Local `claude` CLI (Anthropic). Sandbox-spawned per-message.
    Claude,
    /// Local `codex` CLI (OpenAI). Sandbox-spawned per-message.
    Codex,
    /// HTTP-only mode using `ai.apiKey` / `ai.baseUrl`. No sandbox spawn.
    Api,
}

impl Default for AiProvider {
    fn default() -> Self {
        Self::Claude
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AiConfig {
    pub enabled: bool,
    pub provider: AiProvider,
    /// Model alias. For `provider=claude`: `sonnet` / `opus` / `haiku`.
    /// For `provider=codex`: model name accepted by codex CLI.
    /// For `provider=api`: OpenAI-compatible model id.
    pub model: String,
    pub system_prompt: String,
    pub history_limit: usize,
    pub timeout_ms: u64,
    /// Only used when `provider=api`. Endpoint for OpenAI-compatible HTTP API.
    pub base_url: String,
    /// Only used when `provider=api`. API key for the endpoint.
    pub api_key: String,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: AiProvider::Claude,
            model: "sonnet".to_string(),
            system_prompt: "你是一个友好的微信助手，用简洁的中文回答用户问题。".to_string(),
            history_limit: DEFAULT_HISTORY_LIMIT,
            timeout_ms: DEFAULT_AI_TIMEOUT_MS,
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EchoConfig {
    pub enabled: bool,
    pub prefix: String,
}

impl Default for EchoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            prefix: "[echo] ".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WebhookConfig {
    /// If non-empty, inbound messages are POSTed here and the response's
    /// `reply` field is sent back to WeChat. Takes priority over AI providers.
    pub url: String,
    pub timeout_ms: u64,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            timeout_ms: DEFAULT_WEBHOOK_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    /// Container image used to spawn per-user gVisor sandboxes.
    pub image: String,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            image: "localhost/weclawbot-sandbox-base:dev".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentBindingConfig {
    pub enabled: bool,
    pub max_agents: usize,
}

impl Default for AgentBindingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_agents: DEFAULT_MAX_AGENTS,
        }
    }
}

// --- v2.1.B1: process-wide cached config (ArcSwap) ------------------------
//
// 旧实现 `Config::load()` 每次都 `fs::read_to_string` — handler.rs 每条
// inbound 跑 2 次（早期 cfg + 主 cfg），100 QPS 即 200 syscall/s 阻塞
// async runtime。新接口 `Config::cached()` 走进程级 ArcSwap，0 IO，
// `O(1)` clone Arc。daemon 启动时 spawn 一个 notify watcher 监听
// config.json mtime 变化时自动 reload；HTTP PUT /api/v1/config 写盘
// 后主动 `set_cached()` 一次避免等 watcher 延迟。

use std::sync::OnceLock;

use arc_swap::ArcSwap;

static CACHED: OnceLock<ArcSwap<Config>> = OnceLock::new();

impl Config {
    /// Return the currently cached config (0 IO). On first call —
    /// before daemon boot installs the cache — fall back to a one-shot
    /// load from disk so CLI subcommands still see real values.
    pub fn cached() -> std::sync::Arc<Config> {
        match CACHED.get() {
            Some(swap) => swap.load_full(),
            None => std::sync::Arc::new(Self::load()),
        }
    }

    /// Install the process-wide cache. Called once by daemon `cli::start`
    /// after the config file is known to exist. Second call replaces
    /// the inner Config atomically (used by PUT /api/v1/config).
    pub fn set_cached(cfg: Config) {
        let arc = std::sync::Arc::new(cfg);
        match CACHED.get() {
            Some(swap) => swap.store(arc),
            None => {
                let _ = CACHED.set(ArcSwap::new(arc));
            }
        }
    }

    /// Force a reload from disk into the cache. Idempotent. Called by
    /// the notify watcher on file mtime change, and by daemon boot.
    pub fn reload_cached() {
        Self::set_cached(Self::load());
    }

    /// Load + parse. Missing / corrupted file returns defaults (and
    /// the on-disk file is left untouched so the operator can fix it).
    pub fn load() -> Self {
        let path = config_path();
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<Self>(&raw) {
                Ok(cfg) => cfg,
                Err(e) => {
                    warn!("config parse failed ({}): {e}; using defaults", path.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Atomic write of the typed config back to disk.
    pub fn save(&self) -> Result<(), String> {
        let value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        write_json_atomic(&config_path(), &value).map_err(|e| e.to_string())
    }

    /// Ensure `~/.weclawbot/config.json` exists. If absent, write defaults.
    /// Returns the (now-existing) loaded config.
    pub fn ensure_exists() -> Self {
        let path = config_path();
        if !path.exists() {
            let cfg = Self::default();
            if let Err(e) = cfg.save() {
                warn!("failed to write initial config: {e}");
            }
            return cfg;
        }
        Self::load()
    }

    /// One-shot apply all idempotent migrations on the on-disk config.
    /// Returns the list of migration names that fired. Safe to call on every
    /// daemon start.
    pub fn apply_migrations() -> Vec<&'static str> {
        let mut fired = Vec::new();
        let mut cfg = Self::load();

        // Migration: ai.timeoutMs 60_000 → 300_000 (operators on the old
        // short window before we discovered PPT generation needs 5min).
        if cfg.ai.timeout_ms == OLD_AI_TIMEOUT_MS {
            cfg.ai.timeout_ms = DEFAULT_AI_TIMEOUT_MS;
            fired.push("ai_timeout_60s_to_300s");
        }

        // (Add future migrations above this comment.)

        if !fired.is_empty() {
            if let Err(e) = cfg.save() {
                warn!("migration save failed: {e}");
            }
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_tmp(value: serde_json::Value) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(serde_json::to_string_pretty(&value).unwrap().as_bytes())
            .unwrap();
        f
    }

    #[test]
    fn defaults_are_sensible() {
        let c = Config::default();
        assert_eq!(c.ai.provider, AiProvider::Claude);
        assert_eq!(c.ai.model, "sonnet");
        assert_eq!(c.ai.timeout_ms, 300_000);
        assert_eq!(c.agent_binding.max_agents, 20);
        assert_eq!(c.webhook.url, "");
    }

    #[test]
    fn empty_object_parses_with_all_defaults() {
        let f = write_tmp(json!({}));
        let c = Config::load_from(f.path());
        assert_eq!(c.ai.timeout_ms, 300_000);
        assert_eq!(c.webhook.url, "");
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let f = write_tmp(json!({
            "ai": { "model": "opus", "timeoutMs": 600000 }
        }));
        let c = Config::load_from(f.path());
        assert_eq!(c.ai.model, "opus");
        assert_eq!(c.ai.timeout_ms, 600_000);
        // Other ai fields fall back to defaults:
        assert_eq!(c.ai.history_limit, 20);
        assert_eq!(c.echo.enabled, false);
    }

    #[test]
    fn corrupted_file_falls_back_to_defaults() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"this is not json").unwrap();
        let c = Config::load_from(f.path());
        assert_eq!(c.ai.timeout_ms, 300_000);
    }

    #[test]
    fn camel_case_round_trip() {
        let c = Config {
            ai: AiConfig {
                timeout_ms: 12345,
                ..Default::default()
            },
            ..Default::default()
        };
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains("\"timeoutMs\":12345"));
        assert!(s.contains("\"agentBinding\""));
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.ai.timeout_ms, 12345);
    }

    #[test]
    fn provider_enum_serde() {
        let c: Config = serde_json::from_value(json!({
            "ai": { "provider": "codex" }
        }))
        .unwrap();
        assert_eq!(c.ai.provider, AiProvider::Codex);
        let back = serde_json::to_string(&c.ai.provider).unwrap();
        assert_eq!(back, r#""codex""#);
    }
}
