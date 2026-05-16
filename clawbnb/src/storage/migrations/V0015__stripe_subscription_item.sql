-- v7.7 — tenants.stripe_subscription_item_id for usage-records billing.
--
-- A Stripe Subscription holds N SubscriptionItems, one per metered
-- price. weclawbot bills per-inbound (price A) + per-sandbox-second
-- (price B) + per-ai-token (price C), so each tenant's Subscription
-- has up to 3 items. The pusher needs to know WHICH item id maps to
-- WHICH counter when posting Usage Records.
--
-- We store JSON instead of 3 separate columns: schema flex for
-- adding/removing price tiers without migrations. Shape:
--   {
--     "inbound":         "si_xxxxxxxxxxxxxx",
--     "sandbox_seconds": "si_yyyyyyyyyyyyyy",
--     "ai_tokens_input": "si_zzzzzzzzzzzzzz",
--     "ai_tokens_output":"si_wwwwwwwwwwwwww"
--   }
--
-- Missing keys = that metric isn't billed for this tenant (e.g.
-- token-pass-through tenants don't get a sandbox_seconds charge).
-- Pusher silently skips counters without a mapped item.
--
-- All keys optional. NULL column → tenant has no Stripe metering
-- (operator may bill them off-platform / not be billing them at all).

ALTER TABLE tenants
  ADD COLUMN stripe_subscription_items_json JSONB;
