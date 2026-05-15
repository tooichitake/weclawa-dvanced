//! Discord inbound handler — v5.3。
//!
//! 镜像 `telegram_handler` 的 5 步流水线：
//!
//! ```text
//! gateway / webhook (DiscordMessage)
//!   → discord_handler::handle_inbound_message(bot, msg, account, token)
//!       1. message_to_common (mapping → CommonInbound)
//!       2. dedup_async::mark_seen_scoped (tenant + account + msg_id)
//!       3. Sandbox::ensure (user_hash = discord snowflake)
//!       4. monitor::common::process_inbound (rate_limit + dispatch_reply)
//!       5. bot.send_text (POST /channels/<id>/messages)
//! ```
//!
//! ## 跟 Telegram handler 的差异
//!
//! - **没有 long-poll** —— inbound 来源 = Gateway WebSocket
//!   (`puppet::discord_gateway` feature-gated) OR outgoing webhook
//!   route。两边 yield 同款 `DiscordMessage`，handler 不区分
//! - **target = channel_id**（不是 user_id），所以 reply 走频道
//! - **msg_id 是 snowflake string**（u64 string）—— dedup 用 i64
//!   时需 parse + 截断到 i64 max；u64 高位不会用到（Discord epoch
//!   2015，离 i64 max 还远）
//! - **附件** 是 Discord CDN URL 不是 file_id；下载层留 v5.4 wire
//!
//! ## 未做（留 v5.4）
//!
//! - 真下载 attachment URLs 到 `sandbox/media_inbound/`
//! - 多 message_type（v3.6 只接受 type=0 DEFAULT / type=19 REPLY）

use std::sync::Arc;

use tracing::{info, warn};

use crate::monitor::common::{self};
use crate::puppet::discord::DiscordBot;
use crate::puppet::discord_inbound::{message_to_common, DiscordMessage};
use crate::puppet::{MessagingPlatform, OutboundMessage};
use crate::sandbox::Sandbox;

pub async fn handle_inbound_message(
    bot: Arc<DiscordBot>,
    msg: &DiscordMessage,
    account_id: &str,
    token: &str,
) {
    let tenant = crate::tenancy::resolver::resolve_tenant_for_account(account_id);

    // Step 0: tenant suspended → drop with friendly reply
    if !crate::tenancy::resolver::tenant_is_active_async(&tenant).await {
        warn!(
            "[{account_id}] discord tenant {} suspended/inactive — message dropped",
            tenant.as_str()
        );
        let _ = send_reply(&bot, token, &msg.channel_id, "(账户已暂停，请联系管理员)").await;
        metrics::counter!(
            "weclawbot_inbound_dropped_total",
            "platform" => "discord",
            "reason" => "tenant_suspended"
        )
        .increment(1);
        return;
    }

    // Step 1: mapping
    let common = match message_to_common(msg, tenant, account_id.to_string()) {
        Some(c) => c,
        None => return, // bot self-msg / unsupported type / empty
    };

    // Step 2: dedup scoped — snowflake string → i64 (Discord epoch leaves room)
    let msg_id_i64: i64 = match common.msg_id.parse::<i64>() {
        Ok(v) => v,
        Err(_) => {
            warn!(
                "[{account_id}] discord msg_id not parseable as i64: {}",
                common.msg_id
            );
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
                    "[{account_id}] discord duplicate msg_id={}",
                    common.msg_id
                );
                return;
            }
            Ok(false) => {}
            Err(e) => {
                warn!("[{account_id}] discord dedup error: {e} — proceeding without dedup");
            }
        }
    }

    // Step 3: Sandbox::ensure (user_hash from Discord user snowflake)
    let sandbox = match Sandbox::ensure(&common.user_id) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "[{account_id}] discord sandbox ensure for {}: {e}",
                common.user_id
            );
            let _ = send_reply(
                &bot,
                token,
                &msg.channel_id,
                "(系统初始化中，请稍后再发一次)",
            )
            .await;
            return;
        }
    };
    sandbox.touch_profile();

    // Step 4: process_inbound（rate_limit + dispatch_reply chain）
    let config = crate::config::Config::cached();
    let content = crate::media::inbound::InboundContent {
        text: common.text.clone(),
        // v5.4: download msg.attachments[].url → sandbox/media_inbound/
        attachments: Vec::new(),
        errors: vec![],
    };
    let prov_out = common::process_inbound(&common, &sandbox, &content, &config).await;

    // Step 5: send reply to the channel
    let has_text = prov_out.text.is_some();
    if let Some(text) = prov_out.text {
        if !text.is_empty() {
            if let Err(e) = send_reply(&bot, token, &msg.channel_id, &text).await {
                warn!("[{account_id}] discord send_text: {e}");
            }
        }
    }

    // history append (mirror iLink / Telegram pattern)
    if prov_out.cli_succeeded {
        crate::ai::history::append(sandbox.user_hash.as_str(), "user", &common.text).await;
    }

    metrics::counter!(
        "weclawbot_inbound_messages_total",
        "platform" => "discord",
        "status" => if has_text { "replied" } else { "no_reply" }
    )
    .increment(1);
}

async fn send_reply(
    bot: &DiscordBot,
    token: &str,
    channel_id: &str,
    text: &str,
) -> Result<(), crate::error::WeclawError> {
    bot.send_text(
        token,
        "", // discord base_url is fixed in DiscordBot
        OutboundMessage {
            target: channel_id,
            text: Some(text),
            file: None,
            file_name: None,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::puppet::discord_inbound::{DiscordAttachment, DiscordUser};

    fn sample_msg(content: &str, is_bot: bool) -> DiscordMessage {
        DiscordMessage {
            id: "1234567890".into(),
            message_type: 0,
            channel_id: "chan-99".into(),
            author: DiscordUser {
                id: "user-42".into(),
                username: Some("alice".into()),
                bot: is_bot,
            },
            content: content.into(),
            attachments: Vec::<DiscordAttachment>::new(),
        }
    }

    /// bot author 应被早期跳过 (mapping 返 None)，不调任何 IO
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn bot_message_is_noop() {
        let bot = Arc::new(DiscordBot::new());
        handle_inbound_message(bot, &sample_msg("hi", true), "acct-test", "tok").await;
        // 没 panic + 没真发请求 = OK
    }

    /// 不可解析的 msg_id 早期 warn + return；不调 Sandbox::ensure
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn nonparseable_msg_id_warns_and_returns() {
        let bot = Arc::new(DiscordBot::new());
        let mut m = sample_msg("hi", false);
        m.id = "not-a-number".into();
        handle_inbound_message(bot, &m, "acct-test", "tok").await;
    }
}
