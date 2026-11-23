import { beforeEach, expect, it, vi } from "vitest";
import { fetchNative } from "src/ts/globalApi.svelte";
import { LLMFormat } from "src/ts/model/types";
import { requestClaude } from "./anthropic";
import { requestGoogleCloudVertex } from "./google";
import type { requestDataResponse } from "./request";

vi.mock("src/ts/globalApi.svelte", () => ({
  addFetchLog: vi.fn(),
  fetchNative: vi.fn(),
  globalFetch: vi.fn(),
  textifyReadableStream: vi.fn(),
}));
vi.mock("src/ts/model/modellist", async () => ({
  ...(await import("src/ts/model/types")),
}));
vi.mock("src/ts/observer.svelte", () => ({ registerClaudeObserver: vi.fn() }));
vi.mock("src/ts/storage/database.svelte", () => ({
  getDatabase: () => ({ additionalParams: [], customModels: [], google: {} }),
  setDatabase: vi.fn(),
}));
vi.mock("src/ts/stores.svelte", () => ({ bodyIntercepterStore: [] }));
vi.mock("src/ts/alert", () => ({ alertError: vi.fn() }));
vi.mock("src/ts/util", () => ({
  base64url: vi.fn(),
  replaceAsync: async (text: string) => text,
  simplifySchema: vi.fn(),
  sleep: vi.fn(),
}));
vi.mock("../files/inlays", () => ({
  saveInlayedSignature: vi.fn(),
  setInlayAsset: vi.fn(),
  writeInlayImage: vi.fn(),
}));
vi.mock("../templates/jsonSchema", () => ({
  extractJSON: vi.fn(),
  getGeneralJSONSchema: vi.fn(),
}));
vi.mock("../mcp/mcp", () => ({
  callTool: vi.fn(),
  decodeToolCall: vi.fn(),
  encodeToolCall: vi.fn(),
}));

const text = "가나다";

beforeEach(() => {
  vi.spyOn(console, "log").mockImplementation(() => undefined);
});

const request = (format: LLMFormat) => ({
  formated: [{ role: "user", content: "synthetic" }],
  bias: {},
  aiModel: "synthetic",
  key: "synthetic-key",
  maxTokens: 16,
  mode: "model",
  useStreaming: true,
  modelInfo: {
    id: "synthetic",
    format,
    flags: [],
    parameters: [],
    internalID: "synthetic-model",
  },
});

async function streamedText(response: requestDataResponse) {
  if (response.type !== "streaming") throw new Error(`Unexpected ${response.type} response`);
  const reader = response.result.getReader();
  let last = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) return last;
    last = value["0"];
  }
}

it.each([
  [
    "Anthropic",
    { type: "content_block_delta", delta: { type: "text_delta", text } },
    () => requestClaude(request(LLMFormat.Anthropic) as any),
  ],
  [
    "Google",
    { candidates: [{ content: { parts: [{ text }] } }] },
    () => requestGoogleCloudVertex(request(LLMFormat.GoogleCloud) as any),
  ],
])("keeps a character split across %s stream chunks intact", async (_name, event, send) => {
  const bytes = new TextEncoder().encode(`data: ${JSON.stringify(event)}\n\n`);
  const split = bytes.indexOf(new TextEncoder().encode("나")[0]) + 1;
  vi.mocked(fetchNative).mockResolvedValue(new Response(new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(bytes.subarray(0, split));
      controller.enqueue(bytes.subarray(split));
      controller.close();
    },
  })));

  expect(await streamedText(await send())).toBe(text);
});
