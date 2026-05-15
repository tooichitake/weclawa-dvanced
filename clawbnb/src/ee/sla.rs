//! SLA monitoring — types-only (v7.3 status: aggregator not yet wired).
//!
//! ## 用途
//!
//! Hosted SaaS 客户合同里通常有 SLA：例如"99.5% 可用性 / p99 inbound→reply
//! < 30s"。这块从 Prometheus metrics + audit_log 滚动窗口聚合算
//! tenant-scoped 数字，写到 `sla_rollup` 表（schema 待加），供
//! - GUI SLA dashboard 卡片
//! - Stripe credit 计算
//! - 客户合规报表导出
//!
//! ## 实现状态 (v7.3)
//!
//! - **types** ([`SlaWindow`], [`SlaTarget`]) — stable, importable now
//! - **aggregator** — not yet wired. Two pieces still needed:
//!     1. migration `V0014__sla_rollup.sql` with columns matching
//!        `SlaWindow` (tenant_id / window_start / covered_seconds /
//!        downtime_seconds / latency_p99_ms / error_rate)
//!     2. scheduler task (similar shape to
//!        [`crate::ee::audit_scheduler`]) that runs every 5min, queries
//!        Prometheus via its HTTP API (`/api/v1/query_range`), computes
//!        per-tenant rollups, INSERTs into `sla_rollup`
//!
//! Both are mechanical follow-up work; the types here are the stable
//! contract the aggregator + GUI cards will share.
//!
//! ## 计算口径
//!
//! - **uptime** = (covered_seconds - downtime_seconds) / covered_seconds
//!     - downtime = poller 报 401/5xx > 60s 连续段
//! - **latency_p99** = 滚动 5-min histogram quantile
//! - **error_rate** = `weclawbot_ai_invocations_total{status="error"}` /
//!   total，连续 1 小时窗口

use serde::{Deserialize, Serialize};

use crate::tenancy::TenantId;

/// 单 tenant 单个 5 分钟窗口的 SLA 滚动数字。落 `sla_rollup` 表，列
/// 与本 struct 1:1 对应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlaWindow {
    pub tenant_id: TenantId,
    /// 窗口起点 (RFC 3339, e.g. "2026-05-15T03:25:00Z")。
    pub window_start: String,
    /// 该窗口内被监控的秒数（标准 300）。
    pub covered_seconds: u32,
    /// 期间观察到的 downtime 秒数。
    pub downtime_seconds: u32,
    /// p99 inbound→reply 延迟，单位毫秒。
    pub latency_p99_ms: u32,
    /// AI invocation 错误率 0.0–1.0。
    pub error_rate: f64,
}

impl SlaWindow {
    /// 折算 uptime 比率（0.0–1.0）。
    pub fn uptime(&self) -> f64 {
        if self.covered_seconds == 0 {
            return 1.0;
        }
        1.0 - (self.downtime_seconds as f64 / self.covered_seconds as f64)
    }
}

/// SLA 阈值配置（per-tenant，从 tenants.sla_target_json 加载）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlaTarget {
    /// e.g. 0.995 = 99.5%
    pub uptime_target: f64,
    /// e.g. 30000 ms = 30 s
    pub latency_p99_target_ms: u32,
}

impl Default for SlaTarget {
    fn default() -> Self {
        Self {
            uptime_target: 0.995,
            latency_p99_target_ms: 30_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_perfect() {
        let w = SlaWindow {
            tenant_id: TenantId::default_tenant(),
            window_start: "2026-05-15T00:00:00Z".into(),
            covered_seconds: 300,
            downtime_seconds: 0,
            latency_p99_ms: 1500,
            error_rate: 0.0,
        };
        assert!((w.uptime() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn uptime_50_percent() {
        let w = SlaWindow {
            tenant_id: TenantId::default_tenant(),
            window_start: "2026-05-15T00:00:00Z".into(),
            covered_seconds: 300,
            downtime_seconds: 150,
            latency_p99_ms: 1500,
            error_rate: 0.0,
        };
        assert!((w.uptime() - 0.5).abs() < f64::EPSILON);
    }
}
