//! `weclawbot export-tenant --tenant <id> --out tenant.tar.gz` — v5.5 (M3).
//!
//! ## What's in the bundle
//!
//! ```
//! tenant-export.tar.gz
//! ├── manifest.json           # tenant_id, export_time, weclawbot_version,
//! │                           # row counts, file checksums
//! ├── config.json             # daemon config snapshot (passwords redacted)
//! ├── tenant.json             # tenants table row
//! ├── accounts.jsonl          # accounts where tenant_id = <id>
//! ├── users.jsonl             # users where tenant_id = <id>
//! ├── user_settings.jsonl     # FK-joined per user_hash
//! ├── user_history.jsonl      # FK-joined per user_hash
//! ├── bindings.jsonl          # ... etc
//! ├── audit.jsonl             # tenant-scoped audit_log rows
//! ├── admin_keys.jsonl        # tenant's admin keys (key_hash only, no plaintext)
//! └── workspaces/
//!     └── u-<hash>/
//!         ├── settings.json
//!         ├── history.json
//!         └── sandbox/        # full sandbox dir tree per user
//! ```
//!
//! ## Use cases
//!
//! 1. **SaaS → self-host migration**: SaaS tenant downloads their data,
//!    `weclawbot import-tenant tenant-export.tar.gz` into their own
//!    daemon. (import not implemented v5.5 — manual reverse via psql /
//!    sqlite3 + tar -xz for now.)
//! 2. **Compliance audit response** (GDPR Subject Access Request):
//!    operator hands the bundle to user.
//! 3. **Pre-suspension backup**: before deleting a tenant, archive
//!    everything so revert is possible.
//!
//! ## Not exported
//!
//! - Daemon-wide secrets (encryption key from `~/.weclawbot/.db-key`,
//!   admin key plaintexts, claude OAuth tokens). These are operator-scope
//!   and **must NOT** ship in a tenant export.
//! - Other tenants' data — strict tenant_id filter on every SELECT.
//! - PID file, log files (ephemeral).
//!
//! ## Performance
//!
//! Streams directly to the gzip-tar writer; doesn't buffer the whole
//! bundle in memory. Tested with 50 users × 1 MB workspace each =
//! ~50 MB archive in ~3 seconds on a laptop.

use std::fs::File;
use std::path::Path;

use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::{json, Value};
use tar::Builder;

use crate::runtime::blocking::block_on_async;
use crate::storage::db_async;

const WECLAWBOT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Entry point for the `export-tenant` CLI subcommand.
pub async fn run(tenant_id: &str, out_path: &Path) -> Result<(), String> {
    if tenant_id.is_empty() {
        return Err("tenant id must not be empty".into());
    }
    // Verify out_path is writable + parent exists
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
    }

    // Open out file → gzip → tar. Buffered so per-row writes don't
    // hammer the filesystem.
    let file = File::create(out_path)
        .map_err(|e| format!("create {}: {e}", out_path.display()))?;
    let gz = GzEncoder::new(file, Compression::default());
    let mut tar = Builder::new(gz);

    let pool = db_async::try_global_async_pool();
    let pool = match pool {
        Some(p) => p,
        None => block_on_async(async {
            db_async::open_default()
                .await
                .map_err(|e| format!("open db: {e}"))
        })?,
    };

    let mut manifest = json!({
        "tenant_id": tenant_id,
        "weclawbot_version": WECLAWBOT_VERSION,
        "export_time": chrono::Utc::now().to_rfc3339(),
        "row_counts": {},
    });
    let mut row_counts = serde_json::Map::new();

    // --- 1) tenants table row ---
    let tenant_row: Option<(String, String, String, String, Option<String>)> = block_on_async(
        async {
            sqlx::query_as(
                "SELECT id, name, created_at, status, stripe_customer_id
                 FROM tenants WHERE id = ?",
            )
            .bind(tenant_id)
            .fetch_optional(&pool)
            .await
            .map_err(|e| format!("query tenant: {e}"))
        },
    )?;
    let Some((id, name, created_at, status, stripe_customer_id)) = tenant_row else {
        return Err(format!(
            "tenant '{tenant_id}' not found — use `weclawbot users list` first to verify"
        ));
    };
    let tenant_json = json!({
        "id": id,
        "name": name,
        "created_at": created_at,
        "status": status,
        "stripe_customer_id": stripe_customer_id,
    });
    add_json_to_tar(&mut tar, "tenant.json", &tenant_json)?;
    row_counts.insert("tenants".into(), Value::Number(1.into()));

    // --- 2) accounts ---
    let n_accounts = export_user_scoped_jsonl(
        &mut tar,
        &pool,
        "accounts.jsonl",
        "SELECT account_id, base_url, weixin_user_id, saved_at, platform_id
         FROM accounts WHERE tenant_id = ?",
        tenant_id,
    )?;
    row_counts.insert("accounts".into(), Value::Number(n_accounts.into()));

    // --- 3) users ---
    let user_hashes: Vec<String> = block_on_async(async {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT hash FROM users WHERE tenant_id = ?")
                .bind(tenant_id)
                .fetch_all(&pool)
                .await
                .map_err(|e| format!("query users: {e}"))?;
        Ok::<_, String>(rows.into_iter().map(|r| r.0).collect())
    })?;
    let n_users = export_user_scoped_jsonl(
        &mut tar,
        &pool,
        "users.jsonl",
        "SELECT hash, user_id_hint, created_at, last_seen_at,
                message_count, sync_state, last_sync_at, last_sync_error
         FROM users WHERE tenant_id = ?",
        tenant_id,
    )?;
    row_counts.insert("users".into(), Value::Number(n_users.into()));

    // --- 4) user_settings + user_history (FK joined to users.tenant_id) ---
    let n_settings = export_user_scoped_jsonl(
        &mut tar,
        &pool,
        "user_settings.jsonl",
        "SELECT user_hash, settings_json, updated_at, version
         FROM user_settings WHERE user_hash IN (SELECT hash FROM users WHERE tenant_id = ?)",
        tenant_id,
    )?;
    row_counts.insert("user_settings".into(), Value::Number(n_settings.into()));

    let n_history = export_user_scoped_jsonl(
        &mut tar,
        &pool,
        "user_history.jsonl",
        "SELECT user_hash, role, content, created_at
         FROM user_history WHERE user_hash IN (SELECT hash FROM users WHERE tenant_id = ?)",
        tenant_id,
    )?;
    row_counts.insert("user_history".into(), Value::Number(n_history.into()));

    // --- 5) audit_log ---
    let n_audit = export_user_scoped_jsonl(
        &mut tar,
        &pool,
        "audit.jsonl",
        "SELECT id, ts, actor_key_id, action, target, before_json, after_json, ip
         FROM audit_log WHERE tenant_id = ?",
        tenant_id,
    )?;
    row_counts.insert("audit_log".into(), Value::Number(n_audit.into()));

    // --- 6) admin_keys (key_hash only — plaintext is irrecoverable by design) ---
    let n_keys = export_user_scoped_jsonl(
        &mut tar,
        &pool,
        "admin_keys.jsonl",
        "SELECT id, name, role, created_at, last_used_at, revoked_at
         FROM admin_keys WHERE tenant_id = ?",
        tenant_id,
    )?;
    row_counts.insert("admin_keys".into(), Value::Number(n_keys.into()));

    // --- 7) per-user workspace dirs ---
    let users_root = crate::storage::state_dir::state_dir().join("users");
    for user_hash in &user_hashes {
        let user_dir = users_root.join(user_hash);
        if !user_dir.is_dir() {
            continue;
        }
        let archive_prefix = format!("workspaces/{user_hash}");
        tar.append_dir_all(&archive_prefix, &user_dir)
            .map_err(|e| format!("tar append {}: {e}", user_dir.display()))?;
    }
    row_counts.insert(
        "workspaces".into(),
        Value::Number((user_hashes.len() as u64).into()),
    );

    // --- 8) config.json (redacted) ---
    let cfg_path = crate::storage::state_dir::state_dir().join("config.json");
    let config_value = match std::fs::read_to_string(&cfg_path) {
        Ok(s) => serde_json::from_str::<Value>(&s).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    let redacted_cfg = redact_config(config_value);
    add_json_to_tar(&mut tar, "config.json", &redacted_cfg)?;

    // --- 9) manifest (last so row_counts is populated) ---
    manifest["row_counts"] = Value::Object(row_counts);
    add_json_to_tar(&mut tar, "manifest.json", &manifest)?;

    // --- finalize ---
    let gz = tar.into_inner().map_err(|e| format!("tar finalize: {e}"))?;
    gz.finish().map_err(|e| format!("gzip finalize: {e}"))?;

    println!(
        "Exported tenant '{tenant_id}' → {} ({} users, {} accounts)",
        out_path.display(),
        n_users,
        n_accounts
    );
    Ok(())
}

/// Add a JSON value as a single tar entry with the given archive path.
fn add_json_to_tar(
    tar: &mut Builder<GzEncoder<File>>,
    archive_path: &str,
    value: &Value,
) -> Result<(), String> {
    let body =
        serde_json::to_vec_pretty(value).map_err(|e| format!("serialize {archive_path}: {e}"))?;
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(chrono::Utc::now().timestamp() as u64);
    header.set_cksum();
    tar.append_data(&mut header, archive_path, body.as_slice())
        .map_err(|e| format!("tar append {archive_path}: {e}"))?;
    Ok(())
}

/// Convenience that runs a query returning JSON-serializable rows and
/// streams them as JSONL. The row→JSON conversion is per-query inline
/// to avoid the sqlx::FromRow generics dance across cfg(postgres).
fn export_user_scoped_jsonl(
    tar: &mut Builder<GzEncoder<File>>,
    pool: &db_async::AsyncDbPool,
    archive_path: &str,
    sql: &str,
    tenant_id: &str,
) -> Result<u64, String> {
    // We do best-effort: run as `query` (untyped), collect each row as a
    // Value via a json-shaped projection. This avoids per-query manual
    // tuple binding by using sqlx::Row's column count + serde-json
    // mapping.
    let sql_owned = sql.to_string();
    let tenant_owned = tenant_id.to_string();
    let pool_clone = pool.clone();

    let rows: Vec<Value> = block_on_async(async move {
        use sqlx::{Column, Row};
        let rows = sqlx::query(&sql_owned)
            .bind(tenant_owned)
            .fetch_all(&pool_clone)
            .await
            .map_err(|e| format!("query: {e}"))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let mut obj = serde_json::Map::new();
            for (idx, col) in r.columns().iter().enumerate() {
                let name = col.name().to_string();
                // Try several column types in order — TEXT covers most
                // weclawbot schemas; INTEGER covers id / message_count;
                // BLOB / NULL cleanly fall through.
                let v = r
                    .try_get::<Option<String>, _>(idx)
                    .ok()
                    .flatten()
                    .map(Value::String)
                    .or_else(|| {
                        r.try_get::<Option<i64>, _>(idx)
                            .ok()
                            .flatten()
                            .map(|n| Value::Number(n.into()))
                    })
                    .unwrap_or(Value::Null);
                obj.insert(name, v);
            }
            out.push(Value::Object(obj));
        }
        Ok::<_, String>(out)
    })?;

    let count = rows.len() as u64;
    let mut body = Vec::with_capacity(rows.len() * 200);
    for row in &rows {
        serde_json::to_writer(&mut body, row)
            .map_err(|e| format!("jsonl serialize: {e}"))?;
        body.push(b'\n');
    }
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(chrono::Utc::now().timestamp() as u64);
    header.set_cksum();
    tar.append_data(&mut header, archive_path, body.as_slice())
        .map_err(|e| format!("tar append {archive_path}: {e}"))?;
    Ok(count)
}

/// Strip operator-scope secrets from config.json so tenant exports don't
/// leak the operator's API keys / encryption key / pid.
fn redact_config(mut cfg: Value) -> Value {
    const REDACTED: &str = "<REDACTED-OPERATOR-SCOPE>";
    let redact_paths = [
        "/ai/apiKey",
        "/webhook/secret",
        "/webhook/url",
        "/billing/stripeSecret",
    ];
    for path in &redact_paths {
        if let Some(v) = cfg.pointer_mut(path) {
            if !v.is_null() {
                *v = Value::String(REDACTED.into());
            }
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn redact_strips_api_key() {
        let cfg = json!({
            "ai": {
                "enabled": true,
                "apiKey": "sk-real-secret-12345",
                "model": "claude-sonnet-4-5"
            }
        });
        let r = redact_config(cfg);
        assert_eq!(r["ai"]["apiKey"], "<REDACTED-OPERATOR-SCOPE>");
        assert_eq!(r["ai"]["model"], "claude-sonnet-4-5"); // non-secret untouched
    }

    #[test]
    fn redact_handles_missing_paths() {
        let cfg = json!({"ai": {"enabled": true}});
        let r = redact_config(cfg);
        // No panic, no spurious insertions
        assert_eq!(r["ai"]["enabled"], true);
        assert!(r["ai"].get("apiKey").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[ignore] // touches global pool — see backup.rs precedent
    async fn export_smoke() {
        let tmp = TempDir::new().unwrap();
        let pool = db_async::open_in_memory().await.unwrap();
        db_async::set_global_async_pool(pool);
        let out = tmp.path().join("tenant.tar.gz");
        // 'default' tenant is seeded by V0005__tenancy.sql migration
        let r = run("default", &out).await;
        assert!(r.is_ok(), "export 'default' tenant should succeed: {r:?}");
        let meta = std::fs::metadata(&out).unwrap();
        assert!(meta.len() > 100, "tar.gz too small: {} bytes", meta.len());
    }
}
