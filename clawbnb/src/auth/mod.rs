pub mod accounts;
pub mod admin_key;
pub mod claude_oauth;
pub mod qr_login;
// v7.2: SSO (OIDC + SAML) supporting modules. `sso_session` is
// protocol-independent (signed cookie holding state across init →
// callback); `sso_provision` is the JIT admin_key minting policy.
// The protocol-specific verify lives under `crate::ee::{oidc,saml,jwks}`
// and is feature-gated; these two compile in default builds since
// the cookie + provisioning logic is tiny and is also reusable for
// future non-SSO redirect flows.
pub mod sso_provision;
pub mod sso_session;
