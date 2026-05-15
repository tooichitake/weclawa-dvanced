//! Server-side QR code rendering — v5.4 (fixes accounts QR bug).
//!
//! ## Why this exists
//!
//! iLink `get_bot_qrcode` 返回的 `qrcode_img_content` **是个 URL 字符串**
//! ——不是 base64 PNG。GUI 之前误把它当 base64 PNG 直接塞进 `<img
//! src="data:image/png;base64,..." />`，浏览器解码失败 → QR 不显示。
//!
//! 参考 OpenClaw 2026.5 TS reference (`src/weixin/auth/login-qr.ts:539`)：
//! `qrterm.default.generate(qrResponse.qrcode_img_content, ...)` — TS 端
//! 用 `qrcode-terminal` 把这个 URL **编码成 QR 图像**再渲染。
//!
//! Rust GUI 同样需要把这个 URL 编码成 QR 图像。本模块用 `qrcode = "0.14"`
//! crate (已在 deps 树 —— 终端 print_qr_to_terminal 也用它) 生成 SVG，
//! 再 base64 包装成 `data:image/svg+xml;base64,...` data URL，前端 `<img
//! src=...>` 直接显示。
//!
//! SVG 而不是 PNG 因为：
//! - qrcode crate 0.14 原生支持 SVG render；PNG 要拉 image crate
//! - SVG 体积比 PNG 小很多 (~1KB vs ~4-8KB)
//! - 浏览器 100% 支持 `data:image/svg+xml;base64`，无依赖

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use qrcode::QrCode;
use qrcode::render::svg;

/// 把任意 URL/字符串编码成 QR 码，返回完整的 SVG data URL。
///
/// 返回 `data:image/svg+xml;base64,<encoded>`，前端 `<img src=...>` 可
/// 直接渲染。**失败** (输入太长 / qrcode crate 内部错误) 返回 `None`，
/// caller 应回退到显示原始 URL 文本让用户手动复制。
pub fn url_to_qr_data_url(url: &str) -> Option<String> {
    let code = QrCode::new(url.as_bytes()).ok()?;
    // 渲染配置：黑底白码 + 4 modules quiet zone（QR 标准要求）+
    // 8 px module size。最终 SVG 大小 ~240-280px，跟 GUI CSS
    // (.qr-wrap img { width: 240px }) 匹配。
    let svg_string = code
        .render::<svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build();
    let encoded = BASE64.encode(svg_string.as_bytes());
    Some(format!("data:image/svg+xml;base64,{encoded}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_url_renders() {
        let r = url_to_qr_data_url("https://example.com/login?session=abc123");
        assert!(r.is_some());
        let s = r.unwrap();
        assert!(s.starts_with("data:image/svg+xml;base64,"));
        // Decode + spot-check: SVG body should contain "svg" tag
        let b64 = s.strip_prefix("data:image/svg+xml;base64,").unwrap();
        let decoded = BASE64.decode(b64).unwrap();
        let svg = String::from_utf8(decoded).unwrap();
        assert!(svg.contains("<svg"), "expected SVG, got: {}", &svg[..80.min(svg.len())]);
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn empty_input_handled() {
        // Empty input is technically valid for QR (encodes empty data)
        let r = url_to_qr_data_url("");
        // qrcode crate handles this fine — should still produce a (tiny) QR
        assert!(r.is_some());
    }

    #[test]
    fn long_url_renders() {
        // Typical iLink QR URL is ~200-300 chars; verify well within QR cap
        let long = format!("https://ilinkai.weixin.qq.com/qr/{}", "x".repeat(200));
        let r = url_to_qr_data_url(&long);
        assert!(r.is_some());
    }

    #[test]
    fn unicode_input_renders() {
        let r = url_to_qr_data_url("登录 weclawbot — 扫码确认");
        assert!(r.is_some());
    }
}
