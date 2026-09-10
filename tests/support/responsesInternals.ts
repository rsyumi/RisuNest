import type { Plugin } from "vite";
import type {
  RequestDataArgumentExtended,
  StreamResponseChunk,
} from "../../src/ts/process/request/request";

export interface ResponsesInternals {
  buildResponsesBody(
    arg: RequestDataArgumentExtended,
  ): Promise<Record<string, any>>;
  extractResponsesText(data: any, arg: RequestDataArgumentExtended): string;
  toExternalResponsesBody(body: Record<string, any>): Record<string, any>;
  getResponsesTranStream(
    arg: RequestDataArgumentExtended,
  ): TransformStream<Uint8Array, StreamResponseChunk>;
}

// Preserve the provider's exact unit assertions without adding exports to product code.
// Imported only by vitest.config.ts; normal Vite builds never install this transform.
export function responsesInternalsPlugin(): Plugin {
  return {
    name: "test-responses-internals",
    enforce: "pre",
    transform(code, id) {
      if (
        !id
          .replaceAll("\\", "/")
          .endsWith("/src/ts/process/request/openAI/responses.ts")
      )
        return;
      return `${code}\nexport const __testResponsesAPI = { buildResponsesBody, extractResponsesText, toExternalResponsesBody, getResponsesTranStream };\n`;
    },
  };
}
