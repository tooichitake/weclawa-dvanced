import type { ResolvedAgentConfig } from "../../config.js";
import { createClaudeAdapter } from "./claude-sdk.js";
import { createCodexAdapter } from "./codex-sdk.js";
import type { CliBackendAdapter, CliBackendEvent, CliBackendTurnInput } from "./types.js";

export type { CliBackendAdapter, CliBackendEvent, CliBackendTurnInput } from "./types.js";
export type { CliBackendFile } from "./types.js";
export { CliBackendError } from "./types.js";

/** Build the adapter for the configured CLI backend. */
export function createCliBackend(cfg: ResolvedAgentConfig): CliBackendAdapter {
  if (cfg.cli === "codex") {
    return createCodexAdapter(cfg.codex);
  }
  return createClaudeAdapter(cfg.claude);
}

/** Convenience: drive a turn against the configured CLI. */
export function runCliTurn(
  cfg: ResolvedAgentConfig,
  input: CliBackendTurnInput,
): AsyncIterable<CliBackendEvent> {
  return createCliBackend(cfg).runTurn(input);
}
