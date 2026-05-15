//! Tool policy — v3 per-user tool allowlist enforcement.
//!
//! ## 防御层级
//!
//! 当前 (v2.2) tool gating 只通过 sandbox 内 `~/.claude/settings.json` 的
//! `allowedTools` / `disallowedTools` 数组。这是**软**约束 —— 如果
//! claude-cli 内部解析失败 / 用户 prompt 让 AI 改 settings.json，gating 就
//! 失效。
//!
//! v3 加**硬**约束：从 user_settings 读 policy，**作为 CLI 参数**
//! `--allowedTools <list>` / `--disallowedTools <list>` 传给 claude-cli。
//! 这个参数级是 claude-cli 加载 settings.json **之后**才被解析的，会
//! 覆盖任何 settings.json 内值。
//!
//! ## 为何不只靠 podman --cap-drop
//!
//! sandbox capability 控制 OS 层（read/write fs, network），但 claude 工具
//! 集（Bash / Write / Read / WebSearch / mcp_*）是**应用层**抽象，跟 OS
//! cap 不一一对应。所以工具粒度的 gating 必须走 claude-cli 自己的 flag。
//!
//! podman --cap-drop 仍然存在 —— 作为最外层 cap 收敛（v2 已实施）。

use serde_json::Value;

/// Tool allow/disallow policy from user_settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPolicy {
    /// Whitelist：非空时只允许这些工具。空 = 不限制。
    pub allowed: Vec<String>,
    /// Blacklist：任何时候都禁止这些工具。优先级高于 allowed。
    pub disallowed: Vec<String>,
}

impl ToolPolicy {
    /// 从 user_settings JSON 抽取 policy。容错：字段缺失 / 类型错都默认空。
    pub fn from_settings(settings_json: &Value) -> Self {
        let allowed = extract_string_array(settings_json, "allowedTools");
        let disallowed = extract_string_array(settings_json, "disallowedTools");
        Self {
            allowed,
            disallowed,
        }
    }

    /// 把 policy 转成 claude-cli args（追加进 args 末尾）。
    /// claude-cli 接受 comma-separated 列表 —— "Bash,Write,WebSearch"。
    pub fn to_cli_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.allowed.is_empty() {
            out.push("--allowedTools".into());
            out.push(self.allowed.join(","));
        }
        if !self.disallowed.is_empty() {
            out.push("--disallowedTools".into());
            out.push(self.disallowed.join(","));
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty() && self.disallowed.is_empty()
    }

    /// v5.5: tier-based **hardening overlay**.
    ///
    /// For low-trust tiers (`TrustTier::Quarantined`, `Restricted`), forcibly
    /// add high-risk tools to the disallowed list regardless of what the
    /// user_settings file says. operator-side tightening: a user who has
    /// burned through their trust budget can't reach Bash / Write /
    /// WebFetch via prompt-injection even if their per-user settings
    /// look permissive.
    ///
    /// `Standard` and `Trusted` tiers pass through unmodified.
    ///
    /// Returns a new ToolPolicy; original (`self`) unchanged.
    pub fn with_tier_overlay(
        mut self,
        tier: crate::tenancy::trust::TrustTier,
    ) -> Self {
        use crate::tenancy::trust::TrustTier;
        let force_disallow: &[&str] = match tier {
            TrustTier::Quarantined => &[
                "Bash",
                "Write",
                "Edit",
                "WebFetch",
                "WebSearch",
                "NotebookEdit",
            ],
            TrustTier::Restricted => &["Bash", "Write", "Edit"],
            // Standard and Trusted: no extra restrictions.
            TrustTier::Standard | TrustTier::Trusted => &[],
        };
        for tool in force_disallow {
            let already_disallowed = self.disallowed.iter().any(|s| s == tool);
            if !already_disallowed {
                self.disallowed.push((*tool).to_string());
            }
            // Also remove from allowed (if explicitly allowed) so the
            // disallowed list takes priority unambiguously.
            self.allowed.retain(|s| s != tool);
        }
        self
    }

    /// v5.5 helper: build the effective policy for a user by combining
    /// (a) their per-user settings.json allowedTools/disallowedTools, and
    /// (b) the dynamic tier overlay from `user_trust_inputs` table.
    ///
    /// Fail-open: if the trust repo isn't reachable, return the bare
    /// per-user settings policy (no tier hardening). Operators see a
    /// `weclawbot_tool_policy_trust_lookup_failed_total` metric tick.
    pub async fn for_user(
        user_hash: &str,
        settings_json: &Value,
    ) -> Self {
        let base = Self::from_settings(settings_json);
        let tier = match crate::storage::db_async::try_global_async_pool() {
            Some(pool) => {
                let repo = crate::repo::trust_async::SqlxTrustRepo::new(pool);
                match repo.get(user_hash).await {
                    Ok(Some(snap)) => snap.tier,
                    Ok(None) => crate::tenancy::trust::TrustTier::Standard,
                    Err(e) => {
                        metrics::counter!(
                            "weclawbot_tool_policy_trust_lookup_failed_total"
                        )
                        .increment(1);
                        tracing::debug!(
                            "tool_policy: trust lookup for {user_hash} failed: {e} — \
                             using Standard tier (fail-open)"
                        );
                        crate::tenancy::trust::TrustTier::Standard
                    }
                }
            }
            None => crate::tenancy::trust::TrustTier::Standard,
        };
        base.with_tier_overlay(tier)
    }
}

fn extract_string_array(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_settings_yields_empty_policy() {
        let p = ToolPolicy::from_settings(&json!({}));
        assert!(p.is_empty());
        assert!(p.to_cli_args().is_empty());
    }

    #[test]
    fn extracts_allowed_and_disallowed() {
        let p = ToolPolicy::from_settings(&json!({
            "allowedTools": ["Bash", "Read"],
            "disallowedTools": ["WebSearch"]
        }));
        assert_eq!(p.allowed, vec!["Bash", "Read"]);
        assert_eq!(p.disallowed, vec!["WebSearch"]);
    }

    #[test]
    fn cli_args_join_with_comma() {
        let p = ToolPolicy {
            allowed: vec!["Bash".into(), "Read".into()],
            disallowed: vec!["WebSearch".into()],
        };
        let args = p.to_cli_args();
        assert_eq!(args, vec![
            "--allowedTools".to_string(),
            "Bash,Read".to_string(),
            "--disallowedTools".to_string(),
            "WebSearch".to_string(),
        ]);
    }

    #[test]
    fn ignores_non_string_entries() {
        let p = ToolPolicy::from_settings(&json!({
            "allowedTools": ["Bash", 42, null, "Read"]
        }));
        assert_eq!(p.allowed, vec!["Bash", "Read"]);
    }

    #[test]
    fn ignores_wrong_type() {
        let p = ToolPolicy::from_settings(&json!({
            "allowedTools": "not-an-array"
        }));
        assert!(p.allowed.is_empty());
    }

    #[test]
    fn tier_overlay_untrusted_adds_bash_write_etc() {
        use crate::tenancy::trust::TrustTier;
        let p = ToolPolicy::default().with_tier_overlay(TrustTier::Quarantined);
        for must_block in &["Bash", "Write", "Edit", "WebFetch", "WebSearch"] {
            assert!(
                p.disallowed.iter().any(|s| s == must_block),
                "Untrusted tier should forcibly disallow {must_block}"
            );
        }
    }

    #[test]
    fn tier_overlay_restricted_lighter_set() {
        use crate::tenancy::trust::TrustTier;
        let p = ToolPolicy::default().with_tier_overlay(TrustTier::Restricted);
        assert!(p.disallowed.iter().any(|s| s == "Bash"));
        assert!(p.disallowed.iter().any(|s| s == "Write"));
        // Restricted still allows web tools
        assert!(!p.disallowed.iter().any(|s| s == "WebFetch"));
    }

    #[test]
    fn tier_overlay_standard_passes_through() {
        use crate::tenancy::trust::TrustTier;
        let original = ToolPolicy {
            allowed: vec!["Bash".into()],
            disallowed: vec![],
        };
        let p = original.clone().with_tier_overlay(TrustTier::Standard);
        assert_eq!(p, original);
    }

    #[test]
    fn tier_overlay_removes_explicitly_allowed_dangerous_tool() {
        use crate::tenancy::trust::TrustTier;
        // User settings explicitly allow Bash; tier overlay should
        // strip it from `allowed` AND add to `disallowed`.
        let original = ToolPolicy {
            allowed: vec!["Bash".into(), "Read".into()],
            disallowed: vec![],
        };
        let p = original.with_tier_overlay(TrustTier::Restricted);
        assert!(!p.allowed.iter().any(|s| s == "Bash"));
        assert!(p.allowed.iter().any(|s| s == "Read"));
        assert!(p.disallowed.iter().any(|s| s == "Bash"));
    }

    #[test]
    fn tier_overlay_idempotent() {
        use crate::tenancy::trust::TrustTier;
        let p1 = ToolPolicy::default().with_tier_overlay(TrustTier::Restricted);
        let p2 = p1.clone().with_tier_overlay(TrustTier::Restricted);
        assert_eq!(p1, p2, "applying same tier twice should be a no-op");
    }
}
