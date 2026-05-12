import fs from "node:fs";
import path from "node:path";

import type { WeixinApiOptions } from "../api/api.js";
import { getMimeFromFilename } from "../media/mime.js";
import type { WeixinMsgContext } from "../messaging/inbound.js";
import { sendWeixinErrorNotice } from "../messaging/error-notice.js";
import { sendWeixinMediaFile } from "../messaging/send-media.js";
import { sendMessageWeixin } from "../messaging/send.js";
import { logger } from "../util/logger.js";
import { redactToken } from "../util/redact.js";
import type { ResolvedAgentConfig } from "../../config.js";
import { runCliTurn } from "./cli-backend.js";
import type { CliBackendEvent, CliBackendFile } from "./types.js";

export type RunCliTurnAndDeliverDeps = {
  accountId: string;
  baseUrl: string;
  cdnBaseUrl: string;
  token?: string;
  contextToken?: string;
  log: (msg: string) => void;
  errLog: (msg: string) => void;
};

function buildFilesFromCtx(ctx: WeixinMsgContext): CliBackendFile[] {
  if (!ctx.MediaPath) return [];
  let size: number | undefined;
  try {
    size = fs.statSync(ctx.MediaPath).size;
  } catch {
    size = undefined;
  }
  const fileName = path.basename(ctx.MediaPath);
  const mimeFromCtx = ctx.MediaType && ctx.MediaType !== "image/*" ? ctx.MediaType : undefined;
  return [
    {
      path: ctx.MediaPath,
      mimeType: mimeFromCtx ?? getMimeFromFilename(fileName),
      fileName,
      sizeBytes: size,
    },
  ];
}

/**
 * Drive one CLI turn for the given inbound context, mirroring outputs back to
 * the WeChat peer via existing send / send-media helpers.
 * Plugin acts as pure messenger — text passes through verbatim (no markdown
 * sanitization), and files emitted by the CLI go through sendWeixinMediaFile.
 */
export async function runCliTurnAndDeliver(params: {
  ctx: WeixinMsgContext;
  cfg: ResolvedAgentConfig;
  deps: RunCliTurnAndDeliverDeps;
  signal?: AbortSignal;
}): Promise<void> {
  const { ctx, cfg, deps, signal } = params;
  const sessionScope = `${deps.accountId}:${ctx.From || ctx.To}`;
  const files = buildFilesFromCtx(ctx);
  const text = (ctx.Body ?? "").trim();

  logger.info(
    `[agent-bridge] dispatch scope=${sessionScope} cli=${cfg.cli} files=${files.length} bodyLen=${text.length}`,
  );

  const buffer: string[] = [];
  const filesSent: string[] = [];
  const errors: Error[] = [];
  let outboundFiles = 0;

  const flushTextIfAny = async (): Promise<void> => {
    const text = buffer.join("").trim();
    if (!text) return;
    buffer.length = 0;
    try {
      await sendMessageWeixin({
        to: ctx.To,
        text,
        opts: {
          baseUrl: deps.baseUrl,
          token: deps.token,
          contextToken: deps.contextToken,
        },
      });
      logger.info(`[agent-bridge] text -> ${ctx.To} contextToken=${redactToken(deps.contextToken)} len=${text.length}`);
    } catch (err) {
      errors.push(err instanceof Error ? err : new Error(String(err)));
      deps.errLog(`[agent-bridge] text send failed: ${String(err)}`);
    }
  };

  const opts: WeixinApiOptions & { contextToken?: string } = {
    baseUrl: deps.baseUrl,
    token: deps.token,
    contextToken: deps.contextToken,
  };

  try {
    for await (const event of runCliTurn(cfg, { sessionScope, text, files, signal })) {
      if (signal?.aborted) break;
      await handleEvent(event);
    }
  } catch (err) {
    errors.push(err instanceof Error ? err : new Error(String(err)));
    deps.errLog(`[agent-bridge] runCliTurn threw: ${String(err)}`);
  }

  await flushTextIfAny();

  if (errors.length > 0) {
    const first = errors[0];
    void sendWeixinErrorNotice({
      to: ctx.To,
      contextToken: deps.contextToken,
      message: `⚠️ Agent 不可用：${first.message}`,
      baseUrl: deps.baseUrl,
      token: deps.token,
      errLog: deps.errLog,
    });
  }

  async function handleEvent(event: CliBackendEvent): Promise<void> {
    switch (event.kind) {
      case "text":
        buffer.push(event.chunk);
        return;
      case "file": {
        await flushTextIfAny();
        if (cfg.maxOutboundFiles > 0 && outboundFiles >= cfg.maxOutboundFiles) {
          logger.warn(
            `[agent-bridge] outbound file cap reached (${cfg.maxOutboundFiles}); dropping path=${event.path}`,
          );
          return;
        }
        if (!event.path || !fs.existsSync(event.path)) {
          logger.warn(`[agent-bridge] outbound file missing on disk, skipping path=${event.path}`);
          return;
        }
        try {
          await sendWeixinMediaFile({
            filePath: event.path,
            to: ctx.To,
            text: event.caption ?? "",
            opts,
            cdnBaseUrl: deps.cdnBaseUrl,
          });
          filesSent.push(event.path);
          outboundFiles += 1;
          logger.info(`[agent-bridge] file -> ${ctx.To} path=${event.path}`);
        } catch (err) {
          errors.push(err instanceof Error ? err : new Error(String(err)));
          deps.errLog(`[agent-bridge] file send failed path=${event.path} err=${String(err)}`);
        }
        return;
      }
      case "error":
        errors.push(event.error);
        deps.errLog(`[agent-bridge] CLI error: ${event.error.message}`);
        return;
      case "done":
        return;
      default:
        return;
    }
  }
}
