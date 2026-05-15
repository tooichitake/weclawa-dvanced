//! Operator-facing `weclawbot ai ...` subcommand — set provider / model /
//! credentials in `~/.weclawbot/config.json` from the terminal.
//!
//! Migrated off raw JSON pointer paths in favor of the typed
//! `crate::config::Config` struct.

use crate::config::{AiProvider, Config};
use crate::storage::state_dir::{config_path, ensure_dirs};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    provider: Option<String>,
    binary: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    system_prompt: Option<String>,
    off: bool,
) -> Result<(), String> {
    ensure_dirs().map_err(|e| format!("init dirs: {e}"))?;
    let mut cfg = Config::ensure_exists();

    if off {
        cfg.ai.enabled = false;
        cfg.echo.enabled = false;
        cfg.save()?;
        println!("AI disabled.");
        return Ok(());
    }

    // Provider switch (claude / codex / api).
    if let Some(p) = provider.as_deref() {
        cfg.ai.provider = match p {
            "claude" => AiProvider::Claude,
            "codex" => AiProvider::Codex,
            "api" | "openai" | "openai-compat" => AiProvider::Api,
            other => return Err(format!(
                "unknown provider '{other}' (expected claude|codex|api)"
            )),
        };
    }

    // NOTE: the old `--binary` flag wrote `ai.binary` into the on-disk
    // schema. The typed schema doesn't carry that field — provider name
    // (`claude` / `codex`) maps directly to a binary name baked into the
    // sandbox image. If `binary` is supplied we currently warn and ignore
    // it; a future re-introduction would add `binary_override: Option<String>`
    // to `config::AiConfig`.
    if let Some(b) = binary {
        if !b.is_empty() {
            eprintln!(
                "warning: --binary is no longer supported in the typed schema; \
                 ignoring '{b}'. The provider name picks the binary."
            );
        }
    }

    if let Some(v) = base_url {
        cfg.ai.base_url = v;
    }
    if let Some(v) = api_key {
        cfg.ai.api_key = v;
    }
    if let Some(v) = model {
        cfg.ai.model = v;
    }
    if let Some(v) = system_prompt {
        cfg.ai.system_prompt = v;
    }

    // Per-mode validation.
    match cfg.ai.provider {
        AiProvider::Claude | AiProvider::Codex => {
            // CLI mode: no API key required.
        }
        AiProvider::Api => {
            if cfg.ai.api_key.is_empty() {
                return Err(
                    "API mode requires --api-key. Or use --provider claude / --provider codex for CLI mode."
                        .into(),
                );
            }
        }
    }

    cfg.ai.enabled = true;
    cfg.echo.enabled = false;
    cfg.save()?;

    println!("AI reply enabled.");
    println!("  Config:   {}", config_path().display());
    match cfg.ai.provider {
        AiProvider::Claude => {
            println!("  Provider: claude (CLI subscription)");
            println!("  Binary:   claude");
            println!("  Model:    {}", cfg.ai.model);
        }
        AiProvider::Codex => {
            println!("  Provider: codex (CLI subscription)");
            println!("  Binary:   codex");
            println!("  Model:    {}", cfg.ai.model);
        }
        AiProvider::Api => {
            println!("  Provider: openai-compatible API");
            println!("  baseUrl:  {}", cfg.ai.base_url);
            println!("  model:    {}", cfg.ai.model);
        }
    }
    println!();
    println!("Send a message in WeChat to test. Config is read on each message (no restart needed).");
    Ok(())
}
