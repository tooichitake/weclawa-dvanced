-- Phase 6.1: token-at-rest encryption.
--
-- Add ciphertext + nonce columns alongside the legacy plaintext `token`
-- column. New writes populate both for one release cycle so a downgrade
-- can still read the data; once every operator has rolled forward we
-- can drop the plaintext column in V0003.
--
-- The columns are nullable because v0.2 → v0.3 upgrades start with all
-- existing rows unencrypted; the migrator (in cli/start.rs) sweeps and
-- backfills on first launch after upgrade.

ALTER TABLE accounts ADD COLUMN token_ciphertext BYTEA;
ALTER TABLE accounts ADD COLUMN token_nonce      BYTEA;
