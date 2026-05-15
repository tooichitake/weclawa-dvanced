-- v5 M1: accounts 表加 platform_id。
--
-- v2-v4 单协议 (iLink) 部署 platform_id 列不存在 ── 整个 accounts 表
-- 默认归 'ilink-wechat'。v5 起 cli/start.rs 按 platform_id dispatch 给
-- 对应 poller：iLink 走 monitor::poller::run_account_monitor (long-poll)，
-- Telegram 走 monitor::telegram_poller::run_telegram_monitor (getUpdates loop)，
-- Discord/Feishu 不 poll（gateway WS / webhook），platform_id 仅做配置
-- 标签 + GUI 显示。
--
-- 取值范围与 `crate::puppet::MessagingPlatform::platform_id` 返回值 1:1：
--   'ilink-wechat' (default), 'telegram', 'discord', 'feishu'
-- 不强 CHECK 让 v6 加新协议时不用配套改 schema。
ALTER TABLE accounts
  ADD COLUMN platform_id TEXT NOT NULL DEFAULT 'ilink-wechat';

CREATE INDEX idx_accounts_platform ON accounts (platform_id);
