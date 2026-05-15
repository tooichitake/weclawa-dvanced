-- v3.6 I6: billing periods + grace window 完整化。
--
-- V0007 加了 billing_status / period_end / last_event。本期补：
--
-- - billing_period_start: 当前计费周期起始（Stripe invoice.period_start）。
-- - grace_until: pending 状态宽限期截止（默认 invoice.payment_failed +7d；
--   Stripe Smart Retries 通常 4 次重试在 1 周内跑完）。
--
-- Stripe webhook 在 invoice.payment_failed 时：
--   billing_status='pending', grace_until = now() + 7 days
-- Stripe webhook 在 invoice.paid 时：
--   billing_status='active', grace_until = NULL, period_start = invoice.period_start, period_end = invoice.period_end
--
-- handler.rs 调 is_active 时检查：
--   status='active' AND billing_status='active' → 完全放行
--   billing_status='pending' AND now() < grace_until → 允许处理（grace 期间）
--   billing_status='pending' AND now() >= grace_until → 拒绝（grace 已过）
--   billing_status='suspended' → 拒绝

ALTER TABLE tenants ADD COLUMN billing_period_start TEXT;
ALTER TABLE tenants ADD COLUMN grace_until TEXT;

-- 配合 grace 检查的 index
CREATE INDEX idx_tenants_grace ON tenants (billing_status, grace_until);
