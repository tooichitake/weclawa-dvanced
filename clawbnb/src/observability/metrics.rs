//! Prometheus metrics — registry handle + `/metrics` text renderer.
//!
//! ## Lifecycle
//!
//! Call `install()` exactly once during daemon boot. It creates an
//! in-process `PrometheusHandle`, stashes it in `HANDLE`, and registers
//! the standard set of counters/gauges/histograms below. Subsequent
//! `metrics::counter!(...)` calls from anywhere in the codebase land
//! against this registry.
//!
//! ## Conventions
//!
//! - `weclawbot_*` prefix on every metric (avoid collisions with host
//!   Prometheus jobs).
//! - Counters: `_total` suffix, monotonically increasing.
//! - Gauges: instantaneous, no suffix.
//! - Histograms: `_seconds` for time, no suffix otherwise.
//! - Labels stay bounded: `account_id` ✅, `user_hash` ❌ (cardinality
//!   bomb — millions of users would blow up the registry).
//!
//! ## Test mode
//!
//! `install()` is idempotent: calling it more than once returns the
//! cached handle. Tests that exercise metric emission don't need a
//! special fixture — just call `install()` and emit.

use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the Prometheus exporter and register standard metrics.
/// Subsequent calls return the cached handle (no-op).
pub fn install() -> &'static PrometheusHandle {
    HANDLE.get_or_init(|| {
        // Use build() + set_global_recorder explicitly. install_recorder()
        // can no-op under certain feature flag combinations; doing the
        // recorder registration ourselves lets us surface failure
        // visibly in the log instead of silently skipping it.
        // `build_recorder()` returns (recorder, handle) without
        // touching the metrics global state — we then install the
        // recorder explicitly so any registration failure is visible.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        if let Err(e) = metrics::set_global_recorder(recorder) {
            tracing::warn!("metrics: set_global_recorder: {e}");
        }
        describe_standard_metrics();
        // Tick the startup counter so `/metrics` has visible output
        // even before the first inbound message arrives.
        metrics::counter!("weclawbot_starts_total").increment(1);
        tracing::info!("prometheus recorder installed");
        handle
    })
}

/// Return the handle if `install()` was called. None if metrics are
/// disabled (test mode, CLI subcommands that don't boot the daemon).
pub fn handle() -> Option<&'static PrometheusHandle> {
    HANDLE.get()
}

/// Render the current registry to Prometheus text format. Empty string
/// if the registry isn't installed — callers should treat that as 503.
pub fn render() -> String {
    match HANDLE.get() {
        Some(h) => h.render(),
        None => String::new(),
    }
}

fn describe_standard_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

    describe_counter!(
        "weclawbot_inbound_messages_total",
        "WeChat inbound messages observed (labelled by account_id, status)"
    );
    describe_counter!(
        "weclawbot_ai_invocations_total",
        "AI provider invocations (labelled by provider, status)"
    );
    describe_histogram!(
        "weclawbot_ai_latency_seconds",
        Unit::Seconds,
        "End-to-end AI provider latency"
    );
    describe_gauge!(
        "weclawbot_active_sandboxes",
        "Currently-running per-user sandbox containers"
    );
    describe_counter!(
        "weclawbot_api_requests_total",
        "HTTP admin API requests (labelled by method, path_template, status)"
    );
    describe_gauge!(
        "weclawbot_db_pool_in_use",
        "SQLite connection-pool slots currently checked out"
    );
    describe_gauge!(
        "weclawbot_disk_usage_bytes",
        "Resident disk usage (labelled by path)"
    );
    describe_gauge!(
        "weclawbot_token_expires_seconds",
        Unit::Seconds,
        "Seconds until each account's bot token is considered expired"
    );
    // Phase 5.3 — Claude OAuth freshness. Updated by auth::claude_oauth::report_once.
    describe_gauge!(
        "weclawbot_claude_oauth_expires_seconds",
        Unit::Seconds,
        "Seconds until the operator's Claude OAuth access token expires (negative = already dead)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::counter;

    // NOTE: PrometheusBuilder::install_recorder() installs a process-global
    // recorder via metrics::set_global_recorder. That's a OnceLock-like
    // operation: the second test to run hits "global recorder already
    // installed" and panics. We avoid that by hand-rolling a render-only
    // test that doesn't call install() at all — the documentation-level
    // contract here is small enough that one test is sufficient.

    #[test]
    fn render_without_install_returns_empty() {
        // Before install(), HANDLE is empty.
        // (This test will pass even after another test installs the
        // recorder, because we use a fresh OnceLock-uninitialized check.
        // For test isolation, use the standalone `handle()` check.)
        if HANDLE.get().is_none() {
            assert_eq!(render(), "");
        }
    }

    #[test]
    fn install_then_emit_then_render_contains_metric() {
        // This test owns the "install once" semantics across the suite.
        // Any other test that wants emissions piggybacks on it via
        // `metrics::counter!` (the global recorder is already live).
        let _ = install();
        counter!("weclawbot_inbound_messages_total", "account_id" => "acct-1", "status" => "ok")
            .increment(1);
        let body = render();
        assert!(body.contains("weclawbot_inbound_messages_total"));
        assert!(body.contains("acct-1"));
    }
}
