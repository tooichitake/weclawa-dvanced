//! Per-user conversation history — sqlx async over `UserRepo`. v4.2.
//!
//! v4.2 完整异步化：每条入站消息会调 append + recent，hot path 必须
//! reactor-friendly。本模块的 public API 全部 `async fn`，caller 必须
//! `.await`。

use serde::{Deserialize, Serialize};

use crate::ids::UserHash;
use crate::repo::users_async::SqlxUserRepo;
use crate::storage::db_async;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatTurn {
    pub role: String, // "user" or "assistant"
    pub content: String,
}

async fn repo() -> Option<SqlxUserRepo> {
    let pool = match db_async::try_global_async_pool() {
        Some(p) => p,
        None => db_async::open_default().await.ok()?,
    };
    Some(SqlxUserRepo::new(pool))
}

/// Fetch the merged user_settings JSON. Returns `None` if no settings row
/// or DB unavailable.
pub async fn user_settings_json(user_hash: &str) -> Option<serde_json::Value> {
    let r = repo().await?;
    match r.get_settings(&UserHash::new(user_hash)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("user_settings_json for {user_hash}: {e}");
            None
        }
    }
}

/// Append one turn. Errors logged but not returned — chat flow doesn't
/// die for DB hiccups.
pub async fn append(user_hash: &str, role: &str, content: &str) {
    let Some(r) = repo().await else { return };
    if let Err(e) = r.append_history(&UserHash::new(user_hash), role, content).await {
        tracing::warn!("history append for {user_hash}: {e}");
    }
}

/// All turns for `user_hash`, oldest-first.
pub async fn get(user_hash: &str) -> Vec<ChatTurn> {
    let Some(r) = repo().await else { return Vec::new() };
    match r
        .recent_history(&UserHash::new(user_hash), 1_000_000)
        .await
    {
        Ok(turns) => turns
            .into_iter()
            .map(|t| ChatTurn {
                role: t.role,
                content: t.content,
            })
            .collect(),
        Err(e) => {
            tracing::warn!("history get for {user_hash}: {e}");
            Vec::new()
        }
    }
}

/// Last `limit` turns, oldest-first.
pub async fn recent(user_hash: &str, limit: usize) -> Vec<ChatTurn> {
    let Some(r) = repo().await else { return Vec::new() };
    match r.recent_history(&UserHash::new(user_hash), limit).await {
        Ok(turns) => turns
            .into_iter()
            .map(|t| ChatTurn {
                role: t.role,
                content: t.content,
            })
            .collect(),
        Err(e) => {
            tracing::warn!("history recent for {user_hash}: {e}");
            Vec::new()
        }
    }
}

/// Drop every turn for `user_hash`. Used by `/menu → 开启新对话` and HTTP
/// admin "clear history".
pub async fn clear(user_hash: &str) {
    let Some(r) = repo().await else { return };
    if let Err(e) = r.clear_history(&UserHash::new(user_hash)).await {
        tracing::warn!("history clear for {user_hash}: {e}");
    }
}
