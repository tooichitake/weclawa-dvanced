//! GUI console HTML — v5.4 split assets (L5.1 done).
//!
//! ## 设计
//!
//! v5.4: `assets/console.html` 拆成三个文件：
//! - `assets/console/index.html` — body markup + `<link href="app.css">`
//!   + `<script src="app.js">`
//! - `assets/console/app.css` — 全部 CSS (241 行)
//! - `assets/console/app.js` — 全部 JS (1623 行)
//!
//! 用 `include_dir!()` 把 `assets/console/` 整目录嵌进 binary，保证单
//! binary 部署不破。Operator override 路径：`~/.weclawbot/console/`
//! 优先级高于 embedded fallback。
//!
//! ## 运行时 serve 策略
//!
//! - `GET /` → `index.html` (用 console_html() 读 override-or-embed)
//! - `GET /console/app.css` 等 → server.rs 挂 ServeDir layer
//!   (`tower_http::services::ServeDir`)，操作员热改 CSS/JS 不需要重启
//!   daemon。embed fallback 走 [`serve_asset`] handler。
//!
//! ## 为什么不引入 build step (Vite/webpack)
//!
//! 项目目标是**单 binary 部署** (user_preferences.md：no Node.js runtime
//! deps)。所有 GUI 用现代浏览器原生 ESM module + classic script 即可。
//! app.js 是 IIFE 风格不需要 bundle；CSS 是单文件不需要 PostCSS。

use std::path::PathBuf;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use include_dir::{include_dir, Dir};

/// Embedded fallback: assets/console/ 全部文件编译期嵌进 binary。
/// 改这个目录不需要 cargo clean — include_dir crate 在 fs 改动时让
/// rustc 重建本 module。
static EMBED: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/assets/console");

/// fs override 根目录：`~/.weclawbot/console/`。operator 把 index.html /
/// app.css / app.js 任一个文件丢这里都会优先于 embed 版本。
///
/// v3 SaaS hosted 模式可以 mount per-tenant 自定义品牌资源在这个位置。
pub fn override_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".weclawbot").join("console"))
}

/// Path 解析：override fs > embedded。返回内容 + 推断的 content-type。
fn load_asset(rel_path: &str) -> Option<(Vec<u8>, &'static str)> {
    // Defense in depth: 拒绝任何 `..` / 绝对 / Windows reverse slash 路径
    if rel_path.contains("..") || rel_path.starts_with('/') || rel_path.contains('\\') {
        return None;
    }

    // 1) fs override
    if let Some(dir) = override_dir() {
        let fs_path = dir.join(rel_path);
        if fs_path.is_file() {
            if let Ok(bytes) = std::fs::read(&fs_path) {
                return Some((bytes, guess_content_type(rel_path)));
            }
        }
    }

    // 2) embedded fallback
    if let Some(file) = EMBED.get_file(rel_path) {
        return Some((file.contents().to_vec(), guess_content_type(rel_path)));
    }

    None
}

fn guess_content_type(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

/// `GET /` → `index.html`. 兼容老 API：返回 `Cow<'static, str>` 让现有
/// `routes::get_root` 不动签名。
pub fn console_html() -> std::borrow::Cow<'static, str> {
    match load_asset("index.html") {
        Some((bytes, _)) => match String::from_utf8(bytes) {
            Ok(s) => std::borrow::Cow::Owned(s),
            Err(e) => {
                tracing::warn!(
                    "console index.html is not valid UTF-8: {e} — falling back to error page"
                );
                std::borrow::Cow::Borrowed(
                    "<!DOCTYPE html><html><body><h1>weclawbot</h1>\
                     <p>console asset corrupted; check ~/.weclawbot/console/</p></body></html>",
                )
            }
        },
        None => std::borrow::Cow::Borrowed(
            "<!DOCTYPE html><html><body><h1>weclawbot</h1>\
             <p>console index.html missing</p></body></html>",
        ),
    }
}

/// `GET /console/*path` axum handler — serve CSS/JS/sub-assets。
/// override fs 优先，否则 embed fallback。404 if neither has it.
pub async fn serve_asset(
    axum::extract::Path(rel_path): axum::extract::Path<String>,
) -> Response {
    match load_asset(&rel_path) {
        Some((bytes, ct)) => {
            let mut resp = (StatusCode::OK, bytes).into_response();
            resp.headers_mut()
                .insert(header::CONTENT_TYPE, ct.parse().unwrap());
            // 30 sec cache — operator 改 fs override 后浏览器 1 次刷新就
            // 拿到新内容，不需要 daemon 重启。embed 版本下不变化但
            // operator 也可能 swap binary，不要长 cache。
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                "public, max-age=30".parse().unwrap(),
            );
            resp
        }
        None => (StatusCode::NOT_FOUND, format!("not found: {rel_path}")).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_in_index_is_html() {
        let body = console_html();
        assert!(body.contains("<html") || body.contains("<!DOCTYPE"));
        // Sanity: index.html 应该引用 app.css / app.js
        assert!(body.contains("app.css"));
        assert!(body.contains("app.js"));
    }

    #[test]
    fn embed_has_three_files() {
        // assets/console/{index.html, app.css, app.js} 都应被 include_dir!
        // 编进 binary
        assert!(EMBED.get_file("index.html").is_some(), "index.html missing");
        assert!(EMBED.get_file("app.css").is_some(), "app.css missing");
        assert!(EMBED.get_file("app.js").is_some(), "app.js missing");
    }

    #[test]
    fn load_asset_known_paths() {
        assert!(load_asset("app.css").is_some());
        assert!(load_asset("app.js").is_some());
        assert!(load_asset("index.html").is_some());
    }

    #[test]
    fn load_asset_rejects_traversal() {
        assert!(load_asset("../Cargo.toml").is_none());
        assert!(load_asset("/etc/passwd").is_none());
        assert!(load_asset("..\\..\\..\\windows\\system32").is_none());
    }

    #[test]
    fn content_type_inference() {
        assert_eq!(guess_content_type("app.css"), "text/css; charset=utf-8");
        assert_eq!(guess_content_type("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(guess_content_type("index.html"), "text/html; charset=utf-8");
    }
}
