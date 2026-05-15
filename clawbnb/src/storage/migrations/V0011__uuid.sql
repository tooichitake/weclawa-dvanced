-- v7.0: Convert TEXT-stored UUIDs to native PostgreSQL UUID.
--
-- Currently `admin_keys.id` is TEXT containing uuid v4 strings like
-- `f47ac10b-58cc-4372-a567-0e02b2c3d479`. PG's native UUID type is
-- 16 bytes (vs 36 for TEXT), faster equality compares, and validates
-- format at insert time.
--
-- `audit_log.actor_key_id` references admin_keys.id semantically (FK
-- isn't declared since it can be NULL for 'system' actors), so it
-- also converts. Same for any foreign field referring to admin key.

ALTER TABLE admin_keys
  ALTER COLUMN id TYPE UUID USING id::UUID;

-- audit_log.actor_key_id references admin_keys.id by value (no FK
-- because system-actor rows have NULL there). Convert to UUID too so
-- joins / comparisons stay type-safe.
ALTER TABLE audit_log
  ALTER COLUMN actor_key_id TYPE UUID USING actor_key_id::UUID;
