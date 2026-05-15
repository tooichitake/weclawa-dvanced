//! `weclawbot backup` / `weclawbot restore` — v5.6 PG-only.
//!
//! Both commands shell out to standard Postgres tooling:
//! - **backup**: `pg_dump --format=plain --no-owner --no-privileges`
//! - **restore**: `psql --single-transaction -f <snapshot>`
//!
//! Reads DSN from env `WECLAWBOT_PG_URL` (or `DATABASE_URL`). Caller
//! must have `pg_dump` and `psql` on `PATH` (`apt install postgresql-client`).
//!
//! ## Online vs offline
//!
//! - `backup` is online — daemon can keep running. pg_dump takes an
//!   ACCESS SHARE lock, doesn't block writes.
//! - `restore` is offline — daemon MUST be stopped first because we
//!   may need to drop/recreate the schema or replay constraints that
//!   conflict with live data.

use std::path::Path;

fn dsn() -> Result<String, String> {
    std::env::var("WECLAWBOT_PG_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .map_err(|_| {
            "neither WECLAWBOT_PG_URL nor DATABASE_URL set (backup needs a Postgres DSN)".into()
        })
}

pub fn backup(out_path: &Path) -> Result<(), String> {
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
    }
    let pg_url = dsn()?;
    let out_str = out_path
        .to_str()
        .ok_or_else(|| "output path is not valid UTF-8".to_string())?;
    let status = std::process::Command::new("pg_dump")
        .arg("--dbname")
        .arg(&pg_url)
        .arg("--format=plain")
        .arg("--no-owner")
        .arg("--no-privileges")
        .arg("--file")
        .arg(out_str)
        .status()
        .map_err(|e| format!("spawn pg_dump (is postgresql-client installed?): {e}"))?;
    if !status.success() {
        return Err(format!("pg_dump exited with {status}"));
    }
    println!("Backup written to {}", out_path.display());
    Ok(())
}

pub fn restore(snapshot_path: &Path) -> Result<(), String> {
    if let Some(pid) = crate::daemon::pid::read_pid() {
        if crate::daemon::pid::is_process_alive(pid) {
            return Err(format!(
                "weclawbot is running (pid {pid}); stop it before restore"
            ));
        }
    }
    if !snapshot_path.exists() {
        return Err(format!("snapshot not found: {}", snapshot_path.display()));
    }
    let pg_url = dsn()?;
    let snap_str = snapshot_path
        .to_str()
        .ok_or_else(|| "snapshot path is not valid UTF-8".to_string())?;
    let status = std::process::Command::new("psql")
        .arg("--dbname")
        .arg(&pg_url)
        .arg("--file")
        .arg(snap_str)
        .arg("--single-transaction")
        .status()
        .map_err(|e| format!("spawn psql (is postgresql-client installed?): {e}"))?;
    if !status.success() {
        return Err(format!("psql exited with {status}"));
    }
    println!("Restored from {}", snapshot_path.display());
    Ok(())
}
