use std::time::Duration;

use reqwest::Client;
use tracing::{debug, warn};

use super::headers::{build_common_headers, build_post_headers, channel_version};
use super::types::*;

const FIXED_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const DEFAULT_LONG_POLL_TIMEOUT: Duration = Duration::from_secs(35);
const DEFAULT_API_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_CONFIG_TIMEOUT: Duration = Duration::from_secs(10);

fn ensure_trailing_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

fn build_base_info() -> BaseInfo {
    BaseInfo {
        channel_version: Some(channel_version()),
    }
}

pub fn default_base_url() -> &'static str {
    FIXED_BASE_URL
}

#[derive(Clone)]
pub struct ILinkClient {
    http: Client,
}

/// 判断 api_get/api_post 失败的错误字符串是不是一个传输层瞬时错误
/// （连接超时 / DNS / TLS 握手 / 连接被对端 reset 等）。这些都不是
/// "用户操作错了" —— 上层（QR 轮询、long-poll）应当继续重试。
fn is_transient_network_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    [
        "timed out",
        "timeout",
        "error sending request",
        "connection reset",
        "connection refused",
        "connection closed",
        "dns",
        "tls handshake",
        "broken pipe",
        "unexpected eof",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

impl ILinkClient {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(DEFAULT_API_TIMEOUT)
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    pub async fn api_get(
        &self,
        base_url: &str,
        endpoint: &str,
        timeout: Option<Duration>,
    ) -> Result<String, String> {
        let base = ensure_trailing_slash(base_url);
        let url = format!("{base}{endpoint}");
        let headers = build_common_headers();
        debug!("GET {url}");

        let builder = self.http.get(&url).headers(headers);
        let builder = if let Some(t) = timeout {
            builder.timeout(t)
        } else {
            builder
        };

        let resp = builder.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("{endpoint} {status}: {body}"));
        }
        Ok(body)
    }

    pub async fn api_post(
        &self,
        base_url: &str,
        endpoint: &str,
        body: &str,
        token: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<String, String> {
        let base = ensure_trailing_slash(base_url);
        let url = format!("{base}{endpoint}");
        let headers = build_post_headers(token);
        debug!("POST {url}");

        let builder = self
            .http
            .post(&url)
            .headers(headers)
            .body(body.to_string());
        let builder = if let Some(t) = timeout {
            builder.timeout(t)
        } else {
            builder
        };

        let resp = builder.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("{endpoint} {status}: {text}"));
        }
        Ok(text)
    }

    // --- High-level API wrappers ---

    pub async fn fetch_qr_code(
        &self,
        base_url: &str,
        bot_type: &str,
        local_token_list: &[String],
    ) -> Result<QrCodeResponse, String> {
        let body = serde_json::json!({ "local_token_list": local_token_list });
        let endpoint = format!(
            "ilink/bot/get_bot_qrcode?bot_type={}",
            urlencoding::encode(bot_type)
        );
        let raw = self
            .api_post(base_url, &endpoint, &body.to_string(), None, None)
            .await?;
        serde_json::from_str(&raw).map_err(|e| format!("parse QR response: {e}"))
    }

    pub async fn poll_qr_status(
        &self,
        base_url: &str,
        qrcode: &str,
        timeout: Option<Duration>,
    ) -> Result<QrStatusResponse, String> {
        let endpoint = format!(
            "ilink/bot/get_qrcode_status?qrcode={}",
            urlencoding::encode(qrcode)
        );
        // 8 s — WSL 下 connect 到微信端 host 偶尔会慢 (3-5s 不少见)。
        // 这是用户主动轮询的端点，不是 long-poll，所以慢一点没关系。
        let t = timeout.unwrap_or(Duration::from_secs(8));
        match self.api_get(base_url, &endpoint, Some(t)).await {
            Ok(raw) => {
                serde_json::from_str(&raw).map_err(|e| format!("parse QR status: {e}"))
            }
            // 任何 connect / TCP / TLS / read 阶段的临时网络错误都当作
            // "用户还没扫码" 处理 —— 让前端继续轮询。reqwest 在不同
            // 失败阶段返回的错误字符串差异很大（"timed out"、"error
            // sending request"、"connection reset"、"dns error" 等），
            // 与其逐个匹配，不如把所有传输层错误都吞掉。
            Err(e) if is_transient_network_error(&e) => {
                debug!("poll_qr_status: transient net error, returning wait: {e}");
                Ok(QrStatusResponse {
                    status: "wait".to_string(),
                    bot_token: None,
                    ilink_bot_id: None,
                    baseurl: None,
                    ilink_user_id: None,
                    redirect_host: None,
                })
            }
            Err(e) => Err(e),
        }
    }

    pub async fn get_updates(
        &self,
        base_url: &str,
        token: &str,
        get_updates_buf: &str,
        timeout: Option<Duration>,
    ) -> Result<GetUpdatesResp, String> {
        let t = timeout.unwrap_or(DEFAULT_LONG_POLL_TIMEOUT);
        let body = serde_json::to_string(&GetUpdatesReq {
            get_updates_buf: Some(get_updates_buf.to_string()),
            base_info: build_base_info(),
        })
        .map_err(|e| e.to_string())?;

        match self
            .api_post(
                base_url,
                "ilink/bot/getupdates",
                &body,
                Some(token),
                Some(t + Duration::from_secs(5)),
            )
            .await
        {
            Ok(raw) => {
                serde_json::from_str(&raw).map_err(|e| format!("parse getUpdates: {e}"))
            }
            Err(e) if e.contains("timed out") || e.contains("Timeout") => {
                debug!("getUpdates: client timeout, returning empty");
                Ok(GetUpdatesResp {
                    ret: Some(0),
                    msgs: Some(vec![]),
                    get_updates_buf: Some(get_updates_buf.to_string()),
                    ..Default::default()
                })
            }
            Err(e) => Err(e),
        }
    }

    pub async fn send_message(
        &self,
        base_url: &str,
        token: &str,
        msg: WeixinMessage,
    ) -> Result<(), String> {
        // In WECLAWBOT_TEST_MODE=1 we tee outbound messages to a capture log
        // *and* skip the actual HTTP call. This lets the smoke harness drive
        // the daemon end-to-end without sending real messages to a user.
        if crate::service::test_inject::test_mode_enabled() {
            let payload = serde_json::to_value(&msg).unwrap_or(serde_json::Value::Null);
            crate::service::test_inject::capture_outbound("send_message", &payload);
            return Ok(());
        }

        let body = serde_json::to_string(&SendMessageReq {
            msg,
            base_info: build_base_info(),
        })
        .map_err(|e| e.to_string())?;

        self.api_post(
            base_url,
            "ilink/bot/sendmessage",
            &body,
            Some(token),
            Some(DEFAULT_API_TIMEOUT),
        )
        .await?;
        Ok(())
    }

    pub async fn get_config(
        &self,
        base_url: &str,
        token: &str,
        user_id: &str,
        context_token: Option<&str>,
    ) -> Result<GetConfigResp, String> {
        let body = serde_json::to_string(&GetConfigReq {
            ilink_user_id: user_id.to_string(),
            context_token: context_token.map(|s| s.to_string()),
            base_info: build_base_info(),
        })
        .map_err(|e| e.to_string())?;

        let raw = self
            .api_post(
                base_url,
                "ilink/bot/getconfig",
                &body,
                Some(token),
                Some(DEFAULT_CONFIG_TIMEOUT),
            )
            .await?;
        serde_json::from_str(&raw).map_err(|e| format!("parse getConfig: {e}"))
    }

    /// Ask iLink for a CDN upload URL. The returned `upload_full_url` (or
    /// `upload_param` for legacy fallback) is then POSTed to with the AES-
    /// encrypted file bytes, which yields a `downloadEncryptedQueryParam`
    /// used in subsequent send_message calls referencing this file.
    pub async fn get_upload_url(
        &self,
        base_url: &str,
        token: &str,
        req: GetUploadUrlReq,
    ) -> Result<GetUploadUrlResp, String> {
        let body = serde_json::to_string(&req).map_err(|e| e.to_string())?;
        let raw = self
            .api_post(
                base_url,
                "ilink/bot/getuploadurl",
                &body,
                Some(token),
                Some(DEFAULT_API_TIMEOUT),
            )
            .await?;
        serde_json::from_str(&raw).map_err(|e| format!("parse getUploadUrl: {e}; body={raw}"))
    }

    pub async fn send_typing(
        &self,
        base_url: &str,
        token: &str,
        user_id: &str,
        typing_ticket: Option<&str>,
    ) -> Result<(), String> {
        if crate::service::test_inject::test_mode_enabled() {
            crate::service::test_inject::capture_outbound(
                "send_typing",
                &serde_json::json!({"user_id": user_id, "ticket": typing_ticket}),
            );
            return Ok(());
        }
        let body = serde_json::to_string(&SendTypingReq {
            ilink_user_id: user_id.to_string(),
            typing_ticket: typing_ticket.map(|s| s.to_string()),
            status: TYPING_STATUS_TYPING,
            base_info: build_base_info(),
        })
        .map_err(|e| e.to_string())?;

        if let Err(e) = self
            .api_post(
                base_url,
                "ilink/bot/sendtyping",
                &body,
                Some(token),
                Some(DEFAULT_CONFIG_TIMEOUT),
            )
            .await
        {
            warn!("sendTyping failed: {e}");
        }
        Ok(())
    }
}

impl Default for ILinkClient {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// v2.2 L2.1: MessagingPlatform trait impl
// ============================================================================
//
// `MessagingPlatform` trait 是 v3 多协议（Telegram/Feishu/Discord）的
// 抽象边界。这里给 ILinkClient 加 impl，**不动**现有 17 处 `&ILinkClient`
// callsite —— 它们继续工作。新代码（mock 测试 / 未来 Provider trait）
// 可以接 `&dyn MessagingPlatform` 实现协议无关性。

use crate::puppet::{MessagingPlatform, OutboundMessage};
use async_trait::async_trait;

#[async_trait]
impl MessagingPlatform for ILinkClient {
    fn platform_id(&self) -> &'static str {
        "ilink-wechat"
    }

    async fn poll_updates(
        &self,
        token: &str,
        base_url: &str,
        buf: &str,
    ) -> Result<GetUpdatesResp, crate::error::WeclawError> {
        self.get_updates(base_url, token, buf, None)
            .await
            .map_err(crate::error::WeclawError::Internal)
    }

    async fn send_text(
        &self,
        token: &str,
        base_url: &str,
        out: OutboundMessage<'_>,
    ) -> Result<(), crate::error::WeclawError> {
        let text = out.text.unwrap_or_default().to_string();
        if text.is_empty() {
            return Ok(());
        }
        // iLink 消息构造：把 OutboundMessage 抽象映射到 WeixinMessage
        // 具体结构。v3 加新 platform 时各 impl 自己负责这层映射。
        //
        // 注意：这条路径**不会**保留 incoming session_id / from_user_id
        // swap —— 生产 inbound→reply 链仍走 `monitor::reply::send_text`，
        // 那里有完整 context。此 trait method 给 mock / 直接 push 场景用。
        let now_ms = chrono::Utc::now().timestamp_millis();
        let client_id = format!("weclawbot-{now_ms}-{:08x}", rand::random::<u32>());
        let msg = WeixinMessage {
            to_user_id: Some(out.target.to_string()),
            client_id: Some(client_id),
            message_type: Some(crate::api::types::MESSAGE_TYPE_BOT),
            message_state: Some(crate::api::types::MESSAGE_STATE_FINISH),
            create_time_ms: Some(now_ms),
            update_time_ms: Some(now_ms),
            item_list: Some(vec![crate::api::types::MessageItem {
                item_type: Some(crate::api::types::MESSAGE_ITEM_TYPE_TEXT),
                create_time_ms: Some(now_ms),
                update_time_ms: Some(now_ms),
                is_completed: Some(true),
                text_item: Some(crate::api::types::TextItem {
                    text: Some(text),
                }),
                ..Default::default()
            }]),
            ..Default::default()
        };
        self.send_message(base_url, token, msg)
            .await
            .map_err(crate::error::WeclawError::Internal)
    }

    async fn send_file(
        &self,
        token: &str,
        base_url: &str,
        out: OutboundMessage<'_>,
    ) -> Result<(), crate::error::WeclawError> {
        // ILinkClient 现有的 send file 路径在 media::outbound 里走多步
        // upload + send；trait 这里给 callers 一个简单 API，内部委托。
        // v2.2 只搭 trait 不强制 callsite 切换，所以这里返回 unimplemented
        // 占位，避免 trait 错位风险。真实路径继续走 media::outbound。
        let _ = (token, base_url, out);
        Err(crate::error::WeclawError::Internal(
            "send_file via trait not yet wired — use media::outbound directly for now".into(),
        ))
    }

    async fn send_typing(
        &self,
        token: &str,
        base_url: &str,
        target: &str,
    ) -> Result<(), crate::error::WeclawError> {
        self.send_typing(base_url, token, target, None)
            .await
            .map_err(crate::error::WeclawError::Internal)
    }

    fn supports_qr_login(&self) -> bool {
        true
    }

    async fn fetch_qr_code(
        &self,
        base_url: &str,
        bot_type: &str,
        local_token_list: &[String],
    ) -> Result<QrCodeResponse, crate::error::WeclawError> {
        // 委托给现有 fetch_qr_code 方法（无歧义，trait 默认 method
        // 跟 inherent method 同名时 Rust 自动 disambiguate）
        Self::fetch_qr_code(self, base_url, bot_type, local_token_list)
            .await
            .map_err(crate::error::WeclawError::Internal)
    }

    async fn poll_qr_status(
        &self,
        base_url: &str,
        qrcode_key: &str,
    ) -> Result<QrStatusResponse, crate::error::WeclawError> {
        Self::poll_qr_status(self, base_url, qrcode_key, None)
            .await
            .map_err(crate::error::WeclawError::Internal)
    }
}
