/**
 * Shared types for the CLI agent-bridge.
 * Plugin acts as a pure messenger: WeChat ⇄ claude-code / codex CLI.
 * Files are forwarded as opaque local paths — no MIME-based discrimination.
 */

export type CliBackendFile = {
  /** Absolute local path of a decrypted inbound file. */
  path: string;
  /** Best-effort MIME guess; may be "application/octet-stream". */
  mimeType: string;
  /** Original filename when known, else basename of `path`. */
  fileName: string;
  /** File size in bytes, when stat succeeded; otherwise undefined. */
  sizeBytes?: number;
};

export type CliBackendTurnInput = {
  /** Stable bucket key for session reuse, e.g. `${accountId}:${userId}`. */
  sessionScope: string;
  /** User-visible text body, possibly empty when the user sent only a file. */
  text: string;
  files: CliBackendFile[];
  /** Optional cancellation. */
  signal?: AbortSignal;
};

export type CliBackendEvent =
  | { kind: "text"; chunk: string }
  | { kind: "file"; path: string; mimeType?: string; fileName?: string; caption?: string }
  | { kind: "error"; error: Error }
  | { kind: "done" };

export type CliBackendAdapter = {
  runTurn: (input: CliBackendTurnInput) => AsyncIterable<CliBackendEvent>;
};

export class CliBackendError extends Error {
  constructor(
    message: string,
    public readonly cause?: unknown,
  ) {
    super(message);
    this.name = "CliBackendError";
  }
}
