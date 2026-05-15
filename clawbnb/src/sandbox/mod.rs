//! Per-user sandbox management.
//!
//! Each WeChat user gets a dedicated tree under `~/.weclawbot/users/<hash>/`.
//! The directory holds:
//!   - the user's full claude-cli settings (autonomous after init from defaults)
//!   - profile + sync metadata
//!   - conversation history
//!   - sandbox/ — where the per-message gVisor container's $HOME lives
//!
//! Spawning `claude -p` is wrapped by `bwrap`-style isolation; here we use
//! podman + runsc (gVisor). See `exec.rs`.

pub mod exec;
pub mod layout;
pub mod lifecycle;
pub mod linker;
pub mod materialize;
pub mod quota;
pub mod reconciler;

use std::path::{Path, PathBuf};

use sha1::Sha1;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct Sandbox {
    pub user_hash: String,
    pub user_dir: PathBuf,
}

impl Sandbox {
    pub fn user_dir(&self) -> &Path {
        &self.user_dir
    }
    pub fn sandbox_root(&self) -> PathBuf {
        self.user_dir.join("sandbox")
    }
    pub fn home(&self) -> PathBuf {
        self.sandbox_root().join("home")
    }
    pub fn home_claude(&self) -> PathBuf {
        self.home().join(".claude")
    }
    pub fn plugins(&self) -> PathBuf {
        self.home_claude().join("plugins")
    }
    pub fn work(&self) -> PathBuf {
        self.sandbox_root().join("work")
    }
    pub fn media(&self) -> PathBuf {
        self.sandbox_root().join("media")
    }
    pub fn media_inbound(&self) -> PathBuf {
        self.media().join("inbound")
    }
    pub fn settings_path(&self) -> PathBuf {
        layout::user_settings_path(&self.user_hash)
    }
    pub fn profile_path(&self) -> PathBuf {
        layout::user_profile_path(&self.user_hash)
    }
    pub fn history_path(&self) -> PathBuf {
        layout::user_history_path(&self.user_hash)
    }

    /// Ensure all required directories exist for this user and that the
    /// settings file has been materialized from defaults (first run only).
    /// Idempotent; safe to call on every message.
    pub fn ensure(user_id: &str) -> Result<Self, String> {
        // Phase 6.4: 旧用户保持 SHA-1，新用户用 SHA-256。lookup 函数
        // 一次性把这个决定做掉。
        let user_hash = hash_user_id_for_lookup(user_id);
        let user_dir = layout::user_dir(&user_hash);
        // v2.1.C2: metric — sandbox 创建/复用情况。Phase 5.1 引入限流后
        // 我们关心 sandbox 真正 spawn 失败的频率。is_new 标识是否新建。
        let is_new = !user_dir.exists();
        let result = Self::ensure_inner(user_id, &user_hash, &user_dir);
        metrics::counter!(
            "weclawbot_sandbox_spawn_total",
            "result" => if result.is_ok() { "ok" } else { "error" },
            "is_new" => if is_new { "true" } else { "false" }
        )
        .increment(1);
        result
    }

    fn ensure_inner(user_id: &str, user_hash: &str, user_dir: &std::path::Path) -> Result<Self, String> {
        let user_hash = user_hash.to_string();
        let user_dir = user_dir.to_path_buf();

        // Top-level dirs
        for dir in [
            user_dir.clone(),
            user_dir.join("sandbox"),
            user_dir.join("sandbox").join("home"),
            user_dir.join("sandbox").join("home").join(".claude"),
            user_dir.join("sandbox").join("home").join(".claude").join("plugins"),
            user_dir.join("sandbox").join("home").join(".claude").join("projects"),
            user_dir.join("sandbox").join("work"),
            user_dir.join("sandbox").join("media"),
            user_dir.join("sandbox").join("media").join("inbound"),
        ] {
            std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }

        // First-run init: seed user_settings from defaults if missing.
        if crate::defaults::load_user_settings(&user_hash).is_none() {
            crate::defaults::init_user_settings(&user_hash)
                .map_err(|e| format!("init settings for {user_hash}: {e}"))?;
        }

        // Sync user settings.json into the sandbox's claude config dir
        // (the sandbox copy stays as a file because claude-cli reads it).
        materialize::sync_user_settings_into_sandbox(&user_hash)?;

        // v4.2: sqlx async via block_on_async bridge (Sandbox::ensure 是
        // 同步 API，调用方多在 async context — daemon 主循环；CLI
        // 子命令在 tokio runtime 内也成立)。
        use crate::ids::UserHash;
        use crate::repo::users_async::SqlxUserRepo;
        use crate::runtime::blocking::block_on_async;
        if let Some(apool) = crate::storage::db_async::try_global_async_pool() {
            let r = SqlxUserRepo::new(apool);
            let hash = UserHash::new(user_hash.clone());
            let user_id_redacted = redact_user_id(user_id);
            let user_hash_owned = user_hash.clone();
            block_on_async(async move {
                let exists = r
                    .get_profile(&hash)
                    .await
                    .map_err(|e| format!("get profile: {e}"))?
                    .is_some();
                if !exists {
                    let now = chrono::Utc::now().to_rfc3339();
                    r.upsert_profile(&crate::repo::users::UserProfile {
                        hash: UserHash::new(user_hash_owned),
                        user_id_hint: Some(user_id_redacted),
                        created_at: now,
                        last_seen_at: None,
                        message_count: 0,
                        sync_state: "unknown".into(),
                        last_sync_at: None,
                        last_sync_error: None,
                    })
                    .await
                    .map_err(|e| format!("write profile: {e}"))?;
                }
                Ok::<(), String>(())
            })?;
        }

        Ok(Self { user_hash, user_dir })
    }

    /// Bump last_seen / message_count after a message is processed.
    pub fn touch_profile(&self) {
        use crate::ids::UserHash;
        use crate::repo::users_async::SqlxUserRepo;
        use crate::runtime::blocking::block_on_async;
        let Some(apool) = crate::storage::db_async::try_global_async_pool() else { return };
        let r = SqlxUserRepo::new(apool);
        let hash = UserHash::new(self.user_hash.clone());
        let user_hash_for_log = self.user_hash.clone();
        block_on_async(async move {
            if let Err(e) = r.touch_last_seen(&hash).await {
                tracing::warn!("touch_last_seen({user_hash_for_log}): {e}");
            }
            if let Err(e) = r.incr_message_count(&hash).await {
                tracing::warn!("incr_message_count({user_hash_for_log}): {e}");
            }
        });
    }
}

/// Stable short hash for a WeChat user id. Keeps the raw id off disk.
///
/// Phase 6.4 history & compatibility:
/// - v0 → 6.3 used SHA-1 (`u-` + first 12 hex chars of SHA-1 digest).
/// - 6.4+ uses SHA-256 for new users. SHA-1 is not actually a crypto
///   threat here (we only de-duplicate, never authenticate), but
///   shipping `Sha1` in deps reads as a smell during audit.
/// - To avoid disturbing existing users (who already have FK rows in
///   `users`/`user_settings`/`user_history`/`console_sessions`), the
///   lookup is **compatibility-first**: if a SHA-256 hash has no row
///   in `users` yet, we fall back to the SHA-1 hash; if THAT row
///   exists, we use it. So everyone who logged in before 6.4 keeps
///   the same `u-XXXXXX`, and only brand-new users get SHA-256.
///
/// The decision is made in `hash_user_id_for_lookup` which checks
/// the DB. Plain `hash_user_id` (no DB access) always returns the
/// SHA-256 form — use it for new-user creation paths.
pub fn hash_user_id(user_id: &str) -> String {
    hash_user_id_v2_sha256(user_id)
}

/// SHA-256 form. New users post-6.4 get this. Caller asserts there's
/// no existing SHA-1 row in the DB for this user_id (or doesn't care).
pub fn hash_user_id_v2_sha256(user_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(user_id.as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("u-{}", &hex[..12])
}

/// SHA-1 form. Only used for the compatibility lookup below — never
/// produce new SHA-1 hashes from code paths outside of this.
pub fn hash_user_id_v1_sha1(user_id: &str) -> String {
    use sha1::Digest as _;
    let mut h = Sha1::new();
    h.update(user_id.as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("u-{}", &hex[..12])
}

/// Compatibility-aware lookup: prefer SHA-256, fall back to SHA-1 if
/// the SHA-256 row doesn't exist but the SHA-1 one does. Use this
/// anywhere you're resolving an existing user's hash from the raw
/// `from_user_id` WeChat field. New-user creation should call
/// `hash_user_id` directly (always SHA-256).
pub fn hash_user_id_for_lookup(user_id: &str) -> String {
    let v2 = hash_user_id_v2_sha256(user_id);
    if user_hash_exists(&v2) {
        return v2;
    }
    let v1 = hash_user_id_v1_sha1(user_id);
    if user_hash_exists(&v1) {
        return v1;
    }
    v2 // new user — use SHA-256
}

fn user_hash_exists(hash: &str) -> bool {
    use crate::ids::UserHash;
    use crate::repo::users_async::SqlxUserRepo;
    use crate::runtime::blocking::block_on_async;
    let Some(apool) = crate::storage::db_async::try_global_async_pool() else {
        return false;
    };
    let r = SqlxUserRepo::new(apool);
    let hash_owned = hash.to_string();
    block_on_async(async move {
        matches!(r.get_profile(&UserHash::new(hash_owned)).await, Ok(Some(_)))
    })
}

fn redact_user_id(user_id: &str) -> String {
    let n = user_id.chars().count();
    if n <= 6 {
        return "***".into();
    }
    let head: String = user_id.chars().take(3).collect();
    let tail: String = user_id.chars().rev().take(3).collect::<String>().chars().rev().collect();
    format!("{head}***{tail}")
}

/// All user hashes known to the daemon. Phase 1d: reads from the
/// `users` table; falls back to the on-disk dir listing if the DB isn't
/// yet open (e.g. when invoked from a CLI subcommand that runs before
/// `cli::start` initialised the pool).
pub fn list_user_hashes() -> Vec<String> {
    use crate::repo::users_async::SqlxUserRepo;
    use crate::runtime::blocking::block_on_async;
    block_on_async(async {
        let pool = match crate::storage::db_async::try_global_async_pool() {
            Some(p) => p,
            None => match crate::storage::db_async::open_default().await {
                Ok(p) => p,
                Err(_) => return Vec::new(),
            },
        };
        let r = SqlxUserRepo::new(pool);
        if let Ok(profiles) = r.list_profiles().await {
            let mut out: Vec<String> =
                profiles.into_iter().map(|p| p.hash.into_string()).collect();
            out.sort();
            return out;
        }
        Vec::new()
    })
}

#[derive(Debug)]
pub struct PreflightReport {
    pub podman: Option<exec::PodmanInfo>,
    pub runsc: Option<exec::RuntimeInfo>,
    pub runsc_smoke_ok: Option<bool>,
    pub image_ref: String,
    pub image_cached: bool,
    pub operator_credentials_present: bool,
    pub operator_plugins_present: bool,
    pub errors: Vec<String>,
}

impl PreflightReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Startup checks. Materializes `shared/` symlinks; verifies podman+runsc
/// stack is operational.
pub fn preflight() -> PreflightReport {
    let mut errors: Vec<String> = Vec::new();

    let operator_claude = match dirs::home_dir() {
        Some(h) => h.join(".claude"),
        None => {
            errors.push("could not resolve $HOME".into());
            return empty_report(errors);
        }
    };
    let credentials = operator_claude.join(".credentials.json");
    let plugins = operator_claude.join("plugins");

    let operator_credentials_present = credentials.exists();
    let operator_plugins_present = plugins.exists();
    if !operator_credentials_present {
        errors.push(format!(
            "operator credentials missing at {} — run `claude login` first",
            credentials.display()
        ));
    }

    if let Err(e) = std::fs::create_dir_all(layout::shared_root()) {
        errors.push(format!("mkdir shared: {e}"));
    }
    if operator_credentials_present {
        if let Err(e) = linker::ensure_symlink(&credentials, &layout::shared_credentials()) {
            errors.push(format!("link credentials: {e}"));
        }
    }
    if operator_plugins_present {
        if let Err(e) = linker::ensure_symlink(&plugins, &layout::shared_plugins()) {
            errors.push(format!("link plugins (informational): {e}"));
        }
    }

    // Ensure defaults file exists; if not, materialize an empty starting set.
    if let Err(e) = crate::defaults::ensure_defaults_exist() {
        errors.push(format!("init defaults: {e}"));
    }

    let podman = exec::detect_podman();
    if podman.is_none() {
        errors.push(
            "podman not found — install: `apt install podman` / `dnf install podman`".into(),
        );
    }
    let runsc = exec::detect_runsc();
    if runsc.is_none() {
        errors.push(
            "runsc (gVisor) not found — install per https://gvisor.dev/docs/user_guide/install/"
                .into(),
        );
    } else if let Some(r) = &runsc {
        if !r.registered {
            errors.push(
                "runsc is installed but not registered as a podman runtime — add to \
                 ~/.config/containers/containers.conf under [engine.runtimes]"
                    .into(),
            );
        }
    }

    let runsc_smoke_ok = if podman.is_some() && runsc.is_some() {
        match exec::runsc_smoke_test() {
            Ok(()) => Some(true),
            Err(e) => {
                errors.push(format!(
                    "gVisor smoke test failed: {e}; check kernel.unprivileged_userns_clone=1"
                ));
                Some(false)
            }
        }
    } else {
        None
    };

    let image_ref = exec::sandbox_image();
    let image_cached = match exec::ensure_image(&image_ref) {
        Ok(()) => true,
        Err(e) => {
            errors.push(format!(
                "sandbox image {image_ref} not available: {e}; try `podman pull {image_ref}`"
            ));
            false
        }
    };

    PreflightReport {
        podman,
        runsc,
        runsc_smoke_ok,
        image_ref,
        image_cached,
        operator_credentials_present,
        operator_plugins_present,
        errors,
    }
}

fn empty_report(errors: Vec<String>) -> PreflightReport {
    PreflightReport {
        podman: None,
        runsc: None,
        runsc_smoke_ok: None,
        image_ref: exec::sandbox_image(),
        image_cached: false,
        operator_credentials_present: false,
        operator_plugins_present: false,
        errors,
    }
}
