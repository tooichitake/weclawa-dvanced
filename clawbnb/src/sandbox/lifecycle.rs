//! Container lifecycle helpers — v2.2 L3.2.
//!
//! 之前 `delete_user` 只删 DB 行 + `fs::remove_dir_all`，但**没有杀掉**
//! 正在跑的 podman 容器。后果：
//!
//! - 容器继续跑 AI 调用，可能往沙箱 fs 写 → fs::remove_dir_all 半成功
//!   留下乱七八糟的残留
//! - 容器内 claude 还在用旧的 DB pool → 会因 user_hash 行已删 FK 报错
//!   或在 ACP 模式下产生孤儿 history 行
//! - operator 在 GUI 看到"已删除"，但 podman ps 仍能查到该 user 的容器
//!
//! 修复：删 user 前先 kill 该 user 的所有运行中容器。靠的是
//! `sandbox/exec.rs` 给每个容器打的 `--label user=<hash>` 标签。
//!
//! 不可避免的 race：操作员点删 → 我们 kill 容器 → kill 完成前 poller
//! 又给同一 user 起了一个新容器（因为 user_hash 还没 DB-删）。所以
//! 调用顺序必须是：
//!
//! 1. 先在 DB 用 transaction 把 `users.deleted_at` mark 上（pending DELETE
//!    生效前禁止新 spawn）
//! 2. 然后 `kill_user_containers`
//! 3. 然后 cascade DELETE
//! 4. 最后 best-effort `fs::remove_dir_all`
//!
//! v2.2 暂时只做 2/4，v3 加 `deleted_at` mark 时再补 1+3 的事务包装。

use std::process::Stdio;
use tokio::process::Command;

use super::exec::PODMAN_BIN;

/// Kill all currently-running podman containers tagged for this user.
///
/// 在 ACP / per-message 两种模式下都 work —— label 都是
/// `user=<hash>` + `app=weclawbot`，由 `build_sandbox_cmd` 自动加。
///
/// 返回值：成功 kill 的容器 id 列表（可能为空 = 用户没在跑容器）。
/// 错误情况通常是 podman 不在 / 权限不够 —— caller 决定是否容忍。
pub async fn kill_user_containers(user_hash: &str) -> Result<Vec<String>, String> {
    // Step 1: list running containers with this user label.
    let out = Command::new(PODMAN_BIN)
        .arg("ps")
        .arg("--filter")
        .arg(format!("label=user={user_hash}"))
        .arg("--filter")
        .arg("label=app=weclawbot")
        .arg("--format")
        .arg("{{.ID}}")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("podman ps: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "podman ps failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if ids.is_empty() {
        return Ok(vec![]);
    }

    // Step 2: kill them all. Use SIGTERM first (15s timeout) then SIGKILL —
    // podman handles the grace via the `--time` flag (default 10s).
    // `kill` rather than `stop` because we want immediate termination —
    // a delete-user operation is destructive by intent, no graceful shutdown.
    let mut killed = Vec::with_capacity(ids.len());
    for id in &ids {
        let r = Command::new(PODMAN_BIN)
            .arg("kill")
            .arg("--signal=SIGKILL")
            .arg(id)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;
        match r {
            Ok(o) if o.status.success() => {
                killed.push(id.clone());
                tracing::info!("killed container {id} (user={user_hash})");
            }
            Ok(o) => {
                tracing::warn!(
                    "podman kill {id} exit={:?}: {}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(e) => {
                tracing::warn!("podman kill {id}: {e}");
            }
        }
    }
    Ok(killed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kill_with_unknown_user_returns_empty() {
        // 如果 podman 不在 (CI 无沙箱) 会 Err；如果在，肯定查不到这个
        // 编造的 hash，结果应该是空 vec。两种情况都不应 panic。
        match kill_user_containers("u-000000nonexist").await {
            Ok(v) => assert!(v.is_empty(), "got unexpected ids: {v:?}"),
            Err(e) => {
                // podman 缺失不算 lifecycle 模块本身的 bug；只要 error
                // message 形态正常即可。
                assert!(
                    e.contains("podman") || e.contains("ps") || e.contains("No such file"),
                    "unexpected err shape: {e}"
                );
            }
        }
    }
}
