//! Ephemeral sandbox semantics — v5.4 (L4.1 done).
//!
//! ## 当前架构 vs. e2b-style ephemeral
//!
//! Per [v2.2 L4.1] 目标：
//! - ✅ **Container per-message** —— `exec::build_sandbox_cmd` 已用 `--rm`，
//!   container 在 claude-cli exit 后立刻销毁
//! - ✅ **Workspace persists per-user** —— `sandbox.work()` 是 host bind
//!   mount，跨消息保留（同 podman named volume 等价）
//! - ⚠️ 本模块补的两块：
//!   1. **Per-message scratch cleanup** —— message 之间清理 `/work/output/`
//!      避免 claude 看到上一轮的临时文件
//!   2. **Global concurrency cap** —— 防 1000 个 inbound 同时 spawn 1000
//!      个 podman 容器把主机打爆
//!
//! ## 跟 ACP 模式的关系
//!
//! - 默认 (`--features acp` off)：每消息 spawn 容器 → 走 [`acquire_spawn_slot`]
//!   semaphore 限制并发
//! - ACP (`--features acp` on)：每用户长跑容器 → semaphore 不在 hot path
//!   （long-running 进程只占 1 slot per user）
//!
//! ## 配置
//!
//! `sandbox.max_concurrent_spawns` (config.json) 默认 = CPU 核数。设 0 = 无限。

use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::{Semaphore, SemaphorePermit};

/// 进程全局并发上限 — 第一次 acquire 时按 config / CPU count 初始化。
static SPAWN_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

/// 默认并发上限：CPU 核数。一台 4 核机器同时跑 4 个 sandbox 是合理的；
/// 第 5 条 inbound 等到前面 release。生产环境可在 config.json 覆盖。
fn default_max_concurrent() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn semaphore() -> &'static Semaphore {
    SPAWN_SEMAPHORE.get_or_init(|| {
        let configured = crate::config::Config::cached().sandbox.max_concurrent_spawns;
        let n = if configured == 0 {
            // 0 = unlimited; sized at usize::MAX 实际上 = 无限 acquire 不阻塞
            usize::MAX
        } else if configured > 0 {
            configured as usize
        } else {
            // -1 or any negative = CPU 核数
            default_max_concurrent()
        };
        tracing::info!("sandbox spawn semaphore initialized with max_concurrent={n}");
        Semaphore::new(n)
    })
}

/// Acquire one container-spawn slot. Caller MUST hold the returned
/// permit for the lifetime of the container (即 spawn → wait → exit).
/// Permit 自动 drop 时归还 slot 给 semaphore。
///
/// 失败仅当 semaphore 被 closed —— 我们不主动 close，这里 unwrap 安全。
///
/// 当 max_concurrent_spawns = 0 (unlimited) 时 acquire 立刻返回不阻塞。
pub async fn acquire_spawn_slot() -> SemaphorePermit<'static> {
    let sem = semaphore();
    // 30s soft timeout — 如果等了 30s 还没拿到 slot，主机已经被打满了，
    // 应当 reject 该 inbound 让 caller 回 "(系统繁忙，请稍后再试)"。
    // 但 Semaphore::acquire 没有 timeout 变体，用 tokio::time::timeout 包。
    match tokio::time::timeout(Duration::from_secs(30), sem.acquire()).await {
        Ok(Ok(p)) => {
            metrics::counter!("weclawbot_sandbox_slot_acquired_total").increment(1);
            p
        }
        Ok(Err(_)) => {
            // Semaphore closed (impossible in current code path) — fall
            // back to a leaked permit so caller doesn't deadlock. Logs.
            tracing::warn!("sandbox semaphore unexpectedly closed");
            metrics::counter!("weclawbot_sandbox_slot_acquired_total").increment(1);
            // 用 forget 模拟一个永远 valid 的 permit（仅 fallback path）
            let leaked: &'static Semaphore = Box::leak(Box::new(Semaphore::new(1)));
            leaked.try_acquire().expect("freshly leaked sem has 1 permit")
        }
        Err(_) => {
            metrics::counter!("weclawbot_sandbox_slot_rejected_total").increment(1);
            tracing::warn!(
                "sandbox spawn semaphore acquire timed out after 30s — host saturated"
            );
            // Force-acquire anyway via try_acquire() ignoring cap — better
            // to be slow than to lose the user message. Operator sees the
            // counter spike in Grafana and bumps max_concurrent_spawns.
            // 这是 fail-open 策略 —— 若 100% 严格上限是 SLO 需求，改 abort。
            sem.try_acquire()
                .or_else(|_| {
                    let leaked: &'static Semaphore = Box::leak(Box::new(Semaphore::new(1)));
                    leaked.try_acquire()
                })
                .expect("semaphore exhausted but try_acquire fallback works")
        }
    }
}

/// 清理 sandbox `/work/output/` 子目录 —— message 之间不让 claude 看到
/// 上一轮临时文件。**不**清理 `/work` 整个目录（用户上传过的文件保留）。
///
/// 失败不致命：仅 log warn，下一轮 claude 顶多看到陈旧文件，不会崩溃。
pub fn scrub_per_message_scratch(sandbox: &super::Sandbox) {
    let output_dir = sandbox.work().join("output");
    if !output_dir.exists() {
        return;
    }
    match std::fs::read_dir(&output_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                let res = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                if let Err(e) = res {
                    tracing::debug!(
                        "scrub_per_message_scratch: rm {} failed: {e}",
                        path.display()
                    );
                }
            }
        }
        Err(e) => {
            tracing::debug!("scrub_per_message_scratch: read_dir {}: {e}", output_dir.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permit_drop_releases_slot() {
        // Smoke test: acquire 一次，drop 后能再 acquire（即同一 slot 复用）
        let p1 = acquire_spawn_slot().await;
        drop(p1);
        let p2 = acquire_spawn_slot().await;
        drop(p2);
        // 没卡死即过
    }

    #[test]
    fn scrub_removes_output_files() {
        let td = TempDir::new().unwrap();
        let sandbox_path = td.path();
        let work_output = sandbox_path.join("sandbox/work/output");
        std::fs::create_dir_all(&work_output).unwrap();
        std::fs::write(work_output.join("a.txt"), b"hi").unwrap();
        std::fs::write(work_output.join("b.txt"), b"bye").unwrap();
        std::fs::create_dir_all(work_output.join("subdir")).unwrap();
        std::fs::write(work_output.join("subdir/c.txt"), b"deep").unwrap();

        let sandbox = super::super::Sandbox {
            user_hash: "u-test12345678".into(),
            user_dir: sandbox_path.to_path_buf(),
        };
        scrub_per_message_scratch(&sandbox);

        // output/ 自己保留，里面清空
        assert!(work_output.exists());
        let remaining: Vec<_> = std::fs::read_dir(&work_output).unwrap().collect();
        assert_eq!(remaining.len(), 0, "expected output/ to be empty");
    }

    #[test]
    fn scrub_missing_output_is_noop() {
        let td = TempDir::new().unwrap();
        let sandbox = super::super::Sandbox {
            user_hash: "u-test12345678".into(),
            user_dir: td.path().to_path_buf(),
        };
        // No sandbox/work/output dir at all — should not panic
        scrub_per_message_scratch(&sandbox);
    }
}
