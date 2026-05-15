//! Runtime utilities — async/sync boundary + supervised task helpers.
//!
//! v7.0: storage 层早已 sqlx async（v4.2），原同步 r2d2 桥接路径全部消失，
//! 只剩 [`blocking::block_on_async`] 让仍是 sync 的 CLI 入口 / sandbox helper
//! 调 async repo。等所有 `cli/*.rs` 也迁完 async 后这个模块可以整删。

pub mod blocking;
pub mod supervised;
