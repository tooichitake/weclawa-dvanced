-- weclawbot v2 initial schema.
--
-- All persistent state lives in `~/.weclawbot/state.db`. WAL journaling
-- + NORMAL synchronous strikes a balance between durability and write
-- latency that suits a single-host workload.
--
-- Notes on a few non-obvious choices:
--
-- * Timestamps are stored as RFC3339 TEXT (`2026-05-13T16:48:24Z`). SQLite
--   sorts them lexicographically in the right order, and human inspection
--   of the DB file (sqlite3 cli, DB Browser) is much easier than with
--   Unix epoch integers.
--
-- * `accounts.token_*` are placeholders for AES-256-GCM ciphertext +
--   nonce. Phase 1 stores them with an empty / well-known key so we can
--   ship the schema before the key-management work in Phase 6 lands.
--
-- * `admin_keys` and `audit_log` already exist in V0001 so we don't have
--   to bump schema on every later phase. They get populated by Phase 3.
--
-- * `rate_limits` is here for Phase 5. Same reason — schema first.

CREATE TABLE config_kv (
  key         TEXT PRIMARY KEY,
  value_json  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);

CREATE TABLE defaults (
  id            INTEGER PRIMARY KEY CHECK (id = 1),
  settings_json TEXT NOT NULL,
  updated_at    TEXT NOT NULL
);

CREATE TABLE accounts (
  account_id        TEXT PRIMARY KEY,
  -- AES-256-GCM ciphertext (Phase 6); plaintext UTF-8 for now (Phase 1 ships
  -- the schema; encryption layer lands in Phase 6 with a separate
  -- token_ciphertext_v2 column + one-time migration).
  token             TEXT,
  base_url          TEXT NOT NULL,
  weixin_user_id    TEXT,
  saved_at          TEXT NOT NULL
);

CREATE TABLE users (
  hash              TEXT PRIMARY KEY,
  user_id_hint      TEXT,
  created_at        TEXT NOT NULL,
  last_seen_at      TEXT,
  message_count     BIGINT NOT NULL DEFAULT 0,
  sync_state        TEXT NOT NULL DEFAULT 'unknown',
  last_sync_at      TEXT,
  last_sync_error   TEXT
);

CREATE TABLE user_settings (
  user_hash      TEXT PRIMARY KEY REFERENCES users(hash) ON DELETE CASCADE,
  settings_json  TEXT NOT NULL,
  updated_at     TEXT NOT NULL
);

CREATE TABLE user_history (
  id           BIGSERIAL PRIMARY KEY,
  user_hash    TEXT NOT NULL REFERENCES users(hash) ON DELETE CASCADE,
  role         TEXT NOT NULL CHECK (role IN ('user','assistant')),
  content      TEXT NOT NULL,
  created_at   TEXT NOT NULL
);
CREATE INDEX idx_history_user_time ON user_history (user_hash, created_at);

CREATE TABLE console_sessions (
  user_hash      TEXT PRIMARY KEY REFERENCES users(hash) ON DELETE CASCADE,
  in_menu        BIGINT NOT NULL,
  current_path   TEXT NOT NULL DEFAULT '[]',
  last_input_at  TEXT NOT NULL
);

CREATE TABLE bindings (
  weixin_user_id     TEXT PRIMARY KEY,
  active_account_id  TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
  agent_id           TEXT NOT NULL,
  updated_at         TEXT NOT NULL
);

CREATE TABLE admin_keys (
  id            TEXT PRIMARY KEY,         -- uuid v4
  name          TEXT NOT NULL,
  key_hash      TEXT NOT NULL,            -- argon2id encoded string
  role          TEXT NOT NULL CHECK (role IN ('super_admin','read_write','read_only')),
  created_at    TEXT NOT NULL,
  last_used_at  TEXT,
  revoked_at    TEXT
);
CREATE INDEX idx_admin_keys_active ON admin_keys (revoked_at) WHERE revoked_at IS NULL;

CREATE TABLE audit_log (
  id            BIGSERIAL PRIMARY KEY,
  ts            TEXT NOT NULL,
  actor_key_id  TEXT,                     -- nullable for 'system' actor
  action        TEXT NOT NULL,
  target        TEXT,
  before_json   TEXT,
  after_json    TEXT,
  ip            TEXT
);
CREATE INDEX idx_audit_ts ON audit_log (ts DESC);

CREATE TABLE rate_limits (
  scope_key       TEXT NOT NULL,
  window_start_ts TEXT NOT NULL,
  count           BIGINT NOT NULL DEFAULT 0,
  PRIMARY KEY (scope_key, window_start_ts)
);
