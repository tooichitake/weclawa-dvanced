-- v3.1 D3: trust score 持久化。
--
-- `tenancy::trust::TrustInputs` 计算公式 (4 因子加权和) 在 v3 ship 时是
-- 纯 stateless 函数 —— caller 每次 hot-compute。生产 daemon 需要：
--
-- 1. 因子值跨消息累积（success_rate 看最近 100 条；uptime 看 7 天）
-- 2. tier 切换时记 audit_log（合规 + 调试）
-- 3. GUI 看历史趋势
--
-- schema：
-- - `user_trust_inputs(user_hash PK, tenant_id, ...)` 当前快照 (per user)
-- - `user_trust_history(...)` rollup 历史（每次重算追加一行）
--
-- 后台任务 (v3.1 真启动时 spawn) 每 5 分钟跑一次：
--   for each user with activity in last hour:
--     recompute success_rate / uptime / threat / integrity from audit_log
--     UPSERT user_trust_inputs
--     INSERT user_trust_history if tier 变化
--
-- ## 不连级联到 users 表
--
-- 已 deleted user 的 trust 历史保留 90 天供合规审计；reconciler 后续清。
-- 所以 user_trust_inputs / history 表都**没有** ON DELETE CASCADE。

CREATE TABLE user_trust_inputs (
  user_hash        TEXT PRIMARY KEY,
  tenant_id        TEXT NOT NULL DEFAULT 'default',
  success_rate     REAL NOT NULL DEFAULT 0.5,
  uptime           REAL NOT NULL DEFAULT 0.5,
  threat           REAL NOT NULL DEFAULT 0.5,
  integrity        REAL NOT NULL DEFAULT 0.5,
  --Computed score - cached so hot path不重算。后台 task 写。
  score            REAL NOT NULL DEFAULT 0.5,
  --'trusted' / 'standard' / 'restricted' / 'quarantined'
  tier             TEXT NOT NULL DEFAULT 'standard',
  updated_at       TEXT NOT NULL,
  --该用户首次进入当前 tier 的时间 — 用于"连续 trust 多久"展示
  tier_since       TEXT NOT NULL
);

CREATE INDEX idx_user_trust_tenant ON user_trust_inputs (tenant_id, tier);

CREATE TABLE user_trust_history (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  user_hash    TEXT NOT NULL,
  tenant_id    TEXT NOT NULL DEFAULT 'default',
  ts           TEXT NOT NULL,
  --计算时各因子和 score 的快照（JSON）— 便于 v3.2 改公式后回溯
  inputs_json  TEXT NOT NULL,
  score        REAL NOT NULL,
  tier         TEXT NOT NULL,
  --上一 tier；首次记录为 null
  prev_tier    TEXT
);

CREATE INDEX idx_trust_history_user_ts ON user_trust_history (user_hash, ts DESC);
CREATE INDEX idx_trust_history_tenant_tier ON user_trust_history (tenant_id, tier, ts DESC);
