//! Per-user console session state — DB-backed (`console_sessions` table).
//!
//! Phase 1d migrated from `~/.weclawbot/users/<hash>/console_session.json`
//! to a row in `console_sessions`. The public `Session` struct stayed
//! identical so callers (`wechat_menu::tree`, `monitor::handler`) didn't
//! change.
//!
//! Session expires after `SESSION_TIMEOUT` minutes of no activity — that
//! prevents a user who typed `/menu` and walked away from being "stuck"
//! in menu mode forever on their next chat attempt.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::UserHash;
use crate::repo::users::ConsoleSession;
use crate::repo::users_async::SqlxUserRepo;
use crate::runtime::blocking::block_on_async;
use crate::storage::db_async;

/// Idle timeout. After this much wall-clock time without input, the next
/// inbound message exits menu mode automatically (and falls back to chat).
const SESSION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    pub in_menu: bool,
    pub current_path: Vec<String>,
    pub last_input_at: Option<DateTime<Utc>>,
}

async fn repo_async() -> Option<SqlxUserRepo> {
    let pool = match db_async::try_global_async_pool() {
        Some(p) => p,
        None => db_async::open_default().await.ok()?,
    };
    Some(SqlxUserRepo::new(pool))
}

// --- Conversions between public struct and the repo row --------------------

fn from_repo(c: ConsoleSession) -> Session {
    Session {
        in_menu: c.in_menu,
        current_path: c.current_path,
        last_input_at: DateTime::parse_from_rfc3339(&c.last_input_at)
            .ok()
            .map(|d| d.with_timezone(&Utc)),
    }
}

fn to_repo(s: &Session) -> ConsoleSession {
    ConsoleSession {
        in_menu: s.in_menu,
        current_path: s.current_path.clone(),
        last_input_at: s
            .last_input_at
            .unwrap_or_else(Utc::now)
            .to_rfc3339(),
    }
}

// --- Public API ------------------------------------------------------------

pub fn load(user_hash: &str) -> Session {
    let user_hash_owned = user_hash.to_string();
    block_on_async(async move {
        let Some(r) = repo_async().await else { return Session::default() };
        match r.load_console_session(&UserHash::new(&user_hash_owned)).await {
            Ok(Some(c)) => from_repo(c),
            Ok(None) => Session::default(),
            Err(e) => {
                tracing::warn!("console session load ({user_hash_owned}): {e}");
                Session::default()
            }
        }
    })
}

pub fn save(user_hash: &str, sess: &Session) {
    let user_hash_owned = user_hash.to_string();
    let to_save = to_repo(sess);
    block_on_async(async move {
        let Some(r) = repo_async().await else { return };
        let hash = UserHash::new(&user_hash_owned);
        if let Err(e) = ensure_user_profile_async(&r, &hash).await {
            tracing::warn!("console session ensure user ({user_hash_owned}): {e}");
            return;
        }
        if let Err(e) = r.save_console_session(&hash, &to_save).await {
            tracing::warn!("console session save ({user_hash_owned}): {e}");
        }
    })
}

async fn ensure_user_profile_async(
    r: &SqlxUserRepo,
    hash: &UserHash,
) -> Result<(), crate::storage::db::DbError> {
    if r.get_profile(hash).await?.is_some() {
        return Ok(());
    }
    let now = Utc::now().to_rfc3339();
    r.upsert_profile(&crate::repo::users::UserProfile {
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

pub fn is_in_menu_mode(user_hash: &str) -> bool {
    load(user_hash).in_menu
}

pub fn enter(user_hash: &str) {
    save(
        user_hash,
        &Session {
            in_menu: true,
            current_path: Vec::new(),
            last_input_at: Some(Utc::now()),
        },
    );
}

pub fn exit(user_hash: &str) {
    let user_hash_owned = user_hash.to_string();
    block_on_async(async move {
        let Some(r) = repo_async().await else { return };
        if let Err(e) = r.clear_console_session(&UserHash::new(&user_hash_owned)).await {
            tracing::warn!("console session clear ({user_hash_owned}): {e}");
        }
    })
}

#[allow(dead_code)]
pub fn touch_activity(user_hash: &str, mut sess: Session) {
    sess.last_input_at = Some(Utc::now());
    save(user_hash, &sess);
}

/// Drop the session if it's stale. Call at the top of every console route.
pub fn gc_expired(user_hash: &str) {
    let sess = load(user_hash);
    if !sess.in_menu {
        return;
    }
    let Some(last) = sess.last_input_at else {
        return;
    };
    let elapsed = Utc::now().signed_duration_since(last);
    let elapsed_secs = elapsed.num_seconds().max(0) as u64;
    if Duration::from_secs(elapsed_secs) > SESSION_TIMEOUT {
        tracing::debug!(
            "console session for {user_hash} idle {elapsed_secs}s — expiring"
        );
        exit(user_hash);
    }
}
