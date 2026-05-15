-- v7.5 — SLA rollup table for `ee::sla_driver`.
--
-- Per-tenant 5-minute window rollup of usability metrics. Source data
-- is `user_history` + `audit_log` joined to `users.tenant_id`. The
-- driver upserts one row per (tenant_id, window_start) tuple.
--
-- Schema mirrors `crate::ee::sla::SlaWindow` (types-only struct
-- shipped earlier with no aggregator behind it).
--
-- Pruning: rollups accumulate at 288 rows/tenant/day. The driver
-- prunes anything older than 90 days on each pass (cheap DELETE
-- because of the `idx_sla_window_start` index).

CREATE TABLE sla_rollup (
  tenant_id          TEXT        NOT NULL,
  window_start       TIMESTAMPTZ NOT NULL,
  covered_seconds    INTEGER     NOT NULL,
  -- v7.5 phase 1: uptime + latency are computed from DB-derivable
  -- counters only. Once we add `tenant_id` labels to Prometheus
  -- counters + a Prom HTTP client (v7.6), we'll backfill these from
  -- proper rate() queries. For now they're set to defaults that
  -- don't trip SLA thresholds.
  downtime_seconds   INTEGER     NOT NULL DEFAULT 0,
  latency_p99_ms     INTEGER     NOT NULL DEFAULT 0,
  error_rate         DOUBLE PRECISION NOT NULL DEFAULT 0.0,
  PRIMARY KEY (tenant_id, window_start)
);

CREATE INDEX idx_sla_window_start ON sla_rollup (window_start);
