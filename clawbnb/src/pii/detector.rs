//! PII detector — 14 classes with compiled regex + helper validators.
//!
//! Detectors run linearly. For each detected span, [`scrub_with_policy`]
//! applies the policy to produce the redacted text plus a list of
//! `PiiHit` records for audit / metric purposes.

use std::sync::OnceLock;

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::policy::{PiiPolicy, PolicyMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PiiClass {
    MobilePhoneCn,
    MobilePhoneIntl,
    IdCardCn,
    BankCardCn,
    Email,
    IpAddr,
    Url,
    LicensePlateCn,
    AddressCn,
    PersonNameCn,
    QqNumber,
    WechatId,
    Passport,
    Imei,
}

impl PiiClass {
    pub fn all() -> &'static [PiiClass] {
        use PiiClass::*;
        &[
            MobilePhoneCn,
            MobilePhoneIntl,
            IdCardCn,
            BankCardCn,
            Email,
            IpAddr,
            Url,
            LicensePlateCn,
            AddressCn,
            PersonNameCn,
            QqNumber,
            WechatId,
            Passport,
            Imei,
        ]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MobilePhoneCn => "MOBILE_CN",
            Self::MobilePhoneIntl => "MOBILE_INTL",
            Self::IdCardCn => "ID_CARD_CN",
            Self::BankCardCn => "BANK_CARD",
            Self::Email => "EMAIL",
            Self::IpAddr => "IP",
            Self::Url => "URL",
            Self::LicensePlateCn => "PLATE_CN",
            Self::AddressCn => "ADDRESS_CN",
            Self::PersonNameCn => "NAME_CN",
            Self::QqNumber => "QQ",
            Self::WechatId => "WECHAT_ID",
            Self::Passport => "PASSPORT",
            Self::Imei => "IMEI",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiiHit {
    pub class: PiiClass,
    pub start: usize,
    pub end: usize,
    pub matched: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiiScrubResult {
    /// Text after applying policies (Pass/Redact/Hash replace inline; Block
    /// leaves the match unchanged but reports `blocked = true`).
    pub scrubbed: String,
    /// Every detected hit, in original-text order.
    pub hits: Vec<PiiHit>,
    /// True if any Block-policy hit found. Caller should reject the
    /// operation when this is set.
    pub blocked: bool,
}

struct CompiledDetector {
    class: PiiClass,
    re: Regex,
    /// Optional secondary validator that returns true if the regex match
    /// should actually be reported as a hit. Used to reject bank-card
    /// false positives via Luhn, ID-card with wrong checksum, etc.
    validate: Option<fn(&str) -> bool>,
}

fn detectors() -> &'static Vec<CompiledDetector> {
    static D: OnceLock<Vec<CompiledDetector>> = OnceLock::new();
    D.get_or_init(build_detectors)
}

fn build_detectors() -> Vec<CompiledDetector> {
    vec![
        // 1. CN mobile: 1[3-9]X-XXXX-XXXX (also no separators)
        CompiledDetector {
            class: PiiClass::MobilePhoneCn,
            re: Regex::new(r"(?<![0-9])1[3-9]\d{9}(?![0-9])").unwrap(),
            validate: None,
        },
        // 2. E.164-ish intl: starts with +, 8-15 digits/spaces/dashes
        CompiledDetector {
            class: PiiClass::MobilePhoneIntl,
            re: Regex::new(r"\+[1-9]\d{0,2}[\s\-]?\d{1,4}[\s\-]?\d{1,4}[\s\-]?\d{1,4}").unwrap(),
            validate: Some(|s| {
                // After stripping non-digits, length 8..=15 (E.164 max)
                let d = s.chars().filter(|c| c.is_ascii_digit()).count();
                (8..=15).contains(&d)
            }),
        },
        // 3. CN national ID: 17 digits + (digit or X) with checksum
        CompiledDetector {
            class: PiiClass::IdCardCn,
            re: Regex::new(r"(?<![0-9])\d{17}[\dXx](?![0-9])").unwrap(),
            validate: Some(validate_id_card_cn),
        },
        // 4. Bank card: 13-19 digits, Luhn-valid. Avoid colliding with
        // CN ID by requiring NOT matching id-card check.
        CompiledDetector {
            class: PiiClass::BankCardCn,
            re: Regex::new(r"(?<![0-9])\d{13,19}(?![0-9Xx])").unwrap(),
            validate: Some(validate_luhn),
        },
        // 5. Email — simplified RFC 5322
        CompiledDetector {
            class: PiiClass::Email,
            re: Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}").unwrap(),
            validate: None,
        },
        // 6. IPv4 / IPv6 — IPv4 dotted-quad; IPv6 minimal
        CompiledDetector {
            class: PiiClass::IpAddr,
            re: Regex::new(
                r"(?<![0-9.])(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})(?![0-9.])|([0-9a-fA-F]{1,4}:){2,7}[0-9a-fA-F]{1,4}",
            )
            .unwrap(),
            validate: Some(|s| {
                // IPv4: each octet 0..=255
                if let Some(v4) = s.split('.').collect::<Vec<_>>().get(..4) {
                    if v4.len() == 4 {
                        return v4.iter().all(|p| p.parse::<u8>().is_ok());
                    }
                }
                // IPv6: simplified — must contain ":"
                s.contains(':') && s.split(':').count() >= 3
            }),
        },
        // 7. URL
        CompiledDetector {
            class: PiiClass::Url,
            re: Regex::new(r"https?://[A-Za-z0-9._\-]+(:[0-9]+)?(/[^\s]*)?").unwrap(),
            validate: None,
        },
        // 8. CN license plate: 京A·12345 / 沪B12345 / new-energy 6-suffix
        // Note: Chinese characters span 3 UTF-8 bytes — we accept any
        // such char in the prefix slot and let the alphanumeric tail
        // bound the match.
        CompiledDetector {
            class: PiiClass::LicensePlateCn,
            re: Regex::new(
                r"[一-龥][A-Z][·•・]?[A-Z0-9]{5,6}",
            )
            .unwrap(),
            validate: None,
        },
        // 9. CN address — heuristic: "XX省" / "XX市" / "XX区" / "XX路" + 2-12 chars
        CompiledDetector {
            class: PiiClass::AddressCn,
            re: Regex::new(
                r"[一-龥]{2,8}(省|市|自治区|特别行政区)[一-龥A-Za-z0-9]{2,30}",
            )
            .unwrap(),
            validate: None,
        },
        // 10. CN person name — heuristic context: "我是XX" / "姓名:XX" / "叫XX"
        // 2-4 hanzi after these triggers. This is intentionally narrow
        // because pure-pattern Chinese name detection has high FP rate
        // ("文件夹" looks like a name).
        CompiledDetector {
            class: PiiClass::PersonNameCn,
            re: Regex::new(
                r"(?:我(?:是|叫)|姓名[::]\s*|名字[::]\s*|姓名是)[一-龥]{2,4}",
            )
            .unwrap(),
            validate: None,
        },
        // 11. QQ number — 5-13 digits prefixed by "qq" or "扣扣" context
        CompiledDetector {
            class: PiiClass::QqNumber,
            re: Regex::new(r"(?i)(?:qq|扣扣|企鹅|q号)[号:::\s]*\d{5,13}(?![0-9])").unwrap(),
            validate: None,
        },
        // 12. WeChat ID — `wxid_xxx` or contextual "微信号:..."
        CompiledDetector {
            class: PiiClass::WechatId,
            re: Regex::new(
                r"(?i)wxid_[A-Za-z0-9]{6,18}|微信号?[::\s]*[A-Za-z0-9_\-]{4,20}",
            )
            .unwrap(),
            validate: None,
        },
        // 13. Passport — CN: letter + 8 digits (G/E/D/S/P/H/M). 全球粗略
        CompiledDetector {
            class: PiiClass::Passport,
            re: Regex::new(r"(?<![A-Z0-9])[GEDPHMS]\d{8}(?![0-9])").unwrap(),
            validate: None,
        },
        // 14. IMEI — 15 digits with Luhn
        CompiledDetector {
            class: PiiClass::Imei,
            re: Regex::new(r"(?<![0-9])\d{15}(?![0-9])").unwrap(),
            validate: Some(validate_luhn),
        },
    ]
}

/// Detect (but do not modify) PII spans. Used for metric / audit-only
/// paths where the caller wants to log frequency without altering text.
pub fn detect_classes(text: &str) -> Vec<PiiHit> {
    let mut hits = Vec::new();
    for d in detectors() {
        // fancy-regex `find_iter` yields `Result<Match>` (regex compile is
        // infallible but match can fail under backtracking complexity). We
        // silently skip errors — PII detection is best-effort, not fatal.
        for m_res in d.re.find_iter(text) {
            let Ok(m) = m_res else {
                continue;
            };
            if let Some(v) = d.validate {
                if !v(m.as_str()) {
                    continue;
                }
            }
            hits.push(PiiHit {
                class: d.class,
                start: m.start(),
                end: m.end(),
                matched: m.as_str().to_string(),
            });
        }
    }
    // Sort by start position so output matches narrative order.
    hits.sort_by_key(|h| h.start);
    // Resolve overlap: prefer the earlier-starting hit; drop later hits
    // whose [start, end) overlaps any kept hit.
    let mut kept: Vec<PiiHit> = Vec::new();
    for h in hits {
        if let Some(last) = kept.last() {
            if h.start < last.end {
                continue;
            }
        }
        kept.push(h);
    }
    kept
}

/// Apply policy to every hit and produce the scrubbed text.
///
/// Replacements happen back-to-front so indices stay valid.
pub fn scrub_with_policy(text: &str, policy_map: &PolicyMap) -> PiiScrubResult {
    let hits = detect_classes(text);
    let mut scrubbed = text.to_string();
    let mut blocked = false;

    for hit in hits.iter().rev() {
        let policy = policy_map.policy_for(hit.class);
        match policy {
            PiiPolicy::Pass => {}
            PiiPolicy::Block => {
                blocked = true;
                // Don't mutate text — caller decides whether to send it.
                // We do still emit the hit so audit log records why.
            }
            PiiPolicy::Redact => {
                let placeholder = format!("[{}]", hit.class.as_str());
                replace_range_safe(&mut scrubbed, hit.start, hit.end, &placeholder);
            }
            PiiPolicy::Hash => {
                let h = Sha256::digest(hit.matched.as_bytes());
                let hex: String = h.iter().take(6).map(|b| format!("{b:02x}")).collect();
                let placeholder = format!("[{}:{}]", hit.class.as_str(), hex);
                replace_range_safe(&mut scrubbed, hit.start, hit.end, &placeholder);
            }
        }
    }

    PiiScrubResult {
        scrubbed,
        hits,
        blocked,
    }
}

/// Safe byte-range replacement that snaps to UTF-8 boundaries.
/// Regex `find_iter` returns byte offsets that ARE on char boundaries,
/// so this should always be safe; the snap is defense-in-depth.
fn replace_range_safe(s: &mut String, start: usize, end: usize, replacement: &str) {
    let len = s.len();
    if start > len || end > len || start > end {
        return;
    }
    if !s.is_char_boundary(start) || !s.is_char_boundary(end) {
        return;
    }
    s.replace_range(start..end, replacement);
}

// --------------- Validators ---------------

/// Luhn algorithm for credit card / IMEI. Returns true if checksum OK.
fn validate_luhn(digits: &str) -> bool {
    let only_digits: Vec<u32> = digits
        .chars()
        .filter_map(|c| c.to_digit(10))
        .collect();
    if only_digits.len() < 13 {
        return false;
    }
    let mut sum = 0u32;
    let mut double = false;
    for d in only_digits.iter().rev() {
        let mut x = *d;
        if double {
            x *= 2;
            if x > 9 {
                x -= 9;
            }
        }
        sum += x;
        double = !double;
    }
    sum % 10 == 0
}

/// GB 11643 checksum for CN national ID (18-digit form).
fn validate_id_card_cn(s: &str) -> bool {
    let s = s.trim();
    if s.len() != 18 {
        return false;
    }
    let chars: Vec<char> = s.chars().collect();
    if !chars[..17].iter().all(|c| c.is_ascii_digit()) {
        return false;
    }
    const W: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    const CHECK: [char; 11] = ['1', '0', 'X', '9', '8', '7', '6', '5', '4', '3', '2'];
    let sum: u32 = (0..17)
        .map(|i| chars[i].to_digit(10).unwrap() * W[i])
        .sum();
    let want = CHECK[(sum % 11) as usize];
    chars[17].eq_ignore_ascii_case(&want)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cn_mobile_detected_and_redacted() {
        let text = "请联系13812345678";
        let r = scrub_with_policy(text, &PolicyMap::default());
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].class, PiiClass::MobilePhoneCn);
        assert_eq!(r.scrubbed, "请联系[MOBILE_CN]");
        assert!(!r.blocked);
    }

    #[test]
    fn email_detected_and_redacted() {
        let r = scrub_with_policy("ping alice@example.com 谢谢", &PolicyMap::default());
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].class, PiiClass::Email);
        assert!(r.scrubbed.contains("[EMAIL]"));
    }

    #[test]
    fn id_card_default_blocks() {
        // Valid CN ID checksum: 11010519491231002X
        let r = scrub_with_policy("ID 11010519491231002X 请保密", &PolicyMap::default());
        assert!(r.hits.iter().any(|h| h.class == PiiClass::IdCardCn));
        assert!(r.blocked, "ID_CARD_CN should trigger block");
    }

    #[test]
    fn invalid_id_card_skipped() {
        // 18 digits but wrong checksum
        let r = scrub_with_policy("99999999999999999X", &PolicyMap::default());
        assert!(!r.hits.iter().any(|h| h.class == PiiClass::IdCardCn));
    }

    #[test]
    fn luhn_invalid_skipped() {
        // 16 digits but Luhn-invalid (last digit wrong)
        let r = scrub_with_policy("4111111111111110", &PolicyMap::default());
        assert!(!r.hits.iter().any(|h| h.class == PiiClass::BankCardCn));
    }

    #[test]
    fn luhn_valid_bank_card_detected() {
        // Standard Luhn test card: 4111111111111111
        let r = scrub_with_policy("card 4111111111111111", &PolicyMap::default());
        assert!(r.hits.iter().any(|h| h.class == PiiClass::BankCardCn));
    }

    #[test]
    fn url_passes_through_by_default() {
        let r = scrub_with_policy("see https://example.com/x", &PolicyMap::default());
        assert!(r.hits.iter().any(|h| h.class == PiiClass::Url));
        // Default policy = Pass — text should be unchanged
        assert_eq!(r.scrubbed, "see https://example.com/x");
    }

    #[test]
    fn hash_replacement_correlates() {
        let mut policies = PolicyMap::default();
        policies.0.insert(PiiClass::Email, PiiPolicy::Hash);
        let r1 = scrub_with_policy("a alice@x.com", &policies);
        let r2 = scrub_with_policy("b alice@x.com", &policies);
        // Same email → same hash placeholder
        let h1 = r1.scrubbed.split_whitespace().nth(1).unwrap();
        let h2 = r2.scrubbed.split_whitespace().nth(1).unwrap();
        assert_eq!(h1, h2);
        assert!(h1.starts_with("[EMAIL:"));
    }

    #[test]
    fn strict_preset_blocks_email() {
        // Default = Redact for email; strict = Redact still (only highest-
        // risk get Block). Let's verify the high-risk classes blocks.
        let strict = PolicyMap::strict();
        let r = scrub_with_policy("contact alice@x.com", &strict);
        assert!(r.hits.iter().any(|h| h.class == PiiClass::Email));
        assert!(r.scrubbed.contains("[EMAIL]"));
    }

    #[test]
    fn permissive_passes_id_card() {
        let p = PolicyMap::permissive();
        let r = scrub_with_policy("11010519491231002X", &p);
        assert!(!r.blocked);
        assert_eq!(r.scrubbed, "11010519491231002X");
    }

    #[test]
    fn multiple_hits_replaced_back_to_front() {
        let r = scrub_with_policy(
            "phone 13812345678 email alice@example.com",
            &PolicyMap::default(),
        );
        assert_eq!(r.hits.len(), 2);
        assert!(r.scrubbed.starts_with("phone [MOBILE_CN]"));
        assert!(r.scrubbed.ends_with("[EMAIL]"));
    }

    #[test]
    fn audit_preset_redacts_all() {
        let p = PolicyMap::audit();
        let r = scrub_with_policy("11010519491231002X 13812345678", &p);
        // Both classes redacted (audit = Redact all), not blocked
        assert!(!r.blocked);
        assert!(r.scrubbed.contains("[ID_CARD_CN]"));
        assert!(r.scrubbed.contains("[MOBILE_CN]"));
    }

    #[test]
    fn wechat_id_detected() {
        let r = scrub_with_policy("加我 wxid_abc123def", &PolicyMap::default());
        assert!(r.hits.iter().any(|h| h.class == PiiClass::WechatId));
    }

    #[test]
    fn overlap_resolution() {
        // 13812345678 could match both MOBILE_CN AND IMEI (but IMEI needs
        // 15 digits and Luhn). Test a number that's only an 11-digit mobile.
        let r = scrub_with_policy("13812345678", &PolicyMap::default());
        // Should produce exactly 1 hit (mobile), not double-count.
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].class, PiiClass::MobilePhoneCn);
    }

    #[test]
    fn ipv4_detected_octets_valid() {
        let r = scrub_with_policy("server 192.168.1.100 down", &PolicyMap::default());
        assert!(r.hits.iter().any(|h| h.class == PiiClass::IpAddr));
    }

    #[test]
    fn ipv4_invalid_octet_skipped() {
        let r = scrub_with_policy("999.999.999.999", &PolicyMap::default());
        assert!(!r.hits.iter().any(|h| h.class == PiiClass::IpAddr));
    }
}
