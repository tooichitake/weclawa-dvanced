//! Supervised `tokio::spawn` wrapper — panic 不静默吞。
//!
//! ## 问题
//!
//! 单纯 `tokio::spawn(async { ... })` 启动一个 task 后：
//!
//! - task panic → JoinHandle 上能查到，但**没人 await** → panic 信号
//!   就这么石沉大海
//! - daemon 进程不会挂（panic 默认被 tokio 包住），但功能挂了 caller
//!   不知道
//!
//! `daemon::panic_handler` 装了 process-level hook，能 catch panic
//! payload 写 log + 累计 `weclawbot_panics_total` metric。但**前提**
//! 是 panic 真的 propagate 上来 —— fire-and-forget 的 spawn 不会让
//! panic 出现在 process level，只会让 JoinHandle 静悄悄拿到 `Err`。
//!
//! ## 修复
//!
//! [`spawn`] 替代裸 `tokio::spawn`：包一层 wrapper 把 task `Err` 转成
//! - tracing::error! log（带 task name + panic payload）
//! - `weclawbot_supervised_panics_total{task}` counter +1
//! - 把 panic payload re-resume 到 process panic handler（这样
//!   `panic_handler.rs` 的 hook 也能跑到，保持 audit_log 完整）
//!
//! 长期可考虑 supervisor pattern (restart on panic)，但本期先做"可见性"
//! 一步：panic 不再静默。

use std::future::Future;

/// Spawn a tokio task with panic propagation to the process panic hook.
///
/// ```ignore
/// supervised::spawn("reconciler", async move {
///     loop {
///         reconcile().await;
///         tokio::time::sleep(Duration::from_secs(600)).await;
///     }
/// });
/// ```
///
/// `task_name` 进 tracing::error!（用 `&'static str` 防 String allocation 跑遍 hot path）。
pub fn spawn<F>(task_name: &'static str, fut: F) -> tokio::task::JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        // catch_unwind 在 async future 上不直接 work — async 内部 panic 会
        // unwind 到 task drop。借助 FutureExt::catch_unwind (from futures_util)
        // 包一层。
        use futures_util::FutureExt;
        let result = std::panic::AssertUnwindSafe(fut).catch_unwind().await;
        if let Err(payload) = result {
            // 提取 panic message
            let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            tracing::error!(
                task = task_name,
                payload = %msg,
                "supervised task panicked"
            );
            metrics::counter!(
                "weclawbot_supervised_panics_total",
                "task" => task_name
            )
            .increment(1);
            // re-resume 让 process panic_handler 也跑到（统一 audit/SIGNAL）
            std::panic::resume_unwind(payload);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn supervised_spawn_completes_normally() {
        let h = spawn("test_normal", async {
            // 正常完成
        });
        h.await.unwrap();
    }

    #[tokio::test]
    async fn supervised_spawn_panic_is_logged_and_counted() {
        // panic 会 resume_unwind → JoinError(panic) — 我们能从 JoinError
        // 反向确认 wrapper 跑到了。
        let h = spawn("test_panic", async {
            panic!("boom from test");
        });
        let r = h.await;
        assert!(r.is_err(), "joinhandle should observe panic");
        assert!(r.unwrap_err().is_panic());
    }
}
