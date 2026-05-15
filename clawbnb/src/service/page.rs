//! GUI console HTML — compiled-in default + optional fs override.
//!
//! v2.2 L5.1：之前 `console.html` 2133 行直接 `include_str!` 进 binary，
//! 改一个 button label 都得重发 daemon。新机制：
//!
//! 1. `~/.weclawbot/console/index.html` 存在 → 读它（operator 可热改 UI）
//! 2. 否则用 `include_str!` 编进 binary 的兜底版本
//!
//! 同样的 fs-override 适用于 GUI 引用的任何额外 asset（CSS/JS），由
//! `ServeDir` layer 在 `server.rs` 挂上 `/console/*` 路由。本 module
//! 只负责入口 HTML。
//!
//! ## 为什么不引入 build step（Vite/webpack）
//!
//! 项目目标是**单 binary 部署**（user_preferences.md：no Node.js runtime
//! deps）。所有 GUI 用现代浏览器原生 ESM module 即可。
//!
//! ## 缓存
//!
//! fs override 每次请求都重读 —— 内容是几十 KB 量级，console UI 单
//! operator 用，QPS 极低（个位数/天）。简单胜过精巧的 inotify watcher。
//! 改 UI 后浏览器刷新即生效，无需 daemon 重启。

use std::path::PathBuf;

const COMPILED_DEFAULT: &str = include_str!("../../assets/console.html");

/// Path operators write their override to:
///   `~/.weclawbot/console/index.html`
///
/// v3 SaaS hosted 模式可以 mount per-tenant 自定义品牌 HTML 在这个位置。
fn override_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".weclawbot").join("console").join("index.html"))
}

/// Return the console HTML body. Reads operator override if present,
/// otherwise the compiled-in default.
pub fn console_html() -> std::borrow::Cow<'static, str> {
    if let Some(p) = override_path() {
        if p.is_file() {
            match std::fs::read_to_string(&p) {
                Ok(s) => return std::borrow::Cow::Owned(s),
                Err(e) => {
                    tracing::warn!(
                        "console override exists at {} but read failed: {e} — falling back to compiled-in",
                        p.display()
                    );
                }
            }
        }
    }
    std::borrow::Cow::Borrowed(COMPILED_DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_in_is_html() {
        let body = console_html();
        // Smoke check — embedded asset is HTML.
        assert!(body.contains("<html") || body.contains("<!DOCTYPE"));
    }

    #[test]
    fn missing_override_falls_back() {
        // override_path() may resolve to a path that doesn't exist on test
        // hosts (no ~/.weclawbot/console/). Should silently fall back.
        let body = console_html();
        assert!(!body.is_empty());
    }
}
