//! Telegram Update → `CommonInbound` 映射 (v3.2 E4).
//!
//! Telegram Bot API getUpdates 返回 `result: [{update_id, message: {...}}, ...]`。
//! 这里把单个 Update 转成 [`crate::monitor::common::CommonInbound`]，让
//! `monitor::common::process_inbound` 直接吃。
//!
//! ## 当前不实施的
//!
//! - **Attachment 下载**：Telegram message.document / photo / voice 需要先调
//!   `getFile?file_id=` 拿 file_path，再拉 `https://api.telegram.org/file/bot<token>/<file_path>`。
//!   这层 IO 留 v3.2.1 PR — 它涉及到 sandbox/media 目录路径 + retry 策略
//!   + Telegram 文件大小限制（20MB）等。目前 attachments 字段返回空 vec，
//!   text-only 消息已经能跑通。
//!
//! - **Markdown / HTML 格式化**：Telegram 支持 parse_mode；当前 mapping
//!   只取 text 原文。
//!
//! - **多媒体 caption**：document/photo 带 caption 时把 caption 当 text。

use serde::Deserialize;
use std::path::PathBuf;

use crate::monitor::common::CommonInbound;
use crate::tenancy::TenantId;

/// 顶层 getUpdates 返回。
#[derive(Debug, Deserialize)]
pub struct GetUpdatesResp {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub result: Vec<Update>,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    /// 不是所有 Update 都有 message（edited_message / channel_post 等略）。
    pub message: Option<Message>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    pub from: Option<User>,
    #[serde(default)]
    pub text: Option<String>,
    /// document.caption / photo[].caption 的合并占位（v3.2.1 完整化）
    #[serde(default)]
    pub caption: Option<String>,
    /// v3.5 H2: attachment file_id 提取入口。各类型 attachment 都有
    /// file_id (string)，handler 拿到 file_id 之后调
    /// `TelegramBot::download_attachment` 拉到 sandbox/media/。
    #[serde(default)]
    pub document: Option<Document>,
    #[serde(default)]
    pub voice: Option<Voice>,
    #[serde(default)]
    pub photo: Option<Vec<PhotoSize>>,
}

/// `message.document` — 文件附件。
#[derive(Debug, Deserialize)]
pub struct Document {
    pub file_id: String,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

/// `message.voice` — 语音消息。
#[derive(Debug, Deserialize)]
pub struct Voice {
    pub file_id: String,
    #[serde(default)]
    pub duration: Option<u32>,
}

/// `message.photo` — Telegram 同一图片多个 size，按从小到大排。我们
/// 通常取最大那个（数组末尾）。
#[derive(Debug, Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

impl Message {
    /// 收集本条 message 上所有的 (file_id, suggested_name) 对。handler
    /// 用它驱动 attachment 下载循环。
    pub fn collect_attachments(&self) -> Vec<(String, Option<String>)> {
        let mut out = Vec::new();
        if let Some(d) = &self.document {
            out.push((d.file_id.clone(), d.file_name.clone()));
        }
        if let Some(v) = &self.voice {
            out.push((v.file_id.clone(), Some(format!("voice-{}.ogg", v.file_id))));
        }
        if let Some(photos) = &self.photo {
            // 取最大那张（last 通常是 max size，但严谨起见按 width*height
            // 排序兜底）
            if let Some(largest) = photos
                .iter()
                .max_by_key(|p| {
                    let w = p.width.unwrap_or(0) as u64;
                    let h = p.height.unwrap_or(0) as u64;
                    w * h
                })
            {
                out.push((largest.file_id.clone(), Some(format!("photo-{}.jpg", largest.file_id))));
            }
        }
        out
    }
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
}

/// 把一个 Update 映射成 CommonInbound。返回 None = 跳过该 update（不是
/// 用户 message / 没有 text）。
pub fn update_to_common(
    update: &Update,
    tenant_id: TenantId,
    account_id: String,
) -> Option<CommonInbound> {
    let msg = update.message.as_ref()?;
    // 文本取 text 或 caption，二者都没就 skip（v3.2.1 加 audio/video 处理）
    let text = msg
        .text
        .clone()
        .or_else(|| msg.caption.clone())
        .unwrap_or_default();
    if text.is_empty() {
        return None;
    }
    // user_id 优先取 from.id（私聊），fallback chat.id（群里 bot 看不到 from
    // 的极少数情况）。Telegram id 是 i64，转 string 进 CommonInbound。
    let user_id = msg
        .from
        .as_ref()
        .map(|u| u.id.to_string())
        .unwrap_or_else(|| msg.chat.id.to_string());

    Some(CommonInbound {
        tenant_id,
        account_id,
        platform_id: "telegram",
        user_id,
        msg_id: msg.message_id.to_string(),
        text,
        attachments: Vec::<PathBuf>::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_update(text: Option<&str>, caption: Option<&str>) -> Update {
        Update {
            update_id: 100,
            message: Some(Message {
                message_id: 42,
                chat: Chat {
                    id: 555,
                    username: Some("test_user".into()),
                },
                from: Some(User {
                    id: 999,
                    username: Some("alice".into()),
                }),
                text: text.map(|s| s.to_string()),
                caption: caption.map(|s| s.to_string()),
                document: None,
                voice: None,
                photo: None,
            }),
        }
    }

    #[test]
    fn text_update_maps_correctly() {
        let u = sample_update(Some("hello"), None);
        let c = update_to_common(
            &u,
            TenantId::default_tenant(),
            "acct-tg-1".into(),
        )
        .unwrap();
        assert_eq!(c.platform_id, "telegram");
        assert_eq!(c.user_id, "999");
        assert_eq!(c.msg_id, "42");
        assert_eq!(c.text, "hello");
        assert_eq!(c.account_id, "acct-tg-1");
    }

    #[test]
    fn caption_used_when_text_absent() {
        let u = sample_update(None, Some("photo caption"));
        let c = update_to_common(
            &u,
            TenantId::default_tenant(),
            "acct".into(),
        )
        .unwrap();
        assert_eq!(c.text, "photo caption");
    }

    #[test]
    fn empty_text_and_caption_returns_none() {
        let u = sample_update(None, None);
        assert!(update_to_common(&u, TenantId::default_tenant(), "x".into()).is_none());
    }

    #[test]
    fn fallback_to_chat_id_when_no_from() {
        let mut u = sample_update(Some("hi"), None);
        u.message.as_mut().unwrap().from = None;
        let c = update_to_common(
            &u,
            TenantId::default_tenant(),
            "x".into(),
        )
        .unwrap();
        // chat.id was 555
        assert_eq!(c.user_id, "555");
    }

    #[test]
    fn updates_without_message_skipped() {
        let u = Update {
            update_id: 1,
            message: None,
        };
        assert!(update_to_common(&u, TenantId::default_tenant(), "x".into()).is_none());
    }

    #[test]
    fn collect_attachments_from_document() {
        let m = Message {
            message_id: 1,
            chat: Chat { id: 1, username: None },
            from: None,
            text: None,
            caption: Some("file caption".into()),
            document: Some(Document {
                file_id: "BAADBAAD-DOC".into(),
                file_name: Some("report.pdf".into()),
                mime_type: Some("application/pdf".into()),
            }),
            voice: None,
            photo: None,
        };
        let atts = m.collect_attachments();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].0, "BAADBAAD-DOC");
        assert_eq!(atts[0].1, Some("report.pdf".into()));
    }

    #[test]
    fn collect_attachments_picks_largest_photo() {
        let m = Message {
            message_id: 1,
            chat: Chat { id: 1, username: None },
            from: None,
            text: None,
            caption: None,
            document: None,
            voice: None,
            photo: Some(vec![
                PhotoSize { file_id: "small".into(), width: Some(80), height: Some(80) },
                PhotoSize { file_id: "big".into(), width: Some(1920), height: Some(1080) },
                PhotoSize { file_id: "mid".into(), width: Some(640), height: Some(480) },
            ]),
        };
        let atts = m.collect_attachments();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].0, "big");
    }

    #[test]
    fn collect_attachments_combines_document_voice_photo() {
        let m = Message {
            message_id: 1,
            chat: Chat { id: 1, username: None },
            from: None,
            text: None,
            caption: None,
            document: Some(Document {
                file_id: "doc-1".into(),
                file_name: None,
                mime_type: None,
            }),
            voice: Some(Voice {
                file_id: "voice-1".into(),
                duration: Some(5),
            }),
            photo: Some(vec![PhotoSize {
                file_id: "ph-1".into(),
                width: Some(100),
                height: Some(100),
            }]),
        };
        let ids: Vec<String> = m.collect_attachments().into_iter().map(|(i, _)| i).collect();
        assert!(ids.contains(&"doc-1".to_string()));
        assert!(ids.contains(&"voice-1".to_string()));
        assert!(ids.contains(&"ph-1".to_string()));
    }

    #[test]
    fn deserializes_real_telegram_payload() {
        let payload = r#"
        {
            "ok": true,
            "result": [
                {
                    "update_id": 8472,
                    "message": {
                        "message_id": 12,
                        "chat": {"id": -100123, "username": null},
                        "from": {"id": 42, "username": "bob"},
                        "text": "hi bot"
                    }
                }
            ]
        }
        "#;
        let resp: GetUpdatesResp = serde_json::from_str(payload).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.result.len(), 1);
        let c = update_to_common(
            &resp.result[0],
            TenantId::default_tenant(),
            "acct".into(),
        )
        .unwrap();
        assert_eq!(c.text, "hi bot");
        assert_eq!(c.user_id, "42");
    }
}
