//! Feishu (飞书) / Lark puppet — v3.6 I3.
//!
//! ## 与其他协议的差异
//!
//! - 国际化双品牌：国内 **Feishu**（domain `open.feishu.cn`）/ 海外
//!   **Lark**（`open.larksuite.com`）。同一套 API，本 impl 通过 `base_url`
//!   参数选 endpoint。
//! - 鉴权用 **tenant_access_token**（短期，需要 app_id + app_secret 换取）
//!   而不是固定 bot token。Token 缓存 + 自动 refresh 在 v3.7 加；v3.6
//!   `send_text` 接受 caller-supplied token 占位实施。
//! - **Event Subscription** 模式 inbound — Feishu 服务端 POST event 给
//!   operator-configured callback URL，weclawbot 暴露
//!   `/api/v1/puppet/feishu/webhook/<account>` 接收。v3.7 加路由。
//!
//! ## v3.6 实施
//!
//! REST `send_text` —— POST `/open-apis/im/v1/messages` with `receive_id_type=chat_id`。
//! inbound 走 `feishu_inbound::event_to_common` 把 Feishu event JSON 转 CommonInbound。

use async_trait::async_trait;
use reqwest::Client;

use crate::api::types::{GetUpdatesResp, QrCodeResponse, QrStatusResponse};
use crate::error::WeclawError;
use crate::puppet::{MessagingPlatform, OutboundMessage};

/// Default Feishu (国内) base — operator 在 `base_url` 传海外 Lark 时切到
/// `https://open.larksuite.com`。
pub const FEISHU_DEFAULT_BASE: &str = "https://open.feishu.cn";

pub struct FeishuBot {
    http: Client,
}

impl FeishuBot {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client build"),
        }
    }

    /// API URL builder。`base_url` 空时默认 Feishu 国内域名。
    fn url(base_url: &str, path: &str) -> String {
        let base = if base_url.is_empty() {
            FEISHU_DEFAULT_BASE
        } else {
            base_url.trim_end_matches('/')
        };
        format!("{base}{path}")
    }
}

impl Default for FeishuBot {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MessagingPlatform for FeishuBot {
    fn platform_id(&self) -> &'static str {
        "feishu"
    }

    async fn poll_updates(
        &self,
        _token: &str,
        _base_url: &str,
        _buf: &str,
    ) -> Result<GetUpdatesResp, WeclawError> {
        // Feishu 走 Event Subscription webhook 推 events，不 long-poll。
        // 占位返空让 poller 主循环 sleep；真 inbound 通过
        // `/api/v1/puppet/feishu/webhook/<account>` HTTP 路由进。
        Ok(GetUpdatesResp::default())
    }

    async fn send_text(
        &self,
        token: &str,
        base_url: &str,
        out: OutboundMessage<'_>,
    ) -> Result<(), WeclawError> {
        let text = out.text.unwrap_or_default();
        if text.is_empty() {
            return Ok(());
        }
        // Feishu API: POST /open-apis/im/v1/messages?receive_id_type=chat_id
        // body: { receive_id, msg_type: "text", content: "{\"text\":\"...\"}" }
        let url = format!(
            "{}/open-apis/im/v1/messages?receive_id_type=chat_id",
            Self::url(base_url, "")
        );
        let content_inner = serde_json::json!({ "text": text }).to_string();
        let body = serde_json::json!({
            "receive_id": out.target,
            "msg_type": "text",
            "content": content_inner,
        })
        .to_string();
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(WeclawError::Network)?;
        if !resp.status().is_success() {
            let code = resp.status().as_u16() as i32;
            let body = resp.text().await.unwrap_or_else(|_| "<no body>".into());
            return Err(WeclawError::IlinkApi {
                code,
                message: format!(
                    "feishu send: {}",
                    body.chars().take(200).collect::<String>()
                ),
                retriable: code >= 500,
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
        // v3.7: Feishu file upload 走两步 —— POST /im/v1/files 拿 file_key
        // 后调 sendMessage with msg_type='file'。当前 stub。
        Err(WeclawError::Internal(
            "feishu send_file not yet implemented (v3.7)".into(),
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
            "Feishu apps use app_id + app_secret, not QR scan".into(),
        ))
    }

    async fn poll_qr_status(
        &self,
        _base_url: &str,
        _qrcode_key: &str,
    ) -> Result<QrStatusResponse, WeclawError> {
        Err(WeclawError::Internal(
            "Feishu apps use app_id + app_secret, not QR scan".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_id_is_feishu() {
        let b = FeishuBot::new();
        assert_eq!(b.platform_id(), "feishu");
        assert!(!b.supports_qr_login());
    }

    #[test]
    fn url_defaults_to_feishu_when_base_empty() {
        let u = FeishuBot::url("", "/foo");
        assert!(u.starts_with("https://open.feishu.cn"));
        assert!(u.ends_with("/foo"));
    }

    #[test]
    fn url_uses_provided_base_for_lark() {
        let u = FeishuBot::url("https://open.larksuite.com/", "/api/x");
        assert_eq!(u, "https://open.larksuite.com/api/x");
    }

    #[tokio::test]
    async fn qr_methods_return_unsupported() {
        let b = FeishuBot::new();
        assert!(b.fetch_qr_code("", "", &[]).await.is_err());
        assert!(b.poll_qr_status("", "").await.is_err());
    }
}
