//! `weclawbot backup` / `weclawbot restore` — online snapshots (v5.3 cfg-gated).
//!
//! - **SQLite** (default)：`VACUUM INTO 'path'` 走 sqlx，生成自包含 `.db`
//! - **Postgres** (`--features postgres`)：调外部 `pg_dump` 命令生成
//!   `.sql` text dump（PG 没在线快照内建子句，只能走 pg_dump / pg_basebackup）
//!
//! Restore is offline-only (daemon must be stopped) — SQLite 是文件 copy；
//! Postgres 是 `psql -f snapshot.sql`。

use std::path::Path;

#[cfg(not(feature = "postgres"))]
use crate::runtime::blocking::block_on_async;
use crate::storage::db;
#[cfg(not(feature = "postgres"))]
use crate::storage::db_async;

#[cfg(not(feature = "postgres"))]
pub fn backup(out_path: &Path) -> Result<(), String> {
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
    }

    let out_str = out_path
        .to_str()
        .ok_or_else(|| "output path is not valid UTF-8".to_string())?;
    // SQL-quote the path. Single-quote doubling is the SQL standard form.
    let escaped = out_str.replace('\'', "''");
    let sql = format!("VACUUM INTO '{escaped}'");

    let out_path_owned = out_path.to_path_buf();
    block_on_async(async move {
        let pool = match db_async::try_global_async_pool() {
            Some(p) => p,
            None => db_async::open_default()
                .await
                .map_err(|e| format!("open state.db: {e}"))?,
        };
        sqlx::query(&sql)
            .execute(&pool)
            .await
            .map_err(|e| format!("vacuum into {}: {e}", out_path_owned.display()))?;
        Ok::<_, String>(())
    })?;

    println!("Backup written to {}", out_path.display());
    Ok(())
}

/// Postgres backup —— 调外部 `pg_dump` 命令。需 env `WECLAWBOT_PG_URL`
/// (postgres://user:pass@host/db) 在 PATH 找到 `pg_dump` binary。
///
/// 输出是文本 SQL dump（plain format），restore 用 `psql -f`。
#[cfg(feature = "postgres")]
pub fn backup(out_path: &Path) -> Result<(), String> {
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
    }
    let pg_url = std::env::var("WECLAWBOT_PG_URL")
        .map_err(|_| "WECLAWBOT_PG_URL not set (postgres backup requires it)".to_string())?;
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
        .map_err(|e| format!("spawn pg_dump (is it on PATH?): {e}"))?;
    if !status.success() {
        return Err(format!("pg_dump exited with {status}"));
    }
    println!("Postgres backup written to {}", out_path.display());
    Ok(())
}

#[cfg(not(feature = "postgres"))]
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
    let target = db::default_path();
    if target.exists() {
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let aside = target.with_extension(format!("db.pre-restore-{stamp}"));
        std::fs::rename(&target, &aside).map_err(|e| {
            format!("move existing {} aside: {e}", target.display())
        })?;
        println!("Existing state.db moved to {}", aside.display());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::copy(snapshot_path, &target).map_err(|e| {
        format!("copy {} -> {}: {e}", snapshot_path.display(), target.display())
    })?;
    println!("Restored {} from {}", target.display(), snapshot_path.display());
    Ok(())
}

/// Postgres restore —— 走 `psql -f snapshot.sql`。daemon 必须 stop。
/// 调用者负责先 drop / recreate schema（pg_dump plain 默认 INSERT not
/// drop-create；如需 reset 加 `--clean --if-exists` 到 backup 端）。
#[cfg(feature = "postgres")]
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
    let pg_url = std::env::var("WECLAWBOT_PG_URL")
        .map_err(|_| "WECLAWBOT_PG_URL not set (postgres restore requires it)".to_string())?;
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
        .map_err(|e| format!("spawn psql (is it on PATH?): {e}"))?;
    if !status.success() {
        return Err(format!("psql exited with {status}"));
    }
    println!("Postgres restored from {}", snapshot_path.display());
    Ok(())
}

#[cfg(all(test, not(feature = "postgres")))]
mod tests {
    use super::*;
    use crate::storage::db_async;

    // v4.2: 全局 OnceLock pool 在测试间共享 → 多测试并行时这个测试
    // race 拿不到自己设的 pool。集成测试用 isolated 实例覆盖此能力；
    // 单测留 ignore 避免 flaky CI。
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[ignore]
    async fn backup_writes_snapshot_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("state.db");
        let pool = db_async::open_at(&src).await.unwrap();
        db_async::set_global_async_pool(pool);
        let out = tmp.path().join("snap.db");
        backup(&out).unwrap();
        assert!(out.exists());
        let meta = std::fs::metadata(&out).unwrap();
        assert!(meta.len() > 100, "snapshot too small");
    }
}
