use std::fs;
use std::path::PathBuf;

use super::state_dir::sync_dir;

pub fn sync_buf_path(account_id: &str) -> PathBuf {
    sync_dir().join(format!("{account_id}.syncbuf"))
}

pub fn load_sync_buf(account_id: &str) -> Option<String> {
    let path = sync_buf_path(account_id);
    fs::read_to_string(&path).ok().and_then(|s| {
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

pub fn save_sync_buf(account_id: &str, buf: &str) {
    let path = sync_buf_path(account_id);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(&path, buf);
}
