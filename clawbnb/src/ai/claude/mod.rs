//! Claude Code (Anthropic CLI) reply provider.
//!
//! Spawns `claude -p --output-format stream-json --verbose` inside the
//! per-user gVisor sandbox; parses stream-json events to accumulate text +
//! MCP `attach` tool calls. See submodules:
//!
//! - `prompt`      — build the prompt string (WeChat role, `/tmp` scratch
//!                   convention, MCP guidance, emoji rules).
//! - `stream_json` — parse the JSONL event stream into `ClaudeOutput`.

pub mod prompt;
#[cfg(feature = "acp")]
pub mod session;
pub mod stream_json;

#[cfg(not(feature = "acp"))]
use std::process::Stdio;
#[cfg(not(feature = "acp"))]
use std::time::Duration;

#[cfg(not(feature = "acp"))]
use serde_json::Value;
#[cfg(not(feature = "acp"))]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(not(feature = "acp"))]
use tracing::{debug, warn};

use crate::ai::{ClaudeOutput, CliConfig};
use crate::sandbox::Sandbox;

// v7.0 housekeeping: legacy `invoke(cfg, sandbox, prompt)` entry point
// removed — zero callers (the codex path it was kept for now also goes
// through `invoke_with_system` with empty system prompt). Call sites
// should always specify system + user explicitly.

/// Spawn `claude` inside the user's sandbox.
///
/// `system_prompt` (when non-empty) is wired through `--system-prompt`
/// so the model sees it as a real system instruction. `user_prompt`
/// goes to stdin.
///
/// v5.4 L4.1: 当 `--features acp` 开启时，path 切到
/// [`session::invoke_acp`] —— 复用 per-user 长跑 claude 进程。默认
/// per-message spawn 路径不变。
pub async fn invoke_with_system(
    cfg: &CliConfig,
    sandbox: &Sandbox,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<ClaudeOutput, String> {
    #[cfg(feature = "acp")]
    {
        return session::invoke_acp(cfg, sandbox, system_prompt, user_prompt).await;
    }
    #[cfg(not(feature = "acp"))]
    invoke_per_message(cfg, sandbox, system_prompt, user_prompt).await
}

/// Per-message spawn implementation — daemon's original path, kept as
/// fallback when `acp` feature is off.
#[cfg(not(feature = "acp"))]
async fn invoke_per_message(
    cfg: &CliConfig,
    sandbox: &Sandbox,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<ClaudeOutput, String> {
    // v5.4 L4.1: per-message scratch cleanup + concurrency cap.
    // 1) 清理上一轮在 /work/output/ 下留下的临时文件
    // 2) acquire 一个 spawn slot —— 上限超时 fail-open，保证 inbound 不丢
    crate::sandbox::ephemeral::scrub_per_message_scratch(sandbox);
    let _permit = crate::sandbox::ephemeral::acquire_spawn_slot().await;

    let mut claude_args: Vec<&str> = vec!["-p", "--output-format", "stream-json", "--verbose"];
    if !system_prompt.is_empty() {
        claude_args.push("--system-prompt");
        claude_args.push(system_prompt);
    }
    if cfg.skip_permissions {
        claude_args.push("--dangerously-skip-permissions");
    }

    // v5.5: tool policy 硬约束 + 信任分级 dynamic overlay。
    // user_settings JSON 给基础 allowed/disallowed；trust tier 给"低信任
    // 用户额外封死 Bash/Write/Edit/WebFetch/WebSearch" 的硬限制。两层
    // 都作为 CLI flag 传 claude-cli，覆盖 sandbox 内 settings.json。
    let user_settings = crate::ai::history::user_settings_json(sandbox.user_hash.as_str())
        .await
        .unwrap_or_else(|| serde_json::json!({}));
    let policy = crate::ai::tool_policy::ToolPolicy::for_user(
        sandbox.user_hash.as_str(),
        &user_settings,
    )
    .await;
    let policy_args = policy.to_cli_args();
    for a in &policy_args {
        claude_args.push(a.as_str());
    }

    let mut cmd = crate::sandbox::exec::build_claude_cmd(sandbox, &claude_args);
    debug!(
        "spawn claude-stream (sandbox={}, system_len={}, user_len={})",
        sandbox.user_hash,
        system_prompt.len(),
        user_prompt.len(),
    );

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;

    // Push the user-side prompt to stdin in a detached task; closing
    // stdin signals EOF and lets claude start processing.
    if let Some(mut stdin) = child.stdin.take() {
        let data = user_prompt.to_string();
        tokio::spawn(async move {
            if let Err(e) = stdin.write_all(data.as_bytes()).await {
                debug!("stdin write: {e}");
            }
            drop(stdin);
        });
    }

    // 把 stderr 也接住 —— 之前直接丢导致 exit-non-zero 时无从诊断。
    // 收到的内容存进 Arc<Mutex<Vec>> 让主任务结束后能取出最近 ~4KB
    // 作为 "claude 临终遗言" 一并打 log。stream-json 是机器格式，stderr
    // 才是 claude 的人类可读错误流。
    let stderr_buf: std::sync::Arc<tokio::sync::Mutex<Vec<u8>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(4096)));
    if let Some(stderr) = child.stderr.take() {
        let buf = stderr_buf.clone();
        let user_hash = sandbox.user_hash.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                // 每行同时打到 daemon log 和累计 buffer。Log level
                // 取决于内容 —— 实际 error 字样上 warn，否则 debug。
                let is_serious = line.to_lowercase().contains("error")
                    || line.to_lowercase().contains("fail")
                    || line.to_lowercase().contains("panic");
                if is_serious {
                    warn!("[{user_hash}] claude stderr: {line}");
                } else {
                    debug!("[{user_hash}] claude stderr: {line}");
                }
                let mut g = buf.lock().await;
                if g.len() < 4096 {
                    g.extend_from_slice(line.as_bytes());
                    g.push(b'\n');
                }
            }
        });
    }

    let stdout = child
        .stdout
        .take()
        .ok_or("claude stdout missing".to_string())?;
    let mut reader = BufReader::new(stdout).lines();
    let mut output = ClaudeOutput::default();

    let read_task = async {
        loop {
            let line = match reader.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => {
                    warn!("stream-json read error: {e}");
                    break;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let event: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    debug!(
                        "non-JSON line ignored ({e}): {}",
                        &line.chars().take(120).collect::<String>()
                    );
                    continue;
                }
            };
            stream_json::process_event(&event, &mut output, sandbox);
        }
    };

    let timeout = Duration::from_millis(cfg.timeout_ms);
    if tokio::time::timeout(timeout, read_task).await.is_err() {
        let _ = child.kill().await;
        return Err(format!("claude timed out after {}ms", cfg.timeout_ms));
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("claude wait: {e}"))?;
    // v2.1.B4: claude-cli 通过 stream-json `result` 事件标记了应用层
    // 错误（rate-limit / billing / auth），即使 exit code 是 0 也不
    // 应该把错误当回复发出去。优先于 exit-code 检查。
    if let Some(err_msg) = output.error_message.clone() {
        warn!("claude reported application error via stream-json: {err_msg}");
        return Err(format!("claude application error: {err_msg}"));
    }
    if !status.success() {
        // 取出 stderr 累计的内容 (最近 ~4KB) 用于诊断。
        let stderr_tail = {
            let g = stderr_buf.lock().await;
            String::from_utf8_lossy(&g).into_owned()
        };

        // claude-cli 在 stream-json 模式下偶尔 exit non-zero（plugin
        // sync / hook cleanup / SIGPIPE 等），但 stream 已经把完整
        // response 输出完了。只要解析到了非空内容，就把 exit code 当
        // warning 而不是 hard-error —— 否则用户会收到 "AI 暂时无法回复"
        // + 真正的回复 两条消息（消息系统经常会 redeliver inbound）。
        //
        // 同时把 exit code + stderr tail 打到日志，操作员能逆向追根因。
        if !output.text.is_empty()
            || !output.generated_files.is_empty()
            || !output.generated_urls.is_empty()
        {
            warn!(
                "claude exited {:?} but produced text={} files={} urls={} — accepting output. \
                 stderr tail: {}",
                status.code(),
                output.text.len(),
                output.generated_files.len(),
                output.generated_urls.len(),
                stderr_tail.trim().chars().take(800).collect::<String>(),
            );
            return Ok(output);
        }
        return Err(format!(
            "claude exit {:?} with empty output. stderr tail: {}",
            status.code(),
            stderr_tail.trim().chars().take(400).collect::<String>(),
        ));
    }
    Ok(output)
}
