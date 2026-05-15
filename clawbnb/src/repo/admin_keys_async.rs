//! AsyncAdminKeyRepo — v4 J4 hot path 切 sqlx。
//!
//! bearer_auth middleware 每个 axum 请求都打一次 verify_and_load —
//! 包括 list_active + touch_last_used。这是最热的 repo 之一。
//!
//! ## verify_and_load 仍走 spawn_blocking
//!
//! argon2 验证本身 ~75ms × N keys 是**CPU-bound**，必须 spawn_blocking
//! 推到 blocking pool 否则会 stall reactor。本 repo 提供 async list +
//! touch；argon2 verify 部分 caller 自己 spawn_blocking 跑。
//!
//! 这跟 IO async 不冲突 —— async IO 解决"reactor 等 IO"问题，CPU 重活
//! 仍然得分到 blocking 线程。

use crate::storage::db_async::AsyncDbPool;

use crate::repo::admin_keys::{AdminKeyRecord, Role};
use crate::storage::db::DbError;

pub struct SqlxAdminKeyRepo {
    pool: AsyncDbPool,
}

impl SqlxAdminKeyRepo {
    pub fn new(pool: AsyncDbPool) -> Self {
        Self { pool }
    }

    /// list_active — verify_and_load 的输入。每个 axum 请求都跑。
    pub async fn list_active(&self) -> Result<Vec<AdminKeyRecord>, DbError> {
        let rows: Vec<(String, String, String, String, String, Option<String>, Option<String>, String)> =
            sqlx::query_as(
                "SELECT id, name, key_hash, role, created_at, last_used_at, revoked_at, tenant_id
                 FROM admin_keys WHERE revoked_at IS NULL
                 ORDER BY created_at ASC",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx list_active: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|(id, name, key_hash, role, created_at, last_used_at, revoked_at, tenant_id)| {
                AdminKeyRecord {
                    id,
                    name,
                    key_hash,
                    role: Role::from_str(&role),
                    created_at,
                    last_used_at,
                    revoked_at,
                    tenant_id,
                }
            })
            .collect())
    }

    /// 更新 last_used_at — 每个成功 verify 后 fire-and-forget 调，async 让
    /// 它不阻塞当前请求的 handler 链。
    pub async fn touch_last_used(&self, id: &str) -> Result<(), DbError> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query("UPDATE admin_keys SET last_used_at = $1 WHERE id = $2")
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| DbError::Pool(format!("sqlx touch_last_used: {e}")))?;
        Ok(())
    }

    pub async fn count_active_super_admin(&self) -> Result<u64, DbError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM admin_keys
             WHERE revoked_at IS NULL AND role = 'super_admin'",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::Pool(format!("sqlx count_super: {e}")))?;
        Ok(row.0.max(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    async fn repo() -> SqlxAdminKeyRepo {
        SqlxAdminKeyRepo::new(db_async::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn list_active_on_empty_returns_empty_async() {
        let r = repo().await;
        let list = r.list_active().await.unwrap();
        assert_eq!(list.len(), 0);
    }

    #[tokio::test]
    async fn count_super_admin_on_empty_is_zero_async() {
        let r = repo().await;
        assert_eq!(r.count_active_super_admin().await.unwrap(), 0);
    }
}
