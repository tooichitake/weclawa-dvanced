//! Disk layout for weclawbot's per-user state.
//!
//! ```text
//! ~/.weclawbot/
//! ├── config.json              # weclawbot operational config
//! ├── defaults/
//! │   └── claude-settings.json # factory template for new users
//! ├── shared/
//! │   ├── credentials.json     -> ~/.claude/.credentials.json
//! │   └── plugins              -> ~/.claude/plugins (kept for legacy/inspection)
//! └── users/<u-hash>/
//!     ├── settings.json        # FULL claude-cli settings, autonomous
//!     ├── profile.json         # nickname, last_seen, msg_count, sync_state
//!     ├── history.json         # weclawbot-maintained convo history
//!     └── sandbox/
//!         ├── home/.claude/    # bind-mounted as $HOME/.claude/ in container
//!         │   ├── settings.json       # synced from ../../settings.json
//!         │   ├── plugins/            # RW per-user (PTY-installed)
//!         │   └── .credentials.json   # ro bind-mount from shared
//!         ├── work/                   # container cwd
//!         └── media/                  # WeChat inbound files
//! ```

use std::path::PathBuf;

use crate::storage::state_dir::state_dir;

/// All known WeChat users live under this directory, one subdir per
/// hashed user id.
pub fn users_root() -> PathBuf {
    state_dir().join("users")
}

pub fn user_dir(user_hash: &str) -> PathBuf {
    users_root().join(user_hash)
}

pub fn user_settings_path(user_hash: &str) -> PathBuf {
    user_dir(user_hash).join("settings.json")
}

pub fn user_profile_path(user_hash: &str) -> PathBuf {
    user_dir(user_hash).join("profile.json")
}

pub fn user_history_path(user_hash: &str) -> PathBuf {
    user_dir(user_hash).join("history.json")
}

pub fn user_sandbox_root(user_hash: &str) -> PathBuf {
    user_dir(user_hash).join("sandbox")
}

/// Factory defaults edited by the operator via GUI. Used to seed brand-new
/// users; never auto-merged into existing users.
pub fn defaults_root() -> PathBuf {
    state_dir().join("defaults")
}

pub fn defaults_settings_path() -> PathBuf {
    defaults_root().join("claude-settings.json")
}

/// Shared read-only resources mounted into every sandbox.
pub fn shared_root() -> PathBuf {
    state_dir().join("shared")
}

pub fn shared_credentials() -> PathBuf {
    shared_root().join("credentials.json")
}

/// Legacy path; kept around for `doctor` inspection but no longer mounted.
pub fn shared_plugins() -> PathBuf {
    shared_root().join("plugins")
}
