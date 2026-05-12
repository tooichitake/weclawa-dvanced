import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  _resetSdkCacheForTests,
  _setSdkForTests,
  createClaudeAdapter,
  renderCliPrompt,
} from "../../src/weixin/agent-bridge/claude-sdk.js";
import { _resetForTests, getOrCreateSession } from "../../src/weixin/agent-bridge/session-store.js";
import type { CliBackendEvent } from "../../src/weixin/agent-bridge/types.js";
import { createTempOpenClawEnv } from "../helpers/temp-env.js";

const baseClaudeCfg = {
  useAgentSdk: true,
  binaryPath: "claude",
  model: "",
  extraSystemPrompt: "",
  anthropicApiKey: "",
};

describe("agent-bridge claude-sdk adapter", () => {
  let env: ReturnType<typeof createTempOpenClawEnv>;

  beforeEach(async () => {
    env = createTempOpenClawEnv();
    await _resetForTests();
    _resetSdkCacheForTests();
  });

  afterEach(() => {
    _resetSdkCacheForTests();
    env.cleanup();
  });

  it("renderCliPrompt builds a CLI-style prompt with @file references", () => {
    const prompt = renderCliPrompt("please summarize", [
      { path: "/tmp/x.pdf", mimeType: "application/pdf", fileName: "x.pdf" },
      { path: "/tmp/y.png", mimeType: "image/png", fileName: "screenshot.png" },
    ]);
    expect(prompt).toContain("please summarize");
    expect(prompt).toContain("@/tmp/x.pdf");
    expect(prompt).toContain("@/tmp/y.png (screenshot.png)");
  });

  it("renderCliPrompt falls back to '(no content)' when text + files are empty", () => {
    expect(renderCliPrompt("", [])).toBe("(no content)");
  });

  it("streams text from assistant + result messages and emits a done event", async () => {
    const calls: Array<Record<string, unknown>> = [];
    _setSdkForTests({
      query: (params) => {
        calls.push(params);
        async function* gen(): AsyncGenerator<Record<string, unknown>> {
          yield {
            type: "assistant",
            session_id: "session-AAA",
            message: { content: [{ type: "text", text: "Hello " }] },
          };
          yield {
            type: "assistant",
            session_id: "session-AAA",
            message: { content: [{ type: "text", text: "world" }] },
            attachments: [{ path: "/tmp/out.pdf", fileName: "out.pdf", mimeType: "application/pdf" }],
          };
          yield { type: "result", subtype: "success", session_id: "session-AAA", result: "" };
        }
        return gen();
      },
    });
    const adapter = createClaudeAdapter(baseClaudeCfg);

    const events: CliBackendEvent[] = [];
    for await (const e of adapter.runTurn({
      sessionScope: "acc:user-1",
      text: "ping",
      files: [{ path: "/tmp/a.pdf", mimeType: "application/pdf", fileName: "a.pdf" }],
    })) {
      events.push(e);
    }

    expect(events).toEqual([
      { kind: "text", chunk: "Hello " },
      { kind: "text", chunk: "world" },
      { kind: "file", path: "/tmp/out.pdf", mimeType: "application/pdf", fileName: "out.pdf", caption: undefined },
      { kind: "done" },
    ]);
    expect(calls).toHaveLength(1);
    const promptCall = calls[0];
    expect(typeof promptCall.prompt).toBe("string");
    expect(promptCall.prompt).toMatch(/@\/tmp\/a\.pdf/);
  });

  it("yields error event and drops the session when the SDK throws", async () => {
    _setSdkForTests({
      query: () => {
        async function* gen(): AsyncGenerator<Record<string, unknown>> {
          throw new Error("nope");
          // eslint-disable-next-line no-unreachable
          yield {};
        }
        return gen();
      },
    });
    const adapter = createClaudeAdapter(baseClaudeCfg);
    const events: CliBackendEvent[] = [];
    for await (const e of adapter.runTurn({
      sessionScope: "acc:user-error",
      text: "ping",
      files: [],
    })) {
      events.push(e);
    }
    expect(events).toHaveLength(1);
    expect(events[0]?.kind).toBe("error");
  });

  it("passes resume id on subsequent turns for the same scope", async () => {
    const seenOptions: Array<Record<string, unknown>> = [];
    _setSdkForTests({
      query: (params) => {
        seenOptions.push((params.options ?? {}) as Record<string, unknown>);
        async function* gen(): AsyncGenerator<Record<string, unknown>> {
          yield {
            type: "assistant",
            session_id: "session-RESUME",
            message: { content: [{ type: "text", text: "ok" }] },
          };
          yield { type: "result", subtype: "success", session_id: "session-RESUME", result: "" };
        }
        return gen();
      },
    });
    const adapter = createClaudeAdapter(baseClaudeCfg);

    for await (const _e of adapter.runTurn({ sessionScope: "acc:user-resume", text: "first", files: [] })) {
      void _e;
    }
    for await (const _e of adapter.runTurn({ sessionScope: "acc:user-resume", text: "second", files: [] })) {
      void _e;
    }

    expect(seenOptions[0].resume).toBeUndefined();
    expect(seenOptions[1].resume).toBe("session-RESUME");
  });

  it("emits a friendly error when the SDK is not installed", async () => {
    _setSdkForTests(null);
    const adapter = createClaudeAdapter(baseClaudeCfg);
    const events: CliBackendEvent[] = [];
    for await (const e of adapter.runTurn({ sessionScope: "acc:nosdk", text: "x", files: [] })) {
      events.push(e);
    }
    expect(events).toHaveLength(1);
    expect(events[0]?.kind).toBe("error");
    if (events[0]?.kind === "error") {
      expect(events[0].error.message).toMatch(/claude-agent-sdk is not installed/);
    }
    // No session record should be allocated when the SDK is unavailable.
    const sess = await getOrCreateSession({ scope: "acc:nosdk", backend: "cli", cli: "claude", ttlMs: 0 });
    expect(sess.created).toBe(true);
  });
});
