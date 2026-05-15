use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BaseInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_version: Option<String>,
}

// --- QR Login ---

#[derive(Debug, Deserialize)]
pub struct QrCodeResponse {
    pub qrcode: String,
    pub qrcode_img_content: String,
}

#[derive(Debug, Deserialize)]
pub struct QrStatusResponse {
    pub status: String,
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub ilink_bot_id: Option<String>,
    #[serde(default)]
    pub baseurl: Option<String>,
    #[serde(default)]
    pub ilink_user_id: Option<String>,
    #[serde(default)]
    pub redirect_host: Option<String>,
}

// --- GetUpdates ---

#[derive(Debug, Serialize)]
pub struct GetUpdatesReq {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get_updates_buf: Option<String>,
    pub base_info: BaseInfo,
}

#[derive(Debug, Deserialize, Default)]
pub struct GetUpdatesResp {
    #[serde(default)]
    pub ret: Option<i32>,
    #[serde(default)]
    pub errcode: Option<i32>,
    #[serde(default)]
    pub errmsg: Option<String>,
    #[serde(default)]
    pub msgs: Option<Vec<WeixinMessage>>,
    #[serde(default)]
    pub get_updates_buf: Option<String>,
    #[serde(default)]
    pub longpolling_timeout_ms: Option<u64>,
}

// --- Messages ---

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WeixinMessage {
    #[serde(default)]
    pub seq: Option<i64>,
    #[serde(default)]
    pub message_id: Option<i64>,
    #[serde(default)]
    pub from_user_id: Option<String>,
    #[serde(default)]
    pub to_user_id: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub create_time_ms: Option<i64>,
    #[serde(default)]
    pub update_time_ms: Option<i64>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub message_type: Option<i32>,
    #[serde(default)]
    pub message_state: Option<i32>,
    #[serde(default)]
    pub item_list: Option<Vec<MessageItem>>,
    #[serde(default)]
    pub context_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MessageItem {
    #[serde(rename = "type", default)]
    pub item_type: Option<i32>,
    #[serde(default)]
    pub create_time_ms: Option<i64>,
    #[serde(default)]
    pub update_time_ms: Option<i64>,
    #[serde(default)]
    pub is_completed: Option<bool>,
    #[serde(default)]
    pub msg_id: Option<String>,
    #[serde(default)]
    pub text_item: Option<TextItem>,
    #[serde(default)]
    pub image_item: Option<ImageItem>,
    #[serde(default)]
    pub voice_item: Option<VoiceItem>,
    #[serde(default)]
    pub file_item: Option<FileItem>,
    #[serde(default)]
    pub video_item: Option<VideoItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TextItem {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CdnMedia {
    #[serde(default)]
    pub encrypt_query_param: Option<String>,
    #[serde(default)]
    pub aes_key: Option<String>,
    #[serde(default)]
    pub encrypt_type: Option<i32>,
    #[serde(default)]
    pub full_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageItem {
    #[serde(default)]
    pub media: Option<CdnMedia>,
    #[serde(default)]
    pub thumb_media: Option<CdnMedia>,
    #[serde(default)]
    pub aeskey: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VoiceItem {
    #[serde(default)]
    pub media: Option<CdnMedia>,
    #[serde(default)]
    pub encode_type: Option<i32>,
    #[serde(default)]
    pub playtime: Option<i64>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileItem {
    #[serde(default)]
    pub media: Option<CdnMedia>,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub md5: Option<String>,
    #[serde(default)]
    pub len: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VideoItem {
    #[serde(default)]
    pub media: Option<CdnMedia>,
    #[serde(default)]
    pub video_size: Option<i64>,
    #[serde(default)]
    pub play_length: Option<i64>,
}

// --- SendMessage ---

#[derive(Debug, Serialize)]
pub struct SendMessageReq {
    pub msg: WeixinMessage,
    pub base_info: BaseInfo,
}

// --- GetConfig ---

#[derive(Debug, Serialize)]
pub struct GetConfigReq {
    pub ilink_user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_token: Option<String>,
    pub base_info: BaseInfo,
}

#[derive(Debug, Deserialize, Default)]
pub struct GetConfigResp {
    #[serde(default)]
    pub ret: Option<i32>,
    #[serde(default)]
    pub errmsg: Option<String>,
    #[serde(default)]
    pub typing_ticket: Option<String>,
}

// --- SendTyping ---

#[derive(Debug, Serialize)]
pub struct SendTypingReq {
    pub ilink_user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typing_ticket: Option<String>,
    #[serde(default)]
    pub status: i32,
    pub base_info: BaseInfo,
}

// --- GetUploadUrl ---

#[derive(Debug, Serialize, Default)]
pub struct GetUploadUrlReq {
    pub filekey: String,
    pub media_type: i32,
    pub to_user_id: String,
    pub rawsize: u64,
    pub rawfilemd5: String,
    pub filesize: u64,
    pub no_need_thumb: bool,
    pub aeskey: String,
    pub base_info: BaseInfo,
}

#[derive(Debug, Deserialize, Default)]
pub struct GetUploadUrlResp {
    #[serde(default)]
    pub ret: Option<i32>,
    #[serde(default)]
    pub errcode: Option<i32>,
    #[serde(default)]
    pub errmsg: Option<String>,
    #[serde(default)]
    pub upload_param: Option<String>,
    #[serde(default)]
    pub thumb_upload_param: Option<String>,
    #[serde(default)]
    pub upload_full_url: Option<String>,
}

// --- Upload media type ---

pub const UPLOAD_MEDIA_TYPE_IMAGE: i32 = 1;
pub const UPLOAD_MEDIA_TYPE_VIDEO: i32 = 2;
pub const UPLOAD_MEDIA_TYPE_FILE: i32 = 3;
pub const UPLOAD_MEDIA_TYPE_VOICE: i32 = 4;

// --- Message type constants ---

pub const MESSAGE_TYPE_USER: i32 = 1;
pub const MESSAGE_TYPE_BOT: i32 = 2;
pub const MESSAGE_ITEM_TYPE_TEXT: i32 = 1;
pub const MESSAGE_STATE_NEW: i32 = 0;
pub const MESSAGE_STATE_FINISH: i32 = 2;
pub const TYPING_STATUS_TYPING: i32 = 1;

pub const SESSION_EXPIRED_ERRCODE: i32 = -14;
