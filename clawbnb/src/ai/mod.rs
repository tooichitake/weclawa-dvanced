//! AI reply providers. Public types live at the root; per-provider
//! implementations in `claude/` and `codex/`. The orchestrator
//! `complete_with_content` lives in `cli_provider` (legacy name — kept as a
//! stable import path for `handler.rs`; will be folded in once the
//! re-export shim has been there a release or two).

pub mod chat;
pub mod claude;
pub mod cli_provider;
pub mod codex;
pub mod history;
pub mod provider;
pub mod tool_policy;

use std::path::PathBuf;

// ---------------- Public types shared across providers ----------------

/// Where the host learned about a generated artifact. Drives log tags and
/// lets the smoke harness tell whether the primary signal (MCP tool call)
/// fired or whether the filesystem-diff fallback had to rescue the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSource {
    /// Provider called `mcp__weclawbot__attach` with this path. Primary path.
    McpAttach,
    /// Filesystem diff detected the file after the turn — provider did not
    /// declare it via the MCP tool. Safety net.
    Diff,
}

impl FileSource {
    pub fn tag(self) -> &'static str {
        match self {
            FileSource::McpAttach => "via MCP",
            FileSource::Diff => "via diff",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GeneratedFile {
    pub path: PathBuf,
    pub source: FileSource,
    pub caption: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AttachedUrl {
    pub url: String,
    pub caption: Option<String>,
}

/// Provider-agnostic output of one reply turn — text plus any files / URLs
/// declared via MCP tool calls.
#[derive(Debug, Default)]
pub struct ClaudeOutput {
    pub text: String,
    pub generated_files: Vec<GeneratedFile>,
    pub generated_urls: Vec<AttachedUrl>,
    /// v2.1.B4: 当 stream-json `result` 事件带 `is_error: true`（rate
    /// limit / billing / auth 错误等），claude-cli 把错误信息塞在 result
    /// 里。我们存到这个字段而不是 `text` 防止被当回复发给用户。invoke
    /// 在主流程末尾看到 error_message.is_some() 时返回 Err 走 fallback。
    pub error_message: Option<String>,
}

// ---------------- Provider config (claude / codex shared shape) -------

#[derive(Debug, Clone)]
pub struct CliConfig {
    pub provider: String, // "claude" or "codex"
    pub binary: String,
    pub system_prompt: String,
    pub history_limit: usize,
    pub timeout_ms: u64,
    /// Pass `--dangerously-skip-permissions` to claude. The gVisor sandbox
    /// is the real boundary.
    pub skip_permissions: bool,
}

impl CliConfig {
    /// Build from the typed global `AiConfig`. Returns `None` when AI is
    /// disabled or `provider` isn't a local CLI (Claude / Codex).
    pub fn from_global(c: &crate::config::AiConfig) -> Option<Self> {
        use crate::config::AiProvider;
        if !c.enabled {
            return None;
        }
        let provider = match c.provider {
            AiProvider::Claude => "claude",
            AiProvider::Codex => "codex",
            AiProvider::Api => return None,
        };
        Some(Self {
            provider: provider.to_string(),
            binary: provider.to_string(),
            system_prompt: c.system_prompt.clone(),
            history_limit: c.history_limit,
            timeout_ms: c.timeout_ms,
            skip_permissions: true,
        })
    }
}
