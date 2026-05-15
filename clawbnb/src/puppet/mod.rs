//! Puppet — 协议层抽象（v2.2 L2.1）。
//!
//! 之前 `api/client.rs::ILinkClient` 直接耦合 WeChat iLink 协议；poller、
//! handler、QR 流程、HTTP routes 全都拿 `&ILinkClient` 调具体方法。
//! v3 要支持 Telegram / Feishu / Discord 时必须先做这层抽象。
//!
//! ## 设计
//!
//! `MessagingPlatform` trait 定义 daemon 跟任何 IM 协议交互需要的所有
//! method（poll inbound、send text、send file、QR login 等）。每个协议
//! 一个 impl：
//!
//! - `puppet::ilink::ILink` — 原 `ILinkClient` 搬过来，零行为改动
//! - `puppet::mock::Mock` — 测试用，channel-based，不发真请求
//! - 未来 `puppet::telegram::Telegram` / `feishu::Feishu` 等
//!
//! ## 兼容性
//!
//! v2.2 不强制 callsite 立刻切到 `&dyn MessagingPlatform`。`ILinkClient`
//! 仍 `pub use` 出来，现有 17 处 `&ILinkClient` 不动。新代码 / 单测可
//! 选择性用 trait。**后续 PR** 增量迁移 callsite。
//!
//! ## async fn in trait
//!
//! 用 `#[async_trait]` 而不是 Rust 1.75+ 原生 async fn in trait，因为
//! 我们需要 `Box<dyn MessagingPlatform>` 形态在 dispatcher / state 里
//! 持有，而原生 async fn in trait 还不完全 dyn-compatible。

use std::path::Path;

use async_trait::async_trait;

use crate::api::types::{QrCodeResponse, QrStatusResponse, GetUpdatesResp};
use crate::error::WeclawError;

pub mod discord;
#[cfg(feature = "discord-gateway")]
pub mod discord_gateway;
pub mod discord_handler;
pub mod discord_inbound;
pub mod feishu;
pub mod feishu_handler;
pub mod feishu_inbound;
pub mod mock;
pub mod telegram;
pub mod telegram_handler;
pub mod telegram_inbound;

/// Outbound message description — daemon 把要发的内容传给 puppet，
/// 由 puppet 转换成具体协议格式。当前覆盖 text / file 两种。
#[derive(Debug, Clone)]
pub struct OutboundMessage<'a> {
    pub target: &'a str,
    pub text: Option<&'a str>,
    /// File path on host fs. Puppet 自己负责上传 + 转换。
    pub file: Option<&'a Path>,
    /// 文件的展示名（可选）。
    pub file_name: Option<&'a str>,
}

/// 多协议 IM 桥接的通用接口。Daemon 通过这层访问任何协议，不直接
/// 持有 ILinkClient / TelegramBot 等具体客户端。
///
/// 现阶段方法集合是 iLink 用得到的最小集；未来要支持的协议特性
/// （voice / sticker / group 等）按需扩展，加新方法时给默认 unimplemented
/// 实现以保持向后兼容。
#[async_trait]
pub trait MessagingPlatform: Send + Sync {
    /// 唯一标识，e.g. "ilink-wechat"。出现在 metric label / audit
    /// log 里，方便 multi-platform 部署时区分来源。
    fn platform_id(&self) -> &'static str;

    /// Long-poll：拉新消息，超时返回空 vec。`buf` 是 iLink 的
    /// continuation token；对其他协议可能不用。
    async fn poll_updates(
        &self,
        token: &str,
        base_url: &str,
        buf: &str,
    ) -> Result<GetUpdatesResp, WeclawError>;

    /// 发文本回复。
    async fn send_text(
        &self,
        token: &str,
        base_url: &str,
        out: OutboundMessage<'_>,
    ) -> Result<(), WeclawError>;

    /// 上传 + 发送文件作为消息。
    async fn send_file(
        &self,
        token: &str,
        base_url: &str,
        out: OutboundMessage<'_>,
    ) -> Result<(), WeclawError>;

    /// 让 IM 客户端显示 "正在输入"。可选 — 协议不支持就 no-op + Ok(())。
    async fn send_typing(
        &self,
        _token: &str,
        _base_url: &str,
        _target: &str,
    ) -> Result<(), WeclawError> {
        Ok(())
    }

    // --- QR login (协议有的话) ---

    /// 是否支持 QR 登录流程。默认 false —— 协议不支持则 daemon 改走
    /// 别的登录方式（如 token 直接填）。
    fn supports_qr_login(&self) -> bool {
        false
    }

    /// 拉一个新 QR 码。仅在 supports_qr_login() = true 时调用。
    async fn fetch_qr_code(
        &self,
        _base_url: &str,
        _bot_type: &str,
        _local_token_list: &[String],
    ) -> Result<QrCodeResponse, WeclawError> {
        Err(WeclawError::Internal(
            "this platform does not support QR login".into(),
        ))
    }

    /// 轮询 QR 状态：扫码进度 / 确认 / 过期。
    async fn poll_qr_status(
        &self,
        _base_url: &str,
        _qrcode_key: &str,
    ) -> Result<QrStatusResponse, WeclawError> {
        Err(WeclawError::Internal(
            "this platform does not support QR login".into(),
        ))
    }
}

// v2.2 L2.1: ILinkClient 仍是 prod 唯一 impl。trait impl 加在
// `api/client.rs` 末尾（紧贴具体实现，便于审计）。这里只 re-export。
pub use crate::api::client::ILinkClient;
