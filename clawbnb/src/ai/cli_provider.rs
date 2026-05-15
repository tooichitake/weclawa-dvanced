//! Provider-agnostic AI completion entry point.
//!
//! Used to be a 652-line file mixing prompt building, claude spawn, stream-
//! json parsing, typing pulse, and types. After Phase C the split is:
//!
//! - Types (`FileSource`, `GeneratedFile`, `AttachedUrl`, `ClaudeOutput`,
//!   `CliConfig`) → `crate::ai`
//! - Typing pulse → `crate::monitor::typing`
//! - Claude provider → `crate::ai::claude`
//! - Codex provider (stub) → `crate::ai::codex`
//!
//! This module retains the legacy import path `crate::ai::cli_provider`
//! as a re-export shim and houses the high-level dispatcher
//! (`complete_with_content`). External callers (`monitor/handler.rs`) keep
//! their existing imports working; future refactors can flatten further.

// Re-export public types so external `crate::ai::cli_provider::*` imports
// continue to compile.
pub use crate::ai::{AttachedUrl, ClaudeOutput, CliConfig, GeneratedFile};
// Re-export typing primitives — handler.rs still references them via the
// `cli_provider` path; new code should import from `crate::monitor::typing`.
pub use crate::monitor::typing::{start as start_typing_pulse, TypingContext};

use crate::ai::claude::prompt::{build_prompt, build_system_only, build_user_only, format_user_segment};
use crate::ai::history::{append, recent};
use crate::media::inbound::InboundContent;
use crate::sandbox::Sandbox;

/// Single-shot text completion (no attachments, no streaming). Used by
/// internal callers that want a string reply only.
pub async fn complete(
    cfg: &CliConfig,
    sandbox: &Sandbox,
    user_text: &str,
) -> Result<String, String> {
    let content = InboundContent {
        text: user_text.to_string(),
        ..Default::default()
    };
    complete_with_content(cfg, sandbox, &content)
        .await
        .map(|o| o.text)
}

/// Full multi-modal completion. Per-user state lives in `sandbox`.
/// Returns text plus any files / URLs the provider declared via MCP.
///
/// Dispatches to the configured provider:
/// - `cfg.provider == "claude"` → `crate::ai::claude::invoke`
/// - `cfg.provider == "codex"`  → `crate::ai::codex::invoke` (stub)
pub async fn complete_with_content(
    cfg: &CliConfig,
    sandbox: &Sandbox,
    content: &InboundContent,
) -> Result<ClaudeOutput, String> {
    let user_hash = sandbox.user_hash.as_str();
    let user_segment = format_user_segment(content);
    // v2.1.B3: 不在 invoke 之前 append user turn。如果 invoke 失败而 user
    // turn 已入 DB，下次拼 prompt 会看到一个孤儿 user 消息（没有对应
    // assistant 回复），上下文对不齐，模型会接到错误时机的位置。改为：
    // 把 user_segment 作为 "current message" 临时拼进 prompt，只在
    // invoke 成功后再把 user + assistant 一起 append。
    let history = recent(user_hash, cfg.history_limit).await;
    let mut history_with_current = history.clone();
    history_with_current.push(crate::ai::history::ChatTurn {
        role: "user".to_string(),
        content: user_segment.clone(),
    });
    let history = history_with_current;

    // Phase 4: 计时 + 记录 AI 调用结果到 Prometheus
    let provider_label = cfg.provider.clone();
    let started = std::time::Instant::now();

    let result = match cfg.provider.as_str() {
        "claude" => {
            // Claude-CLI 支持 --system-prompt：身份/行为规则用这个 flag
            // 真注入到 system role；用户/历史走 stdin。这样模型不会把
            // 身份规则当成"用户的话"忽略。
            let system_prompt = build_system_only(&cfg.system_prompt);
            let user_prompt = build_user_only(&history);
            crate::ai::claude::invoke_with_system(cfg, sandbox, &system_prompt, &user_prompt).await
        }
        "codex" => {
            // codex 还没拆 system/user; 暂时还用 composite prompt
            let prompt = build_prompt(&cfg.system_prompt, &history);
            crate::ai::codex::invoke(cfg, sandbox, &prompt).await
        }
        other => Err(format!("unknown CLI provider: {other}")),
    };

    // 不管成功失败都记 — 失败本身也是值得跟踪的信号
    let latency_s = started.elapsed().as_secs_f64();
    let status_label = if result.is_ok() { "ok" } else { "error" };
    metrics::counter!(
        "weclawbot_ai_invocations_total",
        "provider" => provider_label.clone(),
        "status" => status_label
    )
    .increment(1);
    metrics::histogram!(
        "weclawbot_ai_latency_seconds",
        "provider" => provider_label
    )
    .record(latency_s);

    let mut output = result?;

    output.text = output.text.trim().to_string();
    if output.text.is_empty()
        && output.generated_files.is_empty()
        && output.generated_urls.is_empty()
    {
        return Err("CLI returned no text, no files, and no urls".into());
    }
    // v2.1.B3: 成功后才把 user + assistant 一起入 history。这两条 append
    // 不是单事务（UserRepo::append_history 是各一条 INSERT），daemon
    // 在两次 append 之间 crash 的极小概率会产生孤儿 user — 但比之前
    // "失败即孤儿"的概率小 1000 倍以上，可接受。
    append(user_hash, "user", &user_segment).await;
    if !output.text.is_empty() {
        append(user_hash, "assistant", &output.text).await;
    }
    Ok(output)
}
