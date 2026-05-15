//! Application/UseCase layer — v2.2 L3.1.
//!
//! 业务规则的**单一权威实现**。CLI / HTTP / WeChat menu 三个入口都
//! delegate 到这里，保证：
//!
//! - 一处改业务规则 → 三个入口同时更新（之前要改三遍，且容易漏）
//! - 一处插 audit/metric → 三个入口都被记录
//! - 单测只需要测 service 层，不必模拟 axum / clap / wechat 三个 framework
//!
//! ## 边界
//!
//! - Service 接收已经验证过的输入（`UserHash` 而非 raw string）
//! - Service 返回 `Result<T, WeclawError>` —— callers 各自决定怎么呈现
//! - Service **不知道** axum / clap / wechat 存在 —— 纯领域逻辑
//!
//! ## v2.2 进度
//!
//! 本期只搬完 user delete（出现在 3 个入口）。其他多入口业务规则
//! （account add/remove, plugin install, backup snapshot）增量迁移。

pub mod users;
