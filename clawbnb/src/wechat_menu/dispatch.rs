//! State-machine: turn raw inbound text into a reply + session-state update.
//!
//! Accepted input formats:
//!   - `/menu` from chat mode → enter at root
//!   - menu-name (e.g. `model`) → descend into that child
//!   - number (e.g. `2`) → descend into the n-th child of the current menu
//!     (1-indexed; matches what the rendered menu shows)
//!   - `/back` → ascend one level
//!   - `/exit` / `/quit` → leave menu mode
//!   - fully-qualified absolute path (e.g. `/model set sonnet`) → jump and
//!     execute (advanced users; expanded space-separated)
//!
//! Output is always plain text suitable to drop into a WeChat text bubble.

use super::session::{self, Session};
use super::tree::{self, Node, NodeKind};

/// Top-level entry from `route()`. Returns the reply string. Session state
/// is mutated inline (saved to disk).
pub fn dispatch(user_hash: &str, raw: &str) -> String {
    let text = raw.trim();
    let mut sess = session::load(user_hash);

    // Entering menu from chat mode.
    if !sess.in_menu && text == "/menu" {
        session::enter(user_hash);
        return render_menu(&Vec::new());
    }

    // Already in menu mode; classify the input.
    sess.last_input_at = Some(chrono::Utc::now());

    // Universal commands first.
    match text {
        "/menu" => {
            // Reset to root from any depth.
            sess.current_path.clear();
            session::save(user_hash, &sess);
            return render_menu(&sess.current_path);
        }
        "/exit" | "/quit" | "/back-to-chat" | "退出" => {
            session::exit(user_hash);
            return "✅ 已退出控制台，回到普通对话模式。再次输入 /menu 可以再进。".to_string();
        }
        "/back" | "返回" => {
            if sess.current_path.pop().is_none() {
                // Already at root; treat as exit.
                session::exit(user_hash);
                return "✅ 已退出控制台。".to_string();
            }
            session::save(user_hash, &sess);
            return render_menu(&sess.current_path);
        }
        "/help" | "帮助" | "?" => {
            session::save(user_hash, &sess);
            return render_help(&sess.current_path);
        }
        _ => {}
    }

    // Fully-qualified path: "/foo bar baz" or "foo bar baz". Try this first
    // when the input has multiple whitespace-separated tokens.
    let tokens: Vec<&str> = text.trim_start_matches('/').split_whitespace().collect();
    if tokens.len() >= 2 {
        return try_absolute_path(user_hash, &mut sess, &tokens);
    }

    // Single token: either a number (1..=N) or a child name at the current level.
    let single = tokens.first().copied().unwrap_or("");
    if single.is_empty() {
        return render_menu(&sess.current_path);
    }

    let root = tree::root();
    let here = match tree::node_at(root, &sess.current_path) {
        Some(n) => n,
        None => {
            // Path went stale (tree changed across daemon restart?). Reset.
            sess.current_path.clear();
            session::save(user_hash, &sess);
            return format!(
                "(菜单路径无效，已回到根菜单。)\n\n{}",
                render_menu(&sess.current_path)
            );
        }
    };

    let children = match &here.kind {
        NodeKind::Menu(c) => c,
        NodeKind::Action(_) => {
            // We are sitting on a leaf for some reason — just execute it.
            return run_action(user_hash, &mut sess, here);
        }
    };

    let chosen = match resolve_child(children, single) {
        Some(c) => c,
        None => {
            return format!(
                "❌ 未知命令: `{single}`\n\n{}",
                render_menu(&sess.current_path)
            );
        }
    };

    descend_or_run(user_hash, &mut sess, chosen)
}

fn try_absolute_path(user_hash: &str, sess: &mut Session, tokens: &[&str]) -> String {
    // Treat tokens as a path from root. Match each segment by name OR
    // 1-indexed number at that level.
    let mut path: Vec<String> = Vec::new();
    let mut cur: &Node = tree::root();
    for tok in tokens {
        let children = match &cur.kind {
            NodeKind::Menu(c) => c,
            NodeKind::Action(_) => {
                return format!(
                    "❌ 多余的参数: `{tok}` (前面已经是终端命令)"
                );
            }
        };
        let next = match resolve_child(children, tok) {
            Some(c) => c,
            None => {
                return format!(
                    "❌ 在 [{}] 下没有 `{tok}`",
                    if path.is_empty() {
                        "根菜单".to_string()
                    } else {
                        path.join(" > ")
                    }
                );
            }
        };
        path.push(next.name.to_string());
        cur = next;
    }
    sess.current_path = path;
    descend_or_run(user_hash, sess, cur)
}

fn descend_or_run(user_hash: &str, sess: &mut Session, chosen: &Node) -> String {
    match &chosen.kind {
        NodeKind::Menu(_) => {
            sess.current_path.push(chosen.name.to_string());
            session::save(user_hash, sess);
            render_menu(&sess.current_path)
        }
        NodeKind::Action(_) => run_action(user_hash, sess, chosen),
    }
}

fn run_action(user_hash: &str, sess: &mut Session, node: &Node) -> String {
    let NodeKind::Action(f) = &node.kind else {
        return "(internal: tried to run non-action)".to_string();
    };
    let result = f(user_hash, &[]);
    // Stay at the current menu level — `current_path` already points at the
    // action's parent (a `show`/`set` leaf was resolved as a child of the
    // current menu, not descended into). Re-rendering the same menu lets
    // the user pick another sibling without typing /back.
    session::save(user_hash, sess);

    let mut out = result.reply;
    out.push_str("\n\n");
    out.push_str(&render_menu(&sess.current_path));
    out
}

fn resolve_child<'a>(children: &'a [Node], input: &str) -> Option<&'a Node> {
    // Try number 1..=N first.
    if let Ok(n) = input.parse::<usize>() {
        if n >= 1 && n <= children.len() {
            return Some(&children[n - 1]);
        }
    }
    // Then case-insensitive name match — users on mobile keyboards often
    // get an unexpected capital first letter (e.g. "System-prompt"). The
    // names themselves are stable lowercase ASCII so this is safe.
    if let Some(c) = children
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(input))
    {
        return Some(c);
    }
    // Then label match (Chinese friendly). Labels are short and usually all
    // characters are non-ASCII Chinese, so case-insensitive comparison would
    // be a no-op there — keep it as exact match.
    if let Some(c) = children.iter().find(|c| c.label == input) {
        return Some(c);
    }
    None
}

fn render_menu(path: &[String]) -> String {
    let root = tree::root();
    let node = tree::node_at(root, path).unwrap_or(root);

    let breadcrumb = if path.is_empty() {
        "设置 > 根菜单".to_string()
    } else {
        let mut parts = vec!["设置".to_string()];
        let mut cur = root;
        for seg in path {
            if let NodeKind::Menu(children) = &cur.kind {
                if let Some(c) = children.iter().find(|c| c.name == *seg) {
                    parts.push(c.label.to_string());
                    cur = c;
                }
            }
        }
        parts.join(" > ")
    };

    let children = match &node.kind {
        NodeKind::Menu(c) => c,
        NodeKind::Action(_) => {
            // Shouldn't happen — leaves aren't a "current path".
            return format!("[{breadcrumb}]\n\n该项是终端命令。");
        }
    };

    let mut lines = vec![format!("[{breadcrumb}]"), String::new()];
    for (i, c) in children.iter().enumerate() {
        let num = i + 1;
        let arrow = match &c.kind {
            NodeKind::Menu(_) => "›",
            NodeKind::Action(_) => "•",
        };
        let suffix = if c.help.is_empty() {
            String::new()
        } else {
            format!(" — {}", c.help)
        };
        lines.push(format!("  {num}. {arrow} {} ({}){}", c.label, c.name, suffix));
    }
    lines.push(String::new());
    lines.push("输入 数字 / 命令名 / 中文标签 任意一种。".to_string());
    if !path.is_empty() {
        lines.push("/back 返回上一级 · /exit 退出控制台".to_string());
    } else {
        lines.push("/exit 退出控制台".to_string());
    }
    lines.join("\n")
}

fn render_help(path: &[String]) -> String {
    format!(
        "[帮助]\n\
         · 输入子菜单的数字或名字即可进入\n\
         · /back 返回上一级\n\
         · /menu 跳回根菜单\n\
         · /exit 退出控制台\n\
         · 一行多 token 视为绝对路径，例如 `/model set sonnet`\n\n\
         {}",
        render_menu(path)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_child_by_number() {
        let root = tree::root();
        let NodeKind::Menu(kids) = &root.kind else {
            panic!("root must be a menu");
        };
        let first = resolve_child(kids, "1").unwrap();
        assert_eq!(first.name, kids[0].name);
    }

    #[test]
    fn resolve_child_by_name_is_case_insensitive() {
        let root = tree::root();
        let NodeKind::Menu(kids) = &root.kind else {
            panic!();
        };
        assert_eq!(resolve_child(kids, "model").unwrap().name, "model");
        assert_eq!(resolve_child(kids, "MODEL").unwrap().name, "model");
        assert_eq!(resolve_child(kids, "Model").unwrap().name, "model");
    }

    #[test]
    fn resolve_child_by_chinese_label() {
        let root = tree::root();
        let NodeKind::Menu(kids) = &root.kind else {
            panic!();
        };
        assert_eq!(resolve_child(kids, "模型").unwrap().name, "model");
    }

    #[test]
    fn resolve_child_returns_none_for_unknown() {
        let root = tree::root();
        let NodeKind::Menu(kids) = &root.kind else {
            panic!();
        };
        assert!(resolve_child(kids, "nonexistent").is_none());
        assert!(resolve_child(kids, "999").is_none());
    }

    #[test]
    fn render_menu_includes_breadcrumb() {
        let s = render_menu(&[]);
        assert!(s.contains("设置 > 根菜单"));
        let s2 = render_menu(&["model".to_string()]);
        assert!(s2.contains("设置 > 模型"));
    }

    #[test]
    fn render_menu_lists_all_children_at_root() {
        let s = render_menu(&[]);
        assert!(s.contains("model"));
        assert!(s.contains("system-prompt"));
        assert!(s.contains("permissions"));
        assert!(s.contains("plugins"));
        assert!(s.contains("exit"));
        assert!(s.contains("new-chat"));
        assert!(s.contains("provider"));
    }
}
