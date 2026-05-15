//! Test-only endpoint to drive the daemon end-to-end without real WeChat.
//!
//! Enabled via `WECLAWBOT_TEST_MODE=1` env var. When enabled:
//!   - `POST /api/test/inject` accepts a synthetic inbound message and runs
//!     it through `handle_inbound_message` exactly like a real WeChat
//!     message would.
//!   - The daemon's outbound `send_message` calls are tee'd to a JSONL log
//!     at `~/.weclawbot/capture.log` so the smoke test can assert what
//!     would have been sent to the user.
//!
//! Off by default — operator must explicitly opt in.

use std::sync::OnceLock;

pub fn test_mode_enabled() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("WECLAWBOT_TEST_MODE")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

pub fn capture_log_path() -> std::path::PathBuf {
    crate::storage::state_dir::state_dir().join("capture.log")
}

/// Append one captured outbound to the JSONL log. Called by the ILink
/// client wrapper when test mode is on.
pub fn capture_outbound(kind: &str, payload: &serde_json::Value) {
    if !test_mode_enabled() {
        return;
    }
    let now = chrono::Utc::now();
    let entry = serde_json::json!({
        "ts": now.to_rfc3339(),
        // ts_ms is the millisecond epoch — used by smoke-e2e T14 to measure
        // the typing-indicator latency (inbound inject → first send_typing).
        "ts_ms": now.timestamp_millis(),
        "kind": kind,
        "payload": payload,
    });
    use std::io::Write;
    let path = capture_log_path();
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{}", serde_json::to_string(&entry).unwrap_or_default()));
}
