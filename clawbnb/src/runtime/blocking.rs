//! Async wrapper around `tokio::task::spawn_blocking`.
//!
//! 用法：
//!
//! ```ignore
//! use crate::runtime::blocking;
//!
//! async fn axum_handler(State(pool): State<DbPool>) -> Result<Json<Vec<Profile>>, WeclawError> {
//!     let profiles = blocking::run(move || UserRepo::new(&pool).list_profiles()).await??;
//!     //                                                                       ^^  ^^
//!     //                                                                       |   spawn_blocking JoinError
//!     //                                                                       repo Result
//!     Ok(Json(profiles))
//! }
//! ```
//!
//! 直接传 closure 即可；返回类型是 `JoinError` 包裹的 closure 返回值。
//! 如果 closure panic（罕见），`JoinError::is_panic()` 会标记，业务层
//! 可以决定 propagate / 转 internal error。
//!
//! ## 什么时候**不要**用
//!
//! - **超长任务** (> 几百 ms) — `spawn_blocking` 池大小默认 512，被 hog 住
//!   会饿死整个 process。这类工作应该单独 dedicated thread + channel。
//! - **CPU-bound** — 用 `tokio::task::spawn` 跑普通 async + 内部 `block_in_place`
//!   组合，或 rayon。
//!
//! ## 为什么不直接全部 `block_in_place`
//!
//! `block_in_place` 把当前 worker 的 runtime 转交给另一个 worker，spawn cost 更低，
//! 但 **要求当前 thread 是 multi-threaded runtime worker**。我们启动用
//! `#[tokio::main]` 默认 multi-thread，符合条件 —— 但单测里有些用 `current_thread`
//! runtime，那种场景下 `block_in_place` 会 panic。`spawn_blocking` 在两种 runtime
//! 下都 work，所以选它做默认。

use crate::error::WeclawError;

/// Call an async closure from a synchronous context. v4.2: 让保留为
/// sync API 的 helper（defaults.rs / sandbox::ensure / etc）能调 sqlx
/// async repo。
///
/// 走 `tokio::task::block_in_place + Handle::current().block_on`：
/// - 在 multi-thread tokio runtime 内（daemon 主路径）：把当前线程的
///   reactor 控制权交给别的 worker，安全 block 当前线程等异步完成
/// - 在没有 tokio runtime 的 context（极少，CLI 子命令本身已是 async fn）:
///   fall back 起一个一次性 current_thread runtime
pub fn block_on_async<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build oneshot runtime")
            .block_on(fut),
    }
}

/// Run a synchronous closure on the `spawn_blocking` pool and `.await` its
/// result. `JoinError` (closure panic / cancellation) is folded into
/// `WeclawError::Internal` to keep call sites short.
///
/// ```ignore
/// let n: i64 = blocking::run(move || repo.count()).await??;
/// ```
pub async fn run<F, T>(f: F) -> Result<T, WeclawError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        // JoinError = panic in closure OR task aborted. Both are
        // bugs at this level (we don't cancel spawn_blocking tasks
        // anywhere), so report as Internal.
        WeclawError::Internal(format!("blocking task failed: {e}"))
    })
}

/// Convenience: closure that itself returns `Result<T, E>`. Flatten the
/// double-Result so call sites can `?` once.
///
/// ```ignore
/// let user = blocking::try_run(move || repo.get(hash)).await?;
/// ```
pub async fn try_run<F, T, E>(f: F) -> Result<T, WeclawError>
where
    F: FnOnce() -> Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: Into<WeclawError> + Send + 'static,
{
    let inner = run(f).await?;
    inner.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_returns_value() {
        let v: i32 = run(|| 42).await.unwrap();
        assert_eq!(v, 42);
    }

    #[tokio::test]
    async fn try_run_propagates_ok() {
        let v: i32 = try_run(|| Ok::<_, WeclawError>(7)).await.unwrap();
        assert_eq!(v, 7);
    }

    #[tokio::test]
    async fn try_run_propagates_err() {
        let r: Result<i32, _> = try_run(|| {
            Err::<i32, _>(WeclawError::NotFound("xyz".into()))
        })
        .await;
        assert!(matches!(r, Err(WeclawError::NotFound(_))));
    }

    #[tokio::test]
    async fn run_surfaces_panic_as_internal() {
        let r: Result<i32, _> = run(|| panic!("boom")).await;
        assert!(matches!(r, Err(WeclawError::Internal(_))));
    }
}
