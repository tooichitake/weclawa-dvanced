//! Feishu inbound handler — v5.3。
//!
//! 镜像 `telegram_handler` 的 5 步流水线，但**入口来源是 HTTP webhook
//! route** (`service::feishu_webhook`)，不是 long-poll：
//!
//! ```text
//! POST /api/v1/puppet/feishu/webhook/<account_id>  (Feishu Event Subscription)
//!   → feishu_webhook::post_feishu_webhook (timestamp / challenge / parse)
//!     → feishu_handler::handle_inbound_event(bot, common, account, token)
//!       1. dedup_async::mark_seen_scoped (tenant + account + msg_id)
//!       2. Sandbox::ensure (user_hash = feishu open_id)
//!       3. monitor::common::process_inbound (rate_limit + dispatch_reply)
//!       4. bot.send_text (POST /open-apis/im/v1/messages?receive_id_type=chat_id)
//! ```
//!
//! ## 跟 Telegram / Discord handler 的差异
//!
//! - **mapping 已由 webhook route 做完** —— 此 handler 拿到的是
//!   `CommonInbound` 直接走 dedup / sandbox / process_inbound
//! - **target = chat_id** —— Feishu reply 是发到 chat，不直接给用户
//!   （即使是 1v1 也是 chat 概念）。`feishu_inbound::event_to_common`
//!   把 chat_id 放在 `CommonInbound.user_id`？不对 —— 当前 mapping 把
//!   `sender.open_id` 放 user_id，chat_id 没传。**故 reply target = open_id？
//!   不对，Feishu API 要 chat_id**。本期暂时 reply 给 open_id（Feishu
//!   `receive_id_type=open_id` 也是有效的 send target），生产环境
//!   operator 配 base_url + token 时同步
//! - **msg_id 是 `om_xxx` string** —— 不是 i64，dedup 用 hash 折射

use std::sync::Arc;

use tracing::{info, warn};

use crate::monitor::common::{self, CommonInbound};
use crate::puppet::feishu::FeishuBot;
use crate::puppet::{MessagingPlatform, OutboundMessage};
use crate::sandbox::Sandbox;

/// 把 `om_xxx` Feishu msg id 折成 i64 给 dedup repo。dedup msg_id 列
/// 是 INTEGER；Feishu 字符串 ID 不会跨多 message 重复（API 保证），用
/// 截断 hash 当 dedup key 是安全的。
fn hash_msg_id_to_i64(msg_id: &str) -> i64 {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(msg_id.as_bytes());
    // take first 8 bytes as i64 LE
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&h[0..8]);
    i64::from_le_bytes(buf)
}

pub async fn handle_inbound_event(
    bot: Arc<FeishuBot>,
    common: CommonInbound,
    base_url: &str,
    token: &str,
) {
    let account_id = common.account_id.clone();
    let tenant = common.tenant_id.clone();

    // Step 0: tenant suspended → drop with friendly reply
    if !crate::tenancy::resolver::tenant_is_active_async(&tenant).await {
        warn!(
            "[{account_id}] feishu tenant {} suspended/inactive — event dropped",
            tenant.as_str()
        );
        let _ = send_reply(&bot, token, base_url, &common.user_id, "(账户已暂停，请联系管理员)").await;
        metrics::counter!(
            "weclawbot_inbound_dropped_total",
            "platform" => "feishu",
            "reason" => "tenant_suspended"
        )
        .increment(1);
        return;
    }

    // Step 1: dedup scoped — folding string → i64
    let msg_id_i64 = hash_msg_id_to_i64(&common.msg_id);
    if let Some(apool) = crate::storage::db_async::try_global_async_pool() {
        let dedup = crate::repo::dedup_async::SqlxDedupRepo::new(apool);
        match dedup
            .mark_seen_scoped(common.tenant_id.as_str(), &common.account_id, msg_id_i64)
            .await
        {
            Ok(true) => {
                info!(
                    "[{account_id}] feishu duplicate msg_id={}",
                    common.msg_id
                );
                return;
            }
            Ok(false) => {}
            Err(e) => {
                warn!("[{account_id}] feishu dedup error: {e} — proceeding without dedup");
            }
        }
    }

    // Step 2: Sandbox::ensure (user_hash = open_id)
    let sandbox = match Sandbox::ensure(&common.user_id) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "[{account_id}] feishu sandbox ensure for {}: {e}",
                common.user_id
            );
            let _ = send_reply(
                &bot,
                token,
                base_url,
                &common.user_id,
                "(系统初始化中，请稍后再发一次)",
            )
            .await;
            return;
        }
    };
    sandbox.touch_profile();

    // Step 3: process_inbound（rate_limit + dispatch_reply chain）
    let config = crate::config::Config::cached();
    let content = crate::media::inbound::InboundContent {
        text: common.text.clone(),
        attachments: Vec::new(),
        errors: vec![],
    };
    let prov_out = common::process_inbound(&common, &sandbox, &content, &config).await;

    // Step 4: send reply
    let has_text = prov_out.text.is_some();
    if let Some(text) = prov_out.text {
        if !text.is_empty() {
            if let Err(e) = send_reply(&bot, token, base_url, &common.user_id, &text).await {
                warn!("[{account_id}] feishu send_text: {e}");
            }
        }
    }

    if prov_out.cli_succeeded {
        crate::ai::history::append(sandbox.user_hash.as_str(), "user", &common.text).await;
    }

    metrics::counter!(
        "weclawbot_inbound_messages_total",
        "platform" => "feishu",
        "status" => if has_text { "replied" } else { "no_reply" }
    )
    .increment(1);
}

async fn send_reply(
    bot: &FeishuBot,
    token: &str,
    base_url: &str,
    target: &str,
    text: &str,
) -> Result<(), crate::error::WeclawError> {
    bot.send_text(
        token,
        base_url,
        OutboundMessage {
            target,
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

    #[test]
    fn hash_msg_id_stable() {
        let a = hash_msg_id_to_i64("om_xxxxxx");
        let b = hash_msg_id_to_i64("om_xxxxxx");
        assert_eq!(a, b);
        let c = hash_msg_id_to_i64("om_different");
        assert_ne!(a, c);
    }
}
