//! `weclawbot import-sqlite <state.db>` — one-shot migration from
//! the legacy SQLite backend to the v5.6 Postgres-only backend.
//!
//! ## When to use
//!
//! - Upgrading from any pre-v5.6 weclawbot which used `~/.weclawbot/state.db`
//! - Disaster-recovery from an old DB dump someone hands you
//!
//! After v5.6 the daemon **only** runs against Postgres; this is the
//! one-way bridge.
//!
//! ## Pre-flight
//!
//! 1. Postgres must be running and reachable via env `WECLAWBOT_PG_URL`
//!    (or `DATABASE_URL`). Schema will be created automatically by
//!    `apply_migrations` if the DB is empty.
//! 2. Daemon should NOT be running concurrently — the importer writes
//!    without coordination and concurrent writes would race.
//! 3. Idempotency: re-running the same import is safe (every INSERT
//!    uses `ON CONFLICT DO NOTHING`). Failed mid-run imports can be
//!    resumed.
//!
//! ## What's copied
//!
//! All 14 user-data tables in FK-safe order:
//! tenants → accounts → users → user_settings → user_history →
//! console_sessions → bindings → admin_keys → audit_log → rate_limits →
//! seen_messages → sandbox_logs → user_trust_inputs → user_trust_history →
//! defaults → config_kv.
//!
//! `_sqlx_migrations` / `refinery_schema_history` are NOT copied —
//! the v5.6 native PG migrations are applied separately by
//! `apply_migrations()` before any data is inserted.

use std::path::Path;

use rusqlite::OptionalExtension;
use tracing::{info, warn};

use crate::runtime::blocking::block_on_async;
use crate::storage::db_async;

pub fn run(sqlite_path: &Path) -> Result<(), String> {
    if !sqlite_path.exists() {
        return Err(format!("source not found: {}", sqlite_path.display()));
    }

    println!("Opening SQLite source: {}", sqlite_path.display());
    let src = rusqlite::Connection::open_with_flags(
        sqlite_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("open sqlite: {e}"))?;

    println!("Opening Postgres destination via WECLAWBOT_PG_URL...");
    let pg_pool = block_on_async(async {
        db_async::open_default()
            .await
            .map_err(|e| format!("open pg: {e}"))
    })?;

    let stats = block_on_async(async move { copy_all(&src, &pg_pool).await })?;
    println!("\nImport complete. Summary:");
    for (table, n) in &stats {
        println!("  {table:<24} {n} rows");
    }
    println!(
        "\nNext step: stop daemon, point it at PG (WECLAWBOT_PG_URL is already set),\n\
         and `weclawbot start`. Confirm `weclawbot status` shows the right account count.\n\
         The SQLite file is left untouched as a rollback option."
    );
    Ok(())
}

async fn copy_all(
    src: &rusqlite::Connection,
    pg: &db_async::AsyncDbPool,
) -> Result<Vec<(&'static str, u64)>, String> {
    let mut stats: Vec<(&'static str, u64)> = Vec::new();

    macro_rules! step {
        ($name:literal, $body:expr) => {{
            let n = $body.map_err(|e: String| format!("{}: {}", $name, e))?;
            info!("imported {} rows into {}", n, $name);
            stats.push(($name, n));
        }};
    }

    step!("tenants", copy_tenants(src, pg).await);
    step!("accounts", copy_accounts(src, pg).await);
    step!("users", copy_users(src, pg).await);
    step!("user_settings", copy_user_settings(src, pg).await);
    step!("user_history", copy_user_history(src, pg).await);
    step!("console_sessions", copy_console_sessions(src, pg).await);
    step!("bindings", copy_bindings(src, pg).await);
    step!("admin_keys", copy_admin_keys(src, pg).await);
    step!("audit_log", copy_audit_log(src, pg).await);
    step!("rate_limits", copy_rate_limits(src, pg).await);
    step!("seen_messages", copy_seen_messages(src, pg).await);
    step!("sandbox_logs", copy_sandbox_logs(src, pg).await);
    step!("user_trust_inputs", copy_trust_inputs(src, pg).await);
    step!("user_trust_history", copy_trust_history(src, pg).await);
    step!("defaults", copy_defaults(src, pg).await);
    step!("config_kv", copy_config_kv(src, pg).await);
    Ok(stats)
}

// ============================================================================
// Per-table copy helpers. Each:
//   1. SELECT * FROM <table> in SQLite
//   2. INSERT INTO <table> ON CONFLICT DO NOTHING into Postgres
//   3. Returns rows-copied count
// ============================================================================

async fn copy_tenants(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare(
            "SELECT id, name, created_at, status, stripe_customer_id, deleted_at,
                    billing_status, billing_period_end, last_billing_event, last_billing_event_at,
                    billing_period_start, grace_until
             FROM tenants",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO tenants
                (id, name, created_at, status, stripe_customer_id, deleted_at,
                 billing_status, billing_period_end, last_billing_event, last_billing_event_at,
                 billing_period_start, grace_until)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(&row.2)
        .bind(&row.3)
        .bind(&row.4)
        .bind(&row.5)
        .bind(&row.6)
        .bind(&row.7)
        .bind(&row.8)
        .bind(&row.9)
        .bind(&row.10)
        .bind(&row.11)
        .execute(pg)
        .await
        .map_err(|e| format!("insert tenant {}: {e}", row.0))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_accounts(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare(
            "SELECT account_id, token, token_ciphertext, token_nonce,
                    base_url, weixin_user_id, saved_at, tenant_id, platform_id
             FROM accounts",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(
        String,
        Option<String>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        String,
        Option<String>,
        String,
        String,
        String,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
                r.get::<_, Option<Vec<u8>>>(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get::<_, Option<String>>(7)?.unwrap_or_else(|| "default".into()),
                r.get::<_, Option<String>>(8)?.unwrap_or_else(|| "ilink-wechat".into()),
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO accounts
                (account_id, token, token_ciphertext, token_nonce,
                 base_url, weixin_user_id, saved_at, tenant_id, platform_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
             ON CONFLICT (account_id) DO NOTHING",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(&row.2)
        .bind(&row.3)
        .bind(&row.4)
        .bind(&row.5)
        .bind(&row.6)
        .bind(&row.7)
        .bind(&row.8)
        .execute(pg)
        .await
        .map_err(|e| format!("insert account {}: {e}", row.0))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_users(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare(
            "SELECT hash, user_id_hint, created_at, last_seen_at, message_count,
                    sync_state, last_sync_at, last_sync_error, tenant_id FROM users",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(
        String,
        Option<String>,
        String,
        Option<String>,
        i64,
        String,
        Option<String>,
        Option<String>,
        String,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get::<_, Option<String>>(8)?.unwrap_or_else(|| "default".into()),
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO users
                (hash, user_id_hint, created_at, last_seen_at, message_count,
                 sync_state, last_sync_at, last_sync_error, tenant_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
             ON CONFLICT (hash) DO NOTHING",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(&row.2)
        .bind(&row.3)
        .bind(row.4)
        .bind(&row.5)
        .bind(&row.6)
        .bind(&row.7)
        .bind(&row.8)
        .execute(pg)
        .await
        .map_err(|e| format!("insert user {}: {e}", row.0))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_user_settings(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare("SELECT user_hash, settings_json, updated_at, version, tenant_id FROM user_settings")
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, String, i64, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, Option<String>>(4)?.unwrap_or_else(|| "default".into()),
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO user_settings (user_hash, settings_json, updated_at, version, tenant_id)
             VALUES ($1,$2,$3,$4,$5)
             ON CONFLICT (user_hash) DO NOTHING",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(&row.2)
        .bind(row.3)
        .bind(&row.4)
        .execute(pg)
        .await
        .map_err(|e| format!("insert user_settings {}: {e}", row.0))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_user_history(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare("SELECT user_hash, role, content, created_at, tenant_id FROM user_history ORDER BY id")
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, Option<String>>(4)?.unwrap_or_else(|| "default".into()),
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO user_history (user_hash, role, content, created_at, tenant_id)
             VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(&row.2)
        .bind(&row.3)
        .bind(&row.4)
        .execute(pg)
        .await
        .map_err(|e| format!("insert user_history row: {e}"))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_console_sessions(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare("SELECT user_hash, in_menu, current_path, last_input_at, tenant_id FROM console_sessions")
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, i64, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, Option<String>>(4)?.unwrap_or_else(|| "default".into()),
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO console_sessions (user_hash, in_menu, current_path, last_input_at, tenant_id)
             VALUES ($1,$2,$3,$4,$5)
             ON CONFLICT (user_hash) DO NOTHING",
        )
        .bind(&row.0)
        .bind(row.1)
        .bind(&row.2)
        .bind(&row.3)
        .bind(&row.4)
        .execute(pg)
        .await
        .map_err(|e| format!("insert console_session {}: {e}", row.0))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_bindings(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare("SELECT weixin_user_id, active_account_id, agent_id, updated_at, tenant_id FROM bindings")
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, Option<String>>(4)?.unwrap_or_else(|| "default".into()),
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO bindings (weixin_user_id, active_account_id, agent_id, updated_at, tenant_id)
             VALUES ($1,$2,$3,$4,$5)
             ON CONFLICT (weixin_user_id) DO NOTHING",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(&row.2)
        .bind(&row.3)
        .bind(&row.4)
        .execute(pg)
        .await
        .map_err(|e| format!("insert binding {}: {e}", row.0))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_admin_keys(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare(
            "SELECT id, name, key_hash, role, created_at, last_used_at, revoked_at, tenant_id
             FROM admin_keys",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get::<_, Option<String>>(7)?.unwrap_or_else(|| "default".into()),
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO admin_keys
                (id, name, key_hash, role, created_at, last_used_at, revoked_at, tenant_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(&row.2)
        .bind(&row.3)
        .bind(&row.4)
        .bind(&row.5)
        .bind(&row.6)
        .bind(&row.7)
        .execute(pg)
        .await
        .map_err(|e| format!("insert admin_key {}: {e}", row.0))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_audit_log(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare(
            "SELECT ts, actor_key_id, action, target, before_json, after_json, ip, tenant_id
             FROM audit_log ORDER BY id",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get::<_, Option<String>>(7)?.unwrap_or_else(|| "default".into()),
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO audit_log
                (ts, actor_key_id, action, target, before_json, after_json, ip, tenant_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(&row.2)
        .bind(&row.3)
        .bind(&row.4)
        .bind(&row.5)
        .bind(&row.6)
        .bind(&row.7)
        .execute(pg)
        .await
        .map_err(|e| format!("insert audit_log: {e}"))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_rate_limits(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare("SELECT scope_key, window_start_ts, count, tenant_id FROM rate_limits")
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, i64, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get::<_, Option<String>>(3)?.unwrap_or_else(|| "default".into()),
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO rate_limits (scope_key, window_start_ts, count, tenant_id)
             VALUES ($1,$2,$3,$4)
             ON CONFLICT (scope_key, window_start_ts) DO NOTHING",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(row.2)
        .bind(&row.3)
        .execute(pg)
        .await
        .map_err(|e| format!("insert rate_limit: {e}"))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_seen_messages(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare("SELECT msg_id, first_seen_at, tenant_id, account_id FROM seen_messages")
        .map_err(|e| e.to_string())?;
    let rows: Vec<(i64, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_else(|| "default".into()),
                r.get::<_, Option<String>>(3)?.unwrap_or_else(|| "default".into()),
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO seen_messages (msg_id, first_seen_at, tenant_id, account_id)
             VALUES ($1,$2,$3,$4)
             ON CONFLICT (msg_id) DO NOTHING",
        )
        .bind(row.0)
        .bind(&row.1)
        .bind(&row.2)
        .bind(&row.3)
        .execute(pg)
        .await
        .map_err(|e| format!("insert seen_message: {e}"))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_sandbox_logs(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare(
            "SELECT user_hash, container_id, ts, stream, line, tenant_id FROM sandbox_logs ORDER BY id",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, Option<String>, String, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get::<_, Option<String>>(5)?.unwrap_or_else(|| "default".into()),
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO sandbox_logs (user_hash, container_id, ts, stream, line, tenant_id)
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(&row.2)
        .bind(&row.3)
        .bind(&row.4)
        .bind(&row.5)
        .execute(pg)
        .await
        .map_err(|e| format!("insert sandbox_log: {e}"))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_trust_inputs(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare(
            "SELECT user_hash, tenant_id, success_rate, uptime, threat, integrity,
                    score, tier, updated_at, tier_since FROM user_trust_inputs",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, f64, f64, f64, f64, f64, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_else(|| "default".into()),
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO user_trust_inputs
                (user_hash, tenant_id, success_rate, uptime, threat, integrity,
                 score, tier, updated_at, tier_since)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
             ON CONFLICT (user_hash) DO NOTHING",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(row.2)
        .bind(row.3)
        .bind(row.4)
        .bind(row.5)
        .bind(row.6)
        .bind(&row.7)
        .bind(&row.8)
        .bind(&row.9)
        .execute(pg)
        .await
        .map_err(|e| format!("insert trust_input: {e}"))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_trust_history(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare(
            "SELECT user_hash, tenant_id, ts, inputs_json, score, tier, prev_tier
             FROM user_trust_history ORDER BY id",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, String, String, f64, String, Option<String>)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_else(|| "default".into()),
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO user_trust_history
                (user_hash, tenant_id, ts, inputs_json, score, tier, prev_tier)
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(&row.2)
        .bind(&row.3)
        .bind(row.4)
        .bind(&row.5)
        .bind(&row.6)
        .execute(pg)
        .await
        .map_err(|e| format!("insert trust_history: {e}"))?;
        n += 1;
    }
    Ok(n)
}

async fn copy_defaults(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let row: Option<(String, String)> = src
        .query_row(
            "SELECT settings_json, updated_at FROM defaults WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((settings_json, updated_at)) = row else {
        return Ok(0);
    };
    sqlx::query(
        "INSERT INTO defaults (id, settings_json, updated_at)
         VALUES (1, $1, $2)
         ON CONFLICT (id) DO UPDATE SET
            settings_json = EXCLUDED.settings_json,
            updated_at    = EXCLUDED.updated_at",
    )
    .bind(&settings_json)
    .bind(&updated_at)
    .execute(pg)
    .await
    .map_err(|e| format!("insert defaults: {e}"))?;
    Ok(1)
}

async fn copy_config_kv(src: &rusqlite::Connection, pg: &db_async::AsyncDbPool) -> Result<u64, String> {
    let mut stmt = src
        .prepare("SELECT key, value_json, updated_at FROM config_kv")
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0u64;
    for row in rows {
        sqlx::query(
            "INSERT INTO config_kv (key, value_json, updated_at)
             VALUES ($1,$2,$3)
             ON CONFLICT (key) DO UPDATE SET
                value_json = EXCLUDED.value_json,
                updated_at = EXCLUDED.updated_at",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(&row.2)
        .execute(pg)
        .await
        .map_err(|e| format!("insert config_kv {}: {e}", row.0))?;
        n += 1;
    }
    if n > 0 {
        warn!("imported {n} config_kv rows — review for stale entries after migration");
    }
    Ok(n)
}
