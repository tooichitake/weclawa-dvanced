-- v7.2 — per-tenant SSO IdP config.
--
-- 每个 tenant 一份 OIDC 和/或 SAML 配置。NULL 表示"该 tenant 没启用
-- 该协议的 SSO"，登录走 admin_key bearer。两列同时存在 = operator 给
-- 用户两种登录方式选。
--
-- JSONB shape 由 `crate::ee::oidc::OidcConfig` / `crate::ee::saml::SamlConfig`
-- 决定 (serde 透传)。pure-app 层校验，DB 不做结构 CHECK ── operator 后
-- 续给配置新增字段不需要 schema 迁移。
--
-- 读写路径：
--   - GET  /api/v1/admin/tenants/{id}/sso  (super_admin) → 返两个 config
--   - PUT  /api/v1/admin/tenants/{id}/sso  (super_admin) → 写
--   - SSO init/callback handler 走 `repo::tenants_async::get_sso_config(tenant)`
--     即时拉，命中 ArcSwap 缓存 (后续优化)。
--
-- 不加 partial index ── 这俩列用于"按 tenant id 查"，主键 id 上的索引
-- 已经覆盖。SSO 登录流量是低频事件 (人工登录)，不上 cache 也 OK。

ALTER TABLE tenants
  ADD COLUMN oidc_config_json JSONB,
  ADD COLUMN saml_config_json JSONB;
