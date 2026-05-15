//! POSIX symlink helpers. Linux-only deployment, so no fallback needed.

use std::fs;
use std::io;
use std::path::Path;

/// Ensure that `link` is a symlink pointing at `target`.
/// - If the link already exists with the correct target, do nothing.
/// - If it exists with the wrong target, unlink and recreate.
/// - If it doesn't exist, create it.
pub fn ensure_symlink(target: &Path, link: &Path) -> Result<(), String> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    match fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            match fs::read_link(link) {
                Ok(existing) if existing == target => return Ok(()),
                _ => {
                    fs::remove_file(link)
                        .map_err(|e| format!("unlink stale {}: {e}", link.display()))?;
                }
            }
        }
        Ok(_) => {
            // Exists but not a symlink — back up and replace.
            let bak = link.with_extension("bak");
            fs::rename(link, &bak)
                .map_err(|e| format!("backup {} -> {}: {e}", link.display(), bak.display()))?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("stat {}: {e}", link.display())),
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .map_err(|e| format!("symlink {} -> {}: {e}", link.display(), target.display()))?;
    }
    #[cfg(not(unix))]
    {
        // Not supported off-Unix; weclawbot is Linux-only in production.
        return Err(format!(
            "symlink not supported on this platform (link={} target={})",
            link.display(),
            target.display()
        ));
    }
    Ok(())
}
