//! AsyncBindingRepo — v4.1 K5 sqlx 版本。

use chrono::Utc;
use crate::storage::db_async::AsyncDbPool;

use crate::ids::{AccountId, WeixinUserId};
use crate::repo::bindings::Binding;
use crate::storage::db::DbError;

pub struct SqlxBindingRepo {
    pool: AsyncDbPool,
}

impl SqlxBindingRepo {
    pub fn new(pool: AsyncDbPool) -> Self {
        Self { pool }
    }

    pub async fn get(&self, user: &WeixinUserId) -> Result<Option<Binding>, DbError> {
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT active_account_id, agent_id, updated_at
             FROM bindings WHERE weixin_user_id = $1",
        )
        .bind(user.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx binding get: {e}")))?;
        Ok(row.map(|(acct, agent, updated)| Binding {
            weixin_user_id: user.clone(),
            active_account_id: AccountId::new(acct),
            agent_id: agent,
            updated_at: updated,
        }))
    }

    pub async fn register_or_touch(
        &self,
        user: &WeixinUserId,
        active_account_id: &AccountId,
        agent_id: &str,
    ) -> Result<bool, DbError> {
        let now = Utc::now().to_rfc3339();
        // v5.2 O2: ANSI ON CONFLICT 兼容 SQLite 3.24+ 和 Postgres，
        // 取代 SQLite-only "INSERT OR IGNORE"。
        let res = sqlx::query(
            "INSERT INTO bindings
                 (weixin_user_id, active_account_id, agent_id, updated_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (weixin_user_id) DO NOTHING",
        )
        .bind(user.as_str())
        .bind(active_account_id.as_str())
        .bind(agent_id)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx binding insert: {e}")))?;
        if res.rows_affected() == 0 {
            sqlx::query("UPDATE bindings SET updated_at = $1 WHERE weixin_user_id = $2")
                .bind(&now)
                .bind(user.as_str())
                .execute(&self.pool)
                .await
                .map_err(|e| DbError::Pool(format!("sqlx binding touch: {e}")))?;
        }
        Ok(res.rows_affected() > 0)
    }

    pub async fn set_active_account(
        &self,
        user: &WeixinUserId,
        active_account_id: &AccountId,
    ) -> Result<bool, DbError> {
        let now = Utc::now().to_rfc3339();
        let res = sqlx::query(
            "UPDATE bindings SET active_account_id = $1, updated_at = $2
             WHERE weixin_user_id = $3",
        )
        .bind(active_account_id.as_str())
        .bind(&now)
        .bind(user.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx binding rebind: {e}")))?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete(&self, user: &WeixinUserId) -> Result<bool, DbError> {
        let res = sqlx::query("DELETE FROM bindings WHERE weixin_user_id = $1")
            .bind(user.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx binding delete: {e}")))?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn list(&self) -> Result<Vec<Binding>, DbError> {
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT weixin_user_id, active_account_id, agent_id, updated_at
             FROM bindings ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx binding list: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|(u, a, g, t)| Binding {
                weixin_user_id: WeixinUserId::new(u),
                active_account_id: AccountId::new(a),
                agent_id: g,
                updated_at: t,
            })
            .collect())
    }

    pub async fn count(&self) -> Result<u64, DbError> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM bindings")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx binding count: {e}")))?;
        Ok(row.0.max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    async fn repo() -> SqlxBindingRepo {
        SqlxBindingRepo::new(db_async::open_in_memory().await.unwrap())
    }

    async fn seed_account(pool: &AsyncDbPool, id: &str) {
        sqlx::query(
            "INSERT INTO accounts (account_id, base_url, saved_at)
             VALUES ($1, $2, $3)",
        )
        .bind(id)
        .bind("https://ilinkai.weixin.qq.com")
        .bind("2026-05-13T00:00:00Z")
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn register_creates_then_touches_async() {
        let r = repo().await;
        seed_account(&r.pool, "acct-1").await;
        seed_account(&r.pool, "acct-2").await;
        let user = WeixinUserId::new("o9@im");
        assert!(r
            .register_or_touch(&user, &AccountId::new("acct-1"), "wx-abc")
            .await
            .unwrap());
        // 第二次不同 account → 不该重写 (sticky)
        assert!(!r
            .register_or_touch(&user, &AccountId::new("acct-2"), "wx-abc")
            .await
            .unwrap());
        let b = r.get(&user).await.unwrap().unwrap();
        assert_eq!(b.active_account_id.as_str(), "acct-1");
    }

    #[tokio::test]
    async fn set_active_account_force_rebind_async() {
        let r = repo().await;
        seed_account(&r.pool, "acct-1").await;
        seed_account(&r.pool, "acct-2").await;
        let user = WeixinUserId::new("o9@im");
        r.register_or_touch(&user, &AccountId::new("acct-1"), "wx-abc")
            .await
            .unwrap();
        assert!(r
            .set_active_account(&user, &AccountId::new("acct-2"))
            .await
            .unwrap());
        assert_eq!(
            r.get(&user).await.unwrap().unwrap().active_account_id.as_str(),
            "acct-2"
        );
    }
}
