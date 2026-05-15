//! Upload local files to the WeChat CDN.
//!
//! Two-step protocol:
//!   1. `ilink/bot/getuploadurl` → returns `upload_full_url` (or `upload_param`)
//!   2. POST AES-128-ECB-encrypted bytes to that URL → response header
//!      `x-encrypted-param` is the download token used in subsequent
//!      `sendmessage` calls referencing this file.

use std::path::Path;
use std::time::Duration;

use md5::{Digest, Md5};
use tracing::{debug, warn};

use crate::api::client::ILinkClient;
use crate::api::types::{GetUploadUrlReq, BaseInfo};
use crate::media::decrypt::{aes_ecb_padded_size, encrypt_aes_ecb_pkcs7};
use crate::media::inbound::DEFAULT_CDN_BASE_URL;

const UPLOAD_MAX_RETRIES: u32 = 3;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct UploadedFile {
    /// Random per-upload key (hex 32). Both we and iLink reference this.
    pub filekey: String,
    /// Header `x-encrypted-param` from the CDN response — goes into
    /// `media.encrypt_query_param` in the outbound message.
    pub download_param: String,
    /// AES-128 key, hex-encoded (32 chars). The outbound message field
    /// `media.aes_key` carries this as **base64 of the hex string bytes**
    /// (per iLink protocol — see decrypt.rs `parse_aes_key`).
    pub aeskey_hex: String,
    /// Plaintext file size in bytes.
    pub file_size: u64,
    /// Ciphertext (PKCS7-padded) size — used in image `mid_size` / video `video_size`.
    pub file_size_ciphertext: u64,
}

/// Upload a local file to the CDN.
///
/// `media_type` matches `UPLOAD_MEDIA_TYPE_*` constants in api::types.
/// Returns enough metadata for the caller to construct an outbound message
/// referencing this upload.
pub async fn upload_file(
    client: &ILinkClient,
    base_url: &str,
    token: &str,
    to_user_id: &str,
    file_path: &Path,
    media_type: i32,
) -> Result<UploadedFile, String> {
    let plaintext =
        std::fs::read(file_path).map_err(|e| format!("read {}: {e}", file_path.display()))?;
    let rawsize = plaintext.len() as u64;

    // In WECLAWBOT_TEST_MODE=1 we short-circuit the network calls and
    // synthesize a plausibly-shaped UploadedFile so the smoke harness can
    // verify the rest of the pipeline (classification, message build,
    // send_message capture). No bytes leave the daemon.
    if crate::service::test_inject::test_mode_enabled() {
        let filesize = aes_ecb_padded_size(plaintext.len()) as u64;
        let filekey: String = (0..16).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
        let aeskey_bytes: [u8; 16] = rand::random();
        let aeskey_hex: String = aeskey_bytes.iter().map(|b| format!("{b:02x}")).collect();
        crate::service::test_inject::capture_outbound(
            "upload_file",
            &serde_json::json!({
                "kind": media_type,
                "file": file_path.display().to_string(),
                "rawsize": rawsize,
                "to": to_user_id,
            }),
        );
        return Ok(UploadedFile {
            filekey: filekey.clone(),
            download_param: format!("test-mode-fake-{filekey}"),
            aeskey_hex,
            file_size: rawsize,
            file_size_ciphertext: filesize,
        });
    }

    let mut md5 = Md5::new();
    md5.update(&plaintext);
    let md5_hex: String = md5.finalize().iter().map(|b| format!("{b:02x}")).collect();

    let filesize = aes_ecb_padded_size(plaintext.len()) as u64;
    let filekey: String = (0..16).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
    let aeskey_bytes: [u8; 16] = rand::random();
    let aeskey_hex: String = aeskey_bytes.iter().map(|b| format!("{b:02x}")).collect();

    debug!(
        "upload: file={} rawsize={rawsize} filesize={filesize} md5={md5_hex} filekey={filekey}",
        file_path.display()
    );

    let req = GetUploadUrlReq {
        filekey: filekey.clone(),
        media_type,
        to_user_id: to_user_id.to_string(),
        rawsize,
        rawfilemd5: md5_hex,
        filesize,
        no_need_thumb: true,
        aeskey: aeskey_hex.clone(),
        base_info: BaseInfo::default(),
    };
    let resp = client.get_upload_url(base_url, token, req).await?;

    let upload_url = resp
        .upload_full_url
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            resp.upload_param.as_deref().map(|p| {
                format!(
                    "{}/upload?encrypted_query_param={}&filekey={}",
                    DEFAULT_CDN_BASE_URL.trim_end_matches('/'),
                    urlencoding::encode(p),
                    urlencoding::encode(&filekey),
                )
            })
        })
        .ok_or_else(|| {
            format!(
                "getUploadUrl returned no upload URL: ret={:?} errcode={:?} errmsg={:?}",
                resp.ret, resp.errcode, resp.errmsg
            )
        })?;

    let ciphertext = encrypt_aes_ecb_pkcs7(&plaintext, &aeskey_bytes);

    let download_param = post_to_cdn(&upload_url, &ciphertext, &filekey).await?;

    Ok(UploadedFile {
        filekey,
        download_param,
        aeskey_hex,
        file_size: rawsize,
        file_size_ciphertext: filesize,
    })
}

async fn post_to_cdn(
    url: &str,
    ciphertext: &[u8],
    filekey: &str,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(UPLOAD_TIMEOUT)
        .build()
        .map_err(|e| format!("http build: {e}"))?;

    let mut last_err: Option<String> = None;
    for attempt in 1..=UPLOAD_MAX_RETRIES {
        let resp = match client
            .post(url)
            .header("Content-Type", "application/octet-stream")
            .body(ciphertext.to_vec())
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("CDN POST network error attempt={attempt}: {e}");
                warn!("{msg}");
                last_err = Some(msg);
                continue;
            }
        };

        let status = resp.status();
        let dl = resp
            .headers()
            .get("x-encrypted-param")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        if status.is_client_error() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "CDN client error {status} (no retry) filekey={filekey} body={body}"
            ));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let msg = format!("CDN server error {status} attempt={attempt}: {body}");
            warn!("{msg}");
            last_err = Some(msg);
            continue;
        }

        match dl {
            Some(d) if !d.is_empty() => {
                debug!("CDN upload OK filekey={filekey} attempt={attempt}");
                return Ok(d);
            }
            _ => {
                let msg = format!(
                    "CDN response missing x-encrypted-param attempt={attempt} status={status}"
                );
                warn!("{msg}");
                last_err = Some(msg);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "CDN upload failed (no attempts)".into()))
}
