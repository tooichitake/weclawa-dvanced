//! Enterprise Edition (`ee/`) — v3 商业级特性子树。
//!
//! ## 设计动机
//!
//! PostHog 模式：MIT 主线 (`src/*`，不含 `ee/`) 保持精简、self-host
//! 友好；商业级功能 (SSO/SAML/SLA monitoring/white-label/advanced RBAC)
//! 放 `src/ee/`，由 `--features ee` 启用。
//!
//! 这样：
//! - Open-source 用户编译 default 配置 → 体验完整的核心功能
//! - Hosted SaaS / 企业自部署 → `cargo build --features ee` 启用商业模块
//! - 商业模块 license 独立（v3 PR 时增 LICENSE-ee.md）
//!
//! ## 当前内容
//!
//! v3 一次 ship 只搭骨架 + 占位模块：
//!
//! - `sso` — SAML/OIDC 集成（v3.1 实施）
//! - `sla` — SLA 指标聚合（v3.1）
//! - `audit_retention` — 长期审计日志归档（v3.1 HIPAA mode 用）
//!
//! 每个子模块当前只导出 trait + stub impl，让 callsite 编译过 `cfg(feature
//! = "ee")` 即可；真实业务在 v3 增量 PR 补全。

#![cfg(feature = "ee")]

pub mod audit_archiver;
pub mod audit_retention;
pub mod jwks;
pub mod oidc;
pub mod oidc_callback;
pub mod saml;
pub mod sla;
pub mod sso;
