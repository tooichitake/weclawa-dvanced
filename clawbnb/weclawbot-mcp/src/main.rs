//! `weclawbot-mcp` — stdio JSON-RPC MCP server baked into the sandbox image.
//!
//! Claude Code (inside the gVisor container) launches this binary as one of
//! its `mcpServers`. It exposes two tools the model is encouraged to call when
//! it wants to deliver a file or URL to the WeChat user:
//!
//!   - `attach(path, caption?)`     — local absolute file path produced in the sandbox
//!   - `attach_url(url, caption?)`  — remote https URL (e.g., found via web search)
//!
//! This binary itself does **not** upload anything — it only validates inputs
//! and returns an OK acknowledgement. The real capture happens on the host:
//! `weclawbot` parses Claude's `--output-format stream-json` and looks for
//! `tool_use` events whose `name` is `mcp__weclawbot__attach` /
//! `mcp__weclawbot__attach_url`. That stream-driven design mirrors OpenClaw's
//! "LLM declares output via tool call" pattern, with no path conventions or
//! filesystem scanning needed.
//!
//! Protocol: MCP 2024-11-05, minimum subset (`initialize`, `tools/list`,
//! `tools/call`). Transport: line-delimited JSON over stdin/stdout. Logging
//! goes to stderr so it doesn't corrupt the framed stream.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "weclawbot";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

fn ok(id: Option<Value>, result: Value) -> Response {
    Response {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Response {
    Response {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError {
            code,
            message: message.into(),
            data: None,
        }),
    }
}

fn tool_schemas() -> Value {
    json!({
        "tools": [
            {
                "name": "attach",
                "description": "Send a generated file to the WeChat user. Call this AFTER you have created a file (image, docx, xlsx, pdf, zip, etc.) that the user should receive. The file path must be absolute (e.g. /work/output/report.docx). The host will upload and forward the file as a WeChat message.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Absolute path to the file inside the sandbox."
                        },
                        "caption": {
                            "type": "string",
                            "description": "Optional short text to send alongside the file."
                        }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "attach_url",
                "description": "Forward a remote https:// URL (image, video, file) to the WeChat user without downloading it first. Use when you have located media via web search.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Remote https URL. http and other schemes are rejected."
                        },
                        "caption": {
                            "type": "string",
                            "description": "Optional short text to send alongside the URL."
                        }
                    },
                    "required": ["url"]
                }
            }
        ]
    })
}

fn handle_initialize(_params: &Value) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": SERVER_VERSION
        }
    })
}

fn tool_call_result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error
    })
}

fn handle_attach(args: &Value) -> Value {
    let path = match args.get("path").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => p,
        _ => {
            return tool_call_result(
                "attach: missing required string parameter `path`",
                true,
            )
        }
    };

    if !std::path::Path::new(path).is_absolute() {
        return tool_call_result(
            &format!("attach: path must be absolute, got `{path}`"),
            true,
        );
    }

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            return tool_call_result(
                &format!("attach: cannot stat `{path}`: {e}"),
                true,
            )
        }
    };
    if !meta.is_file() {
        return tool_call_result(
            &format!("attach: `{path}` is not a regular file"),
            true,
        );
    }
    if meta.len() == 0 {
        return tool_call_result(
            &format!("attach: `{path}` is empty (0 bytes)"),
            true,
        );
    }

    let caption = args
        .get("caption")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    eprintln!(
        "weclawbot-mcp: attach path={path} size={} caption_len={}",
        meta.len(),
        caption.len()
    );

    tool_call_result(
        &format!(
            "Queued `{path}` ({} bytes) for delivery to the WeChat user.",
            meta.len()
        ),
        false,
    )
}

fn handle_attach_url(args: &Value) -> Value {
    let url = match args.get("url").and_then(Value::as_str) {
        Some(u) if !u.is_empty() => u,
        _ => {
            return tool_call_result(
                "attach_url: missing required string parameter `url`",
                true,
            )
        }
    };
    if !url.starts_with("https://") {
        return tool_call_result(
            &format!("attach_url: only https:// URLs are accepted, got `{url}`"),
            true,
        );
    }

    let caption = args
        .get("caption")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    eprintln!(
        "weclawbot-mcp: attach_url url={url} caption_len={}",
        caption.len()
    );

    tool_call_result(
        &format!("Queued URL `{url}` for delivery to the WeChat user."),
        false,
    )
}

fn handle_tool_call(params: &Value) -> Response {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);
    let result = match name {
        "attach" => handle_attach(args),
        "attach_url" => handle_attach_url(args),
        other => tool_call_result(&format!("unknown tool `{other}`"), true),
    };
    ok(None, result)
}

fn dispatch(req: Request) -> Option<Response> {
    if req.jsonrpc != "2.0" && !req.jsonrpc.is_empty() {
        return Some(err(
            req.id,
            -32600,
            format!("invalid jsonrpc version `{}`", req.jsonrpc),
        ));
    }

    // Notifications (no id) get no reply.
    let is_notification = req.id.is_none();

    let resp = match req.method.as_str() {
        "initialize" => ok(req.id.clone(), handle_initialize(&req.params)),
        "initialized" | "notifications/initialized" => return None,
        "tools/list" => ok(req.id.clone(), tool_schemas()),
        "tools/call" => {
            let mut r = handle_tool_call(&req.params);
            r.id = req.id.clone();
            r
        }
        "ping" => ok(req.id.clone(), json!({})),
        "shutdown" => ok(req.id.clone(), json!({})),
        other => err(
            req.id.clone(),
            -32601,
            format!("method not found: {other}"),
        ),
    };

    if is_notification { None } else { Some(resp) }
}

fn main() {
    eprintln!(
        "weclawbot-mcp {SERVER_VERSION} starting (protocol {PROTOCOL_VERSION})"
    );

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("weclawbot-mcp: stdin read error: {e}");
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = err(None, -32700, format!("parse error: {e}"));
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = writeln!(out, "{s}");
                    let _ = out.flush();
                }
                continue;
            }
        };

        if let Some(resp) = dispatch(req) {
            match serde_json::to_string(&resp) {
                Ok(s) => {
                    if writeln!(out, "{s}").is_err() {
                        break;
                    }
                    let _ = out.flush();
                }
                Err(e) => eprintln!("weclawbot-mcp: encode error: {e}"),
            }
        }
    }
}
