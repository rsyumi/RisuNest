import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { describe, expect, it, vi } from "vitest";

const script = readFileSync("src-tauri/src/ios_ipc.js", "utf8");

function setup() {
  const fetch = vi.fn(async (_input: unknown, init?: RequestInit) => {
    // Older WebKit normalizes a string POST body, but preserves binary bytes.
    const body =
      typeof init?.body === "string"
        ? init.body.normalize("NFC")
        : init?.body instanceof Uint8Array
          ? new TextDecoder().decode(init.body)
          : init?.body;
    return { body };
  });
  const window = { fetch };
  // Tauri appends this source directly after an IIFE without a semicolon.
  runInNewContext(";(function () {})()\n" + script, { window, TextEncoder });
  return { window, fetch };
}

describe("iOS IPC UTF-8 body preservation", () => {
  it("preserves decomposed Unicode in nested JSON without changing caller options", async () => {
    const { window, fetch } = setup();
    const text = "cafe\u0301 A\u030a \u1100\u1161";
    const body = JSON.stringify({
      request: { texts: [text, text.normalize("NFC")] },
    });
    expect(body.normalize("NFC")).not.toBe(body);
    const headers = {
      "Content-Type": "application/json",
      "Tauri-Invoke-Key": "synthetic-key",
    };
    const init = { method: "POST", headers, body };
    const result = await window.fetch("ipc://localhost/tokenize_batch", init);
    expect(result.body).toBe(body);
    expect(init.body).toBe(body);
    expect(fetch.mock.calls[0][1]?.headers).toBe(headers);
    expect(fetch.mock.calls[0][1]?.method).toBe("POST");
  });

  it("passes existing raw IPC buffers through unchanged", async () => {
    const { window, fetch } = setup();
    const init = { method: "POST", body: new Uint8Array([0, 255, 128, 1]) };
    await window.fetch("ipc://localhost/pds_commit_raw", init);
    expect(fetch.mock.calls[0][1]).toBe(init);
    expect(fetch.mock.calls[0][1]?.body).toBe(init.body);
  });

  it("leaves ordinary HTTP and other origins to their original fetch path", async () => {
    const { window, fetch } = setup();
    const init = { method: "POST", body: "e\u0301" };
    for (const url of [
      "https://example.invalid/api",
      "ipc://other-host/command",
      "https://ipc.localhost/command",
    ]) {
      await window.fetch(url, init);
      expect(fetch.mock.lastCall?.[1]).toBe(init);
    }
  });

  it("returns the original promise and never replays a rejected request", async () => {
    const { window, fetch } = setup();
    const error = new Error("synthetic transport failure");
    fetch.mockRejectedValueOnce(error);
    const result = window.fetch("ipc://localhost/pds_commit", { body: "{}" });
    expect(result).toBe(fetch.mock.results[0].value);
    await expect(result).rejects.toBe(error);
    expect(fetch).toHaveBeenCalledTimes(1);
  });
});
