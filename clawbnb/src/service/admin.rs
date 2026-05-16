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
    let rows: Vec<(
        uuid::Uuid,
        String,
        String,
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
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
                "id": id.to_string(),
                "name": name,
                "role": role,
                "created_at": crate::storage::ts::format_rfc3339(&created_at),
                "last_used_at": crate::storage::ts::format_rfc3339_opt(&last_used_at),
                "revoked_at": crate::storage::ts::format_rfc3339_opt(&revoked_at),
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
    // v7.0: admin_keys.id is UUID native. Parse incoming string.
    let id_uuid = uuid::Uuid::parse_str(&id)
        .map_err(|_| bad_request("id must be a valid UUID"))?;
    let target: Option<(String, String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT name, role, revoked_at FROM admin_keys WHERE id = $1",
    )
    .bind(id_uuid)
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

    let res = sqlx::query(
        "UPDATE admin_keys SET revoked_at = $1 WHERE id = $2 AND revoked_at IS NULL",
    )
    .bind(chrono::Utc::now())
    .bind(id_uuid)
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

// --- v5.2 O5: tenants + billing list routes -------------------------------

/// `GET /api/v1/tenants` — super_admin lists every tenant + billing status.
pub async fn list_tenants(req: Request) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = ctx_of(&req)?;
    require(&ctx, Role::SuperAdmin)?;
    let pool = db_async::try_global_async_pool()
        .ok_or_else(|| internal_str("async pool not initialized".to_string()))?;
    let rows: Vec<(
        String,
        String,
        chrono::DateTime<chrono::Utc>,
        String,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "SELECT id, name, created_at, status, stripe_customer_id, deleted_at,
                billing_status, billing_period_end, last_billing_event, last_billing_event_at
         FROM tenants ORDER BY created_at ASC",
    )
    .fetch_all(&pool)
    .await
    .map_err(|e| internal_str(format!("sqlx list_tenants: {e}")))?;
    let fmt = crate::storage::ts::format_rfc3339;
    let fmt_opt = crate::storage::ts::format_rfc3339_opt;
    let view: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, created_at, status, customer, deleted_at, bs, period_end, last_event, last_event_at)| {
            let created_at = fmt(&created_at);
            let deleted_at = fmt_opt(&deleted_at);
            let period_end = fmt_opt(&period_end);
            let last_event_at = fmt_opt(&last_event_at);
            json!({
                "id": id,
                "name": name,
                "created_at": created_at,
                "status": status,
                "stripe_customer_id": customer,
                "deleted_at": deleted_at,
                "billing_status": bs,
                "billing_period_end": period_end,
                "last_billing_event": last_event,
                "last_billing_event_at": last_event_at,
            })
        })
        .collect();
    Ok(Json(json!({ "tenants": view })))
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

// v7.2: per-tenant SSO config GET/PUT. super_admin only because SSO
// config writes effectively grant login access to anyone the IdP says
// is a valid user. Audit-logged on every change.
#[cfg(feature = "ee")]
pub async fn get_tenant_sso_config(
    Path(id): Path<String>,
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = req
        .extensions()
        .get::<AdminContext>()
        .ok_or((StatusCode::UNAUTHORIZED, Json(json!({"error": "no auth"}))))?
        .clone();
    require(&ctx, Role::SuperAdmin)?;

    let pool = match db_async::try_global_async_pool() {
        Some(p) => p,
        None => return Err(internal_str("DB pool not initialized")),
    };
    let repo = crate::repo::tenants_async::SqlxTenantRepo::new(pool);
    let tenant = crate::tenancy::TenantId::new(&id);
    let row = repo
        .get_sso_config(&tenant)
        .await
        .map_err(|e| internal_str(format!("get_sso_config: {e}")))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error": "tenant not found"}))))?;
    Ok(Json(json!({
        "tenant": tenant.as_str(),
        "oidc": row.0,
        "saml": row.1,
    })))
}

#[cfg(feature = "ee")]
#[derive(Deserialize)]
pub struct TenantSsoConfigPut {
    /// OIDC config (set to null to clear)
    #[serde(default)]
    pub oidc: Option<Value>,
    /// SAML config (set to null to clear)
    #[serde(default)]
    pub saml: Option<Value>,
}

#[cfg(feature = "ee")]
pub async fn put_tenant_sso_config(
    Path(id): Path<String>,
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = req
        .extensions()
        .get::<AdminContext>()
        .ok_or((StatusCode::UNAUTHORIZED, Json(json!({"error": "no auth"}))))?
        .clone();
    require(&ctx, Role::SuperAdmin)?;

    // Read body (axum extractors don't compose well with Request — pull
    // it manually).
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|e| bad_request(&format!("body: {e}")))?;
    let body: TenantSsoConfigPut = serde_json::from_slice(&body)
        .map_err(|e| bad_request(&format!("json: {e}")))?;

    // Validate before writing — refuse to persist a config that the
    // verify path would later reject (better DX).
    if let Some(oidc) = &body.oidc {
        let parsed: crate::ee::oidc::OidcConfig = serde_json::from_value(oidc.clone())
            .map_err(|e| bad_request(&format!("oidc shape: {e}")))?;
        parsed
            .validate()
            .map_err(|e| bad_request(&format!("oidc validate: {e}")))?;
    }
    if let Some(saml) = &body.saml {
        let parsed: crate::ee::saml::SamlConfig = serde_json::from_value(saml.clone())
            .map_err(|e| bad_request(&format!("saml shape: {e}")))?;
        parsed
            .validate()
            .map_err(|e| bad_request(&format!("saml validate: {e}")))?;
    }

    let pool = match db_async::try_global_async_pool() {
        Some(p) => p,
        None => return Err(internal_str("DB pool not initialized")),
    };
    let repo = crate::repo::tenants_async::SqlxTenantRepo::new(pool.clone());
    let tenant = crate::tenancy::TenantId::new(&id);
    if let Some(_oidc) = &body.oidc {
        repo.set_oidc_config(&tenant, body.oidc.as_ref())
            .await
            .map_err(|e| internal_str(format!("set_oidc_config: {e}")))?;
    }
    if let Some(_saml) = &body.saml {
        repo.set_saml_config(&tenant, body.saml.as_ref())
            .await
            .map_err(|e| internal_str(format!("set_saml_config: {e}")))?;
    }
    // Audit: the JSON bodies may contain secrets (OIDC client_secret),
    // so we redact `after` to just "{set/cleared}".
    let after_summary = json!({
        "oidc_set": body.oidc.is_some(),
        "saml_set": body.saml.is_some(),
    });
    let audit = SqlxAuditRepo::new(pool);
    let _ = audit
        .record(AuditInput {
            actor_key_id: Some(&ctx.key_id),
            action: "admin.tenants.sso_config.put",
            target: Some(tenant.as_str()),
            before: None,
            after: Some(&after_summary),
            ip: None,
        })
        .await;
    Ok(Json(json!({"ok": true, "tenant": tenant.as_str()})))
}

// v7.8 — GUI-supporting endpoints for the SLA dashboard, Trust tier
// viewer, and Billing summary tabs. All super_admin-gated because
// they expose tenant-scoped data that an ordinary user shouldn't see
// for OTHER tenants (a tenant could see their own data via a
// tenant-scoped variant, but that's GUI v3 polish; v7.8 ships the
// operator-facing pages first).

/// `GET /api/v1/admin/sla?tenant={id}&window=24h`
///
/// Returns the most recent rollup rows from `sla_rollup` for the
/// given tenant. Window is a permissive string — accepts "1h" / "24h"
/// / "7d" / "30d" (default 24h). Used by the GUI SLA dashboard tab
/// to render trend charts and current uptime % cards.
#[cfg(feature = "ee")]
#[derive(Deserialize)]
pub struct SlaQuery {
    pub tenant: Option<String>,
    pub window: Option<String>,
}

#[cfg(feature = "ee")]
pub async fn get_sla_rollups(
    Query(q): Query<SlaQuery>,
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = req
        .extensions()
        .get::<AdminContext>()
        .ok_or((StatusCode::UNAUTHORIZED, Json(json!({"error": "no auth"}))))?
        .clone();
    require(&ctx, Role::SuperAdmin)?;

    let tenant = q
        .tenant
        .unwrap_or_else(|| crate::tenancy::DEFAULT_TENANT.to_string());
    let window_secs = parse_window_secs(q.window.as_deref().unwrap_or("24h"));

    let pool = match db_async::try_global_async_pool() {
        Some(p) => p,
        None => return Err(internal_str("DB pool not initialized")),
    };
    let since = chrono::Utc::now() - chrono::Duration::seconds(window_secs);
    // Direct SQL — sla_rollup is unique to this endpoint, no shared
    // repo needs the read shape.
    let rows: Vec<(
        chrono::DateTime<chrono::Utc>,
        i32,
        i32,
        i32,
        f64,
    )> = sqlx::query_as(
        "SELECT window_start, covered_seconds, downtime_seconds,
                latency_p99_ms, error_rate
         FROM sla_rollup
         WHERE tenant_id = $1 AND window_start >= $2
         ORDER BY window_start DESC
         LIMIT 2000",
    )
    .bind(&tenant)
    .bind(since)
    .fetch_all(&pool)
    .await
    .map_err(|e| internal_str(format!("sla query: {e}")))?;

    // Compute summary: average uptime over the window.
    let (total_covered, total_downtime): (i64, i64) =
        rows.iter()
            .fold((0i64, 0i64), |(c, d), (_, cs, ds, _, _)| {
                (c + *cs as i64, d + *ds as i64)
            });
    let avg_uptime = if total_covered > 0 {
        1.0 - (total_downtime as f64 / total_covered as f64)
    } else {
        1.0
    };

    let series: Vec<Value> = rows
        .into_iter()
        .map(|(ts, cs, ds, lat, er)| {
            json!({
                "window_start": crate::storage::ts::format_rfc3339(&ts),
                "covered_seconds": cs,
                "downtime_seconds": ds,
                "latency_p99_ms": lat,
                "error_rate": er,
                "uptime": if cs > 0 { 1.0 - (ds as f64 / cs as f64) } else { 1.0 },
            })
        })
        .collect();

    Ok(Json(json!({
        "tenant": tenant,
        "window_secs": window_secs,
        "summary": {
            "avg_uptime": avg_uptime,
            "windows_count": series.len(),
        },
        "series": series,
    })))
}

fn parse_window_secs(s: &str) -> i64 {
    // "1h" / "24h" / "7d" / "30d" / "Nm" minutes. Numeric prefix +
    // unit suffix. Anything else → fall back to 24h.
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return 24 * 3600;
    }
    let (num_part, unit) = match trimmed.find(|c: char| !c.is_ascii_digit()) {
        Some(idx) => (&trimmed[..idx], &trimmed[idx..]),
        None => (trimmed, "h"),
    };
    let n: i64 = num_part.parse().unwrap_or(24);
    let multiplier: i64 = match unit {
        "m" | "min" => 60,
        "h" | "hr" => 3600,
        "d" | "day" => 86_400,
        _ => 3600,
    };
    (n.saturating_mul(multiplier)).max(60).min(90 * 86_400)
}

/// `GET /api/v1/admin/trust?tenant={id}&limit=N`
///
/// Returns per-user trust snapshots for the given tenant. Used by the
/// GUI Trust tier viewer to show "which users are restricted /
/// quarantined right now + why". Ordered by score ascending so the
/// most-restricted users surface at the top.
#[derive(Deserialize)]
pub struct TrustQuery {
    pub tenant: Option<String>,
    pub limit: Option<u32>,
}

pub async fn get_trust_snapshots(
    Query(q): Query<TrustQuery>,
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = req
        .extensions()
        .get::<AdminContext>()
        .ok_or((StatusCode::UNAUTHORIZED, Json(json!({"error": "no auth"}))))?
        .clone();
    require(&ctx, Role::ReadWrite)?;

    let tenant = q
        .tenant
        .unwrap_or_else(|| crate::tenancy::DEFAULT_TENANT.to_string());
    let limit = q.limit.unwrap_or(200).clamp(1, 1000) as i64;

    let pool = match db_async::try_global_async_pool() {
        Some(p) => p,
        None => return Err(internal_str("DB pool not initialized")),
    };

    // Direct SQL — trust_async::get is per-user; we need per-tenant
    // list. Same single-caller-no-repo-method tradeoff as sla above.
    let rows: Vec<(
        String,
        f64,
        f64,
        f64,
        f64,
        f64,
        String,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        "SELECT user_hash, success_rate, uptime, threat, integrity,
                score, tier, updated_at, tier_since
         FROM user_trust_inputs
         WHERE tenant_id = $1
         ORDER BY score ASC
         LIMIT $2",
    )
    .bind(&tenant)
    .bind(limit)
    .fetch_all(&pool)
    .await
    .map_err(|e| internal_str(format!("trust query: {e}")))?;

    // Roll up tier distribution.
    let mut tier_counts: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    for (_, _, _, _, _, _, tier, _, _) in &rows {
        *tier_counts.entry(tier.clone()).or_insert(0) += 1;
    }

    let users: Vec<Value> = rows
        .into_iter()
        .map(|(hash, sr, up, threat, integrity, score, tier, updated, since)| {
            json!({
                "user_hash": hash,
                "score": score,
                "tier": tier,
                "inputs": {
                    "success_rate": sr,
                    "uptime": up,
                    "threat": threat,
                    "integrity": integrity,
                },
                "updated_at": crate::storage::ts::format_rfc3339(&updated),
                "tier_since": crate::storage::ts::format_rfc3339(&since),
            })
        })
        .collect();

    Ok(Json(json!({
        "tenant": tenant,
        "tier_counts": tier_counts,
        "users": users,
    })))
}

/// `GET /api/v1/admin/billing/usage?tenant={id}`
///
/// Returns cumulative billing counter values for the given tenant by
/// parsing the daemon's own `/metrics` text output. This is the
/// "what would we bill if we cut a Stripe invoice right now" view.
///
/// Tenant=None aggregates across all tenants — useful for a "total
/// platform usage" card on the operator dashboard.
#[derive(Deserialize)]
pub struct BillingUsageQuery {
    pub tenant: Option<String>,
}

pub async fn get_billing_usage(
    Query(q): Query<BillingUsageQuery>,
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = req
        .extensions()
        .get::<AdminContext>()
        .ok_or((StatusCode::UNAUTHORIZED, Json(json!({"error": "no auth"}))))?
        .clone();
    require(&ctx, Role::SuperAdmin)?;

    let want_tenant = q.tenant.as_deref();

    let metrics_text = crate::observability::metrics::render();
    let (inbound, sandbox_seconds, tokens_input, tokens_output) =
        parse_billing_counters(&metrics_text, want_tenant);

    Ok(Json(json!({
        "tenant": want_tenant.unwrap_or("(all)"),
        "inbound_messages_total": inbound,
        "sandbox_seconds_total": sandbox_seconds,
        "ai_tokens_input_total": tokens_input,
        "ai_tokens_output_total": tokens_output,
        // Heads-up for operators that this is a live snapshot — they
        // can compare against Stripe Usage Records / GUI Billing tab
        // to confirm the pusher is in sync.
        "snapshot_at": chrono::Utc::now().to_rfc3339(),
    })))
}

/// Parse the Prometheus text exposition format for the four billing
/// counters, filtering by `tenant_id` label when provided. Sum across
/// all label variants matching the filter.
///
/// Format snippet:
///   weclawbot_billing_inbound_messages_total{tenant_id="default",platform="ilink-wechat"} 42
///
/// We don't pull in `prometheus-parse` for this — the four counter
/// lookups are simple enough to do with line iteration. If we ever
/// add a fifth, factor out a real parser.
fn parse_billing_counters(text: &str, tenant_filter: Option<&str>) -> (u64, u64, u64, u64) {
    let mut inbound: u64 = 0;
    let mut sandbox_seconds: u64 = 0;
    let mut tokens_input: u64 = 0;
    let mut tokens_output: u64 = 0;
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        // <metric_name>{labels...} <value>
        let (rest, value) = match line.rsplit_once(' ') {
            Some(p) => p,
            None => continue,
        };
        let value: u64 = value.parse::<f64>().unwrap_or(0.0).max(0.0) as u64;
        let (name, labels) = match rest.find('{') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        };
        // If a tenant filter is set, the line must contain the exact
        // label. Loose match because we don't fully parse — but the
        // tenant_id label is alphanumeric (TenantId only allows that),
        // so substring search is safe.
        if let Some(t) = tenant_filter {
            let needle = format!("tenant_id=\"{}\"", t);
            if !labels.contains(&needle) {
                continue;
            }
        }
        match name {
            "weclawbot_billing_inbound_messages_total" => inbound += value,
            "weclawbot_billing_sandbox_seconds_total" => sandbox_seconds += value,
            "weclawbot_billing_ai_tokens_total" => {
                if labels.contains("direction=\"input\"") {
                    tokens_input += value;
                } else if labels.contains("direction=\"output\"") {
                    tokens_output += value;
                }
            }
            _ => {}
        }
    }
    (inbound, sandbox_seconds, tokens_input, tokens_output)
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

// v7.8 unit tests for the GUI-supporting parsers.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_window_h_d_m() {
        assert_eq!(parse_window_secs("1h"), 3600);
        assert_eq!(parse_window_secs("24h"), 24 * 3600);
        assert_eq!(parse_window_secs("7d"), 7 * 86_400);
        assert_eq!(parse_window_secs("30m"), 30 * 60);
        // bare number → assume hours
        assert_eq!(parse_window_secs("48"), 48 * 3600);
        // unknown unit → fall back to hours
        assert_eq!(parse_window_secs("5x"), 5 * 3600);
    }

    #[test]
    fn parse_window_clamps_extremes() {
        // Empty / garbage → 24h default
        assert_eq!(parse_window_secs(""), 24 * 3600);
        // Floor 60s (avoids degenerate divide-by-zero rates)
        assert_eq!(parse_window_secs("0h"), 60);
        // Ceiling 90d (queries beyond that hit pruned data)
        assert_eq!(parse_window_secs("365d"), 90 * 86_400);
    }

    #[test]
    fn parse_counters_sums_across_label_variants() {
        // Mock Prometheus exposition body. Two platforms summed.
        let text = r#"
# HELP weclawbot_billing_inbound_messages_total Counter
# TYPE weclawbot_billing_inbound_messages_total counter
weclawbot_billing_inbound_messages_total{tenant_id="default",platform="ilink-wechat"} 100
weclawbot_billing_inbound_messages_total{tenant_id="default",platform="telegram"} 50
weclawbot_billing_inbound_messages_total{tenant_id="acme",platform="ilink-wechat"} 200
weclawbot_billing_sandbox_seconds_total{tenant_id="default"} 12345
weclawbot_billing_ai_tokens_total{tenant_id="default",provider="claude",direction="input"} 9000
weclawbot_billing_ai_tokens_total{tenant_id="default",provider="claude",direction="output"} 4500
weclawbot_billing_ai_tokens_total{tenant_id="default",provider="openai-compat",direction="input"} 1000
"#;
        // Filter to "default" tenant.
        let (inbound, sandbox, tin, tout) = parse_billing_counters(text, Some("default"));
        assert_eq!(inbound, 150);
        assert_eq!(sandbox, 12345);
        assert_eq!(tin, 10000); // 9000 + 1000
        assert_eq!(tout, 4500);

        // No filter → sum across tenants.
        let (inbound_all, _, _, _) = parse_billing_counters(text, None);
        assert_eq!(inbound_all, 350); // 100 + 50 + 200
    }

    #[test]
    fn parse_counters_ignores_unrelated_metrics() {
        let text = r#"
weclawbot_inbound_messages_total{account_id="acct-1",status="received"} 42
weclawbot_api_requests_total{method="GET",path="/",status="200"} 999
"#;
        let (inbound, sandbox, tin, tout) =
            parse_billing_counters(text, Some("default"));
        // These aren't billing counters — should all be zero.
        assert_eq!((inbound, sandbox, tin, tout), (0, 0, 0, 0));
    }

    #[test]
    fn parse_counters_handles_float_values() {
        // metrics-exporter-prometheus emits counters as floats
        // (e.g. "42.0" instead of "42"); make sure we cast cleanly.
        let text = "weclawbot_billing_sandbox_seconds_total{tenant_id=\"default\"} 12345.0\n";
        let (_, sandbox, _, _) = parse_billing_counters(text, Some("default"));
        assert_eq!(sandbox, 12345);
    }
}
