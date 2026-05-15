//! Operator-level operations exposed via the GUI (Phase 7 follow-up).
//!
//! These routes are about "things the human running this daemon would
//! otherwise do on the host shell" — installing claude plugins, deleting
//! WeChat accounts, etc. Each is a thin wrapper over an existing
//! filesystem / repo primitive plus appropriate audit logging.

use std::fs;
use std::path::PathBuf;

use axum::{
    extract::{Path, Request},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::admin_key::AdminContext;
use crate::ids::AccountId;
use crate::repo::accounts_async::SqlxAccountRepo;
use crate::repo::admin_keys::Role;
use crate::repo::audit::AuditInput;
use crate::repo::audit_async::SqlxAuditRepo;
use crate::storage::atomic_write::write_json_atomic;
use crate::storage::db_async;

// ============================================================================
// Operator-installed claude plugins  (~/.claude/settings.json `enabledPlugins`)
// ============================================================================
//
// claude-cli stores its plugin enable-list in the operator's
// `~/.claude/settings.json` under `enabledPlugins`. The actual plugin
// code lives in `~/.claude/plugins/cache/` and is fetched on demand
// when claude starts. We can manipulate the enable-list as declarative
// data — claude reconciles cache vs enable-list at next launch.

fn operator_claude_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

fn read_operator_settings() -> Result<Value, String> {
    let path = operator_claude_settings_path().ok_or("no home dir")?;
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn write_operator_settings(value: &Value) -> Result<(), String> {
    let path = operator_claude_settings_path().ok_or("no home dir")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    write_json_atomic(&path, value)
}

/// GET /api/v1/operator/plugins
pub async fn list_operator_plugins(
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let _ = ctx_of(&req)?;
    let settings = read_operator_settings().map_err(internal_str)?;
    let plugins = settings
        .get("enabledPlugins")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    Ok(Json(json!({ "enabledPlugins": plugins })))
}

#[derive(Debug, Deserialize)]
pub struct PluginSpecBody {
    /// `name@marketplace`, e.g. `pptx@anthropic-agent-skills`.
    pub spec: String,
}

/// POST /api/v1/operator/plugins  body `{spec: "name@marketplace"}`
pub async fn install_operator_plugin(
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = ctx_of(&req)?;
    require(&ctx, Role::ReadWrite)?;
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 16 * 1024)
        .await
        .map_err(|e| bad_request(&format!("read body: {e}")))?;
    let body: PluginSpecBody = serde_json::from_slice(&bytes)
        .map_err(|e| bad_request(&format!("parse body: {e}")))?;
    let spec = body.spec.trim().to_string();
    if !spec.contains('@') || spec.starts_with('@') || spec.ends_with('@') {
        return Err(bad_request("spec must be of form name@marketplace"));
    }

    let mut settings = read_operator_settings().map_err(internal_str)?;
    let obj = settings
        .as_object_mut()
        .ok_or_else(|| internal_str("settings.json is not an object".to_string()))?;
    let arr = obj
        .entry("enabledPlugins".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let arr = arr
        .as_array_mut()
        .ok_or_else(|| internal_str("enabledPlugins is not an array".to_string()))?;
    // Append iff not present. Tolerant to both string and object shapes.
    let already = arr.iter().any(|v| match v {
        Value::String(s) => s == &spec,
        Value::Object(o) => o.get("plugin").and_then(|x| x.as_str()) == Some(&spec),
        _ => false,
    });
    if already {
        return Ok(Json(json!({"ok": true, "already": true, "spec": spec })));
    }
    arr.push(Value::String(spec.clone()));
    write_operator_settings(&settings).map_err(internal_str)?;

    audit(&ctx, &parts.headers, "operator.plugin.install", Some(&spec), None, Some(&json!({"spec": spec}))).await;
    Ok(Json(json!({
        "ok": true,
        "spec": spec,
        "note": "插件已加入 enabledPlugins。claude 下次启动时会自动从 marketplace 拉取代码。",
    })))
}

/// DELETE /api/v1/operator/plugins/{spec}
pub async fn uninstall_operator_plugin(
    Path(spec): Path<String>,
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = ctx_of(&req)?;
    require(&ctx, Role::ReadWrite)?;
    let mut settings = read_operator_settings().map_err(internal_str)?;
    let Some(arr) = settings.get_mut("enabledPlugins").and_then(|v| v.as_array_mut()) else {
        return Ok(Json(json!({"ok": false, "error": "no enabledPlugins list" })));
    };
    let before_len = arr.len();
    arr.retain(|v| match v {
        Value::String(s) => s != &spec,
        Value::Object(o) => o.get("plugin").and_then(|x| x.as_str()) != Some(&spec),
        _ => true,
    });
    let removed = before_len > arr.len();
    if removed {
        write_operator_settings(&settings).map_err(internal_str)?;
        audit(&ctx, req.headers(), "operator.plugin.uninstall", Some(&spec), Some(&json!({"spec": spec})), None).await;
    }
    Ok(Json(json!({"ok": removed, "spec": spec, "removed": removed})))
}

// ============================================================================
// WeChat account: delete
// ============================================================================

/// DELETE /api/v1/accounts/{id}  — drops the row + clears related state.
pub async fn delete_account(
    Path(id): Path<String>,
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = ctx_of(&req)?;
    require(&ctx, Role::ReadWrite)?;
    let pool = db_async::try_global_async_pool()
        .ok_or_else(|| internal_str("async pool not initialized".to_string()))?;
    let repo = SqlxAccountRepo::new(pool);
    let aid = AccountId::new(id.clone());
    let before = repo.get(&aid).await.map_err(internal_db)?;
    if before.is_none() {
        return Ok(Json(json!({"ok": false, "error": "no such account"})));
    }
    let ok = repo.delete(&aid).await.map_err(internal_db)?;
    audit(
        &ctx,
        req.headers(),
        "accounts.delete",
        Some(&id),
        Some(&json!({"account_id": id, "had_token": before.as_ref().and_then(|a| a.token.as_ref()).is_some()})),
        None,
    )
    .await;
    Ok(Json(json!({"ok": ok, "account_id": id })))
}

// ============================================================================
// helpers
// ============================================================================

fn ctx_of(req: &Request) -> Result<AdminContext, (StatusCode, Json<Value>)> {
    req.extensions()
        .get::<AdminContext>()
        .cloned()
        .ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "auth middleware did not attach AdminContext"})),
        ))
}

fn require(ctx: &AdminContext, min: Role) -> Result<(), (StatusCode, Json<Value>)> {
    if ctx.role.allows(min) {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            Json(json!({
                "type": "https://weclawbot.dev/errors/forbidden",
                "title": "Forbidden",
                "status": 403,
                "detail": format!("role {:?} insufficient (need {:?})", ctx.role, min),
            })),
        ))
    }
}

fn bad_request(detail: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "type": "https://weclawbot.dev/errors/bad-request",
            "title": "Bad request",
            "status": 400,
            "detail": detail,
        })),
    )
}

fn internal_db(e: crate::storage::db::DbError) -> (StatusCode, Json<Value>) {
    internal_str(e.to_string())
}

fn internal_str(detail: impl Into<String>) -> (StatusCode, Json<Value>) {
    let d = detail.into();
    tracing::error!("operator api: {d}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "type": "https://weclawbot.dev/errors/internal",
            "title": "Internal error",
            "status": 500,
            "detail": d,
        })),
    )
}

async fn audit(
    ctx: &AdminContext,
    headers: &axum::http::HeaderMap,
    action: &str,
    target: Option<&str>,
    before: Option<&Value>,
    after: Option<&Value>,
) {
    let Some(pool) = db_async::try_global_async_pool() else { return };
    let r = SqlxAuditRepo::new(pool);
    let ip = headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string());
    let _ = r
        .record(AuditInput {
            actor_key_id: Some(&ctx.key_id),
            action,
            target,
            before,
            after,
            ip: ip.as_deref(),
        })
        .await;
}
