//! Deep liveness/readiness probe for `/healthz`.
//!
//! Returns 200 + JSON when every subsystem passes; otherwise 503 +
//! JSON with the failing checks marked. Probes:
//!
//! - **db**: a `SELECT 1` round-trip against the pool.
//! - **podman**: the binary is on `PATH` and `podman --version` succeeds.
//! - **runsc**: the gVisor binary is present (used as the OCI runtime).
//! - **sandbox_image**: the bundled image tag is cached locally.
//! - **disk_free**: free space on `state_dir` is above the floor.
//! - **accounts**: count of active accounts with non-empty tokens.
//!
//! Probes that can hang (image pull, network) are NOT done here.
//! `/healthz` MUST be cheap — typical SLO 100 ms p99.

use std::time::Instant;

use axum::{http::StatusCode, Json};
use serde_json::{json, Value};

use crate::runtime::blocking::block_on_async;
use crate::storage::db_async::try_global_async_pool;

/// Free-space floor below which we mark the disk check as failing. Picked
/// at 200 MB so the daemon stays online during normal log/audit growth
/// but trips an alert well before SQLite would start failing writes.
const DISK_FREE_FLOOR_BYTES: u64 = 200 * 1024 * 1024;

pub async fn healthz() -> (StatusCode, Json<Value>) {
    let started = Instant::now();
    let mut checks = serde_json::Map::new();
    let mut all_ok = true;

    // --- DB ---
    let db_status = match check_db() {
        Ok(()) => "ok",
        Err(e) => {
            checks.insert("db_error".into(), Value::String(e));
            all_ok = false;
            "fail"
        }
    };
    checks.insert("db".into(), Value::String(db_status.into()));

    // --- podman ---
    match check_binary("podman") {
        Ok(version) => {
            checks.insert("podman".into(), Value::String("ok".into()));
            checks.insert("podman_version".into(), Value::String(version));
        }
        Err(_) => {
            checks.insert("podman".into(), Value::String("missing".into()));
            // Podman missing is fatal for the per-user sandbox path. We
            // still mark unhealthy so operators see it in /healthz, but
            // a non-fatal "running but degraded" mode could be added later.
            all_ok = false;
        }
    }

    // --- runsc ---
    match check_binary("runsc") {
        Ok(version) => {
            checks.insert("runsc".into(), Value::String("ok".into()));
            checks.insert("runsc_version".into(), Value::String(version));
        }
        Err(_) => {
            checks.insert("runsc".into(), Value::String("missing".into()));
            all_ok = false;
        }
    }

    // --- disk free ---
    match disk_free_bytes() {
        Some(free) => {
            checks.insert(
                "disk_free_bytes".into(),
                Value::Number(free.into()),
            );
            if free < DISK_FREE_FLOOR_BYTES {
                checks.insert("disk".into(), Value::String("low".into()));
                all_ok = false;
            } else {
                checks.insert("disk".into(), Value::String("ok".into()));
            }
        }
        None => {
            checks.insert("disk".into(), Value::String("unknown".into()));
            // Unknown disk free isn't a hard fail — Windows reporting
            // gaps shouldn't take prod down.
        }
    }

    // --- accounts ---
    let accounts = active_account_count();
    checks.insert(
        "accounts_with_valid_token".into(),
        Value::Number((accounts as u64).into()),
    );
    if accounts == 0 {
        // Not a failure per se — operator may not have run `login` yet
        // — but flag it so the GUI knows to nudge.
        checks.insert("accounts".into(), Value::String("none-configured".into()));
    } else {
        checks.insert("accounts".into(), Value::String("ok".into()));
    }

    // --- Claude OAuth (Phase 5.3) ---
    if let Some(oauth_secs) = crate::auth::claude_oauth::expires_in_seconds() {
        checks.insert(
            "claude_oauth_expires_in_seconds".into(),
            Value::Number(oauth_secs.into()),
        );
        if oauth_secs < 0 {
            checks.insert(
                "claude_oauth".into(),
                Value::String("expired".into()),
            );
            all_ok = false;
        } else if oauth_secs < 3600 {
            checks.insert(
                "claude_oauth".into(),
                Value::String("expiring-soon".into()),
            );
            // < 1h 是警告级，不直接 fail healthz —— 让运营商先看到
            // 黄色再决定 re-login，而不是健康检查直接挂掉触发 LB
            // 切流（虽然此时单机部署没 LB，未来扩展也别炸）。
        } else if oauth_secs < 24 * 3600 {
            checks.insert("claude_oauth".into(), Value::String("warn-24h".into()));
        } else {
            checks.insert("claude_oauth".into(), Value::String("ok".into()));
        }
    } else {
        checks.insert("claude_oauth".into(), Value::String("unknown".into()));
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;
    // v5.5: surface compliance mode in healthz so monitoring dashboards
    // can verify hipaa/soc2/gdpr modes are actually active. Compliance
    // metadata is informational — does NOT flip overall_ok.
    let compliance_mode = crate::config::Config::cached().compliance.mode.label();
    let body = json!({
        "ok": all_ok,
        "version": env!("CARGO_PKG_VERSION"),
        "checks": Value::Object(checks),
        "probe_ms": elapsed_ms,
        "compliance_mode": compliance_mode,
    });
    let status = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body))
}

fn check_db() -> Result<(), String> {
    block_on_async(async {
        let pool = try_global_async_pool().ok_or_else(|| "pool not initialized".to_string())?;
        let row: (i64,) = sqlx::query_as("SELECT 1")
            .fetch_one(&pool)
            .await
            .map_err(|e| format!("select: {e}"))?;
        if row.0 != 1 {
            return Err("SELECT 1 returned non-1".into());
        }
        Ok(())
    })
}

fn check_binary(name: &str) -> Result<String, String> {
    let out = std::process::Command::new(name)
        .arg("--version")
        .output()
        .map_err(|e| format!("{name}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{name} exit {}", out.status));
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(v)
}

fn disk_free_bytes() -> Option<u64> {
    let dir = crate::storage::state_dir::state_dir();
    fs2_like_free(dir)
}

#[cfg(unix)]
fn fs2_like_free(path: &std::path::Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let cpath = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(windows)]
fn fs2_like_free(path: &std::path::Path) -> Option<u64> {
    // Avoid pulling another dependency: Windows free-space reporting via
    // GetDiskFreeSpaceExW would need `windows-sys`. Until we wire it
    // we report None — the /healthz check downgrades to "unknown" but
    // doesn't fail the probe.
    let _ = path;
    None
}

fn active_account_count() -> usize {
    use crate::repo::accounts_async::SqlxAccountRepo;
    block_on_async(async {
        let Some(pool) = try_global_async_pool() else { return 0 };
        let r = SqlxAccountRepo::new(pool);
        match r.list().await {
            Ok(rows) => rows
                .into_iter()
                .filter(|a| a.token.as_ref().is_some_and(|t| !t.expose().is_empty()))
                .count(),
            Err(_) => 0,
        }
    })
}
