//! v7.0 timestamp helpers — bridge `String` (domain layer, RFC3339) ↔
//! `chrono::DateTime<Utc>` (DB layer, TIMESTAMPTZ).
//!
//! After V0012 every timestamp column in PG is `TIMESTAMPTZ`. Repos
//! bind / read `DateTime<Utc>` natively. Domain types like
//! [`crate::repo::users::UserProfile`] still use `String` for backwards
//! compat with the GUI handlers / JSON serialization, so the repo's
//! materialize functions convert here.

use chrono::{DateTime, TimeZone, Utc};

/// Parse a domain-layer RFC3339 string into a UTC `DateTime`. If the
/// input doesn't parse (corruption / migration leftover), fall back to
/// the Unix epoch so the bind succeeds and the row is recoverable.
/// Logs a warn so operators can spot the bad data.
pub fn parse_rfc3339(s: &str) -> DateTime<Utc> {
    match DateTime::parse_from_rfc3339(s) {
        Ok(dt) => dt.with_timezone(&Utc),
        Err(e) => {
            tracing::warn!("ts::parse_rfc3339: bad input {s:?}: {e} — using epoch fallback");
            Utc.timestamp_opt(0, 0).unwrap()
        }
    }
}

/// Same but for `Option<String>`. None passes through.
pub fn parse_rfc3339_opt(s: &Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref().map(parse_rfc3339)
}

/// Format a UTC DateTime back to RFC3339 for domain-layer transport.
pub fn format_rfc3339(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339()
}

/// `Option<DateTime>` → `Option<RFC3339 string>`.
pub fn format_rfc3339_opt(dt: &Option<DateTime<Utc>>) -> Option<String> {
    dt.as_ref().map(format_rfc3339)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_z_suffix() {
        let s = "2026-05-15T22:30:17.025997Z";
        let dt = parse_rfc3339(s);
        assert_eq!(format_rfc3339(&dt), "2026-05-15T22:30:17.025997+00:00");
    }

    #[test]
    fn parse_with_offset() {
        let dt = parse_rfc3339("2026-05-15T22:30:17+08:00");
        // Normalized to UTC
        assert_eq!(dt.timezone(), Utc);
        assert_eq!(dt.format("%H:%M:%S").to_string(), "14:30:17");
    }

    #[test]
    fn bad_input_uses_epoch_fallback() {
        let dt = parse_rfc3339("not a date");
        assert_eq!(dt.timestamp(), 0);
    }

    #[test]
    fn opt_round_trip() {
        let s = Some("2026-05-15T22:30:17Z".to_string());
        let dt = parse_rfc3339_opt(&s);
        assert!(dt.is_some());
        let back = format_rfc3339_opt(&dt);
        assert!(back.unwrap().starts_with("2026-05-15T22:30:17"));
    }

    #[test]
    fn opt_none_passes_through() {
        assert!(parse_rfc3339_opt(&None).is_none());
        assert!(format_rfc3339_opt(&None).is_none());
    }
}
