//! Periodic trust scoring driver — Plan M4.1. v7.4.
//!
//! Walks each active user every ~15min, recomputes the four trust
//! input factors from observed metrics in DB, upserts the snapshot.
//! `tool_policy::for_user` then reads the tier overlay live on the
//! next inbound message — low-trust users automatically get the
//! Restricted / Quarantined tier hardening (Bash/Write/WebFetch
//! disallowed even if their per-user settings.json says otherwise).
//!
//! ## Scoring formula (Plan §M4.1)
//!
//! `score = 0.4 * success_rate + 0.2 * uptime + 0.2 * (1 - threat) + 0.2 * integrity`
//!
//! ### Input definitions (this driver's interpretation)
//!
//! - **`success_rate`** ∈ [0,1] — fraction of the user's recent inbound
//!   messages that produced a non-empty assistant reply. Pulled from
//!   `user_history`: count(role='assistant')/count(role='user') in the
//!   last 100 turns. New users (< 5 turns) default to 0.7 (above
//!   neutral) so they don't get Quarantined on day 1.
//!
//! - **`uptime`** ∈ [0,1] — distinct days the user sent at least one
//!   message in the last 7 days, divided by 7. A user active 7/7 days
//!   = 1.0; active only today = 1/7 ≈ 0.14. Captures "regular vs
//!   spiky" usage shape — spiky users are more often attackers
//!   bursting through quota.
//!
//! - **`threat`** ∈ [0,1] — fraction of the user's recent inbounds
//!   that hit a PII Block-policy class. v7.5: wired via
//!   `audit_log` rows with `action='ai_prompt.blocked'` and `target`
//!   = user_hash. The AI provider code (cli_provider + chat) writes
//!   one row per Block-policy hit; the driver queries
//!   `blocked_count / max(total_inbound_count, 1)` clamped to [0,1].
//!
//! - **`integrity`** ∈ [0,1] — `1 - (rate_limit_breaches / total_attempts)`.
//!   v7.5: wired via `audit_log` rows with `action='rate_limit.breach'`
//!   and `target` = user_hash. `monitor::rate_limit::check_inbound`
//!   writes one row each time a user exceeds the per-minute cap.
//!   Driver computes `1.0 - (breaches / max(total_attempts, 1))`.
//!
//! ### Total inbound denominator
//!
//! Both threat and integrity divide by the user's "total inbound
//! attempts" in the window. We use `user_history.role='user'` count
//! as the denominator — same metric as success_rate. This is a slight
//! under-count (some inbounds never get appended to history due to
//! dedup / sandbox failure / rate-limit-itself), but it's
//! conservative and stable across users.
//!
//! ## Scheduling
//!
//! Same pattern as `crate::ee::audit_scheduler`:
//!  - 90s warmup (let migrations / pool / monitors settle)
//!  - then loop: sleep `RUN_EVERY`, sweep all users, upsert
//!  - honors shutdown signal at every await
//!
//! ## Metric
//!
//! - `weclawbot_trust_scoring_runs_total{result}` — counter
//! - `weclawbot_trust_scoring_users_total` — counter (cumulative users scored)
//! - `weclawbot_trust_tier_users{tier}` — gauge (current distribution)

use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::repo::trust::TrustSnapshot;
use crate::repo::trust_async::SqlxTrustRepo;
use crate::storage::db_async::AsyncDbPool;
use crate::tenancy::trust::{compute, TrustInputs, TrustTier};

const STARTUP_DELAY: Duration = Duration::from_secs(90);
const RUN_EVERY: Duration = Duration::from_secs(15 * 60);
/// Users seen in the last N days are scored. Anyone older is
/// considered inactive — their old tier persists but isn't refreshed.
const ACTIVE_WINDOW_DAYS: i64 = 7;

/// Spawn the periodic scoring driver. Called from `cli::start::run`
/// alongside `audit_scheduler::spawn_audit_scheduler`.
pub fn spawn_trust_driver(
    pool: AsyncDbPool,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            "trust_driver: starting, first run in {:?}, then every {:?}",
            STARTUP_DELAY, RUN_EVERY
        );

        tokio::select! {
            _ = tokio::time::sleep(STARTUP_DELAY) => {}
            _ = shutdown.changed() => {
                info!("trust_driver: shutdown before first run — exiting");
                return;
            }
        }

        loop {
            if *shutdown.borrow() {
                info!("trust_driver: shutdown signal — exiting");
                return;
            }

            run_one_pass(&pool).await;

            tokio::select! {
                _ = tokio::time::sleep(RUN_EVERY) => {}
                _ = shutdown.changed() => {
                    info!("trust_driver: shutdown during sleep — exiting");
                    return;
                }
            }
        }
    })
}

/// Run one full scoring pass — sweep every active user, compute,
/// upsert. Errors on individual users don't abort the pass.
async fn run_one_pass(pool: &AsyncDbPool) {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(ACTIVE_WINDOW_DAYS);

    // Active users = those with any history in the last N days. Each
    // user is joined to their tenant via the users table.
    let active: Vec<(String, String)> = match sqlx::query_as(
        "SELECT DISTINCT u.hash, u.tenant_id
         FROM users u
         WHERE EXISTS (
             SELECT 1 FROM user_history h
             WHERE h.user_hash = u.hash AND h.created_at >= $1
         )
         ORDER BY u.hash",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!("trust_driver: list active users failed: {e}");
            metrics::counter!(
                "weclawbot_trust_scoring_runs_total",
                "result" => "err_list_users"
            )
            .increment(1);
            return;
        }
    };

    info!("trust_driver: pass start, {} active users", active.len());

    let repo = SqlxTrustRepo::new(pool.clone());
    let mut scored: u64 = 0;
    let mut tier_counts: std::collections::HashMap<&'static str, u64> =
        std::collections::HashMap::new();

    for (hash, tenant_id) in &active {
        let inputs = match compute_inputs(pool, hash, cutoff).await {
            Ok(i) => i,
            Err(e) => {
                warn!("trust_driver: compute_inputs for {hash}: {e}");
                continue;
            }
        };
        let score = compute(&inputs);
        let tier = TrustTier::from_score(score);
        let now_str = crate::storage::ts::format_rfc3339(&chrono::Utc::now());
        let snap = TrustSnapshot {
            user_hash: hash.clone(),
            tenant_id: tenant_id.clone(),
            inputs,
            score,
            tier,
            updated_at: now_str.clone(),
            tier_since: now_str,
        };
        if let Err(e) = repo.upsert(&snap).await {
            warn!("trust_driver: upsert {hash}: {e}");
            continue;
        }
        scored += 1;
        *tier_counts.entry(tier.as_str()).or_insert(0) += 1;
    }

    metrics::counter!(
        "weclawbot_trust_scoring_runs_total",
        "result" => "ok"
    )
    .increment(1);
    metrics::counter!("weclawbot_trust_scoring_users_total").increment(scored);
    for (tier, count) in &tier_counts {
        metrics::gauge!("weclawbot_trust_tier_users", "tier" => *tier).set(*count as f64);
    }

    info!(
        "trust_driver: pass done, scored={} tiers={:?}",
        scored, tier_counts
    );
}

/// Pull observed metrics from DB + build a `TrustInputs`. See module
/// doc for input definitions and v7.4 stubs.
async fn compute_inputs(
    pool: &AsyncDbPool,
    user_hash: &str,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<TrustInputs, sqlx::Error> {
    // success_rate: assistant turns / user turns over last 100 turns.
    let (user_count, assistant_count): (i64, i64) = sqlx::query_as(
        "WITH recent AS (
             SELECT role FROM user_history
             WHERE user_hash = $1
             ORDER BY id DESC LIMIT 100
         )
         SELECT
             COUNT(*) FILTER (WHERE role = 'user')::BIGINT AS user_n,
             COUNT(*) FILTER (WHERE role = 'assistant')::BIGINT AS assistant_n
         FROM recent",
    )
    .bind(user_hash)
    .fetch_one(pool)
    .await?;
    let success_rate = if user_count < 5 {
        // Newcomer: be generous (above neutral) so they don't get
        // tier-restricted before they've had a chance to be observed.
        0.7
    } else if user_count == 0 {
        1.0
    } else {
        (assistant_count as f64 / user_count as f64).min(1.0)
    };

    // uptime: distinct active days in the window / window_days.
    let (active_days,): (i64,) = sqlx::query_as(
        "SELECT COUNT(DISTINCT DATE(created_at))::BIGINT
         FROM user_history
         WHERE user_hash = $1 AND created_at >= $2",
    )
    .bind(user_hash)
    .bind(cutoff)
    .fetch_one(pool)
    .await?;
    let uptime = (active_days as f64 / ACTIVE_WINDOW_DAYS as f64).min(1.0);

    // threat: v7.5 wired. fraction of recent inbound that hit a
    // Block-policy PII class (audit_log action='ai_prompt.blocked').
    // Both ai/cli_provider and ai/chat write this row.
    let (blocked_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*)::BIGINT FROM audit_log
         WHERE action = 'ai_prompt.blocked'
           AND target = $1
           AND ts >= $2",
    )
    .bind(user_hash)
    .bind(cutoff)
    .fetch_one(pool)
    .await?;
    let threat = if user_count > 0 {
        (blocked_count as f64 / user_count as f64).min(1.0)
    } else {
        0.0
    };

    // integrity: v7.5 wired. 1 - fraction of recent inbound that were
    // throttled by per-user rate limit (audit_log action='rate_limit.breach').
    // monitor/rate_limit writes the row when count > limit.
    let (breach_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*)::BIGINT FROM audit_log
         WHERE action = 'rate_limit.breach'
           AND target = $1
           AND ts >= $2",
    )
    .bind(user_hash)
    .bind(cutoff)
    .fetch_one(pool)
    .await?;
    // Denominator = breaches + accepted (= user_count); 0/0 → 1.0 (perfect).
    let total_attempts = (breach_count + user_count) as f64;
    let integrity = if total_attempts > 0.0 {
        (1.0 - (breach_count as f64 / total_attempts)).clamp(0.0, 1.0)
    } else {
        1.0
    };

    Ok(TrustInputs {
        success_rate,
        uptime,
        threat,
        integrity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    /// Empty users table → pass is a no-op + metric tick.
    #[tokio::test]
    async fn empty_users_pass_is_noop() {
        let pool = db_async::open_in_memory().await.unwrap();
        run_one_pass(&pool).await;
    }

    /// Compute_inputs returns the newcomer baseline (success_rate=0.7,
    /// uptime=0, threat=0, integrity=1) when there's no history.
    #[tokio::test]
    async fn compute_inputs_for_newcomer_baseline() {
        let pool = db_async::open_in_memory().await.unwrap();
        let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
        let inputs = compute_inputs(&pool, "u-newcomer", cutoff).await.unwrap();
        assert_eq!(inputs.success_rate, 0.7);
        assert_eq!(inputs.uptime, 0.0);
        assert_eq!(inputs.threat, 0.0);
        assert_eq!(inputs.integrity, 1.0);
        // score = 0.4*0.7 + 0.2*0.0 + 0.2*1.0 + 0.2*1.0 = 0.68
        let score = compute(&inputs);
        assert!((score - 0.68).abs() < 0.001);
        assert_eq!(TrustTier::from_score(score), TrustTier::Standard);
    }

    /// v7.5 — a user with multiple ai_prompt.blocked audit rows + 0
    /// history rows still has threat=0 (because the user_count
    /// denominator is 0 — they never got past the block to be
    /// recorded in history). This is intentional: brand-new users
    /// hitting Block repeatedly can't be Quarantined on their first
    /// session.
    #[tokio::test]
    async fn threat_zero_when_no_history_even_with_blocks() {
        let pool = db_async::open_in_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO audit_log (ts, action, target, before_json, after_json, ip)
             VALUES (now(), 'ai_prompt.blocked', 'u-attacker', NULL, NULL, NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
        let inputs = compute_inputs(&pool, "u-attacker", cutoff).await.unwrap();
        assert_eq!(inputs.threat, 0.0);
        // Newcomer baseline holds.
        assert_eq!(inputs.success_rate, 0.7);
    }

    /// v7.5 — integrity = breaches / (breaches + history_count).
    /// 3 breaches + 0 history → 3/3 = 1.0 → integrity = 0.0 (worst).
    /// This is the right shape: a brand-new user whose every attempt
    /// is throttled has zero integrity.
    #[tokio::test]
    async fn integrity_zero_when_all_attempts_throttled() {
        let pool = db_async::open_in_memory().await.unwrap();
        for _ in 0..3 {
            sqlx::query(
                "INSERT INTO audit_log (ts, action, target, before_json, after_json, ip)
                 VALUES (now(), 'rate_limit.breach', 'u-spammer', NULL, NULL, NULL)",
            )
            .execute(&pool)
            .await
            .unwrap();
        }
        let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
        let inputs = compute_inputs(&pool, "u-spammer", cutoff).await.unwrap();
        assert_eq!(inputs.integrity, 0.0);
    }
}
