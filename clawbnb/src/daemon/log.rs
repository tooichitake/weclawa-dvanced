use std::fs;
use std::time::SystemTime;

use crate::storage::state_dir::logs_dir;

/// Phase 5.7: 删除超过这个天数的 `weclawbot-YYYY-MM-DD.log` 文件。
/// 操作员若希望长留，导出/归档后再启动即可。env `WECLAWBOT_LOG_RETENTION_DAYS`
/// 可覆盖（必须是正整数，否则忽略）。
const DEFAULT_LOG_RETENTION_DAYS: u64 = 30;

pub fn current_log_path() -> std::path::PathBuf {
    let date = chrono::Local::now().format("%Y-%m-%d");
    logs_dir().join(format!("weclawbot-{date}.log"))
}

pub fn setup_file_logging() {
    let dir = logs_dir();
    let _ = fs::create_dir_all(&dir);

    // 启动时顺手清一次老 log（best-effort，错误只 log 不阻塞 boot）
    cleanup_old_logs();

    let path = current_log_path();
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);

    match file {
        Ok(f) => {
            use tracing_subscriber::prelude::*;
            use tracing_subscriber::{fmt, EnvFilter};

            let filter = EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info"));

            tracing_subscriber::registry()
                .with(
                    fmt::layer()
                        .with_writer(f)
                        .with_ansi(false)
                        .with_target(false),
                )
                .with(filter)
                .init();
        }
        Err(e) => {
            eprintln!("warning: could not open log file {}: {e}", path.display());
            tracing_subscriber::fmt::init();
        }
    }
}

/// Boot-time best-effort log retention. Iterates `~/.weclawbot/logs/`,
/// removes regular files whose mtime is older than N days. Today's log
/// is never touched even if mtime suggests it should be (defensive
/// against clock skew).
fn cleanup_old_logs() {
    let dir = logs_dir();
    let days = retention_days();
    let cutoff = match SystemTime::now().checked_sub(std::time::Duration::from_secs(days * 86400)) {
        Some(t) => t,
        None => return,
    };
    let today = current_log_path();
    let read = match fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut removed = 0u64;
    for entry in read.flatten() {
        let path = entry.path();
        if path == today {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        if mtime >= cutoff {
            continue;
        }
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        // 只清 weclawbot-* 与 smoke-e2e-* 这种 daemon 自家产出的；
        // 不要把运营商手动放进 logs/ 的别的东西也删了。
        if !name.starts_with("weclawbot-") && !name.starts_with("smoke-e2e-") && name != "daemon.out" {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        eprintln!("log retention: removed {removed} file(s) older than {days} days");
    }
}

fn retention_days() -> u64 {
    std::env::var("WECLAWBOT_LOG_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_LOG_RETENTION_DAYS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_default_when_env_unset() {
        // SAFETY: 单线程测试，独占 env var。
        unsafe { std::env::remove_var("WECLAWBOT_LOG_RETENTION_DAYS"); }
        assert_eq!(retention_days(), DEFAULT_LOG_RETENTION_DAYS);
    }

    #[test]
    fn retention_honours_env_var() {
        unsafe { std::env::set_var("WECLAWBOT_LOG_RETENTION_DAYS", "7"); }
        assert_eq!(retention_days(), 7);
        unsafe { std::env::remove_var("WECLAWBOT_LOG_RETENTION_DAYS"); }
    }

    #[test]
    fn retention_rejects_garbage() {
        unsafe { std::env::set_var("WECLAWBOT_LOG_RETENTION_DAYS", "not-a-number"); }
        assert_eq!(retention_days(), DEFAULT_LOG_RETENTION_DAYS);
        unsafe { std::env::set_var("WECLAWBOT_LOG_RETENTION_DAYS", "0"); }
        assert_eq!(retention_days(), DEFAULT_LOG_RETENTION_DAYS);
        unsafe { std::env::remove_var("WECLAWBOT_LOG_RETENTION_DAYS"); }
    }
}
