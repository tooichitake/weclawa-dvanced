//! PII policy types — what to do with each detected class.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::detector::PiiClass;

/// Action to take for a detected PII class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PiiPolicy {
    /// Leave the text unchanged.
    Pass,
    /// Replace match with `[<CLASS>]` placeholder.
    Redact,
    /// Replace with `[<CLASS>:<sha256-12hex>]` — correlates across log
    /// entries without revealing the source value.
    Hash,
    /// Reject the operation entirely. Caller decides the user-facing
    /// response (4xx, refuse-to-relay, audit-only event).
    Block,
}

impl PiiPolicy {
    /// Compiled-in defaults per class (table from module docstring).
    pub fn default_for(class: PiiClass) -> Self {
        match class {
            PiiClass::MobilePhoneCn => Self::Redact,
            PiiClass::MobilePhoneIntl => Self::Redact,
            PiiClass::IdCardCn => Self::Block,
            PiiClass::BankCardCn => Self::Redact,
            PiiClass::Email => Self::Redact,
            PiiClass::IpAddr => Self::Redact,
            PiiClass::Url => Self::Pass,
            PiiClass::LicensePlateCn => Self::Redact,
            PiiClass::AddressCn => Self::Hash,
            PiiClass::PersonNameCn => Self::Hash,
            PiiClass::QqNumber => Self::Redact,
            PiiClass::WechatId => Self::Redact,
            PiiClass::Passport => Self::Block,
            PiiClass::Imei => Self::Block,
        }
    }
}

/// Per-class policy override map. Empty = all classes use defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PolicyMap(pub HashMap<PiiClass, PiiPolicy>);

impl PolicyMap {
    pub fn policy_for(&self, class: PiiClass) -> PiiPolicy {
        self.0
            .get(&class)
            .copied()
            .unwrap_or_else(|| PiiPolicy::default_for(class))
    }

    /// "Strict" preset — every class BLOCK or REDACT. For HIPAA/SOC2
    /// outbound (AI prompt, webhook) where any leak is unacceptable.
    pub fn strict() -> Self {
        let mut m = HashMap::new();
        for &class in PiiClass::all() {
            let p = match class {
                PiiClass::IdCardCn
                | PiiClass::Passport
                | PiiClass::Imei
                | PiiClass::BankCardCn => PiiPolicy::Block,
                _ => PiiPolicy::Redact,
            };
            m.insert(class, p);
        }
        Self(m)
    }

    /// "Audit" preset — REDACT all (audit log shouldn't BLOCK because it's
    /// read-only side-effect; we just don't want PII written to disk).
    pub fn audit() -> Self {
        let mut m = HashMap::new();
        for &class in PiiClass::all() {
            m.insert(class, PiiPolicy::Redact);
        }
        Self(m)
    }

    /// "Permissive" preset — all PASS. For dev / single-user deployments
    /// where the operator is the user.
    pub fn permissive() -> Self {
        let mut m = HashMap::new();
        for &class in PiiClass::all() {
            m.insert(class, PiiPolicy::Pass);
        }
        Self(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_module_doc() {
        assert_eq!(PiiPolicy::default_for(PiiClass::IdCardCn), PiiPolicy::Block);
        assert_eq!(PiiPolicy::default_for(PiiClass::Url), PiiPolicy::Pass);
        assert_eq!(
            PiiPolicy::default_for(PiiClass::AddressCn),
            PiiPolicy::Hash
        );
    }

    #[test]
    fn strict_blocks_high_risk() {
        let s = PolicyMap::strict();
        assert_eq!(s.policy_for(PiiClass::IdCardCn), PiiPolicy::Block);
        assert_eq!(s.policy_for(PiiClass::Passport), PiiPolicy::Block);
        assert_eq!(s.policy_for(PiiClass::Email), PiiPolicy::Redact);
        assert_ne!(s.policy_for(PiiClass::Url), PiiPolicy::Pass);
    }

    #[test]
    fn permissive_passes_everything() {
        let p = PolicyMap::permissive();
        for &class in PiiClass::all() {
            assert_eq!(p.policy_for(class), PiiPolicy::Pass);
        }
    }

    #[test]
    fn empty_map_uses_defaults() {
        let m = PolicyMap::default();
        assert_eq!(m.policy_for(PiiClass::IdCardCn), PiiPolicy::Block);
        assert_eq!(m.policy_for(PiiClass::Email), PiiPolicy::Redact);
    }
}
