//! Platform-agnostic inbound processing — v3 multi-protocol 基础.
//!
//! ## 背景
//!
//! v2.x 的 `monitor::handler::handle_inbound_message` 接 `&ILinkClient` +
//! `&WeixinMessage`，完全针对 iLink 协议构建。直接换 trait 不解决问题 ——
//! 函数体里到处用 `msg.item_list[0].text_item.text` / `msg.context_token`
//! / `msg.session_id` 这些 iLink 特有字段，trait 抽象只是套了层皮。
//!
//! ## v3 抽象切法
//!
//! 真正的协议无关切割线在**消息内容已经被解析为 [`CommonInbound`]
//! 之后**。下面这部分对每个协议都长一样：
//!
//! ```text
//! 1. dedup            (msg_id → seen_messages 表)
//! 2. rate_limit       (user_id → per-minute window)
//! 3. Sandbox::ensure  (user_hash → ~/.weclawbot/users/<hash>/)
//! 4. dispatch_reply   (CommonInbound → ReplyProvider chain)
//! 5. audit            (action="inbound", actor=user_id)
//! ```
//!
//! 每个 platform 的 handler（`monitor::handler` for iLink, 未来
//! `puppet::telegram::handler` 等）负责：
//! - 把原生消息（WeixinMessage / TelegramUpdate / FeishuEvent...）转成
//!   [`CommonInbound`]
//! - 调 [`process_inbound`] 跑共通链
//! - 把 [`CommonReply`] 转回平台原生格式发出去
//!
//! ## v2.2 兼容
//!
//! 现行 `monitor::handler` **暂时**保留完整 iLink 路径 —— `process_inbound`
//! 留 skeleton API 在这，v3 第二个 platform impl 完成时再做"抽取共通逻辑
//! 到 process_inbound" 的迁移。这样：
//!
//! - v2.2 ship 时不破坏现有路径
//! - v3 Telegram 第一个 PR 同时完成"抽取 + 接入"，能在同一 PR 验证两个
//!   平台行为一致

use std::path::PathBuf;

use crate::tenancy::TenantId;

/// 协议无关的入站消息表示。各 platform handler 把原生消息映射进来。
#[derive(Debug, Clone)]
pub struct CommonInbound {
    /// v3 多租户：消息归属的 tenant。从 platform credentials → account
    /// → tenants 表反查得到。
    pub tenant_id: TenantId,
    /// 来源 account（iLink bot id / Telegram bot token id 等）。
    pub account_id: String,
    /// Platform 自报家门，用于 metric label 和 audit 区分来源。
    /// 取 [`crate::puppet::MessagingPlatform::platform_id`] 的返回值。
    pub platform_id: &'static str,
    /// 平台原生 user id（iLink openid / Telegram chat_id / 等）。
    /// **未哈希** —— 哈希到 `u-<hex>` 的转换在 Sandbox::ensure 里做。
    pub user_id: String,
    /// 平台原生 message id（去重用）。string 因为不同协议 type 不同
    /// （iLink i64 / Telegram i64 / Discord snowflake string）。
    pub msg_id: String,
    /// 文本（multipart 消息合并后的文本部分）。
    pub text: String,
    /// 附件 file paths（已经下载到本地 sandbox/media/）。
    pub attachments: Vec<PathBuf>,
}

// v7.0 housekeeping: `CommonReply` struct removed — never constructed.
// Each provider in `crate::ai::provider::ReplyProvider` returns its own
// concrete `ProviderOutput` type, which platform handlers consume
// directly. The "common reply" abstraction layer was a stale plan from
// v2.2 L2.2 that didn't survive into the actual ReplyProvider impl.

/// v3.1: 共通处理链真实施 — protocol-agnostic 部分（rate_limit +
/// dispatch_reply）。
///
/// **不**做的事（留给 platform-specific handler）：
/// - dedup —— 必须在 Sandbox::ensure **之前**做（性能）。每个协议自己
///   在 [`process_inbound`] 之前调 `dedup::is_duplicate_scoped`。
/// - Sandbox::ensure —— 每协议自己负责（因为 user_id → user_hash 哈希
///   过程协议无关，但失败时回复用户的消息方式协议特有）。
/// - resolve_message —— iLink 走 WeixinMessage item_list 解析；Telegram
///   走 Update.message.document 不同 mapping；这层 attachment 下载只能
///   protocol-specific。
///
/// 调用前置：caller 已经做完 dedup + Sandbox::ensure + resolve_message，
/// 把结果填进 `inbound` 和 `content`。
///
/// 返回值：text + files + urls + provider_name；caller 用 platform-specific
/// reply API 发回（iLink `reply::send_text` / Telegram `sendMessage`）。
pub async fn process_inbound(
    inbound: &CommonInbound,
    sandbox: &crate::sandbox::Sandbox,
    content: &crate::media::inbound::InboundContent,
    config: &crate::config::Config,
) -> crate::ai::provider::ProviderOutput {
    // --- Rate limit ---
    // 每用户 per-minute 阀。命中 → 不调 AI，直接返回节流文案。caller
    // 把它当普通 reply 发出（用户看得到 "频率过高"）。
    let user_id_typed = crate::ids::WeixinUserId::new(inbound.user_id.clone());
    if let Some(reason) =
        crate::monitor::rate_limit::check_inbound(&user_id_typed, config.rate_limit.user_per_minute)
    {
        tracing::warn!(
            "[{}] rate limited {}: {reason}",
            inbound.account_id,
            inbound.user_id
        );
        return crate::ai::provider::ProviderOutput {
            text: Some(reason),
            provider_name: "rate-limit",
            ..Default::default()
        };
    }

    // --- Dispatch via provider chain ---
    let ctx = crate::ai::provider::DispatchContext {
        account_id: &inbound.account_id,
        sandbox,
        content,
        config,
        msg: None, // protocol-agnostic path —— provider 优先用 inbound
        inbound: Some(inbound),
    };
    crate::ai::provider::run_default_chain(&ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_inbound_serializes_basic() {
        let c = CommonInbound {
            tenant_id: TenantId::default_tenant(),
            account_id: "acct-1".into(),
            platform_id: "ilink-wechat",
            user_id: "openid-xyz".into(),
            msg_id: "12345".into(),
            text: "hi".into(),
            attachments: vec![],
        };
        assert_eq!(c.platform_id, "ilink-wechat");
        assert!(c.tenant_id.is_default());
    }

    // process_inbound 真实施测试需要构造 Sandbox + InboundContent + Config
    // —— 这些都有自己的构造依赖（fs / DB）。集成测在 v3.1 第二个 platform
    // (Telegram) 落地时一并加 —— 单 platform 单元测意义不大。`run_default_chain`
    // 本身的语义已有 ai::provider::tests::first_enabled_wins 覆盖。
}
