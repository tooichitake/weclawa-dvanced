//! AsyncDefaultsRepo — v4.1 K2 sqlx 版本。

use chrono::Utc;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::storage::db::DbError;

pub struct SqlxDefaultsRepo {
    pool: SqlitePool,
}

impl SqlxDefaultsRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get(&self) -> Result<Option<Value>, DbError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT settings_json FROM defaults WHERE id = 1")
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DbError::Pool(format!("sqlx defaults get: {e}")))?;
        Ok(match row {
            Some((s,)) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    pub async fn set(&self, settings: &Value) -> Result<(), DbError> {
        let json = serde_json::to_string(settings)?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO defaults (id, settings_json, updated_at)
             VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET
                 settings_json = excluded.settings_json,
                 updated_at = excluded.updated_at",
        )
        .bind(&json)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx defaults set: {e}")))?;
        Ok(())
    }

    pub async fn bootstrap(&self, seed: &Value) -> Result<bool, DbError> {
        let json = serde_json::to_string(seed)?;
        let now = Utc::now().to_rfc3339();
        // v5.2 O2: ANSI ON CONFLICT (SQLite 3.24+ / Postgres)
        let res = sqlx::query(
            "INSERT INTO defaults (id, settings_json, updated_at)
             VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&json)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx defaults bootstrap: {e}")))?;
        Ok(res.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;
    use serde_json::json;

    async fn repo() -> SqlxDefaultsRepo {
        SqlxDefaultsRepo::new(db_async::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn empty_returns_none_async() {
        let r = repo().await;
        assert!(r.get().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bootstrap_seeds_then_no_op_async() {
        let r = repo().await;
        let seed = json!({"theme": "dark"});
        assert!(r.bootstrap(&seed).await.unwrap());
        assert!(!r.bootstrap(&json!({"theme": "light"})).await.unwrap());
        assert_eq!(r.get().await.unwrap().unwrap(), seed);
    }

    #[tokio::test]
    async fn set_overwrites_async() {
        let r = repo().await;
        r.set(&json!({"a": 1})).await.unwrap();
        r.set(&json!({"b": 2})).await.unwrap();
        assert_eq!(r.get().await.unwrap().unwrap(), json!({"b": 2}));
    }
}
