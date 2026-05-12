# ClawBNB Hub

Repository rename note: this public repository was previously published as `WeClawBot-ex`.

`clawbnb-hub` is a standalone OpenClaw plugin that bundles:

- the `clawbnb-weixin` channel runtime
- the ClawBNB Hub local Weixin control console
- the rental relay and proxy provider used by the ClawBNB platform stack

## Breaking Release

This release is a clean cut from `molthuman-oc-plugin`.

- New package name: `clawbnb-hub`
- New plugin entry id: `clawbnb-hub`
- New channel id: `clawbnb-weixin`
- New state namespace: `<OPENCLAW_STATE_DIR>/clawbnb-weixin/`
- No automatic migration from `molthuman-oc-plugin`
- Do not run `molthuman-oc-plugin` and `clawbnb-hub` in the same OpenClaw profile

## Install

Requirements:

- OpenClaw `>=2026.3.22`
- Node `>=22.16.0` when installing from a GitHub checkout

From a GitHub checkout (recommended today):

```bash
cd clawbnb-hub
npm install
openclaw plugins install .
```

From a local release tarball:

```bash
cd clawbnb-hub
npm pack --cache ./.npm-cache
openclaw plugins install ./clawbnb-hub-<version>.tgz
```

Current npm status:

- `clawbnb-hub` is not yet published to npm
- do not use `openclaw plugins install clawbnb-hub` until a published npm release exists

First-install note:

- OpenClaw may warn about `dangerous code patterns` or `plugins.allow` trust when you install from source or a local artifact
- that warning is expected for this plugin because it reads env vars and exposes local HTTP callbacks for the control console and optional platform linking

## Migration

1. Disable or uninstall `molthuman-oc-plugin`.
2. Remove old config under `plugins.entries.molthuman-oc-plugin` and `channels.openclaw-weixin`.
3. Install `clawbnb-hub`.
4. Add the new config keys shown below.
5. Re-scan Weixin accounts if you want them in the new `clawbnb-weixin` state namespace.

## Config Contract

### Minimal "just works" setup

```bash
export ANTHROPIC_API_KEY=sk-ant-...        # or OPENAI_API_KEY for codex
npm install
openclaw plugins install .
```

The plugin defaults to `agent.backend = "cli"`, `agent.cli = "claude"`, and
reads the API key from the environment, so no extra config keys are required
under `plugins.entries.clawbnb-hub.config`. WeChat inbound text + files are
forwarded as-is to a claude-code (or codex) session; the CLI handles vision,
PDF, document parsing natively.

### Full config schema

Plugin-level settings stay under `plugins.entries.clawbnb-hub.config`:

```jsonc
{
  "plugins": {
    "entries": {
      "clawbnb-hub": {
        "enabled": true,
        "config": {
          "hostModelControl": "inherit",
          "agent": {
            "backend": "cli",            // "cli" (default) | "pi-ai" (legacy rental path)
            "cli": "claude",             // "claude" (default) | "codex"
            "sessionTimeoutMs": 600000,  // idle TTL for per-(account,user) CLI sessions
            "maxOutboundFiles": 8,       // safety cap on files the agent can emit per turn
            "claude": {
              "useAgentSdk": true,
              "binaryPath": "claude",            // path to an authenticated `claude` binary
              "model": "",                       // empty = SDK default
              "extraSystemPrompt": "",
              "anthropicApiKey": ""              // empty = read $ANTHROPIC_API_KEY
            },
            "codex": {
              "binaryPath": "codex",
              "openaiApiKey": ""                 // empty = read $OPENAI_API_KEY
            }
          },

          // Optional legacy rental-relay fields (only when running the
          // marketplace integration; safe to omit for the WeChat-only path).
          "apiKey": "YOUR_AGENT_API_KEY",
          "relayUrl": "ws://127.0.0.1:8787/ws/rental?role=plugin",
          "proxyBaseUrl": "http://127.0.0.1:8787/api/rental-proxy"
        }
      }
    }
  }
}
```

`hostModelControl` modes:

- `inherit` (default): do not rewrite the host OpenClaw model/provider config; use whatever local model stack the Gateway already has
- `proxy`: explicitly rewrite the host config to use the `molt-proxy` provider; requires an explicit `proxyBaseUrl`

`agent.backend` modes:

- `cli` (default): forward inbound text + decrypted file paths to the configured
  CLI (`claude-code` or `codex`). The plugin acts as a pure messenger — the CLI
  decides natively how to read images / PDFs / Office files. Rental sessions
  (any `sessionKey` prefixed with `clawbnb-hub:`) bypass this fork and still
  flow through the legacy dispatcher.
- `pi-ai`: original path; routes via OpenClaw's `dispatchReplyFromConfig` and
  the `molt-proxy` provider. Retained for the rental-relay use case.

Upgrade note:

- Older `clawbnb-hub` builds could inject `molt-proxy` settings into the host OpenClaw profile.
- Starting the updated plugin once with `hostModelControl: "inherit"` will scrub that old plugin-owned `molt-proxy` takeover state from the host profile.

Weixin channel settings stay under `channels.clawbnb-weixin`:

```json
{
  "channels": {
    "clawbnb-weixin": {
      "baseUrl": "https://ilinkai.weixin.qq.com",
      "cdnBaseUrl": "https://novac2c.cdn.weixin.qq.com/c2c",
      "demoService": {
        "enabled": true,
        "bind": "127.0.0.1",
        "port": 19120
      }
    }
  }
}
```

## Optional Integration

The plugin works without platform-side profile linking.

Optional integration points:

- `MOLT_APP_BASE_URL`: used by the local console when generating public profile links
- `INTERNAL_API_KEY`: used when calling platform-side profile binding endpoints
- `/api/accounts/link-agent`: local console helper for linking a Weixin account to a public profile via claim token

If you do not need public profile linking, you can ignore these settings entirely.

## Repository Layout

- `src/weixin/`: embedded Weixin runtime and local control console
- `src/`: rental relay and proxy provider implementation
- `tests/unit` and `tests/smoke`: Weixin regression coverage
- `docs/faq.md`: operator FAQ
- `docs/architecture.md`: routing and isolation model

## Quality Gate

Run these before publishing:

```bash
npm run typecheck
npm run test:unit
npm run test:smoke
npm pack --dry-run --cache ./.npm-cache
```

## Upstream

This project currently tracks `@tencent-weixin/openclaw-weixin@2.1.1` as the upstream runtime baseline.
Upstream-derived files remain intentionally constrained; first-party work should stay in the control console, packaging, and docs layers.

## License

MIT. See [LICENSE](./LICENSE) and [NOTICE](./NOTICE).
