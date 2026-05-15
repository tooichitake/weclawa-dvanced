//! OIDC callback — code → token 交换。v3.4 G3 / v7.2 verified.
//!
//! ## 流程
//!
//! 1. Browser ← 302 ← IdP, 带 `?code=AUTHZ_CODE&state=STATE`
//! 2. axum handler `service::sso::oidc_callback`：
//!    - 校验 state 跟 signed cookie 里存的一致（防 CSRF, [`crate::auth::sso_session`]）
//!    - POST `cfg.token_endpoint` 带 grant_type=authorization_code +
//!      code + code_verifier + client_id + client_secret + redirect_uri
//!      (function [`exchange_code`])
//!    - 解析 response `{ access_token, id_token, ... }`
//!    - **真验签** id_token via [`crate::ee::jwks::verify_id_token`]
//!      — JWKS endpoint 拉公钥 + RS256/ES256 RSA verify + iss/aud/exp/nonce
//!    - JIT provision via [`crate::auth::sso_provision::provision_from_sso`]
//!      — first login mint ReadOnly key in tenant, repeat login refresh
//!
//! ## v7.2 changes from v3.4
//!
//! - **`decode_id_token_unverified` removed**. Before v7.2 this function
//!   existed for "phased rollout" but was a foot-gun: every caller that
//!   used it accidentally instead of `verify_id_token` was an
//!   authentication bypass. Since the verify path is fully implemented,
//!   there's no reason to expose an unverified path. Tests now exercise
//!   `jwks::verify_id_token` against in-memory signed tokens via test
//!   helpers in that module.

use serde::Deserialize;

use crate::ee::oidc::OidcConfig;
use crate::error::WeclawError;

/// IdP /token 返回 body 形态。
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

/// 解出的 id_token claims (主流字段 — 仅本期 JIT provision 用到的)。
#[derive(Debug, Deserialize, Default)]
pub struct IdTokenClaims {
    pub sub: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub nonce: Option<String>,
    pub iat: Option<i64>,
    pub exp: Option<i64>,
}

/// Exchange authorization code for tokens at IdP `/token` endpoint.
///
/// `code_verifier` 是 [`crate::ee::oidc::auth_url`] 返回的 PKCE verifier —
/// caller 从 signed cookie 取出来传进来。
pub async fn exchange_code(
    cfg: &OidcConfig,
    code: &str,
    code_verifier: &str,
) -> Result<TokenResponse, WeclawError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(WeclawError::Network)?;

    let form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", &cfg.redirect_uri),
        ("client_id", &cfg.client_id),
        ("client_secret", &cfg.client_secret),
        ("code_verifier", code_verifier),
    ];

    let resp = client
        .post(&cfg.token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(WeclawError::Network)?;

    if !resp.status().is_success() {
        let code = resp.status().as_u16() as i32;
        let body = resp.text().await.unwrap_or_else(|_| "<no body>".into());
        return Err(WeclawError::IlinkApi {
            code,
            message: format!(
                "oidc token exchange: {}",
                body.chars().take(500).collect::<String>()
            ),
            retriable: false,
        });
    }
    resp.json::<TokenResponse>().await.map_err(WeclawError::Network)
}
