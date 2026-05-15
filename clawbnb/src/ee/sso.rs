//! SSO (SAML / OIDC) — v3 enterprise feature.
//!
//! ## v3 占位
//!
//! 当前只定义 trait shape，让 v3 增量 PR 可以专注于具体 IdP 接入
//! （Okta / Azure AD / Google Workspace 等），不需要先做大重构。
//!
//! 主线 admin_key 流程不动 —— SSO 是**额外的**入站凭证渠道，验证通过后
//! 仍走 [`crate::auth::admin_key::AdminContext`] 拿 tenant + role。
//!
//! ## 设计要点
//!
//! - **每 tenant 独立 IdP 配置**：tenant A 用 Okta，tenant B 用 Azure AD，
//!   都通过同一 daemon binary 提供
//! - **JIT (Just-In-Time) provisioning**：第一次 SSO 登录自动 mint admin
//!   key（role 从 IdP attributes 映射，默认 `read_only`）
//! - **不存 password**：SSO callback 完成后只发 short-lived JWT cookie，
//!   key 仍归 admin_keys 表

use async_trait::async_trait;

use crate::error::WeclawError;
use crate::tenancy::TenantId;

/// IdP 抽象 — 每种 SSO 协议（SAML / OIDC）一个 impl。
#[async_trait]
pub trait IdentityProvider: Send + Sync {
    /// 协议标识（"saml" / "oidc"），metric label / audit 用。
    fn protocol(&self) -> &'static str;

    /// 启动 SSO 流程，返回 redirect URL。caller (axum handler) 把它做 302
    /// 重定向给浏览器。
    async fn initiate(
        &self,
        tenant_id: &TenantId,
        relay_state: &str,
    ) -> Result<String, WeclawError>;

    /// 接收 IdP callback，解析断言/token，返回 [`SsoIdentity`] 给 mint
    /// admin_key 用。失败 → Err，axum handler 渲染 SSO error 页。
    async fn handle_callback(
        &self,
        tenant_id: &TenantId,
        callback_payload: &CallbackPayload<'_>,
    ) -> Result<SsoIdentity, WeclawError>;
}

/// Generic callback payload — 不同协议字段不同，用 enum 抽象。
pub enum CallbackPayload<'a> {
    Saml { saml_response_b64: &'a str, relay_state: &'a str },
    Oidc { code: &'a str, state: &'a str },
}

/// Identity emitted by a successful SSO flow.
#[derive(Debug, Clone)]
pub struct SsoIdentity {
    /// IdP 主标识（email / subject DN）。
    pub subject: String,
    /// 可选 display name。
    pub display_name: Option<String>,
    /// IdP 声明的 role attribute（"super_admin" / "read_write" / "read_only"），
    /// 由 daemon 校验后映射到 [`crate::repo::admin_keys::Role`]。
    pub claimed_role: Option<String>,
}

// v3.1 增量 PR 真实现 SAML / OIDC impl；当前仅 trait shape。
