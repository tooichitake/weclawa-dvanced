//! Discord puppet — v3.6 I2.
//!
//! ## Discord Bot API 与其他协议的差异
//!
//! - Discord 用 **WebSocket Gateway** 推 events 而不是 long-poll。这一层
//!   集成在 v3.7 加 `puppet::discord_gateway` 模块；本文件先提供 REST
//!   API `send_text` / `send_file` 以及 inbound webhook 模式（Discord
//!   支持 outgoing webhooks，operator 可以配 weclawbot 端点接收）。
//! - 消息 id 是 **snowflake** (string-encoded i64)，跟 Telegram 的 i64
//!   不同 —— `msg_id` 字符串语义跟 [`crate::monitor::common::CommonInbound`]
//!   设计一致。
//! - Bot token 格式 `Bot <token>`（前缀必须），跟 Telegram 的 raw token
//!   不同。
//!
//! ## v3.6 范围
//!
//! - `DiscordBot` impl [`crate::puppet::MessagingPlatform`] — send_text
//!   走 REST `POST /api/v10/channels/<chan>/messages`
//! - `poll_updates` 返回空 `GetUpdatesResp`（Discord 不 poll，Gateway 推
//!   留 v3.7）
//! - 不支持 QR login
//!
//! ## v3.7+ 留的
//!
//! - WebSocket Gateway 连接 + heartbeat + sequence resume
//! - `send_file` 走 multipart upload + 8MB cap
//! - Slash command 注册

use async_trait::async_trait;
use reqwest::Client;

use crate::api::types::{GetUpdatesResp, QrCodeResponse, QrStatusResponse};
use crate::error::WeclawError;
use crate::puppet::{MessagingPlatform, OutboundMessage};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

pub struct DiscordBot {
    http: Client,
}

impl DiscordBot {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client build"),
        }
    }

    fn auth_header(token: &str) -> String {
        format!("Bot {token}")
    }
}

impl Default for DiscordBot {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MessagingPlatform for DiscordBot {
    fn platform_id(&self) -> &'static str {
        "discord"
    }

    async fn poll_updates(
        &self,
        _token: &str,
        _base_url: &str,
        _buf: &str,
    ) -> Result<GetUpdatesResp, WeclawError> {
        // Discord 走 WebSocket Gateway 推 events，不是 long-poll。该
        // method 留空返 default 让 poller 主循环跑空 sleep，实际 inbound
        // 进来走 `puppet::discord_gateway` (v3.7) 或 `service::routes`
        // 的 webhook 端点。
        Ok(GetUpdatesResp::default())
    }

    async fn send_text(
        &self,
        token: &str,
        _base_url: &str,
        out: OutboundMessage<'_>,
    ) -> Result<(), WeclawError> {
        let text = out.text.unwrap_or_default();
        if text.is_empty() {
            return Ok(());
        }
        // `target` 是 channel_id（snowflake string）
        let url = format!("{DISCORD_API_BASE}/channels/{}/messages", out.target);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", Self::auth_header(token))
            .header("Content-Type", "application/json")
            .body(serde_json::json!({ "content": text }).to_string())
            .send()
            .await
            .map_err(WeclawError::Network)?;
        if !resp.status().is_success() {
            let code = resp.status().as_u16() as i32;
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(WeclawError::IlinkApi {
                code,
                message: format!(
                    "discord send: {}",
                    body.chars().take(200).collect::<String>()
                ),
                retriable: code >= 500 || code == 429,
            });
        }
        Ok(())
    }

    async fn send_file(
        &self,
        _token: &str,
        _base_url: &str,
        _out: OutboundMessage<'_>,
    ) -> Result<(), WeclawError> {
        // v3.7: Discord multipart upload via POST channels/<id>/messages
        // with `files[0]` form-data。8MB cap for free tier。当前 stub。
        Err(WeclawError::Internal(
            "discord send_file not yet implemented (v3.7)".into(),
        ))
    }

    fn supports_qr_login(&self) -> bool {
        false
    }

    async fn fetch_qr_code(
        &self,
        _base_url: &str,
        _bot_type: &str,
        _local_token_list: &[String],
    ) -> Result<QrCodeResponse, WeclawError> {
        Err(WeclawError::Internal(
            "Discord bots use OAuth bot install URL, not QR scan".into(),
        ))
    }

    async fn poll_qr_status(
        &self,
        _base_url: &str,
        _qrcode_key: &str,
    ) -> Result<QrStatusResponse, WeclawError> {
        Err(WeclawError::Internal(
            "Discord bots use OAuth bot install URL, not QR scan".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_id_is_discord() {
        let b = DiscordBot::new();
        assert_eq!(b.platform_id(), "discord");
        assert!(!b.supports_qr_login());
    }

    #[test]
    fn auth_header_format() {
        assert_eq!(DiscordBot::auth_header("token123"), "Bot token123");
    }

    #[tokio::test]
    async fn qr_methods_return_unsupported() {
        let b = DiscordBot::new();
        assert!(b.fetch_qr_code("", "", &[]).await.is_err());
        assert!(b.poll_qr_status("", "").await.is_err());
    }

    #[tokio::test]
    async fn poll_updates_returns_empty_default() {
        let b = DiscordBot::new();
        let r = b.poll_updates("tok", "", "0").await.unwrap();
        assert!(r.msgs.is_none());
    }
}
