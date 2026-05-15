use std::fs;
use std::path::{Path, PathBuf};

/// Save bytes to `<target_dir>/<basename><ext>` and return the absolute path.
///
/// In the sandbox-per-user model, callers pass `sandbox.media_inbound()`.
pub fn save_inbound(
    target_dir: &Path,
    basename: &str,
    ext: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    fs::create_dir_all(target_dir)
        .map_err(|e| format!("mkdir {}: {e}", target_dir.display()))?;
    let safe_base = sanitize(basename);
    let safe_ext = if ext.starts_with('.') {
        ext.to_string()
    } else {
        format!(".{ext}")
    };
    let path = target_dir.join(format!("{safe_base}{safe_ext}"));
    fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            _ => '-',
        })
        .collect()
}
