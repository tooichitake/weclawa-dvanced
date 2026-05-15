//! AsyncAccountRepo — v4.1 K7 sqlx 版本。
//!
//! 跟 sync 版一样处理 token 加密：写时 AES-GCM 加密到 token_ciphertext +
//! token_nonce；读时优先解 ciphertext，跌回 plaintext。

use chrono::Utc;
use crate::storage::db_async::AsyncDbPool;

use crate::ids::{AccountId, BaseUrl, BotToken, WeixinUserId};
use crate::repo::accounts::Account;
use crate::storage::db::DbError;

pub struct SqlxAccountRepo {
    pool: AsyncDbPool,
}

impl SqlxAccountRepo {
    pub fn new(pool: AsyncDbPool) -> Self {
        Self { pool }
    }

    pub async fn get(&self, id: &AccountId) -> Result<Option<Account>, DbError> {
        let row: Option<(
            String,
            Option<String>,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            String,
            Option<String>,
            String,
            String,
        )> = sqlx::query_as(
            "SELECT account_id, token, token_ciphertext, token_nonce,
                    base_url, weixin_user_id, saved_at, platform_id
             FROM accounts WHERE account_id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx account get: {e}")))?;
        Ok(row.map(materialize_account))
    }

    pub async fn list(&self) -> Result<Vec<Account>, DbError> {
        let rows: Vec<(
            String,
            Option<String>,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            String,
            Option<String>,
            String,
            String,
        )> = sqlx::query_as(
            "SELECT account_id, token, token_ciphertext, token_nonce,
                    base_url, weixin_user_id, saved_at, platform_id
             FROM accounts ORDER BY saved_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx account list: {e}")))?;
        Ok(rows.into_iter().map(materialize_account).collect())
    }

    pub async fn list_for_tenant(&self, tenant_id: &str) -> Result<Vec<Account>, DbError> {
        let rows: Vec<(
            String,
            Option<String>,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            String,
            Option<String>,
            String,
            String,
        )> = sqlx::query_as(
            "SELECT account_id, token, token_ciphertext, token_nonce,
                    base_url, weixin_user_id, saved_at, platform_id
             FROM accounts WHERE tenant_id = ? ORDER BY saved_at DESC",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx account list_for_tenant: {e}")))?;
        Ok(rows.into_iter().map(materialize_account).collect())
    }

    pub async fn upsert(&self, account: &Account) -> Result<(), DbError> {
        let (ct, nonce) = match account.token.as_ref() {
            Some(t) => match crate::storage::crypto::encrypt(t.expose().as_bytes()) {
                Ok((c, n)) => (Some(c), Some(n.to_vec())),
                Err(e) => {
                    tracing::warn!("sqlx token encrypt for {}: {e}", account.account_id);
                    (None, None)
                }
            },
            None => (None, None),
        };
        sqlx::query(
            "INSERT INTO accounts
                 (account_id, token, token_ciphertext, token_nonce,
                  base_url, weixin_user_id, saved_at, platform_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(account_id) DO UPDATE SET
                 token            = excluded.token,
                 token_ciphertext = excluded.token_ciphertext,
                 token_nonce      = excluded.token_nonce,
                 base_url         = excluded.base_url,
                 weixin_user_id   = excluded.weixin_user_id,
                 saved_at         = excluded.saved_at,
                 platform_id      = excluded.platform_id",
        )
        .bind(account.account_id.as_str())
        .bind(account.token.as_ref().map(|t| t.expose()))
        .bind(&ct)
        .bind(&nonce)
        .bind(account.base_url.as_str())
        .bind(account.weixin_user_id.as_ref().map(|u| u.as_str()))
        .bind(&account.saved_at)
        .bind(&account.platform_id)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx account upsert: {e}")))?;
        Ok(())
    }

    pub async fn delete(&self, id: &AccountId) -> Result<bool, DbError> {
        let res = sqlx::query("DELETE FROM accounts WHERE account_id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx account delete: {e}")))?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn rotate_token(
        &self,
        id: &AccountId,
        new_token: &BotToken,
    ) -> Result<bool, DbError> {
        let (ct, nonce) = match crate::storage::crypto::encrypt(new_token.expose().as_bytes()) {
            Ok((c, n)) => (Some(c), Some(n.to_vec())),
            Err(e) => {
                tracing::warn!("sqlx rotate_token encrypt for {id}: {e}");
                (None, None)
            }
        };
        let res = sqlx::query(
            "UPDATE accounts
             SET token = ?,
                 token_ciphertext = ?,
                 token_nonce = ?,
                 saved_at = ?
             WHERE account_id = ?",
        )
        .bind(new_token.expose())
        .bind(&ct)
        .bind(&nonce)
        .bind(Utc::now().to_rfc3339())
        .bind(id.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx rotate: {e}")))?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn get_tenant_id(&self, id: &AccountId) -> Result<Option<String>, DbError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT tenant_id FROM accounts WHERE account_id = ?")
                .bind(id.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DbError::Pool(format!("sqlx account tenant: {e}")))?;
        Ok(row.map(|r| r.0))
    }
}

/// 把 query 返的 row tuple 转成 Account，处理 token 加密优先级。
fn materialize_account(
    row: (
        String,
        Option<String>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        String,
        Option<String>,
        String,
        String,
    ),
) -> Account {
    let (id, plaintext, ct, nonce, base_url, weixin_user_id, saved_at, platform_id) = row;
    // Prefer ciphertext (Phase 6.1), fall back to plaintext for legacy rows.
    let token = match (ct.as_ref(), nonce.as_ref()) {
        (Some(c), Some(n)) if n.len() == 12 => {
            let mut nonce_arr = [0u8; 12];
            nonce_arr.copy_from_slice(n);
            match crate::storage::crypto::decrypt(c, &nonce_arr) {
                Ok(bytes) => String::from_utf8(bytes).ok().map(BotToken::new),
                Err(e) => {
                    tracing::warn!("sqlx token decrypt for {id}: {e} — falling back to plaintext");
                    plaintext.clone().map(BotToken::new)
                }
            }
        }
        _ => plaintext.clone().map(BotToken::new),
    };
    Account {
        account_id: AccountId::new(id),
        token,
        base_url: BaseUrl::new(base_url),
        weixin_user_id: weixin_user_id.map(WeixinUserId::new),
        saved_at,
        platform_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    async fn repo() -> SqlxAccountRepo {
        SqlxAccountRepo::new(db_async::open_in_memory().await.unwrap())
    }

    fn sample(id: &str) -> Account {
        Account {
            account_id: AccountId::new(id),
            token: None,
            base_url: BaseUrl::new("https://ilinkai.weixin.qq.com"),
            weixin_user_id: None,
            saved_at: "2026-05-13T00:00:00Z".into(),
            platform_id: "ilink-wechat".into(),
        }
    }

    #[tokio::test]
    async fn upsert_then_get_async() {
        let r = repo().await;
        r.upsert(&sample("acct-1")).await.unwrap();
        let got = r.get(&AccountId::new("acct-1")).await.unwrap().unwrap();
        assert_eq!(got.account_id.as_str(), "acct-1");
    }

    #[tokio::test]
    async fn delete_returns_existence_async() {
        let r = repo().await;
        r.upsert(&sample("acct-1")).await.unwrap();
        assert!(r.delete(&AccountId::new("acct-1")).await.unwrap());
        assert!(!r.delete(&AccountId::new("acct-1")).await.unwrap());
    }

    #[tokio::test]
    async fn get_tenant_id_returns_default_async() {
        let r = repo().await;
        r.upsert(&sample("acct-1")).await.unwrap();
        assert_eq!(
            r.get_tenant_id(&AccountId::new("acct-1")).await.unwrap(),
            Some("default".into())
        );
    }
}
