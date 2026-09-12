import { expect, it } from "vitest";
import { MAX_BODY_BYTES, readEnvelope } from "../src/protocol";

function streamed(chunks: Uint8Array[], length?: string) {
  const headers = new Headers({ "content-type": "text/plain" });
  if (length !== undefined) headers.set("content-length", length);
  return new Request("https://registry.invalid", {
    method: "POST",
    headers,
    body: new ReadableStream<Uint8Array>({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(chunk);
        controller.close();
      },
    }),
  });
}

it.each([undefined, "1"])(
  "counts actual streamed bytes with Content-Length %s",
  async (length) => {
    const request = streamed(
      [new Uint8Array(3000).fill(65), new Uint8Array(3000).fill(65)],
      length,
    );
    await expect(readEnvelope(request)).rejects.toMatchObject({
      code: "body-too-large",
      status: 413,
    });
  },
);

it("rejects excessive Content-Length before consuming the body", async () => {
  const request = streamed([], String(MAX_BODY_BYTES + 1));
  await expect(readEnvelope(request)).rejects.toMatchObject({
    code: "body-too-large",
    status: 413,
  });
  expect(request.bodyUsed).toBe(false);
});

it("reads a valid envelope across arbitrary chunk boundaries", async () => {
  const text = btoa("x".repeat(40)).replaceAll("=", "");
  const bytes = new TextEncoder().encode(text);
  expect(
    await readEnvelope(streamed([bytes.slice(0, 7), bytes.slice(7)])),
  ).toBe(text);
});

it("rejects malformed UTF-8 and broken streams with a bounded error", async () => {
  await expect(
    readEnvelope(streamed([new Uint8Array([255])])),
  ).rejects.toMatchObject({ code: "invalid-body" });
  const request = new Request("https://registry.invalid", {
    method: "POST",
    headers: { "content-type": "text/plain" },
    body: new ReadableStream({
      start(c) {
        c.error(new Error("synthetic-body-error"));
      },
    }),
  });
  await expect(readEnvelope(request)).rejects.toMatchObject({
    code: "invalid-body",
  });
});

it("requires text/plain even when no Content-Type is supplied", async () => {
  const request = new Request("https://registry.invalid", {
    method: "POST",
    body: new Uint8Array(40),
  });
  await expect(readEnvelope(request)).rejects.toMatchObject({
    code: "unsupported-media-type",
    status: 415,
  });
});

it("rejects a UTF-8 BOM rather than normalizing the submitted body", async () => {
  const body = new TextEncoder().encode(
    "\uFEFF" + btoa("x".repeat(40)).replaceAll("=", ""),
  );
  await expect(readEnvelope(streamed([body]))).rejects.toMatchObject({
    code: "invalid-envelope",
  });
});

it("rejects a malformed Content-Length with a bounded error", async () => {
  await expect(readEnvelope(streamed([], "invalid"))).rejects.toMatchObject({
    code: "invalid-content-length",
  });
});
