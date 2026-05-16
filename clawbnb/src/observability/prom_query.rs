//! Minimal Prometheus HTTP query client. v7.6 (Plan M3 SLA phase 2).
//!
//! Used by `crate::ee::sla_driver` to read aggregate metrics that
//! aren't easily derivable from DB rows (rate of failures over the
//! last 5min window, percentile latency from a histogram, etc.).
//! Keeping the client tiny on purpose: only the `instant query`
//! endpoint, return a single scalar, no Series / matrix.
//!
//! ## Why not just scrape /metrics directly?
//!
//! /metrics returns the *current value* of each counter. To compute
//! "rate over last 5 minutes" or "p99 over last 5 minutes" we need a
//! real time-series store — that's what Prometheus is. So this
//! module talks HTTP to an external Prometheus, not to ourselves.
//!
//! ## Configuration
//!
//! Operator sets `WECLAWBOT_PROMETHEUS_URL` to e.g.
//! `http://prometheus.observability.svc.cluster.local:9090` or
//! `http://localhost:9090`. If unset, `try_global_client()` returns
//! `None` and callers fall back to phase-1 metric defaults (0 / n/a).
//!
//! ## Failure mode
//!
//! Query errors (network, 5xx from Prom, bad PromQL) log warn +
//! return None. The SLA aggregator treats None as "phase-2 metric
//! unavailable for this window" and falls back to the DEFAULT
//! values in the table — no row failure.

use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
struct PromResponse {
    status: String,
    data: Option<PromData>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PromData {
    #[serde(rename = "resultType")]
    result_type: String,
    result: serde_json::Value,
}

/// One-shot client construction; caller decides whether to cache.
/// We hand back a configured `reqwest::Client` because our scheduler
/// stack already uses reqwest elsewhere and adding `prometheus-http-
/// query` (the canonical crate) would be a heavy dep for a single
/// `/api/v1/query` call.
pub struct PromClient {
    base_url: String,
    http: reqwest::Client,
}

impl PromClient {
    pub fn new(base_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(QUERY_TIMEOUT)
            .build()
            .expect("reqwest builder");
        Self { base_url, http }
    }

    /// Execute an instant query (`/api/v1/query`). Returns the single
    /// scalar result, or None on any error / empty result.
    ///
    /// `query` is PromQL — caller composes it (e.g.
    /// `rate(weclawbot_inbound_messages_total{tenant_id="acme"}[5m])`).
    pub async fn instant_scalar(&self, query: &str) -> Option<f64> {
        let url = format!("{}/api/v1/query", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("query", query)])
            .send()
            .await
            .map_err(|e| tracing::warn!("prom query http: {e}"))
            .ok()?;
        if !resp.status().is_success() {
            tracing::warn!("prom query http status: {}", resp.status());
            return None;
        }
        let body: PromResponse = resp
            .json()
            .await
            .map_err(|e| tracing::warn!("prom query json: {e}"))
            .ok()?;
        if body.status != "success" {
            tracing::warn!(
                "prom query status={}, error={:?}",
                body.status,
                body.error
            );
            return None;
        }
        let data = body.data?;
        match data.result_type.as_str() {
            // `vector` result has shape: [{metric: {}, value: [t, "0.5"]}, ...]
            "vector" => {
                let arr = data.result.as_array()?;
                let first = arr.first()?.get("value")?.as_array()?;
                let s = first.get(1)?.as_str()?;
                s.parse::<f64>().ok()
            }
            // `scalar` result has shape: [t, "0.5"]
            "scalar" => {
                let arr = data.result.as_array()?;
                let s = arr.get(1)?.as_str()?;
                s.parse::<f64>().ok()
            }
            other => {
                tracing::debug!("prom query: unsupported result type {other}");
                None
            }
        }
    }
}

static GLOBAL_CLIENT: OnceLock<Option<PromClient>> = OnceLock::new();

/// Returns a global, env-configured Prometheus client if
/// `WECLAWBOT_PROMETHEUS_URL` was set at process startup; else None.
///
/// Caching is process-wide and one-shot — operator must restart the
/// daemon to change the Prometheus URL. That matches our config-reload
/// surface (env is set-once, JSON config can hot-reload).
pub fn try_global_client() -> Option<&'static PromClient> {
    GLOBAL_CLIENT
        .get_or_init(|| {
            let url = std::env::var("WECLAWBOT_PROMETHEUS_URL").ok()?;
            if url.is_empty() {
                return None;
            }
            Some(PromClient::new(url))
        })
        .as_ref()
}
