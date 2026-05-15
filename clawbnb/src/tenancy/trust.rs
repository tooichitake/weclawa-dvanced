//! 用户行为信任评分 — v3 dynamic policy enforcement.
//!
//! ## 用途
//!
//! 给每个用户（per `user_hash`）滚动计算一个 0.0–1.0 的 trust score。
//! 当 score 低时 daemon 自动收紧限制：
//!
//! - rate limit 上限砍半 → 30 msg/min → 15
//! - tool allowlist 强制缩进到最小集（Read-only）
//! - 沙箱内 podman --memory / --cpus 配额砍半
//! - PII redact 强制开（即便 operator 关了全局 PII）
//!
//! ## 公式
//!
//! 灵感来自 `claude-flow` 项目，参数化的加权和：
//!
//! ```text
//! score = 0.4 * success_rate + 0.2 * uptime + 0.2 * (1 - threat) + 0.2 * integrity
//! ```
//!
//! - `success_rate` ∈ [0,1]：最近 100 条消息里成功完成 AI 调用的比例
//! - `uptime` ∈ [0,1]：用户最近 7 天活跃天数 / 7
//! - `threat` ∈ [0,1]：最近 7 天命中 PII / 黑名单关键词的比例
//! - `integrity` ∈ [0,1]：1 - rate_limit 触发次数 / 总尝试次数
//!
//! 全 1 = perfect score 1.0；全 0 = 0.0。新用户初始 0.5（无数据，中性）。
//!
//! ## 不取代 RBAC
//!
//! Trust score 是**动态**限制（用户行为驱动）。RBAC 是**静态**权限
//! （operator 显式配）。两者叠加：score 收紧的限制不可被 RBAC 解除。

use serde::{Deserialize, Serialize};

/// 单用户 trust score 输入 — 各因子计算出来后传进 [`compute`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrustInputs {
    /// 0.0–1.0
    pub success_rate: f64,
    /// 0.0–1.0
    pub uptime: f64,
    /// 0.0–1.0；high = 多次触发威胁信号
    pub threat: f64,
    /// 0.0–1.0；high = 几乎没触发过 rate limit
    pub integrity: f64,
}

impl TrustInputs {
    /// 新用户默认 — 中性 0.5 trust。Used by the periodic scoring
    /// driver as the starting baseline before observed metrics are
    /// available.
    pub fn neutral() -> Self {
        Self {
            success_rate: 0.5,
            uptime: 0.5,
            threat: 0.5,
            integrity: 0.5,
        }
    }
}

/// Weighted aggregate — 0.0 to 1.0. Clamp inputs to defend against
/// out-of-range observations (a bug in the scoring driver could
/// otherwise produce scores > 1 which downstream tier logic doesn't
/// expect).
///
/// Restored in v7.4 alongside the driver. Formula is the plan-mandated
/// `0.4*success + 0.2*uptime + 0.2*(1-threat) + 0.2*integrity`.
pub fn compute(inputs: &TrustInputs) -> f64 {
    let s = inputs.success_rate.clamp(0.0, 1.0);
    let u = inputs.uptime.clamp(0.0, 1.0);
    let t = inputs.threat.clamp(0.0, 1.0);
    let i = inputs.integrity.clamp(0.0, 1.0);
    let raw = 0.4 * s + 0.2 * u + 0.2 * (1.0 - t) + 0.2 * i;
    raw.clamp(0.0, 1.0)
}

/// 根据 trust 分等级，决定 daemon 应用哪套限制档位。各 tier 的具体限制
/// 数字写在 daemon config，本 enum 只标语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustTier {
    /// trust >= 0.85 — 默认权限
    Trusted,
    /// 0.5 <= trust < 0.85 — 标准
    Standard,
    /// 0.2 <= trust < 0.5 — 收紧（rate limit -50%，tool allowlist 收窄）
    Restricted,
    /// trust < 0.2 — 强收紧（只能 Read，无网络，每分钟 5 条消息上限）
    Quarantined,
}

impl TrustTier {
    pub fn from_score(score: f64) -> Self {
        match score {
            s if s >= 0.85 => Self::Trusted,
            s if s >= 0.5 => Self::Standard,
            s if s >= 0.2 => Self::Restricted,
            _ => Self::Quarantined,
        }
    }

    /// Stable string form for DB column storage. Restored in v7.4
    /// alongside the driver (the column type is TEXT, not the
    /// JSON-derived form, so we need a sync-safe formatter that
    /// doesn't go through serde).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Standard => "standard",
            Self::Restricted => "restricted",
            Self::Quarantined => "quarantined",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_boundaries() {
        assert_eq!(TrustTier::from_score(0.9), TrustTier::Trusted);
        assert_eq!(TrustTier::from_score(0.85), TrustTier::Trusted);
        assert_eq!(TrustTier::from_score(0.84), TrustTier::Standard);
        assert_eq!(TrustTier::from_score(0.5), TrustTier::Standard);
        assert_eq!(TrustTier::from_score(0.49), TrustTier::Restricted);
        assert_eq!(TrustTier::from_score(0.2), TrustTier::Restricted);
        assert_eq!(TrustTier::from_score(0.19), TrustTier::Quarantined);
        assert_eq!(TrustTier::from_score(0.0), TrustTier::Quarantined);
    }
}
