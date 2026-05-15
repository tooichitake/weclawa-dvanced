use serde::Serialize;

use crate::auth::accounts::{list_indexed_account_ids, load_account};

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
    pub accounts: usize,
    pub pid: u32,
}

#[derive(Debug, Serialize)]
pub struct AccountSummary {
    pub account_id: String,
    pub has_token: bool,
    pub user_id: Option<String>,
    pub base_url: Option<String>,
    pub saved_at: Option<String>,
    /// v5 M1: 协议归属（"ilink-wechat" / "telegram" / "discord" / "feishu"）
    pub platform_id: String,
}

pub fn build_health() -> HealthResponse {
    let ids = list_indexed_account_ids();
    HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
        accounts: ids.len(),
        pid: std::process::id(),
    }
}

pub fn build_accounts_list() -> Vec<AccountSummary> {
    list_indexed_account_ids()
        .into_iter()
        .filter_map(|id| {
            let account = load_account(&id)?;
            Some(AccountSummary {
                account_id: id,
                has_token: account.token.as_ref().is_some_and(|t| !t.is_empty()),
                user_id: account.user_id,
                base_url: account.base_url,
                saved_at: account.saved_at,
                platform_id: account.platform_id,
            })
        })
        .collect()
}
