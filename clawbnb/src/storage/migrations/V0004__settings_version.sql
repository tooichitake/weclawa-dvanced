-- v2.2 L3.2/L4.1: settings 版本号 + sandbox 状态镜像。
--
-- 问题：user_settings 在 DB 改了之后，sandbox 文件系统里的 ~/.claude/
-- settings.json 是单独写盘的镜像。如果两边同时写（GUI 改一次，wechat
-- menu 同时改一次），后写的覆盖先写的而没 conflict detection。
--
-- 修复：`version` 列 monotonic 递增。`sandbox::materialize::sync` 在
-- 把 DB → fs 时检查 file 的 version stamp，只在 DB.version > file.version
-- 才覆盖。两边并发改 → 后一次拿不到最新 version → conflict 报错（操作员
-- 重试拿到新版本即可）。
--
-- 默认值 0 让现有行无须特殊处理升级。所有新写入操作必须 SET version =
-- (SELECT IFNULL(MAX(version),0)+1 FROM ...) 才能保证递增。
-- v2.2 暂时只加列；callsite enforcement 在 repo::settings 增量补。

ALTER TABLE user_settings ADD COLUMN version INTEGER NOT NULL DEFAULT 0;

-- v2.2 L4.1: 容器日志持久化。
--
-- 当前 per-message 模式 stdout/stderr 通过 stream-json 解析后丢弃。
-- 容器 panic / SIGKILL / OOM 时 stderr 的 "last words"（已经在 v2.1
-- handler.rs 通过 4KB buffer 捕获）只活在 daemon log，没结构化检索。
--
-- L4.1 ACP 上线后，long-running container 跨多条消息执行，stdout/stderr
-- 必须持久化才能调试。schema 现在就建好，reconciler 收 orphan 容器时
-- 可以同步 cleanup 老日志。
CREATE TABLE sandbox_logs (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  user_hash       TEXT NOT NULL,
  container_id    TEXT,
  ts              TEXT NOT NULL,
  stream          TEXT NOT NULL CHECK (stream IN ('stdout','stderr','event')),
  line            TEXT NOT NULL
);
CREATE INDEX idx_sandbox_logs_user_ts ON sandbox_logs (user_hash, ts DESC);
CREATE INDEX idx_sandbox_logs_container ON sandbox_logs (container_id) WHERE container_id IS NOT NULL;
