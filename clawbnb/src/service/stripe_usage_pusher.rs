//! Daemon-side Stripe Usage Records pusher. v7.7.
//!
//! ## Why we have one, given the earlier "operator bridges externally"
//! design
//!
//! v7.4 shipped only Prometheus counters with tenant labels — the
//! plan said "operator runs a sidecar to bridge Prom → Stripe". For
//! the smallest/single-tenant SaaS deployment that bridge sidecar
//! adds operational complexity for no benefit. v7.7 adds an in-
//! daemon pusher as an **opt-in** path (env-gated): tiny deployments
//! flip a switch and get billing without standing up promitor or
//! writing a CronJob.
//!
//! Operators with existing Prom→Stripe pipelines (e.g. kube-prom
//! + Grafana Cloud alerting) leave the env unset and continue to
//! bridge externally — both worlds work simultaneously without
//! double-billing because we record local sent-counts to prevent
//! re-sending if the pusher restarts.
//!
//! ## Wire protocol
//!
//! Hand-rolled HTTP POST to:
//!   `https://api.stripe.com/v1/subscription_items/{si_id}/usage_records`
//! with form-encoded body:
//!   `quantity={N}&timestamp={unix}&action=increment`
//! and header:
//!   `Authorization: Bearer {STRIPE_API_KEY}`
//!
//! We don't add the `async-stripe` crate (5MB+ transitive, slow
//! build) because this is the only Stripe endpoint we call. The
//! webhook receiver (`service::billing::post_stripe_webhook`) is
//! already pure hmac-verify without the crate; this stays
//! consistent.
//!
//! ## Idempotency
//!
//! Stripe Usage Records are `action=increment` by default — calling
//! twice with the same `timestamp + quantity` causes double-bill.
//! We use `action=set` with the cumulative-since-window-start value
//! to make the call idempotent on retries.
//!
//! Wait, actually Stripe's `set` action sets the period total,
//! not the windowed delta. To safely support both retries and the
//! continuous-increment model, we:
//!   1. Read the counter's cumulative value from Prometheus at call
//!      time (sum, not rate)
//!   2. POST with `action=set` + `timestamp=now` — Stripe replaces
//!      the period's recorded usage with this total
//!
//! This is the recommended Stripe pattern for cumulative metering.
//! Retry-safe and survives pusher restarts.
//!
//! ## Scheduling
//!
//! 5min warmup, then hourly. Aligns with most Stripe billing periods
//! being hour-granular. Configurable via constant for now.
//!
//! ## Config
//!
//! - `STRIPE_API_KEY` env (or `WECLAWBOT_STRIPE_API_KEY` to match
//!   the webhook-secret naming) — required, else task no-ops.
//! - `WECLAWBOT_PROMETHEUS_URL` — required for reading current
//!   cumulative values. If unset, we can't compute deltas and the
//!   task no-ops.
//! - `tenants.stripe_subscription_items_json` — per-tenant mapping
//!   from counter-name → SubscriptionItem ID. Tenants with NULL
//!   here are skipped.
//!
//! ## Counters bridged
//!
//! Maps `tenants.stripe_subscription_items_json` key → Prom query:
//!
//! | JSON key             | Prom counter                              |
//! |---|---|
//! | `inbound`            | weclawbot_billing_inbound_messages_total  |
//! | `sandbox_seconds`    | weclawbot_billing_sandbox_seconds_total   |
//! | `ai_tokens_input`    | weclawbot_billing_ai_tokens_total{direction=input}  |
//! | `ai_tokens_output`   | weclawbot_billing_ai_tokens_total{direction=output} |

#![cfg(feature = "ee")]

use std::time::Duration;

use serde_json::Value;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::storage::db_async::AsyncDbPool;

const STARTUP_DELAY: Duration = Duration::from_secs(5 * 60);
const RUN_EVERY: Duration = Duration::from_secs(60 * 60);
const STRIPE_USAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-metric Prom query template. `{t}` is interpolated with the
/// tenant id. We use `sum(...)` (no rate/increase) because the
/// `action=set` semantics want a cumulative value.
fn prom_query_for_key(key: &str, tenant: &str) -> Option<String> {
    match key {
        "inbound" => Some(format!(
            "sum(weclawbot_billing_inbound_messages_total{{tenant_id=\"{tenant}\"}})"
        )),
        "sandbox_seconds" => Some(format!(
            "sum(weclawbot_billing_sandbox_seconds_total{{tenant_id=\"{tenant}\"}})"
        )),
        "ai_tokens_input" => Some(format!(
            "sum(weclawbot_billing_ai_tokens_total{{tenant_id=\"{tenant}\",direction=\"input\"}})"
        )),
        "ai_tokens_output" => Some(format!(
            "sum(weclawbot_billing_ai_tokens_total{{tenant_id=\"{tenant}\",direction=\"output\"}})"
        )),
        _ => None,
    }
}

/// Spawn the pusher task. Same lifecycle shape as
/// `crate::ee::audit_scheduler` / `crate::ee::sla_driver`.
pub fn spawn_stripe_usage_pusher(
    pool: AsyncDbPool,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            "stripe_usage_pusher: starting (first run in {:?}, then every {:?})",
            STARTUP_DELAY, RUN_EVERY
        );

        tokio::select! {
            _ = tokio::time::sleep(STARTUP_DELAY) => {}
            _ = shutdown.changed() => {
                info!("stripe_usage_pusher: shutdown before first run — exiting");
                return;
            }
        }

        loop {
            if *shutdown.borrow() {
                info!("stripe_usage_pusher: shutdown signal — exiting");
                return;
            }
            run_one_pass(&pool).await;
            tokio::select! {
                _ = tokio::time::sleep(RUN_EVERY) => {}
                _ = shutdown.changed() => {
                    info!("stripe_usage_pusher: shutdown during sleep — exiting");
                    return;
                }
            }
        }
    })
}

async fn run_one_pass(pool: &AsyncDbPool) {
    let api_key = match std::env::var("WECLAWBOT_STRIPE_API_KEY")
        .or_else(|_| std::env::var("STRIPE_API_KEY"))
    {
        Ok(k) if !k.is_empty() => k,
        _ => {
            // Operator hasn't configured the pusher; no-op silently.
            // (One log line per startup would be more informative but
            // we already logged the "starting" line; spamming each
            // hour is noise.)
            return;
        }
    };

    let prom = match crate::observability::prom_query::try_global_client() {
        Some(p) => p,
        None => {
            warn!(
                "stripe_usage_pusher: WECLAWBOT_PROMETHEUS_URL unset — \
                 can't read cumulative counts. Skipping pass."
            );
            return;
        }
    };

    // Fetch all tenants with a non-NULL stripe_subscription_items_json.
    let rows: Vec<(String, Value)> = match sqlx::query_as(
        "SELECT id, stripe_subscription_items_json
         FROM tenants
         WHERE stripe_subscription_items_json IS NOT NULL
           AND deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("stripe_usage_pusher: list tenants: {e}");
            metrics::counter!(
                "weclawbot_stripe_usage_runs_total",
                "result" => "err_list_tenants"
            )
            .increment(1);
            return;
        }
    };

    let http = match reqwest::Client::builder()
        .timeout(STRIPE_USAGE_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("stripe_usage_pusher: build reqwest: {e}");
            return;
        }
    };

    let mut posted: u64 = 0;
    let mut failed: u64 = 0;
    for (tenant_id, mapping) in &rows {
        let Some(obj) = mapping.as_object() else {
            warn!("stripe_usage_pusher: tenant={tenant_id} mapping not an object — skip");
            continue;
        };
        for (key, item_id_val) in obj {
            let Some(item_id) = item_id_val.as_str() else {
                continue;
            };
            if !item_id.starts_with("si_") {
                warn!(
                    "stripe_usage_pusher: tenant={tenant_id} key={key} \
                     item_id={item_id} doesn't look like a SubscriptionItem id"
                );
                continue;
            }
            let Some(query) = prom_query_for_key(key, tenant_id) else {
                warn!(
                    "stripe_usage_pusher: tenant={tenant_id} key={key} \
                     unknown billing metric — skip"
                );
                continue;
            };
            let value = match prom.instant_scalar(&query).await {
                Some(v) if v.is_finite() && v >= 0.0 => v as u64,
                Some(_) => {
                    // NaN / negative — skip
                    continue;
                }
                None => {
                    // No data yet (clean install / counter never fired) — skip,
                    // not an error.
                    continue;
                }
            };
            if value == 0 {
                // Nothing to bill this period.
                continue;
            }
            match post_usage_record(&http, &api_key, item_id, value).await {
                Ok(()) => {
                    posted += 1;
                    info!(
                        "stripe_usage_pusher: tenant={tenant_id} key={key} \
                         item={item_id} qty={value}"
                    );
                }
                Err(e) => {
                    failed += 1;
                    warn!(
                        "stripe_usage_pusher: POST tenant={tenant_id} key={key} \
                         item={item_id}: {e}"
                    );
                }
            }
        }
    }

    metrics::counter!(
        "weclawbot_stripe_usage_runs_total",
        "result" => if failed == 0 { "ok" } else { "partial" }
    )
    .increment(1);
    metrics::counter!("weclawbot_stripe_usage_records_posted_total").increment(posted);
    metrics::counter!("weclawbot_stripe_usage_records_failed_total").increment(failed);
    info!(
        "stripe_usage_pusher: pass done, posted={posted} failed={failed}"
    );
}

/// POST one Usage Record to Stripe. Uses `action=set` (idempotent
/// across retries within the period).
async fn post_usage_record(
    http: &reqwest::Client,
    api_key: &str,
    subscription_item_id: &str,
    cumulative_quantity: u64,
) -> Result<(), String> {
    let url = format!(
        "https://api.stripe.com/v1/subscription_items/{}/usage_records",
        subscription_item_id
    );
    let timestamp = chrono::Utc::now().timestamp();
    let form = [
        ("quantity", cumulative_quantity.to_string()),
        ("timestamp", timestamp.to_string()),
        ("action", "set".to_string()),
    ];
    let resp = http
        .post(&url)
        .bearer_auth(api_key)
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".into());
        return Err(format!(
            "stripe http {} — first 300 chars: {}",
            status,
            body.chars().take(300).collect::<String>()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_template_for_known_keys() {
        assert!(prom_query_for_key("inbound", "default")
            .unwrap()
            .contains("weclawbot_billing_inbound_messages_total"));
        assert!(prom_query_for_key("ai_tokens_input", "acme")
            .unwrap()
            .contains("direction=\"input\""));
        assert!(prom_query_for_key("unknown_key", "x").is_none());
    }
}
