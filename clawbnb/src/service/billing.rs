//! Stripe billing webhook — v3.3 F2.
//!
//! ## 接入流程
//!
//! 1. Operator 在 Stripe dashboard 配 webhook endpoint：
//!    `https://daemon.example/api/v1/billing/stripe-webhook`
//! 2. Stripe 把 webhook signing secret 给 operator
//! 3. Operator 写进环境变量 `WECLAWBOT_STRIPE_WEBHOOK_SECRET=whsec_...`
//! 4. Stripe 每次 event POST 带 `Stripe-Signature: t=<unix>,v1=<hex>` header
//! 5. 我们用 HMAC-SHA256(secret, "<t>.<body>") 验证
//!
//! ## 处理的 event 类型
//!
//! - `invoice.paid` — 标记 tenant 当前 billing period 已付款
//! - `invoice.payment_failed` — 标记 tenant pending suspension
//! - `customer.subscription.deleted` — 标记 tenant suspended（不再处理 inbound）
//! - 其他 event 一律 200 但不做实际处理（webhook 可重放，保持幂等）
//!
//! ## 不在本期做的
//!
//! - **真改 tenants.status**：当前只 log + audit。actual suspension hook
//!   需要 tenants 表加 `billing_status / next_billing_date` 列（V0007）。
//!   v3.4 PR 跟 GUI billing tab 一起做。
//!
//! - **dunning emails**：发提醒邮件超出 daemon 范围 —— operator 应配
//!   Stripe Smart Retries + Stripe-side dunning emails，不在 daemon 重做。

use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
    Json,
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;

const STRIPE_SIGNATURE_HEADER: &str = "stripe-signature";

/// Maximum age of a Stripe event we'll accept. Stripe recommends 5 min
/// to balance replay defence vs receiver clock skew.
const MAX_TIMESTAMP_AGE_SECS: i64 = 300;

/// `axum::routing::post("/api/v1/billing/stripe-webhook", post_stripe_webhook)`
pub async fn post_stripe_webhook(
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Step 1: secret 必须配置 — 没配 = 拒所有请求（防止 dev 漏配上 prod）
    let secret = match std::env::var("WECLAWBOT_STRIPE_WEBHOOK_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "stripe webhook secret not configured"})),
            ));
        }
    };

    // Step 2: signature header 解析
    let sig_header = headers
        .get(STRIPE_SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing Stripe-Signature header"})),
        ))?;
    let parts = parse_stripe_signature(sig_header).ok_or((
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "malformed Stripe-Signature header"})),
    ))?;

    // Step 3: timestamp tolerance check
    let now = chrono::Utc::now().timestamp();
    if (now - parts.timestamp).abs() > MAX_TIMESTAMP_AGE_SECS {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "stripe event timestamp too old or in the future"})),
        ));
    }

    // Step 4: HMAC verify
    let signed_payload = format!("{}.{}", parts.timestamp, std::str::from_utf8(&body).unwrap_or(""));
    if !verify_hmac(&signed_payload, &parts.v1_hex, &secret) {
        metrics::counter!("weclawbot_stripe_webhook_total", "result" => "bad_signature").increment(1);
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "signature mismatch"})),
        ));
    }

    // Step 5: parse event
    let event: StripeEvent = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(e) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("event parse: {e}")})),
            ));
        }
    };

    // Step 6: route by event type
    handle_event(&event).await;
    metrics::counter!(
        "weclawbot_stripe_webhook_total",
        "event_type" => event.event_type.clone(),
        "result" => "ok"
    )
    .increment(1);
    Ok(Json(json!({"received": true, "event_id": event.id})))
}

#[derive(Debug, Deserialize)]
struct StripeEvent {
    id: String,
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    data: Value,
}

#[derive(Debug)]
struct StripeSignatureParts {
    timestamp: i64,
    v1_hex: String,
}

/// `t=1614265200,v1=abcdef...` — Stripe spec allows multiple v1 entries
/// (rotation) — we only check the first matching one which is the most
/// common shape.
fn parse_stripe_signature(header: &str) -> Option<StripeSignatureParts> {
    let mut timestamp: Option<i64> = None;
    let mut v1_hex: Option<String> = None;
    for pair in header.split(',') {
        let mut iter = pair.trim().splitn(2, '=');
        let k = iter.next()?.trim();
        let v = iter.next()?.trim();
        match k {
            "t" => timestamp = v.parse().ok(),
            "v1" => {
                if v1_hex.is_none() {
                    v1_hex = Some(v.to_string());
                }
            }
            _ => {}
        }
    }
    Some(StripeSignatureParts {
        timestamp: timestamp?,
        v1_hex: v1_hex?,
    })
}

fn verify_hmac(signed_payload: &str, expected_hex: &str, secret: &str) -> bool {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(signed_payload.as_bytes());
    let expected_bytes = match hex::decode(expected_hex) {
        Ok(b) => b,
        Err(_) => return false,
    };
    mac.verify_slice(&expected_bytes).is_ok()
}

async fn handle_event(event: &StripeEvent) {
    // v3.4 G2: 真改 tenant billing_status。从 event.data.object.customer
    // 拿 stripe_customer_id，反查 tenants 表对应行。
    let customer_id = event
        .data
        .get("object")
        .and_then(|o| o.get("customer"))
        .and_then(|c| c.as_str());

    let new_status = match event.event_type.as_str() {
        "invoice.paid" => Some("active"),
        "invoice.payment_failed" => Some("pending"),
        "customer.subscription.deleted" => Some("suspended"),
        _ => None,
    };

    let Some(status) = new_status else {
        tracing::debug!(event_type = %event.event_type, "stripe event ignored (no status mapping)");
        return;
    };
    let Some(cust) = customer_id else {
        tracing::warn!(
            event_id = %event.id,
            event_type = %event.event_type,
            "stripe event missing data.object.customer"
        );
        return;
    };

    // 推到 spawn_blocking — repo IO 是 sync r2d2。
    use crate::repo::tenants_async::SqlxTenantRepo;
    let event_id = event.id.clone();
    let event_type = event.event_type.clone();
    let cust = cust.to_string();
    let Some(pool) = crate::storage::db_async::try_global_async_pool() else {
        tracing::warn!("stripe event {event_id}: DB pool unavailable");
        return;
    };
    let repo = SqlxTenantRepo::new(pool);
    let tenant = match repo.find_by_stripe_customer(&cust).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            tracing::warn!(
                "stripe event {event_id} for customer {cust}: no matching tenant"
            );
            return;
        }
        Err(e) => {
            tracing::error!("stripe tenant lookup: {e}");
            return;
        }
    };
    let tid = crate::tenancy::TenantId::new(tenant.id.clone());
    match repo.update_billing_status(&tid, status, &event_type).await {
        Ok(true) => {
            tracing::info!(
                tenant = %tenant.id,
                billing_status = %status,
                event_type = %event_type,
                "tenant billing status updated"
            );
        }
        Ok(false) => {
            tracing::warn!("update billing_status returned 0 rows for {}", tenant.id);
        }
        Err(e) => {
            tracing::error!("update billing_status for {}: {e}", tenant.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::Mac;

    fn hmac_hex(payload: &str, secret: &str) -> String {
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn parse_signature_basic() {
        let p = parse_stripe_signature("t=1614265200,v1=abcdef").unwrap();
        assert_eq!(p.timestamp, 1614265200);
        assert_eq!(p.v1_hex, "abcdef");
    }

    #[test]
    fn parse_signature_with_v0_legacy() {
        // Stripe doc 里有 v0 (test mode) 和 v1 — 我们只用 v1
        let p = parse_stripe_signature("t=100,v0=deadbeef,v1=cafebabe").unwrap();
        assert_eq!(p.v1_hex, "cafebabe");
    }

    #[test]
    fn malformed_header_returns_none() {
        assert!(parse_stripe_signature("garbage").is_none());
        assert!(parse_stripe_signature("t=100").is_none()); // missing v1
        assert!(parse_stripe_signature("v1=abc").is_none()); // missing t
    }

    #[test]
    fn verify_hmac_round_trip() {
        let secret = "whsec_test";
        let body = "1614265200.{\"id\":\"evt_1\",\"type\":\"invoice.paid\"}";
        let sig = hmac_hex(body, secret);
        assert!(verify_hmac(body, &sig, secret));
        // wrong secret fails
        assert!(!verify_hmac(body, &sig, "wrong"));
        // tampered payload fails
        assert!(!verify_hmac(
            "1614265200.{\"id\":\"evt_2\",\"type\":\"invoice.paid\"}",
            &sig,
            secret
        ));
    }

    #[test]
    fn verify_hmac_rejects_bad_hex() {
        assert!(!verify_hmac("anything", "not-hex-zzz", "secret"));
    }

    /// v7.5 — full end-to-end fixture: build a signed Stripe webhook
    /// payload, call the actual `post_stripe_webhook` handler, assert
    /// 200 + verify signature path. Doesn't exercise the
    /// tenant-billing-status update (would need a seeded tenants row
    /// + DB pool); that's covered by the dedicated tenants_async
    /// tests. Here we confirm the wire-level handler accepts a
    /// well-formed signed payload.
    #[tokio::test]
    async fn handler_accepts_well_formed_signed_payload() {
        use axum::body::Bytes;
        use axum::http::HeaderMap;

        let secret = "whsec_e2e_test_secret";
        // SAFETY: env mutation in test. Tests in this module run
        // serialized by cargo's default behavior within a binary.
        unsafe {
            std::env::set_var("WECLAWBOT_STRIPE_WEBHOOK_SECRET", secret);
        }

        let ts = chrono::Utc::now().timestamp();
        // event_type without status mapping → handler ignores after
        // signature verify; we don't need a tenants row.
        let body = r#"{"id":"evt_e2e","type":"ping","data":{"object":{}}}"#;
        let signed_payload = format!("{ts}.{body}");
        let sig = hmac_hex(&signed_payload, secret);

        let mut headers = HeaderMap::new();
        headers.insert(
            STRIPE_SIGNATURE_HEADER,
            format!("t={ts},v1={sig}").parse().unwrap(),
        );

        let result =
            post_stripe_webhook(headers, Bytes::from(body.to_string())).await;
        assert!(
            result.is_ok(),
            "expected handler to accept signed payload: {result:?}"
        );

        // Negative: tampered payload → 401 unauthorized
        let mut bad_headers = HeaderMap::new();
        bad_headers.insert(
            STRIPE_SIGNATURE_HEADER,
            format!("t={ts},v1=00000000").parse().unwrap(),
        );
        let bad =
            post_stripe_webhook(bad_headers, Bytes::from(body.to_string())).await;
        assert!(bad.is_err(), "expected handler to reject bad signature");
    }
}
