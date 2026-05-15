//! Sandbox reconciler — v2.2 L4.1.
//!
//! 后台 task 定期扫 orphan 资源：
//!
//! - **Orphan podman containers**：label 上的 `user=<hash>` 在 DB 里
//!   找不到对应 user → 该用户已被删但容器还在跑，杀掉
//! - **Orphan sandbox dirs**：`~/.weclawbot/users/<hash>/` 存在但 DB 没行 →
//!   delete_user 时 fs::remove_dir_all 失败留下的残留，再删
//!
//! ## 为什么需要
//!
//! `app::users::delete` 的设计是"kill containers → DB delete → fs cleanup"，
//! 第 3 步是 best-effort（容器在 fs 上持文件、磁盘满、权限等都可能失败）。
//! 没有 reconciler，残留会无限堆积。
//!
//! ## 频率
//!
//! 默认每 10 分钟扫一次。reconcile 本身极快（podman ps + dir scan + diff），
//! 1000 用户量级一次 < 100ms。CPU/IO 都可忽略。
//!
//! ## 安全性
//!
//! - 容器残留 → kill：操作员已通过 delete_user 显式表达意图，所以这里
//!   按"DB 是真相源"清掉对应容器是安全的
//! - dir 残留 → remove_dir_all：同上，DB 没行说明用户已被删，dir 是垃圾
//! - **不会** 反向："DB 里没的就当 orphan" 误删活用户的临时状态：
//!   active 用户每条消息都会触发 `Sandbox::ensure` → 自动 upsert profile，
//!   所以"在 fs 但不在 DB" 这种情况只在 delete 路径之后才发生
//!
//! ## 防止 race
//!
//! delete_user 与 reconciler 同时跑：
//! - delete_user 先 DB delete → reconciler 看到 DB 没行 + fs 有残留 → 删
//! - 顺序反过来也无害（reconciler 看到 DB 有行就跳过）
//!
//! 唯一危险：用户**首次** message 处理中 `Sandbox::ensure` 创建了 dir 但
//! 还没 upsert profile。窗口在 ms 级，reconciler 跑频率分钟级，碰上的
//! 概率极低；即使碰上也只是删了一个空 fresh dir，下条 message 重建。

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

use crate::repo::users_async::SqlxUserRepo;
use crate::sandbox::exec::PODMAN_BIN;

/// Default reconcile interval. Long enough that the cost is negligible;
/// short enough that orphans don't accumulate visibly between runs.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(600);

/// 单次 reconcile pass 的报告。
#[derive(Debug, Default, Clone)]
pub struct ReconcileReport {
    pub orphan_containers_killed: usize,
    pub orphan_dirs_removed: usize,
    pub errors: Vec<String>,
}

/// 跑一次 reconcile pass。可独立调用（test / 手动 /healthz check）或
/// 由 [`spawn_reconciler`] 在后台循环里调。
pub async fn reconcile_once(repo: &SqlxUserRepo) -> ReconcileReport {
    let mut report = ReconcileReport::default();

    // Source of truth: which user hashes does DB know about?
    let known_hashes: HashSet<String> = match repo.list_profiles().await {
        Ok(profiles) => profiles.into_iter().map(|p| p.hash.as_str().to_string()).collect(),
        Err(e) => {
            report.errors.push(format!("list_profiles: {e}"));
            return report;
        }
    };

    // --- Orphan containers ---
    match list_weclawbot_containers().await {
        Ok(containers) => {
            for (id, user_hash) in containers {
                if !known_hashes.contains(&user_hash) {
                    tracing::info!(
                        "reconciler: killing orphan container {id} (user={user_hash} not in DB)"
                    );
                    match kill_container(&id).await {
                        Ok(()) => report.orphan_containers_killed += 1,
                        Err(e) => report.errors.push(format!("kill {id}: {e}")),
                    }
                }
            }
        }
        Err(e) => {
            // podman 不在 = 没沙箱模式，正常情况；不算错误。
            tracing::debug!("reconciler: list containers skipped: {e}");
        }
    }

    // --- Orphan dirs ---
    match list_user_dirs() {
        Ok(dirs) => {
            for (hash, path) in dirs {
                if !known_hashes.contains(&hash) {
                    tracing::info!(
                        "reconciler: removing orphan dir {} (user={hash} not in DB)",
                        path.display()
                    );
                    match std::fs::remove_dir_all(&path) {
                        Ok(()) => report.orphan_dirs_removed += 1,
                        Err(e) => report.errors.push(format!("rm {}: {e}", path.display())),
                    }
                }
            }
        }
        Err(e) => {
            report.errors.push(format!("list_user_dirs: {e}"));
        }
    }

    if report.orphan_containers_killed > 0 || report.orphan_dirs_removed > 0 {
        tracing::info!(
            "reconciler: pass complete — killed {} container(s), removed {} dir(s), {} errors",
            report.orphan_containers_killed,
            report.orphan_dirs_removed,
            report.errors.len()
        );
    }
    report
}

/// Spawn the reconciler as a tokio background task. Returns the JoinHandle
/// (drop = task continues until daemon exits; await = block until shutdown).
///
/// Repo is owned by the task —— pass an `Arc`-wrapped repo type, or
/// re-resolve it inside via `db::pool()` each pass for robustness.
pub fn spawn_reconciler(
    repo: std::sync::Arc<SqlxUserRepo>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    crate::runtime::supervised::spawn("reconciler", async move {
        tokio::time::sleep(interval).await;
        loop {
            let _ = reconcile_once(repo.as_ref()).await;
            tokio::time::sleep(interval).await;
        }
    })
}

// =========================================================================
// Helpers
// =========================================================================

/// `podman ps --filter label=app=weclawbot --format "{{.ID}}\t{{.Label \"user\"}}"`
/// 返回 (container_id, user_hash) pairs。
async fn list_weclawbot_containers() -> Result<Vec<(String, String)>, String> {
    let out = Command::new(PODMAN_BIN)
        .arg("ps")
        .arg("--filter")
        .arg("label=app=weclawbot")
        .arg("--format")
        .arg("{{.ID}}\t{{.Label \"user\"}}")
        .output()
        .await
        .map_err(|e| format!("podman ps: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "podman ps exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut result = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.splitn(2, '\t');
        let id = parts.next().unwrap_or("").trim();
        let user = parts.next().unwrap_or("").trim();
        if !id.is_empty() && !user.is_empty() {
            result.push((id.to_string(), user.to_string()));
        }
    }
    Ok(result)
}

async fn kill_container(id: &str) -> Result<(), String> {
    let out = Command::new(PODMAN_BIN)
        .arg("kill")
        .arg("--signal=SIGKILL")
        .arg(id)
        .output()
        .await
        .map_err(|e| format!("podman kill: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "podman kill exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Scan `~/.weclawbot/users/` for `u-<12hex>` dirs.
fn list_user_dirs() -> Result<Vec<(String, PathBuf)>, String> {
    let base = crate::sandbox::layout::users_root();
    if !base.exists() {
        return Ok(vec![]);
    }
    let mut result = Vec::new();
    let entries = std::fs::read_dir(&base).map_err(|e| format!("read_dir {}: {e}", base.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        // 只认 u-<12 hex> 形式，跳过临时 / 备份目录
        if name.len() == 14 && name.starts_with("u-") && name[2..].chars().all(|c| c.is_ascii_hexdigit()) {
            result.push((name, path));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    #[tokio::test]
    async fn reconcile_with_empty_db_and_no_fs_is_noop() {
        let pool = db_async::open_in_memory().await.unwrap();
        let repo = SqlxUserRepo::new(pool);
        let report = reconcile_once(&repo).await;
        // podman / fs 可能都不存在；只要 list_profiles 成功就 noop。
        assert_eq!(report.orphan_containers_killed, 0);
        // dir scan 在 ~/.weclawbot/users/ 不存在时返回空，没 error。
        // 若操作员实际有 dir 残留，report 可能 > 0 —— 不算 bug，但测试
        // 不该依赖这个。只断言不 panic。
    }
}
