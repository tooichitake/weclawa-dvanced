//! `weclawbot backup` / `weclawbot restore` — online SQLite snapshots (v4.2 sqlx).
//!
//! v4.2: 走 sqlx 的 `VACUUM INTO 'path'` 子句生成自包含 `.db` 文件。
//! Restore is offline-only (daemon must be stopped).

use std::path::Path;

use crate::runtime::blocking::block_on_async;
use crate::storage::db;
use crate::storage::db_async;

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

#[cfg(test)]
mod tests {
    use super::*;

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
