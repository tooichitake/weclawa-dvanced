//! WeChat-user ↔ bot-account binding — thin shim over `BindingRepo`.
//!
//! Phase 1d moved this from `~/.weclawbot/user-agent-map.json` (a single
//! file with the whole map) into the `bindings` table. The function
//! signatures here stayed the same so callers (`monitor::handler`,
//! `service::routes`) keep working.
//!
//! The legacy `history_account_ids` field is **not** persisted in the new
//! schema — it was only used for diagnostics and the new `audit_log`
//! covers the same need with timestamps. Existing values in the old JSON
//! file are dropped on import.

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::ids::{AccountId, WeixinUserId};
use crate::repo::bindings_async::SqlxBindingRepo;
use crate::runtime::blocking::block_on_async;
use crate::storage::db_async;

/// External shape returned to legacy callers. `history_account_ids` is
/// always empty in the SQLite era; preserved on the struct for source
/// compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserAgentRecord {
    pub user_id: String,
    pub agent_id: String,
    pub active_account_id: String,
    #[serde(default)]
    pub history_account_ids: Vec<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

// UserAgentMap 整批 binding 的旧表示形式（来自 v0 JSON 时代），
// Phase 1d 之后所有逻辑都按行级 BindingRepo 直接读写，整体集合
// 视图不再需要 —— 删掉避免 dead-code 警告。

async fn repo_async() -> Option<SqlxBindingRepo> {
    let pool = match db_async::try_global_async_pool() {
        Some(p) => p,
        None => db_async::open_default().await.ok()?,
    };
    Some(SqlxBindingRepo::new(pool))
}

// --- Public API ------------------------------------------------------------

pub fn build_agent_id(user_id: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(user_id.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("wx-{}", &hex[..8])
}

// load_user_agent_map / save_user_agent_map / get_binding_for_user
// 在 Phase 1d 之后已经没有任何 caller —— 全部走 register_or_update_binding +
// 直接 BindingRepo。删除以减少 dead-code 警告 + 误用风险。
// 历史版本若需查询 binding，请用 BindingRepo::get / list 直接调。

pub fn register_or_update_binding(user_id: &str, account_id: &str) -> UserAgentRecord {
    let agent_id = build_agent_id(user_id);
    let now = chrono::Utc::now().to_rfc3339();
    let fallback = UserAgentRecord {
        user_id: user_id.to_string(),
        agent_id: agent_id.clone(),
        active_account_id: account_id.to_string(),
        history_account_ids: Vec::new(),
        created_at: now.clone(),
        updated_at: now,
    };

    let user_id_owned = user_id.to_string();
    let account_id_owned = account_id.to_string();
    let agent_id_for_async = agent_id.clone();
    let fallback_for_async = fallback.clone();
    block_on_async(async move {
        let Some(r) = repo_async().await else { return fallback_for_async };
        let user = WeixinUserId::new(&user_id_owned);
        let acct = AccountId::new(&account_id_owned);
        if let Err(e) = r.register_or_touch(&user, &acct, &agent_id_for_async).await {
            tracing::warn!(
                "register_or_update_binding({user_id_owned}, {account_id_owned}): {e}"
            );
            return fallback_for_async;
        }
        match r.get(&user).await {
            Ok(Some(b)) => UserAgentRecord {
                user_id: b.weixin_user_id.into_string(),
                agent_id: b.agent_id,
                active_account_id: b.active_account_id.into_string(),
                history_account_ids: Vec::new(),
                created_at: b.updated_at.clone(),
                updated_at: b.updated_at,
            },
            _ => fallback_for_async,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_agent_id_is_stable_and_short() {
        let a = build_agent_id("o9cq80w4@im.wechat");
        assert!(a.starts_with("wx-"));
        assert_eq!(a.len(), 3 + 8);
        // Stable across calls.
        assert_eq!(a, build_agent_id("o9cq80w4@im.wechat"));
    }

    #[test]
    fn build_agent_id_differs_per_user() {
        let a = build_agent_id("user-1");
        let b = build_agent_id("user-2");
        assert_ne!(a, b);
    }
}
