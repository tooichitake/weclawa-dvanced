# weclawbot

WeChat ↔ Claude Code bridge with per-user gVisor sandboxing. A single Rust binary that:

- Logs into WeChat via iLink bot protocol (QR scan)
- Long-polls inbound messages from each bound account
- Spawns a fresh, isolated gVisor container per WeChat user per message
- Drives Claude Code (or Codex) inside the container, parses its
  `stream-json` output, and routes generated files (docx, xlsx, pdf, png,
  pptx, …) back to the user as WeChat media messages
- Exposes a per-user `/menu` console for end-users to view/change their
  Claude settings, and a separate HTTP admin GUI on `http://127.0.0.1:18011`
  for the operator

## Requirements

Linux x86_64 or aarch64. `podman` + `runsc` (gVisor) must be installed —
the install script handles them.

The daemon, the sandbox runtime, and the per-user state all live entirely
on the local host. No external services are required beyond an Anthropic
Claude subscription for the model itself.

## Install

One-button (v7.9+):

```bash
curl -fsSL https://raw.githubusercontent.com/tooichitake/weclawa-dvanced/main/clawbnb/build/install.sh | bash
```

Fully self-contained — no manual pre-reqs. The installer:

1. Drops the `weclawbot` binary to `~/.local/bin/` (SHA-256 verified)
2. Installs `podman` (apt / dnf / pacman / zypper by distro)
3. Starts **Postgres 16** as a podman container (`weclawbot-pg`) on
   `127.0.0.1:5432`, persisted in volume `weclawbot-pgdata`, supervised
   by a systemd --user unit so it survives reboots
4. Generates a 32-byte AES master key for token-at-rest encryption,
   writes it (mode 0600) to `~/.config/environment.d/weclawbot.conf`
5. Downloads `runsc` from gVisor releases, verifies sha512, registers
   it as a podman runtime
6. Pulls the sandbox base image
   (`ghcr.io/tooichitake/weclawa-dvanced/weclawbot-sandbox-base:latest`)
7. Installs and starts `weclawbot.service` (systemd --user) + enables
   user linger so the daemon survives logout

Idempotent — re-running upgrades each piece in place.

### Optional env knobs

```bash
# Use an existing Postgres instead of the bundled podman container:
WECLAWBOT_PG_URL='postgres://user:pw@host:5432/db' bash install.sh

# Pin to a specific release tag:
WECLAWBOT_VERSION=weclawbot-v0.1.0 bash install.sh

# Skip the binary step (you'll cargo-build it yourself first):
WECLAWBOT_SKIP_BINARY=1 bash install.sh

# Air-gapped (skip the sandbox-image pull):
SKIP_IMAGE_PULL=1 bash install.sh
```

### Uninstall

```bash
bash uninstall.sh           # preserves data
bash uninstall.sh --purge   # also wipes ~/.weclawbot + PG container + volume
```

## Daily use

```bash
weclawbot login            # QR-scan to bind a WeChat account
weclawbot start            # background daemon (foreground: --foreground)
weclawbot console          # open http://127.0.0.1:18011 in browser
weclawbot status           # daemon health + accounts
weclawbot stop
```

End-user (WeChat side): send any text and Claude replies. Send `/menu`
to enter the per-user settings console — model / system prompt / plugins /
"open a new chat" etc., all edits applied to that user's `settings.json`.

Operator (HTTP console): visit `http://127.0.0.1:18011` to view per-user
state, edit defaults, manage accounts.

## Configuration

`~/.weclawbot/config.json` is the **operator-managed global** config. Edit
via `weclawbot ai --provider claude --model sonnet`, `weclawbot config set
webhook.url https://your-handler/...`, or the admin GUI.

`~/.weclawbot/users/<u-hash>/settings.json` is each WeChat user's
Claude-Code-style settings, edited by them via `/menu` or by the operator
in the admin GUI.

`~/.weclawbot/defaults/claude-settings.json` is the factory template
applied to new users.

Reply provider precedence:

1. `config.webhook.url` non-empty → POST to that URL, use response
2. `config.ai.provider == claude` → spawn claude in sandbox
3. `config.ai.provider == codex` → spawn codex in sandbox
4. `config.ai.provider == api` with `apiKey` set → OpenAI-compatible HTTP
5. `config.echo.enabled` → echo the input
6. otherwise: no reply

## File delivery

When Claude wants to send a file back to the WeChat user it calls the MCP
tool `attach(path="/work/output/foo.docx")`. The host parses the
`tool_use` event in stream-json, uploads the file via iLink's CDN, and
attaches it to the next outbound WeChat message. Remote URLs go through
`attach_url(url="https://...")`.

If Claude forgets to call `attach`, a filesystem-diff fallback over
`/work/output/` catches anything created during the turn — logged at WARN
level as a prompt regression. Helper scripts written to `/tmp/` (per the
system prompt convention) are never forwarded.

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for module map, inbound message
flow, and sandbox lifecycle.

## Development

```bash
cargo build --release
cargo test --release
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

E2E smoke (requires a real Claude OAuth token):

```bash
WECLAWBOT_BIN=$HOME/weclawbot-target/release/weclawbot \
  bash scripts/smoke-e2e.sh
```

CI on every push runs build + clippy + test. The smoke harness is
gated behind `workflow_dispatch` because it needs Claude credentials.

## License

MIT.
