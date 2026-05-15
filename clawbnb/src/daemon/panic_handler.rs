//! Process-wide panic handler.
//!
//! Without this, a panic inside `tokio::spawn`'d task is silently
//! swallowed and the daemon keeps running with one fewer worker. With
//! it, every panic becomes a tracing `error!` line carrying the panic
//! location and message — operators see the failure in logs and the
//! Prometheus `weclawbot_panics_total` counter ticks.

use std::panic;

/// Install the process-wide panic hook. Idempotent: a second call
/// silently no-ops (panic::set_hook always overwrites, but we want the
/// behaviour to be "install once" semantically).
pub fn install() {
    static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if INSTALLED.set(()).is_err() {
        return;
    }
    panic::set_hook(Box::new(|info| {
        // Extract a printable payload. std uses `&dyn Any`; we try the
        // two common payload concrete types (string forms) before
        // giving up.
        let msg = if let Some(s) = info.payload().downcast_ref::<&'static str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "(non-string panic payload)".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "(no location)".into());
        tracing::error!(target: "panic", panic_location = %location, panic_msg = %msg, "task panicked");
        metrics::counter!("weclawbot_panics_total").increment(1);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent() {
        install();
        install(); // does not panic
    }
}
