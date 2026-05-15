//! Inbound media handling.
//!
//! Philosophy: weclawbot is a pure forwarder. It does NOT interpret message
//! content based on type. For every non-text item WeChat delivers, we fetch
//! the bytes, decrypt if needed, save to a local file, and let the downstream
//! AI / webhook handle the file by its filesystem path. Image, voice, file,
//! video — all treated the same way.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use tracing::{debug, warn};

use crate::api::types::{CdnMedia, MessageItem, WeixinMessage, MESSAGE_ITEM_TYPE_TEXT};

use super::download::{fetch_and_decrypt, fetch_plain, pick_download_url};
use super::mime::{ext_from_filename, sniff_image_ext};
use super::store::save_inbound;

pub const DEFAULT_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

const MESSAGE_ITEM_TYPE_IMAGE: i32 = 2;
const MESSAGE_ITEM_TYPE_VOICE: i32 = 3;
const MESSAGE_ITEM_TYPE_FILE: i32 = 4;
const MESSAGE_ITEM_TYPE_VIDEO: i32 = 5;

#[derive(Debug, Clone)]
pub struct Attachment {
    /// Absolute path to the saved file.
    pub path: PathBuf,
    /// Original file name if the protocol provided one (file messages).
    pub original_name: Option<String>,
    /// Optional transcription/text content embedded in the protocol field (voice messages).
    pub embedded_text: Option<String>,
    /// Kind hint for logging only — never used to gate logic.
    pub kind: &'static str,
}

#[derive(Debug, Default, Clone)]
pub struct InboundContent {
    pub text: String,
    pub attachments: Vec<Attachment>,
    /// Non-fatal media fetch/decrypt errors (preserved for prompt context).
    pub errors: Vec<String>,
}

impl InboundContent {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.attachments.is_empty()
    }
}

/// Download, decrypt, and persist all media parts of an inbound message into
/// `inbound_dir`. The caller is responsible for choosing a sandbox-scoped
/// directory (e.g. `sandbox.media_inbound()`); this function does not know
/// or care about user scoping.
pub async fn resolve_message(
    inbound_dir: &Path,
    log_tag: &str,
    msg: &WeixinMessage,
) -> InboundContent {
    let account_id = log_tag; // kept as a name to minimise churn below
    let mut out = InboundContent::default();
    let items: Vec<MessageItem> = msg.item_list.clone().unwrap_or_default();
    let msg_id = msg.message_id.unwrap_or(0);

    let mut texts: Vec<String> = Vec::new();

    for (idx, item) in items.iter().enumerate() {
        let basename = format!("{msg_id}-{idx}");
        let item_type = item.item_type.unwrap_or(0);

        match item_type {
            t if t == MESSAGE_ITEM_TYPE_TEXT => {
                if let Some(s) = item.text_item.as_ref().and_then(|t| t.text.clone()) {
                    if !s.is_empty() {
                        texts.push(s);
                    }
                }
            }
            t if t == MESSAGE_ITEM_TYPE_IMAGE => {
                if let Some(image) = &item.image_item {
                    let media = image.media.as_ref();
                    let key = derive_image_key(image.aeskey.as_deref(), media);
                    match fetch_media_bytes(media, key.as_deref()).await {
                        Ok(bytes) => {
                            let ext = sniff_image_ext(&bytes);
                            match save_inbound(inbound_dir, &basename, ext, &bytes) {
                                Ok(p) => {
                                    debug!("[{account_id}] image saved: {}", p.display());
                                    out.attachments.push(Attachment {
                                        path: p,
                                        original_name: None,
                                        embedded_text: None,
                                        kind: "image",
                                    });
                                }
                                Err(e) => out.errors.push(format!("image save: {e}")),
                            }
                        }
                        Err(e) => {
                            warn!("[{account_id}] image fetch failed: {e}");
                            out.errors.push(format!("image fetch: {e}"));
                        }
                    }
                }
            }
            t if t == MESSAGE_ITEM_TYPE_VOICE => {
                if let Some(voice) = &item.voice_item {
                    let media = voice.media.as_ref();
                    let key = media.and_then(|m| m.aes_key.as_deref());
                    match fetch_media_bytes(media, key).await {
                        Ok(bytes) => match save_inbound(inbound_dir, &basename, ".silk", &bytes) {
                            Ok(p) => {
                                out.attachments.push(Attachment {
                                    path: p,
                                    original_name: None,
                                    embedded_text: voice.text.clone(),
                                    kind: "voice",
                                });
                            }
                            Err(e) => out.errors.push(format!("voice save: {e}")),
                        },
                        Err(e) => {
                            warn!("[{account_id}] voice fetch failed: {e}");
                            out.errors.push(format!("voice fetch: {e}"));
                            if let Some(txt) = voice.text.clone() {
                                texts.push(txt);
                            }
                        }
                    }
                }
            }
            t if t == MESSAGE_ITEM_TYPE_FILE => {
                if let Some(file) = &item.file_item {
                    let media = file.media.as_ref();
                    let key = media.and_then(|m| m.aes_key.as_deref());
                    match fetch_media_bytes(media, key).await {
                        Ok(bytes) => {
                            let name = file.file_name.as_deref().unwrap_or("file");
                            let ext = ext_from_filename(name);
                            match save_inbound(inbound_dir, &basename, &ext, &bytes) {
                                Ok(p) => {
                                    out.attachments.push(Attachment {
                                        path: p,
                                        original_name: file.file_name.clone(),
                                        embedded_text: None,
                                        kind: "file",
                                    });
                                }
                                Err(e) => out.errors.push(format!("file save: {e}")),
                            }
                        }
                        Err(e) => {
                            warn!("[{account_id}] file fetch failed: {e}");
                            out.errors.push(format!("file fetch: {e}"));
                        }
                    }
                }
            }
            t if t == MESSAGE_ITEM_TYPE_VIDEO => {
                if let Some(video) = &item.video_item {
                    let media = video.media.as_ref();
                    let key = media.and_then(|m| m.aes_key.as_deref());
                    match fetch_media_bytes(media, key).await {
                        Ok(bytes) => match save_inbound(inbound_dir, &basename, ".mp4", &bytes) {
                            Ok(p) => {
                                out.attachments.push(Attachment {
                                    path: p,
                                    original_name: None,
                                    embedded_text: None,
                                    kind: "video",
                                });
                            }
                            Err(e) => out.errors.push(format!("video save: {e}")),
                        },
                        Err(e) => {
                            warn!("[{account_id}] video fetch failed: {e}");
                            out.errors.push(format!("video fetch: {e}"));
                        }
                    }
                }
            }
            _ => {
                debug!("[{account_id}] unknown item type {item_type}");
            }
        }
    }

    out.text = texts.join("\n");
    out
}

fn derive_image_key(aeskey_hex: Option<&str>, media: Option<&CdnMedia>) -> Option<String> {
    if let Some(hex_key) = aeskey_hex.filter(|s| !s.is_empty()) {
        if let Ok(raw) = hex::decode(hex_key) {
            if raw.len() == 16 {
                return Some(base64::engine::general_purpose::STANDARD.encode(&raw));
            }
        }
    }
    media
        .and_then(|m| m.aes_key.clone())
        .filter(|s| !s.is_empty())
}

async fn fetch_media_bytes(
    media: Option<&CdnMedia>,
    aes_key_base64: Option<&str>,
) -> Result<Vec<u8>, String> {
    let m = media.ok_or("no media")?;
    let url = pick_download_url(
        m.full_url.as_deref(),
        m.encrypt_query_param.as_deref(),
        DEFAULT_CDN_BASE_URL,
    )
    .ok_or("no full_url / encrypt_query_param")?;

    // v2.1.C2: 通过 fetch_media_bytes 统一计数 inbound media 下载结果。
    // 不带 `kind` label —— caller 那里区分 image/voice/video/file 但
    // 这里看不到，要在 caller 处包 metric 会重复 4 次；统一 fetch 层
    // 简化为 plain/decrypt + ok/error 4 个组合。
    let is_encrypted = matches!(aes_key_base64, Some(k) if !k.is_empty());
    let result = if is_encrypted {
        fetch_and_decrypt(&url, aes_key_base64.unwrap()).await
    } else {
        fetch_plain(&url).await
    };
    metrics::counter!(
        "weclawbot_media_fetch_total",
        "mode" => if is_encrypted { "encrypted" } else { "plain" },
        "status" => if result.is_ok() { "ok" } else { "error" }
    )
    .increment(1);
    result
}
