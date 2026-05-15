//! OIDC callback — code → token 交换。v3.4 G3。
//!
//! ## 流程
//!
//! 1. Browser ← 302 ← IdP, 带 `?code=AUTHZ_CODE&state=STATE`
//! 2. axum handler `post_oidc_callback`：
//!    - 校验 state 跟 session 里存的一致（防 CSRF）
//!    - POST `cfg.token_endpoint` 带 grant_type=authorization_code +
//!      code + code_verifier + client_id + client_secret + redirect_uri
//!    - 解析 response `{ access_token, id_token, ... }`
//!    - 解 id_token claims（不验签 — 留 v3.5 jsonwebtoken 依赖一起做）
//!    - JIT provision：根据 claims.email + IdP `claimed_role` 自动 mint
//!      admin_key (role default=read_only，super_admin 必须 operator 显式
//!      升级)
//!
//! ## v3.5 留的
//!
//! - **JWT signature verify**：当前只 decode claims，不验 IdP 签名。这
//!   依赖 jsonwebtoken crate + JWKS endpoint 缓存。生产部署前**必须**完成
//!   这层，否则任何能 reach `/oidc/callback` 的人伪造 id_token 都能登
//! - **role attribute mapping**：从 id_token claims `groups` / `roles` /
//!   `app_role` 反查 weclawbot Role。当前一律给 read_only，operator 手动
//!   升 role。
//! - **session 存储 state / nonce / code_verifier**：本期 stub —— 真实施需
//!   要 secure cookie 或 server-side session table

use base64::{engine::general_purpose, Engine as _};
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
/// caller 从 session 取出来传进来。
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

/// **不验签**地解 id_token claims。**生产部署必须先做 JWT verify**（v3.5
/// PR）—— 这层 decode 仅在 verify 通过之后用。
///
/// JWT 格式：`<header>.<payload>.<signature>`，三段 base64url-no-pad。
/// 我们只关心 payload claims。
pub fn decode_id_token_unverified(id_token: &str) -> Result<IdTokenClaims, String> {
    let mut parts = id_token.split('.');
    let _header = parts.next().ok_or("empty token")?;
    let payload_b64 = parts.next().ok_or("missing payload")?;
    let _signature = parts.next().ok_or("missing signature")?;
    let payload_bytes = general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| format!("payload base64: {e}"))?;
    let claims: IdTokenClaims = serde_json::from_slice(&payload_bytes)
        .map_err(|e| format!("payload json: {e}"))?;
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_jwt(claims: &serde_json::Value) -> String {
        let header_b64 = general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&json!({"alg":"RS256","typ":"JWT"})).unwrap());
        let payload_b64 = general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(claims).unwrap());
        // signature 部分给 dummy；本函数不验签，只要格式三段。
        format!("{header_b64}.{payload_b64}.dummy-sig")
    }

    #[test]
    fn decode_claims_extracts_email_and_sub() {
        let token = make_jwt(&json!({
            "sub": "user-42",
            "email": "alice@example.com",
            "name": "Alice",
            "iat": 1700000000,
            "exp": 1700003600,
            "nonce": "abc"
        }));
        let claims = decode_id_token_unverified(&token).unwrap();
        assert_eq!(claims.sub.as_deref(), Some("user-42"));
        assert_eq!(claims.email.as_deref(), Some("alice@example.com"));
        assert_eq!(claims.name.as_deref(), Some("Alice"));
        assert_eq!(claims.nonce.as_deref(), Some("abc"));
        assert_eq!(claims.exp, Some(1700003600));
    }

    #[test]
    fn malformed_jwt_returns_err() {
        assert!(decode_id_token_unverified("not.a.jwt-blob").is_err());
        assert!(decode_id_token_unverified("onlyonepart").is_err());
        assert!(decode_id_token_unverified("two.parts").is_err());
    }

    #[test]
    fn decode_tolerates_missing_optional_claims() {
        let token = make_jwt(&json!({"sub": "x"}));
        let claims = decode_id_token_unverified(&token).unwrap();
        assert_eq!(claims.sub.as_deref(), Some("x"));
        assert!(claims.email.is_none());
        assert!(claims.exp.is_none());
    }

    #[test]
    fn decode_handles_empty_object() {
        let token = make_jwt(&json!({}));
        let claims = decode_id_token_unverified(&token).unwrap();
        assert!(claims.sub.is_none());
    }
}
