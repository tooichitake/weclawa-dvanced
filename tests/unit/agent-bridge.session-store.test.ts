import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  _resetForTests,
  dropSession,
  getOrCreateSession,
  persistSessionId,
} from "../../src/weixin/agent-bridge/session-store.js";
import { createTempOpenClawEnv } from "../helpers/temp-env.js";

describe("agent-bridge session store", () => {
  let env: ReturnType<typeof createTempOpenClawEnv>;

  beforeEach(async () => {
    env = createTempOpenClawEnv();
    await _resetForTests();
  });

  afterEach(() => {
    env.cleanup();
  });

  it("reuses the same session for repeated scopes", async () => {
    const first = await getOrCreateSession({
      scope: "acc-A:user-1",
      backend: "cli",
      cli: "claude",
      ttlMs: 0,
    });
    expect(first.created).toBe(true);
    const second = await getOrCreateSession({
      scope: "acc-A:user-1",
      backend: "cli",
      cli: "claude",
      ttlMs: 0,
    });
    expect(second.created).toBe(false);
    expect(second.record.sessionId).toBe(first.record.sessionId);
  });

  it("isolates different scopes", async () => {
    const a = await getOrCreateSession({ scope: "acc:user-A", backend: "cli", cli: "claude", ttlMs: 0 });
    const b = await getOrCreateSession({ scope: "acc:user-B", backend: "cli", cli: "claude", ttlMs: 0 });
    expect(a.record.sessionId).not.toBe(b.record.sessionId);
  });

  it("creates a fresh record when backend or cli changes", async () => {
    const claude = await getOrCreateSession({ scope: "acc:user", backend: "cli", cli: "claude", ttlMs: 0 });
    const codex = await getOrCreateSession({ scope: "acc:user", backend: "cli", cli: "codex", ttlMs: 0 });
    expect(codex.created).toBe(true);
    expect(codex.record.sessionId).not.toBe(claude.record.sessionId);
  });

  it("persists an externally observed session id", async () => {
    const initial = await getOrCreateSession({ scope: "x", backend: "cli", cli: "claude", ttlMs: 0 });
    await persistSessionId("x", "external-uuid-123");
    const reloaded = await getOrCreateSession({ scope: "x", backend: "cli", cli: "claude", ttlMs: 0 });
    expect(reloaded.record.sessionId).toBe("external-uuid-123");
    expect(reloaded.record.sessionId).not.toBe(initial.record.sessionId);
  });

  it("dropSession clears the record so the next call creates a new one", async () => {
    const first = await getOrCreateSession({ scope: "y", backend: "cli", cli: "claude", ttlMs: 0 });
    await dropSession("y");
    const second = await getOrCreateSession({ scope: "y", backend: "cli", cli: "claude", ttlMs: 0 });
    expect(second.created).toBe(true);
    expect(second.record.sessionId).not.toBe(first.record.sessionId);
  });
});
