//! Factory defaults — operator-edited template seeding new users.
//!
//! v4.2: 全 sqlx async repo。Public API 保留 sync 形态（callers 涵盖
//! sync 的 sandbox::ensure / materialize 路径），内部用
//! `runtime::blocking::block_on_async` 桥到 async repo。daemon 主路径
//! 在 multi-thread tokio runtime 内，安全；偶发非 tokio 调用（如 CLI
//! 子命令独立 invoke）会起一次性 current_thread runtime 兜底。

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::ids::UserHash;
use crate::repo::defaults_async::SqlxDefaultsRepo;
use crate::repo::users::UserProfile;
use crate::repo::users_async::SqlxUserRepo;
use crate::runtime::blocking::block_on_async;
use crate::sandbox::layout;
use crate::storage::db_async;

pub const DEFAULT_MODEL: &str = "sonnet";

fn bootstrap_defaults() -> Value {
    json!({
        "theme": "dark",
        "model": DEFAULT_MODEL,
        "permissionMode": "default",
        "enabledPlugins": [],
        "allowedTools": [],
        "disallowedTools": [],
        "env": {},
        "_weclawbotManaged": true
    })
}

async fn defaults_repo_async() -> Option<SqlxDefaultsRepo> {
    let pool = match db_async::try_global_async_pool() {
        Some(p) => p,
        None => db_async::open_default().await.ok()?,
    };
    Some(SqlxDefaultsRepo::new(pool))
}

async fn users_repo_async() -> Option<SqlxUserRepo> {
    let pool = match db_async::try_global_async_pool() {
        Some(p) => p,
        None => db_async::open_default().await.ok()?,
    };
    Some(SqlxUserRepo::new(pool))
}

pub fn ensure_defaults_exist() -> Result<(), String> {
    block_on_async(async {
        let Some(r) = defaults_repo_async().await else {
            return Err("state.db unavailable".into());
        };
        r.bootstrap(&bootstrap_defaults())
            .await
            .map(|_| ())
            .map_err(|e| format!("bootstrap defaults: {e}"))
    })
}

pub fn load_defaults() -> Value {
    block_on_async(async {
        match defaults_repo_async().await {
            Some(r) => match r.get().await {
                Ok(Some(v)) => v,
                Ok(None) => bootstrap_defaults(),
                Err(e) => {
                    tracing::warn!("load defaults: {e}");
                    bootstrap_defaults()
                }
            },
            None => bootstrap_defaults(),
        }
    })
}

pub fn save_defaults(value: &Value) -> Result<(), String> {
    block_on_async(async {
        let Some(r) = defaults_repo_async().await else {
            return Err("state.db unavailable".into());
        };
        r.set(value).await.map_err(|e| format!("write defaults: {e}"))
    })
}

async fn ensure_user_profile(
    r: &SqlxUserRepo,
    hash: &UserHash,
) -> Result<(), crate::storage::db::DbError> {
    if r.get_profile(hash).await?.is_some() {
        return Ok(());
    }
    let now = chrono::Utc::now().to_rfc3339();
    r.upsert_profile(&UserProfile {
        hash: hash.clone(),
        user_id_hint: None,
        created_at: now,
        last_seen_at: None,
        message_count: 0,
        sync_state: "unknown".into(),
        last_sync_at: None,
        last_sync_error: None,
    })
    .await
}

pub fn init_user_settings(user_hash: &str) -> Result<(), String> {
    let _ = ensure_defaults_exist();
    let template = load_defaults();
    block_on_async(async {
        let Some(r) = users_repo_async().await else {
            return Err("state.db unavailable".into());
        };
        let hash = UserHash::new(user_hash);
        ensure_user_profile(&r, &hash)
            .await
            .map_err(|e| format!("ensure profile: {e}"))?;
        r.upsert_settings(&hash, &template)
            .await
            .map_err(|e| format!("write user settings: {e}"))
    })
}

pub fn load_user_settings(user_hash: &str) -> Option<Value> {
    block_on_async(async {
        let r = users_repo_async().await?;
        match r.get_settings(&UserHash::new(user_hash)).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("load user settings ({user_hash}): {e}");
                None
            }
        }
    })
}

pub fn save_user_settings(user_hash: &str, value: &Value) -> Result<(), String> {
    block_on_async(async {
        let Some(r) = users_repo_async().await else {
            return Err("state.db unavailable".into());
        };
        let hash = UserHash::new(user_hash);
        ensure_user_profile(&r, &hash)
            .await
            .map_err(|e| format!("ensure profile: {e}"))?;
        r.upsert_settings(&hash, value)
            .await
            .map_err(|e| format!("write user settings: {e}"))
    })
}

pub fn apply_defaults_to_user(user_hash: &str) -> Result<Option<Value>, String> {
    let old = load_user_settings(user_hash);
    let template = load_defaults();
    save_user_settings(user_hash, &template)?;
    Ok(old)
}

pub fn user_settings_path(user_hash: &str) -> PathBuf {
    layout::user_settings_path(user_hash)
}

#[derive(Debug, Default)]
pub struct MigrationReport {
    pub users_scanned: usize,
    pub users_updated: usize,
    pub users_kept_existing: usize,
    pub errors: Vec<String>,
}

pub fn migrate_existing_users() -> MigrationReport {
    let mut report = MigrationReport::default();

    if let Err(e) = upgrade_defaults_template() {
        report.errors.push(format!("defaults template upgrade: {e}"));
    }

    block_on_async(async {
        let Some(r) = users_repo_async().await else {
            report.errors.push("state.db unavailable".into());
            return;
        };
        let profiles = match r.list_profiles().await {
            Ok(ps) => ps,
            Err(e) => {
                report.errors.push(format!("list users: {e}"));
                return;
            }
        };
        for p in profiles {
            report.users_scanned += 1;
            match upgrade_one_user_async(&r, &p.hash).await {
                Ok(true) => report.users_updated += 1,
                Ok(false) => report.users_kept_existing += 1,
                Err(e) => report.errors.push(format!("user {}: {e}", p.hash.as_str())),
            }
        }
    });

    report
}

fn upgrade_defaults_template() -> Result<(), String> {
    block_on_async(async {
        let Some(r) = defaults_repo_async().await else {
            return Err("state.db unavailable".into());
        };
        let mut current = match r.get().await {
            Ok(Some(v)) => v,
            Ok(None) => return r.set(&bootstrap_defaults()).await.map_err(|e| format!("{e}")),
            Err(e) => return Err(format!("read defaults: {e}")),
        };
        let mut changed = false;
        if let Value::Object(map) = &mut current {
            if !map.contains_key("model") {
                map.insert("model".into(), Value::String(DEFAULT_MODEL.into()));
                changed = true;
            }
        }
        if changed {
            r.set(&current).await.map_err(|e| format!("write defaults: {e}"))
        } else {
            Ok(())
        }
    })
}

async fn upgrade_one_user_async(
    r: &SqlxUserRepo,
    hash: &UserHash,
) -> Result<bool, String> {
    let current = match r.get_settings(hash).await {
        Ok(Some(v)) => v,
        Ok(None) => return Ok(false),
        Err(e) => return Err(format!("{e}")),
    };
    if let Value::Object(map) = &current {
        if map.contains_key("model") {
            return Ok(false);
        }
    }
    let mut updated = current.clone();
    if let Value::Object(map) = &mut updated {
        map.insert("model".into(), Value::String(DEFAULT_MODEL.into()));
    }
    r.upsert_settings(hash, &updated).await.map_err(|e| format!("{e}"))?;
    Ok(true)
}
