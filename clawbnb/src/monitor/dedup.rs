//! Inbound message deduplication — sqlx async (v4.2).
//!
//! iLink 偶发在 long-poll window 内重发同一 `message_id`。`is_duplicate_async`
//! 把 msg_id 落 `seen_messages` 表（V0003 migration）+ ~1% lazy prune。

use crate::repo::dedup_async::SqlxDedupRepo;
use crate::storage::db_async;

const RETENTION_SECS: i64 = 3600;

/// 返回 true 表示之前已见过该 msg_id（应当跳过）。msg_id == 0 永远 false。
pub async fn is_duplicate_async(msg_id: i64) -> bool {
    if msg_id == 0 {
        return false;
    }
    let Some(apool) = db_async::try_global_async_pool() else {
        // DB 还没 open — fail-open as new (boot 早期窗口)
        return false;
    };
    let repo = SqlxDedupRepo::new(apool);

    if rand::random::<f32>() < 0.01 {
        let cutoff =
            (chrono::Utc::now() - chrono::Duration::seconds(RETENTION_SECS)).to_rfc3339();
        let _ = repo.prune_older_than(&cutoff).await;
    }

    match repo.mark_seen(msg_id).await {
        Ok(was_duplicate) => {
            metrics::counter!(
                "weclawbot_dedup_checks_total",
                "result" => if was_duplicate { "duplicate" } else { "new" }
            )
            .increment(1);
            was_duplicate
        }
        Err(e) => {
            tracing::warn!("dedup mark_seen({msg_id}): {e} — fail-open");
            false
        }
    }
}

/// 撤回 msg_id 的 "已见" 标记 — handler 在 sandbox::ensure 失败时调，
/// 让 redeliver 还有机会被处理。
pub async fn unmark_async(msg_id: i64) {
    if msg_id == 0 {
        return;
    }
    let Some(apool) = db_async::try_global_async_pool() else {
        return;
    };
    let repo = SqlxDedupRepo::new(apool);
    if let Err(e) = repo.unmark(msg_id).await {
        tracing::warn!("dedup unmark({msg_id}): {e}");
    }
}
