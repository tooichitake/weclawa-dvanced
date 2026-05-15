//! Orchestrator for a single inbound **WeChat (iLink)** message.
//!
//! v3 note: this handler is iLink-specific by design — it operates on
//! `WeixinMessage` (uses `item_list[0].text_item.text`, `context_token`,
//! `session_id` swap). Multi-protocol support adds **sibling** handlers
//! (e.g. `puppet::telegram::handler::handle_inbound_message`) that share
//! the platform-agnostic core via `monitor::common::process_inbound`.
//! See `monitor::common` for the abstraction boundary.
//!
//! ```text
//! poller → handler::handle_inbound_message → {
//!     dedup            (drop duplicates)
//!     Sandbox::ensure  (init per-user state)
//!     wechat_menu      (/menu console — short-circuits AI)
//!     typing pulse     (start before resolve_message)
//!     resolve_message  (download attachments)
//!     dispatch reply   (webhook → claude → codex → api → echo → none)
//!     reply::send_text (text bubble back to WeChat)
//!     forward::*       (MCP attach files, diff fallback, attach_url)
//! }
//! ```
//!
//! Each substep lives in its own sibling module; this file glues them.

use std::collections::HashSet;

use tracing::{info, warn};

use super::{dedup, forward, reply};
use crate::api::client::ILinkClient;
use crate::api::types::{WeixinMessage, MESSAGE_TYPE_USER};
use crate::binding::agent_map::register_or_update_binding;
use crate::config::Config;
use crate::media::inbound::resolve_message;
use crate::sandbox::Sandbox;

pub async fn handle_inbound_message(
    client: &ILinkClient,
    account_id: &str,
    token: &str,
    base_url: &str,
    msg: &WeixinMessage,
) {
    // --- Filter & dedup ---
    if msg.message_type.unwrap_or(0) != MESSAGE_TYPE_USER {
        return;
    }
    let from = msg.from_user_id.as_deref().unwrap_or("");
    if from.is_empty() {
        return;
    }
    let msg_id = msg.message_id.unwrap_or(0);
    // v4.1 K9: sqlx async dedup（省 spawn_blocking 跳板）
    if dedup::is_duplicate_async(msg_id).await {
        return;
    }

    // --- Phase 5.1 + v2.1.B1: per-user rate limit ---
    // 阈值来自 config.json::rateLimit.userPerMinute。读 ArcSwap 缓存
    // 而不是每条消息同步 IO；GUI 改 + watcher / PUT /api/v1/config 主动
    // 触发 reload，依然热加载。
    let user_id_typed = crate::ids::WeixinUserId::new(from.to_string());
    let early_cfg = Config::cached();
    if let Some(reason) =
        super::rate_limit::check_inbound(&user_id_typed, early_cfg.rate_limit.user_per_minute)
    {
        warn!("[{account_id}] rate limited {from}: {reason}");
        reply::send_text(client, base_url, token, msg, &reason).await;
        return;
    }

    // --- Per-user sandbox ---
    let sandbox = match Sandbox::ensure(from) {
        Ok(s) => s,
        Err(e) => {
            warn!("[{account_id}] sandbox ensure failed for {from}: {e}");
            // v2.1.A3: 给用户一个友好提示，否则消息会"石沉大海"。
            // 同时撤掉 dedup 标记：sandbox 初始化是 transient failure
            // 类型（podman 重启、磁盘满临时清理、image pull 慢），iLink
            // 下一次 redeliver 同 msg_id 应该被允许重试，而不是当成
            // 重复直接静默丢掉。
            reply::send_text(
                client,
                base_url,
                token,
                msg,
                "(系统初始化中，请稍后再发一次)",
            )
            .await;
            dedup::unmark_async(msg_id).await;
            return;
        }
    };

    // --- /menu console route (short-circuits AI) ---
    // Console replies are instant; we deliberately do NOT start typing here.
    if let Some(text) = msg
        .item_list
        .as_ref()
        .and_then(|items| items.first())
        .and_then(|item| item.text_item.as_ref())
        .and_then(|t| t.text.as_deref())
    {
        if let crate::wechat_menu::ConsoleOutcome::Handled { reply: r } =
            crate::wechat_menu::route(&sandbox.user_hash, text)
        {
            reply::send_text(client, base_url, token, msg, &r).await;
            return;
        }
    }

    // --- Typing pulse (start ASAP — covers resolve_message + cold-start) ---
    let typing = crate::ai::cli_provider::start_typing_pulse(
        crate::ai::cli_provider::TypingContext {
            client: std::sync::Arc::new(client.clone()),
            base_url: base_url.to_string(),
            token: token.to_string(),
            user_id: from.to_string(),
            context_token: msg.context_token.clone(),
        },
    );

    // --- Resolve inbound media into the sandbox ---
    let content = resolve_message(&sandbox.media_inbound(), account_id, msg).await;

    info!(
        "[{account_id}] sandbox={} from={from} msg_id={msg_id} text={:?} attachments={}",
        sandbox.user_hash,
        content.text,
        content.attachments.len(),
    );
    for a in &content.attachments {
        info!("[{account_id}]   {} -> {}", a.kind, a.path.display());
    }

    if content.is_empty() {
        typing.stop();
        return;
    }

    sandbox.touch_profile();

    // Snapshot output dirs before the AI runs so we can diff afterwards.
    let snapshot_before = forward::snapshot(&sandbox);

    let config = Config::cached();

    if config.agent_binding.enabled {
        let record = register_or_update_binding(from, account_id);
        info!(
            "[{account_id}] user {from} -> agent {} (account={})",
            record.agent_id, record.active_account_id
        );
    }

    // --- Reply provider selection (v2.2 L2.2: trait chain) ---
    // v3.1: 构造 CommonInbound 让 provider 走 protocol-agnostic 路径
    //（webhook payload 用 CommonInbound 字段, 不再依赖 WeixinMessage）。
    //  msg 仍保留作为 iLink-specific fallback (context_token / session_id).
    //
    // v3.2: tenant_id 真从 accounts 表查（之前硬编码 default）。account
    // 不在 DB 时（极少数 bootstrap race）fallback default —— 不阻塞处理。
    let tenant_id = crate::tenancy::resolver::resolve_tenant_for_account(account_id);

    // v3.5 H4 / v4 J5: tenant 暂停拦截。Stripe webhook 已实时更新
    // tenants.billing_status，这里查 `is_active`（async sqlx 路径，省去
    // spawn_blocking 跳板）—— 若 tenant 被标 suspended，**不**调 AI、
    // **不**发文件 forward、直接回用户一句话提示。
    if !crate::tenancy::resolver::tenant_is_active_async(&tenant_id).await {
        warn!(
            "[{account_id}] tenant {} suspended/inactive — message dropped from {from}",
            tenant_id.as_str()
        );
        reply::send_text(
            client,
            base_url,
            token,
            msg,
            "(账户已暂停，请联系管理员)",
        )
        .await;
        metrics::counter!(
            "weclawbot_inbound_dropped_total",
            "reason" => "tenant_suspended"
        )
        .increment(1);
        return;
    }

    let common = crate::monitor::common::CommonInbound {
        tenant_id,
        account_id: account_id.to_string(),
        platform_id: "ilink-wechat",
        user_id: from.to_string(),
        msg_id: msg.message_id.unwrap_or(0).to_string(),
        text: content.text.clone(),
        attachments: content
            .attachments
            .iter()
            .map(|a| a.path.clone())
            .collect(),
    };
    let prov_out = crate::ai::provider::run_default_chain(&crate::ai::provider::DispatchContext {
        account_id,
        sandbox: &sandbox,
        content: &content,
        config: &config,
        msg: Some(msg),
        inbound: Some(&common),
    })
    .await;
    let reply_text = prov_out.text;
    let ai_files = prov_out.files;
    let ai_urls = prov_out.urls;
    let cli_succeeded = prov_out.cli_succeeded;

    typing.stop();

    if let Some(r) = reply_text {
        if !r.is_empty() {
            reply::send_text(client, base_url, token, msg, &r).await;
        }
    }

    // --- File / URL forwarding ---
    let mut sent: HashSet<std::path::PathBuf> = forward::forward_mcp_files(
        client, base_url, token, msg, account_id, &ai_files,
    )
    .await;
    if cli_succeeded {
        forward::forward_diff_fallback(
            client, base_url, token, msg, account_id,
            &sandbox, snapshot_before, &mut sent,
        )
        .await;
    }
    forward::forward_urls(client, base_url, token, msg, account_id, &sandbox, &ai_urls).await;
}

// v2.2 L2.2: 老 dispatch_reply 已搬到 `crate::ai::provider`，通过
// `run_default_chain` 调用。加新 provider 不再改这个文件。

// v3.6 I1: tenant resolver helpers 已抽到 `crate::tenancy::resolver`，
// iLink 和 Telegram handler 共用。该文件不再持有 fn 副本。
