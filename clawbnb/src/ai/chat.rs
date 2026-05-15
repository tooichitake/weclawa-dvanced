use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::history::{append, recent, ChatTurn};
use crate::config::{AiConfig as GlobalAiConfig, AiProvider};
use crate::sandbox::Sandbox;

/// Subset of the global `AiConfig` that the API-only (OpenAI-compatible
/// HTTP) reply path needs. Renamed from `AiConfig` to disambiguate from
/// `crate::config::AiConfig` after the typed-config migration.
#[derive(Debug, Clone)]
pub struct ChatProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub system_prompt: String,
    pub history_limit: usize,
    pub timeout_ms: u64,
}

impl ChatProviderConfig {
    /// Construct from the typed global config. Returns `None` (skip this
    /// provider) when:
    ///   - AI is disabled
    ///   - provider isn't `Api`
    ///   - API key is empty
    pub fn from_global(c: &GlobalAiConfig) -> Option<Self> {
        if !c.enabled || c.provider != AiProvider::Api {
            return None;
        }
        if c.api_key.is_empty() {
            return None;
        }
        Some(Self {
            base_url: c.base_url.trim_end_matches('/').to_string(),
            api_key: c.api_key.clone(),
            model: c.model.clone(),
            system_prompt: c.system_prompt.clone(),
            history_limit: c.history_limit,
            timeout_ms: c.timeout_ms,
        })
    }
}

#[derive(Serialize)]
struct Msg<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Msg<'a>>,
    stream: bool,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

pub async fn complete(
    cfg: &ChatProviderConfig,
    sandbox: &Sandbox,
    user_text: &str,
) -> Result<String, String> {
    let user_hash = sandbox.user_hash.as_str();

    // v7.3 — PII scrub on outbound API prompt. Same policy + Block
    // behavior as `crate::ai::cli_provider::complete_with_content`.
    // Permissive mode = no-op walk; Strict mode = redact/block.
    let pii_policy = crate::config::Config::cached()
        .compliance
        .ai_prompt_policy();
    let scrub = crate::pii::scrub_with_policy(user_text, &pii_policy);
    if scrub.blocked {
        tracing::warn!(
            "chat::complete blocked for {user_hash} by PII policy ({} hits)",
            scrub.hits.len()
        );
        metrics::counter!(
            "weclawbot_ai_prompt_blocked_total",
            "reason" => "pii_block_policy"
        )
        .increment(1);
        return Err("(消息中含敏感信息，已被合规策略拒绝处理)".to_string());
    }
    let user_text = scrub.scrubbed.as_str();

    append(user_hash, "user", user_text).await;
    let turns: Vec<ChatTurn> = recent(user_hash, cfg.history_limit).await;

    let mut messages: Vec<Msg> = Vec::with_capacity(turns.len() + 1);
    if !cfg.system_prompt.is_empty() {
        messages.push(Msg {
            role: "system",
            content: &cfg.system_prompt,
        });
    }
    for t in &turns {
        messages.push(Msg {
            role: &t.role,
            content: &t.content,
        });
    }

    let req = ChatRequest {
        model: &cfg.model,
        messages,
        stream: false,
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(cfg.timeout_ms))
        .build()
        .map_err(|e| format!("http build: {e}"))?;

    let url = format!("{}/chat/completions", cfg.base_url);
    debug!("AI -> {url} model={} msgs={}", cfg.model, turns.len());

    let resp = client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .header("Content-Type", "application/json")
        .json(&req)
        .send()
        .await
        .map_err(|e| format!("ai request: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("ai body: {e}"))?;
    if !status.is_success() {
        warn!("AI {status}: {body}");
        return Err(format!("AI {status}: {body}"));
    }

    let parsed: ChatResponse =
        serde_json::from_str(&body).map_err(|e| format!("parse AI response: {e}; body={body}"))?;

    let reply = parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or("AI returned empty reply")?;

    append(user_hash, "assistant", &reply).await;
    Ok(reply)
}
