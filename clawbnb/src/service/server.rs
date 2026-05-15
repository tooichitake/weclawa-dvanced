//! Axum HTTP surface for the admin/console API.
//!
//! ## Route layout (Phase 3)
//!
//! - `GET /` — admin GUI entry
//! - `GET /healthz` — deep liveness check, **no auth**
//! - `GET /api/health` — legacy alias for the GUI, **no auth**
//! - `/api/v1/...` — versioned API, **requires Bearer auth**
//! - `/api/...` (unversioned) — kept for one release as a back-compat
//!   shim. Routes match the v1 paths byte-for-byte and require auth too.
//!
//! Auth uses `service::auth::bearer_auth`. Operators receive an
//! `INITIAL-ADMIN-KEY` on first boot (see `auth::admin_key::ensure_bootstrap_super_admin`).
//! Per-route RBAC layers are deferred to a follow-up patch — Phase 3
//! ships baseline auth (key must be valid + not revoked) so every
//! existing endpoint is at minimum protected.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::Router;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::info;

use super::auth::bearer_auth;
use super::routes;

/// Phase 4: API 请求计数 + 延迟直方图 middleware。每个 HTTP 请求一进
/// 一出都打一行 metric。`path_template` 用 `req.uri().path()` 的原始
/// 形态（带 `{hash}` 这种 placeholder 不展开），所以 cardinality 不爆。
async fn metrics_middleware(req: Request<Body>, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = std::time::Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16();
    let latency_s = started.elapsed().as_secs_f64();
    // 把高 cardinality 的 user-hash 等动态段折叠成 placeholder
    let path_template = collapse_path(&path);
    metrics::counter!(
        "weclawbot_api_requests_total",
        "method" => method.to_string(),
        "path" => path_template.clone(),
        "status" => status.to_string()
    )
    .increment(1);
    metrics::histogram!(
        "weclawbot_api_request_duration_seconds",
        "method" => method.to_string(),
        "path" => path_template
    )
    .record(latency_s);
    resp
}

/// 把动态段 (user hash / key id / qrcode token 等) 折叠掉，防止
/// Prometheus 标签爆炸 — 每个 path 对应一个固定模板。
fn collapse_path(p: &str) -> String {
    let segs: Vec<&str> = p.split('/').collect();
    let mut out: Vec<String> = Vec::with_capacity(segs.len());
    for s in segs {
        if s.starts_with("u-") && s.len() >= 8 {
            out.push("{hash}".into());
        } else if s.len() == 36 && s.matches('-').count() == 4 {
            out.push("{uuid}".into()); // admin key id
        } else if s.len() >= 24 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            out.push("{hex}".into()); // QR code keys
        } else {
            out.push(s.to_string());
        }
    }
    out.join("/")
}

/// Cap inbound request bodies at 2 MiB. settings.json edits / inject payloads
/// rarely exceed a few hundred KiB; anything bigger is either malformed or a
/// DoS attempt (e.g. send 10 GB of JSON to /api/test/inject and watch the
/// daemon OOM). Per Phase 0.4 of the v2 hardening plan.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Build the protected route subtree (all `/api/v1/...` endpoints).
/// Pulled out as a function so we can mount it twice — once at
/// `/api/v1/*` (canonical) and once at `/api/*` (back-compat shim that
/// goes away after one release).
fn protected_routes() -> Router {
    Router::new()
        .route("/accounts", get(routes::get_accounts))
        .route("/accounts/{id}/relogin", post(routes::post_relogin))
        .route("/accounts/link-agent", post(routes::post_link_agent))
        .route("/qr/create", post(routes::post_qr_create))
        .route("/qr/{key}/status", get(routes::get_qr_status))
        .route("/gateway/restart", post(routes::post_gateway_restart))
        .route("/errors", get(routes::get_errors))
        .route("/defaults", get(routes::get_defaults))
        .route("/defaults", put(routes::put_defaults))
        .route("/defaults/apply", post(routes::post_defaults_apply))
        .route("/users", get(routes::get_users))
        .route("/users/{hash}/settings", get(routes::get_user_settings))
        .route("/users/{hash}/settings", put(routes::put_user_settings))
        .route("/users/{hash}/history", get(routes::get_user_history))
        .route("/users/{hash}/history", delete(routes::delete_user_history))
        .route("/users/{hash}/sync", post(routes::post_user_sync))
        .route("/users/{hash}", delete(routes::delete_user))
        .route("/claude/schema", get(routes::get_claude_schema))
        // Test injection is protected by Bearer auth on top of the
        // env-var gate baked into the handler.
        .route("/test/inject", post(routes::post_test_inject))
        .route("/test/capture", get(routes::get_test_capture))
        // Phase 3/7: admin self-introspection + key management.
        .route("/auth/me", get(super::admin::get_auth_me))
        .route("/admin/keys", get(super::admin::list_admin_keys))
        .route("/admin/keys", post(super::admin::create_admin_key))
        .route("/admin/keys/{id}", delete(super::admin::revoke_admin_key))
        .route("/admin/backup", post(super::admin::trigger_backup))
        // v5.2 O5: multi-tenant admin
        .route("/tenants", get(super::admin::list_tenants))
        // Phase 3: audit log read.
        .route("/audit", get(super::admin::list_audit))
        // Phase 7 follow-up: operator-level UI actions.
        .route("/operator/plugins", get(super::operator::list_operator_plugins))
        .route("/operator/plugins", post(super::operator::install_operator_plugin))
        .route("/operator/plugins/{spec}", delete(super::operator::uninstall_operator_plugin))
        .route("/accounts/{id}", delete(super::operator::delete_account))
        // Live config + diagnostics
        .route("/config", get(super::sysconfig::get_config))
        .route("/config", put(super::sysconfig::put_config))
        .route("/logs", get(super::sysconfig::get_logs))
        .route("/sandboxes", get(super::sysconfig::get_sandboxes))
        .layer(middleware::from_fn(bearer_auth))
}

pub async fn run_server(bind: String, port: u16) -> Result<(), String> {
    let bind = &bind;
    let app = Router::new()
        .route("/", get(routes::get_root))
        // v5.4 L5.1: split console assets (app.css / app.js / future
        // ESM modules). embed-or-override served by `service::page`.
        // Unauthenticated since assets are static UI shell; all API
        // calls from app.js still go through bearer middleware.
        .route("/console/{*path}", get(super::page::serve_asset))
        // Unauthenticated probes.
        .route("/api/health", get(routes::get_health))
        .route("/healthz", get(crate::observability::healthz::healthz))
        .route("/metrics", get(metrics_endpoint))
        // v3.4 G2: Stripe webhook unauthenticated (HMAC verify in-handler).
        .route(
            "/api/v1/billing/stripe-webhook",
            post(super::billing::post_stripe_webhook),
        )
        // v5.2 O4: Feishu Event Subscription webhook (challenge + event dispatch).
        // Unauthenticated — operator-side encrypt key signature verify in handler.
        .route(
            "/api/v1/puppet/feishu/webhook/{account_id}",
            post(super::feishu_webhook::post_feishu_webhook),
        )
        // Versioned + back-compat aliased API surfaces. Both go through
        // the same protected subtree — old clients keep working while
        // the GUI migrates to /api/v1/*.
        .nest("/api/v1", protected_routes())
        .nest("/api", protected_routes())
        .fallback(routes::fallback)
        .layer(middleware::from_fn(metrics_middleware))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES));

    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .map_err(|e| format!("invalid address: {e}"))?;

    info!("Console listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind: {e}"))?;

    axum::serve(listener, app)
        .await
        .map_err(|e| format!("serve: {e}"))
}

/// Prometheus scrape endpoint. Returns plaintext exposition format
/// (Content-Type `text/plain; version=0.0.4`) so a stock scraper picks
/// it up without negotiation. 503 if the recorder wasn't installed
/// (boot ordering bug — never expected in prod).
async fn metrics_endpoint() -> axum::response::Response {
    use axum::http::header;
    // Check the install state via the typed handle, not via "is the
    // rendered body empty" — an installed recorder can legitimately
    // render an empty body if no metric has been emitted yet (e.g.
    // right after boot, before the first request).
    if crate::observability::metrics::handle().is_none() {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "metrics recorder not installed",
        )
            .into_response();
    }
    let body = crate::observability::metrics::render();
    (
        axum::http::StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

use axum::response::IntoResponse;
