pub mod admin;
pub mod auth;
pub mod billing;
// v7.4 — per-tenant usage counters for external Prometheus → Stripe
// bridging. Compiles in default (no Stripe dep).
pub mod billing_metering;
pub mod feishu_webhook;
pub mod operator;
pub mod page;
pub mod sysconfig;
pub mod routes;
pub mod server;
// v7.2: SSO axum router — OIDC + SAML init/callback. Feature-gated
// since the verify logic lives under crate::ee::{oidc,saml,jwks,saml_dsig}.
#[cfg(feature = "ee")]
pub mod sso;
pub mod state;
pub mod test_inject;
