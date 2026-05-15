//! PII detection + redaction pipeline — v5.5 (v3 SaaS M2 follow-up).
//!
//! ## Why this module exists
//!
//! Multi-tenant SaaS deployments need to:
//! 1. **Audit log** without leaking user PII (phone / ID / email shouldn't
//!    appear in `before_json` / `after_json` columns the operator can read)
//! 2. **Webhook** without leaking user PII to operator-configured third-party
//!    sinks (`webhook.url` carries inbound text by default)
//! 3. **AI prompt** with PII optionally scrubbed before being sent to Claude
//!    (HIPAA/SOC2/GDPR mode — `compliance.pii_scrub_outbound = true`)
//!
//! ## Detector classes (14)
//!
//! Optimized for Chinese + global formats (weclawbot's primary users are
//! WeChat businesses, with secondary Telegram/Feishu in CN + intl).
//!
//! | # | Class | Pattern shape | Default policy |
//! |---|---|---|---|
//! | 1 | `MobilePhoneCn` | 13/14/15/16/17/18/19[0-9]{9} (CN 11-digit mobile) | REDACT |
//! | 2 | `MobilePhoneIntl` | `+\d{1,3}[\d -]{7,15}` (E.164-ish) | REDACT |
//! | 3 | `IdCardCn` | 18-digit Chinese national ID (with checksum) | BLOCK |
//! | 4 | `BankCardCn` | 16-19 digit Luhn-valid | REDACT |
//! | 5 | `Email` | RFC 5322 simplified | REDACT |
//! | 6 | `IpAddr` | IPv4 / IPv6 | REDACT |
//! | 7 | `Url` | `https?://...` | PASS (allowed by default; opt-in REDACT) |
//! | 8 | `LicensePlateCn` | 京A·12345 etc | REDACT |
//! | 9 | `AddressCn` | "XX省XX市XX区..." | HASH |
//! | 10 | `PersonNameCn` | Heuristic 2-4 hanzi after 姓名/我是/称呼 | HASH |
//! | 11 | `QqNumber` | 5-13 digit context "QQ" / "扣扣" | REDACT |
//! | 12 | `WechatId` | `wxid_xxx` / context-prefixed | REDACT |
//! | 13 | `Passport` | letter + 7-9 digits (CN: E12345678 / G12345678) | BLOCK |
//! | 14 | `Imei` | 15 digits with Luhn | BLOCK |
//!
//! ## Policies
//!
//! - `Pass` — leave text unchanged
//! - `Redact` — replace with `[<CLASS>]` placeholder (default for low-risk
//!   identifiers; preserves audit log readability)
//! - `Hash` — replace with `[<CLASS>:<sha256-12-hex>]` (allows correlation
//!   across log entries without revealing the value)
//! - `Block` — caller must refuse the operation entirely (e.g. webhook
//!   handler returns 4xx; inbound handler replies "请勿在消息中包含敏感
//!   信息"). Used for highest-risk classes (national ID, IMEI, passport)
//!
//! ## Performance
//!
//! Detectors are compiled `Regex` instances inside a process-global
//! `OnceLock<Vec<CompiledDetector>>`. Per-message scan = single linear
//! pass per detector = O(N×K) where N=text len, K=14. Typical WeChat
//! inbound is ≤500 chars, so worst-case scan is ~7000 char ops = sub-ms.
//!
//! ## Configuration
//!
//! `config.compliance.pii_audit_policy` / `pii_webhook_policy` /
//! `pii_ai_prompt_policy` — three independent policy maps. Each maps
//! class name → policy. Missing class = use compiled-in default (table
//! above). See `crate::config::ComplianceConfig`.

mod detector;
#[cfg(test)]
mod integration_tests;
pub mod policy;

pub use detector::{detect_classes, scrub_with_policy, PiiClass, PiiHit, PiiScrubResult};
pub use policy::{PiiPolicy, PolicyMap};
