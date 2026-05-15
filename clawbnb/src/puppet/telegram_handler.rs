//! Telegram inbound handler — v3.4 G1.
//!
//! ## 调用链
//!
//! ```text
//! poller (telegram getUpdates loop)
//!   → telegram_handler::handle_inbound_update(bot, update, account)
//!       1. update_to_common (mapping)
//!       2. dedup::is_duplicate_scoped (tenant+account+msg_id)
//!       3. Sandbox::ensure(user_hash from telegram chat id)
//!       4. monitor::common::process_inbound (rate_limit + dispatch_reply)
//!       5. bot.send_text (回 Telegram chat)
//! ```
//!
//! ## 与 iLink handler 的差异
//!
//! - `dedup` 走 `_scoped` 变体 — 不同 platform 的 msg_id 可能撞车
//! - `user_id` 是 Telegram `chat_id` (i64 转 String) — 不哈希 SHA-256
//!   (iLink 的 sha2 流程 sandbox::ensure 内部自己做)
//! - **没有** menu route / typing pulse — Telegram 协议没这俩概念，
//!   后续 v3.5 可以加 `bot.send_chat_action(typing)` 等价于 iLink typing
//! - send_file forward 链待 v3.5 (Telegram sendDocument multipart 流程)
//!
//! ## 不在本期做的
//!
//! - Attachment 入站（message.document → 调 `bot.download_attachment`
//!   写 sandbox/media）— v3.5 wire 进 `update_to_common` 之后的 attachments
//!   字段。当前只跑 text-only。
//! - reply 链 forward (MCP attach files / diff fallback / urls)。

use std::sync::Arc;

use tracing::{info, warn};

use crate::error::WeclawError;
use crate::ids::WeixinUserId;
use crate::monitor::common::{self};
use crate::puppet::telegram::TelegramBot;
use crate::puppet::telegram_inbound::{update_to_common, Update};
use crate::puppet::{MessagingPlatform, OutboundMessage};
use crate::sandbox::Sandbox;

/// 处理一条 Telegram update。失败不传出 — Telegram bot 是"消失消息"
/// 没 redeliver 机制（除非 long-poll offset 没推进），跟 iLink redeliver
/// 不同 —— 这里只 log warn。
pub async fn handle_inbound_update(
    bot: Arc<TelegramBot>,
    update: &Update,
    account_id: &str,
    token: &str,
) {
    let tenant = crate::tenancy::resolver::resolve_tenant_for_account(account_id);

    // v3.5 H4 / v4 J5: tenant 暂停拦截（async 路径，省去 spawn_blocking）。
    if !crate::tenancy::resolver::tenant_is_active_async(&tenant).await {
        warn!(
            "[{account_id}] telegram tenant {} suspended/inactive — update dropped",
            tenant.as_str()
        );
        let chat_id = update
            .message
            .as_ref()
            .map(|m| m.chat.id.to_string())
            .unwrap_or_default();
        if !chat_id.is_empty() {
            let _ = send_reply(&bot, token, &chat_id, "(账户已暂停，请联系管理员)").await;
        }
        metrics::counter!(
            "weclawbot_inbound_dropped_total",
            "platform" => "telegram",
            "reason" => "tenant_suspended"
        )
        .increment(1);
        return;
    }

    // Step 1: mapping
    let common = match update_to_common(update, tenant, account_id.to_string()) {
        Some(c) => c,
        None => {
            // 不是用户 text message / 空文本 — skip
            return;
        }
    };

    // Step 2: dedup scoped (tenant + account + msg_id)
    let msg_id_i64: i64 = match common.msg_id.parse() {
        Ok(v) => v,
        Err(_) => {
            warn!("[{account_id}] telegram msg_id not i64: {}", common.msg_id);
            return;
        }
    };
    if let Some(apool) = crate::storage::db_async::try_global_async_pool() {
        let dedup = crate::repo::dedup_async::SqlxDedupRepo::new(apool);
        match dedup
            .mark_seen_scoped(common.tenant_id.as_str(), &common.account_id, msg_id_i64)
            .await
        {
            Ok(true) => {
                info!(
                    "[{account_id}] telegram duplicate update_id={} msg_id={}",
                    update.update_id, common.msg_id
                );
                return;
            }
            Ok(false) => {}
            Err(e) => {
                warn!("[{account_id}] telegram dedup error: {e} — proceeding without dedup");
            }
        }
    }

    // Step 3: Sandbox::ensure (user_hash from telegram chat id)
    let sandbox = match Sandbox::ensure(&common.user_id) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "[{account_id}] telegram sandbox ensure for {}: {e}",
                common.user_id
            );
            // 回用户友好提示 — Telegram sendMessage 不 redeliver，但我们
            // 给用户一句话，下次他发的时候沙箱已经热了。
            let _ = send_reply(&bot, token, &common.user_id, "(系统初始化中，请稍后再发一次)").await;
            return;
        }
    };
    sandbox.touch_profile();

    // Step 4: download attachments (v5.1 N1)
    let config = crate::config::Config::cached();
    let attachments = match download_telegram_attachments(&bot, token, &sandbox, update).await {
        Ok(a) => a,
        Err(e) => {
            warn!("[{account_id}] telegram attach download: {e} — proceeding without files");
            Vec::new()
        }
    };
    let content = crate::media::inbound::InboundContent {
        text: common.text.clone(),
        attachments,
        errors: vec![],
    };
    // 更新 CommonInbound.attachments 以便 webhook payload 携带（虽然现在
    // 没 caller 用到，但保持 source-of-truth 一致）。
    let mut common = common;
    common.attachments = content
        .attachments
        .iter()
        .map(|a| a.path.clone())
        .collect();
    let prov_out = common::process_inbound(&common, &sandbox, &content, &config).await;

    // Step 5: send reply
    let has_text = prov_out.text.is_some();
    if let Some(text) = prov_out.text {
        if !text.is_empty() {
            if let Err(e) = send_reply(&bot, token, &common.user_id, &text).await {
                warn!("[{account_id}] telegram send_text: {e}");
            }
        }
    }

    // history append (provider chain 不写，handler 写) — 跟 iLink handler 一致
    if prov_out.cli_succeeded {
        crate::ai::history::append(sandbox.user_hash.as_str(), "user", &common.text).await;
        // assistant turn 在 cli_provider 内部 append（v2.1.B3 已修），这里
        // 只补 user turn 即可；非 cli provider (echo / webhook) 不进 history。
    }

    metrics::counter!(
        "weclawbot_inbound_messages_total",
        "platform" => "telegram",
        "status" => if has_text { "replied" } else { "no_reply" }
    )
    .increment(1);
    let _ = msg_id_i64; // used by metrics labels in v3.5
    let _: WeixinUserId; // type imported for v3.5 unified user_id wrapper
}

async fn send_reply(
    bot: &TelegramBot,
    token: &str,
    chat_id: &str,
    text: &str,
) -> Result<(), WeclawError> {
    bot.send_text(
        token,
        "", // base_url not used for Telegram
        OutboundMessage {
            target: chat_id,
            text: Some(text),
            file: None,
            file_name: None,
        },
    )
    .await
}

// v3.6 I1: helpers 已抽到 `crate::tenancy::resolver`，所有协议共用。

/// v5.1 N1: 下载 Telegram message 上的所有 attachment 到
/// `sandbox/media_inbound/`。返回填好 path / kind / original_name 的
/// `media::inbound::Attachment` 列表（跟 iLink 路径产出格式一致，让
/// 下游 dispatch_reply 不需要分协议差异）。
///
/// 失败 (token 错 / 文件 > 20MB / fs IO) → Err 给 caller log，但不阻
/// 塞当前消息（caller fail-open 走 text-only）。
async fn download_telegram_attachments(
    bot: &TelegramBot,
    token: &str,
    sandbox: &Sandbox,
    update: &Update,
) -> Result<Vec<crate::media::inbound::Attachment>, String> {
    let msg = match update.message.as_ref() {
        Some(m) => m,
        None => return Ok(Vec::new()),
    };
    let files = msg.collect_attachments();
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let media_dir = sandbox.media_inbound();
    if !media_dir.exists() {
        std::fs::create_dir_all(&media_dir).map_err(|e| format!("mkdir media: {e}"))?;
    }

    let mut out = Vec::with_capacity(files.len());
    for (file_id, suggested_name) in files {
        // 让 path 文件名稳定 + 唯一：用 file_id + 用户给的 suggested_name 后缀
        let safe_name = suggested_name
            .as_deref()
            .map(|n| n.chars().filter(|c| c.is_alphanumeric() || *c == '.' || *c == '_' || *c == '-').collect::<String>())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("tg-{file_id}"));
        let dest = media_dir.join(&safe_name);

        let kind: &'static str = if safe_name.ends_with(".ogg") || safe_name.starts_with("voice-") {
            "voice"
        } else if safe_name.starts_with("photo-") || safe_name.ends_with(".jpg") || safe_name.ends_with(".png") {
            "image"
        } else {
            "file"
        };

        match bot.download_attachment(token, &file_id, &dest).await {
            Ok(bytes) => {
                tracing::info!(
                    "telegram attach: {file_id} -> {} ({bytes} bytes, kind={kind})",
                    dest.display()
                );
                out.push(crate::media::inbound::Attachment {
                    path: dest,
                    original_name: suggested_name,
                    embedded_text: None,
                    kind,
                });
            }
            Err(e) => {
                tracing::warn!("telegram attach {file_id} skip: {e}");
                // 不阻塞 — 多附件场景一个失败不影响别的
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::puppet::telegram_inbound::Update;

    /// 验证 update 不带 message 时 handler 不 panic / 不调下游。
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn empty_update_is_noop() {
        // 无 DB → 各 repo 调用都返回 None / fallback path。
        // bot.send_text 不会调到（mapping 早就返回 None）。
        let bot = Arc::new(TelegramBot::new());
        let u = Update {
            update_id: 1,
            message: None,
        };
        // 不应 panic
        handle_inbound_update(bot, &u, "acct-test", "tok").await;
    }

    // 真消息路径的测试需要 Sandbox::ensure + 真 fs，会污染共享的
    // ~/.weclawbot/users/ 跟其他并行测试 race。集成测放进
    // tests/integration/ 用 TempDir 隔离一份 weclawbot home — v3.5 PR
    // 跟 sqlx async + 全 testcontainers 一起做。
}
