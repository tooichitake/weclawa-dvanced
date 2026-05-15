# Compliance mode — HIPAA / SOC2 / GDPR

weclawbot ships a `compliance.mode` setting in `config.json` (or env
`WECLAWBOT_COMPLIANCE_MODE`) that wires together the **PII detection
pipeline**, **webhook scrub**, **audit log scrub**, and **AI prompt
scrub** into one toggle.

## Modes

| Mode | Webhook | Audit log | AI prompt | Use case |
|---|---|---|---|---|
| `permissive` (default) | Pass | Pass | Pass | Self-host, single operator. |
| `standard` | Redact | Redact | Pass | Most multi-tenant SaaS. |
| `strict` | Redact+Block | Redact | Redact+Block | Internal corporate IT. |
| `hipaa` | Redact+Block | Redact | Redact+Block | Healthcare PHI (US). |
| `soc2` | Redact+Block | Redact | Redact+Block | SOC2 Type II audit prep. |
| `gdpr` | Redact+Block | Redact | Redact+Block | EU PII (DSR-friendly). |

The `hipaa`, `soc2`, `gdpr` modes are **semantically identical** to
`strict` in terms of redaction policy. The differences are operational:

- **`hipaa`** writes a metadata flag in audit log so downstream
  compliance tooling can filter for "this row was produced under HIPAA
  mode."
- **`soc2`** triggers tighter log retention (default 12 months → see
  `OPERATIONS.md`) and tags audit rows accordingly.
- **`gdpr`** injects EU-residency hints in outbound webhook headers
  (`X-Weclawbot-Data-Region: eu`) so receivers can route accordingly.

## Detected PII classes (14)

See `src/pii/mod.rs` docstring for the full table. Key classes:

- **Block** by default (forbidden): `ID_CARD_CN`, `PASSPORT`, `IMEI`
- **Redact** by default: `MOBILE_CN`, `MOBILE_INTL`, `BANK_CARD`,
  `EMAIL`, `IP`, `PLATE_CN`, `QQ`, `WECHAT_ID`
- **Hash** by default: `ADDRESS_CN`, `NAME_CN`
- **Pass** by default: `URL`

Operators can override per-class via `config.compliance.pii_*_overrides`.

## DSR (Data Subject Request) workflow — GDPR Art. 15/17

When a user requests their data:

```bash
# 1. Export the tenant's full bundle (includes all user data scoped to
#    this tenant; per-user filter handled by the receiving operator).
weclawbot export-tenant --tenant <id> --out /tmp/tenant-<id>-export.tar.gz

# 2. For per-user DSR, extract only that user's workspaces/<hash>/ and
#    their rows from users.jsonl / user_settings.jsonl / user_history.jsonl
#    / audit.jsonl (filter on user_hash = <their hash>).

# 3. Hand the filtered archive to the requester.

# 4. For erasure (Art. 17), reset the user:
weclawbot users reset <hash>
```

The export bundle automatically redacts operator-scope secrets (API
keys, encryption key, webhook secret); see `src/cli/export_tenant.rs`
docstring.

## Token-at-rest encryption

All bot tokens are AES-256-GCM encrypted before being written to the
`accounts.token_ciphertext` column (Phase 6.1, MIT main). Key sourced
from (in priority order):

1. `WECLAWBOT_DB_KEY` env var (base64 32 bytes)
2. `~/.weclawbot/.db-key` file (mode 0600)
3. OS keyring (`secret-tool` on Linux)

For HIPAA/SOC2 deployments, source from a KMS (AWS KMS / GCP KMS /
HashiCorp Vault) and inject via env at boot.

## What weclawbot does NOT do

- **HIPAA Business Associate Agreement (BAA)** — operator must sign
  their own BAA with Anthropic (for Claude) and any other downstream
  services. weclawbot is the bridge, not the data processor.
- **Encryption at rest for SQLite/Postgres** — use filesystem-level
  encryption (LUKS / dm-crypt) or Postgres TDE. weclawbot only
  encrypts the most sensitive column (bot tokens).
- **SOC2 audit trail forwarding** — `audit_log` table is the source.
  Operators must export to a tamper-resistant sink (CloudTrail /
  Splunk) themselves; see `OPERATIONS.md` for the recommended
  Vector/Fluentd configs.
- **CSE (customer-supplied encryption keys)** — single operator-wide
  key for now. v6 SaaS roadmap.

## XML signature verify gap for SAML SSO

The bundled SAML signature verify (`ee::saml::verify_signature_rsa_sha256`)
does real RSA-PKCS#1 v1.5 SHA-256 over the extracted `SignedInfo`
substring, but does NOT perform W3C Exclusive XML Canonicalization
(c14n) — see `ee/saml.rs` docstring.

**For compliance-grade SAML deployments, put weclawbot behind
`mod_auth_mellon` or `shibboleth-sp`** (Apache/nginx module that does
full canonical XML verify in C via libxmlsec1) and have it inject
trusted headers (`Mellon-NAME-Email`, `SHIB-USER`) that weclawbot
consumes. This is the standard production pattern for SAML in the
Rust ecosystem and is what the `hipaa` / `soc2` modes assume is in
place upstream.
