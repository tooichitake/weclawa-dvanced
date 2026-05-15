//! Observability — metrics + healthz + (later) structured logging.
//!
//! ## What lives here
//!
//! - `metrics` — Prometheus exporter handle + counter/gauge/histogram
//!   declarations consumed by the rest of the codebase.
//! - `healthz` — `/healthz` deep-check route. Probes podman, runsc,
//!   disk space, DB connectivity, claude credential freshness.
//!
//! ## Why not in `service::`
//!
//! Both pieces are read by code outside the HTTP handler tree
//! (poller increments counters, daemon-boot sets up the exporter).
//! Putting them under `service::` would force the call graph
//! into a circular shape; a sibling crate-level module is cleaner.

pub mod healthz;
pub mod metrics;
pub mod otel;
