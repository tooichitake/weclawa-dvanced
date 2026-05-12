import { EventEmitter, PassThrough } from "node:stream";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { createCodexAdapter, type CodexSpawnFn } from "../../src/weixin/agent-bridge/codex-sdk.js";
import { _resetForTests } from "../../src/weixin/agent-bridge/session-store.js";
import type { CliBackendEvent } from "../../src/weixin/agent-bridge/types.js";
import { createTempOpenClawEnv } from "../helpers/temp-env.js";

const baseCodexCfg = {
  binaryPath: "codex",
  openaiApiKey: "",
};

type FakeChild = EventEmitter & {
  stdin: PassThrough;
  stdout: PassThrough;
  stderr: PassThrough;
  kill: (signal?: NodeJS.Signals | number) => boolean;
};

function makeFakeChild(stdoutLines: string[], opts?: { exitCode?: number; stderr?: string }): FakeChild {
  const emitter = new EventEmitter() as FakeChild;
  emitter.stdin = new PassThrough();
  emitter.stdout = new PassThrough();
  emitter.stderr = new PassThrough();
  emitter.kill = vi.fn(() => true);
  setImmediate(() => {
    for (const line of stdoutLines) emitter.stdout.write(`${line}\n`);
    if (opts?.stderr) emitter.stderr.write(opts.stderr);
    emitter.stdout.end();
    emitter.stderr.end();
    setImmediate(() => emitter.emit("close", opts?.exitCode ?? 0));
  });
  return emitter;
}

describe("agent-bridge codex-sdk adapter", () => {
  let env: ReturnType<typeof createTempOpenClawEnv>;

  beforeEach(async () => {
    env = createTempOpenClawEnv();
    await _resetForTests();
  });

  afterEach(() => {
    env.cleanup();
  });

  it("spawns codex with the session args and yields text/file events", async () => {
    let captured: { command?: string; args?: string[]; env?: NodeJS.ProcessEnv } = {};
    const spawnFn: CodexSpawnFn = (command, args, options) => {
      captured = { command, args, env: options.env };
      return makeFakeChild([
        JSON.stringify({ type: "text", chunk: "Hello" }),
        JSON.stringify({ type: "file", path: "/tmp/out.pdf", fileName: "out.pdf", mimeType: "application/pdf" }),
        JSON.stringify({ type: "done" }),
      ]) as unknown as ReturnType<CodexSpawnFn>;
    };

    const adapter = createCodexAdapter({ ...baseCodexCfg, openaiApiKey: "sk-test" }, { spawnFn });
    const events: CliBackendEvent[] = [];
    for await (const ev of adapter.runTurn({
      sessionScope: "acc:user-c",
      text: "ping",
      files: [{ path: "/tmp/a.pdf", mimeType: "application/pdf", fileName: "a.pdf" }],
    })) {
      events.push(ev);
    }

    expect(captured.command).toBe("codex");
    expect(captured.args?.slice(0, 2)).toEqual(["exec", "--session"]);
    expect(captured.args?.[3]).toBe("--json");
    expect(captured.env?.OPENAI_API_KEY).toBe("sk-test");

    const textChunks = events.filter((e) => e.kind === "text");
    const fileEvents = events.filter((e) => e.kind === "file");
    const doneEvents = events.filter((e) => e.kind === "done");
    expect(textChunks).toHaveLength(1);
    expect(fileEvents).toHaveLength(1);
    expect(doneEvents).toHaveLength(1);
  });

  it("reports an error event when codex exits non-zero", async () => {
    const spawnFn: CodexSpawnFn = () =>
      makeFakeChild([], { exitCode: 1, stderr: "boom" }) as unknown as ReturnType<CodexSpawnFn>;
    const adapter = createCodexAdapter(baseCodexCfg, { spawnFn });
    const events: CliBackendEvent[] = [];
    for await (const ev of adapter.runTurn({ sessionScope: "acc:err", text: "x", files: [] })) {
      events.push(ev);
    }
    const error = events.find((e) => e.kind === "error");
    expect(error).toBeDefined();
    if (error?.kind === "error") expect(error.error.message).toMatch(/codex exited with code 1/);
  });
});
