//! Live config + diagnostics endpoints for the admin GUI.
//!
//! 这些 endpoint 把"必须 SSH 进机器看/改"的东西都暴露成 HTTP，让
//! GUI 上一个 super_admin 角色就够运维。
//!
//! - `GET /api/v1/config` — 当前 ~/.weclawbot/config.json（apiKey 掩码）
//! - `PUT /api/v1/config` — 整段写回（atomic write），daemon 下条 inbound 自动重读
//! - `GET /api/v1/logs?lines=N` — daemon 日志最后 N 行
//! - `GET /api/v1/sandboxes` — `podman ps` 输出，列出运行中的 per-user 沙箱

use axum::extract::{Query, Request};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::auth::admin_key::AdminContext;
use crate::config::Config;
use crate::repo::admin_keys::Role;
use crate::repo::audit::AuditInput;
use crate::repo::audit_async::SqlxAuditRepo;
use crate::storage::db_async;
use crate::storage::atomic_write::write_json_atomic;
use crate::storage::state_dir::{config_path, logs_dir};

// ============================================================================
// /config
// ============================================================================

pub async fn get_config(req: Request) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let _ = ctx_of(&req)?;
    let cfg = Config::load();
    let mut v = serde_json::to_value(&cfg).map_err(internal_str_box)?;
    // 掩码 ai.apiKey — 让 GUI 操作员知道"已设置"但不暴露明文给共享屏幕/截图
    if let Some(ai) = v.get_mut("ai").and_then(|x| x.as_object_mut()) {
        if let Some(key) = ai.get("apiKey").and_then(|x| x.as_str()) {
            if !key.is_empty() {
                ai.insert("apiKey".into(), Value::String(mask_secret(key)));
                ai.insert("_apiKeyMasked".into(), Value::Bool(true));
            }
        }
    }
    Ok(Json(v))
}

#[derive(Debug, Deserialize)]
pub struct PutConfigBody {
    /// 整段 Config JSON。如果 ai.apiKey 仍是 mask 形式 (sk_***...***)
    /// 或者带 `_apiKeyMasked: true`，daemon 保留磁盘上的原值。
    pub config: Value,
}

pub async fn put_config(req: Request) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = ctx_of(&req)?;
    require(&ctx, Role::ReadWrite)?;
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 256 * 1024)
        .await
        .map_err(|e| bad_request(&format!("read body: {e}")))?;
    let body_obj: PutConfigBody = serde_json::from_slice(&bytes)
        .map_err(|e| bad_request(&format!("parse body: {e}")))?;
    let mut incoming = body_obj.config;
    if !incoming.is_object() {
        return Err(bad_request("config must be a JSON object"));
    }

    // 一些字段绝对不能改：webhook.url 必须是 https:// 或空字符串
    if let Some(url) = incoming
        .get("webhook")
        .and_then(|w| w.get("url"))
        .and_then(|u| u.as_str())
    {
        if !url.is_empty() && !url.starts_with("https://") && !is_localhost(url) {
            return Err(bad_request(
                "webhook.url 必须以 https:// 开头（仅 localhost 允许 http://）",
            ));
        }
    }

    // 如果客户端传回 apiKey 是掩码或者标了 _apiKeyMasked，保留磁盘原值，
    // 避免 GUI 编辑顺手把真 key 涂成 "sk***...***" 写盘。
    let current = Config::load();
    let saved_api_key = current.ai.api_key.clone();
    let apply_existing_api_key = match incoming.get("ai").and_then(|a| a.get("apiKey")) {
        Some(Value::String(s)) => s.is_empty() || looks_masked(s),
        _ => true,
    };
    if apply_existing_api_key {
        if let Some(ai) = incoming.get_mut("ai").and_then(|x| x.as_object_mut()) {
            ai.insert("apiKey".into(), Value::String(saved_api_key));
            ai.remove("_apiKeyMasked");
        }
    }

    // 用 strong typing 解析一遍，拒绝结构错误的 JSON。
    let new_cfg: Config = serde_json::from_value(incoming)
        .map_err(|e| bad_request(&format!("config schema: {e}")))?;

    let path: PathBuf = config_path();
    let v = serde_json::to_value(&new_cfg).map_err(internal_str_box)?;
    write_json_atomic(&path, &v).map_err(internal_str)?;
    // v2.1.B1: 主动 reload ArcSwap 缓存 — 不要等 watcher 的 mtime 通知
    // （文件系统层 1-2s 延迟），让 GUI 改的下条 inbound 立刻生效。
    crate::config::Config::set_cached(new_cfg.clone());

    // Audit
    if let Some(pool) = db_async::try_global_async_pool() {
        let audit = SqlxAuditRepo::new(pool);
        let _ = audit
            .record(AuditInput {
                actor_key_id: Some(&ctx.key_id),
                action: "config.update",
                target: Some(path.to_string_lossy().as_ref()),
                before: Some(&serde_json::to_value(&current).unwrap_or(Value::Null)),
                after: Some(&serde_json::to_value(&new_cfg).unwrap_or(Value::Null)),
                ip: extract_ip(&parts.headers).as_deref(),
            })
            .await;
    }

    Ok(Json(json!({
        "ok": true,
        "note": "daemon 下一条 inbound 消息会自动读到新配置（热加载），不需要重启。"
    })))
}

fn mask_secret(s: &str) -> String {
    let n = s.chars().count();
    if n <= 8 {
        return "[hidden]".into();
    }
    let head: String = s.chars().take(4).collect();
    let tail: String = s.chars().skip(n - 4).collect();
    format!("{head}***{tail}")
}

fn looks_masked(s: &str) -> bool {
    s.contains("***") || s == "[hidden]"
}

fn is_localhost(url: &str) -> bool {
    if let Ok(u) = url::Url::parse(url) {
        matches!(
            u.host_str(),
            Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
        )
    } else {
        false
    }
}

// ============================================================================
// /logs
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    /// 多少行（默认 200，上限 2000）
    #[serde(default)]
    pub lines: Option<usize>,
}

pub async fn get_logs(
    Query(q): Query<LogsQuery>,
    req: Request,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ctx = ctx_of(&req)?;
    require(&ctx, Role::ReadWrite)?;
    let lines = q.lines.unwrap_or(200).min(2000);
    let path = current_log_path();
    let content = std::fs::read_to_string(&path).map_err(|e| {
        internal_str(format!("read {}: {e}", path.display()))
    })?;
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(lines);
    let tail: Vec<&str> = all[start..].to_vec();
    Ok(Json(json!({
        "path": path.to_string_lossy(),
        "total_lines": all.len(),
        "returned": tail.len(),
        "lines": tail,
    })))
}

fn current_log_path() -> PathBuf {
    let date = chrono::Local::now().format("%Y-%m-%d");
    logs_dir().join(format!("weclawbot-{date}.log"))
}

// ============================================================================
// /sandboxes
// ============================================================================

pub async fn get_sandboxes(req: Request) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let _ = ctx_of(&req)?;
    // podman ps --format json: 返回当前所有 running container
    let out = std::process::Command::new("podman")
        .args(["ps", "--format", "json"])
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            return Ok(Json(json!({
                "ok": false,
                "error": format!("podman ps exit {}: {}", o.status,
                    String::from_utf8_lossy(&o.stderr).trim())
            })));
        }
        Err(e) => {
            return Ok(Json(json!({
                "ok": false,
                "error": format!("podman not available: {e}")
            })));
        }
    };
    let containers: Value =
        serde_json::from_slice(&out).unwrap_or(Value::Array(Vec::new()));
    let summary: Vec<Value> = containers
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| {
                    json!({
                        "id": c.get("Id").and_then(|x| x.as_str()).map(|s| &s[..12.min(s.len())]),
                        "image": c.get("Image"),
                        "command": c.get("Command"),
                        "state": c.get("State"),
                        "status": c.get("Status"),
                        "created": c.get("CreatedAt"),
                        "names": c.get("Names"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(json!({
        "ok": true,
        "count": summary.len(),
        "containers": summary,
    })))
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

fn internal_str(detail: impl Into<String>) -> (StatusCode, Json<Value>) {
    let d = detail.into();
    tracing::error!("sysconfig api: {d}");
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

fn internal_str_box<E: std::fmt::Display>(e: E) -> (StatusCode, Json<Value>) {
    internal_str(e.to_string())
}

fn extract_ip(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
}
