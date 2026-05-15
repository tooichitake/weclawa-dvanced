//! Static command tree exposed via `/menu`.
//!
//! Each `Node` is either a `Menu` (children to drill into) or an `Action`
//! (terminal — runs immediately and returns a reply string).
//!
//! The tree is hand-curated to expose ONLY claude-cli / codex-cli settings.
//! By design it does NOT include any of:
//!   - Account commands (login / logout / switch-account / API key)
//!   - Host commands (restart daemon / view logs / podman / runsc)
//!   - File operations (rm / mv / chmod)
//!   - Anything that spawns the `claude` binary interactively
//!
//! See also `apply::BLOCKED_KEYS` for the JSON-path-level guard that
//! enforces this even if a future tree node is misconfigured.

use std::sync::OnceLock;

use serde_json::{json, Value};

use super::apply;

/// What a leaf command returns to the dispatcher.
pub struct ActionResult {
    /// Message to send back to the WeChat user.
    pub reply: String,
}

pub type ActionFn = fn(user_hash: &str, args: &[&str]) -> ActionResult;

pub enum NodeKind {
    /// Container for child nodes. Selecting a child descends.
    Menu(Vec<Node>),
    /// Leaf — executes immediately.
    Action(ActionFn),
}

pub struct Node {
    /// Identifier used in the path / typed by the user.
    pub name: &'static str,
    /// Human-readable label shown in the menu.
    pub label: &'static str,
    /// One-line hint.
    pub help: &'static str,
    pub kind: NodeKind,
}

pub fn root() -> &'static Node {
    static ROOT: OnceLock<Node> = OnceLock::new();
    ROOT.get_or_init(build_root)
}

fn build_root() -> Node {
    Node {
        name: "",
        label: "设置",
        help: "Claude Code 设置控制台",
        kind: NodeKind::Menu(vec![
            Node {
                name: "provider",
                label: "AI 提供商",
                help: "切换 claude / codex / api 模式（影响所有用户，需操作员授权时管理员可改）",
                kind: NodeKind::Menu(vec![
                    Node {
                        name: "show",
                        label: "显示当前 provider",
                        help: "",
                        kind: NodeKind::Action(action_provider_show),
                    },
                ]),
            },
            Node {
                name: "model",
                label: "模型",
                help: "切换 Claude 使用的模型 (sonnet/opus/haiku)",
                kind: NodeKind::Menu(vec![
                    Node {
                        name: "show",
                        label: "显示当前模型",
                        help: "查看 settings.json 里当前的 model 字段",
                        kind: NodeKind::Action(action_model_show),
                    },
                    Node {
                        name: "set",
                        label: "选择模型",
                        help: "从内置列表里选一个模型",
                        kind: NodeKind::Menu(vec![
                            Node {
                                name: "sonnet",
                                label: "sonnet",
                                help: "平衡型，速度快 (默认推荐)",
                                kind: NodeKind::Action(make_set_model("sonnet")),
                            },
                            Node {
                                name: "opus",
                                label: "opus",
                                help: "能力最强，速度慢",
                                kind: NodeKind::Action(make_set_model("opus")),
                            },
                            Node {
                                name: "haiku",
                                label: "haiku",
                                help: "最快，能力较弱",
                                kind: NodeKind::Action(make_set_model("haiku")),
                            },
                        ]),
                    },
                ]),
            },
            Node {
                name: "system-prompt",
                label: "系统提示",
                help: "查看 / 修改附加 system prompt",
                kind: NodeKind::Menu(vec![
                    Node {
                        name: "show",
                        label: "显示当前 system prompt",
                        help: "",
                        kind: NodeKind::Action(action_systemprompt_show),
                    },
                    Node {
                        name: "clear",
                        label: "清空 system prompt",
                        help: "回到内置默认",
                        kind: NodeKind::Action(action_systemprompt_clear),
                    },
                ]),
            },
            Node {
                name: "history-limit",
                label: "历史轮数",
                help: "对话历史保留多少轮",
                kind: NodeKind::Menu(vec![
                    Node {
                        name: "show",
                        label: "显示当前值",
                        help: "",
                        kind: NodeKind::Action(action_historylimit_show),
                    },
                    Node {
                        name: "set",
                        label: "设置（输入数字）",
                        help: "下一条消息发数字，例如 30",
                        kind: NodeKind::Action(action_historylimit_set_prompt),
                    },
                ]),
            },
            Node {
                name: "timeout",
                label: "AI 超时（秒）",
                help: "Claude 单次回复最长等多久",
                kind: NodeKind::Menu(vec![
                    Node {
                        name: "show",
                        label: "显示当前值",
                        help: "",
                        kind: NodeKind::Action(action_timeout_show),
                    },
                ]),
            },
            Node {
                name: "permissions",
                label: "权限",
                help: "查看 / 修改 Claude 工具允许 / 拒绝列表",
                kind: NodeKind::Menu(vec![
                    Node {
                        name: "list",
                        label: "查看当前 allow/deny",
                        help: "",
                        kind: NodeKind::Action(action_permissions_list),
                    },
                ]),
            },
            Node {
                name: "plugins",
                label: "插件",
                help: "查看 / 启用 / 禁用 enabledPlugins",
                kind: NodeKind::Menu(vec![
                    Node {
                        name: "list",
                        label: "已启用插件",
                        help: "",
                        kind: NodeKind::Action(action_plugins_list),
                    },
                ]),
            },
            Node {
                name: "mcp",
                label: "MCP 服务器",
                help: "查看注册的 MCP servers (weclawbot 自管的不可改)",
                kind: NodeKind::Menu(vec![
                    Node {
                        name: "list",
                        label: "已注册",
                        help: "",
                        kind: NodeKind::Action(action_mcp_list),
                    },
                ]),
            },
            Node {
                name: "output-style",
                label: "输出风格",
                help: "Claude 的 outputStyle 字段",
                kind: NodeKind::Menu(vec![
                    Node {
                        name: "show",
                        label: "显示当前值",
                        help: "",
                        kind: NodeKind::Action(action_outputstyle_show),
                    },
                ]),
            },
            Node {
                name: "new-chat",
                label: "开启新对话",
                help: "清空当前对话历史，下一句不再继承之前的上下文",
                kind: NodeKind::Action(action_new_chat),
            },
            Node {
                name: "exit",
                label: "退出控制台",
                help: "回到普通对话模式",
                kind: NodeKind::Action(action_exit),
            },
        ]),
    }
}

// ---------------- Action helpers ----------------

fn ok_reply(text: impl Into<String>) -> ActionResult {
    ActionResult { reply: text.into() }
}

fn err_reply(text: impl Into<String>) -> ActionResult {
    ActionResult {
        reply: format!("❌ {}", text.into()),
    }
}

// ---------------- provider ----------------

fn action_provider_show(_user_hash: &str, _args: &[&str]) -> ActionResult {
    // Provider is operator-managed (`~/.weclawbot/config.json` ai.provider).
    // We expose read-only here — switching providers globally is admin-only.
    let cfg = crate::config::Config::load();
    let name = match cfg.ai.provider {
        crate::config::AiProvider::Claude => "claude",
        crate::config::AiProvider::Codex => "codex",
        crate::config::AiProvider::Api => "api",
    };
    ok_reply(format!(
        "当前 AI 提供商: {name}\n\n\
         (此项为全局配置，需管理员通过 `weclawbot ai --provider <name>` 切换。\
         本控制台不暴露切换入口以避免单一用户影响他人。)"
    ))
}

// ---------------- model ----------------

fn action_model_show(user_hash: &str, _args: &[&str]) -> ActionResult {
    let cur = apply::get_field(user_hash, &["model"])
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or("(未设置，使用 claude 默认)")
        .to_string();
    ok_reply(format!("当前模型: {cur}"))
}

fn make_set_model(model: &'static str) -> ActionFn {
    // Each leaf has its own ActionFn; we close over `model` through a static
    // dispatch table. Since fn pointers can't capture, we map by name.
    match model {
        "sonnet" => set_model_sonnet,
        "opus" => set_model_opus,
        "haiku" => set_model_haiku,
        _ => unreachable!("unknown model alias {model}"),
    }
}

fn set_model_sonnet(user_hash: &str, _args: &[&str]) -> ActionResult {
    apply_model(user_hash, "sonnet")
}

fn set_model_opus(user_hash: &str, _args: &[&str]) -> ActionResult {
    apply_model(user_hash, "opus")
}

fn set_model_haiku(user_hash: &str, _args: &[&str]) -> ActionResult {
    apply_model(user_hash, "haiku")
}

fn apply_model(user_hash: &str, model: &str) -> ActionResult {
    match apply::set_field(user_hash, &["model"], json!(model)) {
        Ok(()) => ok_reply(format!(
            "✅ 已设置模型为 {model}。下条 AI 对话生效。"
        )),
        Err(e) => err_reply(format!("写入失败: {e}")),
    }
}

// ---------------- system-prompt ----------------

fn action_systemprompt_show(user_hash: &str, _args: &[&str]) -> ActionResult {
    // 用户视角只暴露/管理 per-user override —— 不显示全局默认（用户既不
    // 知道有全局默认存在，也无权修改它）。空 = 没有用户自定义，明确说明
    // 怎么去设。
    let user_value = apply::get_field(user_hash, &["systemPrompt"])
        .as_ref()
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());

    match user_value {
        Some(s) => ok_reply(format!("你的 systemPrompt:\n{s}")),
        None => ok_reply(
            "你还没有自定义 systemPrompt（当前走管理员预设的默认行为）。\n\n\
             用 set 子命令可以设置自己的；之后用 clear 可以清除回到默认。"
                .to_string(),
        ),
    }
}

fn action_systemprompt_clear(user_hash: &str, _args: &[&str]) -> ActionResult {
    match apply::set_field(user_hash, &["systemPrompt"], Value::String(String::new())) {
        Ok(()) => ok_reply("✅ 已清空 systemPrompt。"),
        Err(e) => err_reply(format!("写入失败: {e}")),
    }
}

// ---------------- history-limit ----------------

fn action_historylimit_show(user_hash: &str, _args: &[&str]) -> ActionResult {
    let cur = apply::get_field(user_hash, &["historyLimit"])
        .as_ref()
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "(未设置，使用默认 20)".to_string());
    ok_reply(format!("当前历史轮数: {cur}"))
}

fn action_historylimit_set_prompt(_user_hash: &str, _args: &[&str]) -> ActionResult {
    ok_reply(
        "请输入一个数字 (例如 30) 来设置历史轮数。\n\
         (该交互需要等下版才支持。目前请通过 GUI 设定。)"
            .to_string(),
    )
}

// ---------------- timeout ----------------

fn action_timeout_show(user_hash: &str, _args: &[&str]) -> ActionResult {
    let cur = apply::get_field(user_hash, &["timeoutMs"])
        .as_ref()
        .and_then(Value::as_u64)
        .map(|ms| format!("{:.1} 秒", ms as f64 / 1000.0))
        .unwrap_or_else(|| "(未设置，使用默认 300 秒)".to_string());
    ok_reply(format!("当前 AI 超时: {cur}"))
}

// ---------------- permissions ----------------

fn action_permissions_list(user_hash: &str, _args: &[&str]) -> ActionResult {
    let allow = apply::get_field(user_hash, &["permissions", "allow"]);
    let deny = apply::get_field(user_hash, &["permissions", "deny"]);
    let allow_s = format_array(allow.as_ref());
    let deny_s = format_array(deny.as_ref());
    ok_reply(format!(
        "[权限] 允许:\n{allow_s}\n\n[权限] 拒绝:\n{deny_s}"
    ))
}

// ---------------- plugins ----------------

fn action_plugins_list(user_hash: &str, _args: &[&str]) -> ActionResult {
    // 只显示 per-user enabledPlugins —— 用户视角只管理自己的列表，不
    // 暴露管理员默认模板（用户既看不到 `~/.weclawbot/defaults/`，也无权
    // 改它）。空 = 用户自己没启用任何插件。
    let user_v = apply::get_field(user_hash, &["enabledPlugins"]);
    let user_s = format_array(user_v.as_ref());

    ok_reply(format!(
        "你启用的插件:\n{user_s}\n\n\
         (Anthropic 官方的 PPT / DOCX / XLSX / PDF skills 也是以插件形式提供，\
         由管理员预装；当前可用列表请咨询管理员。)"
    ))
}

// ---------------- mcp ----------------

fn action_mcp_list(user_hash: &str, _args: &[&str]) -> ActionResult {
    // mcpServers actually lives in .claude.json (not settings.json), but
    // the user can also list any operator-added entries from settings.json.
    // For visibility we show settings.json's view here.
    let v = apply::get_field(user_hash, &["mcpServers"]);
    let names: Vec<String> = v
        .as_ref()
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if names.is_empty() {
        ok_reply("MCP servers: (settings.json 里没有；weclawbot 自管的在 .claude.json)")
    } else {
        ok_reply(format!("MCP servers:\n• {}", names.join("\n• ")))
    }
}

// ---------------- output-style ----------------

fn action_outputstyle_show(user_hash: &str, _args: &[&str]) -> ActionResult {
    let cur = apply::get_field(user_hash, &["outputStyle"])
        .as_ref()
        .and_then(Value::as_str)
        .unwrap_or("(未设置，使用 claude 默认)")
        .to_string();
    ok_reply(format!("当前 outputStyle: {cur}"))
}

// ---------------- new-chat ----------------

fn action_new_chat(user_hash: &str, _args: &[&str]) -> ActionResult {
    // Clear the per-user conversation history so the next message starts a
    // fresh context (Claude won't see prior turns). Phase 1d: now backed
    // by the `user_history` table via `UserRepo::clear_history`.
    //
    // v4.2: history::clear 是 async fn，但 ActionFn 类型是 sync fn pointer。
    // 用 tokio::spawn 异步执行（fire-and-forget），用户在下一条消息发出来
    // 之前 clear 通常已完成；即便没完，下条消息会 race 但 INSERT vs
    // DELETE 在 SQLite WAL 下不会破坏 schema。
    let user_hash_owned = user_hash.to_string();
    tokio::spawn(async move {
        crate::ai::history::clear(&user_hash_owned).await;
    });
    ok_reply(
        "✅ 已开启新对话。之前的聊天记录已清空，下一句不会继承上下文。\n\n\
         (退出 /menu 后继续聊即可)",
    )
}

// ---------------- exit ----------------

fn action_exit(user_hash: &str, _args: &[&str]) -> ActionResult {
    super::session::exit(user_hash);
    ok_reply("✅ 已退出控制台，回到普通对话模式。再次输入 /menu 可以再进。")
}

// ---------------- helpers ----------------

fn format_array(v: Option<&Value>) -> String {
    let arr = match v.and_then(Value::as_array) {
        Some(a) if !a.is_empty() => a,
        _ => return "  (空)".to_string(),
    };
    arr.iter()
        .filter_map(|x| x.as_str().map(String::from).or_else(|| Some(x.to_string())))
        .map(|s| format!("  • {s}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Walk the tree to a node at `path`. Returns `None` if any segment doesn't
/// match. Empty path returns root.
pub fn node_at<'a>(root: &'a Node, path: &[String]) -> Option<&'a Node> {
    let mut cur = root;
    for seg in path {
        let NodeKind::Menu(children) = &cur.kind else {
            return None;
        };
        cur = children.iter().find(|c| c.name == seg)?;
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_builds_without_panicking() {
        let r = root();
        assert_eq!(r.label, "设置");
        let model = node_at(r, &["model".to_string()]).unwrap();
        assert_eq!(model.name, "model");
        let set_sonnet = node_at(r, &["model".to_string(), "set".to_string(), "sonnet".to_string()]).unwrap();
        assert!(matches!(set_sonnet.kind, NodeKind::Action(_)));
    }

    #[test]
    fn no_account_commands_anywhere_in_tree() {
        // Defense in depth: even if someone adds a node, never expose these
        // names. The dispatcher additionally validates against this list.
        let banned: &[&str] = &[
            "login", "logout", "signin", "signout", "switch-account",
            "add-account", "remove-account", "api-key", "oauth", "credentials",
            "logs", "restart", "podman", "runsc", "sandbox-image",
        ];
        fn walk(node: &Node, banned: &[&str]) {
            for b in banned {
                assert_ne!(node.name, *b, "command tree should not expose {b}");
            }
            if let NodeKind::Menu(children) = &node.kind {
                for c in children {
                    walk(c, banned);
                }
            }
        }
        walk(root(), banned);
    }
}
