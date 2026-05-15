use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderValue};

const ILINK_APP_ID: &str = "bot";
const CHANNEL_VERSION: &str = env!("CARGO_PKG_VERSION");
const WEIXIN_UPSTREAM_BASELINE: &str = "2.1.1";

fn build_client_version(version: &str) -> u32 {
    let parts: Vec<u32> = version
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let major = parts.first().copied().unwrap_or(0) & 0xff;
    let minor = parts.get(1).copied().unwrap_or(0) & 0xff;
    let patch = parts.get(2).copied().unwrap_or(0) & 0xff;
    (major << 16) | (minor << 8) | patch
}

fn random_wechat_uin() -> String {
    let val: u32 = rand::rng().random();
    BASE64.encode(val.to_string().as_bytes())
}

pub fn build_common_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("iLink-App-Id", HeaderValue::from_static(ILINK_APP_ID));
    let version = build_client_version(WEIXIN_UPSTREAM_BASELINE);
    if let Ok(v) = HeaderValue::from_str(&version.to_string()) {
        headers.insert("iLink-App-ClientVersion", v);
    }
    headers
}

pub fn build_post_headers(token: Option<&str>) -> HeaderMap {
    let mut headers = build_common_headers();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    headers.insert(
        "AuthorizationType",
        HeaderValue::from_static("ilink_bot_token"),
    );
    if let Ok(v) = HeaderValue::from_str(&random_wechat_uin()) {
        headers.insert("X-WECHAT-UIN", v);
    }
    if let Some(tok) = token {
        if !tok.is_empty() {
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {tok}")) {
                headers.insert("Authorization", v);
            }
        }
    }
    headers
}

pub fn channel_version() -> String {
    CHANNEL_VERSION.to_string()
}
