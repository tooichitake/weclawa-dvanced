-- v7.0: Convert all TEXT-stored RFC3339 timestamps to PostgreSQL TIMESTAMPTZ.
--
-- Benefits:
--   - 8 bytes vs ~25 chars storage
--   - Native sort/compare without lexicographic-vs-temporal traps
--   - Functions: `now() - ts`, `age()`, `date_trunc()` work directly
--   - Type-safe at the schema level (PG rejects malformed input)
--
-- All existing TEXT values were written via `chrono::Utc::now().to_rfc3339()`
-- producing strings like `2026-05-15T22:30:17.025997Z` which PG parses
-- via `::TIMESTAMPTZ` cast losslessly.
--
-- Columns ordered by table for easier review.

-- config_kv
ALTER TABLE config_kv
  ALTER COLUMN updated_at TYPE TIMESTAMPTZ USING updated_at::TIMESTAMPTZ;

-- defaults
ALTER TABLE defaults
  ALTER COLUMN updated_at TYPE TIMESTAMPTZ USING updated_at::TIMESTAMPTZ;

-- accounts
ALTER TABLE accounts
  ALTER COLUMN saved_at TYPE TIMESTAMPTZ USING saved_at::TIMESTAMPTZ;

-- users
ALTER TABLE users
  ALTER COLUMN created_at   TYPE TIMESTAMPTZ USING created_at::TIMESTAMPTZ;
ALTER TABLE users
  ALTER COLUMN last_seen_at TYPE TIMESTAMPTZ USING last_seen_at::TIMESTAMPTZ;
ALTER TABLE users
  ALTER COLUMN last_sync_at TYPE TIMESTAMPTZ USING last_sync_at::TIMESTAMPTZ;

-- user_settings
ALTER TABLE user_settings
  ALTER COLUMN updated_at TYPE TIMESTAMPTZ USING updated_at::TIMESTAMPTZ;

-- user_history
ALTER TABLE user_history
  ALTER COLUMN created_at TYPE TIMESTAMPTZ USING created_at::TIMESTAMPTZ;

-- console_sessions
ALTER TABLE console_sessions
  ALTER COLUMN last_input_at TYPE TIMESTAMPTZ USING last_input_at::TIMESTAMPTZ;

-- bindings
ALTER TABLE bindings
  ALTER COLUMN updated_at TYPE TIMESTAMPTZ USING updated_at::TIMESTAMPTZ;

-- admin_keys
ALTER TABLE admin_keys
  ALTER COLUMN created_at   TYPE TIMESTAMPTZ USING created_at::TIMESTAMPTZ;
ALTER TABLE admin_keys
  ALTER COLUMN last_used_at TYPE TIMESTAMPTZ USING last_used_at::TIMESTAMPTZ;
ALTER TABLE admin_keys
  ALTER COLUMN revoked_at   TYPE TIMESTAMPTZ USING revoked_at::TIMESTAMPTZ;

-- audit_log
ALTER TABLE audit_log
  ALTER COLUMN ts TYPE TIMESTAMPTZ USING ts::TIMESTAMPTZ;

-- rate_limits — `window_start_ts` is RFC3339 in code, hence TIMESTAMPTZ.
ALTER TABLE rate_limits
  ALTER COLUMN window_start_ts TYPE TIMESTAMPTZ USING window_start_ts::TIMESTAMPTZ;

-- seen_messages
ALTER TABLE seen_messages
  ALTER COLUMN first_seen_at TYPE TIMESTAMPTZ USING first_seen_at::TIMESTAMPTZ;

-- sandbox_logs
ALTER TABLE sandbox_logs
  ALTER COLUMN ts TYPE TIMESTAMPTZ USING ts::TIMESTAMPTZ;

-- tenants
ALTER TABLE tenants
  ALTER COLUMN created_at            TYPE TIMESTAMPTZ USING created_at::TIMESTAMPTZ;
ALTER TABLE tenants
  ALTER COLUMN deleted_at            TYPE TIMESTAMPTZ USING deleted_at::TIMESTAMPTZ;
ALTER TABLE tenants
  ALTER COLUMN billing_period_end    TYPE TIMESTAMPTZ USING billing_period_end::TIMESTAMPTZ;
ALTER TABLE tenants
  ALTER COLUMN last_billing_event_at TYPE TIMESTAMPTZ USING last_billing_event_at::TIMESTAMPTZ;
ALTER TABLE tenants
  ALTER COLUMN billing_period_start  TYPE TIMESTAMPTZ USING billing_period_start::TIMESTAMPTZ;
ALTER TABLE tenants
  ALTER COLUMN grace_until           TYPE TIMESTAMPTZ USING grace_until::TIMESTAMPTZ;

-- user_trust_inputs
ALTER TABLE user_trust_inputs
  ALTER COLUMN updated_at TYPE TIMESTAMPTZ USING updated_at::TIMESTAMPTZ;
ALTER TABLE user_trust_inputs
  ALTER COLUMN tier_since TYPE TIMESTAMPTZ USING tier_since::TIMESTAMPTZ;

-- user_trust_history
ALTER TABLE user_trust_history
  ALTER COLUMN ts TYPE TIMESTAMPTZ USING ts::TIMESTAMPTZ;

-- _sqlx_migrations metadata table also uses TEXT for `applied_at`. Migrate
-- it so the runner row inserted by THIS migration uses TIMESTAMPTZ from
-- the start.
ALTER TABLE _sqlx_migrations
  ALTER COLUMN applied_at TYPE TIMESTAMPTZ USING applied_at::TIMESTAMPTZ;
