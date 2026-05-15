use std::fs;
use std::path::Path;

pub fn write_json_atomic(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent directory")?;
    fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;

    let temp_name = format!(
        ".weclawbot-{}-{}-{:x}.tmp",
        std::process::id(),
        chrono::Utc::now().timestamp_millis(),
        rand::random::<u32>(),
    );
    let temp_path = dir.join(temp_name);
    let content = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;

    fs::write(&temp_path, format!("{content}\n")).map_err(|e| format!("write tmp: {e}"))?;
    fs::rename(&temp_path, path).map_err(|e| {
        let _ = fs::remove_file(&temp_path);
        format!("rename: {e}")
    })
}
