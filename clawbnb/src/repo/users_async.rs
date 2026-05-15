//! AsyncUserRepo — v4.1 K8 sqlx 版本。最大的 repo (profile + settings +
//! history + console_sessions)。

use chrono::Utc;
use serde_json::Value;
use crate::storage::db_async::AsyncDbPool;

use crate::ids::UserHash;
use crate::repo::users::{ConsoleSession, HistoryTurn, UserProfile};
use crate::storage::db::DbError;

pub struct SqlxUserRepo {
    pool: AsyncDbPool,
}

impl SqlxUserRepo {
    pub fn new(pool: AsyncDbPool) -> Self {
        Self { pool }
    }

    // ===== profile =====

    pub async fn get_profile(&self, hash: &UserHash) -> Result<Option<UserProfile>, DbError> {
        let row: Option<(
            String,
            Option<String>,
            String,
            Option<String>,
            i64,
            String,
            Option<String>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT hash, user_id_hint, created_at, last_seen_at, message_count,
                    sync_state, last_sync_at, last_sync_error
             FROM users WHERE hash = ?",
        )
        .bind(hash.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx user get: {e}")))?;
        Ok(row.map(materialize_profile))
    }

    pub async fn list_profiles_for_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<UserProfile>, DbError> {
        let rows: Vec<(
            String,
            Option<String>,
            String,
            Option<String>,
            i64,
            String,
            Option<String>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT hash, user_id_hint, created_at, last_seen_at, message_count,
                    sync_state, last_sync_at, last_sync_error
             FROM users WHERE tenant_id = ? ORDER BY last_seen_at DESC NULLS LAST",
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx user list: {e}")))?;
        Ok(rows.into_iter().map(materialize_profile).collect())
    }

    pub async fn list_profiles(&self) -> Result<Vec<UserProfile>, DbError> {
        self.list_profiles_for_tenant(crate::tenancy::DEFAULT_TENANT)
            .await
    }

    pub async fn upsert_profile(&self, p: &UserProfile) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO users (hash, user_id_hint, created_at, last_seen_at,
                                message_count, sync_state, last_sync_at, last_sync_error)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(hash) DO UPDATE SET
                 user_id_hint = excluded.user_id_hint,
                 last_seen_at = excluded.last_seen_at,
                 message_count = excluded.message_count,
                 sync_state = excluded.sync_state,
                 last_sync_at = excluded.last_sync_at,
                 last_sync_error = excluded.last_sync_error",
        )
        .bind(p.hash.as_str())
        .bind(&p.user_id_hint)
        .bind(&p.created_at)
        .bind(&p.last_seen_at)
        .bind(p.message_count as i64)
        .bind(&p.sync_state)
        .bind(&p.last_sync_at)
        .bind(&p.last_sync_error)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx user upsert: {e}")))?;
        Ok(())
    }

    pub async fn touch_last_seen(&self, hash: &UserHash) -> Result<(), DbError> {
        sqlx::query("UPDATE users SET last_seen_at = ? WHERE hash = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(hash.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx user touch: {e}")))?;
        Ok(())
    }

    pub async fn incr_message_count(&self, hash: &UserHash) -> Result<(), DbError> {
        sqlx::query("UPDATE users SET message_count = message_count + 1 WHERE hash = ?")
            .bind(hash.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx user incr: {e}")))?;
        Ok(())
    }

    pub async fn set_sync_state(
        &self,
        hash: &UserHash,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE users SET sync_state = ?, last_sync_at = ?, last_sync_error = ?
             WHERE hash = ?",
        )
        .bind(state)
        .bind(Utc::now().to_rfc3339())
        .bind(error)
        .bind(hash.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx user sync_state: {e}")))?;
        Ok(())
    }

    pub async fn delete(&self, hash: &UserHash) -> Result<bool, DbError> {
        let res = sqlx::query("DELETE FROM users WHERE hash = ?")
            .bind(hash.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx user delete: {e}")))?;
        Ok(res.rows_affected() > 0)
    }

    // ===== settings =====

    pub async fn get_settings(&self, hash: &UserHash) -> Result<Option<Value>, DbError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT settings_json FROM user_settings WHERE user_hash = ?")
                .bind(hash.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DbError::Pool(format!("sqlx settings get: {e}")))?;
        Ok(match row {
            Some((s,)) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    pub async fn upsert_settings(&self, hash: &UserHash, settings: &Value) -> Result<(), DbError> {
        self.upsert_settings_bump_version(hash, settings)
            .await
            .map(|_| ())
    }

    pub async fn get_settings_version(&self, hash: &UserHash) -> Result<Option<i64>, DbError> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT version FROM user_settings WHERE user_hash = ?")
                .bind(hash.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DbError::Pool(format!("sqlx settings ver: {e}")))?;
        Ok(row.map(|r| r.0))
    }

    pub async fn upsert_settings_bump_version(
        &self,
        hash: &UserHash,
        settings: &Value,
    ) -> Result<i64, DbError> {
        let json = serde_json::to_string(settings)?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO user_settings (user_hash, settings_json, updated_at, version)
             VALUES (?, ?, ?, 1)
             ON CONFLICT(user_hash) DO UPDATE SET
                 settings_json = excluded.settings_json,
                 updated_at = excluded.updated_at,
                 version = user_settings.version + 1",
        )
        .bind(hash.as_str())
        .bind(&json)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx settings upsert: {e}")))?;
        let row: (i64,) =
            sqlx::query_as("SELECT version FROM user_settings WHERE user_hash = ?")
                .bind(hash.as_str())
                .fetch_one(&self.pool)
                .await
                .map_err(|e| DbError::Pool(format!("sqlx settings ver-after: {e}")))?;
        Ok(row.0)
    }

    // ===== history =====

    pub async fn append_history(
        &self,
        hash: &UserHash,
        role: &str,
        content: &str,
    ) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO user_history (user_hash, role, content, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(hash.as_str())
        .bind(role)
        .bind(content)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx history append: {e}")))?;
        Ok(())
    }

    pub async fn recent_history(
        &self,
        hash: &UserHash,
        limit: usize,
    ) -> Result<Vec<HistoryTurn>, DbError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT role, content FROM (
                SELECT role, content, id FROM user_history
                WHERE user_hash = ? ORDER BY id DESC LIMIT ?
             ) ORDER BY id ASC",
        )
        .bind(hash.as_str())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx history recent: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|(role, content)| HistoryTurn { role, content })
            .collect())
    }

    pub async fn clear_history(&self, hash: &UserHash) -> Result<u64, DbError> {
        let res = sqlx::query("DELETE FROM user_history WHERE user_hash = ?")
            .bind(hash.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx history clear: {e}")))?;
        Ok(res.rows_affected())
    }

    pub async fn history_count(&self, hash: &UserHash) -> Result<u64, DbError> {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM user_history WHERE user_hash = ?")
                .bind(hash.as_str())
                .fetch_one(&self.pool)
                .await
                .map_err(|e| DbError::Pool(format!("sqlx history count: {e}")))?;
        Ok(row.0.max(0) as u64)
    }

    // ===== console_session =====

    pub async fn load_console_session(
        &self,
        hash: &UserHash,
    ) -> Result<Option<ConsoleSession>, DbError> {
        let row: Option<(i64, String, String)> = sqlx::query_as(
            "SELECT in_menu, current_path, last_input_at
             FROM console_sessions WHERE user_hash = ?",
        )
        .bind(hash.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx console load: {e}")))?;
        Ok(row.map(|(in_menu, path_json, last_input_at)| ConsoleSession {
            in_menu: in_menu != 0,
            current_path: serde_json::from_str(&path_json).unwrap_or_default(),
            last_input_at,
        }))
    }

    pub async fn save_console_session(
        &self,
        hash: &UserHash,
        session: &ConsoleSession,
    ) -> Result<(), DbError> {
        let path_json = serde_json::to_string(&session.current_path)?;
        sqlx::query(
            "INSERT INTO console_sessions (user_hash, in_menu, current_path, last_input_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(user_hash) DO UPDATE SET
                 in_menu = excluded.in_menu,
                 current_path = excluded.current_path,
                 last_input_at = excluded.last_input_at",
        )
        .bind(hash.as_str())
        .bind(if session.in_menu { 1i64 } else { 0i64 })
        .bind(&path_json)
        .bind(&session.last_input_at)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx console save: {e}")))?;
        Ok(())
    }

    pub async fn clear_console_session(&self, hash: &UserHash) -> Result<(), DbError> {
        sqlx::query("DELETE FROM console_sessions WHERE user_hash = ?")
            .bind(hash.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx console clear: {e}")))?;
        Ok(())
    }
}

fn materialize_profile(
    row: (
        String,
        Option<String>,
        String,
        Option<String>,
        i64,
        String,
        Option<String>,
        Option<String>,
    ),
) -> UserProfile {
    UserProfile {
        hash: UserHash::new(row.0),
        user_id_hint: row.1,
        created_at: row.2,
        last_seen_at: row.3,
        message_count: row.4.max(0) as u64,
        sync_state: row.5,
        last_sync_at: row.6,
        last_sync_error: row.7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    async fn repo() -> SqlxUserRepo {
        SqlxUserRepo::new(db_async::open_in_memory().await.unwrap())
    }

    fn sample(hash: &str) -> UserProfile {
        UserProfile {
            hash: UserHash::new(hash),
            user_id_hint: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            last_seen_at: None,
            message_count: 0,
            sync_state: "unknown".into(),
            last_sync_at: None,
            last_sync_error: None,
        }
    }

    #[tokio::test]
    async fn upsert_profile_get_async() {
        let r = repo().await;
        r.upsert_profile(&sample("u-aaaaaaaaaaaa")).await.unwrap();
        let got = r
            .get_profile(&UserHash::new("u-aaaaaaaaaaaa"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.hash.as_str(), "u-aaaaaaaaaaaa");
    }

    #[tokio::test]
    async fn settings_version_increments_on_each_upsert_async() {
        use serde_json::json;
        let r = repo().await;
        let hash = UserHash::new("u-aaaaaaaaaaaa");
        r.upsert_profile(&sample("u-aaaaaaaaaaaa")).await.unwrap();
        let v1 = r
            .upsert_settings_bump_version(&hash, &json!({"theme": "dark"}))
            .await
            .unwrap();
        assert_eq!(v1, 1);
        let v2 = r
            .upsert_settings_bump_version(&hash, &json!({"theme": "light"}))
            .await
            .unwrap();
        assert_eq!(v2, 2);
        assert_eq!(r.get_settings_version(&hash).await.unwrap(), Some(2));
    }

    #[tokio::test]
    async fn history_append_and_recent_async() {
        let r = repo().await;
        let hash = UserHash::new("u-aaaaaaaaaaaa");
        r.upsert_profile(&sample("u-aaaaaaaaaaaa")).await.unwrap();
        for i in 0..5 {
            r.append_history(&hash, "user", &format!("msg-{i}")).await.unwrap();
        }
        let turns = r.recent_history(&hash, 3).await.unwrap();
        assert_eq!(turns.len(), 3);
        // Last 3 in chronological order
        assert_eq!(turns[0].content, "msg-2");
        assert_eq!(turns[2].content, "msg-4");
    }

    #[tokio::test]
    async fn delete_cascades_async() {
        use serde_json::json;
        let r = repo().await;
        let hash = UserHash::new("u-aaaaaaaaaaaa");
        r.upsert_profile(&sample("u-aaaaaaaaaaaa")).await.unwrap();
        r.upsert_settings(&hash, &json!({"a": 1})).await.unwrap();
        r.append_history(&hash, "user", "hi").await.unwrap();

        assert!(r.delete(&hash).await.unwrap());
        // FK CASCADE wiped settings + history
        assert!(r.get_settings(&hash).await.unwrap().is_none());
        assert_eq!(r.history_count(&hash).await.unwrap(), 0);
    }
}
