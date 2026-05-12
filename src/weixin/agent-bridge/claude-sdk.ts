import path from "node:path";

import type { ResolvedAgentClaudeConfig } from "../../config.js";
import { logger } from "../util/logger.js";
import { dropSession, getOrCreateSession, persistSessionId } from "./session-store.js";
import type {
  CliBackendAdapter,
  CliBackendEvent,
  CliBackendTurnInput,
  CliBackendFile,
} from "./types.js";
import { CliBackendError } from "./types.js";

/**
 * Loosely typed mirror of @anthropic-ai/claude-agent-sdk to keep this adapter
 * resilient to non-breaking SDK changes — we only use a small surface.
 */
type ClaudeAgentSdkModule = {
  query: (params: {
    prompt: string | AsyncIterable<unknown>;
    options?: Record<string, unknown>;
  }) => AsyncIterable<Record<string, unknown>>;
};

let cachedSdk: ClaudeAgentSdkModule | null | undefined;

async function loadAgentSdk(): Promise<ClaudeAgentSdkModule | null> {
  if (cachedSdk !== undefined) return cachedSdk;
  try {
    const mod = (await import("@anthropic-ai/claude-agent-sdk")) as unknown as ClaudeAgentSdkModule;
    if (typeof mod?.query !== "function") {
      cachedSdk = null;
      return null;
    }
    cachedSdk = mod;
    return mod;
  } catch (err) {
    logger.warn(`[agent-bridge] @anthropic-ai/claude-agent-sdk not available: ${String(err)}`);
    cachedSdk = null;
    return null;
  }
}

/**
 * Render a CLI-style user prompt: text body + @-references for any files.
 * Matches how a human types "@/path/to/file" in claude-code / codex CLI,
 * so the agent decides natively how to consume each path (vision, PDF reader,
 * Read tool for office docs, etc.).
 */
export function renderCliPrompt(text: string, files: CliBackendFile[]): string {
  const lines: string[] = [];
  const body = text.trim();
  if (body) lines.push(body);
  for (const f of files) {
    const label = f.fileName || path.basename(f.path);
    const basename = path.basename(f.path);
    lines.push(`@${f.path}${label && label !== basename ? ` (${label})` : ""}`);
  }
  return lines.length > 0 ? lines.join("\n") : "(no content)";
}

function coerceTextFromContent(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((part) => {
      if (!part || typeof part !== "object") return "";
      const obj = part as Record<string, unknown>;
      if (obj.type === "text" && typeof obj.text === "string") return obj.text;
      return "";
    })
    .join("");
}

function extractAttachments(record: Record<string, unknown>): Array<{
  path: string;
  fileName?: string;
  mimeType?: string;
  caption?: string;
}> {
  const out: Array<{ path: string; fileName?: string; mimeType?: string; caption?: string }> = [];
  const att = record.attachments;
  if (Array.isArray(att)) {
    for (const entry of att) {
      if (entry && typeof entry === "object") {
        const e = entry as Record<string, unknown>;
        const p = typeof e.path === "string" ? e.path : undefined;
        if (!p) continue;
        out.push({
          path: p,
          fileName: typeof e.fileName === "string" ? e.fileName : undefined,
          mimeType: typeof e.mimeType === "string" ? e.mimeType : undefined,
          caption: typeof e.caption === "string" ? e.caption : undefined,
        });
      }
    }
  }
  return out;
}

export function createClaudeAdapter(cfg: ResolvedAgentClaudeConfig): CliBackendAdapter {
  return {
    runTurn(input: CliBackendTurnInput) {
      return runQuery(cfg, input);
    },
  };
}

async function* runQuery(
  cfg: ResolvedAgentClaudeConfig,
  input: CliBackendTurnInput,
): AsyncGenerator<CliBackendEvent> {
  const sdk = await loadAgentSdk();
  if (!sdk) {
    yield {
      kind: "error",
      error: new CliBackendError(
        "@anthropic-ai/claude-agent-sdk is not installed; run `npm install @anthropic-ai/claude-agent-sdk` or set agent.cli=\"codex\"",
      ),
    };
    return;
  }

  const session = await getOrCreateSession({
    scope: input.sessionScope,
    backend: "cli",
    cli: "claude",
    ttlMs: 0,
  });

  const prompt = renderCliPrompt(input.text, input.files);
  const abortController = new AbortController();
  if (input.signal) {
    if (input.signal.aborted) abortController.abort();
    else input.signal.addEventListener("abort", () => abortController.abort(), { once: true });
  }

  const env: Record<string, string | undefined> = { ...process.env };
  if (cfg.anthropicApiKey) env.ANTHROPIC_API_KEY = cfg.anthropicApiKey;

  const options: Record<string, unknown> = {
    abortController,
    env,
  };
  if (cfg.model) options.model = cfg.model;
  if (cfg.extraSystemPrompt) options.systemPrompt = cfg.extraSystemPrompt;
  if (!session.created && session.record.sessionId) {
    options.resume = session.record.sessionId;
  }

  logger.info(
    `[agent-bridge:claude] runTurn scope=${input.sessionScope} session=${session.record.sessionId} created=${session.created} files=${input.files.length} promptLen=${prompt.length}`,
  );

  let observedSessionId: string | undefined;
  try {
    const stream = sdk.query({ prompt, options });
    for await (const raw of stream) {
      if (input.signal?.aborted) return;
      if (!raw || typeof raw !== "object") continue;
      const msg = raw as Record<string, unknown>;
      const type = typeof msg.type === "string" ? msg.type : "";
      if (typeof msg.session_id === "string" && !observedSessionId) {
        observedSessionId = msg.session_id;
      }

      if (type === "assistant") {
        const apiMessage =
          msg.message && typeof msg.message === "object"
            ? (msg.message as Record<string, unknown>)
            : undefined;
        const chunk = coerceTextFromContent(apiMessage?.content ?? msg.content);
        if (chunk) yield { kind: "text", chunk };
        for (const att of extractAttachments(msg)) {
          yield { kind: "file", path: att.path, mimeType: att.mimeType, fileName: att.fileName, caption: att.caption };
        }
        continue;
      }

      if (type === "result") {
        const resultText = typeof msg.result === "string" ? msg.result : "";
        if (resultText) yield { kind: "text", chunk: resultText };
        for (const att of extractAttachments(msg)) {
          yield { kind: "file", path: att.path, mimeType: att.mimeType, fileName: att.fileName, caption: att.caption };
        }
        break;
      }
    }
    if (observedSessionId && observedSessionId !== session.record.sessionId) {
      await persistSessionId(input.sessionScope, observedSessionId).catch(() => {});
    }
    yield { kind: "done" };
  } catch (err) {
    await dropSession(input.sessionScope).catch(() => {});
    yield {
      kind: "error",
      error: err instanceof Error ? err : new CliBackendError(String(err), err),
    };
  }
}

/** Test-only: reset the cached SDK module so unit tests can re-mock. */
export function _resetSdkCacheForTests(): void {
  cachedSdk = undefined;
}

/** Test-only: inject an SDK shim so we don't have to spawn the real CLI. */
export function _setSdkForTests(mock: ClaudeAgentSdkModule | null): void {
  cachedSdk = mock;
}
