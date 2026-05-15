//! Feishu / Lark event → [`CommonInbound`] mapping. v3.6 I3.
//!
//! ## Feishu Event Subscription
//!
//! Feishu 服务端 POST 一个 wrapper JSON：
//!
//! ```json
//! {
//!   "schema": "2.0",
//!   "header": {
//!     "event_type": "im.message.receive_v1",
//!     "event_id": "...",
//!     "tenant_key": "..."
//!   },
//!   "event": {
//!     "sender": { "sender_id": { "open_id": "ou_xxx" }, ... },
//!     "message": {
//!       "message_id": "om_xxx",
//!       "chat_id": "oc_xxx",
//!       "message_type": "text",
//!       "content": "{\"text\":\"hi\"}",  // 嵌套 JSON string
//!       "create_time": "1700000000000"
//!     }
//!   }
//! }
//! ```
//!
//! 我们只处理 `event_type == "im.message.receive_v1"` 的 `text` 消息。
//! 其他 (post / image / file / interactive) 留 v3.7 扩展。

use serde::Deserialize;
use std::path::PathBuf;

use crate::monitor::common::CommonInbound;
use crate::tenancy::TenantId;

#[derive(Debug, Deserialize)]
pub struct FeishuEvent {
    pub header: FeishuEventHeader,
    pub event: FeishuEventBody,
}

#[derive(Debug, Deserialize)]
pub struct FeishuEventHeader {
    pub event_type: String,
    #[serde(default)]
    pub event_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FeishuEventBody {
    pub sender: FeishuSender,
    pub message: FeishuMessage,
}

#[derive(Debug, Deserialize)]
pub struct FeishuSender {
    pub sender_id: FeishuSenderId,
}

#[derive(Debug, Deserialize)]
pub struct FeishuSenderId {
    /// open_id 是 per-app unique 的用户标识 — weclawbot 用它当 user_id
    pub open_id: String,
}

#[derive(Debug, Deserialize)]
pub struct FeishuMessage {
    pub message_id: String,
    pub chat_id: String,
    pub message_type: String,
    /// 嵌套 JSON string —— 不同 message_type 解出来不同结构
    pub content: String,
}

/// Feishu text message content schema: `{"text": "..."}`
#[derive(Debug, Deserialize)]
struct FeishuTextContent {
    text: String,
}

/// Convert Feishu event → CommonInbound. Returns None when:
/// - event_type != "im.message.receive_v1"
/// - message_type != "text" (post / image / 等留 v3.7)
/// - content JSON parse 失败 / text 空
pub fn event_to_common(
    ev: &FeishuEvent,
    tenant_id: TenantId,
    account_id: String,
) -> Option<CommonInbound> {
    if ev.header.event_type != "im.message.receive_v1" {
        return None;
    }
    if ev.event.message.message_type != "text" {
        // v3.7: 加 post / image / file mapping
        return None;
    }
    let parsed: FeishuTextContent = serde_json::from_str(&ev.event.message.content).ok()?;
    let text = parsed.text.trim();
    if text.is_empty() {
        return None;
    }

    Some(CommonInbound {
        tenant_id,
        account_id,
        platform_id: "feishu",
        user_id: ev.event.sender.sender_id.open_id.clone(),
        msg_id: ev.event.message.message_id.clone(),
        text: text.to_string(),
        attachments: Vec::<PathBuf>::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(event_type: &str, msg_type: &str, content: &str) -> FeishuEvent {
        FeishuEvent {
            header: FeishuEventHeader {
                event_type: event_type.into(),
                event_id: Some("e-1".into()),
            },
            event: FeishuEventBody {
                sender: FeishuSender {
                    sender_id: FeishuSenderId {
                        open_id: "ou_user1".into(),
                    },
                },
                message: FeishuMessage {
                    message_id: "om_msg1".into(),
                    chat_id: "oc_chat1".into(),
                    message_type: msg_type.into(),
                    content: content.into(),
                },
            },
        }
    }

    #[test]
    fn text_message_maps() {
        let ev = make_event(
            "im.message.receive_v1",
            "text",
            r#"{"text":"hello feishu"}"#,
        );
        let c = event_to_common(&ev, TenantId::default_tenant(), "acct-feishu".into()).unwrap();
        assert_eq!(c.platform_id, "feishu");
        assert_eq!(c.user_id, "ou_user1");
        assert_eq!(c.msg_id, "om_msg1");
        assert_eq!(c.text, "hello feishu");
    }

    #[test]
    fn non_text_skipped() {
        let ev = make_event(
            "im.message.receive_v1",
            "image",
            r#"{"image_key":"img_xxx"}"#,
        );
        assert!(event_to_common(&ev, TenantId::default_tenant(), "acct".into()).is_none());
    }

    #[test]
    fn other_event_types_skipped() {
        let ev = make_event(
            "im.message.read_v1",
            "text",
            r#"{"text":"x"}"#,
        );
        assert!(event_to_common(&ev, TenantId::default_tenant(), "acct".into()).is_none());
    }

    #[test]
    fn malformed_content_skipped() {
        let ev = make_event("im.message.receive_v1", "text", "not-json");
        assert!(event_to_common(&ev, TenantId::default_tenant(), "acct".into()).is_none());
    }

    #[test]
    fn empty_text_skipped() {
        let ev = make_event(
            "im.message.receive_v1",
            "text",
            r#"{"text":"   "}"#,
        );
        assert!(event_to_common(&ev, TenantId::default_tenant(), "acct".into()).is_none());
    }

    #[test]
    fn deserializes_real_feishu_payload() {
        let payload = r#"
        {
            "schema": "2.0",
            "header": {
                "event_type": "im.message.receive_v1",
                "event_id": "abc-123"
            },
            "event": {
                "sender": {
                    "sender_id": {"open_id": "ou_xxxxx"}
                },
                "message": {
                    "message_id": "om_yyyy",
                    "chat_id": "oc_zzzz",
                    "message_type": "text",
                    "content": "{\"text\":\"你好\"}"
                }
            }
        }
        "#;
        let ev: FeishuEvent = serde_json::from_str(payload).unwrap();
        let c = event_to_common(&ev, TenantId::default_tenant(), "acct".into()).unwrap();
        assert_eq!(c.text, "你好");
    }
}
