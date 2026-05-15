//! Tenant resolution helpers — 共用查询 + active check。v3.6 I1 清理。
//!
//! 之前 `monitor::handler` 和 `puppet::telegram_handler` 各自 copy 了
//! `resolve_tenant_for_account` + `tenant_is_active` —— 任何新协议
//! (Discord / Feishu) 接进来都会再 copy 一份。抽到这里：每协议直接调
//! 同样两个函数，DB IO + fallback 逻辑只有一处。
//!
//! ## 失败兜底语义
//!
//! 单 operator 部署 / 早期 boot / DB 临时不可用 → 返 `default` tenant +
//! active=true。这维持 v2.2 单租户行为 —— hosted SaaS 部署只要 tenants
//! 表 seed 完整 + accounts 行有 tenant_id，就走真路径；本兜底只在边缘
//! 情况下生效，且会上报 metric 让 ops 看见。

use crate::ids::AccountId;
use crate::repo::accounts_async::SqlxAccountRepo;
use crate::repo::tenants_async::SqlxTenantRepo;
use crate::runtime::blocking::block_on_async;
use crate::tenancy::TenantId;

/// 根据 account_id 查它归属的 tenant。
pub fn resolve_tenant_for_account(account_id: &str) -> TenantId {
    block_on_async(async {
        let Some(apool) = crate::storage::db_async::try_global_async_pool() else {
            return TenantId::default_tenant();
        };
        let repo = SqlxAccountRepo::new(apool);
        let aid = AccountId::new(account_id);
        match repo.get_tenant_id(&aid).await {
            Ok(Some(t)) => TenantId::new(t),
            Ok(None) => {
                metrics::counter!(
                    "weclawbot_tenant_lookup_fallback_total",
                    "reason" => "account_not_found"
                )
                .increment(1);
                TenantId::default_tenant()
            }
            Err(e) => {
                tracing::warn!("resolve_tenant_for_account({account_id}): {e}");
                metrics::counter!(
                    "weclawbot_tenant_lookup_fallback_total",
                    "reason" => "db_error"
                )
                .increment(1);
                TenantId::default_tenant()
            }
        }
    })
}

/// 校验 tenant 当前是否允许处理 inbound (sync 入口)。
pub fn tenant_is_active(tenant_id: &TenantId) -> bool {
    block_on_async(async {
        let Some(apool) = crate::storage::db_async::try_global_async_pool() else {
            return true;
        };
        let repo = SqlxTenantRepo::new(apool);
        repo.is_active(tenant_id).await.unwrap_or(true)
    })
}

/// v4 J5: async 版本（直接 sqlx pool）。
pub async fn tenant_is_active_async(tenant_id: &TenantId) -> bool {
    let Some(pool) = crate::storage::db_async::try_global_async_pool() else {
        return true;
    };
    let repo = SqlxTenantRepo::new(pool);
    repo.is_active(tenant_id).await.unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_unknown_account_falls_back_to_default() {
        // 没 DB pool 注册 → 直接返 default。
        let t = resolve_tenant_for_account("acct-never-existed");
        assert!(t.is_default());
    }

    #[test]
    fn is_active_returns_true_when_pool_unavailable() {
        // 同上，pool 不可用时放行。
        assert!(tenant_is_active(&TenantId::default_tenant()));
    }
}
