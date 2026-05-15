//! Enterprise Edition (`ee/`) — 商业级特性子树。
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
//! ## 实现状态 (v7.3)
//!
//! | 模块 | 状态 | 入口 |
//! |---|---|---|
//! | `oidc` + `oidc_callback` + `jwks` | **生产就绪** | `crate::service::sso::oidc_callback` |
//! | `saml` + `saml_dsig` | **生产就绪** (严格-profile XML-DSig verify) | `crate::service::sso::saml_acs` |
//! | `audit_archiver` + `audit_scheduler` | **生产就绪** (24h 周期 task) | `cli/start::run` |
//! | `audit_retention` | trait shape | 被 `audit_archiver` 实现 |
//! | `sla` | **类型层就位**，aggregator 未 wire | 待 plan M3 |
//!
//! v3 alpha 阶段（v5.x）确实是"占位模块"，v7.x 把 SSO + audit retention
//! 一路打通；SLA aggregator 是仍待补的最后一块（依赖 Prometheus rollup
//! pipeline，是独立的运维事项）。

#![cfg(feature = "ee")]

pub mod audit_archiver;
pub mod audit_retention;
pub mod audit_scheduler;
pub mod jwks;
pub mod oidc;
pub mod oidc_callback;
pub mod saml;
pub mod saml_dsig;
pub mod sla;
// v7.5 — DB-derived SLA rollup driver. Reads `user_history` +
// `audit_log` joined to `users.tenant_id`, upserts into `sla_rollup`
// every 5 minutes. See `sla_driver.rs` doc for the phase-1/phase-2
// split (phase 2 wants Prometheus query API for uptime + latency).
pub mod sla_driver;
// v7.3 housekeeping: `ee/sso.rs` (the dead `IdentityProvider` trait)
// removed. v7.2 wired SSO via free functions in `service/sso.rs` +
// `auth/sso_session.rs` + `auth/sso_provision.rs` — cleaner than the
// trait dispatch because OIDC and SAML callbacks have meaningfully
// different shape (GET vs POST, query vs form). If a third SSO
// protocol (e.g. WS-Federation) shows up we'll revisit the abstraction.
