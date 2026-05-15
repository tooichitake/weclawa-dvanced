//! Drive `claude` interactively via a pseudoterminal.
//!
//! ⚠️ **OPERATOR-ONLY** — this module spawns `claude` as a real interactive
//! TUI process. It MUST NEVER be reachable from a WeChat-driven dispatch
//! path. WeChat users only ever go through:
//!   - `src/ai/cli_provider.rs::invoke_claude` (non-interactive `-p
//!     --output-format stream-json`)
//!   - `src/console/` (read/write the user's `settings.json` directly, no
//!     claude binary spawn)
//!
//! The audited callers of this module are:
//!   - `src/service/routes.rs::post_apply_user_plugins` (HTTP admin route,
//!     operator-gated GUI button — not WeChat)
//! If you add a new caller, double-check it's NOT downstream of WeChat
//! inbound message handling. The plugin-install code below also auto-
//! confirms `[y/N]` prompts, which is appropriate ONLY because the operator
//! already opted in via GUI; it would be a privilege-escalation hazard if a
//! WeChat user could trigger it.
//!
//! Used by weclawbot for two things:
//!   1. Per-user plugin install/uninstall (reconcile against the user's
//!      `settings.json` `enabledPlugins`).
//!   2. (Future) Enumerate claude's slash commands + config schema for the
//!      schema-driven GUI editor.
//!
//! The TUI is messy — ANSI escapes, redraw on every keystroke, banner +
//! tips at start. We use `vt100` to maintain a virtual terminal screen and
//! scan it for completion markers / prompts. Any approval prompt is
//! auto-answered "yes" (operator already opted in via GUI).

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tracing::{debug, info, warn};

use crate::sandbox::Sandbox;

/// Wait for at most this long for any single slash-command interaction.
const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(90);
/// Quiet period after the last output byte considered "done".
const QUIET_PERIOD: Duration = Duration::from_millis(800);

/// One automated claude session: spawn -> send commands -> /quit -> wait.
pub struct ClaudeSession {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    screen: Arc<Mutex<vt100::Parser>>,
    last_byte_at: Arc<Mutex<Instant>>,
}

impl ClaudeSession {
    // v7.0 housekeeping: `spawn_host()` removed — never called (host-side
    // claude preflight is done via a one-shot subprocess in
    // `sandbox::preflight`, not a long-lived PTY). Restore from git if a
    // future `weclawbot plugins list-host` command needs it.

    /// Spawn `podman run ... <image> claude` for a given user sandbox.
    /// Plugin installs land in that sandbox's writable plugins/ dir.
    pub fn spawn_in_sandbox(sandbox: &Sandbox) -> Result<Self, String> {
        let cmd = crate::sandbox::exec::build_claude_cmd(sandbox, &[]);
        // build_claude_cmd uses tokio::process::Command; we need the std-cmd
        // shape that portable-pty wants. Recreate from cmdline:
        let program = cmd.as_std().get_program().to_string_lossy().to_string();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        spawn_inner(&program, &args, None)
    }

    /// Send a single line (will append `\n`). The TUI receives it as user
    /// input on the prompt.
    pub fn send_line(&mut self, line: &str) -> Result<(), String> {
        self.writer
            .write_all(line.as_bytes())
            .and_then(|_| self.writer.write_all(b"\n"))
            .and_then(|_| self.writer.flush())
            .map_err(|e| format!("pty write: {e}"))
    }

    /// Wait until the screen contains any of the given substrings, or the
    /// output goes quiet (no new bytes for QUIET_PERIOD), or timeout.
    /// Returns the matching substring, or None on quiet/timeout.
    pub fn wait_for_any(
        &self,
        needles: &[&str],
        timeout: Duration,
    ) -> Result<Option<String>, String> {
        let start = Instant::now();
        loop {
            if start.elapsed() > timeout {
                return Err(format!(
                    "timeout after {timeout:?}; screen tail: {}",
                    self.screen_tail(400)
                ));
            }

            let screen_text = self.screen_text();
            for n in needles {
                if screen_text.contains(n) {
                    return Ok(Some((*n).to_string()));
                }
            }
            // We used to try to auto-answer "[y/N]" prompts here, but the
            // `writer_send_raw` workaround was always a no-op (the writer
            // lives behind `&mut self` and we hold `&self`). The honest
            // behavior is: if claude blocks on an approval prompt, log it
            // and let the outer timeout fire — the operator can rerun.
            if let Some(prompt) = self.detect_approval_prompt(&screen_text) {
                debug!(
                    "claude TUI is waiting on approval prompt — letting it time out: {prompt}"
                );
            }

            let last = *self.last_byte_at.lock().unwrap();
            if last.elapsed() > QUIET_PERIOD {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn screen_text(&self) -> String {
        self.screen.lock().unwrap().screen().contents()
    }

    pub fn screen_tail(&self, max_chars: usize) -> String {
        let full = self.screen_text();
        if full.len() <= max_chars {
            full
        } else {
            full[full.len() - max_chars..].to_string()
        }
    }

    // v7.0 housekeeping: `raw_dump()` removed — was a debug aid for
    // capturing raw VT100 bytes during plugin-install regressions; the
    // current `wait_for_any` error already includes a screen tail.

    fn detect_approval_prompt(&self, text: &str) -> Option<String> {
        for pattern in [
            "[y/N]",
            "[Y/n]",
            "(y/N)",
            "(Y/n)",
            "Confirm",
            "Are you sure",
        ] {
            if let Some(idx) = text.rfind(pattern) {
                let start = idx.saturating_sub(40);
                return Some(text[start..(idx + pattern.len()).min(text.len())].to_string());
            }
        }
        None
    }

    pub fn quit_and_wait(mut self, timeout: Duration) -> Result<i32, String> {
        // Try slash quit, fall back to SIGTERM via child.kill()
        let _ = self.send_line("/quit");
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().map_err(|e| format!("wait: {e}"))? {
                Some(status) => {
                    return Ok(status.exit_code() as i32);
                }
                None => {
                    if Instant::now() > deadline {
                        let _ = self.child.kill();
                        return Err("claude did not exit after /quit; killed".into());
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

fn spawn_inner(
    program: &str,
    args: &[String],
    extra_env: Option<&[(&str, &str)]>,
) -> Result<ClaudeSession, String> {
    let pty_sys = native_pty_system();
    let pair = pty_sys
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;

    let mut builder = CommandBuilder::new(program);
    for a in args {
        builder.arg(a);
    }
    builder.env("TERM", "xterm-256color");
    builder.env("NO_COLOR", "1");
    if let Some(envs) = extra_env {
        for (k, v) in envs {
            builder.env(k, v);
        }
    }

    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| format!("spawn: {e}"))?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().map_err(|e| format!("clone reader: {e}"))?;
    let writer = pair.master.take_writer().map_err(|e| format!("take writer: {e}"))?;

    let screen = Arc::new(Mutex::new(vt100::Parser::new(40, 120, 0)));
    let last_byte_at = Arc::new(Mutex::new(Instant::now()));

    let screen_for_thread = Arc::clone(&screen);
    let last_for_thread = Arc::clone(&last_byte_at);
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    {
                        let mut s = screen_for_thread.lock().unwrap();
                        s.process(&buf[..n]);
                    }
                    *last_for_thread.lock().unwrap() = Instant::now();
                }
                Err(e) => {
                    debug!("pty read error: {e}");
                    break;
                }
            }
        }
    });

    Ok(ClaudeSession {
        child,
        writer,
        screen,
        last_byte_at,
    })
}

// -------------------- High-level plugin operations --------------------

pub fn install_plugin_in_sandbox(sandbox: &Sandbox, spec: &str) -> Result<(), String> {
    let (plugin, marketplace) = match spec.split_once('@') {
        Some((p, m)) if !p.is_empty() && !m.is_empty() => (p, m),
        _ => return Err(format!("invalid plugin spec '{spec}', expected name@marketplace")),
    };

    info!("[{}] install plugin {plugin}@{marketplace}", sandbox.user_hash);
    let mut sess = ClaudeSession::spawn_in_sandbox(sandbox)?;

    // Wait for claude to reach ready state (banner usually has "tip:" or "?" prompt)
    sess.wait_for_any(&["?", "tip:", "›"], DEFAULT_STEP_TIMEOUT)?;

    // Add marketplace then install
    sess.send_line(&format!("/plugin marketplace add {marketplace}"))?;
    let r1 = sess.wait_for_any(
        &["added", "already", "error", "Error", "failed"],
        DEFAULT_STEP_TIMEOUT,
    )?;
    debug!("[{}] marketplace add reply: {:?}", sandbox.user_hash, r1);

    sess.send_line(&format!("/plugin install {plugin}@{marketplace}"))?;
    let r2 = sess.wait_for_any(
        &["installed", "already", "error", "Error", "failed"],
        Duration::from_secs(180),
    )?;
    debug!("[{}] install reply: {:?}", sandbox.user_hash, r2);

    let exit = sess.quit_and_wait(Duration::from_secs(20))?;
    if exit != 0 {
        warn!("[{}] claude exited {exit} during install", sandbox.user_hash);
    }
    Ok(())
}

pub fn uninstall_plugin_in_sandbox(sandbox: &Sandbox, spec: &str) -> Result<(), String> {
    info!("[{}] uninstall plugin {spec}", sandbox.user_hash);
    let mut sess = ClaudeSession::spawn_in_sandbox(sandbox)?;
    sess.wait_for_any(&["?", "tip:", "›"], DEFAULT_STEP_TIMEOUT)?;
    sess.send_line(&format!("/plugin uninstall {spec}"))?;
    let _ = sess.wait_for_any(
        &["uninstalled", "removed", "not installed", "error"],
        DEFAULT_STEP_TIMEOUT,
    )?;
    let _ = sess.quit_and_wait(Duration::from_secs(20))?;
    Ok(())
}

// -------------------- Reconcile --------------------

/// Compare the user's `settings.json` `enabledPlugins` to what's currently
/// installed in the sandbox's `plugins/cache/`, and run install/uninstall
/// to make them match.
///
/// Best-effort: any single plugin failure is logged but doesn't abort the
/// rest. Returns a structured report for the caller to surface in GUI.
pub fn reconcile_user_plugins(sandbox: &Sandbox) -> ReconcileReport {
    let desired = read_desired_plugins(sandbox);
    let installed = read_installed_plugins(sandbox);

    let mut report = ReconcileReport {
        added: Vec::new(),
        removed: Vec::new(),
        errors: Vec::new(),
    };

    for spec in &desired {
        if !installed.contains(spec) {
            match install_plugin_in_sandbox(sandbox, spec) {
                Ok(()) => report.added.push(spec.clone()),
                Err(e) => report.errors.push(format!("install {spec}: {e}")),
            }
        }
    }
    for spec in &installed {
        if !desired.contains(spec) {
            match uninstall_plugin_in_sandbox(sandbox, spec) {
                Ok(()) => report.removed.push(spec.clone()),
                Err(e) => report.errors.push(format!("uninstall {spec}: {e}")),
            }
        }
    }

    update_profile_sync_state(sandbox, &report);
    report
}

#[derive(Debug, serde::Serialize)]
pub struct ReconcileReport {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub errors: Vec<String>,
}

fn read_desired_plugins(sandbox: &Sandbox) -> Vec<String> {
    let raw = match std::fs::read_to_string(sandbox.settings_path()) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v.get("enabledPlugins")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn read_installed_plugins(sandbox: &Sandbox) -> Vec<String> {
    // Plugins live at <sandbox>/home/.claude/plugins/cache/<plugin-id>/
    // The id is opaque ("document-skills-anthropic-agent-skills" style),
    // not necessarily "name@marketplace". We try to recover the spec from
    // each plugin's .claude-plugin/plugin.json marketplace metadata.
    let cache_dir = sandbox.plugins().join("cache");
    let read = match std::fs::read_dir(&cache_dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for entry in read.flatten() {
        let manifest = entry.path().join(".claude-plugin/plugin.json");
        if let Ok(raw) = std::fs::read_to_string(&manifest) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let (Some(name), Some(market)) = (
                    v.get("name").and_then(|x| x.as_str()),
                    v.get("marketplace").and_then(|x| x.as_str()),
                ) {
                    out.push(format!("{name}@{market}"));
                    continue;
                }
            }
        }
        // Fallback: use directory name as best-effort id (won't always match spec)
        if let Some(n) = entry.file_name().to_str() {
            out.push(n.to_string());
        }
    }
    out
}

fn update_profile_sync_state(sandbox: &Sandbox, report: &ReconcileReport) {
    let path = sandbox.profile_path();
    let mut v = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = v.as_object_mut() {
        let state = if report.errors.is_empty() {
            "synced"
        } else {
            "out_of_sync"
        };
        obj.insert("sync_state".into(), serde_json::json!(state));
        obj.insert(
            "last_sync_at".into(),
            serde_json::json!(chrono::Utc::now().to_rfc3339()),
        );
        obj.insert(
            "last_sync_error".into(),
            if report.errors.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!(report.errors.join("; "))
            },
        );
    }
    let _ = crate::storage::atomic_write::write_json_atomic(&path, &v);
}
