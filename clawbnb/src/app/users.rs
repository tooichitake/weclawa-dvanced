//! User-related UseCases.

use crate::error::WeclawError;
use crate::ids::UserHash;
use crate::repo::users_async::SqlxUserRepo;
use crate::sandbox;

/// Result of a delete operation. Useful for callers that need to render
/// distinct messages ("deleted", "no such user", "killed N containers").
#[derive(Debug, Default)]
pub struct DeleteOutcome {
    /// DB row existed and was removed.
    pub db_removed: bool,
    /// Number of running containers that were killed before DB delete.
    pub containers_killed: usize,
    /// On-disk sandbox dir was found and removed.
    pub dir_removed: bool,
}

/// Delete a user atomically-ish:
///
/// 1. Kill any running podman containers for this user (so they don't
///    keep writing to the soon-to-be-deleted DB row).
/// 2. DELETE from `users` — FK CASCADE wipes settings/history/sessions.
/// 3. Best-effort `fs::remove_dir_all` on the sandbox dir.
///
/// 三步顺序故意如此 —— 容器先杀，再 DB delete，最后 fs cleanup。
/// 如果中间任一步骤失败：DB DELETE 成功但 fs 失败 → 留 orphan dir，
/// reconciler 后续清理；容器 kill 失败 → operator 收警告，可重试。
/// **不会** 出现"DB 删了但容器还在写 history → FK 报错"的状态污染。
///
/// 调用方：`cli/users.rs::reset` / `service/routes.rs::delete_user` /
/// `wechat_menu/*` 删用户菜单。
pub async fn delete(
    repo: &SqlxUserRepo,
    hash: &UserHash,
) -> Result<DeleteOutcome, WeclawError> {
    let mut outcome = DeleteOutcome::default();

    // Step 1: kill running containers.
    match sandbox::lifecycle::kill_user_containers(hash.as_str()).await {
        Ok(ids) => outcome.containers_killed = ids.len(),
        Err(e) => {
            tracing::warn!(
                "delete_user: kill_user_containers failed for {}: {e} \
                 — proceeding with DB delete; reconciler will collect any orphans",
                hash.as_str()
            );
        }
    }

    // Step 2: DB cascade delete.
    outcome.db_removed = repo.delete(hash).await.map_err(|e| {
        WeclawError::Internal(format!("db delete: {e}"))
    })?;

    // Step 3: best-effort fs cleanup.
    let dir = sandbox::layout::user_dir(hash.as_str());
    if dir.exists() {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => outcome.dir_removed = true,
            Err(e) => {
                tracing::warn!("delete_user: rm {}: {e}", dir.display());
            }
        }
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db_async;

    #[tokio::test]
    async fn delete_nonexistent_user_returns_no_op_outcome() {
        let pool = db_async::open_in_memory().await.unwrap();
        let repo = SqlxUserRepo::new(pool);
        let hash = UserHash::new("u-000000000000");
        let out = delete(&repo, &hash).await.unwrap();
        assert!(!out.db_removed);
        assert_eq!(out.containers_killed, 0);
        assert!(!out.dir_removed);
    }

    #[tokio::test]
    async fn delete_existing_user_removes_db_row() {
        use crate::repo::users::UserProfile;
        let pool = db_async::open_in_memory().await.unwrap();
        let repo = SqlxUserRepo::new(pool);
        let hash = UserHash::new("u-abcdef012345");
        repo.upsert_profile(&UserProfile {
            hash: hash.clone(),
            user_id_hint: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            last_seen_at: None,
            message_count: 0,
            sync_state: "unknown".into(),
            last_sync_at: None,
            last_sync_error: None,
        })
        .await
        .unwrap();
        let out = delete(&repo, &hash).await.unwrap();
        assert!(out.db_removed);
        assert!(repo.get_profile(&hash).await.unwrap().is_none());
    }
}
