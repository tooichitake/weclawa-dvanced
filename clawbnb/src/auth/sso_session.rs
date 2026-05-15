//! Signed-cookie session for SSO flows (OIDC + SAML). v7.2.
//!
//! v7.5: gated behind `--features ee` since the only consumers
//! (`service/sso` routes + ee SAML/OIDC handlers) are ee-only.

#![cfg(feature = "ee")]

//!
//! ## 用途
//!
//! OIDC / SAML SP-initiated flow 都需要在 `init` 和 `callback` 之间持有
//! 一份每用户、每登录尝试的临时状态：
//!
//! - **OIDC**: `state` (CSRF), `nonce` (replay), `code_verifier` (PKCE),
//!   `tenant` (which IdP), `expiration`
//! - **SAML**: `relay_state` (`tenant` + opaque random), `request_id` (assertion
//!   InResponseTo binding), `expiration`
//!
//! 我们存哪？三个常见 pattern:
//! 1. server-side session table (DB row, 用 cookie 装 session id)
//! 2. signed cookie (cookie 内含 payload + HMAC, 服务端无状态)
//! 3. external session store (Redis / memcached)
//!
//! ## 为什么选 signed cookie (option 2)
//!
//! - 短生命期 (≤ 5 min)，垃圾自动失效，不需要 DB cleanup job
//! - 完全无状态，daemon HA / restart 不掉 in-flight 登录
//! - 不引 tower-cookies / async-session 等额外 dep
//! - HMAC-SHA256 over [payload bytes] 防篡改；MAC key 跟 AES master key
//!   独立 (`crypto::derive_subkey("sso-session-v1")`)
//!
//! Trade-off: cookie size 上限 4KB，payload 含 code_verifier (~43 chars)
//! + state/nonce (~32 chars 每) + tenant id (<32) + expiration (10)
//! < 200 bytes 远低于上限。
//!
//! ## Cookie 形态
//!
//! ```text
//! Set-Cookie: weclawbot_sso_state=<base64url(mac || payload_json)>;
//!             Path=/api/v1/auth/sso;
//!             HttpOnly;
//!             SameSite=Lax;       # 不能 Strict — IdP 回跳是 cross-site
//!             Secure;             # 仅当请求是 https (init handler 决定)
//!             Max-Age=300         # 5 分钟
//! ```
//!
//! `SameSite=Lax` 是必要的：`Strict` 会阻止 IdP 302 跳回时的 cookie，
//! `None` 又强制 Secure 在非 https 下 dev 不便。Lax 在"top-level
//! navigation"时发送 cookie，正好 cover SSO callback。
//!
//! ## 不该用本模块的场景
//!
//! - **持久登录** session — 不要把 admin_key 塞进 signed cookie，应该
//!   用 admin_keys 表 mint 长期 key 然后 Bearer header（现有路径）
//! - **跨 tenant 共享数据** — payload 只装跟一次 SSO attempt 相关的
//!   transient 字段，业务数据用别的 channel

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const COOKIE_NAME: &str = "weclawbot_sso_state";
const MAX_AGE_SECS: u32 = 300;
const KEY_LABEL: &str = "sso-session-v1";

/// MAC tag length — HMAC-SHA256 = 32 bytes.
const MAC_LEN: usize = 32;

/// Payload stored in the cookie. Fields populated only as needed for the
/// flow (OIDC needs `code_verifier`, SAML needs `request_id`; both use
/// `tenant`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SsoSession {
    /// Tenant id this SSO attempt is for. Verified in callback so a
    /// state value stolen from tenant A can't be replayed against
    /// tenant B's callback URL.
    pub tenant: String,
    /// OIDC: random opaque from `auth_url()`. SAML: same role as
    /// `RelayState`. Either way, used as CSRF protection — callback
    /// compares URL query `state` to cookie `state`.
    pub state: String,
    /// OIDC `nonce` claim. None for SAML.
    #[serde(default)]
    pub nonce: Option<String>,
    /// OIDC PKCE verifier (RFC 7636). None for SAML.
    #[serde(default)]
    pub code_verifier: Option<String>,
    /// SAML SP-issued AuthnRequest ID — assertion's `InResponseTo` must
    /// match. None for OIDC.
    #[serde(default)]
    pub request_id: Option<String>,
    /// Absolute expiration (unix seconds). Defense-in-depth on top of
    /// the cookie Max-Age — even if a UA ignores Max-Age, we reject
    /// expired cookies server-side.
    pub exp: i64,
}

impl SsoSession {
    pub fn for_oidc(
        tenant: String,
        state: String,
        nonce: String,
        code_verifier: String,
    ) -> Self {
        Self {
            tenant,
            state,
            nonce: Some(nonce),
            code_verifier: Some(code_verifier),
            request_id: None,
            exp: chrono::Utc::now().timestamp() + MAX_AGE_SECS as i64,
        }
    }

    pub fn for_saml(tenant: String, state: String, request_id: String) -> Self {
        Self {
            tenant,
            state,
            nonce: None,
            code_verifier: None,
            request_id: Some(request_id),
            exp: chrono::Utc::now().timestamp() + MAX_AGE_SECS as i64,
        }
    }

    /// True if `now >= self.exp`.
    pub fn is_expired(&self) -> bool {
        chrono::Utc::now().timestamp() >= self.exp
    }
}

/// Encode + sign a session into a Set-Cookie header value (everything
/// after `Set-Cookie: `). Caller (axum handler) attaches it to its
/// response via `header::SET_COOKIE`.
///
/// `secure` toggles the `Secure` attribute — pass `true` when serving
/// over HTTPS, `false` for `http://localhost` dev. Lying about this is
/// only a cosmetic concern (UAs send the cookie either way until the
/// scheme switches), so handlers can derive it from request scheme.
pub fn encode_cookie(session: &SsoSession, secure: bool) -> Result<String, String> {
    let key = crate::storage::crypto::derive_subkey(KEY_LABEL)?;
    let payload =
        serde_json::to_vec(session).map_err(|e| format!("serialize sso session: {e}"))?;
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(&key).map_err(|e| format!("hmac init: {e}"))?;
    mac.update(&payload);
    let tag = mac.finalize().into_bytes();

    let mut signed = Vec::with_capacity(MAC_LEN + payload.len());
    signed.extend_from_slice(&tag);
    signed.extend_from_slice(&payload);
    let encoded = URL_SAFE_NO_PAD.encode(&signed);

    let secure_attr = if secure { "; Secure" } else { "" };
    Ok(format!(
        "{COOKIE_NAME}={encoded}; Path=/api/v1/auth/sso; HttpOnly; SameSite=Lax; Max-Age={MAX_AGE_SECS}{secure_attr}"
    ))
}

/// Build a Set-Cookie value that clears the SSO state cookie. Used in
/// the callback handler to consume the one-shot state.
pub fn clear_cookie() -> String {
    format!(
        "{COOKIE_NAME}=; Path=/api/v1/auth/sso; HttpOnly; SameSite=Lax; Max-Age=0"
    )
}

/// Parse + verify a session out of an inbound `Cookie:` header value.
/// Returns `Err` on any of: missing cookie, bad base64, bad HMAC, bad
/// JSON, expired session.
pub fn decode_cookie(cookie_header: &str) -> Result<SsoSession, String> {
    let raw_value = cookie_header
        .split(';')
        .map(str::trim)
        .find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k == COOKIE_NAME {
                Some(v)
            } else {
                None
            }
        })
        .ok_or_else(|| format!("cookie {COOKIE_NAME} not present"))?;

    let signed = URL_SAFE_NO_PAD
        .decode(raw_value)
        .map_err(|e| format!("cookie base64: {e}"))?;

    if signed.len() < MAC_LEN {
        return Err(format!(
            "cookie too short ({} bytes; need >= {} for MAC)",
            signed.len(),
            MAC_LEN
        ));
    }
    let (tag_recv, payload) = signed.split_at(MAC_LEN);

    let key = crate::storage::crypto::derive_subkey(KEY_LABEL)?;
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(&key).map_err(|e| format!("hmac init: {e}"))?;
    mac.update(payload);
    // `verify_slice` is constant-time; reject on mismatch.
    mac.verify_slice(tag_recv)
        .map_err(|_| "cookie MAC mismatch (forged or wrong key)".to_string())?;

    let session: SsoSession =
        serde_json::from_slice(payload).map_err(|e| format!("cookie payload json: {e}"))?;
    if session.is_expired() {
        return Err(format!(
            "cookie expired at {} (now {})",
            session.exp,
            chrono::Utc::now().timestamp()
        ));
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_key() {
        // Use a deterministic test key — keep aligned with crypto::tests.
        if std::env::var("WECLAWBOT_DB_KEY").is_err() {
            // SAFETY: env mutation in tests, runner serializes.
            unsafe {
                std::env::set_var(
                    "WECLAWBOT_DB_KEY",
                    base64::engine::general_purpose::STANDARD.encode([0xCD; 32]),
                );
            }
        }
    }

    #[test]
    fn round_trip_oidc_session() {
        ensure_key();
        let s = SsoSession::for_oidc(
            "default".into(),
            "state-abc".into(),
            "nonce-xyz".into(),
            "verifier-12345".into(),
        );
        let cookie = encode_cookie(&s, false).unwrap();
        // Cookie header includes attributes; for decode_cookie we only
        // need the name=value pair.
        let value_only = cookie.split(';').next().unwrap();
        let back = decode_cookie(value_only).unwrap();
        assert_eq!(back.tenant, "default");
        assert_eq!(back.state, "state-abc");
        assert_eq!(back.nonce.as_deref(), Some("nonce-xyz"));
        assert_eq!(back.code_verifier.as_deref(), Some("verifier-12345"));
        assert!(back.request_id.is_none());
        assert!(!back.is_expired());
    }

    #[test]
    fn round_trip_saml_session() {
        ensure_key();
        let s = SsoSession::for_saml(
            "acme".into(),
            "relay-abc".into(),
            "_req-12345".into(),
        );
        let cookie = encode_cookie(&s, true).unwrap();
        assert!(cookie.contains("Secure"));
        let value_only = cookie.split(';').next().unwrap();
        let back = decode_cookie(value_only).unwrap();
        assert_eq!(back.tenant, "acme");
        assert_eq!(back.request_id.as_deref(), Some("_req-12345"));
        assert!(back.nonce.is_none());
    }

    #[test]
    fn tampered_payload_fails_mac() {
        ensure_key();
        let s = SsoSession::for_oidc("a".into(), "s".into(), "n".into(), "v".into());
        let cookie = encode_cookie(&s, false).unwrap();
        let value_only = cookie.split(';').next().unwrap();
        // Flip the last char (part of base64-encoded payload, not the
        // attribute bits).
        let mut chars: Vec<char> = value_only.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        let r = decode_cookie(&tampered);
        assert!(r.is_err());
    }

    #[test]
    fn missing_cookie_in_header_returns_err() {
        ensure_key();
        let r = decode_cookie("session=foo; other=bar");
        assert!(r.is_err());
    }

    #[test]
    fn cookie_too_short_returns_err() {
        ensure_key();
        let r = decode_cookie(&format!("{COOKIE_NAME}=AA"));
        assert!(r.is_err());
    }

    #[test]
    fn expired_cookie_rejected() {
        ensure_key();
        let mut s = SsoSession::for_oidc("a".into(), "s".into(), "n".into(), "v".into());
        s.exp = chrono::Utc::now().timestamp() - 1; // 1s in the past
        let cookie = encode_cookie(&s, false).unwrap();
        let value_only = cookie.split(';').next().unwrap();
        let r = decode_cookie(value_only);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("expired"));
    }

    #[test]
    fn clear_cookie_is_empty_value_zero_max_age() {
        let c = clear_cookie();
        assert!(c.starts_with(&format!("{COOKIE_NAME}=;")));
        assert!(c.contains("Max-Age=0"));
    }
}
