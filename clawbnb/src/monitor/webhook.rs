//! Outbound webhook dispatch — the highest-priority reply path when
//! `config.webhook.url` is set.
//!
//! POSTs the inbound message + resolved attachments to the operator's
//! webhook endpoint and expects a JSON response `{ "reply": "<text>" }`.
//!
//! ## v2.1.A4 security hardening
//!
//! Before this revision the webhook path had three independent gaps:
//!
//! 1. URL validation only string-matched `localhost` / `127.0.0.1` —
//!    DNS rebinding (`evil.com → 127.0.0.1`) walked right past it.
//!    Now: every host goes through `storage::url_guard::resolve_and_check_public`
//!    which actually resolves DNS and rejects any IP in the private /
//!    link-local / cloud-metadata / CGNAT / IPv6-ULA classes.
//! 2. reqwest client didn't disable redirect, so a 302 from the
//!    user-supplied URL could re-target to an internal IP. Now:
//!    `Policy::none()` (mirrors media::outbound).
//! 3. Payload was unsigned + carried `from_user_id` (raw WeChat id),
//!    host paths (absolute fs paths), and the full message object.
//!    Anyone with the secret-less webhook URL could spoof or replay.
//!    Now: `X-Weclawbot-Signature: t=<unix>,v1=<hex-hmac>` header,
//!    Stripe-style; host paths are dropped from the payload (operators
//!    fetch the raw bytes via the daemon's HTTP API in v3); URL is
//!    masked in daemon log.

use std::time::Duration;

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use tracing::{info, warn};

use crate::api::types::WeixinMessage;
use crate::media::inbound::InboundContent;
use crate::storage::url_guard::{mask_url_for_log, resolve_and_check_public};

/// POST the message to the operator webhook, parse `reply` field. Returns
/// `None` on any error or missing reply (caller falls back to next provider).
pub async fn dispatch(
    url: &str,
    account_id: &str,
    msg: &WeixinMessage,
    content: &InboundContent,
) -> Option<String> {
    // --- URL guard 1: scheme + loopback string match (fast-fail) ---
    if !is_https_or_localhost(url) {
        warn!(
            "webhook url rejected (scheme): {} — must be https:// (or http://localhost / 127.0.0.1)",
            mask_url_for_log(url)
        );
        return None;
    }

    // --- URL guard 2: DNS resolve + IP class check (anti-DNS-rebind) ---
    // Skip for localhost literals — they're loopback by definition.
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return None,
    };
    if let Some(host) = parsed.host_str() {
        // localhost / 127.0.0.1 / ::1 / [::1] 已被 scheme guard 接受为合法
        // dev 场景；这里跳过 IP-class 检查避免误拒 dev。
        let is_explicit_loopback =
            matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]");
        if !is_explicit_loopback {
            let port = parsed.port_or_known_default().unwrap_or(443);
            match resolve_and_check_public(host, port).await {
                Ok(true) => {}
                Ok(false) => {
                    warn!(
                        "webhook url rejected (private/loopback IP after DNS): {}",
                        mask_url_for_log(url)
                    );
                    metrics::counter!(
                        "weclawbot_webhook_calls_total",
                        "status" => "ssrf_blocked"
                    )
                    .increment(1);
                    return None;
                }
                Err(e) => {
                    warn!(
                        "webhook url DNS resolve failed for {}: {e}",
                        mask_url_for_log(url)
                    );
                    metrics::counter!(
                        "weclawbot_webhook_calls_total",
                        "status" => "dns_error"
                    )
                    .increment(1);
                    return None;
                }
            }
        }
    }

    // --- Payload (iLink-specific shim — uses WeixinMessage fields) ---
    let redacted_text = crate::storage::pii::redact(&content.text).into_owned();
    let payload = serde_json::json!({
        "account_id": account_id,
        "platform_id": "ilink-wechat",
        "from": msg.from_user_id,
        "text": redacted_text,
        "attachments": build_attachments_payload(account_id, msg.message_id.unwrap_or(0).to_string().as_str(), content),
        "message_id": msg.message_id,
    });
    send_webhook(url, account_id, payload).await
}

/// v3.1 — protocol-agnostic webhook dispatch driven by [`CommonInbound`].
/// `WebhookProvider` 已经切到这条路径，iLink-only fields (context_token /
/// session_id) 不再出现在 payload 里 —— 多协议接收端拿到一致 schema。
pub async fn dispatch_common(
    url: &str,
    inbound: &crate::monitor::common::CommonInbound,
    content: &InboundContent,
) -> Option<String> {
    // --- URL guard 1: scheme + loopback string match (fast-fail) ---
    if !is_https_or_localhost(url) {
        warn!(
            "webhook url rejected (scheme): {} — must be https:// (or http://localhost / 127.0.0.1)",
            mask_url_for_log(url)
        );
        return None;
    }
    // --- URL guard 2: DNS resolve + IP class check ---
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return None,
    };
    if let Some(host) = parsed.host_str() {
        let is_explicit_loopback =
            matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]");
        if !is_explicit_loopback {
            let port = parsed.port_or_known_default().unwrap_or(443);
            match resolve_and_check_public(host, port).await {
                Ok(true) => {}
                Ok(false) => {
                    warn!(
                        "webhook url rejected (private/loopback IP after DNS): {}",
                        mask_url_for_log(url)
                    );
                    metrics::counter!(
                        "weclawbot_webhook_calls_total",
                        "status" => "ssrf_blocked"
                    )
                    .increment(1);
                    return None;
                }
                Err(e) => {
                    warn!(
                        "webhook url DNS resolve failed for {}: {e}",
                        mask_url_for_log(url)
                    );
                    return None;
                }
            }
        }
    }

    let redacted_text = crate::storage::pii::redact(&content.text).into_owned();
    let payload = serde_json::json!({
        "account_id": inbound.account_id,
        "tenant_id": inbound.tenant_id.as_str(),
        "platform_id": inbound.platform_id,
        "from": inbound.user_id,
        "text": redacted_text,
        "attachments": build_attachments_payload(&inbound.account_id, &inbound.msg_id, content),
        "message_id": inbound.msg_id,
    });
    send_webhook(url, &inbound.account_id, payload).await
}

fn build_attachments_payload(
    account_id: &str,
    msg_id: &str,
    content: &InboundContent,
) -> Vec<Value> {
    // v3-B1: text / embedded_text 过 PII redact 层 —— 防止 operator 接收端
    // 把 webhook payload 写进它自己的 plain log 时泄漏手机/身份证/银行卡。
    content
        .attachments
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let embedded_redacted = a
                .embedded_text
                .as_ref()
                .map(|t| crate::storage::pii::redact(t).into_owned());
            serde_json::json!({
                "token": format!("attach://{}/{}/{}", account_id, msg_id, i),
                "kind": a.kind,
                "name": a.original_name,
                "embedded_text": embedded_redacted,
            })
        })
        .collect()
}

/// Shared sender — payload-builder agnostic. Returns `reply` field from
/// JSON response on 2xx, None otherwise.
async fn send_webhook(url: &str, account_id: &str, payload: Value) -> Option<String> {
    let body = payload.to_string();

    let timestamp = chrono::Utc::now().timestamp();
    let signature_header = build_signature_header(&body, timestamp);

    let started = std::time::Instant::now();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .ok()?;

    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("X-Weclawbot-Account", account_id);
    if let Some(sig) = signature_header.as_deref() {
        req = req.header("X-Weclawbot-Signature", sig);
    }
    let resp = match req.body(body).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("webhook failed ({}): {e}", mask_url_for_log(url));
            metrics::counter!(
                "weclawbot_webhook_calls_total",
                "status" => "network_error"
            )
            .increment(1);
            return None;
        }
    };

    let status = resp.status();
    let body = resp.text().await.ok()?;
    let elapsed = started.elapsed().as_secs_f64();
    info!(
        "webhook -> {} status={status} elapsed={elapsed:.2}s",
        mask_url_for_log(url)
    );
    metrics::counter!(
        "weclawbot_webhook_calls_total",
        "status" => http_status_label(status.as_u16())
    )
    .increment(1);
    metrics::histogram!("weclawbot_webhook_duration_seconds").record(elapsed);

    if !status.is_success() {
        return None;
    }

    let parsed: Value = serde_json::from_str(&body).ok()?;
    parsed
        .get("reply")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// `t=<unix>,v1=<hex-hmac-sha256>` over `<timestamp>.<body>`. Stripe-compatible
/// scheme — receiver should reconstruct `<t>.<body>` and HMAC-verify with
/// the same secret, then reject if |now - t| > acceptable_window.
///
/// Returns None if secret isn't configured — signature is opt-in, but
/// always emitted when secret is set. Operators without secret get the
/// old unsigned behaviour (logged warn at boot in a follow-up).
fn build_signature_header(body: &str, timestamp: i64) -> Option<String> {
    let secret = std::env::var("WECLAWBOT_WEBHOOK_SECRET").ok()?;
    if secret.is_empty() {
        return None;
    }
    type HmacSha256 = Hmac<Sha256>;
    let signed_payload = format!("{timestamp}.{body}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(signed_payload.as_bytes());
    let result = mac.finalize().into_bytes();
    let hex: String = result.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("t={timestamp},v1={hex}"))
}

fn http_status_label(code: u16) -> &'static str {
    match code {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "other",
    }
}

/// Fast string-level scheme check (cheap), before the slower DNS check.
fn is_https_or_localhost(url: &str) -> bool {
    if let Ok(parsed) = url::Url::parse(url) {
        match parsed.scheme() {
            "https" => return true,
            "http" => {
                if let Some(host) = parsed.host_str() {
                    return host == "localhost"
                        || host == "127.0.0.1"
                        || host == "::1"
                        || host == "[::1]";
                }
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::WeixinMessage;
    use crate::media::inbound::InboundContent;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn empty_msg() -> WeixinMessage {
        WeixinMessage {
            from_user_id: Some("u-test".into()),
            ..Default::default()
        }
    }

    fn empty_content() -> InboundContent {
        InboundContent {
            text: "hello".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn returns_reply_on_200_ok() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .and(header("X-Weclawbot-Account", "acct-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"reply":"hi back"}"#))
            .mount(&server)
            .await;
        let out = dispatch(&server.uri(), "acct-1", &empty_msg(), &empty_content()).await;
        assert_eq!(out.as_deref(), Some("hi back"));
    }

    #[tokio::test]
    async fn returns_none_on_500() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let out = dispatch(&server.uri(), "acct-1", &empty_msg(), &empty_content()).await;
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn returns_none_when_reply_is_empty_string() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"reply":""}"#))
            .mount(&server)
            .await;
        let out = dispatch(&server.uri(), "acct-1", &empty_msg(), &empty_content()).await;
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn returns_none_on_connection_refused() {
        let out = dispatch(
            "http://127.0.0.1:1",
            "acct-1",
            &empty_msg(),
            &empty_content(),
        )
        .await;
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn rejects_plain_http_to_remote() {
        let out = dispatch(
            "http://example.com/hook",
            "acct-1",
            &empty_msg(),
            &empty_content(),
        )
        .await;
        assert_eq!(out, None);
    }

    #[test]
    fn is_https_or_localhost_accepts_https() {
        assert!(is_https_or_localhost("https://example.com/hook"));
    }

    #[test]
    fn is_https_or_localhost_accepts_loopback_http() {
        assert!(is_https_or_localhost("http://localhost:8080/x"));
        assert!(is_https_or_localhost("http://127.0.0.1:8080/x"));
        assert!(is_https_or_localhost("http://[::1]:8080/x"));
    }

    #[test]
    fn is_https_or_localhost_rejects_plain_remote() {
        assert!(!is_https_or_localhost("http://example.com/hook"));
        assert!(!is_https_or_localhost("http://10.0.0.1/hook"));
    }

    #[test]
    fn is_https_or_localhost_rejects_other_schemes() {
        assert!(!is_https_or_localhost("ftp://example.com/x"));
        assert!(!is_https_or_localhost("file:///etc/passwd"));
        assert!(!is_https_or_localhost("javascript:alert(1)"));
        assert!(!is_https_or_localhost("not a url at all"));
    }

    #[test]
    fn signature_header_only_when_secret_set() {
        // SAFETY: 单进程测试，独占 env
        unsafe { std::env::remove_var("WECLAWBOT_WEBHOOK_SECRET"); }
        assert!(build_signature_header("{}", 123).is_none());

        unsafe { std::env::set_var("WECLAWBOT_WEBHOOK_SECRET", "test123"); }
        let h = build_signature_header("{\"x\":1}", 100).unwrap();
        assert!(h.starts_with("t=100,v1="));
        assert!(h.len() > 20);
        // 同 body + 同 timestamp + 同 secret 应当确定性
        let h2 = build_signature_header("{\"x\":1}", 100).unwrap();
        assert_eq!(h, h2);
        // body 不同 → signature 不同
        let h3 = build_signature_header("{\"x\":2}", 100).unwrap();
        assert_ne!(h, h3);
        unsafe { std::env::remove_var("WECLAWBOT_WEBHOOK_SECRET"); }
    }
}
