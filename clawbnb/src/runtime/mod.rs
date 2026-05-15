//! Runtime utilities — async/sync boundary helpers (v2.2 L1.1).
//!
//! 我们的 storage 层用 `rusqlite + r2d2`（同步），但 axum handler 是 async。
//! 在 async 里直接 `pool.get()` 会**阻塞 tokio reactor** —— pool 满或 IO
//! 慢都会让别的请求 stall。
//!
//! 解决方案：所有"短而频繁"的 DB / fs / subprocess 调用通过
//! [`blocking::run`] 跳到 `spawn_blocking` 池里跑，async 层只 `.await` 结果。
//! 这是 tokio 官方推荐的同步 IO 桥接模式。
//!
//! v3 路线会把 sqlx async 替换上来；那之前这层是必需的护栏。

pub mod blocking;
pub mod supervised;
