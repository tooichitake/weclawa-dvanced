-- v2.1.A2: persistent inbound message deduplication.
--
-- iLink platform occasionally re-delivers the same `message_id` within
-- its long-poll window (network jitter, retry, daemon restart during
-- graceful drain). Before this table dedup was process-local — daemon
-- restart wiped it and every redelivered message ran the full handler
-- a second time (duplicate AI reply, double billing of Claude call,
-- user confusion).
--
-- Schema:
--   `msg_id`         iLink's per-message i64 id; PK
--   `first_seen_at`  RFC3339 UTC; used by prune sweep
--
-- TTL is enforced at the application layer via `prune_older_than`
-- with a 1% chance per inbound (mirroring rate_limits' lazy prune).
-- Default keep window is 1 hour — far longer than iLink's redeliver
-- window (~minutes) but small enough to keep table tiny.

CREATE TABLE seen_messages (
  msg_id         INTEGER PRIMARY KEY,
  first_seen_at  TEXT NOT NULL
);

CREATE INDEX idx_seen_at ON seen_messages (first_seen_at);
