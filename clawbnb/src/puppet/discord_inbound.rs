//! Discord Gateway / webhook payload → [`CommonInbound`] mapping. v3.6 I2.
//!
//! ## 适用形态
//!
//! 目前覆盖 Discord 两种 inbound 来源：
//!
//! 1. **Gateway MESSAGE_CREATE event** —— v3.7 接 WebSocket 后会 yield
//!    `DiscordMessage` 给本模块。
//! 2. **HTTP outgoing webhook** —— operator 在 Discord 服务器配的 webhook
//!    把消息 POST 到 `weclawbot /api/v1/puppet/discord/webhook/<account>`。
//!
//! 两种形态消息体 JSON 字段一致（Discord API 统一），所以同一个 mapping。

use serde::Deserialize;
use std::path::PathBuf;

use crate::monitor::common::CommonInbound;
use crate::tenancy::TenantId;

#[derive(Debug, Deserialize)]
pub struct DiscordMessage {
    /// snowflake string —— Discord 的 i64 但 JSON 表示是 string 防溢出
    pub id: String,
    /// 跟普通 user 消息区分（系统/机器人/链接预览 bot 等）；只处理 type=0 / 19。
    #[serde(rename = "type", default)]
    pub message_type: u32,
    /// 频道 id (snowflake string) — 回复时用作 target
    pub channel_id: String,
    /// 来源用户
    pub author: DiscordUser,
    pub content: String,
    /// 附件列表（已上传到 Discord CDN，url 直接可下）
    #[serde(default)]
    pub attachments: Vec<DiscordAttachment>,
}

#[derive(Debug, Deserialize)]
pub struct DiscordUser {
    pub id: String,
    #[serde(default)]
    pub username: Option<String>,
    /// True = 该 author 是 bot —— inbound mapping 应跳过避免回环
    #[serde(default)]
    pub bot: bool,
}

#[derive(Debug, Deserialize)]
pub struct DiscordAttachment {
    pub id: String,
    pub filename: String,
    pub url: String,
    pub size: u64,
    #[serde(default)]
    pub content_type: Option<String>,
}

/// 把 Discord message → CommonInbound。返回 None 表示：
/// - bot 自发消息（避免回环）
/// - 不支持的 message_type（系统消息 / call / pin / 等）
/// - 内容为空且无附件
pub fn message_to_common(
    msg: &DiscordMessage,
    tenant_id: TenantId,
    account_id: String,
) -> Option<CommonInbound> {
    // 跳过 bot 自发消息（防止 weclawbot 自己回复自己）
    if msg.author.bot {
        return None;
    }
    // 只处理普通 / reply 消息（Discord type 0 = DEFAULT, 19 = REPLY）
    if !matches!(msg.message_type, 0 | 19) {
        return None;
    }
    if msg.content.is_empty() && msg.attachments.is_empty() {
        return None;
    }

    Some(CommonInbound {
        tenant_id,
        account_id,
        platform_id: "discord",
        // Discord user_id 是 snowflake string —— 直接当 weclawbot user_id 用
        user_id: msg.author.id.clone(),
        msg_id: msg.id.clone(),
        text: msg.content.clone(),
        // v3.7: 真填 — 当前 attachments 不下载，留 Vec<PathBuf> 空。
        // Discord attachment.url 已是 https 直接可下，下载层走
        // `puppet::discord::DiscordBot::download_attachment` (v3.7)。
        attachments: Vec::<PathBuf>::new(),
    })
}

/// 收集 message 上的 attachment URL 列表（filename, url, size）。handler
/// 用它驱动下载循环（v3.7）。
pub fn collect_attachment_urls(msg: &DiscordMessage) -> Vec<(String, String, u64)> {
    msg.attachments
        .iter()
        .map(|a| (a.filename.clone(), a.url.clone(), a.size))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_msg(content: &str, msg_type: u32, is_bot: bool) -> DiscordMessage {
        DiscordMessage {
            id: "snowflake-100".into(),
            message_type: msg_type,
            channel_id: "chan-1".into(),
            author: DiscordUser {
                id: "user-42".into(),
                username: Some("alice".into()),
                bot: is_bot,
            },
            content: content.into(),
            attachments: vec![],
        }
    }

    #[test]
    fn user_text_maps() {
        let c = message_to_common(
            &sample_msg("hi bot", 0, false),
            TenantId::default_tenant(),
            "acct-discord".into(),
        )
        .unwrap();
        assert_eq!(c.platform_id, "discord");
        assert_eq!(c.user_id, "user-42");
        assert_eq!(c.msg_id, "snowflake-100");
        assert_eq!(c.text, "hi bot");
    }

    #[test]
    fn bot_messages_skipped() {
        let r = message_to_common(
            &sample_msg("from a bot", 0, true),
            TenantId::default_tenant(),
            "acct".into(),
        );
        assert!(r.is_none());
    }

    #[test]
    fn empty_messages_skipped() {
        let r = message_to_common(
            &sample_msg("", 0, false),
            TenantId::default_tenant(),
            "acct".into(),
        );
        assert!(r.is_none());
    }

    #[test]
    fn unsupported_type_skipped() {
        // type=1 = RECIPIENT_ADD (system); type=7 = USER_JOIN; etc.
        let r = message_to_common(
            &sample_msg("any text", 7, false),
            TenantId::default_tenant(),
            "acct".into(),
        );
        assert!(r.is_none());
    }

    #[test]
    fn reply_type_19_accepted() {
        let c = message_to_common(
            &sample_msg("reply text", 19, false),
            TenantId::default_tenant(),
            "acct".into(),
        );
        assert!(c.is_some());
    }

    #[test]
    fn deserializes_real_discord_payload() {
        let payload = r#"
        {
            "id": "1234567890",
            "type": 0,
            "channel_id": "98765",
            "author": {"id": "111", "username": "bob", "bot": false},
            "content": "hello",
            "attachments": [
                {
                    "id": "999",
                    "filename": "report.pdf",
                    "url": "https://cdn.discordapp.com/attachments/.../report.pdf",
                    "size": 12345,
                    "content_type": "application/pdf"
                }
            ]
        }
        "#;
        let m: DiscordMessage = serde_json::from_str(payload).unwrap();
        assert_eq!(m.id, "1234567890");
        let atts = collect_attachment_urls(&m);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].0, "report.pdf");
        assert_eq!(atts[0].2, 12345);
    }
}
