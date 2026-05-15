//! Legacy JSON importer — v4.2 stub.
//!
//! v2 used to import `~/.weclawbot/users/*/*.json` into `state.db` on first
//! boot. v4.2 sqlx async runner handles fresh migrations directly; existing
//! state.db files from v2-v4.1 carry refinery_schema_history which v4.2's
//! migration runner detects and imports into _sqlx_migrations (no DDL re-run).
//!
//! Legacy JSON-only deployments (no state.db, only json files under
//! ~/.weclawbot/users/) are no longer auto-imported. Operators upgrading
//! from pre-Phase-1 v0/v1 binaries should first run v3.x once to populate
//! state.db, then upgrade to v4.2.

use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub accounts_imported: u64,
    pub defaults_imported: bool,
    pub users_imported: u64,
    pub user_settings_imported: u64,
    pub history_turns_imported: u64,
    pub bindings_imported: u64,
    pub errors: Vec<String>,
}

impl ImportReport {
    pub fn summary(&self) -> String {
        format!(
            "accounts={} users={} settings={} turns={} bindings={} errors={}",
            self.accounts_imported,
            self.users_imported,
            self.user_settings_imported,
            self.history_turns_imported,
            self.bindings_imported,
            self.errors.len()
        )
    }
}

#[allow(dead_code)]
pub fn should_auto_import(_state_base: &Path) -> bool {
    false
}
