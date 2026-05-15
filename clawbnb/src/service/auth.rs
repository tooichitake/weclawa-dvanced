//! Bearer-token authentication + RBAC middleware for `/api/v1/...`.
//!
//! ## Auth flow
//!
//! 1. Caller sends `Authorization: Bearer weclawbot_<prefix>_<secret>`.
//! 2. `bearer_auth` middleware extracts the key, calls
//!    `admin_key::verify_and_load`, attaches the resulting
//!    `AdminContext` to the request extensions, then forwards.
//! 3. Per-route RBAC checks (`require_role(min)`) read the context and
//!    reject 403 if the role is insufficient.
//!
//! ## Bypass paths
//!
//! Some endpoints **must** be reachable without a key:
//! - `GET /healthz` — load balancers / k8s probes
//! - `GET /metrics` — Prometheus scraper (Phase 4)
//! - `POST /api/v1/auth/login` — GUI exchanges a key for a session cookie
//!
//! These are wired in `service::server` at the route level — they
//! simply don't go through `bearer_auth` at all. Everything else does.

use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
    Json,
};
use serde_json::json;

use crate::auth::admin_key::verify_and_load_async;
#[cfg(test)]
use crate::auth::admin_key::AdminContext;

/// Middleware: require a valid `Authorization: Bearer ...` header, attach
/// the resolved `AdminContext` to request extensions.
pub async fn bearer_auth(mut req: Request<Body>, next: Next) -> Result<Response, Response> {
    let header_val = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string());

    let Some(plaintext) = header_val else {
        return Err(unauthorized("missing Authorization: Bearer header"));
    };

    // v4.2: verify_and_load_async — IO 用 sqlx async + argon2 CPU 部分仍
    // 通过 spawn_blocking 推到 blocking pool（在 verify_and_load_async
    // 内部）。
    let ctx = match verify_and_load_async(&plaintext).await {
        Ok(Some(c)) => c,
        Ok(None) => return Err(unauthorized("invalid or revoked admin key")),
        Err(e) => return Err(internal(&format!("auth: {e}"))),
    };

    // v3-C1: 同时注入 TenantId — downstream repo / app service 通过
    // request extension 拿到当前 tenant，不再各自硬编码 DEFAULT_TENANT。
    // v2.2 数据下所有 key 仍归 'default'；v3 hosted 多租户上线时 bootstrap
    // 路径会让操作员 mint per-tenant key，此处自然分流。
    let tenant_id = crate::tenancy::TenantId::new(&ctx.tenant_id);
    req.extensions_mut().insert(ctx);
    req.extensions_mut().insert(tenant_id);
    Ok(next.run(req).await)
}

// v7.0 housekeeping: `require_role` factory + `forbidden` helper removed.
// `service::admin` hand-rolls its own per-route role check (see comment
// at admin.rs:10 — axum's nested-router type erasure made the layer
// factory clumsy). When we get a second route that needs RBAC, revive
// the factory then; until then it was zero-caller dead weight.
//
// `Role` is still re-exported from `crate::repo::admin_keys` for the
// hand-rolled checks.

fn unauthorized(detail: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"weclawbot\"")],
        Json(json!({
            "type": "https://weclawbot.dev/errors/unauthorized",
            "title": "Unauthorized",
            "status": 401,
            "detail": detail,
        })),
    )
        .into_response()
}

fn internal(detail: &str) -> Response {
    tracing::error!("{detail}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "type": "https://weclawbot.dev/errors/internal",
            "title": "Internal error",
            "status": 500,
            "detail": detail,
        })),
    )
        .into_response()
}

use axum::response::IntoResponse;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    use crate::storage::db_async as db;

    async fn drain(resp: Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    fn handler_ok() -> Router {
        Router::new()
            .route(
                "/echo-role",
                get(|req: Request<Body>| async move {
                    let role = req
                        .extensions()
                        .get::<AdminContext>()
                        .map(|c| format!("{:?}", c.role))
                        .unwrap_or_else(|| "missing".into());
                    Json(json!({"role": role}))
                }),
            )
            .layer(axum::middleware::from_fn(bearer_auth))
    }

    #[tokio::test]
    async fn missing_header_is_401() {
        let pool = db::open_in_memory().await.unwrap();
        db::set_global_async_pool(pool);
        let app = handler_ok();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/echo-role")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = drain(resp).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["status"], 401);
    }

    // NOTE: The full "valid key passes" / "revoked key fails" path uses
    // `set_global_pool` which is a process-wide singleton (OnceLock). It
    // can only be set once per test binary, which makes additional
    // middleware tests here fragile. Those flows are already covered by
    // the unit tests in `auth::admin_key`; this module focuses on the
    // axum integration shape (header parsing, response formatting).

    // v7.0 housekeeping: the `require_role_factory_returns_callable`
    // smoke test was removed alongside the `require_role` factory.
}
