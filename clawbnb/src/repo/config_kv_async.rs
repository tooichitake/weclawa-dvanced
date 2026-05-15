//! AsyncConfigKvRepo — v7.0 JSONB + TIMESTAMPTZ.

use chrono::Utc;
use serde_json::Value;
use crate::storage::db_async::AsyncDbPool;

use crate::storage::db::DbError;

pub struct SqlxConfigKvRepo {
    pool: AsyncDbPool,
}

impl SqlxConfigKvRepo {
    pub fn new(pool: AsyncDbPool) -> Self {
        Self { pool }
    }

    pub async fn get(&self, key: &str) -> Result<Option<Value>, DbError> {
        let row: Option<(Value,)> =
            sqlx::query_as("SELECT value_json FROM config_kv WHERE key = $1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| DbError::Pool(format!("sqlx config_kv get: {e}")))?;
        Ok(row.map(|(v,)| v))
    }

    pub async fn set(&self, key: &str, value: &Value) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO config_kv (key, value_json, updated_at)
             VALUES ($1, $2, $3)
             ON CONFLICT(key) DO UPDATE SET
                 value_json = excluded.value_json,
                 updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(Utc::now())
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx config_kv set: {e}")))?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<bool, DbError> {
        let res = sqlx::query("DELETE FROM config_kv WHERE key = $1")
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx config_kv delete: {e}")))?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn list(&self) -> Result<Vec<(String, Value)>, DbError> {
        let rows: Vec<(String, Value)> =
            sqlx::query_as("SELECT key, value_json FROM config_kv ORDER BY key ASC")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| DbError::Pool(format!("sqlx config_kv list: {e}")))?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;
    use serde_json::json;

    async fn repo() -> SqlxConfigKvRepo {
        SqlxConfigKvRepo::new(db_async::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn set_get_round_trip_async() {
        let r = repo().await;
        r.set("ai.timeoutMs", &json!(300_000)).await.unwrap();
        assert_eq!(
            r.get("ai.timeoutMs").await.unwrap().unwrap(),
            json!(300_000)
        );
    }

    #[tokio::test]
    async fn delete_returns_existence_async() {
        let r = repo().await;
        r.set("k", &json!(true)).await.unwrap();
        assert!(r.delete("k").await.unwrap());
        assert!(!r.delete("k").await.unwrap());
    }

    #[tokio::test]
    async fn list_sorts_by_key_async() {
        let r = repo().await;
        r.set("zeta", &json!(1)).await.unwrap();
        r.set("alpha", &json!(2)).await.unwrap();
        let list = r.list().await.unwrap();
        assert_eq!(list[0].0, "alpha");
        assert_eq!(list[1].0, "zeta");
    }
}
