//! Periodic audit-log archiver driver — v7.0 H2.
//!
//! Plan M2 (audit retention) 把归档 trait + 实施都写好了
//! ([`crate::ee::audit_retention::AuditArchiver`] +
//! [`crate::ee::audit_archiver::SqliteAuditArchiver`])，但缺一个"daemon
//! 启动时起来、按周期跑、按 tenant 切片"的 driver。本模块就是那一层。
//!
//! ## Lifecycle
//!
//! [`spawn_audit_scheduler`] 在 daemon `cli::start::run` 启动时调一次。
//! 它 spawn 一个 tokio task：
//!
//! 1. 立即 sleep `STARTUP_DELAY` —— 给 daemon 启动期让路（migrations
//!    跑完、pool 初始化、监控起来）
//! 2. 进 loop：每 `RUN_EVERY` 跑一轮 archive，针对每个 active tenant，
//!    cutoff = now - `RETENTION_WINDOW`，调
//!    [`AuditArchiver::archive_before`]
//! 3. 任一 tenant 失败 → log warn 继续下一个（不全盘 abort —— 部分归档
//!    比零归档好）
//! 4. shutdown signal 触发 → 立刻退出 loop（不等下一轮）
//!
//! ## 选参数
//!
//! - `STARTUP_DELAY = 60s` —— daemon 完全就绪之后再开始动数据库，避免
//!   跟 migration / first inbound 抢锁
//! - `RUN_EVERY = 24h` —— audit log 增长慢，日级足够。Cron-style 在午夜
//!   触发对运维更友好但需要时区处理 + 漏跑回补；本期固定间隔够用
//! - `RETENTION_WINDOW = 90 天` —— SOC2 通常要求 90+ 天 hot retention；
//!   HIPAA 要 6 年但允许冷存档。落盘的 .enc 文件就是冷存档载体。
//!   operator 想要更长 hot 窗口可以改 const + rebuild
//!
//! ## 失败语义
//!
//! - DB 不可达 → 单次 run 跳过，下次 run 重试（24h 后）
//! - encrypt key 缺 → archive_before 内部返 Err，scheduler 记 warn
//! - 磁盘满 → archive_before 内部返 Err，scheduler 记 warn；audit_log
//!   表继续累，下次 archive 试再清——硬故障，operator 需要看 metric 介入
//!
//! ## Metric
//!
//! - `weclawbot_audit_archive_runs_total{result}` — counter (ok/err)
//! - `weclawbot_audit_archive_rows_archived_total` — counter (累加 rows_archived)
//! - `weclawbot_audit_archive_bytes_written_total` — counter

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::ee::audit_archiver::SqliteAuditArchiver;
use crate::ee::audit_retention::AuditArchiver;
use crate::storage::db_async::AsyncDbPool;
use crate::tenancy::TenantId;

/// 启动时延迟首次跑的时间。
const STARTUP_DELAY: Duration = Duration::from_secs(60);
/// 跑一轮归档的间隔。
const RUN_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// 保留窗口 —— cutoff = now - 这段。早于 cutoff 的行被归档 + 从主表删。
const RETENTION_WINDOW: chrono::Duration = chrono::Duration::days(90);

/// daemon 启动时调一次。返回 `JoinHandle` 让 caller 可以 `await` shutdown
/// 时归档循环干净退出（不强求 —— spawn 是 fire-and-forget 也安全）。
pub fn spawn_audit_scheduler(
    pool: AsyncDbPool,
    archive_dir: std::path::PathBuf,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            "audit_scheduler: starting, first run in {:?}, then every {:?}, retention = 90d",
            STARTUP_DELAY, RUN_EVERY
        );

        // Initial warm-up sleep — but honor shutdown immediately if it
        // fires during the warmup (no point waiting 60s to discover the
        // operator already wants out).
        tokio::select! {
            _ = tokio::time::sleep(STARTUP_DELAY) => {}
            _ = shutdown.changed() => {
                info!("audit_scheduler: shutdown before first run — exiting");
                return;
            }
        }

        let archiver = Arc::new(SqliteAuditArchiver::new(pool.clone(), archive_dir));

        loop {
            if *shutdown.borrow() {
                info!("audit_scheduler: shutdown signal — exiting");
                return;
            }

            run_one_pass(&pool, archiver.as_ref()).await;

            // Sleep until next run, honoring shutdown.
            tokio::select! {
                _ = tokio::time::sleep(RUN_EVERY) => {}
                _ = shutdown.changed() => {
                    info!("audit_scheduler: shutdown during sleep — exiting");
                    return;
                }
            }
        }
    })
}

/// 一次完整 pass：拉所有 active tenant，每个 tenant 跑一次 archive。
async fn run_one_pass(pool: &AsyncDbPool, archiver: &SqliteAuditArchiver) {
    let cutoff = chrono::Utc::now() - RETENTION_WINDOW;
    let cutoff_rfc = crate::storage::ts::format_rfc3339(&cutoff);

    // Direct SQL: 没专门的 `SqlxTenantRepo::list_active` (yet) —— audit
    // scheduler 是唯一 caller，本地查就够，不为它单加 repo 方法。状态
    // 过滤的逻辑跟 tenants_async::is_active 保持一致（deleted_at 是
    // tombstone）。
    let tenants: Vec<(String,)> = match sqlx::query_as(
        "SELECT id FROM tenants WHERE deleted_at IS NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!("audit_scheduler: list tenants failed: {e} — skipping this pass");
            metrics::counter!(
                "weclawbot_audit_archive_runs_total",
                "result" => "err_list_tenants"
            )
            .increment(1);
            return;
        }
    };

    info!(
        "audit_scheduler: pass start, cutoff = {cutoff_rfc}, tenants = {}",
        tenants.len()
    );

    let mut total_rows: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut ok_count: u32 = 0;
    let mut err_count: u32 = 0;

    for (id,) in tenants {
        let tenant = TenantId::new(id);
        match archiver.archive_before(&tenant, &cutoff_rfc).await {
            Ok(report) => {
                ok_count += 1;
                total_rows += report.rows_archived;
                total_bytes += report.bytes_written;
                if report.rows_archived > 0 {
                    info!(
                        "audit_scheduler: tenant={} rows={} bytes={} file={}",
                        tenant.as_str(),
                        report.rows_archived,
                        report.bytes_written,
                        report.file_path
                    );
                }
            }
            Err(e) => {
                err_count += 1;
                warn!(
                    "audit_scheduler: tenant={} archive failed: {e}",
                    tenant.as_str()
                );
            }
        }
    }

    metrics::counter!(
        "weclawbot_audit_archive_runs_total",
        "result" => if err_count == 0 { "ok" } else { "partial" }
    )
    .increment(1);
    metrics::counter!("weclawbot_audit_archive_rows_archived_total").increment(total_rows);
    metrics::counter!("weclawbot_audit_archive_bytes_written_total").increment(total_bytes);

    info!(
        "audit_scheduler: pass done, ok={ok_count} err={err_count} rows={total_rows} bytes={total_bytes}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    /// Smoke: pool 起来、empty tenants 表（除 default seed）→ run_one_pass
    /// 不 panic，metric tick。
    #[tokio::test]
    async fn empty_tenants_pass_is_noop() {
        let pool = db_async::open_in_memory().await.unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let archiver = SqliteAuditArchiver::new(pool.clone(), tmp.path().to_path_buf());
        // 直接调 run_one_pass —— 不走 spawn_audit_scheduler 的 60s warmup。
        run_one_pass(&pool, &archiver).await;
        // 没 panic 即可。default tenant seed 由 migration V0005 装好，
        // 一次 archive 对空 audit_log 是 no-op。
    }
}
