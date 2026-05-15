pub mod admin;
pub mod auth;
pub mod billing;
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
