use std::sync::OnceLock;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json};
use regex::Regex;
use serde_json::{json, Value};

use super::page::console_html;
use super::state::{build_accounts_list, build_health};
use crate::api::client::ILinkClient;
use crate::auth::accounts::{
    get_local_bot_tokens, normalize_account_id, register_account_id, save_account,
};
use crate::sandbox;
use crate::skills::detect;

/// Reject any `user_hash` path-parameter that doesn't match the canonical
/// `u-<12 lowercase hex>` shape. Without this every `/api/users/{hash}/...`
/// route would let an attacker write to arbitrary `~/.weclawbot/` paths via
/// `hash="../config"` etc. (CVE-class P0 — fixed in Phase 0.1).
fn validate_user_hash(hash: &str) -> Result<(), (StatusCode, Json<Value>)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^u-[0-9a-f]{12}$").unwrap());
    if re.is_match(hash) {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": "invalid user_hash; expected u-<12 lowercase hex>"
            })),
        ))
    }
}

// -------------------- existing health/account routes --------------------

pub async fn get_root() -> Html<String> {
    // v2.2 L5.1: console_html() now returns Cow — operator may override via
    // ~/.weclawbot/console/index.html without rebuilding the binary.
    Html(console_html().into_owned())
}

pub async fn get_health() -> Json<Value> {
    Json(serde_json::to_value(build_health()).unwrap_or(json!({"ok": false})))
}

pub async fn get_accounts() -> Json<Value> {
    Json(json!({ "accounts": build_accounts_list() }))
}

pub async fn post_qr_create() -> Json<Value> {
    let client = ILinkClient::new();
    let local_tokens = get_local_bot_tokens();
    match client
        .fetch_qr_code("https://ilinkai.weixin.qq.com", "3", &local_tokens)
        .await
    {
        Ok(qr) => Json(json!({
            "ok": true,
            "qrcode": qr.qrcode,
            "qrcode_img_content": qr.qrcode_img_content,
        })),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

pub async fn get_qr_status(Path(key): Path<String>) -> Json<Value> {
    let client = ILinkClient::new();
    match client
        .poll_qr_status("https://ilinkai.weixin.qq.com", &key, None)
        .await
    {
        Ok(status) => {
            if status.status == "confirmed" {
                if let Some(bot_id) = &status.ilink_bot_id {
                    let account_id = normalize_account_id(bot_id);
                    save_account(
                        &account_id,
                        status.bot_token.as_deref(),
                        status.baseurl.as_deref(),
                        status.ilink_user_id.as_deref(),
                    );
                    register_account_id(&account_id);
                }
            }
            Json(json!({
                "ok": true,
                "status": status.status,
                "bot_token": status.bot_token,
                "ilink_bot_id": status.ilink_bot_id,
                "baseurl": status.baseurl,
            }))
        }
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

pub async fn post_relogin(Path(id): Path<String>) -> Json<Value> {
    let client = ILinkClient::new();
    let local_tokens = get_local_bot_tokens();
    match client
        .fetch_qr_code("https://ilinkai.weixin.qq.com", "3", &local_tokens)
        .await
    {
        Ok(qr) => Json(json!({
            "ok": true,
            "account_id": id,
            "qrcode": qr.qrcode,
            "qrcode_img_content": qr.qrcode_img_content,
        })),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

pub async fn post_link_agent(Json(body): Json<Value>) -> Json<Value> {
    let user_id = body["user_id"].as_str().unwrap_or("");
    let account_id = body["account_id"].as_str().unwrap_or("");
    if user_id.is_empty() || account_id.is_empty() {
        return Json(json!({"ok": false, "error": "user_id and account_id required"}));
    }
    let record = crate::binding::agent_map::register_or_update_binding(user_id, account_id);
    Json(json!({
        "ok": true,
        "agent_id": record.agent_id,
        "user_id": record.user_id,
        "active_account_id": record.active_account_id,
    }))
}

pub async fn post_gateway_restart() -> Json<Value> {
    Json(json!({"ok": true, "message": "gateway restart no-op in standalone mode"}))
}

pub async fn get_errors() -> Json<Value> {
    Json(json!({"errors": []}))
}

pub async fn fallback() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"})))
}

// -------------------- defaults --------------------

pub async fn get_defaults() -> Json<Value> {
    let _ = crate::defaults::ensure_defaults_exist();
    Json(json!({
        "settings": crate::defaults::load_defaults(),
        "path": crate::sandbox::layout::defaults_settings_path().display().to_string(),
    }))
}

pub async fn put_defaults(Json(body): Json<Value>) -> Json<Value> {
    let settings = match body.get("settings") {
        Some(v) if v.is_object() => v.clone(),
        _ => return Json(json!({"ok": false, "error": "expected {settings: object}"})),
    };
    match crate::defaults::save_defaults(&settings) {
        Ok(()) => Json(json!({"ok": true})),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

/// Apply current defaults to specific users (or all if `user_hashes` empty).
pub async fn post_defaults_apply(Json(body): Json<Value>) -> Json<Value> {
    let target: Vec<String> = body
        .get("user_hashes")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();

    // Per-element validation: any malformed hash gets rejected here so it
    // can't escape the user directory tree (path-traversal guard).
    for h in &target {
        if validate_user_hash(h).is_err() {
            return Json(json!({
                "ok": false,
                "error": format!("invalid user_hash in list: {h:?}")
            }));
        }
    }

    let hashes = if target.is_empty() {
        sandbox::list_user_hashes()
    } else {
        target
    };
    let mut applied: Vec<Value> = Vec::new();
    for h in hashes {
        match crate::defaults::apply_defaults_to_user(&h) {
            Ok(_) => {
                schedule_reconcile(&h);
                applied.push(json!({"user_hash": h, "ok": true, "syncing": true}));
            }
            Err(e) => applied.push(json!({"user_hash": h, "ok": false, "error": e})),
        }
    }
    Json(json!({"ok": true, "applied": applied}))
}

// -------------------- users --------------------

pub async fn get_users() -> Json<Value> {
    // v4.1 K9: 优先走 sqlx async pool。sqlx pool 未 init 时（极少数 boot
    // 早期）跌回 rusqlite sync 路径，省 spawn_blocking 跳板。
    let profiles_result = if let Some(apool) =
        crate::storage::db_async::try_global_async_pool()
    {
        let r = crate::repo::users_async::SqlxUserRepo::new(apool);
        r.list_profiles().await
    } else {
        // sqlx pool 不可用 — 返回空（CLI 路径走自己的 open_default）
        Ok(Vec::new())
    };
    let profiles = match profiles_result {
        Ok(p) => p,
        Err(e) => {
            return Json(json!({"users": [], "error": format!("{e}")}));
        }
    };
    let users: Vec<Value> = profiles
        .into_iter()
        .map(|p| {
            json!({
                "hash": p.hash.as_str(),
                "profile": {
                    "user_hash": p.hash.as_str(),
                    "user_id_hint": p.user_id_hint,
                    "created_at": p.created_at,
                    "last_seen": p.last_seen_at,
                    "message_count": p.message_count,
                    "sync_state": p.sync_state,
                    "last_sync_at": p.last_sync_at,
                    "last_sync_error": p.last_sync_error,
                }
            })
        })
        .collect();
    Json(json!({"users": users}))
}

pub async fn get_user_settings(Path(hash): Path<String>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    validate_user_hash(&hash)?;
    let settings = crate::defaults::load_user_settings(&hash).unwrap_or_else(|| json!({}));
    Ok(Json(json!({"hash": hash, "settings": settings})))
}

pub async fn put_user_settings(
    Path(hash): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    validate_user_hash(&hash)?;
    let settings = match body.get("settings") {
        Some(v) if v.is_object() => v.clone(),
        _ => {
            return Ok(Json(
                json!({"ok": false, "error": "expected {settings: object}"}),
            ))
        }
    };
    if let Err(e) = crate::defaults::save_user_settings(&hash, &settings) {
        return Ok(Json(json!({"ok": false, "error": e})));
    }
    schedule_reconcile(&hash);
    audit_routes("users.settings.update", Some(&hash), Some(&settings));
    Ok(Json(json!({"ok": true, "hash": hash, "syncing": true})))
}

/// Spawn a background task that ensures the user's sandbox exists, syncs
/// settings.json in, and reconciles plugins via PTY automation.
///
/// Writes `sync_state` (and optionally `last_sync_error`) on the user
/// row so the GUI reflects what happened — `syncing` → `synced` /
/// `failed` — instead of staying on the initial `unknown` forever.
fn schedule_reconcile(user_hash: &str) {
    let user_hash = user_hash.to_string();
    set_user_sync_state(&user_hash, "syncing", None);
    tokio::spawn(async move {
        // Heavy work (PTY, podman) goes on a blocking thread so the tokio
        // executor isn't held up.
        let user_hash_clone = user_hash.clone();
        let join = tokio::task::spawn_blocking(move || run_reconcile(&user_hash_clone));
        match join.await {
            Ok(Ok(report)) => {
                tracing::info!(
                    "reconcile {} done: added={:?} removed={:?} errors={}",
                    user_hash,
                    report.added,
                    report.removed,
                    report.errors.len()
                );
                let err = if report.errors.is_empty() {
                    None
                } else {
                    Some(report.errors.join("; "))
                };
                let state = if report.errors.is_empty() { "synced" } else { "failed" };
                set_user_sync_state(&user_hash, state, err.as_deref());
            }
            Ok(Err(e)) => {
                tracing::warn!("reconcile {} failed: {}", user_hash, e);
                set_user_sync_state(&user_hash, "failed", Some(&e));
            }
            Err(e) => {
                tracing::warn!("reconcile {} panicked: {}", user_hash, e);
                set_user_sync_state(&user_hash, "failed", Some(&format!("panic: {e}")));
            }
        }
    });
}

fn set_user_sync_state(user_hash: &str, state: &str, error: Option<&str>) {
    use crate::ids::UserHash;
    use crate::repo::users_async::SqlxUserRepo;
    use crate::runtime::blocking::block_on_async;
    let Some(pool) = crate::storage::db_async::try_global_async_pool() else { return };
    let r = SqlxUserRepo::new(pool);
    let user_hash_owned = user_hash.to_string();
    let state_owned = state.to_string();
    let error_owned = error.map(String::from);
    block_on_async(async move {
        if let Err(e) = r
            .set_sync_state(&UserHash::new(&user_hash_owned), &state_owned, error_owned.as_deref())
            .await
        {
            tracing::warn!("set_sync_state({user_hash_owned}, {state_owned}): {e}");
        }
    })
}

/// `POST /api/v1/users/{hash}/sync` — explicit manual sync trigger.
pub async fn post_user_sync(
    Path(hash): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    validate_user_hash(&hash)?;
    let user_dir = crate::sandbox::layout::user_dir(&hash);
    if !user_dir.exists() {
        // First sync — make sure dirs exist by loading the user id via
        // profile hint isn't possible (the original WeChat id isn't
        // stored). Use Sandbox::ensure with the hash itself.
        if let Err(e) = std::fs::create_dir_all(&user_dir) {
            return Ok(Json(json!({"ok": false, "error": format!("mkdir: {e}")})));
        }
    }
    schedule_reconcile(&hash);
    audit_routes("users.sync", Some(&hash), None);
    Ok(Json(json!({"ok": true, "hash": hash, "state": "syncing"})))
}

fn run_reconcile(
    user_hash: &str,
) -> Result<crate::automation::claude_cli::ReconcileReport, String> {
    // Recover the "user_id" (we hash internally; ensure() also hashes, so
    // we pre-hash it back by using the hash as both inputs — simpler: load
    // the existing user dir directly).
    let user_dir = crate::sandbox::layout::user_dir(user_hash);
    if !user_dir.exists() {
        return Err(format!("no such user {user_hash}"));
    }
    let sandbox = crate::sandbox::Sandbox {
        user_hash: user_hash.to_string(),
        user_dir,
    };
    // Make sure settings are mirrored into the sandbox before reconciling.
    crate::sandbox::materialize::sync_user_settings_into_sandbox(user_hash)?;
    Ok(crate::automation::claude_cli::reconcile_user_plugins(&sandbox))
}

pub async fn delete_user(
    Path(hash): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    validate_user_hash(&hash)?;
    use crate::ids::UserHash;
    use crate::repo::users_async::SqlxUserRepo;

    // v2.2 L3.1 / v4.2 L8: 业务逻辑集中在 `crate::app::users::delete` ——
    // CLI + HTTP + WeChat menu 都 delegate 到那里，async sqlx 路径。
    let pool = match crate::storage::db_async::try_global_async_pool() {
        Some(p) => p,
        None => return Ok(Json(json!({"ok": false, "error": "async db pool not ready"}))),
    };
    let repo = SqlxUserRepo::new(pool);
    let user_hash = UserHash::new(&hash);
    let out = match crate::app::users::delete(&repo, &user_hash).await {
        Ok(o) => o,
        Err(e) => {
            return Ok(Json(json!({"ok": false, "error": format!("{e}")})));
        }
    };
    if out.containers_killed > 0 {
        tracing::info!(
            "delete_user: killed {} container(s) for {hash}",
            out.containers_killed
        );
    }
    if out.db_removed {
        audit_routes("users.delete", Some(&hash), None);
        Ok(Json(json!({
            "ok": true,
            "hash": hash,
            "containers_killed": out.containers_killed,
            "dir_removed": out.dir_removed,
        })))
    } else {
        Ok(Json(json!({"ok": false, "error": "no such user"})))
    }
}

/// Return the chat history (history.json) for one user. Format:
/// `{ ok, hash, turns: [{role, content}, ...] }`. Empty `turns` if the
/// user has no history yet.
pub async fn get_user_history(
    Path(hash): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    validate_user_hash(&hash)?;
    let turns = crate::ai::history::get(&hash).await;
    let count = turns.len();
    Ok(Json(json!({
        "ok": true,
        "hash": hash,
        "count": count,
        "turns": turns,
    })))
}

/// Clear the chat history for one user. Mirrors the WeChat-side
/// `/menu → 开启新对话` action so operators can do it from the console.
pub async fn delete_user_history(
    Path(hash): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    validate_user_hash(&hash)?;
    let user_dir = crate::sandbox::layout::user_dir(&hash);
    if !user_dir.exists() {
        return Ok(Json(json!({"ok": false, "error": "no such user"})));
    }
    crate::ai::history::clear(&hash).await;
    audit_routes("users.history.clear", Some(&hash), None);
    Ok(Json(json!({"ok": true, "hash": hash, "cleared": true})))
}

// -------------------- test mode (inject + capture) --------------------

/// Inject a synthetic inbound message through the full handler pipeline.
/// Only available when `WECLAWBOT_TEST_MODE=1` is set in the daemon env.
///
/// Body: `{ from_user_id, text?, attachment_paths?[<host path>] }`.
/// All outbound messages the daemon would have sent are tee'd to
/// `~/.weclawbot/capture.log` (JSONL); the caller reads that to assert.
pub async fn post_test_inject(Json(body): Json<Value>) -> Json<Value> {
    if !crate::service::test_inject::test_mode_enabled() {
        return Json(json!({"ok": false, "error": "test mode not enabled"}));
    }

    let from = match body.get("from_user_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return Json(json!({"ok": false, "error": "from_user_id required"})),
    };
    let text = body
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let attachment_paths: Vec<String> = body
        .get("attachment_paths")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Synthesize a WeixinMessage.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut items: Vec<crate::api::types::MessageItem> = Vec::new();
    if !text.is_empty() {
        items.push(crate::api::types::MessageItem {
            item_type: Some(crate::api::types::MESSAGE_ITEM_TYPE_TEXT),
            text_item: Some(crate::api::types::TextItem {
                text: Some(text.clone()),
            }),
            ..Default::default()
        });
    }
    // Wrap each attachment as a "fake CDN media" by setting full_url to the
    // file:// path; resolve_message will fetch it via reqwest (works for
    // local files thanks to reqwest's file:// support… actually no, reqwest
    // doesn't do file://). Easier: write to sandbox media dir directly,
    // bypass resolve_message for injected files.
    //
    // For simplicity: synthesize message with no attachments, then
    // separately copy each attachment_paths file into the user's sandbox
    // media dir AFTER ensure(). The handler scans it normally.
    let msg = crate::api::types::WeixinMessage {
        message_id: Some(now_ms),
        from_user_id: Some(from.clone()),
        to_user_id: Some("test-bot".to_string()),
        message_type: Some(crate::api::types::MESSAGE_TYPE_USER),
        message_state: Some(crate::api::types::MESSAGE_STATE_FINISH),
        item_list: Some(items),
        create_time_ms: Some(now_ms),
        update_time_ms: Some(now_ms),
        ..Default::default()
    };

    // NOTE: we used to truncate the capture log on every inject, but that
    // breaks any test that sends multiple inbound messages in sequence (the
    // /menu console flow being the clearest case — 5 sequential injects).
    // The smoke harness handles resets explicitly via reset_capture() between
    // independent test cases, so we leave it alone here.

    // Pre-stage attachment files into the sandbox media inbound dir so
    // the AI prompt sees them.
    // Phase 6.4: test injection 也用 compat-aware lookup，否则注入老
    // 用户时算出来的 SHA-256 跟实际 DB 里的 SHA-1 hash 不对。
    let user_hash = crate::sandbox::hash_user_id_for_lookup(&from);
    if !attachment_paths.is_empty() {
        let _ = crate::sandbox::Sandbox::ensure(&from);
        let target_dir = crate::sandbox::layout::user_sandbox_root(&user_hash)
            .join("media")
            .join("inbound");
        let _ = std::fs::create_dir_all(&target_dir);
        // v2.1.A5: src 路径必须 canonicalize 后落在受信 staging 子树内。
        // test_mode 已经是 env 闸门，但纵深防御 —— 即使 test_mode 误开，
        // 攻击者也不能用这个 endpoint 把 host 上任意文件（/etc/passwd /
        // ~/.ssh/* 之类）拷进 inbound dir 再让 AI 读出去。
        let staging_root = test_staging_root();
        for src in &attachment_paths {
            let src_path = std::path::Path::new(src);
            let canon = match src_path.canonicalize() {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!(
                        "test_inject: src path not accessible / not canonical: {src}"
                    );
                    continue;
                }
            };
            if !canon.starts_with(&staging_root) {
                tracing::warn!(
                    "test_inject: refusing src outside staging dir: {} (staging={})",
                    canon.display(),
                    staging_root.display()
                );
                continue;
            }
            if let Some(fname) = canon.file_name() {
                let dest = target_dir.join(fname);
                let _ = std::fs::copy(&canon, &dest);
            }
        }
    }

    // Build a dummy ILinkClient (we won't actually send because test mode
    // intercepts both send_message and send_typing).
    let client = crate::api::client::ILinkClient::new();
    crate::monitor::handler::handle_inbound_message(
        &client,
        "test-account",
        "test-token",
        "https://example.invalid",
        &msg,
    )
    .await;

    Json(json!({
        "ok": true,
        "user_hash": user_hash,
        "capture_log": crate::service::test_inject::capture_log_path().display().to_string(),
    }))
}

pub async fn get_test_capture() -> Json<Value> {
    if !crate::service::test_inject::test_mode_enabled() {
        return Json(json!({"ok": false, "error": "test mode not enabled"}));
    }
    let path = crate::service::test_inject::capture_log_path();
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut entries: Vec<Value> = Vec::new();
    for line in raw.lines() {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            entries.push(v);
        }
    }
    Json(json!({"ok": true, "entries": entries}))
}

// -------------------- claude schema (read-only) --------------------

/// Returns the operator-installed plugin set (informational; admin uses
/// this in GUI to know what they can put in `enabledPlugins`).
pub async fn get_claude_schema() -> Json<Value> {
    let plugins: Vec<Value> = detect::enabled_plugins()
        .iter()
        .map(|p| json!({"plugin": p.plugin, "marketplace": p.marketplace}))
        .collect();
    let skills: Vec<Value> = detect::discover_skills()
        .iter()
        .map(|s| json!({"plugin": s.plugin, "skill": s.skill, "description": s.description}))
        .collect();
    Json(json!({
        "installedPlugins": plugins,
        "discoveredSkills": skills,
        // TODO(phase D): expose `claude config list -g` keys + slash command list
        "configKeys": Value::Null,
        "slashCommands": Value::Null,
    }))
}

/// v2.1.C1: 简短 audit helper — routes.rs 里的 mutating handler 走它。
/// 4 个之前漏的写路径（put_user_settings / delete_user / delete_user_history
/// / post_user_sync）以及 wechat_menu/apply 通过这个统一写 audit_log。
///
/// 注意 actor_key_id 这里给 None ("system"-actor) 因为 routes.rs
/// handler 没拿到 AdminContext extension（路由没经过 admin auth
/// middleware 的 ctx 注入路径）。要拿 actor 应该用 service::admin 那套
/// req.extensions().get::<AdminContext>() 的写法 —— v2.1 优先保证有记录，
/// actor 字段填准确性留 v2.2。
fn audit_routes(action: &str, target: Option<&str>, after: Option<&serde_json::Value>) {
    use crate::repo::audit::AuditInput;
    use crate::repo::audit_async::SqlxAuditRepo;
    use crate::runtime::blocking::block_on_async;
    let Some(pool) = crate::storage::db_async::try_global_async_pool() else { return };
    let r = SqlxAuditRepo::new(pool);
    let action_owned = action.to_string();
    let target_owned = target.map(String::from);
    let after_owned = after.cloned();
    block_on_async(async move {
        let _ = r.record(AuditInput {
            actor_key_id: None,
            action: &action_owned,
            target: target_owned.as_deref(),
            before: None,
            after: after_owned.as_ref(),
            ip: None,
        }).await;
    })
}

/// v2.1.A5: test_inject 接受 attachment src 必须落在这个目录子树内。
/// 默认 `~/.weclawbot/test-staging/` —— daemon 启动时确保存在。
/// 运营商可以通过 env `WECLAWBOT_TEST_STAGING_DIR` 覆盖（例如 /tmp/...）
/// 让 CI 用 tempdir。canonicalize 后比较，对 symlink 友好。
fn test_staging_root() -> std::path::PathBuf {
    if let Ok(v) = std::env::var("WECLAWBOT_TEST_STAGING_DIR") {
        if !v.is_empty() {
            if let Ok(c) = std::path::PathBuf::from(&v).canonicalize() {
                return c;
            }
        }
    }
    let dir = crate::storage::state_dir::state_dir().join("test-staging");
    let _ = std::fs::create_dir_all(&dir);
    dir.canonicalize().unwrap_or(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_user_hash_accepts_canonical() {
        assert!(validate_user_hash("u-44a160bb371f").is_ok());
        assert!(validate_user_hash("u-000000000000").is_ok());
        assert!(validate_user_hash("u-ffffffffffff").is_ok());
    }

    #[test]
    fn validate_user_hash_rejects_path_traversal() {
        // Any hash with `/`, `\`, `..`, or wrong length is rejected.
        assert!(validate_user_hash("../config").is_err());
        assert!(validate_user_hash("u-../../etc/passwd").is_err());
        assert!(validate_user_hash("u-..").is_err());
        assert!(validate_user_hash("u-..%2f..").is_err());
    }

    #[test]
    fn validate_user_hash_rejects_wrong_length() {
        assert!(validate_user_hash("u-").is_err());
        assert!(validate_user_hash("u-44a160bb371").is_err()); // 11 hex
        assert!(validate_user_hash("u-44a160bb371f1").is_err()); // 13 hex
    }

    #[test]
    fn validate_user_hash_rejects_non_hex() {
        assert!(validate_user_hash("u-XXXXXXXXXXXX").is_err());
        assert!(validate_user_hash("u-44a160bb371G").is_err());
        // uppercase hex rejected — canonical form is lowercase only
        assert!(validate_user_hash("u-44A160BB371F").is_err());
    }

    #[test]
    fn validate_user_hash_rejects_wrong_prefix() {
        assert!(validate_user_hash("44a160bb371f").is_err());
        assert!(validate_user_hash("v-44a160bb371f").is_err());
        assert!(validate_user_hash("user-44a160bb371f").is_err());
    }

    #[test]
    fn validate_user_hash_rejects_empty() {
        assert!(validate_user_hash("").is_err());
    }
}
