# Security Policy & Threat Model

`weclawbot` v0.2+ — commercial-grade architecture.

This document is the **definitive** description of:
- What attacks the daemon defends against
- Which attacks are out of scope (and why)
- How operators should think about their own threat model
- Where to report vulnerabilities

---

## 1. Threat model

### 1.1 In-scope adversaries

| # | Adversary                                  | Capability                                  |
|---|--------------------------------------------|---------------------------------------------|
| A | **Unprivileged local user** on daemon host | Read/write own files; cannot read 0600 ones |
| B | **Network attacker** on the admin LAN      | Can reach `127.0.0.1:18011` from a sidecar  |
| C | **WeChat user** sending crafted messages   | Full control over inbound text/attachments  |
| D | **Compromised AI provider** (Claude/Codex) | Can return arbitrary tool calls + URLs      |
| E | **Compromised operator workstation**       | Has a valid admin API key (until revoked)   |

### 1.2 Out of scope (v1)

- **Kernel-level attacker on the daemon host.** Root can read
  `state.db`. We don't try to defeat that; we assume host hardening
  (selinux/apparmor, full-disk encryption) is the operator's job.
- **State-level traffic analysis.** Webhook destinations get
  inbound-message metadata; that's the operator's design choice.
- **Side-channel attacks on AES-GCM / argon2** (timing, cache).
  We use the standard library implementations as-is.

---

## 2. Defences in v0.2

### 2.1 Authentication

- All `/api/v1/...` endpoints require `Authorization: Bearer
  weclawbot_<prefix>_<secret>`. Missing or invalid → 401.
- Keys are 256-bit random, argon2id-hashed in `admin_keys.key_hash`.
  Plaintext is shown to the operator **once at mint time** and
  written to `~/.weclawbot/INITIAL-ADMIN-KEY.txt` (mode 0600).
- Bootstrap: daemon mints a super_admin on first boot if the table
  is empty. Idempotent across restarts.
- Revocation is a soft delete (`revoked_at` column); audit history
  is preserved.

### 2.2 Authorization (RBAC)

Three roles, in order of privilege:

| Role          | Can read | Can mutate users/defaults | Can manage keys / backup |
|---------------|----------|---------------------------|--------------------------|
| `read_only`   | yes      | no                        | no                       |
| `read_write`  | yes      | yes                       | no                       |
| `super_admin` | yes      | yes                       | yes                      |

Enforcement is at the HTTP middleware (`service::auth::require_role`);
the WeChat `/menu` surface is intentionally exempt — those commands
are scoped to the user's own per-user state.

### 2.3 Input validation

- User-hash path parameters (`/api/v1/users/{hash}`) validated against
  `^u-[0-9a-f]{12}$` regex (Phase 0.1).
- `config set` and the WeChat `/menu` editor reject writes to
  `BLOCKED_PATH_PREFIXES` (Phase 0.3).
- Request bodies capped at 2 MiB (Phase 0.4).
- Inbound media URLs (Claude `attach_url`) are SSRF-checked:
  DNS-resolve → reject loopback / private / link-local / CGNAT
  (Phase 0.6).
- Webhook URLs must be `https://...` unless the host is localhost
  (Phase 0.7).

### 2.4 Secret handling

- iLink bot tokens: today stored as plaintext in `accounts.token`.
  Phase 6 will move to AES-256-GCM with per-row nonces; the schema
  has placeholder columns ready. **Operators should not consider
  v0.2 a defense against host-root attackers** (see §1.2).
- Admin keys: argon2id hashed; plaintext never persisted past mint
  time.
- Account JSON files (legacy): chmod 0o600 on Unix (Phase 0.2).
- PID file: created with `O_CREAT|O_EXCL|O_NOFOLLOW` + 0o600
  (Phase 0.5).
- `BotToken` newtype: `Debug` and `Display` redact the middle of
  the string; the full value only escapes via `expose()` which has
  greppable callsites.

### 2.5 Sandboxing

Per-user gVisor (`runsc`) containers via podman. Each WeChat user's
Claude session runs in its own namespace, with `$HOME/.claude` bind-
mounted from a read-only operator-controlled source plus a writable
`work/` dir.

### 2.6 Observability for security

- `audit_log` records every mutating API call: actor key id, action,
  target, before/after JSON, source IP.
- 401 / 403 rejects are also recorded (Phase 3).
- `weclawbot_panics_total` increments on every panic anywhere in the
  process; the panic hook logs location + message.
- All HTTP errors return RFC 7807 `application/problem+json`.

---

## 3. Vulnerability reporting

Email security findings to `<operator-set>` with the prefix
`[weclawbot security]`. Use GPG (key in this repo's
`build/security-key.asc`) for issues that warrant non-public discussion.

Expected response time:

- Acknowledgement: 2 business days
- Initial triage: 5 business days
- Coordinated disclosure for verified high-severity issues:
  90 days from acknowledgement

---

## 4. Future hardening (planned, not yet shipped)

These are documented as deferred so operators know what's coming and
don't assume an unimplemented protection:

- **AES-256-GCM at rest for iLink tokens.** Per-row random nonce,
  key from OS keyring or `WECLAWBOT_DB_KEY` env. Tracked as Phase 6.1.
- **SHA-256 user-id hashing** to replace SHA-1. Phase 6.4. Cosmetic
  more than security — SHA-1 is used for non-cryptographic dedup —
  but operators expecting modern hash families notice the difference.
- **Sandbox disk quota** (refuse spawn if user's `work/` > 500 MB).
  Phase 6.3.
- **Per-user inbound rate limit** with token-bucket policy (schema
  is already in `rate_limits`). Phase 5.1.
- **TLS termination** for `/api/v1/*`. Today the daemon listens
  plaintext on `127.0.0.1`. Operators wanting remote access should
  front it with a TLS-terminating reverse proxy (nginx, caddy).

---

## 5. Trust boundaries summary

```
                    WeChat user (untrusted, C)
                          │
                          │  inbound msg
                          ▼
            ┌────────────────────────────┐
            │  monitor::poller / handler │  ← validates roles, applies
            │  (per-account)             │     rate limits (Phase 5)
            └────────────────┬───────────┘
                             │
                             ▼
           ┌────────────────────────────────┐
           │  per-user sandbox (gVisor)     │ ← strong isolation
           │  $HOME/.claude/ + work/        │
           │  no network egress except via  │
           │  weclawbot-mcp                 │
           └─────────────┬──────────────────┘
                         │
                         ▼
                Claude / Codex API (D)

Admin operator (E)                    Local user (A)
        │                                    │
        │  Authorization: Bearer …           │  filesystem read
        ▼                                    ▼
┌──────────────────┐                ┌─────────────────────────┐
│ /api/v1/* + GUI  │                │ ~/.weclawbot/  (0600 on │
│ (Bearer + RBAC)  │                │  sensitive files only)  │
└──────────────────┘                └─────────────────────────┘
```
