//! Per-user sandbox disk quota (Phase 6.3).
//!
//! Each user's `work/` directory accumulates artefacts (generated
//! files, downloaded inbound media, claude-cli plugin caches). Without
//! a ceiling, one chatty user could fill the daemon's disk and take
//! every other user offline.
//!
//! Policy:
//! - `WORK_DIR_SOFT_LIMIT_BYTES` per user: warn + refuse to spawn new
//!   sandboxes for that user. Existing in-flight runs are not killed.
//! - Operators reset via `weclawbot users reset <hash>` (wipes the
//!   whole sandbox tree).
//!
//! The check is cheap (one `walk + sum file_size`); we cache the
//! computed size for `CACHE_TTL` to avoid hammering the FS on every
//! inbound message.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::sandbox::layout;

/// Per-user work-dir cap. 500 MiB picked to comfortably handle a
/// month of normal chat-derived artefacts while flagging runaway use
/// long before disk fills. Configurable later via `config_kv`.
pub const WORK_DIR_SOFT_LIMIT_BYTES: u64 = 500 * 1024 * 1024;

/// How long we trust a cached du calculation before re-walking. The
/// FS cost of a fresh walk is small (~ms) but per-message accumulates;
/// 30 s is short enough that a runaway run is caught within 10-20
/// messages but long enough to amortise.
const CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
struct CachedSize {
    bytes: u64,
    taken_at: Instant,
}

// Process-global one-row-per-user-hash cache. Mutex is fine — quota
// checks are off the hot path (one per inbound message, not per
// stream-token).
static CACHE: Mutex<Option<std::collections::HashMap<String, CachedSize>>> = Mutex::new(None);

/// Result of a quota check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaStatus {
    /// Under the limit — sandbox spawn is allowed.
    Ok { bytes: u64 },
    /// Over the limit — sandbox spawn must be denied.
    Exceeded { bytes: u64, limit: u64 },
    /// FS walk failed (permission, race with delete, …). Fail open —
    /// we'd rather risk one user filling disk than block every user
    /// because the daemon can't read its own state dir.
    Unknown,
}

impl QuotaStatus {
    pub fn allowed(&self) -> bool {
        !matches!(self, QuotaStatus::Exceeded { .. })
    }
    pub fn bytes(&self) -> Option<u64> {
        match self {
            QuotaStatus::Ok { bytes } => Some(*bytes),
            QuotaStatus::Exceeded { bytes, .. } => Some(*bytes),
            QuotaStatus::Unknown => None,
        }
    }
}

/// Check whether `user_hash`'s work-dir is within quota. The result
/// may be a cached one less than `CACHE_TTL` old.
pub fn check(user_hash: &str) -> QuotaStatus {
    if let Some(cached) = lookup_cached(user_hash) {
        return classify(cached);
    }
    let work_dir = layout::user_sandbox_root(user_hash).join("work");
    let bytes = match dir_size_bytes(&work_dir) {
        Some(b) => b,
        None => return QuotaStatus::Unknown,
    };
    store_cached(user_hash, bytes);
    classify(bytes)
}

/// Invalidate the cache for `user_hash` — call after destructive ops
/// like `weclawbot users reset` so the next check reflects reality
/// without waiting for TTL.
pub fn invalidate(user_hash: &str) {
    let mut g = CACHE.lock().expect("quota cache poisoned");
    if let Some(map) = g.as_mut() {
        map.remove(user_hash);
    }
}

fn classify(bytes: u64) -> QuotaStatus {
    if bytes > WORK_DIR_SOFT_LIMIT_BYTES {
        QuotaStatus::Exceeded {
            bytes,
            limit: WORK_DIR_SOFT_LIMIT_BYTES,
        }
    } else {
        QuotaStatus::Ok { bytes }
    }
}

fn lookup_cached(user_hash: &str) -> Option<u64> {
    let g = CACHE.lock().expect("quota cache poisoned");
    let map = g.as_ref()?;
    let entry = map.get(user_hash)?;
    if entry.taken_at.elapsed() > CACHE_TTL {
        None
    } else {
        Some(entry.bytes)
    }
}

fn store_cached(user_hash: &str, bytes: u64) {
    let mut g = CACHE.lock().expect("quota cache poisoned");
    let map = g.get_or_insert_with(std::collections::HashMap::new);
    map.insert(
        user_hash.to_string(),
        CachedSize {
            bytes,
            taken_at: Instant::now(),
        },
    );
}

fn dir_size_bytes(root: &Path) -> Option<u64> {
    if !root.exists() {
        return Some(0);
    }
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue, // race with rm — best-effort sum
        };
        for ent in entries.flatten() {
            let path = ent.path();
            let meta = match ent.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(path);
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_status_allows_spawn() {
        assert!(QuotaStatus::Ok { bytes: 0 }.allowed());
        assert!(QuotaStatus::Unknown.allowed()); // fail open
    }

    #[test]
    fn exceeded_status_denies_spawn() {
        assert!(!QuotaStatus::Exceeded {
            bytes: 1_000_000_000,
            limit: WORK_DIR_SOFT_LIMIT_BYTES
        }
        .allowed());
    }

    #[test]
    fn dir_size_handles_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert_eq!(dir_size_bytes(&missing), Some(0));
    }

    #[test]
    fn dir_size_sums_file_lengths() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), vec![0u8; 100]).unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub").join("b.txt"), vec![0u8; 250]).unwrap();
        assert_eq!(dir_size_bytes(tmp.path()), Some(350));
    }

    #[test]
    fn classify_boundary() {
        let just_under = classify(WORK_DIR_SOFT_LIMIT_BYTES - 1);
        assert!(matches!(just_under, QuotaStatus::Ok { .. }));
        let at_limit = classify(WORK_DIR_SOFT_LIMIT_BYTES);
        assert!(matches!(at_limit, QuotaStatus::Ok { .. }));
        let over = classify(WORK_DIR_SOFT_LIMIT_BYTES + 1);
        assert!(matches!(over, QuotaStatus::Exceeded { .. }));
    }
}
