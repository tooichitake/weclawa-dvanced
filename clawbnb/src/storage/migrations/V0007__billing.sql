-- v3.4 G2: tenants 表加 billing 相关列。
--
-- Stripe webhook 收到 invoice.paid / payment_failed / subscription.deleted
-- 时更新 tenants.billing_status。poller / handler 启动前查 status —
-- 'suspended' 直接 reject 处理（用户消息看到"账户暂停"提示）。

ALTER TABLE tenants ADD COLUMN billing_status TEXT NOT NULL DEFAULT 'active'
  CHECK (billing_status IN ('active', 'pending', 'suspended', 'unknown'));
ALTER TABLE tenants ADD COLUMN billing_period_end TEXT;
ALTER TABLE tenants ADD COLUMN last_billing_event TEXT;
ALTER TABLE tenants ADD COLUMN last_billing_event_at TEXT;

-- v3.5 dual-index: 'active'/'pending' lookup hot path
CREATE INDEX idx_tenants_billing_status ON tenants (billing_status);

-- Stripe customer ID 在 V0005 init 时已经有列；本期不重复加。
