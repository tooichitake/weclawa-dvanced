//! Per-WeChat-user inbound rate limiter (Phase 5.1).
//!
//! 入站消息进入 handler 前先过这道闸：单个 WeChat 用户每分钟最多
//! `MAX_PER_MINUTE` 条 — 默认 30 条。超过的就丢掉并发一条客气的
//! 节流提示。
//!
//! 实现：固定窗口（1 分钟桶）记到 `rate_limits` 表的 `(scope_key,
//! window_start_ts)` 行，靠 `RateLimitRepo::increment` 原子 +1
//! 然后跟阈值比。固定窗口比滑动窗口在 SQLite 上简单且足够 —— 万
//! 一被人精准踩点边界灌一次也只是双倍流量，没真危险。
//!
//! 老窗口由 `prune_older_than(cutoff)` 周期清理；当前没接 cron，
//! 每次 increment 时附带 1% 概率自洁，简单够用。

use crate::ids::WeixinUserId;
use crate::repo::rate_limits_async::SqlxRateLimitRepo;
use crate::runtime::blocking::block_on_async;
use crate::storage::db_async;

/// 老窗口在 DB 里留多久。比窗口大很多，足够审计回看；超过后清。
const RETENTION_SECS: i64 = 60 * 60 * 24; // 1 day

/// 检查一条入站消息是否应当被拒绝。
/// 上限来自 `config.rateLimit.userPerMinute`（运营商在 GUI 可改、
/// 热加载）。`0` 表示不限。
///
/// 返回 `Some(reason)` = 拒绝，daemon 应回一条节流提示；
/// 返回 `None` = 放行，daemon 正常处理。
pub fn check_inbound(user_id: &WeixinUserId, limit: u64) -> Option<String> {
    if limit == 0 {
        return None; // 0 = 不限
    }
    let user_id_str = user_id.as_str().to_string();
    block_on_async(async move {
        let Some(apool) = db_async::try_global_async_pool() else {
            return None;
        };
        let repo = SqlxRateLimitRepo::new(apool);
        let scope_key = format!("inbound:{user_id_str}");
        let window = current_minute_window();
        match repo.increment(&scope_key, &window).await {
            Ok(count) => {
                if rand::random::<f32>() < 0.01 {
                    let cutoff = past_cutoff(RETENTION_SECS);
                    let _ = repo.prune_older_than(&cutoff).await;
                }
                metrics::counter!(
                    "weclawbot_rate_limit_checks_total",
                    "result" => if count > limit { "throttled" } else { "allowed" }
                )
                .increment(1);
                if count > limit {
                    // v7.5 — write audit row so `trust_driver` can
                    // compute the `integrity` factor (1 - breaches/total).
                    // user_id (WeChat from_user_id) is mapped to user_hash
                    // for stable trust scoring keying — same hashing as
                    // monitor::handler uses for Sandbox::ensure. The
                    // `block_on_async` here is in a hot path, so we
                    // sync-await the audit write; failure is logged but
                    // doesn't gate the throttle decision.
                    let user_hash = crate::sandbox::hash_user_id_for_lookup(&user_id_str);
                    if let Some(audit_pool) = db_async::try_global_async_pool() {
                        let audit_repo = crate::repo::audit_async::SqlxAuditRepo::new(audit_pool);
                        let after = serde_json::json!({
                            "count": count,
                            "limit": limit,
                        });
                        let _ = audit_repo
                            .record(crate::repo::audit::AuditInput {
                                actor_key_id: None,
                                action: "rate_limit.breach",
                                target: Some(&user_hash),
                                before: None,
                                after: Some(&after),
                                ip: None,
                            })
                            .await;
                    }
                    Some(format!(
                        "消息频率过高（当前窗口 {count}/{limit} 条/分钟），稍后再试。"
                    ))
                } else {
                    None
                }
            }
            Err(e) => {
                tracing::warn!("rate_limit::check_inbound({user_id_str}): {e} — fail-open");
                None
            }
        }
    })
}

/// 当前所在的 1 分钟桶起始时间（RFC3339 UTC，秒已对齐到 :00）。
fn current_minute_window() -> String {
    let now = chrono::Utc::now();
    // 把秒/纳秒清零，得到 minute-aligned timestamp
    now.format("%Y-%m-%dT%H:%M:00Z").to_string()
}

fn past_cutoff(secs_ago: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(secs_ago))
        .format("%Y-%m-%dT%H:%M:00Z")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    #[test]
    fn current_window_aligned_to_minute() {
        let w = current_minute_window();
        // 应当以 :00Z 结尾
        assert!(w.ends_with(":00Z"));
        // 该是这个月（粗略 sanity）
        let now = chrono::Utc::now();
        assert!(w.contains(&format!("{:04}-{:02}", now.year(), now.month())));
    }

    #[test]
    fn past_cutoff_is_in_the_past() {
        let c = past_cutoff(60);
        let w = current_minute_window();
        assert!(c < w);
    }
}
