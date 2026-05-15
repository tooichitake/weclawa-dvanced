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
use crate::monitor::common::{self, CommonInbound};
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

    // Step 4: process_inbound (rate_limit + dispatch chain)
    let config = crate::config::Config::cached();
    let content = crate::media::inbound::InboundContent {
        text: common.text.clone(),
        attachments: vec![], // v3.5: 真填 telegram attachments
        errors: vec![],
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::puppet::telegram_inbound::{Chat, Message, Update, User};

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
