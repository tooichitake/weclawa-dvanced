//! Tenancy primitives — v3 multi-tenant 基础设施。
//!
//! v2.2 是单 operator / 单 tenant 部署：所有数据归"default"租户。v3 hosted
//! 模式让多 tenant 共用同一 daemon binary + DB 文件，本模块定义那层抽象。
//!
//! ## v2.2 → v3 渐进迁移
//!
//! 现在所有 callsite 用 [`TenantId::default_tenant()`] 兜底（"default"
//! 字符串）。v3 auth 层 (`AdminContext.tenant_id`) 上线后，把这个 helper
//! 在 callsite 替换成"从请求上下文取"。schema 已经准备好（V0005 migration
//! 给所有 user-scoped 表加了 `tenant_id` 列）。
//!
//! ## 为什么单独建模块
//!
//! TenantId 用 newtype 而不是裸 `String` 防止把它跟 UserHash / AccountId
//! 等其他 ID 类型混。`ids.rs` 已有 UserHash/WeixinUserId 同样思路，这层
//! 是兄弟模块。
//!
//! ## 测试覆盖目标
//!
//! - newtype 防错（同形参数顺序错传不编译）
//! - default tenant 常量稳定
//! - serde 序列化/反序列化 round-trip（v3 API payload 需要）

pub mod resolver;
pub mod trust;

use serde::{Deserialize, Serialize};
use std::fmt;

/// 默认租户 ID — v2.2 数据全归这里，单 operator 部署也用这个。
pub const DEFAULT_TENANT: &str = "default";

/// Strongly-typed tenant identifier. **必须**通过 `TenantId::new` /
/// `TenantId::default_tenant` 构造，防止 callsite 把 user_hash / account_id
/// 跟 tenant_id 串错。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TenantId(String);

impl TenantId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// `TenantId::default_tenant()` — 单 tenant 模式 / v2.2 兼容兜底。
    pub fn default_tenant() -> Self {
        Self(DEFAULT_TENANT.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 是否为 default tenant —— 用于 v2 模式短路（不需要查 tenants 表）。
    pub fn is_default(&self) -> bool {
        self.0 == DEFAULT_TENANT
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TenantId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for TenantId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Tenant 状态（DB schema `tenants.status` 列对应 enum）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TenantStatus {
    Active,
    Suspended,
    Deleted,
}

impl TenantStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Deleted => "deleted",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "suspended" => Some(Self::Suspended),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tenant_stable() {
        assert_eq!(TenantId::default_tenant().as_str(), "default");
        assert!(TenantId::default_tenant().is_default());
    }

    #[test]
    fn newtype_distinguishes_from_raw_string() {
        let t: TenantId = "acme".into();
        assert_eq!(t.as_str(), "acme");
        assert!(!t.is_default());
    }

    #[test]
    fn serde_roundtrip_is_transparent() {
        let t = TenantId::new("acme-corp");
        let json = serde_json::to_string(&t).unwrap();
        // transparent serialization = bare string "acme-corp"
        assert_eq!(json, "\"acme-corp\"");
        let back: TenantId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn status_strings_match_schema_check() {
        // SQL 里的 CHECK 约束: status IN ('active','suspended','deleted')
        for s in ["active", "suspended", "deleted"] {
            let parsed = TenantStatus::from_str(s).unwrap();
            assert_eq!(parsed.as_str(), s);
        }
        assert!(TenantStatus::from_str("bogus").is_none());
    }
}
