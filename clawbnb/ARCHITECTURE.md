# weclawbot — Architecture

## Module map

```
src/
├── main.rs             clap-based CLI; routes to cli/*::run
├── ids.rs              newtypes: AccountId, UserHash, WeixinUserId,
│                       BaseUrl, BotToken (redacted Debug/Display)
├── config.rs           typed schema for ~/.weclawbot/config.json
│                       + Config::apply_migrations (idempotent bumps)
│
├── ai/                 AI reply providers
│   ├── mod.rs          public types (ClaudeOutput, GeneratedFile,
│   │                   AttachedUrl, FileSource, CliConfig)
│   ├── cli_provider.rs thin dispatcher: complete_with_content() picks
│   │                   provider by cfg.ai.provider, calls into:
│   ├── claude/
│   │   ├── mod.rs      invoke(): spawn claude in sandbox, stream-json loop
│   │   ├── prompt.rs   build_prompt: WeChat role + /tmp scratch + emoji
│   │   └── stream_json.rs  parse events → ClaudeOutput (MCP attach mining)
│   ├── codex/
│   │   └── mod.rs      invoke(): spawn codex (best-effort if installed)
│   ├── chat.rs         OpenAI-compatible HTTP fallback (no sandbox)
│   └── history.rs      per-user history.json (append, get, clear)
│
├── api/                iLink WeChat HTTP client
│   ├── client.rs       ILinkClient: get_updates, send_message, send_typing,
│   │                   get_upload_url, get_config, fetch_qr_code, ...
│   ├── headers.rs      common headers (channel version, UA, ...)
│   └── types.rs        WeixinMessage + request/response structs
│
├── auth/
│   └── accounts.rs     persisted WeixinAccount (token, base_url, user_id);
│                       per-account JSON files under ~/.weclawbot/accounts/
│
├── automation/
│   └── claude_cli.rs   OPERATOR-ONLY PTY automation for plugin install.
│                       Never reachable from WeChat path; HTTP admin only.
│
├── binding/
│   └── agent_map.rs    multi-tenant (WeChat user ⇄ bound bot account)
│
├── cli/                operator-facing subcommands
│   ├── start.rs        daemon entry; runs migrations + spawns monitors
│   ├── stop.rs / restart.rs
│   ├── login.rs        QR scan flow
│   ├── status.rs / doctor.rs
│   ├── config.rs       weclawbot config get/set <pointer> <value>
│   ├── ai_setup.rs     weclawbot ai --provider claude --model sonnet
│   ├── open_browser.rs weclawbot console (open http://127.0.0.1:18011)
│   ├── send.rs         debug: send a synthetic message
│   ├── users.rs        list/manage users
│   ├── update.rs
│   └── version.rs
│
├── daemon/
│   ├── log.rs          tracing-subscriber file logger
│   └── pid.rs          PID file mgmt (foreground vs background)
│
├── defaults.rs         factory settings template + migrate_existing_users
│
├── media/              attachment crypto + upload/download
│   ├── inbound.rs      resolve_message: download + decrypt user's media
│   ├── outbound.rs     send_text_then_file, send_text_then_url
│   ├── decrypt.rs / crypto AES helpers
│   └── upload.rs       CDN upload + encryption
│
├── monitor/            the message-handling pipeline
│   ├── mod.rs
│   ├── poller.rs       long-poll getUpdates per account
│   ├── handler.rs      ORCHESTRATOR (242 LOC) — wires everything together
│   ├── dedup.rs        SEEN_MSGS guard (10-min TTL)
│   ├── typing.rs       "对方正在输入" pulse + typing_ticket cache
│   ├── webhook.rs      operator webhook reply path
│   ├── reply.rs        build & send a text-only reply
│   └── forward.rs      forward MCP attach files + diff fallback + URLs
│
├── sandbox/            per-user gVisor lifecycle
│   ├── mod.rs          Sandbox::ensure(user_id) → per-user dirs/state
│   ├── exec.rs         build_claude_cmd / build_codex_cmd / preflight
│   ├── materialize.rs  sync user settings.json → sandbox + register MCP
│   └── layout.rs       on-disk path layout under ~/.weclawbot/
│
├── service/            HTTP admin GUI
│   ├── server.rs       axum Router
│   ├── routes.rs       /api/health, /api/users/{hash}/..., test inject, ...
│   ├── page.rs         serve assets/console.html
│   ├── state.rs        build_health, build_accounts_list
│   └── test_inject.rs  WECLAWBOT_TEST_MODE=1: synthetic inbound + capture
│
├── skills/
│   └── detect.rs       discover operator-installed Claude plugins/skills
│
├── storage/
│   ├── atomic_write.rs write_json_atomic (tmp + rename)
│   ├── json_path.rs    shared set_at / array_add / array_remove on Value
│   ├── state_dir.rs    ~/.weclawbot/ layout
│   └── sync_buf.rs
│
└── wechat_menu/        /menu console (WeChat-driven settings UI)
    ├── mod.rs          route() entry; returns ConsoleOutcome
    ├── session.rs      per-user mode + current_path, persisted to JSON
    ├── tree.rs         static command tree (Node / NodeKind)
    ├── dispatch.rs     state machine: input → reply + state update
    └── apply.rs        settings.json mutations + BLOCKED_KEYS guard
```

## Inbound message flow

```
WeChat user types something
        ↓
[iLink endpoint] getUpdates  ← weclawbot long-polls per account
        ↓
poller (one tokio task per account)
        ↓
handler::handle_inbound_message
   1. dedup::is_duplicate           → drop if seen recently
   2. Sandbox::ensure(user_id)      → init per-user state on first sight
   3. wechat_menu::route            → if /menu or already in menu mode,
                                       reply instantly via local logic,
                                       skip AI + typing entirely
   4. typing::start                 → pulse "对方正在输入" every 3s
                                       (fetches typing_ticket via get_config
                                       on first call, caches per user)
   5. resolve_message               → download + decrypt user's attachments
   6. forward::snapshot             → record /work/output/ before AI runs
   7. dispatch_reply                → pick provider by precedence:
        a. webhook (config.webhook.url non-empty)
        b. claude (config.ai.provider == claude, AI enabled)
        c. codex  (config.ai.provider == codex,  AI enabled)
        d. api    (config.ai.provider == api,    AI enabled + apiKey set)
        e. echo   (config.echo.enabled)
        f. None
      Returns: (text, generated_files, generated_urls, cli_succeeded)
   8. typing.stop                   → indicator fades on WeChat side
   9. reply::send_text              → text bubble back to WeChat
  10. forward::forward_mcp_files    → upload + send each attach()
  11. forward::forward_diff_fallback (only if cli_succeeded) → catch
                                       /work/output/ files Claude wrote
                                       but didn't declare via attach()
  12. forward::forward_urls         → download + forward attach_url()
```

## Sandbox lifecycle

Each inbound message spawns a fresh ephemeral container:

```
podman run --rm
    --userns=keep-id:uid=1000,gid=1000  ← so credentials.json (mode 600) is
                                          readable inside; rootless podman
                                          remapping would otherwise show
                                          everything as root
    --runtime=runsc                     ← gVisor user-space kernel; intercepts
                                          syscalls; ~100ms cold start; safe
                                          against kernel exploits inside the
                                          container
    --memory=2g --cpus=2 --pids-limit=512
    --read-only                         ← / is RO; only mounted dirs are RW
    --tmpfs /tmp --tmpfs /run
    --network=bridge
    -v <sandbox>/home:/home/claude:rw   ← per-user $HOME (claude state)
    -v <sandbox>/work:/work:rw          ← per-user cwd (where deliverables go)
    -v <sandbox>/media:/home/claude/media:ro  ← inbound files (read-only)
    -v ~/.claude/.credentials.json:.../.credentials.json:ro
    <image>
    <args>                              ← claude -p --output-format stream-json
```

Per-user state lives in `~/.weclawbot/users/<u-hash>/`:

```
sandbox/
├── home/.claude/         claude's $HOME — settings.json, plugins, projects/
├── home/.claude.json      MCP servers + oauth state (synced by materialize)
├── work/                 container cwd; /work/output/ is delivery dir
└── media/inbound/        inbound user attachments (read-only in container)
settings.json             canonical per-user Claude settings
profile.json              nickname, last_seen, msg_count
history.json              conversation history (cleared by /menu → new-chat)
console_session.json      /menu state machine: current_path + last_input_at
```

Image layout: see `build/Dockerfile`. Multi-stage:
1. `mcp-builder` (rust:1-bookworm) compiles the standalone
   `weclawbot-mcp` JSON-RPC binary
2. Final image (ubuntu:24.04) installs python3 + python-docx/openpyxl/
   pdfplumber + libreoffice + Claude CLI, COPYs in the MCP binary, sets
   up the unprivileged `claude` user

## MCP file capture

The sandbox image bakes `/usr/local/bin/weclawbot-mcp` — a minimal stdio
JSON-RPC server exposing two tools:

- `attach(path, caption?)` — declares a deliverable file
- `attach_url(url, caption?)` — declares a remote https URL to forward

`sandbox/materialize.rs` writes `<sandbox>/home/.claude.json` with:

```json
{
  "mcpServers": {
    "weclawbot": {
      "type": "stdio",
      "command": "/usr/local/bin/weclawbot-mcp",
      "args": [],
      "env": {}
    }
  }
}
```

When Claude calls `attach()`, the call surfaces in stream-json as a
`tool_use` event with name `mcp__weclawbot__attach`. The host's
`ai::claude::stream_json::process_event` mines those events and pushes
each path into `ClaudeOutput.generated_files`.

## Testing

| Layer | What | Where |
|---|---|---|
| Unit | newtype IDs (Debug redaction, transparent serde) | `src/ids.rs::tests` |
| Unit | typed Config (parse, defaults, migration, enum) | `src/config.rs::tests` |
| Unit | `/menu` dispatch (case-insensitive, breadcrumb) | `src/wechat_menu/dispatch.rs::tests` |
| Unit | `/menu` tree shape (no account commands leak) | `src/wechat_menu/tree.rs::tests` |
| Unit | JSON-path helpers (set_at, array_add, …) | `src/storage/json_path.rs::tests` |
| Unit | webhook dispatch (200 / 500 / no body / refused) | `src/monitor/webhook.rs::tests` (wiremock) |
| Unit | dedup TTL guard | `src/monitor/dedup.rs::tests` |
| Unit | stream-json parsing (MCP attach, text, result, …) | `src/ai/claude/stream_json.rs::tests` |
| Unit | prompt builder (WeChat role, /tmp, emoji rules) | `src/ai/claude/prompt.rs::tests` |
| Unit | defaults migration helpers (model_is_unset, …) | `src/defaults.rs::tests` |
| E2E  | 14 smoke cases over real Claude via test_inject | `scripts/smoke-e2e.sh` |

CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo clippy
--all-targets -D warnings`, `cargo test --release`, `cargo build --release`
on every push/PR. The E2E smoke needs a real Claude OAuth token so it's
gated behind `workflow_dispatch` (`.github/workflows/smoke.yml`).

## Boundary invariants

- **WeChat path NEVER spawns interactive claude.** The only spawn site
  in `crate::ai::claude::invoke` uses `-p --output-format stream-json`
  (non-interactive). The PTY in `crate::automation::claude_cli` is
  HTTP-admin-only (operator opted in).
- **WeChat path NEVER reaches account commands.** `wechat_menu/tree.rs`
  has no `login`, `logout`, `switch-account`, API-key fields. Defense in
  depth: `wechat_menu/apply.rs::BLOCKED_KEYS` rejects writes to
  `mcpServers.weclawbot`, `env.ANTHROPIC_API_KEY`, `oauthAccount`, etc.
- **Per-user isolation.** Each user has their own `Sandbox`, their own
  gVisor container, their own `settings.json` / `history.json`. The host
  enforces these via dir layout + per-message `Sandbox::ensure`.
- **Operator credentials are bind-mounted read-only.** A compromised
  in-container claude cannot tamper with `~/.claude/.credentials.json`.
