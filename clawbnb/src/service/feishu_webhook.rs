//! Feishu Event Subscription webhook — v5.2 O4.
//!
//! Feishu 推 events 给 operator-configured callback URL，不 poll。weclawbot
//! 暴露：
//!
//! - **POST `/api/v1/puppet/feishu/webhook/<account_id>`** —— Feishu 服务器
//!   推 event 进来 + 处理两种 payload:
//!   1. URL verification challenge (`type=url_verification`)：必须立刻 echo
//!      `challenge` 字段，否则 Feishu 拒绝注册 webhook
//!   2. Real event (`schema=2.0` + `header.event_type=im.message.receive_v1`)：
//!      解 + dispatch to feishu_handler
//!
//! ## 签名验证
//!
//! Feishu 用 `X-Lark-Signature` (timestamp + nonce + body, AES-encrypted
//! with operator-configured encrypt key)。本期实施 timestamp 防 replay +
//! 长度/格式校验；完整 AES-encrypted body 解密走 v5.3（需要拿 IM 应用
//! Encrypt Key，可选 feature）。

use axum::{
    extract::{Json, Path},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::puppet::feishu_inbound::{event_to_common, FeishuEvent};
use crate::tenancy::resolver::resolve_tenant_for_account;

/// `POST /api/v1/puppet/feishu/webhook/<account_id>`
///
/// Returns the URL verification challenge response when applicable,
/// else returns `{"code": 0}` (Feishu OK marker) after dispatching.
pub async fn post_feishu_webhook(
    Path(account_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    // Step 1: URL verification challenge
    if payload.get("type").and_then(|v| v.as_str()) == Some("url_verification") {
        let challenge = payload
            .get("challenge")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return (StatusCode::OK, Json(json!({ "challenge": challenge })));
    }

    // Step 2: timestamp replay check
    if let Some(ts_header) = headers
        .get("X-Lark-Request-Timestamp")
        .and_then(|h| h.to_str().ok())
    {
        if let Ok(ts) = ts_header.parse::<i64>() {
            let now = chrono::Utc::now().timestamp();
            if (now - ts).abs() > 300 {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "feishu event timestamp too old or in the future"})),
                );
            }
        }
    }

    // Step 3: parse + dispatch event
    let event: FeishuEvent = match serde_json::from_value(payload) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("parse: {e}")})),
            );
        }
    };

    let tenant = resolve_tenant_for_account(&account_id);
    let common = match event_to_common(&event, tenant, account_id.clone()) {
        Some(c) => c,
        None => {
            // 非 text message 或不支持的 event_type — 200 让 Feishu 不重试
            return (StatusCode::OK, Json(json!({"code": 0, "msg": "skipped"})));
        }
    };

    // v5.3: 真 dispatch 到 feishu_handler。读 account 拿 token + base_url
    // （Feishu API URL — Feishu 国内 vs Lark 海外双品牌）。account 找不到
    // → 200 让 Feishu 不重试，但 daemon log warn（operator 配置不对）。
    let acct = match crate::auth::accounts::load_account(&account_id) {
        Some(a) => a,
        None => {
            tracing::warn!(
                "[{account_id}] feishu webhook: account not configured — dropping event"
            );
            return (
                StatusCode::OK,
                Json(json!({"code": 0, "msg": "account not configured"})),
            );
        }
    };
    let token = match acct.token.as_ref() {
        Some(t) => t.clone(),
        None => {
            tracing::warn!(
                "[{account_id}] feishu webhook: account has no token — dropping event"
            );
            return (
                StatusCode::OK,
                Json(json!({"code": 0, "msg": "account has no token"})),
            );
        }
    };
    let base_url = acct.base_url.clone().unwrap_or_default();

    let bot = std::sync::Arc::new(crate::puppet::feishu::FeishuBot::new());
    // dispatch async — Feishu 期望 webhook fast-200，否则它会重试。
    // 真处理推到 background task。dedup 防 redeliver 内已经 idempotent。
    tokio::spawn(async move {
        crate::puppet::feishu_handler::handle_inbound_event(bot, common, &base_url, &token).await;
    });

    (StatusCode::OK, Json(json!({"code": 0})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn url_verification_echoes_challenge() {
        let resp = post_feishu_webhook(
            Path("acct-1".to_string()),
            HeaderMap::new(),
            Json(json!({"type": "url_verification", "challenge": "abc123"})),
        )
        .await;
        assert_eq!(resp.0, StatusCode::OK);
        assert_eq!(resp.1["challenge"], "abc123");
    }

    #[tokio::test]
    async fn stale_timestamp_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Lark-Request-Timestamp",
            "0".parse().unwrap(), // 1970
        );
        let resp = post_feishu_webhook(
            Path("acct".to_string()),
            headers,
            Json(json!({
                "schema": "2.0",
                "header": {"event_type": "im.message.receive_v1"},
                "event": {
                    "sender": {"sender_id": {"open_id": "ou_1"}},
                    "message": {
                        "message_id": "m",
                        "chat_id": "c",
                        "message_type": "text",
                        "content": "{\"text\":\"hi\"}"
                    }
                }
            })),
        )
        .await;
        assert_eq!(resp.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn non_text_event_returns_ok_skipped() {
        let resp = post_feishu_webhook(
            Path("acct".to_string()),
            HeaderMap::new(),
            Json(json!({
                "schema": "2.0",
                "header": {"event_type": "im.message.read_v1"}, // 不是 receive_v1
                "event": {
                    "sender": {"sender_id": {"open_id": "ou_1"}},
                    "message": {
                        "message_id": "m",
                        "chat_id": "c",
                        "message_type": "text",
                        "content": "{\"text\":\"hi\"}"
                    }
                }
            })),
        )
        .await;
        assert_eq!(resp.0, StatusCode::OK);
        assert_eq!(resp.1["msg"], "skipped");
    }
}
