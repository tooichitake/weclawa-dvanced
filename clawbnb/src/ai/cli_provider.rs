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

// v7.0 housekeeping: the `complete(cfg, sandbox, user_text)` convenience
// wrapper (text-only, no attachments) was removed — zero callers.
// `ReplyProvider` always builds an `InboundContent` (even text-only
// inbound has empty `attachments`) and goes through `complete_with_content`.

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

    // v7.3 — PII scrub on outbound AI prompt. Plan M2 calls for
    // "AI prompt with PII optionally scrubbed before being sent to
    // Claude (HIPAA/SOC2/GDPR mode)". The policy comes from the
    // Mode-driven `ai_prompt_baseline` plus any per-class overrides
    // from operator config. In Permissive (default OSS) mode, every
    // class is Pass and this is a no-op O(N) walk; in Strict mode
    // every class is Block/Redact.
    //
    // If any Block-policy class hits → refuse to call the AI and return
    // a user-facing error string. We deliberately surface this to the
    // WeChat user ("您的消息含有不可发送的敏感内容") instead of
    // silently scrubbing — Block means the operator's compliance policy
    // forbids that data leaving the daemon.
    //
    // We scrub `content.text` AND use the scrubbed version for both
    // (a) the prompt sent to Claude and (b) the history we persist.
    // Storing the unscrubbed original would defeat the point in Strict
    // mode (operator could later read history and see PII).
    let pii_policy = crate::config::Config::cached()
        .compliance
        .ai_prompt_policy();
    let scrub = crate::pii::scrub_with_policy(&content.text, &pii_policy);
    if scrub.blocked {
        let classes: Vec<&'static str> = scrub
            .hits
            .iter()
            .filter(|h| matches!(
                pii_policy.policy_for(h.class),
                crate::pii::PiiPolicy::Block
            ))
            .map(|h| h.class.as_str())
            .collect();
        tracing::warn!(
            "ai_prompt blocked for {user_hash} by policy ({} classes): {:?}",
            classes.len(),
            classes
        );
        metrics::counter!(
            "weclawbot_ai_prompt_blocked_total",
            "reason" => "pii_block_policy"
        )
        .increment(1);
        // v7.5 — write audit row so `trust_driver` can compute the
        // `threat` factor (fraction of recent inbounds that hit a PII
        // Block policy). target = user_hash so the driver's GROUP BY
        // works. The class list goes into `after` for forensics; PII
        // scrubber on the audit path is a no-op on class names (they're
        // not user content).
        if let Some(pool) = crate::storage::db_async::try_global_async_pool() {
            let audit_repo = crate::repo::audit_async::SqlxAuditRepo::new(pool);
            let after_summary = serde_json::json!({
                "classes": classes,
                "hit_count": scrub.hits.len(),
            });
            let _ = audit_repo
                .record(crate::repo::audit::AuditInput {
                    actor_key_id: None,
                    action: "ai_prompt.blocked",
                    target: Some(user_hash),
                    before: None,
                    after: Some(&after_summary),
                    ip: None,
                })
                .await;
        }
        return Err("(消息中含敏感信息，已被合规策略拒绝处理)".to_string());
    }
    let scrubbed_content = InboundContent {
        text: scrub.scrubbed.clone(),
        ..(*content).clone()
    };

    let user_segment = format_user_segment(&scrubbed_content);
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
