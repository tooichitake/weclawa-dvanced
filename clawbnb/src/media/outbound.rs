//! Build and send outbound WeChat media messages.
//!
//! Per the iLink protocol (mirrored from clawbnb-hub's TypeScript):
//!   - image  → MessageItem.type=2, image_item.media + mid_size
//!   - video  → MessageItem.type=5, video_item.media + video_size
//!   - file   → MessageItem.type=4, file_item.media + file_name + len
//!
//! `media.aes_key` is base64-encoded over the 32-char hex key string (the
//! decrypt path handles both raw and hex-of-base64; we always send hex form).
//! `media.encrypt_type` is 1 for outbound.

use std::path::Path;

use base64::Engine as _;
use tracing::{debug, info, warn};

use crate::api::client::ILinkClient;
use crate::api::types::{
    CdnMedia, FileItem, ImageItem, MessageItem, TextItem, VideoItem, WeixinMessage,
    MESSAGE_ITEM_TYPE_TEXT, MESSAGE_STATE_FINISH, MESSAGE_TYPE_BOT, UPLOAD_MEDIA_TYPE_FILE,
    UPLOAD_MEDIA_TYPE_IMAGE, UPLOAD_MEDIA_TYPE_VIDEO,
};
use crate::media::upload::{upload_file, UploadedFile};

const MESSAGE_ITEM_TYPE_IMAGE: i32 = 2;
const MESSAGE_ITEM_TYPE_FILE: i32 = 4;
const MESSAGE_ITEM_TYPE_VIDEO: i32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    File,
}

/// Pick the right WeChat message type for a given file based on extension.
pub fn classify(path: &Path) -> MediaKind {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => MediaKind::Image,
        "mp4" | "mov" | "webm" | "mkv" | "avi" => MediaKind::Video,
        _ => MediaKind::File,
    }
}

/// Plain text reply.
pub async fn send_text(
    client: &ILinkClient,
    base_url: &str,
    token: &str,
    incoming: &WeixinMessage,
    text: &str,
) -> Result<(), String> {
    let to = incoming
        .from_user_id
        .clone()
        .ok_or("incoming has no from_user_id")?;
    let item = MessageItem {
        item_type: Some(MESSAGE_ITEM_TYPE_TEXT),
        text_item: Some(TextItem {
            text: Some(text.to_string()),
        }),
        is_completed: Some(true),
        ..Default::default()
    };
    let msg = build_reply(incoming, &to, item);
    client.send_message(base_url, token, msg).await
}

/// Upload a local file and send it as a WeChat media message. Routes the
/// upload + message construction based on file extension.
pub async fn send_file(
    client: &ILinkClient,
    base_url: &str,
    token: &str,
    incoming: &WeixinMessage,
    file_path: &Path,
) -> Result<(), String> {
    let to = incoming
        .from_user_id
        .clone()
        .ok_or("incoming has no from_user_id")?;
    let kind = classify(file_path);
    let media_type = match kind {
        MediaKind::Image => UPLOAD_MEDIA_TYPE_IMAGE,
        MediaKind::Video => UPLOAD_MEDIA_TYPE_VIDEO,
        MediaKind::File => UPLOAD_MEDIA_TYPE_FILE,
    };

    info!(
        "outbound upload: kind={kind:?} file={} to={to}",
        file_path.display()
    );
    let uploaded = upload_file(client, base_url, token, &to, file_path, media_type).await?;
    debug!(
        "uploaded: filekey={} dl_len={} size={}",
        uploaded.filekey,
        uploaded.download_param.len(),
        uploaded.file_size
    );

    let item = build_media_item(file_path, kind, &uploaded);
    let msg = build_reply(incoming, &to, item);
    client.send_message(base_url, token, msg).await
}

fn build_media_item(file_path: &Path, kind: MediaKind, up: &UploadedFile) -> MessageItem {
    // Encode the hex string as UTF-8 bytes, then base64 — iLink protocol form.
    let aes_key_b64 = base64::engine::general_purpose::STANDARD.encode(up.aeskey_hex.as_bytes());

    let media = CdnMedia {
        encrypt_query_param: Some(up.download_param.clone()),
        aes_key: Some(aes_key_b64),
        encrypt_type: Some(1),
        full_url: None,
    };

    match kind {
        MediaKind::Image => MessageItem {
            item_type: Some(MESSAGE_ITEM_TYPE_IMAGE),
            is_completed: Some(true),
            image_item: Some(ImageItem {
                media: Some(media),
                aeskey: Some(up.aeskey_hex.clone()),
                ..Default::default()
            }),
            ..Default::default()
        },
        MediaKind::Video => MessageItem {
            item_type: Some(MESSAGE_ITEM_TYPE_VIDEO),
            is_completed: Some(true),
            video_item: Some(VideoItem {
                media: Some(media),
                video_size: Some(up.file_size_ciphertext as i64),
                ..Default::default()
            }),
            ..Default::default()
        },
        MediaKind::File => {
            let file_name = file_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(String::from);
            MessageItem {
                item_type: Some(MESSAGE_ITEM_TYPE_FILE),
                is_completed: Some(true),
                file_item: Some(FileItem {
                    media: Some(media),
                    file_name,
                    len: Some(up.file_size.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }
    }
}

fn build_reply(incoming: &WeixinMessage, to: &str, item: MessageItem) -> WeixinMessage {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let client_id = format!("weclawbot-{now_ms}-{:08x}", rand::random::<u32>());
    let mut item = item;
    item.create_time_ms = Some(now_ms);
    item.update_time_ms = Some(now_ms);

    WeixinMessage {
        to_user_id: Some(to.to_string()),
        from_user_id: incoming.to_user_id.clone(),
        client_id: Some(client_id),
        session_id: incoming.session_id.clone(),
        context_token: incoming.context_token.clone(),
        message_type: Some(MESSAGE_TYPE_BOT),
        message_state: Some(MESSAGE_STATE_FINISH),
        create_time_ms: Some(now_ms),
        update_time_ms: Some(now_ms),
        item_list: Some(vec![item]),
        ..Default::default()
    }
}

/// Best-effort: send a text caption (if any) followed by a file. Logs but
/// doesn't propagate per-file failures so other files still go through.
pub async fn send_text_then_file(
    client: &ILinkClient,
    base_url: &str,
    token: &str,
    incoming: &WeixinMessage,
    caption: Option<&str>,
    file_path: &Path,
) {
    if let Some(c) = caption.filter(|s| !s.is_empty()) {
        if let Err(e) = send_text(client, base_url, token, incoming, c).await {
            warn!("send caption failed: {e}");
        }
    }
    if let Err(e) = send_file(client, base_url, token, incoming, file_path).await {
        warn!("send file {} failed: {e}", file_path.display());
    }
}

/// Caps on remote downloads triggered by `mcp__weclawbot__attach_url`. Sizing
/// matches WeChat's own outbound media limits with a small safety margin.
const URL_DOWNLOAD_MAX_BYTES: u64 = 50 * 1024 * 1024;
const URL_DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Fetch a remote https URL into a temporary file in `temp_dir`, then send it
/// to the WeChat user via the same upload+sendmessage path as a local file.
/// Caption goes first (if non-empty), then the file. Failures are logged but
/// not propagated.
pub async fn send_text_then_url(
    client: &ILinkClient,
    base_url: &str,
    token: &str,
    incoming: &WeixinMessage,
    caption: Option<&str>,
    url: &str,
    temp_dir: &Path,
) {
    if let Some(c) = caption.filter(|s| !s.is_empty()) {
        if let Err(e) = send_text(client, base_url, token, incoming, c).await {
            warn!("send caption failed: {e}");
        }
    }
    match download_url_to_temp(url, temp_dir).await {
        Ok(path) => {
            if let Err(e) = send_file(client, base_url, token, incoming, &path).await {
                warn!("send url-file {} ({url}) failed: {e}", path.display());
            }
        }
        Err(e) => warn!("download url {url} failed: {e}"),
    }
}

async fn download_url_to_temp(url: &str, temp_dir: &Path) -> Result<std::path::PathBuf, String> {
    if !url.starts_with("https://") {
        return Err(format!("non-https url rejected: {url}"));
    }

    // SSRF guard (Phase 0.6 of the v2 hardening plan).
    // Claude's `attach_url` accepts any https URL; without this guard a
    // malicious prompt could trick Claude into hitting cloud metadata
    // endpoints (169.254.169.254), local Redis/Postgres on 127.0.0.1, or
    // any other internal service that's only reachable from this host.
    // Strategy: parse URL → resolve hostname to IPs → reject any
    // loopback / private / link-local / unspecified.
    let parsed = url::Url::parse(url).map_err(|e| format!("parse url: {e}"))?;
    let host = parsed.host_str().ok_or("url has no host")?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let ssrf_ok = resolve_and_check_public(host, port).await?;
    if !ssrf_ok {
        return Err(format!(
            "ssrf-block: refusing to fetch {url} (host {host} resolves to a \
             loopback / private / link-local / cloud-metadata address)"
        ));
    }

    std::fs::create_dir_all(temp_dir)
        .map_err(|e| format!("mkdir {}: {e}", temp_dir.display()))?;

    // No redirects: a 302 could send us from a public host to an internal
    // one and we'd lose the SSRF guard. Operators who legitimately need
    // a redirect chain can resolve it upstream.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(URL_DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|e| format!("http build: {e}"))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = resp.status();
    if status.is_redirection() {
        return Err(format!(
            "GET {url} -> {status} (redirects are disabled to preserve the SSRF guard)"
        ));
    }
    if !status.is_success() {
        return Err(format!("GET {url} -> {status}"));
    }

    // Check Content-Length BEFORE buffering the body so a 10 GB response
    // doesn't fill the heap.
    if let Some(cl) = resp.content_length() {
        if cl > URL_DOWNLOAD_MAX_BYTES {
            return Err(format!(
                "remote {url} too large: Content-Length={cl} > cap {URL_DOWNLOAD_MAX_BYTES}"
            ));
        }
    }

    let filename = pick_url_filename(url, resp.headers());
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("read body {url}: {e}"))?;
    if bytes.len() as u64 > URL_DOWNLOAD_MAX_BYTES {
        return Err(format!(
            "remote {url} too large: {} bytes > cap {URL_DOWNLOAD_MAX_BYTES}",
            bytes.len()
        ));
    }
    if bytes.is_empty() {
        return Err(format!("remote {url} empty"));
    }

    let suffix: u32 = rand::random();
    let target = temp_dir.join(format!("url-{suffix:08x}-{filename}"));
    std::fs::write(&target, &bytes)
        .map_err(|e| format!("write {}: {e}", target.display()))?;
    debug!(
        "downloaded {url} -> {} ({} bytes)",
        target.display(),
        bytes.len()
    );
    Ok(target)
}

// resolve_and_check_public / is_public_ip 已经搬到 `crate::storage::url_guard`，
// 让 webhook.rs (v2.1.A4) 共用同一份 SSRF 防护。下面是兼容 alias，
// 避免修改全部 callsite。
use crate::storage::url_guard::resolve_and_check_public;
#[cfg(test)]
use crate::storage::url_guard::is_public_ip;

fn pick_url_filename(url: &str, headers: &reqwest::header::HeaderMap) -> String {
    // Try Content-Disposition first, fall back to last URL segment.
    if let Some(cd) = headers
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(idx) = cd.find("filename=") {
            let rest = &cd[idx + "filename=".len()..];
            let trimmed = rest.trim_matches('"').split(';').next().unwrap_or("").trim();
            if !trimmed.is_empty() {
                return sanitize_filename(trimmed);
            }
        }
    }
    let last = url
        .split('?')
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .unwrap_or("download");
    if last.is_empty() {
        "download".to_string()
    } else {
        sanitize_filename(last)
    }
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            _ => c,
        })
        .collect::<String>()
        .chars()
        .take(128)
        .collect()
}

#[cfg(test)]
mod ssrf_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn is_public_ip_blocks_loopback() {
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(!is_public_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn is_public_ip_blocks_private_v4() {
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(172, 16, 5, 5))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    }

    #[test]
    fn is_public_ip_blocks_link_local_v4() {
        // 169.254.0.0/16 — covers cloud-metadata 169.254.169.254
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(169, 254, 0, 5))));
    }

    #[test]
    fn is_public_ip_blocks_cgnat() {
        // 100.64.0.0/10 — CGNAT (RFC 6598)
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(100, 127, 255, 254))));
        // 100.128.x.x is OUTSIDE CGNAT range — should pass
        assert!(is_public_ip(&IpAddr::V4(Ipv4Addr::new(100, 128, 0, 0))));
    }

    #[test]
    fn is_public_ip_blocks_unspecified_and_multicast() {
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1))));
    }

    #[test]
    fn is_public_ip_accepts_real_addresses() {
        // Cloudflare DNS
        assert!(is_public_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        // Google DNS
        assert!(is_public_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        // GitHub
        assert!(is_public_ip(&IpAddr::V4(Ipv4Addr::new(140, 82, 121, 4))));
    }

    #[test]
    fn is_public_ip_blocks_v6_link_local_and_ula() {
        // fe80::/10 link-local
        assert!(!is_public_ip(&IpAddr::V6("fe80::1".parse().unwrap())));
        // fc00::/7 unique-local
        assert!(!is_public_ip(&IpAddr::V6("fc00::1".parse().unwrap())));
        assert!(!is_public_ip(&IpAddr::V6("fd12:3456::1".parse().unwrap())));
    }

    #[test]
    fn is_public_ip_accepts_v6_global() {
        // Google IPv6 DNS
        assert!(is_public_ip(&IpAddr::V6("2001:4860:4860::8888".parse().unwrap())));
    }

    #[tokio::test]
    async fn resolve_loopback_literal_rejected() {
        let ok = resolve_and_check_public("127.0.0.1", 443).await.unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn resolve_cloud_metadata_rejected() {
        let ok = resolve_and_check_public("169.254.169.254", 80).await.unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn resolve_public_ip_literal_accepted() {
        let ok = resolve_and_check_public("1.1.1.1", 443).await.unwrap();
        assert!(ok);
    }
}
