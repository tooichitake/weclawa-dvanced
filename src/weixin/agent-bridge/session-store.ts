import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

import { resolveStateDir } from "../storage/state-dir.js";

const CHANNEL_ID = "clawbnb-weixin";
const STORE_VERSION = 1;
const LOCK_STALE_MS = 10_000;
const LOCK_RETRY_MS = 80;
const LOCK_MAX_RETRIES = 60;

export type SessionRecord = {
  sessionId: string;
  backend: string;
  cli: string;
  createdAt: string;
  updatedAt: string;
};

type SessionMap = {
  version: number;
  sessions: Record<string, SessionRecord>;
};

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function storePath(): string {
  return path.join(resolveStateDir(), CHANNEL_ID, "agent-sessions.json");
}

function readStore(): SessionMap {
  const file = storePath();
  try {
    if (!fs.existsSync(file)) {
      return { version: STORE_VERSION, sessions: {} };
    }
    const parsed = JSON.parse(fs.readFileSync(file, "utf-8")) as Partial<SessionMap>;
    if (!parsed || typeof parsed !== "object") {
      return { version: STORE_VERSION, sessions: {} };
    }
    return {
      version: STORE_VERSION,
      sessions: parsed.sessions && typeof parsed.sessions === "object" ? parsed.sessions : {},
    };
  } catch {
    return { version: STORE_VERSION, sessions: {} };
  }
}

function writeStore(map: SessionMap): void {
  const file = storePath();
  const dir = path.dirname(file);
  fs.mkdirSync(dir, { recursive: true });
  const tmp = path.join(
    dir,
    `.agent-sessions-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}.tmp`,
  );
  fs.writeFileSync(tmp, `${JSON.stringify(map, null, 2)}\n`, "utf-8");
  fs.renameSync(tmp, file);
}

async function withLock<T>(task: () => Promise<T> | T): Promise<T> {
  const lockFile = `${storePath()}.lock`;
  fs.mkdirSync(path.dirname(lockFile), { recursive: true });
  for (let attempt = 0; attempt < LOCK_MAX_RETRIES; attempt += 1) {
    try {
      const fd = fs.openSync(lockFile, "wx");
      fs.closeSync(fd);
      try {
        return await task();
      } finally {
        try {
          fs.unlinkSync(lockFile);
        } catch {
          // best-effort
        }
      }
    } catch (error) {
      const err = error as NodeJS.ErrnoException;
      if (err.code !== "EEXIST") throw error;
      try {
        const stat = fs.statSync(lockFile);
        if (Date.now() - stat.mtimeMs > LOCK_STALE_MS) {
          fs.unlinkSync(lockFile);
          continue;
        }
      } catch {
        // ignore — another process may have removed it
      }
      await sleep(LOCK_RETRY_MS);
    }
  }
  throw new Error(`failed to acquire agent-sessions lock: ${lockFile}`);
}

function recordExpired(rec: SessionRecord, ttlMs: number): boolean {
  if (ttlMs <= 0) return false;
  const last = Date.parse(rec.updatedAt);
  if (!Number.isFinite(last)) return true;
  return Date.now() - last > ttlMs;
}

export function generateSessionId(prefix = "wx"): string {
  return `${prefix}-${crypto.randomBytes(8).toString("hex")}`;
}

/** Look up or create a stable session id for the given scope. */
export async function getOrCreateSession(params: {
  scope: string;
  backend: string;
  cli: string;
  ttlMs: number;
}): Promise<{ record: SessionRecord; created: boolean }> {
  const { scope, backend, cli, ttlMs } = params;
  return withLock(() => {
    const map = readStore();
    const existing = map.sessions[scope];
    if (existing && existing.backend === backend && existing.cli === cli && !recordExpired(existing, ttlMs)) {
      existing.updatedAt = new Date().toISOString();
      map.sessions[scope] = existing;
      writeStore(map);
      return { record: existing, created: false };
    }
    const now = new Date().toISOString();
    const record: SessionRecord = {
      sessionId: generateSessionId(),
      backend,
      cli,
      createdAt: now,
      updatedAt: now,
    };
    map.sessions[scope] = record;
    writeStore(map);
    return { record, created: true };
  });
}

/**
 * Replace the stored sessionId for `scope` with one observed from a CLI run.
 * The SDK or subprocess may issue its own UUIDs that we should track for resume.
 */
export async function persistSessionId(scope: string, sessionId: string): Promise<void> {
  if (!sessionId) return;
  await withLock(() => {
    const map = readStore();
    const existing = map.sessions[scope];
    if (!existing) return;
    if (existing.sessionId === sessionId) return;
    existing.sessionId = sessionId;
    existing.updatedAt = new Date().toISOString();
    map.sessions[scope] = existing;
    writeStore(map);
  });
}

/** Drop a session record, e.g. after CLI crash. */
export async function dropSession(scope: string): Promise<void> {
  await withLock(() => {
    const map = readStore();
    if (map.sessions[scope]) {
      delete map.sessions[scope];
      writeStore(map);
    }
  });
}

/** Test-only: clear all sessions (used in unit tests). */
export async function _resetForTests(): Promise<void> {
  await withLock(() => {
    writeStore({ version: STORE_VERSION, sessions: {} });
  });
}
