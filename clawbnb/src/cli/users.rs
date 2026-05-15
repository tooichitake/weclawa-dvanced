//! `weclawbot users` — list / inspect / reset per-user sandboxes.
//!
//! Phase 1d: reads come from the `users` table (via `UserRepo`) rather
//! than walking `~/.weclawbot/users/<hash>/profile.json` files. Deletes
//! drop the DB row (cascading to settings/history/console_session) and
//! also wipe the on-disk sandbox directory.

use std::fs;

use chrono::DateTime;

use crate::ids::UserHash;
use crate::repo::users::UserProfile;
use crate::repo::users_async::SqlxUserRepo;
use crate::sandbox;
use crate::storage::db_async;

async fn open_repo() -> Result<SqlxUserRepo, String> {
    let pool = match db_async::try_global_async_pool() {
        Some(p) => p,
        None => db_async::open_default()
            .await
            .map_err(|e| format!("open state.db: {e}"))?,
    };
    Ok(SqlxUserRepo::new(pool))
}

pub async fn list() -> Result<(), String> {
    let r = open_repo().await?;
    let profiles = r.list_profiles().await.map_err(|e| format!("list: {e}"))?;
    if profiles.is_empty() {
        println!("No user sandboxes yet.");
        return Ok(());
    }
    println!(
        "{:<14}  {:<8}  {:<25}  {:<10}  HINT",
        "USER", "MSGS", "LAST SEEN", "SYNC"
    );
    for p in &profiles {
        let last_seen = p.last_seen_at.as_deref().unwrap_or("-");
        let hint = p.user_id_hint.as_deref().unwrap_or("-");
        println!(
            "{:<14}  {:<8}  {:<25}  {:<10}  {}",
            p.hash.as_str(),
            p.message_count,
            last_seen,
            p.sync_state,
            hint,
        );
    }
    Ok(())
}

pub async fn reset(hash: &str) -> Result<(), String> {
    let r = open_repo().await?;
    let user_hash = UserHash::new(hash);
    let out = crate::app::users::delete(&r, &user_hash)
        .await
        .map_err(|e| format!("{e}"))?;
    if out.containers_killed > 0 {
        println!("Killed {} running container(s)", out.containers_killed);
    }
    if out.db_removed {
        println!("Removed DB rows for {hash}");
    }
    if out.dir_removed {
        println!("Removed sandbox dir {}", sandbox::layout::user_dir(hash).display());
    }
    if !out.db_removed && !out.dir_removed {
        return Err(format!("no user {hash}"));
    }
    Ok(())
}

pub async fn prune(older_than_days: u64) -> Result<(), String> {
    let r = open_repo().await?;
    let threshold = chrono::Utc::now() - chrono::Duration::days(older_than_days as i64);
    let profiles = r.list_profiles().await.map_err(|e| format!("list: {e}"))?;
    let mut removed = 0usize;
    for p in profiles {
        if !is_stale(&p, threshold) {
            continue;
        }
        let hash = p.hash.clone();
        let _ = r.delete(&hash).await;
        let dir = sandbox::layout::user_dir(hash.as_str());
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
        let last_seen = p.last_seen_at.as_deref().unwrap_or("-");
        println!("pruned {} (last_seen={last_seen})", hash.as_str());
        removed += 1;
    }
    println!("Pruned {removed} user(s) older than {older_than_days} day(s).");
    Ok(())
}

fn is_stale(p: &UserProfile, threshold: chrono::DateTime<chrono::Utc>) -> bool {
    match p.last_seen_at.as_deref() {
        None => true, // never seen → stale
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&chrono::Utc) < threshold)
            .unwrap_or(true),
    }
}
