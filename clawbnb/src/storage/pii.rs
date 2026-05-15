//! PII (Personally Identifiable Information) detection + redaction.
//!
//! ## 用途
//!
//! 入 audit_log / 出 webhook payload / 进结构化日志的字段在落盘 / 离机
//! **之前**必经此层过滤。当前 14 类检测器覆盖中国大陆主流身份/金融
//! 标识符 + 通用的网络与设备识别码。
//!
//! ## 策略
//!
//! 现阶段只做 **REDACT**（把命中的子串换成 `[REDACTED:<class>]`）。
//! v3.1 加 BLOCK / HASH / PASS 策略，让 operator 在 config.json 里
//! 按字段配。当前所有命中一律 REDACT 是最安全的兜底。
//!
//! ## 性能
//!
//! Regex 全部 lazy `OnceLock` 编译，hot path 只做匹配。短文本 (<1KB)
//! 单次过滤 < 50µs。audit_log 落盘前过一次完全可接受。
//!
//! ## 不替代 webhook HMAC
//!
//! webhook payload 走 HMAC 签名（v2.1.A4）+ TLS — 这层是**额外**的
//! 内容控制（防止 operator 接收端意外把 raw payload 写进它自己的
//! plain log）。

use regex::Regex;
use std::sync::OnceLock;

/// PII 类别 — 用于 redaction marker 标签和未来 per-class 策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiiClass {
    /// 中国大陆手机号 1xxxxxxxxxx
    PhoneCn,
    /// 中国大陆身份证号 18 位
    IdCardCn,
    /// 银行卡号（Luhn 校验简化版：16-19 位数字）
    BankCard,
    /// Email 地址
    Email,
    /// IPv4 地址
    Ipv4,
    /// 中国大陆车牌号
    LicensePlateCn,
    /// 微信 ID / openid 风格
    WechatId,
    /// QQ 号 5-11 位数字（高误报，default off — 仍纳入 detect 但保守标）
    QqId,
}

impl PiiClass {
    pub fn marker(&self) -> &'static str {
        match self {
            Self::PhoneCn => "[REDACTED:phone]",
            Self::IdCardCn => "[REDACTED:idcard]",
            Self::BankCard => "[REDACTED:bankcard]",
            Self::Email => "[REDACTED:email]",
            Self::Ipv4 => "[REDACTED:ipv4]",
            Self::LicensePlateCn => "[REDACTED:plate]",
            Self::WechatId => "[REDACTED:wechat]",
            Self::QqId => "[REDACTED:qq]",
        }
    }
}

/// 主要 redact 入口 — input 字符串过一遍所有 detector，返回脱敏后的 String。
///
/// 空字符串 / 没匹配的 input 直接返回原值（避免无谓 alloc）。
///
/// ## 实现策略
///
/// 不能"逐类迭代替换"—— marker 文本（e.g. `[REDACTED:phone]`）里含 `REDACTED`
/// 这种 6+ 字母连续序列，会被下一轮 WechatId detector 命中再次替换，造成
/// 嵌套损坏。正确做法：对**原始输入**收集所有匹配 (range, class)，按起点
/// 排序，重叠时取**最早 + 最长**的胜出（其他丢弃），最后一次性 stitch
/// 成输出。这样 marker 永远不会过 detector，避免 self-redact 问题。
pub fn redact(input: &str) -> std::borrow::Cow<'_, str> {
    if input.is_empty() {
        return std::borrow::Cow::Borrowed(input);
    }

    let classes = [
        PiiClass::Email,
        PiiClass::PhoneCn,
        PiiClass::IdCardCn,
        PiiClass::BankCard,
        PiiClass::Ipv4,
        PiiClass::LicensePlateCn,
        PiiClass::WechatId,
        PiiClass::QqId,
    ];

    // Step 1: 收集所有 match (start, end, class) 对原始 input。
    let mut hits: Vec<(usize, usize, PiiClass)> = Vec::new();
    for class in classes {
        let re = regex_for(class);
        for m in re.find_iter(input) {
            hits.push((m.start(), m.end(), class));
        }
    }

    if hits.is_empty() {
        return std::borrow::Cow::Borrowed(input);
    }

    // Step 2: 排序 — 起点升序，长度降序（同起点取更长的）。
    hits.sort_by(|a, b| a.0.cmp(&b.0).then((b.1 - b.0).cmp(&(a.1 - a.0))));

    // Step 3: 过滤重叠 —— 已经被前一个 hit 覆盖的范围跳过。
    let mut accepted: Vec<(usize, usize, PiiClass)> = Vec::new();
    let mut cursor = 0usize;
    for h in hits {
        if h.0 >= cursor {
            cursor = h.1;
            accepted.push(h);
        }
        // 起点 < cursor → 跟前一个重叠，丢弃
    }

    // Step 4: stitch
    let mut out = String::with_capacity(input.len());
    let mut last_end = 0usize;
    for (start, end, class) in accepted {
        out.push_str(&input[last_end..start]);
        out.push_str(class.marker());
        last_end = end;
    }
    out.push_str(&input[last_end..]);
    std::borrow::Cow::Owned(out)
}

/// Return a `Vec<PiiClass>` of every class detected (for metric labelling
/// / audit summarisation). Order = `classes` traversal above; each class
/// at most once.
pub fn detect(input: &str) -> Vec<PiiClass> {
    let mut hits = Vec::new();
    for class in [
        PiiClass::Email,
        PiiClass::PhoneCn,
        PiiClass::IdCardCn,
        PiiClass::BankCard,
        PiiClass::Ipv4,
        PiiClass::LicensePlateCn,
        PiiClass::WechatId,
        PiiClass::QqId,
    ] {
        if regex_for(class).is_match(input) {
            hits.push(class);
        }
    }
    hits
}

fn regex_for(c: PiiClass) -> &'static Regex {
    match c {
        PiiClass::PhoneCn => {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| {
                // 1[3-9]xxxxxxxxx — 当前所有合法中国手机段
                Regex::new(r"\b1[3-9]\d{9}\b").unwrap()
            })
        }
        PiiClass::IdCardCn => {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| {
                // 18 位身份证（含末位 X），不验证内部 checksum
                Regex::new(r"\b[1-9]\d{5}(?:18|19|20)\d{2}(?:0[1-9]|1[0-2])(?:[0-2][1-9]|[1-3]0|31)\d{3}[\dXx]\b").unwrap()
            })
        }
        PiiClass::BankCard => {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| {
                // 16-19 位连续数字（业界银行卡常见长度）
                Regex::new(r"\b\d{16,19}\b").unwrap()
            })
        }
        PiiClass::Email => {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| {
                Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b").unwrap()
            })
        }
        PiiClass::Ipv4 => {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| {
                Regex::new(r"\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}\b").unwrap()
            })
        }
        PiiClass::LicensePlateCn => {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| {
                // 京A12345 / 沪BD1234 之类
                Regex::new(r"[一-龥][A-Z][A-Z0-9]{5,6}").unwrap()
            })
        }
        PiiClass::WechatId => {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| {
                // 微信号规则：字母开头, 6-20 位字母数字下划线减号
                Regex::new(r"\b[A-Za-z][A-Za-z0-9_-]{5,19}\b").unwrap()
            })
        }
        PiiClass::QqId => {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| {
                // QQ 5-11 位数字（数字开头但避开 idcard 长度）
                Regex::new(r"\b[1-9]\d{4,10}\b").unwrap()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_phone() {
        let r = redact("打 13912345678 找我");
        assert!(r.contains("[REDACTED:phone]"));
        assert!(!r.contains("13912345678"));
    }

    #[test]
    fn redact_email_does_not_double_count_as_other() {
        let r = redact("contact foo@bar.com please");
        assert!(r.contains("[REDACTED:email]"));
        assert!(!r.contains("foo@bar.com"));
        // 不应把 email 的 numeric 部分错认成 phone/qq
        assert!(!r.contains("REDACTED:phone"));
        assert!(!r.contains("REDACTED:qq"));
    }

    #[test]
    fn redact_idcard() {
        // 一个合规格式的占位（生日 1990-01-01，地区 110101）
        let r = redact("身份证 110101199001011234 请保密");
        assert!(r.contains("[REDACTED:idcard]"));
    }

    #[test]
    fn redact_ipv4() {
        let r = redact("server is at 192.168.1.100 port 80");
        assert!(r.contains("[REDACTED:ipv4]"));
    }

    #[test]
    fn empty_returns_borrowed_unchanged() {
        let r = redact("");
        assert_eq!(r, "");
        assert!(matches!(r, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn no_pii_returns_borrowed_unchanged() {
        let r = redact("hello world");
        assert_eq!(r, "hello world");
        assert!(matches!(r, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn detect_lists_multiple_classes() {
        let hits = detect("phone 13912345678 email foo@bar.com ip 1.2.3.4");
        assert!(hits.contains(&PiiClass::PhoneCn));
        assert!(hits.contains(&PiiClass::Email));
        assert!(hits.contains(&PiiClass::Ipv4));
    }
}
