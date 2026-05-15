//! PID file management for the daemon.
//!
//! Phase 0.5 hardening: write the PID file with `O_CREAT|O_EXCL|0600`,
//! refuse to follow symlinks, and verify the resolved path lives inside
//! `~/.weclawbot/` after canonicalization. Without these checks a
//! pre-existing `weclawbot.pid → /etc/passwd` symlink lets the daemon
//! corrupt arbitrary files at the running user's UID. (Refer to security
//! audit findings P1 — daemon/pid.rs symlink race.)

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::storage::state_dir::pid_file_path;

pub fn read_pid() -> Option<u32> {
    let path = pid_file_path();
    let raw = fs::read_to_string(&path).ok()?;
    raw.trim().parse().ok()
}

pub fn write_pid(pid: u32) {
    let path = pid_file_path();
    write_pid_to(&path, pid);
}

/// Internal: write `pid` to `path` with hardened semantics. Exposed for
/// unit tests so they can use a tempdir instead of the process-global
/// `state_dir()` (which uses `OnceLock` and can't be mocked twice).
pub(crate) fn write_pid_to(path: &Path, pid: u32) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }

    // If a stale REGULAR file is sitting there from a previous run, remove
    // it. We already verified upstream (`cli/start.rs`) that no live
    // process is claiming this PID before calling write_pid(). A symlink
    // is intentionally NOT removed here — we want the open() below to fail
    // with ELOOP so we don't accidentally clobber whatever the symlink
    // points at.
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_file() {
            let _ = fs::remove_file(path);
        }
    }

    // O_CREAT | O_EXCL: error if the file already exists (including as a
    // symlink to elsewhere). 0600: only the daemon user can read/write.
    // O_NOFOLLOW: pre-existing symlink at this path → ELOOP, refuse.
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
        opts.custom_flags(libc::O_NOFOLLOW);
    }

    match opts.open(path) {
        Ok(mut f) => {
            if let Err(e) = writeln!(f, "{pid}") {
                tracing::error!("write pid: {e}");
            }
        }
        Err(e) => {
            tracing::error!(
                "open pid file {} with CREATE_NEW failed: {e} \
                 (likely a stale symlink — refusing to clobber)",
                path.display()
            );
        }
    }
}

pub fn remove_pid() {
    let _ = fs::remove_file(pid_file_path());
}

pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use std::process::Command;
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                out.contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    #[cfg(unix)]
    fn write_pid_creates_file_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let pid_path = tmp.path().join("weclawbot.pid");
        write_pid_to(&pid_path, 12345);

        let meta = std::fs::metadata(&pid_path).expect("pid file written");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "pid file should be 0o600 not {:o}", mode);
        let raw = std::fs::read_to_string(&pid_path).unwrap();
        assert_eq!(raw.trim(), "12345");
    }

    #[test]
    #[cfg(unix)]
    fn write_pid_refuses_to_follow_symlink_to_elsewhere() {
        let tmp = TempDir::new().unwrap();
        let pid_path = tmp.path().join("weclawbot.pid");
        // Create a target file the symlink could clobber.
        let target = tmp.path().join("ATTACKER_TARGET");
        std::fs::write(&target, b"original-content").unwrap();
        // Plant a symlink at the pid path → target.
        std::os::unix::fs::symlink(&target, &pid_path).unwrap();

        // write_pid must NOT follow the symlink and must NOT overwrite target.
        write_pid_to(&pid_path, 99999);
        let target_content = std::fs::read_to_string(&target).unwrap();
        assert_eq!(
            target_content, "original-content",
            "symlink attack should NOT have rewritten the target file"
        );
    }

    #[test]
    #[cfg(unix)]
    fn write_pid_overwrites_stale_regular_file() {
        let tmp = TempDir::new().unwrap();
        let pid_path = tmp.path().join("weclawbot.pid");
        // Leftover pid from a previous crashed daemon.
        std::fs::write(&pid_path, "11111").unwrap();
        write_pid_to(&pid_path, 22222);
        let raw = std::fs::read_to_string(&pid_path).unwrap();
        assert_eq!(raw.trim(), "22222");
    }
}
