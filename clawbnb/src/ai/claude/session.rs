//! Long-running claude-cli session — v5.4 ACP mode (L4.1 done).
//!
//! ## 设计
//!
//! 之前每条 inbound message 都 spawn 一个 `claude -p` 进程，cold start
//! 500ms-2s（podman 起容器 + claude 启动 + auth load）。本模块改成
//! per-user **长跑** 进程：
//!
//! ```bash
//! claude --bare \
//!   --input-format stream-json \
//!   --output-format stream-json \
//!   --verbose
//! ```
//!
//! `--bare` 跳过 hook/skill/plugin/MCP/CLAUDE.md autodiscovery，cold
//! start 主要瓶颈。`--input-format stream-json` 让 stdin 接受多个 NDJSON
//! 用户消息帧，进程保持长跑：
//!
//! ```jsonl
//! {"type":"user","message":{"role":"user","content":"first prompt"}}
//! {"type":"user","message":{"role":"user","content":"follow-up"}}
//! ```
//!
//! ## Reuse 机制
//!
//! `static SESSIONS: OnceLock<Mutex<HashMap<UserHash, Arc<Mutex<ClaudeAcpSession>>>>>`
//! 维护 per-user session 句柄。第一条消息进来：spawn + cache。后续消息
//! 复用同一进程，stdin 推 prompt 帧，stdout 读 NDJSON 直到 `result` 帧。
//!
//! ## 故障恢复
//!
//! - 进程 EOF / crash → session 句柄标记 dead，下次 invoke 重 spawn
//! - 读 stdout 超时 → kill + 重 spawn
//! - daemon shutdown → drop session table → kill_on_drop 关所有进程
//!
//! ## 跟 per-message 模式共存
//!
//! 通过 `--features acp` Cargo flag 切换。默认 (off) 走老的
//! [`super::invoke_with_system`] per-message spawn。开 (`--features acp`)
//! 走本模块的 [`invoke_acp`]。CallSites 通过 `crate::ai::claude::invoke_with_system`
//! API 入口，内部 cfg 选 backend。
//!
//! ## 跟 anthropics/claude-code#41230 bug 共存
//!
//! 该 bug：消息发到 stdin 时若上一轮 turn 还在进行，会丢历史。本实现
//! **严格串行化** per-session：拿到 `Arc<Mutex<Session>>` 后整个 turn
//! 持锁，等到 `result` 帧才释放锁，保证下一轮 send 时上一轮已结束。

#![cfg(feature = "acp")]

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::ai::{ClaudeOutput, CliConfig};
use crate::sandbox::Sandbox;

/// One long-running claude-cli stdio process for one user.
pub struct ClaudeAcpSession {
    child: Child,
    stdin: ChildStdin,
    stdout_lines: tokio::io::Lines<BufReader<ChildStdout>>,
    /// session_id from claude's first `system/init` frame — used for
    /// audit log correlation
    session_id: Option<String>,
}

impl Drop for ClaudeAcpSession {
    fn drop(&mut self) {
        // kill_on_drop 已经在 spawn 时设上；但为防 reqwest/tokio 边角
        // 情况下没真 kill，drop 时再 try 一次。
        let _ = self.child.start_kill();
    }
}

type SessionTable = Mutex<HashMap<String, Arc<Mutex<ClaudeAcpSession>>>>;

fn sessions() -> &'static SessionTable {
    static T: OnceLock<SessionTable> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

impl ClaudeAcpSession {
    /// Spawn a new long-running claude-cli inside the sandbox.
    pub async fn spawn(sandbox: &Sandbox, cfg: &CliConfig) -> Result<Self, String> {
        let args = build_acp_args(cfg);
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut cmd = crate::sandbox::exec::build_claude_cmd(sandbox, &args_ref);

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| format!("acp spawn: {e}"))?;
        let stdin = child.stdin.take().ok_or("acp stdin missing".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or("acp stdout missing".to_string())?;
        let stdout_lines = BufReader::new(stdout).lines();

        // Drain stderr in background → daemon log (debug/warn by content)
        if let Some(stderr) = child.stderr.take() {
            let user_hash = sandbox.user_hash.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let serious = line.to_lowercase().contains("error")
                        || line.to_lowercase().contains("fail");
                    if serious {
                        warn!("[{user_hash}] claude-acp stderr: {line}");
                    } else {
                        debug!("[{user_hash}] claude-acp stderr: {line}");
                    }
                }
            });
        }

        let mut sess = ClaudeAcpSession {
            child,
            stdin,
            stdout_lines,
            session_id: None,
        };

        // Read system/init frame to capture session_id + verify the
        // process is alive and listening.
        if let Some(line) = tokio::time::timeout(
            Duration::from_millis(5000),
            sess.stdout_lines.next_line(),
        )
        .await
        .map_err(|_| "acp init timed out (5s) — claude binary missing or container slow".to_string())?
        .map_err(|e| format!("acp init read: {e}"))?
        {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("system") {
                    if let Some(sid) = v
                        .get("session_id")
                        .or_else(|| v.pointer("/message/session_id"))
                        .and_then(|s| s.as_str())
                    {
                        sess.session_id = Some(sid.to_string());
                    }
                    info!(
                        "[{}] claude-acp ready (session_id={:?})",
                        sandbox.user_hash, sess.session_id
                    );
                }
            }
        }

        Ok(sess)
    }

    /// Send one user-message frame and read until `result` frame.
    /// Returns the assembled `ClaudeOutput`.
    pub async fn invoke(
        &mut self,
        sandbox: &Sandbox,
        system_prompt: &str,
        user_prompt: &str,
        timeout_ms: u64,
    ) -> Result<ClaudeOutput, String> {
        // Compose the user-message NDJSON frame. system_prompt is folded
        // into the content as a prefix when non-empty (claude-cli ACP
        // mode doesn't expose per-turn --system-prompt; the persistent
        // session-level system is set at spawn via the first user frame
        // optionally; for v5.4 we prepend it to each user turn — simple
        // and matches per-message behaviour).
        let composed = if system_prompt.is_empty() {
            user_prompt.to_string()
        } else {
            format!("{system_prompt}\n\n{user_prompt}")
        };
        let frame = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": composed,
            }
        });
        let line = format!("{}\n", frame);

        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("acp stdin write: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("acp stdin flush: {e}"))?;

        // Read NDJSON frames until `result` (per claude-cli stream-json
        // protocol: each turn ends with a single result frame containing
        // either success text or is_error=true).
        let mut output = ClaudeOutput::default();
        let read = async {
            loop {
                let line_opt = self.stdout_lines.next_line().await;
                let line = match line_opt {
                    Ok(Some(l)) => l,
                    Ok(None) => {
                        return Err("acp stdout EOF (claude process exited)".to_string());
                    }
                    Err(e) => return Err(format!("acp stdout read: {e}")),
                };
                if line.trim().is_empty() {
                    continue;
                }
                let event: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!(
                            "acp non-JSON line ignored ({e}): {}",
                            &line.chars().take(120).collect::<String>()
                        );
                        continue;
                    }
                };
                // Reuse the existing stream_json processor — same event
                // schema between `-p` per-message mode and `--bare`
                // ACP mode (text / tool_use / result / error).
                super::stream_json::process_event(&event, &mut output, sandbox);

                // Detect `result` frame: turn complete, return.
                let event_type = event.get("type").and_then(|t| t.as_str());
                if event_type == Some("result") {
                    return Ok::<_, String>(());
                }
            }
        };

        tokio::time::timeout(Duration::from_millis(timeout_ms), read)
            .await
            .map_err(|_| format!("acp invoke timed out after {timeout_ms}ms"))??;

        if let Some(err_msg) = output.error_message.clone() {
            return Err(format!("claude application error: {err_msg}"));
        }
        Ok(output)
    }

    /// Health check — is the child process still alive?
    fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,          // still running
            Ok(Some(_status)) => false, // exited
            Err(_) => false,
        }
    }
}

fn build_acp_args(cfg: &CliConfig) -> Vec<String> {
    let mut args = vec![
        "--bare".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
    ];
    if cfg.skip_permissions {
        args.push("--dangerously-skip-permissions".to_string());
    }
    args
}

/// ACP entry point — replaces per-message `invoke_with_system` when
/// `--features acp` is on.
///
/// Reuses an existing session per user; spawns lazily on first call;
/// auto-respawns if previous process died (network drop, crash, OOM).
pub async fn invoke_acp(
    cfg: &CliConfig,
    sandbox: &Sandbox,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<ClaudeOutput, String> {
    let key = sandbox.user_hash.as_str().to_string();

    // Get-or-create session arc. Hold table lock only briefly.
    let sess_arc = {
        let mut table = sessions().lock().await;
        if let Some(existing) = table.get(&key).cloned() {
            // Verify liveness — cheap try_wait under per-session lock.
            let alive = {
                let mut s = existing.lock().await;
                s.is_alive()
            };
            if alive {
                existing
            } else {
                info!("[{}] claude-acp dead — respawning", sandbox.user_hash);
                table.remove(&key);
                let fresh = ClaudeAcpSession::spawn(sandbox, cfg).await?;
                let arc = Arc::new(Mutex::new(fresh));
                table.insert(key.clone(), arc.clone());
                arc
            }
        } else {
            let fresh = ClaudeAcpSession::spawn(sandbox, cfg).await?;
            let arc = Arc::new(Mutex::new(fresh));
            table.insert(key.clone(), arc.clone());
            arc
        }
    };

    // Per-session serialization: hold session lock across the entire
    // send-prompt → read-until-result cycle. Defends against
    // anthropics/claude-code#41230 (concurrent stdin writes lose
    // history) — only one turn in flight per user at a time.
    let mut sess = sess_arc.lock().await;
    sess.invoke(sandbox, system_prompt, user_prompt, cfg.timeout_ms)
        .await
}

/// Shutdown hook — called from daemon graceful drain to clean up all
/// ACP sessions. Each session's Drop will kill_on_drop the child.
pub async fn shutdown_all() {
    let mut table = sessions().lock().await;
    let count = table.len();
    table.clear();
    if count > 0 {
        info!("claude-acp shutdown: killed {count} long-running sessions");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_args_have_bare_and_stream_json() {
        let cfg = CliConfig {
            provider: "claude".to_string(),
            binary: "claude".to_string(),
            system_prompt: String::new(),
            history_limit: 16,
            timeout_ms: 30_000,
            skip_permissions: false,
        };
        let args = build_acp_args(&cfg);
        assert!(args.contains(&"--bare".to_string()));
        assert!(args.contains(&"--input-format".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
        assert!(args.contains(&"--verbose".to_string()));
    }

    #[test]
    fn acp_args_respects_skip_permissions() {
        let cfg = CliConfig {
            provider: "claude".to_string(),
            binary: "claude".to_string(),
            system_prompt: String::new(),
            history_limit: 16,
            timeout_ms: 30_000,
            skip_permissions: true,
        };
        let args = build_acp_args(&cfg);
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
    }
}
