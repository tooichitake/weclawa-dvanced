//! Build + send a plain-text bot reply.

use tracing::{info, warn};

use crate::api::client::ILinkClient;
use crate::api::types::{
    MessageItem, TextItem, WeixinMessage, MESSAGE_ITEM_TYPE_TEXT, MESSAGE_STATE_FINISH,
    MESSAGE_TYPE_BOT,
};

/// Send a single text-only reply mirroring the structure of `incoming`
/// (preserves session_id, swaps from/to). No-op if the incoming message
/// has no `from_user_id`.
pub async fn send_text(
    client: &ILinkClient,
    base_url: &str,
    token: &str,
    incoming: &WeixinMessage,
    text: &str,
) {
    let to = match &incoming.from_user_id {
        Some(s) => s.clone(),
        None => return,
    };

    let now_ms = chrono::Utc::now().timestamp_millis();
    let client_id = format!("weclawbot-{now_ms}-{:08x}", rand::random::<u32>());

    let reply = WeixinMessage {
        to_user_id: Some(to.clone()),
        from_user_id: incoming.to_user_id.clone(),
        client_id: Some(client_id),
        session_id: incoming.session_id.clone(),
        message_type: Some(MESSAGE_TYPE_BOT),
        message_state: Some(MESSAGE_STATE_FINISH),
        create_time_ms: Some(now_ms),
        update_time_ms: Some(now_ms),
        item_list: Some(vec![MessageItem {
            item_type: Some(MESSAGE_ITEM_TYPE_TEXT),
            create_time_ms: Some(now_ms),
            update_time_ms: Some(now_ms),
            is_completed: Some(true),
            text_item: Some(TextItem {
                text: Some(text.to_string()),
            }),
            ..Default::default()
        }]),
        ..Default::default()
    };

    match client.send_message(base_url, token, reply).await {
        Ok(()) => info!(
            "reply -> {to}: {}",
            &text.chars().take(60).collect::<String>()
        ),
        Err(e) => warn!("send reply failed: {e}"),
    }
}
