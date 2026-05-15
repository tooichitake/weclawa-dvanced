//! Integration tests — cross-module wiring (`ComplianceConfig` + `pii::scrub_with_policy`).
//!
//! These tests live inside the binary crate (no `lib.rs` target) so they
//! exercise the same code path the daemon does. Run with:
//!
//! ```
//! cargo test --release --bin weclawbot -- --test-threads=1 pii::integration
//! ```

#![cfg(test)]

use crate::config::{ComplianceConfig, ComplianceMode};
use crate::pii::{scrub_with_policy, PiiClass, PiiPolicy, PolicyMap};

#[test]
fn permissive_mode_passes_everything() {
    let cfg = ComplianceConfig {
        mode: ComplianceMode::Permissive,
        ..Default::default()
    };
    let text = "phone 13812345678 email alice@example.com id 11010519491231002X";
    let r = scrub_with_policy(text, &cfg.webhook_policy());
    assert!(!r.blocked);
    assert_eq!(r.scrubbed, text);
}

#[test]
fn standard_mode_redacts_webhook() {
    let cfg = ComplianceConfig {
        mode: ComplianceMode::Standard,
        ..Default::default()
    };
    let r = scrub_with_policy("send me 13812345678", &cfg.webhook_policy());
    assert!(r.scrubbed.contains("[MOBILE_CN]"));
}

#[test]
fn strict_mode_blocks_id_card_on_webhook() {
    let cfg = ComplianceConfig {
        mode: ComplianceMode::Strict,
        ..Default::default()
    };
    let r = scrub_with_policy("身份证 11010519491231002X", &cfg.webhook_policy());
    assert!(r.blocked);
}

#[test]
fn hipaa_policy_equivalent_to_strict_for_pii() {
    let strict = ComplianceConfig {
        mode: ComplianceMode::Strict,
        ..Default::default()
    };
    let hipaa = ComplianceConfig {
        mode: ComplianceMode::Hipaa,
        ..Default::default()
    };
    let text = "patient 13812345678 ID 11010519491231002X";
    let r_strict = scrub_with_policy(text, &strict.webhook_policy());
    let r_hipaa = scrub_with_policy(text, &hipaa.webhook_policy());
    assert_eq!(r_strict.blocked, r_hipaa.blocked);
    assert_eq!(r_strict.scrubbed, r_hipaa.scrubbed);
}

#[test]
fn audit_policy_redacts_never_blocks() {
    // Audit log is side-effect AFTER action completed. Block doesn't
    // make sense — degrade to Redact across all modes.
    for mode in [
        ComplianceMode::Strict,
        ComplianceMode::Hipaa,
        ComplianceMode::Soc2,
        ComplianceMode::Gdpr,
    ] {
        let cfg = ComplianceConfig {
            mode,
            ..Default::default()
        };
        let r = scrub_with_policy("admin saw 11010519491231002X", &cfg.audit_policy());
        assert!(!r.blocked, "audit must not block in mode {mode:?}");
        assert!(r.scrubbed.contains("[ID_CARD_CN]"));
    }
}

#[test]
fn ai_prompt_policy_passes_in_permissive_and_standard() {
    for mode in [ComplianceMode::Permissive, ComplianceMode::Standard] {
        let cfg = ComplianceConfig {
            mode,
            ..Default::default()
        };
        let r = scrub_with_policy("user: 13812345678", &cfg.ai_prompt_policy());
        assert!(!r.blocked);
        if mode == ComplianceMode::Permissive {
            assert!(r.scrubbed.contains("13812345678"));
        }
    }
}

#[test]
fn override_relaxes_strict_for_one_class() {
    let mut overrides = PolicyMap::default();
    overrides.0.insert(PiiClass::Email, PiiPolicy::Pass);
    let cfg = ComplianceConfig {
        mode: ComplianceMode::Strict,
        pii_webhook_overrides: overrides,
        ..Default::default()
    };
    let r = scrub_with_policy("alice@example.com", &cfg.webhook_policy());
    assert!(!r.blocked);
    assert_eq!(r.scrubbed, "alice@example.com");
}

#[test]
fn multiple_classes_in_same_text() {
    let cfg = ComplianceConfig {
        mode: ComplianceMode::Standard,
        ..Default::default()
    };
    let text = "联系 13812345678 邮箱 bob@x.com IP 192.168.1.5";
    let r = scrub_with_policy(text, &cfg.webhook_policy());
    assert!(r.scrubbed.contains("[MOBILE_CN]"));
    assert!(r.scrubbed.contains("[EMAIL]"));
    assert!(r.scrubbed.contains("[IP]"));
    assert_eq!(r.hits.len(), 3);
}

#[test]
fn empty_text() {
    let cfg = ComplianceConfig::default();
    let r = scrub_with_policy("", &cfg.webhook_policy());
    assert!(!r.blocked);
    assert!(r.hits.is_empty());
}

#[test]
fn tier_overlay_combines_with_user_settings() {
    use crate::ai::tool_policy::ToolPolicy;
    use crate::tenancy::trust::TrustTier;
    use serde_json::json;

    // user explicitly allowed Bash; Quarantined tier should yank it.
    let settings = json!({
        "allowedTools": ["Bash", "Read"],
        "disallowedTools": []
    });
    let p = ToolPolicy::from_settings(&settings).with_tier_overlay(TrustTier::Quarantined);
    assert!(!p.allowed.iter().any(|t| t == "Bash"));
    assert!(p.disallowed.iter().any(|t| t == "Bash"));
    // Read survives — not in the dangerous list.
    assert!(p.allowed.iter().any(|t| t == "Read"));
}
