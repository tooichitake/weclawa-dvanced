//! Long-term audit retention — v3 enterprise feature.
//!
//! ## 用途
//!
//! HIPAA / SOC2 等合规要求 audit log 持 6 年 / 7 年。当前主线只在
//! `audit_log` 表无限累积，单 SQLite 文件不适合多年规模（GB 量级 +
//! 索引 IO）。
//!
//! enterprise mode 下 `audit_retention::archive_old(cutoff)` 把
//! cutoff 之前的行：
//! 1. 流式导出到 newline-delimited JSON (`~/.weclawbot/archive/audit-YYYY-MM.jsonl.zst`)
//! 2. 按 tenant 分文件落盘（防止合规审计时跨 tenant 混读）
//! 3. SHA-256 校验和写 `archive_manifest.json`
//! 4. 从 `audit_log` 表 DELETE
//!
//! 后续合规审计要查老数据 → operator 解压对应文件 → grep 即可。
//!
//! ## 加密
//!
//! 落盘前用 `crate::storage::crypto` 的 AES-256-GCM 包一层，key 同
//! token-at-rest 那把（WECLAWBOT_DB_KEY env）。压缩在加密之前
//! （加密后熵高，zstd 压缩比骤降）。
//!
//! ## v3 占位
//!
//! Trait shape 就位，真 archive 逻辑 v3.1 实施。

use async_trait::async_trait;

use crate::error::WeclawError;
use crate::tenancy::TenantId;

/// 归档结果摘要 — 通过 admin API 返给操作员检查"这次跑了多少行"。
#[derive(Debug, Clone)]
pub struct ArchiveReport {
    pub tenant_id: TenantId,
    pub rows_archived: u64,
    pub bytes_written: u64,
    pub file_path: String,
    pub sha256: String,
}

#[async_trait]
pub trait AuditArchiver: Send + Sync {
    /// 把 `cutoff_rfc3339` 之前的 audit_log 行打包落地 + 从主表删。
    /// idempotent — 跨多次调用同一 cutoff 不会重复写文件（用 sha256
    /// 做 dedup）。
    async fn archive_before(
        &self,
        tenant_id: &TenantId,
        cutoff_rfc3339: &str,
    ) -> Result<ArchiveReport, WeclawError>;

    /// 列出该 tenant 历史归档清单 — GUI 合规面板列表用。
    async fn list_archives(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<ArchiveReport>, WeclawError>;
}

// v3.1 增量 PR 真实现；当前仅 trait shape。
