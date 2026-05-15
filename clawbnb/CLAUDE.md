# CLAUDE.md — weclawbot dev rules

## Build / test environment: **WSL only**

The daemon links against Linux-only deps:
- `podman` + `runsc` (gVisor) for per-user sandboxes — Linux kernel
- `sqlx-sqlite` bundled libsqlite3 — works on both but tested only on Linux
- File permission semantics (`0o600` for token-at-rest) — Unix `mode_bits`

**Never run `cargo build` or `cargo test` from Windows-side PowerShell/cmd.**
The resulting `.exe` is unused dead weight and pollutes
`target/release/`. Always shell into WSL first:

```bash
wsl
cd /mnt/c/projects/weclawa-advanced/.claude/worktrees/<branch>/clawbnb
cargo build --release --bin weclawbot
cargo test --release --bin weclawbot
```

## Deploy fresh binary to running daemon

User-facing daemon symlink: `~/.local/bin/weclawbot →
~/weclawbot-src/target/release/weclawbot`

After building from worktree, copy over the symlink target:

```bash
cp /mnt/c/projects/weclawa-advanced/.claude/worktrees/<branch>/clawbnb/target/release/weclawbot \
   ~/weclawbot-src/target/release/weclawbot

# If daemon was running, restart it for the new binary to take effect:
weclawbot stop && weclawbot start
```

## Push commits

Git operations work from Windows shell (worktree `.git` file
references a Windows path). From either:

```bash
git push -u origin <branch>
```

## Feature flag matrix

Test these combinations before commit:

| Feature flags | When required |
|---|---|
| (default) | always |
| `--features ee` | when touching `src/ee/*` (SSO/SAML/JWT) |
| `--features acp` | when touching `src/ai/claude/*` |
| `--features postgres` | when touching `src/storage/db_async.rs` or `src/repo/*_async.rs` |
| `--features otel` | when touching observability/tracing |
| `--features discord-gateway` | when touching discord puppet |
| `--features "ee,otel,acp,discord-gateway,postgres"` | always — kitchen-sink smoke test |

Tests use `--test-threads=1` because:
- Some tests install a singleton `OnceLock` global pool (`backup`,
  `admin_key::install_pool`).
- Postgres tests share one container across tests; data race needs serial.

## CI

GitHub Actions workflows in `.github/workflows/`:
- `ci.yml` — fmt/clippy/test/build on every push + PR
- `postgres.yml` — `--features postgres` matrix with `postgres:16` service
- `smoke.yml` — nightly end-to-end smoke (manual + cron)
- `release.yml` — tag-triggered release build

## Repo layout

```
clawbnb/                 — daemon crate (this dir)
  src/
    api/                 — iLink WeChat client + QR rendering
    ai/                  — Claude/Codex/OpenAI dispatcher + ACP session
    app/                 — UseCase layer (delete user, etc.)
    auth/                — admin keys + SAML/OIDC
    cli/                 — top-level CLI subcommands
    config.rs            — global config + ArcSwap hot-reload
    daemon/              — log/pid/panic handler
    ee/                  — enterprise features (gated `--features ee`)
    error.rs             — WeclawError + RFC 7807 problem+json
    media/               — inbound/outbound attachment handling
    monitor/             — message poller + handler + rate_limit
    puppet/              — protocol-layer (iLink/Telegram/Discord/Feishu)
    repo/                — DB access layer (sqlx async)
    runtime/             — blocking ↔ async bridge
    sandbox/             — gVisor container lifecycle
    service/             — axum HTTP routes
    storage/             — db_async pool + migrations + atomic_write
    tenancy/             — multi-tenant resolver + trust scoring
  assets/console/        — GUI (HTML + CSS + JS, also embedded in binary)
  weclawbot-mcp/         — separate stdio MCP server crate
```
