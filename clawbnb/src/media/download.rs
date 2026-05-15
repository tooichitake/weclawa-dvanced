use std::time::Duration;

use tracing::debug;

use super::decrypt::{decrypt_aes_ecb_pkcs7, parse_aes_key};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Fetch a CDN URL and return raw bytes (no decryption).
pub async fn fetch_plain(url: &str) -> Result<Vec<u8>, String> {
    debug!("CDN GET {url}");
    let client = reqwest::Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .build()
        .map_err(|e| format!("client: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("CDN fetch: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("CDN {status}: {body}"));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("CDN body: {e}"))?;
    Ok(bytes.to_vec())
}

/// Fetch CDN bytes and AES-128-ECB decrypt with the given key.
pub async fn fetch_and_decrypt(url: &str, aes_key_base64: &str) -> Result<Vec<u8>, String> {
    let key = parse_aes_key(aes_key_base64)?;
    let encrypted = fetch_plain(url).await?;
    decrypt_aes_ecb_pkcs7(&encrypted, &key)
}

/// Choose the download URL: prefer `full_url`, otherwise build from encrypt_query_param.
pub fn pick_download_url(
    full_url: Option<&str>,
    encrypt_query_param: Option<&str>,
    cdn_base_url: &str,
) -> Option<String> {
    if let Some(u) = full_url.filter(|s| !s.is_empty()) {
        return Some(u.to_string());
    }
    let q = encrypt_query_param.filter(|s| !s.is_empty())?;
    Some(format!(
        "{}/download?encrypted_query_param={}",
        cdn_base_url.trim_end_matches('/'),
        urlencoding::encode(q),
    ))
}
