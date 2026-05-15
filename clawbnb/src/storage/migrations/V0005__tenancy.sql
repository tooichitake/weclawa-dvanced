-- v3 M2: multi-tenancy schema 基础设施。
--
-- 当前 v2.2 是单 operator/单 daemon = 一套 schema。v3 SaaS hosted 模式
-- 需要在**同一个 SQLite 文件**里多 tenant 共存（CLI 部署可保持单
-- tenant，hosted 部署可水平切租户）。
--
-- ## 渐进迁移策略
--
-- 所有 user-scoped 表加 `tenant_id TEXT NOT NULL DEFAULT 'default'`。
-- DEFAULT 让现存行无须 backfill —— 所有 v2.2 数据自动归入 'default' 租户。
-- callsite 现在仍用 'default' 兜底，等 v3 auth 层把 AdminContext.tenant_id
-- 传进来后，repo 切换为按变量过滤。
--
-- ## tenants 主表
--
-- 单独建 tenants 表是为了：
-- - 注册 / suspend / 删 tenant 的元数据存放点
-- - billing (v3 M3) 关联 stripe_customer_id
-- - 软删（deleted_at 列）防止误删全租户数据
CREATE TABLE tenants (
  id              TEXT PRIMARY KEY,
  name            TEXT NOT NULL,
  created_at      TEXT NOT NULL,
  status          TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','suspended','deleted')),
  stripe_customer_id TEXT,
  deleted_at      TEXT
);

-- Seed the default tenant for backwards compat. v2.2 数据全部归这里。
INSERT INTO tenants (id, name, created_at, status)
VALUES ('default', 'Default Tenant', datetime('now'), 'active');

-- ## user-scoped 表加 tenant_id 列
--
-- SQLite ALTER TABLE 不支持 ALTER COLUMN，但能 ADD COLUMN with DEFAULT
-- —— 完美匹配渐进迁移：现存行自动拿 'default'，新行随插入。
ALTER TABLE users         ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE user_settings ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE user_history  ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE console_sessions ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE accounts      ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE bindings      ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE admin_keys    ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE audit_log     ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE rate_limits   ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE seen_messages ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE seen_messages ADD COLUMN account_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE sandbox_logs  ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';

-- ## 索引：所有"按租户列表"查询的预备路径
--
-- 当前 callsite 不强求按 tenant_id 过滤（用 'default' 单值），但建好
-- 索引为 v3 切换做准备 —— 切换时只改 SQL，不动 schema。
CREATE INDEX idx_users_tenant         ON users         (tenant_id, last_seen_at DESC);
CREATE INDEX idx_user_history_tenant  ON user_history  (tenant_id, user_hash, created_at DESC);
CREATE INDEX idx_accounts_tenant      ON accounts      (tenant_id);
CREATE INDEX idx_admin_keys_tenant    ON admin_keys    (tenant_id, revoked_at);
CREATE INDEX idx_audit_log_tenant     ON audit_log     (tenant_id, ts DESC);
CREATE INDEX idx_rate_limits_tenant   ON rate_limits   (tenant_id, scope_key);

-- v3-A5: dedup 复合主键考虑
--
-- 当前 seen_messages 主键是 msg_id 单列。多租户后两个不同租户的两个
-- 不同 account 完全有可能 msg_id 撞车（iLink 服务端发的 id 不保证全局
-- 唯一，只保证 per-account 唯一）。新的复合 PK = (tenant_id, account_id, msg_id)
-- 才严谨。
--
-- SQLite 不能改 PRIMARY KEY，得重建表。这里在新列就位后追加索引模拟
-- 唯一约束；v3.1 真切多租户时做正式的"重建表"迁移。
CREATE UNIQUE INDEX idx_seen_messages_composite
  ON seen_messages (tenant_id, account_id, msg_id);
