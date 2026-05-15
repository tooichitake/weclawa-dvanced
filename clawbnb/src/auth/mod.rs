pub mod accounts;
pub mod admin_key;
pub mod claude_oauth;
pub mod qr_login;
// v7.5 — `sso_session` + `sso_provision` gated behind `--features ee`
// because their only consumers (service/sso routes + ee/oidc + ee/saml
// callbacks) are themselves ee-gated. v7.2 left them in default builds
// "in case future non-SSO redirect flows need them" but that
// possibility hasn't materialized; gating cleans up dead-code warnings
// in default builds without losing functionality.
#[cfg(feature = "ee")]
pub mod sso_provision;
#[cfg(feature = "ee")]
pub mod sso_session;
