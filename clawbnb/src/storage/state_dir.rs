use std::path::PathBuf;
use std::sync::OnceLock;

static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn state_dir() -> &'static PathBuf {
    STATE_DIR.get_or_init(|| {
        if let Ok(val) = std::env::var("WECLAWBOT_STATE_DIR") {
            if !val.is_empty() {
                return PathBuf::from(val);
            }
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".weclawbot")
    })
}

pub fn accounts_dir() -> PathBuf {
    state_dir().join("accounts")
}

pub fn sync_dir() -> PathBuf {
    state_dir().join("sync")
}

pub fn config_path() -> PathBuf {
    if let Ok(val) = std::env::var("WECLAWBOT_CONFIG_PATH") {
        if !val.is_empty() {
            return PathBuf::from(val);
        }
    }
    state_dir().join("config.json")
}

pub fn logs_dir() -> PathBuf {
    state_dir().join("logs")
}

pub fn pid_file_path() -> PathBuf {
    state_dir().join("weclawbot.pid")
}

pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir())?;
    std::fs::create_dir_all(accounts_dir())?;
    std::fs::create_dir_all(sync_dir())?;
    std::fs::create_dir_all(logs_dir())?;
    Ok(())
}
