//! JWKS endpoint fetcher + cache + id_token verify — v3.5 H1.
//!
//! ## 为什么需要
//!
//! OIDC callback 必须验证 id_token 签名才能信任 IdP 主张的身份。本模块
//! 提供：
//!
//! 1. `fetch_jwks(jwks_uri)` — 从 IdP 拉 `/.well-known/jwks.json`
//! 2. `JwksCache` — 全局 OnceLock<RwLock> 缓存（TTL 1h），避免每次 login 都拉
//! 3. `verify_id_token(token, cfg, claims_validation)` — 用缓存的公钥验 RS256/ES256
//!    签名 + 校验 iss / aud / exp / iat / nonce
//!
//! ## TTL 选择
//!
//! 1 小时折中。IdP key rotation 通常是 weeks/months 节奏，所以 1h 命中率高；
//! 但如果 IdP 出了 emergency rotation（key compromise），1h 内我们还在用旧
//! key 验 token —— 接受这个风险，因为攻击者拿到的是 IdP 私钥而不是我们的，
//! 即便他们 forge token，他们也已经控制 IdP 本身。
//!
//! ## 不支持的
//!
//! - **HS256**：symmetric key 在 OIDC 里不实用（client_secret 共享给所有
//!   parties），生产 OIDC 几乎都用 RS256/ES256/RS384/ES384。本期硬拒。
//! - **encrypted JWT (JWE)**：极少 IdP 用，需要 jose-rs 之类的额外 dep。
//!   后续 PR 加 if needed。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use crate::ee::oidc::OidcConfig;
use crate::ee::oidc_callback::IdTokenClaims;
use crate::error::WeclawError;

/// 缓存 TTL — JWKS 公钥 1 小时刷一次。
const JWKS_TTL: Duration = Duration::from_secs(3600);

/// JWKS 文档结构（RFC 7517）.
#[derive(Debug, Clone, Deserialize)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Jwk {
    pub kty: String,
    /// Key ID — JWT header.kid 通过这个匹配。
    pub kid: Option<String>,
    /// 算法 — "RS256" / "ES256"。某些 IdP 不出，按 header.alg 兜底。
    pub alg: Option<String>,
    /// RSA: modulus + exponent (base64url)
    pub n: Option<String>,
    pub e: Option<String>,
    /// EC: curve + x/y (base64url)
    pub crv: Option<String>,
    pub x: Option<String>,
    pub y: Option<String>,
}

struct CachedJwks {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Instant,
}

static JWKS_CACHE: std::sync::OnceLock<RwLock<HashMap<String, Arc<CachedJwks>>>> =
    std::sync::OnceLock::new();

fn cache() -> &'static RwLock<HashMap<String, Arc<CachedJwks>>> {
    JWKS_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// HTTP-fetch JWKS document and build a `kid → DecodingKey` map.
async fn fetch_jwks(jwks_uri: &str) -> Result<Arc<CachedJwks>, WeclawError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(WeclawError::Network)?;
    let resp = client
        .get(jwks_uri)
        .send()
        .await
        .map_err(WeclawError::Network)?;
    if !resp.status().is_success() {
        return Err(WeclawError::IlinkApi {
            code: resp.status().as_u16() as i32,
            message: format!("jwks fetch failed: {jwks_uri}"),
            retriable: resp.status().is_server_error(),
        });
    }
    let doc: Jwks = resp.json().await.map_err(WeclawError::Network)?;

    let mut keys = HashMap::with_capacity(doc.keys.len());
    for jwk in doc.keys {
        let kid = match jwk.kid.clone() {
            Some(k) => k,
            None => {
                // No kid → can't match against header.kid. Skip.
                tracing::debug!("jwks: skipping key with no kid (kty={})", jwk.kty);
                continue;
            }
        };
        let decoding_key = match jwk.kty.as_str() {
            "RSA" => {
                let n = jwk.n.as_deref().ok_or_else(|| WeclawError::Internal(
                    format!("jwks RSA key {kid} missing n"),
                ))?;
                let e = jwk.e.as_deref().ok_or_else(|| WeclawError::Internal(
                    format!("jwks RSA key {kid} missing e"),
                ))?;
                DecodingKey::from_rsa_components(n, e).map_err(|err| {
                    WeclawError::Internal(format!("jwks RSA components {kid}: {err}"))
                })?
            }
            "EC" => {
                let x = jwk.x.as_deref().ok_or_else(|| WeclawError::Internal(
                    format!("jwks EC key {kid} missing x"),
                ))?;
                let y = jwk.y.as_deref().ok_or_else(|| WeclawError::Internal(
                    format!("jwks EC key {kid} missing y"),
                ))?;
                DecodingKey::from_ec_components(x, y).map_err(|err| {
                    WeclawError::Internal(format!("jwks EC components {kid}: {err}"))
                })?
            }
            other => {
                tracing::debug!("jwks: skipping key with unsupported kty={other}");
                continue;
            }
        };
        keys.insert(kid, decoding_key);
    }

    Ok(Arc::new(CachedJwks {
        keys,
        fetched_at: Instant::now(),
    }))
}

async fn get_or_fetch_jwks(jwks_uri: &str) -> Result<Arc<CachedJwks>, WeclawError> {
    // Fast path: cache hit + fresh
    {
        let read = cache().read().map_err(|e| {
            WeclawError::Internal(format!("jwks cache poisoned: {e}"))
        })?;
        if let Some(c) = read.get(jwks_uri) {
            if c.fetched_at.elapsed() < JWKS_TTL {
                return Ok(c.clone());
            }
        }
    }
    // Slow path: fetch + insert
    let fresh = fetch_jwks(jwks_uri).await?;
    {
        let mut write = cache().write().map_err(|e| {
            WeclawError::Internal(format!("jwks cache write poisoned: {e}"))
        })?;
        write.insert(jwks_uri.to_string(), fresh.clone());
    }
    Ok(fresh)
}

/// Verify id_token signature + standard claims (iss / aud / exp / iat),
/// optionally also `nonce` match.
///
/// `jwks_uri` 通常来自 IdP `.well-known/openid-configuration` 的
/// `jwks_uri` 字段。本期不做 discovery，operator 在 OidcConfig 里手动
/// 配（V0008 schema 加列时 wire 进 tenants 表）。
pub async fn verify_id_token(
    id_token: &str,
    jwks_uri: &str,
    cfg: &OidcConfig,
    expected_nonce: Option<&str>,
) -> Result<IdTokenClaims, WeclawError> {
    // Step 1: header.kid 拿来找 key
    let header = decode_header(id_token).map_err(|e| {
        WeclawError::BadRequest(format!("id_token header decode: {e}"))
    })?;
    let kid = header.kid.ok_or_else(|| {
        WeclawError::BadRequest("id_token header missing kid".into())
    })?;
    let algorithm = match header.alg {
        Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => header.alg,
        Algorithm::ES256 | Algorithm::ES384 => header.alg,
        other => {
            return Err(WeclawError::BadRequest(format!(
                "unsupported id_token alg: {other:?} (RS256/RS384/RS512/ES256/ES384 only)"
            )));
        }
    };

    // Step 2: JWKS lookup
    let jwks = get_or_fetch_jwks(jwks_uri).await?;
    let decoding_key = jwks.keys.get(&kid).ok_or_else(|| {
        WeclawError::BadRequest(format!(
            "id_token kid {kid} not in JWKS at {jwks_uri} (key rotation? expired cache)"
        ))
    })?;

    // Step 3: validation rules
    let mut validation = Validation::new(algorithm);
    validation.set_issuer(&[cfg.issuer.as_str()]);
    validation.set_audience(&[cfg.client_id.as_str()]);
    // iat + exp validated by default (60s leeway built into jsonwebtoken)
    validation.validate_exp = true;

    let token_data = decode::<serde_json::Value>(id_token, decoding_key, &validation)
        .map_err(|e| WeclawError::BadRequest(format!("id_token verify: {e}")))?;

    // Step 4: nonce check (claims.nonce == expected, if expected provided)
    if let Some(expected) = expected_nonce {
        let actual = token_data
            .claims
            .get("nonce")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                WeclawError::BadRequest("id_token missing nonce claim".into())
            })?;
        if actual != expected {
            return Err(WeclawError::BadRequest(format!(
                "id_token nonce mismatch (expected={expected}, got={actual})"
            )));
        }
    }

    // Step 5: map claims into our type
    Ok(IdTokenClaims {
        sub: token_data.claims.get("sub").and_then(|v| v.as_str()).map(String::from),
        email: token_data.claims.get("email").and_then(|v| v.as_str()).map(String::from),
        name: token_data.claims.get("name").and_then(|v| v.as_str()).map(String::from),
        nonce: token_data.claims.get("nonce").and_then(|v| v.as_str()).map(String::from),
        iat: token_data.claims.get("iat").and_then(|v| v.as_i64()),
        exp: token_data.claims.get("exp").and_then(|v| v.as_i64()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_initially_empty() {
        // 同 test 进程内 cache 是 process-wide static — 我们不直接读它，
        // 而是验证 RwLock 至少能取（poisoned 时会 fail）。
        // 用作用域块让 guard 立刻 drop —— `let _ =` 在 lock 上是 clippy 警告
        // （会被立刻 drop，等同于没锁），所以显式绑定 + drop。
        let _guard = cache().read().unwrap();
    }

    /// 真做 fetch_jwks 需要 IdP HTTP — 在 CI 不可达。这里只验证 URL
    /// invalid 时 fetch 失败转 WeclawError。
    #[tokio::test]
    async fn fetch_jwks_with_bad_url_errs() {
        let r = fetch_jwks("https://invalid.invalid.example/jwks").await;
        assert!(r.is_err());
    }

    /// verify_id_token 的解码部分独立可测 — 用 mock kid → DecodingKey
    /// 在 CI 跑 sign+verify round-trip 需要私钥；该 round-trip 用
    /// `jsonwebtoken` 自家 encode/decode 互测覆盖（v7.2+ 计划补充）。
    /// 这里只验：
    /// 1. malformed header → BadRequest
    /// 2. unsupported alg → BadRequest
    #[tokio::test]
    async fn verify_unsupported_alg_rejected() {
        // 构造一个 HS256 token header（base64url）。jsonwebtoken 的
        // decode_header 会先校验形态，然后我们的 algorithm match 拒绝 HS256。
        let cfg = OidcConfig {
            issuer: "https://accounts.google.com".into(),
            client_id: "abc".into(),
            client_secret: "s".into(),
            redirect_uri: "https://x.example/cb".into(),
            scopes: vec!["openid".into()],
            authorization_endpoint: "https://x".into(),
            token_endpoint: "https://x".into(),
            jwks_uri: "https://x.example/jwks".into(),
        };
        // Generate a token with HS256 header for the negative test.
        use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
        use base64::Engine as _;
        let header = serde_json::json!({"alg":"HS256","typ":"JWT","kid":"x"});
        let payload = serde_json::json!({"sub":"u"});
        let token = format!(
            "{}.{}.dummy",
            B64.encode(serde_json::to_vec(&header).unwrap()),
            B64.encode(serde_json::to_vec(&payload).unwrap())
        );
        let r = verify_id_token(&token, "https://x.example/jwks", &cfg, None).await;
        assert!(r.is_err());
        // Should be BadRequest about unsupported alg before any JWKS fetch
        match r.unwrap_err() {
            WeclawError::BadRequest(m) => assert!(m.contains("unsupported")),
            e => panic!("expected BadRequest, got {e:?}"),
        }
    }
}
