//! OpenAI Codex CLI reply provider.
//!
//! Spawns `codex exec` (the non-interactive subcommand) inside the per-user
//! gVisor sandbox and reads its plain-text reply.
//!
//! ## Differences from the Claude provider
//!
//! - **No stream-json**: codex emits plain text (or its own JSON-RPC stream
//!   over `--json`, which is not stable yet). We use `codex exec` for a
//!   single-shot reply.
//! - **No MCP attach yet**: codex CLI doesn't host an MCP server the same
//!   way claude does. Generated files inside `/work/output/` get picked up
//!   by the filesystem-diff fallback in `monitor::forward`; MCP `attach`
//!   tool calls do not fire from codex.
//! - **Best-effort install detection**: the sandbox image does NOT bundle
//!   the `codex` binary by default. If the operator wants codex they
//!   `npm install -g @openai/codex` in a derived image. We surface a
//!   friendly error when the binary is missing.
//!
//! The shape (`pub async fn invoke(cfg, sandbox, prompt) -> ClaudeOutput`)
//! matches `crate::ai::claude::invoke` so `cli_provider::complete_with_content`
//! can dispatch on `cfg.provider` uniformly.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

use crate::ai::{ClaudeOutput, CliConfig};
use crate::sandbox::Sandbox;

/// Spawn codex non-interactively inside `sandbox`. The full prompt goes via
/// stdin; the assistant reply comes back on stdout.
///
/// `cfg.binary` is the program name (`"codex"`). The sandbox image is
/// expected to provide it on `$PATH`; if not, the spawn returns a
/// human-readable error instructing the operator how to add it.
pub async fn invoke(
    cfg: &CliConfig,
    sandbox: &Sandbox,
    prompt: &str,
) -> Result<ClaudeOutput, String> {
    // `codex exec` is the documented one-shot mode. We pipe the prompt
    // through stdin to avoid argv length limits on long histories.
    let args: &[&str] = &[
        "exec",
        "--no-color",
        "--quiet",
    ];
    let mut cmd = crate::sandbox::exec::build_codex_cmd(sandbox, args);
    debug!(
        "spawn codex (sandbox={}, prompt_len={})",
        sandbox.user_hash,
        prompt.len()
    );

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(format!(
                "codex spawn failed: {e}. The sandbox image doesn't bundle codex by default; \
                 build a derived image with `npm install -g @openai/codex` to enable this provider."
            ));
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let data = prompt.to_string();
        tokio::spawn(async move {
            if let Err(e) = stdin.write_all(data.as_bytes()).await {
                debug!("codex stdin write: {e}");
            }
            drop(stdin);
        });
    }

    let mut stdout = child
        .stdout
        .take()
        .ok_or("codex stdout missing".to_string())?;
    let mut buf = String::new();

    let timeout = Duration::from_millis(cfg.timeout_ms);
    let read_task = async {
        // codex's reply can be large; cap so a misbehaving model can't OOM.
        let mut chunk = vec![0u8; 4096];
        loop {
            match stdout.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => buf.push_str(&String::from_utf8_lossy(&chunk[..n])),
                Err(e) => {
                    warn!("codex stdout read error: {e}");
                    break;
                }
            }
            if buf.len() > 2 * 1024 * 1024 {
                warn!("codex output exceeded 2 MiB cap — truncating");
                break;
            }
        }
    };

    if tokio::time::timeout(timeout, read_task).await.is_err() {
        let _ = child.kill().await;
        return Err(format!("codex timed out after {}ms", cfg.timeout_ms));
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("codex wait: {e}"))?;
    if !status.success() {
        return Err(format!(
            "codex exit {:?} (text accumulated={})",
            status.code(),
            buf.len()
        ));
    }

    let mut output = ClaudeOutput {
        text: buf.trim().to_string(),
        generated_files: Vec::new(),
        generated_urls: Vec::new(),
        error_message: None,
    };
    // codex doesn't emit MCP tool calls; deliverable files (if any) go
    // through the filesystem-diff fallback in `monitor::forward`. Nothing
    // for us to push into `generated_files` here.
    if output.text.is_empty() {
        output.text = "(codex returned empty output)".to_string();
    }
    Ok(output)
}
