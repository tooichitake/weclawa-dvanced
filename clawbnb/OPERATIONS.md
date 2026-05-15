# Operations Manual

Day-2 operations for `weclawbot` v0.2+ (post commercial-grade hardening).

This document is the **operator's** reference — not the developer's.
Architecture docs live in `ARCHITECTURE.md`; the dev-facing API and
threat model live in `SECURITY.md` and the source comments.

---

## 1. Process model

Single long-running daemon (`weclawbot start`). State lives in
`~/.weclawbot/`:

| Path                              | Contents                                      |
|-----------------------------------|-----------------------------------------------|
| `state.db`                        | SQLite — accounts, users, history, audit log  |
| `state.db-wal` / `state.db-shm`   | Write-ahead log (auto-managed by SQLite)      |
| `config.json`                     | Operator config (rate limits, webhook URL, …) |
| `INITIAL-ADMIN-KEY.txt` (mode 600)| First-boot super_admin API key                |
| `logs/weclawbot-YYYY-MM-DD.log`   | Daily rotated structured-JSON logs            |
| `users/<u-hash>/sandbox/...`      | Per-user gVisor sandbox state                 |
| `weclawbot.pid`                   | Bound-PID file (created with O_NOFOLLOW)      |

The PID file path is checked at boot — symlink overwrite attacks are
rejected (see `daemon::pid::write_pid_to`).

---

## 2. Daily checks

Drive these via a cron job + alerts. Sample script at
`bin/health-watcher.sh`.

### 2.1 Liveness

```
curl -fsS http://127.0.0.1:18011/healthz
```

200 + `{"ok": true, ...}` = healthy. Any 5xx or `ok:false` means
**page the operator**. The body explains which sub-check failed:

```json
{
  "ok": false,
  "checks": {
    "db": "ok",
    "podman": "missing",
    "runsc": "ok",
    "disk": "ok",
    "disk_free_bytes": 41943040,
    "accounts": "ok"
  }
}
```

### 2.2 Metrics

`http://127.0.0.1:18011/metrics` — Prometheus exposition. Scrape
every 15 s. Key alerts:

| Alert                             | Condition                                                | Severity |
|-----------------------------------|----------------------------------------------------------|----------|
| `weclawbot_panics_total` ↑        | `increase(weclawbot_panics_total[5m]) > 0`               | page     |
| AI latency p99                    | `histogram_quantile(0.99, weclawbot_ai_latency_seconds_bucket) > 60` | ticket |
| Disk space                        | `weclawbot_disk_usage_bytes` > 80% of mount size         | ticket   |
| Token expiry                      | `weclawbot_token_expires_seconds < 3600` for any account | page     |
| API 5xx rate                      | 5xx fraction > 1% over 5 min                             | ticket   |

### 2.3 Logs

Structured JSON to `~/.weclawbot/logs/`. Tail with:

```
jq -c '.' < ~/.weclawbot/logs/weclawbot-$(date +%F).log | grep -i error
```

Logs older than 30 days are auto-deleted on each daemon boot.

---

## 3. Common procedures

### 3.1 Rotate the bootstrap admin key

Done from the GUI (super_admin → Admin Keys tab → Create), or via API:

```
curl -sX POST http://127.0.0.1:18011/api/v1/admin/keys \
  -H "Authorization: Bearer $OLD_KEY" \
  -d '{"name":"alice-laptop","role":"super_admin"}'
```

Plaintext is returned **once**. Revoke the old key after the new one
is verified to work:

```
curl -sX DELETE http://127.0.0.1:18011/api/v1/admin/keys/<OLD-KEY-ID> \
  -H "Authorization: Bearer $NEW_KEY"
```

### 3.2 Add a WeChat account

```
weclawbot login
```

Scan the QR code. The account row + token are persisted to `state.db`
(token plaintext today; AES-256-GCM in Phase 6). The token file is
the **only** way to talk to that user's WeChat bot — back it up.

### 3.3 Edit factory defaults for new users

GUI → Defaults tab. Changes take effect for new users immediately;
to push to existing users, click "Apply to all". Audit-log records
the diff per user.

### 3.4 Onboard a new operator

1. Super_admin issues a new admin key for them via the GUI.
2. They run `weclawbot console` from their machine (or open
   `http://daemon-host:18011/`).
3. Paste the new key into the login page. Session cookie kept 2 h.

---

## 4. Backup & restore

### 4.1 Take a snapshot

```
weclawbot backup --out /var/backups/weclawbot/state-$(date +%F).db
```

Uses SQLite's `VACUUM INTO` — no daemon stop required, output is a
defragmented self-contained file.

### 4.2 Suggested retention

Cron template at `bin/backup-and-rotate.sh`:

```
0 4 * * * weclawbot backup --out /var/backups/weclawbot/state-$(date +%F).db && \
          find /var/backups/weclawbot -name 'state-*.db' -mtime +30 -delete
```

Keep 30 daily + 12 monthly off-host backups. Periodically verify
restorability (procedure in §4.3).

### 4.3 Restore

**Daemon must be stopped first.** A running daemon holds an exclusive
WAL lock — `restore` refuses to clobber it.

```
weclawbot stop
weclawbot restore /var/backups/weclawbot/state-2026-05-13.db
weclawbot start
```

The pre-restore `state.db` is renamed to `state.db.pre-restore-<TS>`
so you can roll forward again if the restore was wrong.

---

## 5. Upgrades

### 5.1 In-place

```
weclawbot stop
# replace binary
weclawbot start
```

Schema migrations run automatically on first launch
(`refinery::migrations::runner().run()`). Old schemas → new schemas
are forward-compatible; downgrades require restore from snapshot.

### 5.2 Pre-flight before upgrade

1. Take a backup (§4.1).
2. Read the release notes for breaking changes.
3. If switching major version, test the upgrade on a staging copy of
   the production DB first.

### 5.3 Rollback

```
weclawbot stop
weclawbot restore <pre-upgrade snapshot>
# install previous binary
weclawbot start
```

---

## 6. Disaster recovery

### 6.1 Lost admin key

The daemon checks at boot: zero active super_admins → mints a fresh
one and prints it. So if you've lost *every* super_admin key:

1. `weclawbot stop`
2. Edit `state.db` directly: `UPDATE admin_keys SET revoked_at =
   datetime('now') WHERE role = 'super_admin';`
3. `weclawbot start` — prints a new `INITIAL-ADMIN-KEY` to stderr
   and `~/.weclawbot/INITIAL-ADMIN-KEY.txt` (mode 600).

### 6.2 Disk full

`weclawbot_disk_usage_bytes` alert fires at 80% utilization.
Mitigations:

1. Rotate logs sooner: edit the daemon's `WECLAWBOT_LOG_RETENTION_DAYS`
   env var (default 30).
2. Prune inactive user sandboxes: `weclawbot users prune --older-than-days 60`.
3. Compact audit log: SQL `DELETE FROM audit_log WHERE ts <
   date('now', '-180 days');` then `VACUUM;`.

### 6.3 Claude OAuth token expired

The `weclawbot_token_expires_seconds` metric goes negative; AI calls
return 401. Re-auth:

```
claude auth login    # from the daemon host
weclawbot restart    # picks up the new ~/.claude/.credentials.json
```

---

## 7. Tuning knobs

Set via `weclawbot config set <key> <value>`. Most operators never
touch these.

| Key                              | Default  | Notes                                       |
|----------------------------------|----------|---------------------------------------------|
| `ai.timeoutMs`                   | 300000   | Per-AI-call hard ceiling (5 min)            |
| `ai.historyLimit`                | 16       | Turns of context fed to the model           |
| `rate_limit.user_per_minute`     | 30       | Per-WeChat-user inbound cap (Phase 5)       |
| `webhook.url`                    | (unset)  | Forward inbound events; must be https://    |

**Blocked keys** (cannot be edited from `config set` or the WeChat
`/menu` flow): everything under `mcpServers.weclawbot`, all `env/*_API_KEY`,
`oauthAccount`, `accounts/*`. Phase 0.3 enforces this.
