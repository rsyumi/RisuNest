import { beforeEach, expect, it, vi } from "vitest";
import { requestChatDataMain } from "./request";

const mocks = vi.hoisted(() => ({
  fetch: vi.fn(),
  db: {
    aiModel: "ollama-cloud",
    ollamaRequestFormat: 15,
    ollamaCloudModel: "synthetic-model",
    ollamaApiKey: "synthetic-key",
    useStreaming: true,
    temperature: 70,
    genTime: 1,
  },
}));
vi.mock("src/lang", () => ({ language: {} }));
vi.mock("src/ts/globalApi.svelte", () => ({
  fetchNative: mocks.fetch,
  globalFetch: vi.fn(),
}));
vi.mock("src/ts/storage/database.svelte", () => ({
  getDatabase: () => mocks.db,
  getCurrentCharacter: vi.fn(),
  getCurrentChat: vi.fn(),
}));
vi.mock("src/ts/model/modellist", async () => ({
  ...(await import("src/ts/model/types")),
  getModelInfo: () => ({
    id: "ollama-cloud",
    format: 15,
    flags: [],
    internalID: "synthetic-model",
  }),
}));
vi.mock("src/ts/parser/parser.svelte", () => ({
  risuChatParser: vi.fn(),
  risuEscape: vi.fn(),
  risuUnescape: vi.fn(),
}));
vi.mock("src/ts/plugins/plugins.svelte", () => ({
  pluginProcess: vi.fn(),
  pluginV2: {},
}));
vi.mock("src/ts/tokenizer", () => ({ tokenizeNum: vi.fn() }));
vi.mock("src/ts/util", () => ({ sleep: vi.fn() }));
vi.mock("../mcp/mcp", () => ({ getTools: vi.fn() }));
vi.mock("../models/nai", () => ({
  NovelAIBadWordIds: [],
  stringlizeNAIChat: vi.fn(),
}));
vi.mock("../prompt", () => ({ OobaParams: {} }));
vi.mock("../stringlize", () => ({
  getStopStrings: vi.fn(),
  stringlizeAINChat: vi.fn(),
  unstringlizeAIN: vi.fn(),
  unstringlizeChat: (text: string) => text,
}));
vi.mock("../templates/chatTemplate", () => ({ applyChatTemplate: vi.fn() }));
vi.mock("../transformers", () => ({ runTransformers: vi.fn() }));
vi.mock("../triggers", () => ({ runTrigger: vi.fn() }));
vi.mock("./anthropic", () => ({ requestClaude: vi.fn() }));
vi.mock("./google", () => ({ requestGoogleCloudVertex: vi.fn() }));
vi.mock("./openAI/requests", () => ({
  requestOpenAI: vi.fn(),
  requestOpenAILegacyInstruct: vi.fn(),
  requestOpenAIResponseAPI: vi.fn(),
}));
vi.mock("./shared", () => ({
  applyAdditionalParameters: (body: unknown) => body,
  applyParameters: vi.fn(),
  getAdditionalParameters: () => [],
}));

beforeEach(() => vi.clearAllMocks());

it.each(["caller", "consumer"])(
  "propagates %s cancellation through the Ollama SDK to native HTTP",
  async (kind) => {
    let source!: ReadableStreamDefaultController<Uint8Array>;
    let transportSignal!: AbortSignal;
    const encoder = new TextEncoder();
    const line = (done: boolean) =>
      encoder.encode(
        JSON.stringify({
          message: { role: "assistant", content: "합성🐿️" },
          done,
        }) + "\n",
      );
    mocks.fetch.mockImplementation(async (_url, options) => {
      transportSignal = options.signal;
      return new Response(
        new ReadableStream<Uint8Array>({
          start(controller) {
            source = controller;
            controller.enqueue(line(false));
            transportSignal.addEventListener(
              "abort",
              () => controller.error(new DOMException("Stopped", "AbortError")),
              { once: true },
            );
          },
        }),
        { headers: { "Content-Type": "application/x-ndjson" } },
      );
    });
    const caller = new AbortController();
    const response = await requestChatDataMain(
      {
        formated: [{ role: "user", content: "synthetic" }],
        bias: {},
        useStreaming: true,
      },
      "model",
      caller.signal,
    );
    expect(response.type).toBe("streaming");
    if (response.type !== "streaming")
      throw Error("Expected streaming response");
    const reader = response.result.getReader();
    expect((await reader.read()).value).toEqual({ "0": "합성🐿️" });
    if (kind === "caller") caller.abort();
    else await reader.cancel();
    try {
      expect(transportSignal.aborted).toBe(true);
    } finally {
      if (!transportSignal.aborted) {
        source.enqueue(line(true));
        source.close();
      }
      await reader.cancel().catch(() => undefined);
    }
  },
);

it("cancels while the Ollama request is still waiting for response headers", async () => {
  let transportSignal!: AbortSignal;
  mocks.fetch.mockImplementation((_url, options) => {
    transportSignal = options.signal;
    return new Promise((_resolve, reject) =>
      transportSignal.addEventListener(
        "abort",
        () => reject(new DOMException("Stopped", "AbortError")),
        { once: true },
      ),
    );
  });
  const caller = new AbortController();
  const pending = requestChatDataMain(
    {
      formated: [{ role: "user", content: "synthetic" }],
      bias: {},
      useStreaming: true,
    },
    "model",
    caller.signal,
  );
  const rejected = expect(pending).rejects.toMatchObject({
    name: "AbortError",
  });
  await vi.waitFor(() => expect(mocks.fetch).toHaveBeenCalledOnce());
  caller.abort();
  await rejected;
  expect(transportSignal.aborted).toBe(true);
});

it("preserves non-streaming thinking output and releases the native request", async () => {
  let transportSignal!: AbortSignal;
  mocks.fetch.mockImplementation(async (_url, options) => {
    transportSignal = options.signal;
    return Response.json({
      message: {
        role: "assistant",
        content: "합성🐿️",
        thinking: "synthetic reasoning",
      },
      done: true,
    });
  });
  const response = await requestChatDataMain(
    {
      formated: [{ role: "user", content: "synthetic" }],
      bias: {},
      useStreaming: false,
    },
    "model",
    new AbortController().signal,
  );
  expect(response).toEqual({
    type: "success",
    result: "<Thoughts>\nsynthetic reasoning\n</Thoughts>\n\n합성🐿️",
    model: "ollama-cloud",
  });
  expect(transportSignal.aborted).toBe(true);
});
