//! OIDC (OpenID Connect) Identity Provider — v3.3 F3.
//!
//! # SECURITY: routes not mounted yet
//!
//! The cryptographic verify path is complete (see
//! [`crate::ee::jwks::verify_id_token`] — RS256/ES256 + JWKS cache +
//! iss/aud/exp/nonce checks). **What's missing before mounting
//! `/api/v1/auth/sso/oidc/{init,callback}` into axum** (plan v3 M2 scope):
//!
//! 1. Per-tenant `OidcConfig` storage — needs `tenants.oidc_config_json`
//!    column (migration V0013) + admin GUI form to write it.
//! 2. Session storage for `state`/`nonce`/`code_verifier` between
//!    `init` and `callback` — pick: signed-cookie (tower-cookies +
//!    HMAC key reused from BotToken AES key) OR DB table
//!    `oidc_sessions(state, ..., expires_at)`.
//! 3. JIT provisioning policy: who can self-register? Default `read_only`
//!    role, super_admin promotion via admin GUI — but the mapping from
//!    IdP `email`/`groups` claim → weclawbot role needs explicit
//!    operator opt-in (else any IdP user with valid email becomes
//!    read_only on this daemon).
//! 4. `decode_id_token_unverified` (in `oidc_callback.rs`) must be made
//!    private or deleted — public unverified decode is a footgun. The
//!    callback route should call `verify_id_token` exclusively.
//!
//! Until all four land, the OIDC flow stays unreachable. The crypto
//! primitives below are tested in isolation and ready to plug in.
//!
//! 通用 OIDC 适配 Okta / Azure AD / Google Workspace / Auth0 等。每个
//! tenant 一份 [`OidcConfig`]（issuer URL + client_id + client_secret +
//! redirect_uri），存 tenants 表的 `sso_config_json` 列（v3.4 schema 加）。
//!
//! ## v3.3 实施范围
//!
//! - [`OidcConfig`] 配置类型 + 校验
//! - [`auth_url`] 构造跳转 URL（含 state + nonce + PKCE code challenge）
//! - [`OidcProvider`] impl [`crate::ee::sso::IdentityProvider`] 的 `initiate`
//!
//! ## v3.4 留的
//!
//! - **token endpoint exchange**：用 `code` + `code_verifier` 换 id_token
//!   + access_token。这层需要 reqwest POST + form-encoded body + jsonwebtoken
//!   crate verify id_token signature。本期不引入 jsonwebtoken 依赖，留
//!   独立 PR。
//! - **JWKS 公钥缓存**：IdP 的 `/.well-known/jwks.json` 拉来缓存（key
//!   rotation 需要 refresh 机制）
//! - **真 JIT provisioning**：把 IdP attributes 映射到 admin_keys role

use base64::{engine::general_purpose, Engine as _};
use rand::TryRngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 一份完整 OIDC 配置 — 由 operator 在 tenants 表 sso_config_json 里写。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcConfig {
    /// IdP 标识符（`https://login.microsoftonline.com/<tenant>/v2.0`
    /// for Azure，`https://accounts.google.com` for Google 等）。
    pub issuer: String,
    /// IdP 给的 client identifier。
    pub client_id: String,
    /// IdP 给的 client secret（confidential client 模式）。public client
    /// 模式（移动 app）可以省，但 weclawbot 是 server-side flow，需要 secret。
    pub client_secret: String,
    /// SSO 完成后 IdP 重定向到这里。形如
    /// `https://daemon.example/api/v1/auth/sso/callback`。
    pub redirect_uri: String,
    /// scopes — 至少要 `openid`；通常加 `email profile`。
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// authorization_endpoint — 从 `<issuer>/.well-known/openid-configuration`
    /// 拿来。本期不做 discovery，operator 直接配。
    pub authorization_endpoint: String,
    /// token_endpoint — 同上。
    pub token_endpoint: String,
    /// jwks_uri — `<issuer>/.well-known/jwks.json` 通常即此。v7.2 加，
    /// `crate::ee::jwks::verify_id_token` 需要。
    pub jwks_uri: String,
}

fn default_scopes() -> Vec<String> {
    vec!["openid".into(), "email".into(), "profile".into()]
}

impl OidcConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !self.issuer.starts_with("https://") {
            return Err("OIDC issuer must be https://".into());
        }
        if !self.authorization_endpoint.starts_with("https://") {
            return Err("authorization_endpoint must be https://".into());
        }
        if !self.token_endpoint.starts_with("https://") {
            return Err("token_endpoint must be https://".into());
        }
        if !self.jwks_uri.starts_with("https://") {
            return Err("jwks_uri must be https://".into());
        }
        if !self.redirect_uri.starts_with("https://")
            && !self.redirect_uri.starts_with("http://localhost")
        {
            return Err("redirect_uri must be https:// (or http://localhost for dev)".into());
        }
        if self.client_id.is_empty() {
            return Err("client_id must not be empty".into());
        }
        if !self.scopes.iter().any(|s| s == "openid") {
            return Err("scopes must include 'openid'".into());
        }
        Ok(())
    }
}

/// Generated outbound auth URL + state / nonce / code_verifier 三元组。
/// caller 把 state/nonce/verifier 写进 session/cookie，callback 时校验。
#[derive(Debug, Clone)]
pub struct AuthInitiation {
    /// 完整的 redirect URL — 浏览器 302 跳过来。
    pub url: String,
    /// 随机字符串绑定到 session — 防 CSRF。
    pub state: String,
    /// 随机字符串放进 id_token claims — 防 replay。
    pub nonce: String,
    /// PKCE 原文 verifier — callback 时跟 IdP 一起换 token 时回传。
    pub code_verifier: String,
}

/// 生成 OIDC auth-flow 跳转 URL。
///
/// PKCE (Proof Key for Code Exchange, RFC 7636) — 即便 client_secret 漏了，
/// 攻击者也没法拿到 code_verifier 用 code 换 token。Server-side 流程不
/// 严格要求，但工业最佳实践。
pub fn auth_url(cfg: &OidcConfig) -> Result<AuthInitiation, String> {
    cfg.validate()?;

    // PKCE: code_verifier = 32 bytes random base64url-encoded (no padding)
    let mut verifier_bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut verifier_bytes)
        .map_err(|e| format!("rng: {e}"))?;
    let code_verifier = general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);

    // code_challenge = base64url(sha256(verifier))
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let challenge = general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());

    let mut state_bytes = [0u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut state_bytes)
        .map_err(|e| format!("rng: {e}"))?;
    let state = general_purpose::URL_SAFE_NO_PAD.encode(state_bytes);

    let mut nonce_bytes = [0u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut nonce_bytes)
        .map_err(|e| format!("rng: {e}"))?;
    let nonce = general_purpose::URL_SAFE_NO_PAD.encode(nonce_bytes);

    // URL-encode params
    let scope = cfg.scopes.join(" ");
    let qs = [
        ("response_type", "code"),
        ("client_id", &cfg.client_id),
        ("redirect_uri", &cfg.redirect_uri),
        ("scope", &scope),
        ("state", &state),
        ("nonce", &nonce),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
    .collect::<Vec<_>>()
    .join("&");

    let url = format!("{}?{}", cfg.authorization_endpoint, qs);

    Ok(AuthInitiation {
        url,
        state,
        nonce,
        code_verifier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cfg() -> OidcConfig {
        OidcConfig {
            issuer: "https://accounts.google.com".into(),
            client_id: "client-abc".into(),
            client_secret: "shh".into(),
            redirect_uri: "https://daemon.example/api/v1/auth/sso/callback".into(),
            scopes: vec!["openid".into(), "email".into()],
            authorization_endpoint: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_endpoint: "https://oauth2.googleapis.com/token".into(),
            jwks_uri: "https://www.googleapis.com/oauth2/v3/certs".into(),
        }
    }

    #[test]
    fn validate_ok() {
        assert!(sample_cfg().validate().is_ok());
    }

    #[test]
    fn validate_rejects_http_issuer() {
        let mut c = sample_cfg();
        c.issuer = "http://insecure".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_requires_openid_scope() {
        let mut c = sample_cfg();
        c.scopes = vec!["email".into()];
        assert!(c.validate().unwrap_err().contains("openid"));
    }

    #[test]
    fn validate_allows_http_localhost_redirect() {
        let mut c = sample_cfg();
        c.redirect_uri = "http://localhost:8080/cb".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn auth_url_contains_required_params() {
        let init = auth_url(&sample_cfg()).unwrap();
        assert!(init.url.contains("response_type=code"));
        assert!(init.url.contains("client_id=client-abc"));
        assert!(init.url.contains("code_challenge_method=S256"));
        assert!(init.url.contains("state="));
        assert!(init.url.contains("nonce="));
        assert!(init.url.contains("code_challenge="));
        // state / nonce / verifier should each be non-empty
        assert!(!init.state.is_empty());
        assert!(!init.nonce.is_empty());
        assert!(!init.code_verifier.is_empty());
    }

    #[test]
    fn auth_url_generates_different_state_each_call() {
        let a = auth_url(&sample_cfg()).unwrap();
        let b = auth_url(&sample_cfg()).unwrap();
        assert_ne!(a.state, b.state);
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.code_verifier, b.code_verifier);
    }

    #[test]
    fn challenge_is_sha256_of_verifier() {
        let init = auth_url(&sample_cfg()).unwrap();
        // Recompute challenge from verifier and look for it in the URL
        let mut hasher = Sha256::new();
        hasher.update(init.code_verifier.as_bytes());
        let expected = general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize());
        // Challenge appears URL-encoded; check both raw and percent-encoded
        let pct_encoded = urlencoding::encode(&expected).to_string();
        assert!(
            init.url.contains(&expected) || init.url.contains(&pct_encoded),
            "challenge missing from url; expected {expected}"
        );
    }
}
