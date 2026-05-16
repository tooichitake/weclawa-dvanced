//! Stripe usage metering — counter emission only. v7.4 (Plan M3.2).
//!
//! ## Scope
//!
//! Emit per-tenant counters that an external bridge (Prometheus →
//! Stripe Usage Records API) consumes to invoice metered customers.
//! We don't build the bridge inside the daemon — that would require:
//!
//!   - the Stripe API client crate (`async-stripe`, ~80MB transitive
//!     dep), or
//!   - hand-rolled HTTP to `https://api.stripe.com/v1/subscription_items/
//!     <id>/usage_records`, with credential storage
//!
//! Neither belongs in the daemon. Operators run a small sidecar
//! container (or scheduled job) that:
//!   1. queries Prometheus `increase(weclawbot_billing_inbound_messages_total[1h])`
//!      grouped by `tenant_id` once per hour
//!   2. POSTs each result to Stripe Usage Records
//!
//! This pattern keeps daemon simple + lets operators reuse existing
//! Prometheus → Stripe bridges (e.g. promitor, custom k8s CronJob).
//!
//! ## What we count
//!
//! - `weclawbot_billing_inbound_messages_total{tenant_id, platform}` —
//!   counter, +1 per ack'd inbound (post-dedup). Bill at per-message
//!   pricing tier.
//! - `weclawbot_billing_sandbox_seconds_total{tenant_id}` — counter,
//!   accumulates per-sandbox active seconds. Bill at per-second
//!   compute pricing.
//! - `weclawbot_billing_ai_tokens_total{tenant_id, provider, direction}` —
//!   counter, AI token counts (input/output separately). Bill at
//!   per-1k-tokens pricing for resellers passing through Anthropic.
//!
//! Tenant label cardinality: bounded by the number of paying tenants
//! (single digits to low hundreds for a typical SaaS). Safe.
//!
//! ## Where the call sites are
//!
//! See the helper functions below — every billable event in the
//! daemon's hot path calls one of these. Locating new billable
//! actions: grep for `billing_metering::`.

use crate::tenancy::TenantId;

/// Record one ack'd inbound message after dedup passed. Called from
/// each protocol handler right before dispatching to the AI provider.
pub fn record_inbound(tenant: &TenantId, platform: &str) {
    metrics::counter!(
        "weclawbot_billing_inbound_messages_total",
        "tenant_id" => tenant.as_str().to_string(),
        "platform" => platform.to_string()
    )
    .increment(1);
}

/// Record sandbox compute time. Called from `sandbox::ensure` exit
/// path (best-effort approximation — measures wall time from
/// `ensure()` start to caller's invoke completion). Per-message
/// sandbox spawn model uses this; the long-running ACP model
/// accumulates differently (TODO: ACP sandbox accounting in v7.5).
pub fn record_sandbox_seconds(tenant: &TenantId, seconds: f64) {
    metrics::counter!(
        "weclawbot_billing_sandbox_seconds_total",
        "tenant_id" => tenant.as_str().to_string()
    )
    .increment(seconds as u64);
}

/// v7.6 — record AI provider token usage. Restored from v7.5 deletion
/// now that the multi-provider extraction story is uniform:
///   - claude / codex: parsed from `stream-json` `result.usage` event
///     by `crate::ai::claude::stream_json::process_event`
///   - openai-compat: parsed from response body `usage` by
///     `crate::ai::chat::complete`
///
/// Both populate `ClaudeOutput::token_usage` (or equivalent), which
/// the dispatcher then passes here.
///
/// Tenant label cardinality is bounded by the number of paying
/// tenants. Provider + direction are small fixed sets
/// (`claude`/`codex`/`openai-compat` × `input`/`output`).
pub fn record_ai_tokens(
    tenant: &TenantId,
    provider: &'static str,
    direction: &'static str,
    tokens: u64,
) {
    if tokens == 0 {
        return;
    }
    metrics::counter!(
        "weclawbot_billing_ai_tokens_total",
        "tenant_id" => tenant.as_str().to_string(),
        "provider" => provider,
        "direction" => direction
    )
    .increment(tokens);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_do_not_panic() {
        let t = TenantId::default_tenant();
        record_inbound(&t, "ilink-wechat");
        record_sandbox_seconds(&t, 4.2);
        record_ai_tokens(&t, "claude", "input", 12_345);
        record_ai_tokens(&t, "claude", "output", 678);
        record_ai_tokens(&t, "claude", "input", 0); // no-op short-circuit
    }
}
