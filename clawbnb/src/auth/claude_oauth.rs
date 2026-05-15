//! Operator-side Claude OAuth token freshness monitoring (Phase 5.3).
//!
//! daemon 读 `~/.claude/.credentials.json::claudeAiOauth.expiresAt`，
//! 把 token 剩余寿命暴露给：
//! - 启动时一次 + 每 30 分钟一次的 tracing 告警（warn/<24h, error/<1h）
//! - `/healthz` 里的 `claude_oauth_expires_in_seconds` 字段
//! - Prometheus `weclawbot_claude_oauth_expires_seconds` gauge
//!
//! 这样运营商在 token 即将过期前就能看到信号，不会像 v0 那样卡 15h
//! 才发现 sandbox 内 claude 401。

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use tracing::{error, info, warn};

const POLL_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 min
const WARN_THRESHOLD: i64 = 24 * 3600;
const ERROR_THRESHOLD: i64 = 3600;

#[derive(Debug, Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OauthBlock>,
}

#[derive(Debug, Deserialize)]
struct OauthBlock {
    /// Unix ms when access token stops being honored by Anthropic API.
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
}

/// 当前 `expiresAt` 距现在的秒数。正数=未到期，负数=已过期。
/// `None` 表示文件不存在 / 格式不对 / 没字段 — 调用方决定怎么处理
/// （通常作为 "unknown" 处理，不报错，因为可能用户根本不用 claude）。
pub fn expires_in_seconds() -> Option<i64> {
    let path = credentials_path()?;
    if !path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    let parsed: CredentialsFile = serde_json::from_str(&raw).ok()?;
    let expires_ms = parsed.claude_ai_oauth?.expires_at?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    Some((expires_ms - now_ms) / 1000)
}

fn credentials_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join(".credentials.json"))
}

/// 把当前剩余时间写到 Prometheus gauge 并 log。每个执行体内部 idempotent。
pub fn report_once() {
    let Some(secs) = expires_in_seconds() else {
        return;
    };
    metrics::gauge!("weclawbot_claude_oauth_expires_seconds").set(secs as f64);
    if secs < 0 {
        error!(
            "claude OAuth token EXPIRED {} hours ago — sandbox claude will 401. \
             Run `claude /login` on the host to refresh.",
            -secs / 3600
        );
    } else if secs < ERROR_THRESHOLD {
        error!(
            "claude OAuth token expires in {} minutes — run `claude /login` now.",
            secs / 60
        );
    } else if secs < WARN_THRESHOLD {
        warn!(
            "claude OAuth token expires in {} hours — plan a `claude /login` before then.",
            secs / 3600
        );
    } else {
        info!("claude OAuth token has {} hours of life left", secs / 3600);
    }
}

/// 启动后台 task：每 30 分钟跑一次 `report_once()`。已经在 daemon 启动
/// 时调用一次，这个 task 维持后续节奏。Shutdown 信号到达时退出。
pub fn spawn_watcher(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        // 首次 tick 是立即触发的，但我们已经在 boot 里跑过一次 —— skip
        interval.tick().await;
        loop {
            tokio::select! {
                _ = interval.tick() => report_once(),
                _ = shutdown.changed() => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_credentials_with_oauth() {
        let json = r#"{
            "claudeAiOauth": {
                "accessToken": "xxx",
                "refreshToken": "yyy",
                "expiresAt": 1778718985432
            }
        }"#;
        let parsed: CredentialsFile = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.claude_ai_oauth.unwrap().expires_at, Some(1778718985432));
    }

    #[test]
    fn parse_credentials_without_oauth() {
        let parsed: CredentialsFile = serde_json::from_str("{}").unwrap();
        assert!(parsed.claude_ai_oauth.is_none());
    }
}
