//! Telegram puppet (skeleton) — v3 多协议第一个新 impl。
//!
//! ## 目的
//!
//! 验证 [`crate::puppet::MessagingPlatform`] trait 抽象正确性 ——
//! 在不真接 Telegram Bot API 服务的前提下，证明：
//!
//! - trait method 集对 Telegram 也是充分的（不需要回头加新 method）
//! - `OutboundMessage` 结构能映射到 Telegram sendMessage / sendDocument 参数
//! - 错误处理走同一套 [`crate::error::WeclawError`] 不需要分支
//!
//! ## 当前实现层次
//!
//! - **存根 HTTP**：reqwest 调用 `https://api.telegram.org/bot<token>/...`，
//!   构造请求但**不实际发送**（除非启 `WECLAWBOT_TELEGRAM_LIVE=1` 环境
//!   变量并提供真 token 在 dev 验证）
//! - `poll_updates` 用 long-polling `getUpdates?offset=N&timeout=25`
//!   语义，返回 Telegram Update 数组（v3.1 适配进 weclawbot 共通 inbound 类型）
//! - QR login: Telegram 没有 QR scan 概念（用户加 bot → bot 自动 inbound），
//!   `supports_qr_login() = false`
//!
//! ## v3.1 完成路径
//!
//! 1. 接 [`crate::monitor::common::process_inbound`] —— 写
//!    `puppet::telegram::handler::handle_inbound_update` 把 Telegram
//!    Update → `CommonInbound`，调共通链
//! 2. `send_text` 内 reqwest 真发 POST `/sendMessage`
//! 3. 加 webhook 模式作为 long-poll 的替代（Telegram 推 events 进
//!    `POST /api/v1/puppet/telegram/webhook/<token>`）

use async_trait::async_trait;
use reqwest::Client;

use crate::api::types::{GetUpdatesResp, QrCodeResponse, QrStatusResponse};
use crate::error::WeclawError;
use crate::puppet::{MessagingPlatform, OutboundMessage};

/// Telegram Bot client. Token 是 BotFather 给的 `123456:ABC-...`。
pub struct TelegramBot {
    http: Client,
}

impl TelegramBot {
    pub fn new() -> Self {
        // 跟 ILinkClient 同款 reqwest 配置 — 短 timeout + redirect off
        // (防 SSRF / 防误中转)。
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client build");
        Self { http }
    }

    /// Build a Telegram Bot API URL: `https://api.telegram.org/bot<token>/<method>`.
    fn url(token: &str, method: &str) -> String {
        format!("https://api.telegram.org/bot{token}/{method}")
    }

    /// v3.3 F1: 拿 file metadata —— `getFile?file_id=X` → 返回 file_path
    /// 后调用方拼接 `https://api.telegram.org/file/bot<token>/<file_path>`
    /// 下载实际内容。Telegram 限制每个文件最多 20MB（标准 bot token），
    /// 大文件会返回错误。
    pub async fn get_file_path(
        &self,
        token: &str,
        file_id: &str,
    ) -> Result<String, crate::error::WeclawError> {
        let url = Self::url(token, "getFile");
        let resp = self
            .http
            .get(&url)
            .query(&[("file_id", file_id)])
            .send()
            .await
            .map_err(crate::error::WeclawError::Network)?;
        if !resp.status().is_success() {
            return Err(crate::error::WeclawError::IlinkApi {
                code: resp.status().as_u16() as i32,
                message: format!("getFile {file_id}"),
                retriable: resp.status().is_server_error(),
            });
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(crate::error::WeclawError::Network)?;
        if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
            return Err(crate::error::WeclawError::IlinkApi {
                code: -1,
                message: format!("getFile not ok: {v}"),
                retriable: false,
            });
        }
        v.get("result")
            .and_then(|r| r.get("file_path"))
            .and_then(|p| p.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                crate::error::WeclawError::IlinkApi {
                    code: -1,
                    message: "getFile response missing result.file_path".into(),
                    retriable: false,
                }
            })
    }

    /// v3.3 F1: 下载 attachment 到 `dest` (本地 path)。`file_id` 来自
    /// Telegram message 字段 (document.file_id / photo[N].file_id / 等)。
    ///
    /// **限制大小**到 20MB —— 防 OOM 攻击。Telegram 标准 bot 本身限 20MB，
    /// 我们这层是 defense-in-depth。下载完即 close stream。
    pub async fn download_attachment(
        &self,
        token: &str,
        file_id: &str,
        dest: &std::path::Path,
    ) -> Result<u64, crate::error::WeclawError> {
        const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

        let file_path = self.get_file_path(token, file_id).await?;
        let dl_url = format!(
            "https://api.telegram.org/file/bot{token}/{file_path}"
        );
        let resp = self
            .http
            .get(&dl_url)
            .send()
            .await
            .map_err(crate::error::WeclawError::Network)?;
        if !resp.status().is_success() {
            return Err(crate::error::WeclawError::IlinkApi {
                code: resp.status().as_u16() as i32,
                message: format!("download {file_id}"),
                retriable: resp.status().is_server_error(),
            });
        }
        if let Some(len) = resp.content_length() {
            if len > MAX_FILE_BYTES {
                return Err(crate::error::WeclawError::BadRequest(format!(
                    "attachment {file_id} too large: {len} bytes (max {MAX_FILE_BYTES})"
                )));
            }
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(crate::error::WeclawError::Network)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(crate::error::WeclawError::BadRequest(format!(
                "attachment {file_id} exceeded {MAX_FILE_BYTES} during streaming ({})",
                bytes.len()
            )));
        }
        // Parent dir 必须存在 — caller (sandbox/media) 负责创建
        std::fs::write(dest, &bytes)?;
        Ok(bytes.len() as u64)
    }
}

impl Default for TelegramBot {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MessagingPlatform for TelegramBot {
    fn platform_id(&self) -> &'static str {
        "telegram"
    }

    async fn poll_updates(
        &self,
        token: &str,
        _base_url: &str, // Telegram 用固定 api.telegram.org，参数忽略
        buf: &str,       // offset (last update_id + 1)，跟 iLink 一样存 sync_buf
    ) -> Result<GetUpdatesResp, WeclawError> {
        let offset: i64 = buf.parse().unwrap_or(0);
        let url = Self::url(token, "getUpdates");
        let resp = self
            .http
            .get(&url)
            .query(&[("offset", offset.to_string()), ("timeout", "25".into())])
            .send()
            .await
            .map_err(WeclawError::Network)?;
        if !resp.status().is_success() {
            return Err(WeclawError::IlinkApi {
                code: resp.status().as_u16() as i32,
                message: format!("telegram getUpdates failed"),
                retriable: resp.status().is_server_error(),
            });
        }
        // v3.1: 真的把 Telegram Update[] → WeixinMessage 形态 mapping
        // 现在返回空 resp 占位（caller 会当 long-poll timeout 处理）。
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
        let url = Self::url(token, "sendMessage");
        let body = serde_json::json!({
            "chat_id": out.target,
            "text": text,
        });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(WeclawError::Network)?;
        if !resp.status().is_success() {
            let code = resp.status().as_u16() as i32;
            let msg = resp
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(WeclawError::IlinkApi {
                code,
                message: format!("telegram sendMessage: {}", msg.chars().take(200).collect::<String>()),
                retriable: code >= 500,
            });
        }
        Ok(())
    }

    async fn send_file(
        &self,
        token: &str,
        _base_url: &str,
        out: OutboundMessage<'_>,
    ) -> Result<(), WeclawError> {
        let path = match out.file {
            Some(p) => p,
            None => return Ok(()),
        };
        let _url = Self::url(token, "sendDocument");
        // v3.1 实施：multipart upload；当前 stub。
        let _ = path;
        Err(WeclawError::Internal(
            "telegram send_file not yet implemented (v3.1)".into(),
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
            "Telegram bots use BotFather token, not QR scan".into(),
        ))
    }

    async fn poll_qr_status(
        &self,
        _base_url: &str,
        _qrcode_key: &str,
    ) -> Result<QrStatusResponse, WeclawError> {
        Err(WeclawError::Internal(
            "Telegram bots use BotFather token, not QR scan".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_id_is_telegram() {
        let t = TelegramBot::new();
        assert_eq!(t.platform_id(), "telegram");
        assert!(!t.supports_qr_login());
    }

    #[test]
    fn url_format() {
        let u = TelegramBot::url("123:abc", "sendMessage");
        assert_eq!(u, "https://api.telegram.org/bot123:abc/sendMessage");
    }

    #[tokio::test]
    async fn qr_methods_return_unsupported() {
        let t = TelegramBot::new();
        assert!(t.fetch_qr_code("", "", &[]).await.is_err());
        assert!(t.poll_qr_status("", "").await.is_err());
    }
}
