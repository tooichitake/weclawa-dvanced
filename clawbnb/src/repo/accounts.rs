//! Account types — v4.2 types-only stub.
//!
//! sqlx async impl lives in [`crate::repo::accounts_async`].

use crate::ids::{AccountId, BaseUrl, BotToken, WeixinUserId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub account_id: AccountId,
    pub token: Option<BotToken>,
    pub base_url: BaseUrl,
    pub weixin_user_id: Option<WeixinUserId>,
    /// RFC3339 timestamp.
    pub saved_at: String,
    /// v5 M1: 协议归属 ── matches [`crate::puppet::MessagingPlatform::platform_id`]
    /// ("ilink-wechat" / "telegram" / "discord" / "feishu" / ...).
    /// 老 v2-v4 数据 schema 默认 'ilink-wechat'。
    pub platform_id: String,
}
