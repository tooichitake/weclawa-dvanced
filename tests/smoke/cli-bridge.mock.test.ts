import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const sendMessageWeixinMock = vi.hoisted(() => vi.fn(async () => undefined));
const sendWeixinMediaFileMock = vi.hoisted(() =>
  vi.fn(async () => ({ messageId: "fake-msg" })),
);
const sendWeixinErrorNoticeMock = vi.hoisted(() => vi.fn(async () => undefined));

vi.mock("../../src/weixin/messaging/send.js", async (importOriginal) => {
  const actual = (await importOriginal()) as Record<string, unknown>;
  return { ...actual, sendMessageWeixin: sendMessageWeixinMock };
});
vi.mock("../../src/weixin/messaging/send-media.js", () => ({
  sendWeixinMediaFile: sendWeixinMediaFileMock,
}));
vi.mock("../../src/weixin/messaging/error-notice.js", () => ({
  sendWeixinErrorNotice: sendWeixinErrorNoticeMock,
}));

import { runCliTurnAndDeliver } from "../../src/weixin/agent-bridge/deliver.js";
import {
  _resetForTests as resetSessionStore,
} from "../../src/weixin/agent-bridge/session-store.js";
import {
  _resetSdkCacheForTests,
  _setSdkForTests,
} from "../../src/weixin/agent-bridge/claude-sdk.js";
import type { WeixinMsgContext } from "../../src/weixin/messaging/inbound.js";
import { resolveMoltMarketConfig } from "../../src/config.js";
import { createTempOpenClawEnv } from "../helpers/temp-env.js";

const previousAnthropic = process.env.ANTHROPIC_API_KEY;

describe("CLI bridge smoke: inbound text+file → outbound text+media", () => {
  let env: ReturnType<typeof createTempOpenClawEnv>;
  let tmpInboundFile: string;
  let tmpOutboundFile: string;

  beforeEach(async () => {
    env = createTempOpenClawEnv();
    await resetSessionStore();
    _resetSdkCacheForTests();
    sendMessageWeixinMock.mockClear();
    sendWeixinMediaFileMock.mockClear();
    sendWeixinErrorNoticeMock.mockClear();
    delete process.env.ANTHROPIC_API_KEY;
    tmpInboundFile = path.join(os.tmpdir(), `weclaw-inbound-${Date.now()}.pdf`);
    tmpOutboundFile = path.join(os.tmpdir(), `weclaw-outbound-${Date.now()}.png`);
    fs.writeFileSync(tmpInboundFile, "%PDF-1.4 fake");
    fs.writeFileSync(tmpOutboundFile, "fake-png-bytes");
  });

  afterEach(() => {
    fs.rmSync(tmpInboundFile, { force: true });
    fs.rmSync(tmpOutboundFile, { force: true });
    env.cleanup();
    _resetSdkCacheForTests();
    if (previousAnthropic === undefined) delete process.env.ANTHROPIC_API_KEY;
    else process.env.ANTHROPIC_API_KEY = previousAnthropic;
  });

  it("delivers a text reply and a file emission via existing send helpers", async () => {
    const outboundPath = tmpOutboundFile;
    _setSdkForTests({
      query: () => {
        async function* gen(): AsyncGenerator<Record<string, unknown>> {
          yield {
            type: "assistant",
            session_id: "session-DEMO",
            message: { content: [{ type: "text", text: "I read your PDF. " }] },
          };
          yield {
            type: "assistant",
            session_id: "session-DEMO",
            message: { content: [{ type: "text", text: "Here is the chart." }] },
            attachments: [
              { path: outboundPath, fileName: "chart.png", mimeType: "image/png" },
            ],
          };
          yield { type: "result", subtype: "success", session_id: "session-DEMO", result: "" };
        }
        return gen();
      },
    });

    const ctx: WeixinMsgContext = {
      Body: "Please summarize",
      From: "wx-user-xyz",
      To: "wx-user-xyz",
      AccountId: "acc-1",
      OriginatingChannel: "clawbnb-weixin",
      OriginatingTo: "wx-user-xyz",
      MessageSid: "sid-1",
      Provider: "clawbnb-weixin",
      ChatType: "direct",
      MediaPath: tmpInboundFile,
      MediaType: "application/pdf",
    };

    const cfg = resolveMoltMarketConfig({});

    await runCliTurnAndDeliver({
      ctx,
      cfg: cfg.agent,
      deps: {
        accountId: ctx.AccountId,
        baseUrl: "https://example.invalid",
        cdnBaseUrl: "https://cdn.example.invalid",
        token: undefined,
        contextToken: "ctx-token",
        log: () => {},
        errLog: () => {},
      },
    });

    // One outbound text send (the buffered text "I read your PDF. Here is the chart.").
    expect(sendMessageWeixinMock).toHaveBeenCalledTimes(1);
    const textArgs = sendMessageWeixinMock.mock.calls[0]?.[0] as { text: string };
    expect(textArgs.text).toContain("I read your PDF.");
    expect(textArgs.text).toContain("Here is the chart.");

    // One outbound media send for the chart attachment.
    expect(sendWeixinMediaFileMock).toHaveBeenCalledTimes(1);
    const mediaArgs = sendWeixinMediaFileMock.mock.calls[0]?.[0] as { filePath: string };
    expect(mediaArgs.filePath).toBe(outboundPath);

    // No error notice should fire.
    expect(sendWeixinErrorNoticeMock).not.toHaveBeenCalled();
  });

  it("notifies the user when the CLI raises an error", async () => {
    _setSdkForTests({
      query: () => {
        async function* gen(): AsyncGenerator<Record<string, unknown>> {
          throw new Error("upstream went bye");
          // eslint-disable-next-line no-unreachable
          yield {};
        }
        return gen();
      },
    });

    const ctx: WeixinMsgContext = {
      Body: "hi",
      From: "wx-user-1",
      To: "wx-user-1",
      AccountId: "acc-1",
      OriginatingChannel: "clawbnb-weixin",
      OriginatingTo: "wx-user-1",
      MessageSid: "sid-2",
      Provider: "clawbnb-weixin",
      ChatType: "direct",
    };
    const cfg = resolveMoltMarketConfig({});

    await runCliTurnAndDeliver({
      ctx,
      cfg: cfg.agent,
      deps: {
        accountId: ctx.AccountId,
        baseUrl: "https://example.invalid",
        cdnBaseUrl: "https://cdn.example.invalid",
        contextToken: "ctx-token-2",
        log: () => {},
        errLog: () => {},
      },
    });

    expect(sendMessageWeixinMock).not.toHaveBeenCalled();
    expect(sendWeixinErrorNoticeMock).toHaveBeenCalledTimes(1);
    const noticeArgs = sendWeixinErrorNoticeMock.mock.calls[0]?.[0] as { message: string };
    expect(noticeArgs.message).toMatch(/Agent 不可用/);
  });
});
