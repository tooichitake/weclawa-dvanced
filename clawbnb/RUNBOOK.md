# Runbook

Specific, low-context responses to the alerts named in
`OPERATIONS.md §2.2`. Every section assumes you have:

- Shell on the daemon host
- A `super_admin` API key (used as `$KEY`)
- Daemon URL in `$BASE` (default `http://127.0.0.1:18011`)

If an alert fires that's not in this runbook, write it up here when
you finish triaging — that's how this file stays useful.

---

## Alert: `weclawbot_panics_total` increased

**What happened.** A Rust panic fired somewhere in the process.
The panic hook logged it but the tokio task that owned the stack
is dead. The daemon stayed up — but whatever feature that task
was implementing is degraded.

**Triage.**

1. Find the panic message:
   ```
   jq -c 'select(.target=="panic")' \
     ~/.weclawbot/logs/weclawbot-$(date +%F).log | tail -20
   ```

2. Identify the affected subsystem from `panic_location`. Common
   suspects:
   - `monitor/poller.rs` — one account's poll loop died; that account
     stops receiving messages until restart.
   - `service/routes.rs` — single API request panicked; clients see
     500.

3. If the panic is reproducible (e.g. operator-supplied input), open
   a bug with the log line attached.

4. **Mitigation:** `weclawbot restart` brings the dead tokio task
   back. Audit log will show pre-restart actions.

---

## Alert: AI latency p99 > 60 s

**What happened.** The model is slow or struggling. Could be:
- Claude API overload (look at `https://status.anthropic.com`)
- Network path degradation
- Sandbox image cold-start (first invocation in a long while)

**Triage.**

1. Check `/metrics`: is it all providers, one account, or specific
   user hash?
   ```
   curl -s $BASE/metrics | grep weclawbot_ai
   ```

2. Look for `provider="claude" status="timeout"` counter ticks.
   If yes, Anthropic side; advise users; no action.

3. If only one account is slow, check `weclawbot_token_expires_seconds`
   for that account — token may be near expiry and the daemon is
   doing more handshake work.

---

## Alert: Disk free < 200 MB

**What happened.** `/healthz` is now returning 503 because the
disk-floor check tripped.

**Triage.**

1. Find biggest consumers:
   ```
   du -sh ~/.weclawbot/* | sort -h | tail
   ```

2. Common offenders, in order:
   - `users/*/sandbox/work/` — clear with `weclawbot users prune
     --older-than-days 60`.
   - `logs/` — older than 30 days are auto-deleted on boot, but
     active days can accumulate fast. Compress old ones with
     `gzip ~/.weclawbot/logs/weclawbot-2026-04-*.log`.
   - `state.db` — if grown to many GB, run `VACUUM;` against it
     (daemon must be stopped). `audit_log` is the usual culprit.

3. After clearing, `weclawbot restart` to clear the cached check.

---

## Alert: Token expires in < 1 h

**What happened.** A bot account's iLink token is about to die.

**Triage.**

1. Which account? `weclawbot status` shows per-account info.
2. Re-login that specific account:
   ```
   weclawbot login
   ```
   QR flow walks through it. The new token replaces the old in
   `state.db` (UPSERT via `AccountRepo::rotate_token`).

3. Watch `weclawbot_token_expires_seconds{account_id="<id>"}` go
   back to a large positive value.

---

## Alert: API 5xx rate > 1%

**What happened.** Server-side errors. Either DB unhealthy, a panic
loop, or a bad deploy.

**Triage.**

1. `/healthz` — what's failing?
2. Check log for error spam:
   ```
   jq -c 'select(.level=="ERROR")' \
     ~/.weclawbot/logs/weclawbot-$(date +%F).log | tail -50
   ```
3. If DB is the failing check, look for "database is locked" or
   "disk i/o error" — could be WAL corruption. Stop the daemon,
   run `sqlite3 ~/.weclawbot/state.db 'PRAGMA integrity_check;'`.
   If it reports errors, restore from the most recent backup
   (`OPERATIONS.md §4.3`).

---

## Procedure: Suspected compromised admin key

1. Identify the suspect key by `last_used_at` and the audit_log
   entries with `actor_key_id = <id>`.
2. Revoke immediately:
   ```
   curl -X DELETE $BASE/api/v1/admin/keys/<id> \
     -H "Authorization: Bearer $YOUR_KEY"
   ```
3. Mint a replacement for whoever owned the revoked key.
4. Review the audit_log for that key's actions in the last 30 days.
   Suspicious activity? Roll back any unauthorized changes from
   the `before_json` snapshots.
5. Document the incident in this file under a dated entry.

---

## Procedure: Migrate to a new host

1. New host: install weclawbot binary + `podman` + `runsc`.
2. Old host: `weclawbot backup --out /tmp/snap.db`.
3. Copy `/tmp/snap.db` and `~/.weclawbot/config.json` to new host.
4. New host: `mkdir -p ~/.weclawbot && cp /tmp/snap.db ~/.weclawbot/state.db
   && cp config.json ~/.weclawbot/config.json`.
5. New host: `weclawbot start`. Schema migrations apply if the
   binary is newer.
6. Verify: hit `/healthz`, then send a test message from one
   bound WeChat account.

The iLink tokens are still valid on the new host (they're tied to
the account, not the host) until they expire on their normal
schedule.
