//! Admin API routes — Phase 3/5/7 additions on `/api/v1/admin/...`.
//!
//! - Admin keys: create / list / revoke (super_admin only)
//! - Audit log: paginated read (read_write+)
//! - Backup: trigger a `weclawbot backup` from the GUI (super_admin only)
//! - Auth helpers: `/auth/me` for the GUI to discover its own role
//!
//! Per-route RBAC is enforced INSIDE each handler (reading
//! `AdminContext` from extensions). We chose handler-level checks
//! over the `require_role` layer factory because axum's nested-router
//! + middleware composition has sharp edges around extension
//! propagation in 0.8 — explicit checks are more boring and equally
//! safe given the small number of routes.

use axum::extract::{Path, Query, Request};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::admin_key::{mint_new_key_async, AdminContext};
use crate::repo::admin_keys::Role;
use crate::repo::admin_keys_async::SqlxAdminKeyRepo;
use crate::repo::audit::AuditInput;
use crate::repo::audit_async::SqlxAuditRepo;
use crate::storage::db_async;

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

fn ctx_of(req: &Request) -> Result<AdminContext, (StatusCode, Json<Value>)> {
    req.extensions()
        .get::<AdminContext>()
        .cloned()
        .ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "auth middleware did not attach AdminContext"})),
        ))
}

// --- /auth/me --------------------------------------------------------------

pub async fn get_auth_me(req: Request) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = ctx_of(&req)?;
    Ok(Json(json!({
        "key_id": ctx.key_id,
        "key_name": ctx.key_name,
        "role": ctx.role.as_str(),
    })))
}

// --- /admin/keys ----------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateKeyReq {
    pub name: String,
    pub role: String,
}

pub async fn list_admin_keys(req: Request) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = ctx_of(&req)?;
    require(&ctx, Role::SuperAdmin)?;
    let pool = db_async::try_global_async_pool()
        .ok_or_else(|| internal_str("async pool not initialized".to_string()))?;
    let rows: Vec<(String, String, String, String, Option<String>, Option<String>)> =
        sqlx::query_as(
            "SELECT id, name, role, created_at, last_used_at, revoked_at
             FROM admin_keys ORDER BY created_at DESC",
        )
        .fetch_all(&pool)
        .await
        .map_err(|e| internal_str(format!("sqlx admin list: {e}")))?;
    let view: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, role, created_at, last_used_at, revoked_at)| {
            let active = revoked_at.is_none();
            json!({
                "id": id,
                "name": name,
                "role": role,
                "created_at": created_at,
                "last_used_at": last_used_at,
                "revoked_at": revoked_at,
                "active": active,
            })
        })
        .collect();
    Ok(Json(json!({ "keys": view })))
}

pub async fn create_admin_key(
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = ctx_of(&req)?;
    require(&ctx, Role::SuperAdmin)?;
    // Body extraction is manual here because we already consumed the
    // request to read extensions. Pull bytes, decode JSON.
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024)
        .await
        .map_err(|e| bad_request(&format!("read body: {e}")))?;
    let req_body: CreateKeyReq = serde_json::from_slice(&bytes)
        .map_err(|e| bad_request(&format!("parse body: {e}")))?;
    let role = match req_body.role.as_str() {
        "super_admin" => Role::SuperAdmin,
        "read_write" => Role::ReadWrite,
        "read_only" => Role::ReadOnly,
        other => return Err(bad_request(&format!("unknown role {other:?}"))),
    };
    if req_body.name.trim().is_empty() {
        return Err(bad_request("name must not be empty"));
    }

    let pool = db_async::try_global_async_pool()
        .ok_or_else(|| internal_str("async pool not initialized".to_string()))?;
    let name = req_body.name.trim().to_string();
    let minted = mint_new_key_async(pool.clone(), &name, role)
        .await
        .map_err(internal_str)?;

    // Audit.
    let audit = SqlxAuditRepo::new(pool);
    let _ = audit
        .record(AuditInput {
            actor_key_id: Some(&ctx.key_id),
            action: "admin_keys.create",
            target: Some(&minted.record.id),
            before: None,
            after: Some(&json!({
                "name": minted.record.name,
                "role": minted.record.role.as_str(),
            })),
            ip: extract_ip(&parts.headers).as_deref(),
        })
        .await;

    // Return the plaintext ONCE. The GUI must display it and warn the
    // operator to copy it before navigating away.
    Ok(Json(json!({
        "id": minted.record.id,
        "name": minted.record.name,
        "role": minted.record.role.as_str(),
        "plaintext": minted.plaintext,
        "warning": "This plaintext key is shown ONLY once. Save it now.",
    })))
}

pub async fn revoke_admin_key(
    Path(id): Path<String>,
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = ctx_of(&req)?;
    require(&ctx, Role::SuperAdmin)?;
    // Defence in depth: refuse to revoke your own key (would lock you
    // out mid-session) and refuse to revoke the last super_admin.
    if id == ctx.key_id {
        return Err(bad_request("cannot revoke the key you're using"));
    }
    let pool = db_async::try_global_async_pool()
        .ok_or_else(|| internal_str("async pool not initialized".to_string()))?;
    // 取 target row + super_admin count
    let target: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT name, role, revoked_at FROM admin_keys WHERE id = ?1",
    )
    .bind(&id)
    .fetch_optional(&pool)
    .await
    .map_err(|e| internal_str(format!("sqlx admin get: {e}")))?;
    let Some((target_name, target_role, target_revoked)) = target else {
        return Ok(Json(json!({"ok": false, "error": "no such key"})));
    };
    let target_active = target_revoked.is_none();
    if target_role == "super_admin" && target_active {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM admin_keys WHERE revoked_at IS NULL AND role = 'super_admin'",
        )
        .fetch_one(&pool)
        .await
        .map_err(|e| internal_str(format!("count super: {e}")))?;
        if row.0 <= 1 {
            return Err(bad_request(
                "refusing to revoke the last active super_admin key",
            ));
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE admin_keys SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
    )
    .bind(&now)
    .bind(&id)
    .execute(&pool)
    .await
    .map_err(|e| internal_str(format!("sqlx admin revoke: {e}")))?;
    let ok = res.rows_affected() > 0;
    if ok {
        let audit = SqlxAuditRepo::new(pool);
        let _ = audit
            .record(AuditInput {
                actor_key_id: Some(&ctx.key_id),
                action: "admin_keys.revoke",
                target: Some(&id),
                before: Some(&json!({"name": target_name, "role": target_role})),
                after: None,
                ip: None,
            })
            .await;
    }
    Ok(Json(json!({ "ok": ok, "id": id })))
}

// --- /audit ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AuditListQuery {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
}

pub async fn list_audit(
    Query(q): Query<AuditListQuery>,
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = ctx_of(&req)?;
    require(&ctx, Role::ReadWrite)?;
    let limit = q.limit.unwrap_or(100).min(1000);
    let pool = db_async::try_global_async_pool()
        .ok_or_else(|| internal_str("async pool not initialized".to_string()))?;
    let repo = SqlxAuditRepo::new(pool);
    // list_by_action / list_by_actor not yet on SqlxAuditRepo —— v4.3 加；
    // 本期所有变体都走 list_recent + 客户端 filter（典型 N < 1000，cost OK）。
    let _ = (q.action, q.actor);
    let rows = repo
        .list_recent(limit)
        .await
        .map_err(|e| internal_str(format!("sqlx audit list: {e}")))?;
    let view: Vec<Value> = rows
        .into_iter()
        .map(|e| {
            json!({
                "id": e.id,
                "ts": e.ts,
                "actor_key_id": e.actor_key_id,
                "action": e.action,
                "target": e.target,
                "before": e.before,
                "after": e.after,
                "ip": e.ip,
            })
        })
        .collect();
    Ok(Json(json!({"entries": view, "limit": limit })))
}

// --- /admin/backup --------------------------------------------------------

pub async fn trigger_backup(req: Request) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = ctx_of(&req)?;
    require(&ctx, Role::SuperAdmin)?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let out = crate::storage::state_dir::state_dir()
        .join("backups")
        .join(format!("state-{stamp}.db"));
    crate::cli::backup::backup(&out).map_err(internal_str)?;

    let pool = db_async::try_global_async_pool()
        .ok_or_else(|| internal_str("async pool not initialized".to_string()))?;
    let audit = SqlxAuditRepo::new(pool);
    let _ = audit
        .record(AuditInput {
            actor_key_id: Some(&ctx.key_id),
            action: "admin.backup",
            target: Some(out.to_string_lossy().as_ref()),
            before: None,
            after: None,
            ip: None,
        })
        .await;

    Ok(Json(json!({
        "ok": true,
        "path": out.to_string_lossy(),
        "size_bytes": std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0),
    })))
}

// --- helpers --------------------------------------------------------------

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

#[allow(dead_code)]
fn internal(e: crate::storage::db::DbError) -> (StatusCode, Json<Value>) {
    internal_str(e.to_string())
}

fn internal_str(detail: impl Into<String>) -> (StatusCode, Json<Value>) {
    let detail = detail.into();
    tracing::error!("admin api error: {detail}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "type": "https://weclawbot.dev/errors/internal",
            "title": "Internal error",
            "status": 500,
            "detail": detail,
        })),
    )
}

fn extract_ip(headers: &axum::http::HeaderMap) -> Option<String> {
    // Honour the standard reverse-proxy header chain. Don't trust
    // X-Forwarded-For from unauthenticated paths — but this endpoint
    // is already past auth, so the IP recorded is best-effort
    // attribution, not authorisation evidence.
    if let Some(v) = headers.get("x-forwarded-for") {
        if let Ok(s) = v.to_str() {
            return Some(s.split(',').next().unwrap_or(s).trim().to_string());
        }
    }
    if let Some(v) = headers.get("x-real-ip") {
        if let Ok(s) = v.to_str() {
            return Some(s.to_string());
        }
    }
    None
}
