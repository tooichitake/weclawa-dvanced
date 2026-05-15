//! Parse `claude -p --output-format stream-json --verbose` event stream.
//!
//! Each line is a JSON event; we only mine these:
//! - `assistant.message.content[].type == "text"` → accumulate as the reply
//!   (overwrites; the final value comes from `result`)
//! - `assistant.message.content[].type == "tool_use"` for our MCP tools
//!   `mcp__weclawbot__attach` and `mcp__weclawbot__attach_url` → push into
//!   `ClaudeOutput.generated_files` / `.generated_urls`
//! - `result.result` → canonical final reply text
//!
//! Built-in tools (Write/Edit/MultiEdit/Bash) are intentionally ignored;
//! Claude's explicit `attach` call is the only "deliverability" signal so
//! we don't accidentally forward intermediate helper files.

use std::path::PathBuf;

use serde_json::Value;
use tracing::{debug, warn};

use crate::ai::{AttachedUrl, ClaudeOutput, FileSource, GeneratedFile};
use crate::sandbox::Sandbox;

/// Parse a single stream-json event into `output`. Unknown event shapes
/// are silently ignored so a future claude-code release doesn't break us.
pub fn process_event(event: &Value, output: &mut ClaudeOutput, sandbox: &Sandbox) {
    let ty = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ty {
        "system" | "rate_limit_event" | "user" => {
            // Setup / metadata / tool_result echoes — nothing to forward.
        }
        "assistant" => {
            if let Some(content) = event.pointer("/message/content").and_then(|v| v.as_array()) {
                for block in content {
                    match block.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                if !t.is_empty() {
                                    output.text = t.to_string();
                                }
                            }
                        }
                        Some("tool_use") => handle_tool_use(block, output, sandbox),
                        _ => {}
                    }
                }
            }
        }
        "result" => {
            // v2.1.B4: claude-cli 在 rate-limit / billing / auth 错误时也
            // 发 `{type:"result", result:"Rate limit exceeded", is_error:true}`
            // 之前直接覆盖 output.text → 错误信息被当回复发给 WeChat 用户。
            // 检测 is_error / subtype，错误时记到 output.text 但带前缀，
            // 让上层 invoke 的 exit-code 检查能区分（exit 非零 + is_error
            // 走 Err 路径，不走 fallback）。
            let is_error = event.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
            let subtype = event.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(t) = event.get("result").and_then(|v| v.as_str()) {
                if is_error || subtype.contains("error") {
                    // 标记 + 不写到 output.text 让上层判定为失败
                    warn!("claude stream-json result is_error: subtype={subtype} text={t:?}");
                    output.error_message = Some(t.to_string());
                } else {
                    output.text = t.to_string();
                }
            }
        }
        _ => debug!("ignored stream-json event type={ty}"),
    }
}

fn handle_tool_use(block: &Value, output: &mut ClaudeOutput, sandbox: &Sandbox) {
    let Some(name) = block.get("name").and_then(|v| v.as_str()) else {
        return;
    };
    let input = block.get("input").cloned().unwrap_or(Value::Null);
    let caption = input
        .get("caption")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());

    match name {
        "mcp__weclawbot__attach" => {
            let Some(path) = input.get("path").and_then(|v| v.as_str()) else {
                warn!("mcp attach: missing path field");
                return;
            };
            let Some(host) = container_path_to_host(path, sandbox) else {
                warn!("mcp attach: cannot map container path `{path}` to host");
                return;
            };
            if output.generated_files.iter().any(|f| f.path == host) {
                return;
            }
            debug!("mcp attach captured: container={path} host={}", host.display());
            output.generated_files.push(GeneratedFile {
                path: host,
                source: FileSource::McpAttach,
                caption,
            });
        }
        "mcp__weclawbot__attach_url" => {
            let Some(url) = input.get("url").and_then(|v| v.as_str()) else {
                warn!("mcp attach_url: missing url field");
                return;
            };
            if !url.starts_with("https://") {
                warn!("mcp attach_url: rejecting non-https url `{url}`");
                return;
            }
            if output.generated_urls.iter().any(|u| u.url == url) {
                return;
            }
            debug!("mcp attach_url captured: {url}");
            output.generated_urls.push(AttachedUrl {
                url: url.to_string(),
                caption,
            });
        }
        _ => {
            // Ignore Write/Edit/MultiEdit/NotebookEdit and unrelated tools.
        }
    }
}

/// Map a container path (`/work/X` or `/home/claude/X`) to its host
/// counterpart under the sandbox dir.
fn container_path_to_host(container_path: &str, sandbox: &Sandbox) -> Option<PathBuf> {
    if let Some(suffix) = container_path.strip_prefix("/work/") {
        return Some(sandbox.work().join(suffix));
    }
    if container_path == "/work" {
        return Some(sandbox.work());
    }
    if let Some(suffix) = container_path.strip_prefix("/home/claude/") {
        return Some(sandbox.home().join(suffix));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn fake_sandbox() -> Sandbox {
        Sandbox {
            user_hash: "u-test".to_string(),
            user_dir: PathBuf::from("/tmp/fake-sandbox-test"),
        }
    }

    #[test]
    fn assistant_text_event_sets_output_text() {
        let event = json!({
            "type": "assistant",
            "message": { "content": [{ "type": "text", "text": "hello there" }] }
        });
        let mut out = ClaudeOutput::default();
        process_event(&event, &mut out, &fake_sandbox());
        assert_eq!(out.text, "hello there");
    }

    #[test]
    fn result_event_overrides_assistant_text() {
        let mut out = ClaudeOutput::default();
        process_event(
            &json!({"type":"assistant","message":{"content":[{"type":"text","text":"interim"}]}}),
            &mut out,
            &fake_sandbox(),
        );
        assert_eq!(out.text, "interim");
        process_event(
            &json!({"type":"result","result":"final"}),
            &mut out,
            &fake_sandbox(),
        );
        assert_eq!(out.text, "final");
    }

    #[test]
    fn mcp_attach_pushes_generated_file() {
        let event = json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "name": "mcp__weclawbot__attach",
                    "input": { "path": "/work/output/report.docx", "caption": "your report" }
                }]
            }
        });
        let mut out = ClaudeOutput::default();
        process_event(&event, &mut out, &fake_sandbox());
        assert_eq!(out.generated_files.len(), 1);
        assert_eq!(out.generated_files[0].source, FileSource::McpAttach);
        assert_eq!(out.generated_files[0].caption.as_deref(), Some("your report"));
        // Container path /work/X → host <sandbox>/work/X
        assert!(out.generated_files[0]
            .path
            .ends_with("work/output/report.docx"));
    }

    #[test]
    fn mcp_attach_dedupes_same_path() {
        let block = json!({
            "type": "tool_use",
            "name": "mcp__weclawbot__attach",
            "input": { "path": "/work/output/a.png" }
        });
        let event = json!({ "type":"assistant","message":{"content":[block.clone(), block]} });
        let mut out = ClaudeOutput::default();
        process_event(&event, &mut out, &fake_sandbox());
        assert_eq!(out.generated_files.len(), 1);
    }

    #[test]
    fn mcp_attach_rejects_non_mappable_path() {
        let event = json!({
            "type": "assistant",
            "message": { "content": [{
                "type": "tool_use",
                "name": "mcp__weclawbot__attach",
                "input": { "path": "/etc/passwd" }
            }]}
        });
        let mut out = ClaudeOutput::default();
        process_event(&event, &mut out, &fake_sandbox());
        assert!(out.generated_files.is_empty());
    }

    #[test]
    fn mcp_attach_url_https_only() {
        let mut out = ClaudeOutput::default();
        process_event(
            &json!({
                "type": "assistant",
                "message": { "content": [{
                    "type": "tool_use",
                    "name": "mcp__weclawbot__attach_url",
                    "input": { "url": "https://example.com/x.png" }
                }]}
            }),
            &mut out,
            &fake_sandbox(),
        );
        assert_eq!(out.generated_urls.len(), 1);
        // http:// rejected
        process_event(
            &json!({
                "type": "assistant",
                "message": { "content": [{
                    "type": "tool_use",
                    "name": "mcp__weclawbot__attach_url",
                    "input": { "url": "http://insecure.example.com/x.png" }
                }]}
            }),
            &mut out,
            &fake_sandbox(),
        );
        assert_eq!(out.generated_urls.len(), 1, "http should be rejected");
    }

    #[test]
    fn unknown_tool_calls_ignored() {
        let event = json!({
            "type":"assistant",
            "message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"/x.txt"}}]}
        });
        let mut out = ClaudeOutput::default();
        process_event(&event, &mut out, &fake_sandbox());
        // Built-in Write tool is ignored — only MCP attach is captured.
        assert!(out.generated_files.is_empty());
    }

    #[test]
    fn unknown_event_types_silently_skipped() {
        let mut out = ClaudeOutput::default();
        process_event(
            &json!({"type": "future_event_that_doesnt_exist_yet"}),
            &mut out,
            &fake_sandbox(),
        );
        assert!(out.text.is_empty());
    }

    #[test]
    fn container_path_to_host_work() {
        let sb = fake_sandbox();
        let h = container_path_to_host("/work/output/x.pdf", &sb).unwrap();
        assert_eq!(h, sb.work().join("output/x.pdf"));
    }

    #[test]
    fn container_path_to_host_home() {
        let sb = fake_sandbox();
        let h = container_path_to_host("/home/claude/notes.md", &sb).unwrap();
        assert_eq!(h, sb.home().join("notes.md"));
    }
}
