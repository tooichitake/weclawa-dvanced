//! Periodic SLA rollup aggregator — v7.5 (Plan M3.x finalization).
//!
//! Walks active tenants every 5 minutes, computes the SLA window
//! from DB-derivable counters (history rows + audit rows), upserts
//! into `sla_rollup`. Sibling to `audit_scheduler` + `trust_driver`.
//!
//! ## Phase 1 (this commit): DB-derivable metrics only
//!
//! - `error_rate` ∈ [0,1] — `user_history.role='user'` count vs
//!   `role='assistant'` count over the window. `error_rate = 1 -
//!   (assistant / user)` clamped to [0,1]. 0 if there were no inbound
//!   attempts (vacuum: a window with zero traffic has zero observable
//!   errors).
//! - `covered_seconds` — fixed 300 (5min window).
//!
//! ## Phase 2 (v7.6 / follow-up): Prometheus-derived metrics
//!
//! `downtime_seconds` + `latency_p99_ms` require:
//!   1. Per-tenant labels on `weclawbot_api_requests_total{...}` +
//!      `weclawbot_api_request_duration_seconds{...}` (currently no
//!      tenant label — would explode cardinality if we add it blindly,
//!      so it'd be a feature-gated tenant-aware path).
//!   2. A Prometheus HTTP client + scheduled queries `rate(...)` and
//!      `histogram_quantile(0.99, ...)` per-tenant.
//!
//! Until then, both columns are written as 0 (their DEFAULT in
//! V0014). GUI cards should display "n/a" rather than imply 100%
//! uptime when downtime data isn't being collected.
//!
//! ## Scheduling
//!
//! - 30s warmup (shorter than audit/trust because the rollup is
//!   tiny and we want the first window visible quickly)
//! - 5 minute period
//! - Prune rollups older than 90 days on each pass
//! - Honors shutdown signal at every await
//!
//! ## Metrics
//!
//! - `weclawbot_sla_rollup_runs_total{result}`
//! - `weclawbot_sla_rollup_tenants_total` (cumulative tenants rolled up)

use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::storage::db_async::AsyncDbPool;

const STARTUP_DELAY: Duration = Duration::from_secs(30);
const RUN_EVERY: Duration = Duration::from_secs(5 * 60);
const WINDOW_SECONDS: i64 = 300;
const RETENTION_DAYS: i64 = 90;

/// Spawn the SLA rollup driver. Same shape as audit_scheduler /
/// trust_driver. Called from `cli::start::run` under `#[cfg(feature = "ee")]`.
pub fn spawn_sla_driver(
    pool: AsyncDbPool,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            "sla_driver: starting, first run in {:?}, then every {:?}, 90d retention",
            STARTUP_DELAY, RUN_EVERY
        );

        tokio::select! {
            _ = tokio::time::sleep(STARTUP_DELAY) => {}
            _ = shutdown.changed() => {
                info!("sla_driver: shutdown before first run — exiting");
                return;
            }
        }

        loop {
            if *shutdown.borrow() {
                info!("sla_driver: shutdown signal — exiting");
                return;
            }

            run_one_pass(&pool).await;

            tokio::select! {
                _ = tokio::time::sleep(RUN_EVERY) => {}
                _ = shutdown.changed() => {
                    info!("sla_driver: shutdown during sleep — exiting");
                    return;
                }
            }
        }
    })
}

async fn run_one_pass(pool: &AsyncDbPool) {
    let window_end = chrono::Utc::now();
    let window_start = window_end - chrono::Duration::seconds(WINDOW_SECONDS);

    // Tenants that have any history in the window are candidates.
    // Quiet tenants (no inbound) don't get a row — keeps the table
    // sparse for low-usage operators.
    let tenants: Vec<(String,)> = match sqlx::query_as(
        "SELECT DISTINCT u.tenant_id
         FROM users u
         JOIN user_history h ON h.user_hash = u.hash
         WHERE h.created_at >= $1 AND h.created_at < $2
         ORDER BY u.tenant_id",
    )
    .bind(window_start)
    .bind(window_end)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!("sla_driver: list active tenants failed: {e}");
            metrics::counter!(
                "weclawbot_sla_rollup_runs_total",
                "result" => "err_list_tenants"
            )
            .increment(1);
            return;
        }
    };

    if tenants.is_empty() {
        metrics::counter!(
            "weclawbot_sla_rollup_runs_total",
            "result" => "ok_empty"
        )
        .increment(1);
        return;
    }

    let mut rolled: u64 = 0;
    for (tenant_id,) in &tenants {
        if let Err(e) = compute_and_upsert(pool, tenant_id, window_start, window_end).await {
            warn!("sla_driver: tenant={tenant_id} failed: {e}");
            continue;
        }
        rolled += 1;
    }

    // Prune old rollups. Cheap on idx_sla_window_start.
    let prune_cutoff = chrono::Utc::now() - chrono::Duration::days(RETENTION_DAYS);
    let _ = sqlx::query("DELETE FROM sla_rollup WHERE window_start < $1")
        .bind(prune_cutoff)
        .execute(pool)
        .await;

    metrics::counter!(
        "weclawbot_sla_rollup_runs_total",
        "result" => "ok"
    )
    .increment(1);
    metrics::counter!("weclawbot_sla_rollup_tenants_total").increment(rolled);

    info!("sla_driver: pass done, rolled {} tenants", rolled);
}

async fn compute_and_upsert(
    pool: &AsyncDbPool,
    tenant_id: &str,
    window_start: chrono::DateTime<chrono::Utc>,
    window_end: chrono::DateTime<chrono::Utc>,
) -> Result<(), sqlx::Error> {
    // Count inbound (user) + assistant turns inside the window for
    // this tenant. Joining via users table for tenant_id.
    let (user_count, assistant_count): (i64, i64) = sqlx::query_as(
        "SELECT
             COUNT(*) FILTER (WHERE h.role = 'user')::BIGINT      AS user_n,
             COUNT(*) FILTER (WHERE h.role = 'assistant')::BIGINT AS assistant_n
         FROM user_history h
         JOIN users u ON u.hash = h.user_hash
         WHERE u.tenant_id = $1
           AND h.created_at >= $2
           AND h.created_at < $3",
    )
    .bind(tenant_id)
    .bind(window_start)
    .bind(window_end)
    .fetch_one(pool)
    .await?;

    let error_rate = if user_count > 0 {
        let success = (assistant_count as f64 / user_count as f64).clamp(0.0, 1.0);
        1.0 - success
    } else {
        0.0
    };

    // Upsert. window_start is part of the composite PK. Re-running the
    // same pass overwrites with fresh numbers if there's any drift
    // (idempotent: ON CONFLICT DO UPDATE).
    sqlx::query(
        "INSERT INTO sla_rollup
             (tenant_id, window_start, covered_seconds,
              downtime_seconds, latency_p99_ms, error_rate)
         VALUES ($1, $2, $3, 0, 0, $4)
         ON CONFLICT (tenant_id, window_start) DO UPDATE SET
             covered_seconds = excluded.covered_seconds,
             error_rate      = excluded.error_rate",
    )
    .bind(tenant_id)
    .bind(window_start)
    .bind(WINDOW_SECONDS as i32)
    .bind(error_rate)
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    #[tokio::test]
    async fn empty_tenants_pass_is_noop() {
        let pool = db_async::open_in_memory().await.unwrap();
        run_one_pass(&pool).await;
    }

    #[tokio::test]
    async fn rollup_records_error_rate_from_history() {
        let pool = db_async::open_in_memory().await.unwrap();
        // Seed user in default tenant + 4 user turns + 3 assistant turns
        // → success=0.75, error_rate=0.25.
        sqlx::query(
            "INSERT INTO users (hash, user_id_hint, tenant_id, created_at)
             VALUES ('u-aaaaaaaaaaaa', 'wxid_test', 'default', now())",
        )
        .execute(&pool)
        .await
        .unwrap();
        let now = chrono::Utc::now();
        for i in 0..7 {
            let role = if i < 4 { "user" } else { "assistant" };
            sqlx::query(
                "INSERT INTO user_history (user_hash, role, content, created_at)
                 VALUES ('u-aaaaaaaaaaaa', $1, 'x', $2)",
            )
            .bind(role)
            .bind(now - chrono::Duration::seconds(i as i64))
            .execute(&pool)
            .await
            .unwrap();
        }
        let window_end = now + chrono::Duration::seconds(1);
        let window_start = window_end - chrono::Duration::seconds(WINDOW_SECONDS);
        compute_and_upsert(&pool, "default", window_start, window_end)
            .await
            .unwrap();
        let (er,): (f64,) = sqlx::query_as(
            "SELECT error_rate FROM sla_rollup WHERE tenant_id = 'default' LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!((er - 0.25).abs() < 1e-9, "expected 0.25, got {er}");
    }
}
