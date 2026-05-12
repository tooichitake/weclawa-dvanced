import { describe, expect, it, beforeEach, afterEach } from "vitest";

import { resolveMoltMarketConfig } from "../../src/config.js";

describe("clawbnb-hub agent config", () => {
  const previousEnv = {
    ANTHROPIC_API_KEY: process.env.ANTHROPIC_API_KEY,
    OPENAI_API_KEY: process.env.OPENAI_API_KEY,
    CLAUDE_API_KEY: process.env.CLAUDE_API_KEY,
  };

  beforeEach(() => {
    delete process.env.ANTHROPIC_API_KEY;
    delete process.env.OPENAI_API_KEY;
    delete process.env.CLAUDE_API_KEY;
  });

  afterEach(() => {
    for (const [k, v] of Object.entries(previousEnv)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  });

  it("defaults the backend to cli/claude with sensible knobs", () => {
    const resolved = resolveMoltMarketConfig({});
    expect(resolved.agent.backend).toBe("cli");
    expect(resolved.agent.cli).toBe("claude");
    expect(resolved.agent.sessionTimeoutMs).toBeGreaterThanOrEqual(5_000);
    expect(resolved.agent.claude.useAgentSdk).toBe(true);
    expect(resolved.agent.codex.binaryPath).toBe("codex");
  });

  it("honours user overrides without dropping unknown fields", () => {
    const resolved = resolveMoltMarketConfig({
      agent: {
        backend: "pi-ai",
        cli: "codex",
        sessionTimeoutMs: 30_000,
        maxOutboundFiles: 2,
        claude: { model: "claude-opus-4-7", anthropicApiKey: "sk-explicit" },
        codex: { binaryPath: "/usr/local/bin/codex" },
      },
    });
    expect(resolved.agent.backend).toBe("pi-ai");
    expect(resolved.agent.cli).toBe("codex");
    expect(resolved.agent.sessionTimeoutMs).toBe(30_000);
    expect(resolved.agent.maxOutboundFiles).toBe(2);
    expect(resolved.agent.claude.model).toBe("claude-opus-4-7");
    expect(resolved.agent.claude.anthropicApiKey).toBe("sk-explicit");
    expect(resolved.agent.codex.binaryPath).toBe("/usr/local/bin/codex");
  });

  it("falls back to environment keys when config omits them", () => {
    process.env.ANTHROPIC_API_KEY = "sk-env-anthropic";
    process.env.OPENAI_API_KEY = "sk-env-openai";
    const resolved = resolveMoltMarketConfig({});
    expect(resolved.agent.claude.anthropicApiKey).toBe("sk-env-anthropic");
    expect(resolved.agent.codex.openaiApiKey).toBe("sk-env-openai");
  });
});
