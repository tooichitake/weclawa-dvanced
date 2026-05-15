-- v7.0: Convert TEXT-stored JSON columns to native PostgreSQL JSONB.
--
-- Benefits:
--   - Storage: parsed/binary form, no per-read JSON parse cost
--   - Indexability: GIN indexes on JSONB enable `WHERE settings @> '{"x":1}'`
--   - Validity: PG validates JSON shape on insert (rejects malformed)
--   - Native operators: `->`, `->>`, `#>`, `@>`, `?`, etc.
--
-- Existing data: all current values were written via serde_json::to_string,
-- so they parse cleanly. `USING value::JSONB` casts in place; ~50ms per
-- table on a fresh install (single-operator scale).

ALTER TABLE config_kv
  ALTER COLUMN value_json TYPE JSONB USING value_json::JSONB;

ALTER TABLE defaults
  ALTER COLUMN settings_json TYPE JSONB USING settings_json::JSONB;

ALTER TABLE user_settings
  ALTER COLUMN settings_json TYPE JSONB USING settings_json::JSONB;

ALTER TABLE audit_log
  ALTER COLUMN before_json TYPE JSONB USING before_json::JSONB;
ALTER TABLE audit_log
  ALTER COLUMN after_json  TYPE JSONB USING after_json::JSONB;

ALTER TABLE user_trust_history
  ALTER COLUMN inputs_json TYPE JSONB USING inputs_json::JSONB;
