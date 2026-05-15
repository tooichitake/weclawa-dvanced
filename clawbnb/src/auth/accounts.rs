//! Bot-account persistence — thin shim over `repo::accounts::AccountRepo`.
//!
//! Phase 1d migrated the on-disk format from `~/.weclawbot/accounts/<id>.json`
//! files to the `accounts` table in `state.db`. The public function
//! signatures here stayed the same so existing callers (`monitor::poller`,
//! `service::routes`, `cli::login`, `cli::send`, …) didn't have to learn
//! the repo API. Internally every call now goes through `AccountRepo`.
//!
//! On first daemon boot `cli::import` copies the legacy JSON files into
//! the DB; after that the JSON files are inert (no writer touches them).
//! Operators who downgrade with `weclawbot export-legacy` will recreate
//! them.


use serde::{Deserialize, Serialize};

use crate::ids::{AccountId, BaseUrl, BotToken, WeixinUserId};
use crate::repo::accounts::Account;
use crate::repo::accounts_async::SqlxAccountRepo;
use crate::runtime::blocking::block_on_async;
use crate::storage::db_async;

/// External shape returned to legacy callers. Field-for-field compatible
/// with the previous JSON-backed struct so consuming code is unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeixinAccount {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default, rename = "savedAt")]
    pub saved_at: Option<String>,
    /// v5 M1: 协议归属
    #[serde(default = "default_platform_id")]
    pub platform_id: String,
}

fn default_platform_id() -> String {
    "ilink-wechat".to_string()
}

pub fn normalize_account_id(input: &str) -> String {
    input.replace(|c: char| c == '@' || c == '.', "-")
}

// --- Pool access -----------------------------------------------------------

async fn repo_async() -> Option<SqlxAccountRepo> {
    let pool = match db_async::try_global_async_pool() {
        Some(p) => p,
        None => db_async::open_default().await.ok()?,
    };
    Some(SqlxAccountRepo::new(pool))
}

fn account_to_external(a: &Account) -> WeixinAccount {
    WeixinAccount {
        token: a.token.as_ref().map(|t| t.expose().to_string()),
        base_url: Some(a.base_url.as_str().to_string()),
        user_id: a.weixin_user_id.as_ref().map(|u| u.as_str().to_string()),
        saved_at: Some(a.saved_at.clone()),
        platform_id: a.platform_id.clone(),
    }
}

// --- Public API (unchanged signatures) -------------------------------------

pub fn load_account(account_id: &str) -> Option<WeixinAccount> {
    let account_id_owned = account_id.to_string();
    block_on_async(async move {
        let r = repo_async().await?;
        match r.get(&AccountId::new(&account_id_owned)).await {
            Ok(Some(a)) => Some(account_to_external(&a)),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("load_account({account_id_owned}): {e}");
                None
            }
        }
    })
}

pub fn save_account(
    account_id: &str,
    token: Option<&str>,
    base_url: Option<&str>,
    user_id: Option<&str>,
) {
    let acct = Account {
        account_id: AccountId::new(account_id),
        token: token.map(BotToken::new),
        base_url: BaseUrl::new(
            base_url
                .unwrap_or("https://ilinkai.weixin.qq.com")
                .to_string(),
        ),
        weixin_user_id: user_id.map(WeixinUserId::new),
        saved_at: chrono::Utc::now().to_rfc3339(),
        platform_id: "ilink-wechat".into(),
    };
    let account_id_owned = account_id.to_string();
    block_on_async(async move {
        let Some(r) = repo_async().await else { return };
        if let Err(e) = r.upsert(&acct).await {
            tracing::error!("save account {account_id_owned}: {e}");
        }
    })
}

pub fn list_indexed_account_ids() -> Vec<String> {
    block_on_async(async {
        let Some(r) = repo_async().await else { return Vec::new() };
        match r.list().await {
            Ok(rows) => rows
                .into_iter()
                .map(|a| a.account_id.into_string())
                .collect(),
            Err(e) => {
                tracing::warn!("list accounts: {e}");
                Vec::new()
            }
        }
    })
}

pub fn register_account_id(_account_id: &str) {}

pub fn get_local_bot_tokens() -> Vec<String> {
    block_on_async(async {
        let Some(r) = repo_async().await else { return Vec::new() };
        match r.list().await {
            Ok(rows) => rows
                .into_iter()
                .take(10)
                .filter_map(|a| a.token.map(|t| t.expose().to_string()))
                .filter(|t| !t.trim().is_empty())
                .collect(),
            Err(e) => {
                tracing::warn!("get_local_bot_tokens: {e}");
                Vec::new()
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_at_and_dot() {
        assert_eq!(
            normalize_account_id("alice@example.com"),
            "alice-example-com"
        );
        assert_eq!(normalize_account_id("plain"), "plain");
    }
}
