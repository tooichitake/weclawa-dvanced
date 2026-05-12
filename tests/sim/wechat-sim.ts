/**
 * Sandbox WeChat simulator.
 *
 * Pretends to be the WeChat ↔ OpenClaw monitor loop:
 *   - constructs a WeixinMsgContext for inbound text + optional file
 *   - drives the real CLI agent-bridge (claude-agent-sdk) end-to-end
 *   - prints what *would* be sent back to the WeChat user
 *
 * Usage:
 *   ANTHROPIC_BASE_URL=... \
 *   npx tsx tests/sim/wechat-sim.ts <scenario>
 *
 * Scenarios: text | image | pdf | docx | multi
 */

import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { resolveMoltMarketConfig } from "../../src/config.js";
import { runCliTurn } from "../../src/weixin/agent-bridge/cli-backend.js";
import type { CliBackendFile } from "../../src/weixin/agent-bridge/types.js";

const SCENARIO = (process.argv[2] ?? "text").toLowerCase();

function ensureStateDir(): string {
  const dir = path.join(os.tmpdir(), "weclaw-sim-state");
  fs.mkdirSync(dir, { recursive: true });
  process.env.OPENCLAW_STATE_DIR = dir;
  return dir;
}

function makeTextFile(name: string, body: string): string {
  const dir = path.join(os.tmpdir(), "weclaw-sim-files");
  fs.mkdirSync(dir, { recursive: true });
  const p = path.join(dir, name);
  fs.writeFileSync(p, body);
  return p;
}

function makePngStub(name: string): string {
  // 1x1 transparent PNG
  const png = Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=",
    "base64",
  );
  const dir = path.join(os.tmpdir(), "weclaw-sim-files");
  fs.mkdirSync(dir, { recursive: true });
  const p = path.join(dir, name);
  fs.writeFileSync(p, png);
  return p;
}

type Scenario = {
  label: string;
  text: string;
  files: CliBackendFile[];
};

function buildScenario(name: string): Scenario {
  switch (name) {
    case "text":
      return {
        label: "Plain text",
        text: "Hello agent. Reply with the literal phrase 'WECLAW-SIM-OK' and nothing else.",
        files: [],
      };
    case "image": {
      const p = makePngStub("smoke.png");
      return {
        label: "1x1 transparent PNG",
        text: "I sent you a tiny PNG. Reply in one short sentence describing what file you received (filename + format).",
        files: [{ path: p, mimeType: "image/png", fileName: "smoke.png" }],
      };
    }
    case "pdf": {
      const body = "%PDF-1.4\n% fake PDF for sandbox test\nWECLAW-SIM-MARKER: pdf-passthrough-ok\n";
      const p = makeTextFile("fake-doc.pdf", body);
      return {
        label: "fake PDF with marker",
        text: "Read the PDF attached and tell me the exact value of the WECLAW-SIM-MARKER line.",
        files: [{ path: p, mimeType: "application/pdf", fileName: "fake-doc.pdf" }],
      };
    }
    case "docx": {
      const body = "Project status\nWECLAW-SIM-DOCX-MARKER: docx-passthrough-ok\n";
      const p = makeTextFile("notes.docx", body);
      return {
        label: "fake DOCX (plain text body) — relies on Read tool",
        text: "Use the Read tool to read the attached file and quote the WECLAW-SIM-DOCX-MARKER line verbatim.",
        files: [
          {
            path: p,
            mimeType: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            fileName: "notes.docx",
          },
        ],
      };
    }
    case "multi":
      // Handled separately in main()
      return { label: "multi-turn", text: "", files: [] };
    default:
      throw new Error(`unknown scenario: ${name}`);
  }
}

async function runOneTurn(label: string, scope: string, text: string, files: CliBackendFile[]) {
  console.log("\n======================================");
  console.log(`▶ ${label}`);
  console.log(`  scope=${scope} files=${files.length} text="${text.slice(0, 80)}${text.length > 80 ? "…" : ""}"`);
  console.log("======================================");

  const cfg = resolveMoltMarketConfig({
    agent: {
      backend: "cli",
      cli: "claude",
      claude: {
        useAgentSdk: true,
        // Use the sandbox's system claude binary (already authenticated via OAuth).
        binaryPath: process.env.CLAUDE_BINARY_PATH ?? "/opt/claude-code/bin/claude",
      },
    },
  }).agent;

  const captured: { text: string[]; files: Array<{ path: string }>; errors: string[] } = {
    text: [],
    files: [],
    errors: [],
  };

  const start = Date.now();
  for await (const ev of runCliTurn(cfg, { sessionScope: scope, text, files })) {
    if (ev.kind === "text") {
      captured.text.push(ev.chunk);
      process.stdout.write(`\x1b[36m${ev.chunk}\x1b[0m`);
    } else if (ev.kind === "file") {
      captured.files.push({ path: ev.path });
      console.log(`\n[outbound file] ${ev.path} mime=${ev.mimeType ?? "?"} caption=${ev.caption ?? ""}`);
    } else if (ev.kind === "error") {
      captured.errors.push(ev.error.message);
      console.error(`\n[error] ${ev.error.message}`);
    } else if (ev.kind === "done") {
      // ignored
    }
  }
  console.log(`\n----- summary -----`);
  console.log(`elapsed=${Date.now() - start}ms textChunks=${captured.text.length} files=${captured.files.length} errors=${captured.errors.length}`);
  return captured;
}

async function main() {
  ensureStateDir();
  if (SCENARIO === "multi") {
    const scope = `sim:multi:${Date.now()}`;
    await runOneTurn("turn 1: tell a secret", scope, "Remember this secret word: PLUM-92. Confirm in one short sentence.", []);
    await runOneTurn("turn 2: recall the secret", scope, "What was the secret word I just told you?", []);
    return;
  }
  const sc = buildScenario(SCENARIO);
  const scope = `sim:${SCENARIO}:${Date.now()}`;
  await runOneTurn(sc.label, scope, sc.text, sc.files);
}

main().catch((err) => {
  console.error("sim crashed:", err);
  process.exit(1);
});
