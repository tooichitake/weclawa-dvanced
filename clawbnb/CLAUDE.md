# CLAUDE.md — weclawbot dev rules

## Build / test environment: **WSL only**

The daemon links against Linux-only deps:
- `podman` + `runsc` (gVisor) for per-user sandboxes — Linux kernel
- Postgres client libs (sqlx-postgres) — works on both but tested on Linux
- File permission semantics (`0o600` for token-at-rest) — Unix `mode_bits`

**Never run `cargo build` or `cargo test` from Windows-side PowerShell/cmd.**
The resulting `.exe` is unused dead weight and pollutes `target/release/`.
Always shell into WSL first:

```bash
wsl
cd /mnt/c/projects/weclawa-advanced/.claude/worktrees/<branch>/clawbnb
cargo build --release --bin weclawbot
cargo test --release --bin weclawbot -- --test-threads=1
```

## Postgres backend (v5.6+) + native types (v7.0+)

Daemon is **Postgres-only** from v5.6 onwards. SQLite was dropped to
eliminate dual-backend bugs.

v7.0 upgraded all column types to PG-native: JSONB (settings/audit/trust
payloads), UUID (admin_keys.id, audit_log.actor_key_id), TIMESTAMPTZ
(all 22 timestamp columns). Domain types still expose `String` at the
API boundary for backward compat — translation happens in repo via
`crate::storage::ts::*`.

### Local dev setup (one-time)

```bash
# 1. Run postgres:16 in a podman container (same podman as sandbox).
podman run -d --name weclawbot-pg --restart unless-stopped \
  -e POSTGRES_USER=weclawbot -e POSTGRES_PASSWORD=weclawbot-local \
  -e POSTGRES_DB=weclawbot -p 127.0.0.1:5432:5432 \
  -v weclawbot-pgdata:/var/lib/postgresql/data docker.io/postgres:16

# 2. Persist the DSN env (already in ~/.bashrc for this WSL):
export WECLAWBOT_PG_URL='postgres://weclawbot:weclawbot-local@127.0.0.1:5432/weclawbot'

# 3. Start daemon as usual — migrations apply automatically on first boot.
weclawbot start
```

### Migrating from a legacy SQLite state.db

If you're upgrading from v5.5 or earlier, your data lives in
`~/.weclawbot/state.db` (SQLite). One-shot import:

```bash
weclawbot stop
podman start weclawbot-pg  # if container not already running
weclawbot import-sqlite ~/.weclawbot/state.db
weclawbot start
```

The SQLite file is left untouched as a rollback option. The importer
is idempotent — re-running it is a no-op.

## Deploy fresh binary to running daemon

User-facing daemon symlink: `~/.local/bin/weclawbot →
~/weclawbot-src/target/release/weclawbot`

After building from worktree, copy over the symlink target:

```bash
cd /mnt/c/projects/weclawa-advanced/.claude/worktrees/<branch>/clawbnb
cargo build --release --bin weclawbot
cp target/release/weclawbot ~/weclawbot-src/target/release/weclawbot
weclawbot stop && weclawbot start  # if it was running
```

## Feature flag matrix

Test these combinations before commit:

| Feature flags | When required |
|---|---|
| (default) | always — daemon runtime (PG backend always on) |
| `--features ee` | when touching `src/ee/*` (SSO/SAML/JWT) |
| `--features acp` | when touching `src/ai/claude/*` |
| `--features otel` | when touching observability/tracing |
| `--features discord-gateway` | when touching discord puppet |
| `--features "ee,otel,acp,discord-gateway"` | always — kitchen-sink smoke test |

**Note**: `postgres` feature flag deleted in v5.6 — PG is always on.
**Note**: `sqlite` feature is no longer mentioned anywhere; sqlx still
ships with sqlite enabled for the `import-sqlite` one-shot migration,
but no runtime code uses it.

Tests use `testcontainers` (v7.0+) — each test gets its own randomly-
named Postgres database inside a shared `postgres:16` container that
lives for the test process. Tests parallelize by default (no
`--test-threads=1` needed). The container is auto-stopped at process
exit.

**WSL setup (one-time)**: `testcontainers` needs a Docker-compatible
socket. WSL provides podman's native socket which speaks the Docker
API:

```bash
# Enable podman API socket as a systemd user service (persistent).
systemctl --user enable --now podman.socket

# Ensure log driver is k8s-file (not journald) so testcontainers'
# `follow logs` wait-for-ready strategy works. Default journald
# + file events-backend combo doesn't support log streaming.
cat > ~/.config/containers/containers.conf << 'EOF'
[containers]
log_driver = "k8s-file"

[engine]
cgroup_manager = "cgroupfs"
events_logger = "file"

[engine.runtimes]
runsc = ["/usr/local/bin/runsc-wsl"]

[network]
default_rootless_network_cmd = "pasta"
EOF
systemctl --user restart podman.socket

# Point testcontainers at the socket. Put in ~/.profile so it
# persists across WSL sessions:
echo 'export DOCKER_HOST=unix:///run/user/1000/podman/podman.sock' >> ~/.profile

# Verify (in a new shell):
echo $DOCKER_HOST
podman info --format json | grep logDriver  # should report "k8s-file"
cargo test --release  # should run all tests in parallel, ~35s
```

CI Ubuntu runners have Docker pre-installed so they don't need this
workaround — testcontainers picks up `/var/run/docker.sock` directly.

## CI

GitHub Actions workflows in `.github/workflows/`:
- `ci.yml` — fmt/clippy/test/build on every push + PR (PG service
  container always running)
- `smoke.yml` — nightly end-to-end smoke (manual + cron)
- `release.yml` — tag-triggered release build
- `sandbox-image.yml` — gVisor sandbox image build

## Repo layout

```
clawbnb/                 — daemon crate (this dir)
  src/
    api/                 — iLink WeChat client + QR rendering
    ai/                  — Claude/Codex/OpenAI dispatcher + ACP session
    app/                 — UseCase layer (delete user, etc.)
    auth/                — admin keys + SAML/OIDC
    cli/                 — top-level CLI subcommands
      import_sqlite.rs   — one-shot SQLite → PG data migration (v5.6)
    config.rs            — global config + ArcSwap hot-reload
    daemon/              — log/pid/panic handler
    ee/                  — enterprise features (gated `--features ee`)
    error.rs             — WeclawError + RFC 7807 problem+json
    media/               — inbound/outbound attachment handling
    monitor/             — message poller + handler + rate_limit
    pii/                 — PII detection + redaction (14 classes)
    puppet/              — protocol-layer (iLink/Telegram/Discord/Feishu)
    repo/                — DB access layer (sqlx PgPool)
    runtime/             — blocking ↔ async bridge
    sandbox/             — gVisor container lifecycle
    service/             — axum HTTP routes
    storage/             — db_async pool + PG migrations + atomic_write
    tenancy/             — multi-tenant resolver + trust scoring
  assets/console/        — GUI (HTML + CSS + JS, embedded in binary)
  weclawbot-mcp/         — separate stdio MCP server crate
  deploy/                — docker-compose + Helm chart for self-host
  scripts/               — one-off dev scripts (pg_placeholder_rewrite.py)
```

## Git workflow

Worktree's `.git` file references a Windows path, so git commands run
from Windows shell or WSL with the right wrapper. The simplest:
just run from the Windows side:

```powershell
cd C:\projects\weclawa-advanced\.claude\worktrees\<branch>\clawbnb
git add .
git commit -m "..."
git push
```
