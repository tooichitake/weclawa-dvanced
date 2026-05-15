//! File / URL forwarding for the AI reply path.
//!
//! Two sources of files get delivered to the WeChat user:
//!
//! 1. **MCP `attach` tool calls** — Claude declares each deliverable via
//!    `mcp__weclawbot__attach`. The host parses stream-json `tool_use`
//!    events and accumulates them in `ClaudeOutput.generated_files`. This
//!    is the primary path (matches OpenClaw / ChatGPT pattern).
//!
//! 2. **Filesystem diff fallback** — for the rare case where Claude
//!    forgets to call `attach`. We snapshot `/work/output/` before the
//!    CLI runs (`snapshot_before`) and again after; the delta is
//!    forwarded. Gated on CLI success — a timed-out run does NOT trigger
//!    the diff (or we'd leak half-written helper files).
//!
//! Helper scripts must live in `/tmp/` per the system prompt rule
//! (`build_prompt` in `ai/cli_provider.rs`), so `output_scan_dirs` only
//! covers `/work/output/`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::ai::cli_provider::{AttachedUrl, GeneratedFile};
use crate::api::client::ILinkClient;
use crate::api::types::WeixinMessage;
use crate::media::outbound;
use crate::sandbox::Sandbox;

/// `(path, mtime_ms)` for every file under the user-visible output dirs.
/// Used to diff pre/post-CLI to catch files Claude forgot to `attach`.
pub type OutputSnapshot = std::collections::HashMap<PathBuf, i64>;

/// Snapshot the contents of every directory the diff-fallback monitors.
/// Call **before** the CLI runs.
pub fn snapshot(sandbox: &Sandbox) -> OutputSnapshot {
    let mut out = OutputSnapshot::new();
    for dir in scan_dirs(sandbox) {
        walk_into(&dir, &mut out);
    }
    out
}

/// User-visible delivery directories. Per the Anthropic-skill `/tmp/`
/// scratch convention the prompt pushes to Claude, helper scripts go to
/// `/tmp/` (not scanned) and only deliverables land in `/work/output/`.
fn scan_dirs(sandbox: &Sandbox) -> Vec<PathBuf> {
    vec![sandbox.work().join("output")]
}

fn walk_into(dir: &Path, into: &mut OutputSnapshot) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_file() {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            into.insert(path, mtime);
        } else if meta.is_dir() {
            // Recurse one level only — avoid pulling in nested `.claude/`
            // / `projects/` / cache trees that Claude maintains on its own.
            walk_into(&path, into);
        }
    }
}

/// Compute the diff against `before` and return new / modified files in
/// "most-recently-created first" order. Excludes Claude's own state files.
pub fn collect_new(sandbox: &Sandbox, before: OutputSnapshot) -> Vec<PathBuf> {
    let after = snapshot(sandbox);
    let mut new_files: Vec<PathBuf> = Vec::new();
    for (path, mtime) in after {
        let is_new = before.get(&path).map(|m| *m != mtime).unwrap_or(true);
        if !is_new {
            continue;
        }
        if is_internal_state(&path) {
            continue;
        }
        if std::fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(true) {
            continue; // skip empty files
        }
        new_files.push(path);
    }
    new_files.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .map(std::cmp::Reverse)
            .unwrap_or(std::cmp::Reverse(std::time::SystemTime::UNIX_EPOCH))
    });
    new_files
}

fn is_internal_state(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.contains("/.claude/")
        || s.ends_with("/.claude.json")
        || s.contains("/.cache/")
        || s.contains("/projects/")
        || s.ends_with(".log")
}

/// Deliver all files Claude declared via MCP `attach` to the WeChat user.
/// Returns the set of host paths that were successfully forwarded so the
/// caller can skip them in the diff fallback.
pub async fn forward_mcp_files(
    client: &ILinkClient,
    base_url: &str,
    token: &str,
    incoming: &WeixinMessage,
    account_id: &str,
    files: &[GeneratedFile],
) -> HashSet<PathBuf> {
    let mut sent = HashSet::new();
    for gf in files {
        let path = &gf.path;
        if !path.exists()
            || std::fs::metadata(path).map(|m| m.len() == 0).unwrap_or(true)
        {
            continue;
        }
        info!(
            "[{account_id}] forwarding {} ({})",
            path.display(),
            gf.source.tag()
        );
        outbound::send_text_then_file(
            client,
            base_url,
            token,
            incoming,
            gf.caption.as_deref(),
            path,
        )
        .await;
        sent.insert(path.clone());
    }
    sent
}

/// Belt-and-suspenders: forward any file Claude wrote into `/work/output/`
/// without declaring it via `attach`. Each is logged as a prompt regression.
/// Skips anything already delivered via MCP.
pub async fn forward_diff_fallback(
    client: &ILinkClient,
    base_url: &str,
    token: &str,
    incoming: &WeixinMessage,
    account_id: &str,
    sandbox: &Sandbox,
    snapshot_before: OutputSnapshot,
    already_sent: &mut HashSet<PathBuf>,
) {
    for path in collect_new(sandbox, snapshot_before) {
        if already_sent.contains(&path) {
            continue;
        }
        warn!(
            "[{account_id}] forwarding {} (via diff fallback — Claude forgot to call attach)",
            path.display()
        );
        outbound::send_text_then_file(client, base_url, token, incoming, None, &path).await;
        already_sent.insert(path);
    }
}

/// Download + forward URLs Claude declared via `mcp__weclawbot__attach_url`.
pub async fn forward_urls(
    client: &ILinkClient,
    base_url: &str,
    token: &str,
    incoming: &WeixinMessage,
    account_id: &str,
    sandbox: &Sandbox,
    urls: &[AttachedUrl],
) {
    let url_temp_dir = sandbox.media().join("url_temp");
    for u in urls {
        info!("[{account_id}] forwarding url {} (via MCP)", u.url);
        outbound::send_text_then_url(
            client,
            base_url,
            token,
            incoming,
            u.caption.as_deref(),
            &u.url,
            &url_temp_dir,
        )
        .await;
    }
}
