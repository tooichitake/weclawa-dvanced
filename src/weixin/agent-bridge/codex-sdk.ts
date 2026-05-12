import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import path from "node:path";
import readline from "node:readline";

import type { ResolvedAgentCodexConfig } from "../../config.js";
import { logger } from "../util/logger.js";
import { dropSession, getOrCreateSession } from "./session-store.js";
import type {
  CliBackendAdapter,
  CliBackendEvent,
  CliBackendFile,
  CliBackendTurnInput,
} from "./types.js";
import { CliBackendError } from "./types.js";

export type CodexSpawnFn = (
  command: string,
  args: string[],
  options: { env?: NodeJS.ProcessEnv },
) => ChildProcessWithoutNullStreams;

const defaultSpawn: CodexSpawnFn = (command, args, options) =>
  spawn(command, args, {
    env: options.env,
    stdio: ["pipe", "pipe", "pipe"],
  }) as ChildProcessWithoutNullStreams;

function renderPrompt(text: string, files: CliBackendFile[]): string {
  const lines: string[] = [];
  const body = text.trim();
  if (body) lines.push(body);
  for (const f of files) {
    const label = f.fileName || path.basename(f.path);
    lines.push(`@${f.path}${label && label !== path.basename(f.path) ? ` (${label})` : ""}`);
  }
  return lines.length > 0 ? lines.join("\n") : "(no content)";
}

function parseJsonLine(line: string): Record<string, unknown> | null {
  const trimmed = line.trim();
  if (!trimmed) return null;
  if (trimmed[0] !== "{" && trimmed[0] !== "[") return null;
  try {
    const parsed = JSON.parse(trimmed);
    return parsed && typeof parsed === "object" ? (parsed as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

export type CodexAdapterDeps = {
  spawnFn?: CodexSpawnFn;
};

export function createCodexAdapter(
  cfg: ResolvedAgentCodexConfig,
  deps: CodexAdapterDeps = {},
): CliBackendAdapter {
  const spawnFn = deps.spawnFn ?? defaultSpawn;
  return {
    runTurn(input: CliBackendTurnInput) {
      return runCodex(cfg, input, spawnFn);
    },
  };
}

async function* runCodex(
  cfg: ResolvedAgentCodexConfig,
  input: CliBackendTurnInput,
  spawnFn: CodexSpawnFn,
): AsyncGenerator<CliBackendEvent> {
  const session = await getOrCreateSession({
    scope: input.sessionScope,
    backend: "cli",
    cli: "codex",
    ttlMs: 0,
  });

  const prompt = renderPrompt(input.text, input.files);
  const args = ["exec", "--session", session.record.sessionId, "--json"];
  const env: NodeJS.ProcessEnv = { ...process.env };
  if (cfg.openaiApiKey) env.OPENAI_API_KEY = cfg.openaiApiKey;

  logger.info(
    `[agent-bridge:codex] spawn binary=${cfg.binaryPath} session=${session.record.sessionId} created=${session.created} files=${input.files.length}`,
  );

  let child: ChildProcessWithoutNullStreams;
  try {
    child = spawnFn(cfg.binaryPath, args, { env });
  } catch (err) {
    yield { kind: "error", error: new CliBackendError(`failed to spawn codex: ${String(err)}`, err) };
    return;
  }

  const onAbort = () => {
    try {
      child.kill("SIGTERM");
    } catch {
      // ignore
    }
  };
  if (input.signal) {
    if (input.signal.aborted) onAbort();
    else input.signal.addEventListener("abort", onAbort, { once: true });
  }

  try {
    child.stdin.end(`${prompt}\n`);
  } catch (err) {
    yield { kind: "error", error: new CliBackendError(`failed to write codex stdin: ${String(err)}`, err) };
    return;
  }

  const rl = readline.createInterface({ input: child.stdout, crlfDelay: Infinity });
  const errChunks: string[] = [];
  child.stderr.on("data", (buf: Buffer) => {
    errChunks.push(buf.toString("utf-8"));
  });

  try {
    for await (const line of rl) {
      if (input.signal?.aborted) break;
      const event = parseJsonLine(line);
      if (!event) continue;
      const type = typeof event.type === "string" ? event.type : "";
      if (type === "text" && typeof event.chunk === "string") {
        yield { kind: "text", chunk: event.chunk };
        continue;
      }
      if (type === "message" && typeof event.content === "string") {
        yield { kind: "text", chunk: event.content };
        continue;
      }
      if (type === "file" && typeof event.path === "string") {
        yield {
          kind: "file",
          path: event.path,
          mimeType: typeof event.mimeType === "string" ? event.mimeType : undefined,
          fileName: typeof event.fileName === "string" ? event.fileName : undefined,
          caption: typeof event.caption === "string" ? event.caption : undefined,
        };
        continue;
      }
      if (type === "done" || type === "result") break;
    }

    const exitCode: number = await new Promise((resolve) => {
      child.once("close", (code) => resolve(typeof code === "number" ? code : 0));
    });

    if (exitCode !== 0) {
      const stderrText = errChunks.join("").trim();
      await dropSession(input.sessionScope).catch(() => {});
      yield {
        kind: "error",
        error: new CliBackendError(
          `codex exited with code ${exitCode}${stderrText ? `: ${stderrText.slice(0, 400)}` : ""}`,
        ),
      };
      return;
    }
    yield { kind: "done" };
  } catch (err) {
    await dropSession(input.sessionScope).catch(() => {});
    yield { kind: "error", error: err instanceof Error ? err : new CliBackendError(String(err), err) };
  } finally {
    if (input.signal) input.signal.removeEventListener("abort", onAbort);
    rl.close();
  }
}
