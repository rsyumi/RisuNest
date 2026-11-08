import { existsSync, readdirSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { runInNewContext } from "node:vm";
import { webcrypto } from "node:crypto";
import { describe, expect, it, vi } from "vitest";

const tauriVersion = "2.12.5";
const fixtureDir = `tests/fixtures/tauri-${tauriVersion}`;
const fixtureNames = ["core.js", "ipc-protocol.js", "process-ipc-message-fn.js"];
const fixture = (name: string) => readFileSync(`${fixtureDir}/${name}`, "utf8");
const guard = readFileSync("src-tauri/src/ipc_read_guard.js", "utf8");
const iosIpc = readFileSync("src-tauri/src/ios_ipc.js", "utf8");
const desktopLifecycle = readFileSync("src-tauri/src/desktop_lifecycle.js", "utf8");
const channelCommand = "plugin:__TAURI_CHANNEL__|fetch";

type Os = "android" | "windows" | "macos" | "ios";

interface NativeResponse {
  headers: Headers;
  json?: () => Promise<unknown>;
  text?: () => Promise<string>;
  arrayBuffer?: () => Promise<ArrayBuffer>;
}

// Renders Tauri's templates the way `serialize_to_javascript` does: template
// values as JSON literals, raw values as source.
function render(source: string, os: Os) {
  return source
    .replace("__TEMPLATE_invoke_key__", JSON.stringify("synthetic-key"))
    .replace("__RAW_process_ipc_message_fn__", fixture("process-ipc-message-fn.js"))
    .replaceAll("__TEMPLATE_os_name__", JSON.stringify(os))
    .replace("__TEMPLATE_protocol_scheme__", JSON.stringify("http"))
    .replace("__TEMPLATE_fetch_channel_data_command__", JSON.stringify(channelCommand));
}

function setup(
  os: Os,
  nativeFetch: (input: unknown, init?: RequestInit) => Promise<NativeResponse>,
  { withGuard = true, entryScript = "" } = {},
) {
  const fetch = vi.fn(nativeFetch);
  const postMessage = vi.fn();
  const warn = vi.fn();
  const context: Record<string, any> = {
    Headers,
    TextEncoder,
    crypto: webcrypto,
    setTimeout,
    console: { warn, error: vi.fn() },
    fetch,
    ipc: { postMessage },
  };
  context.window = context;
  context.__TAURI_INTERNALS__ = {};
  const guardForTarget = guard.replace("__RISUNEST_ANDROID__", String(os === "android"));
  // The invoke script is Tauri's IPC protocol followed by the appended scripts
  // in builder order, joined without a separator: the guard on every target,
  // then the iOS body encoder or the desktop lifecycle bridge, then whatever an
  // alternative entry appends to the product builder.
  const invokeScript = [
    render(fixture("ipc-protocol.js"), os),
    ...(withGuard ? [guardForTarget] : []),
    ...(os === "ios" ? [iosIpc] : []),
    ...(os === "windows" ? [desktopLifecycle] : []),
    entryScript,
  ].join("");
  runInNewContext(render(fixture("core.js"), os), context);
  runInNewContext(invokeScript, context);
  // Tauri's brownfield `ipc.js` forwards every message unchanged.
  context.__TAURI_INTERNALS__.ipc = (message: unknown) =>
    context.__TAURI_INTERNALS__.postMessage(message);
  const invoke = (cmd: string, payload: unknown = {}, options?: unknown): Promise<unknown> =>
    context.__TAURI_INTERNALS__.invoke(cmd, payload, options);
  return { context, fetch, invoke, postMessage, warn };
}

function nativeResponse(contentType: string, body: string | Uint8Array, ok = true) {
  return new Response(body, {
    headers: { "Content-Type": contentType, "Tauri-Response": ok ? "ok" : "error" },
  });
}

function unreadable(contentType: string, error: unknown): NativeResponse {
  const reject = () => Promise.reject(error);
  return {
    headers: new Headers({ "Content-Type": contentType, "Tauri-Response": "ok" }),
    json: vi.fn(reject),
    text: vi.fn(reject),
    arrayBuffer: vi.fn(reject),
  };
}

describe("Tauri IPC script fixtures", () => {
  it("match the Tauri version in Cargo.lock", () => {
    const lock = readFileSync("src-tauri/Cargo.lock", "utf8");
    expect(lock).toMatch(new RegExp(`name = "tauri"\\r?\\nversion = "${tauriVersion}"\\r?\\n`));
  });

  it("match the registry copy", (context) => {
    const registry = join(process.env.CARGO_HOME ?? join(homedir(), ".cargo"), "registry", "src");
    const sources = existsSync(registry)
      ? readdirSync(registry)
          .map((index) => join(registry, index, `tauri-${tauriVersion}`, "scripts"))
          .filter((dir) => existsSync(dir))
      : [];
    context.skip(sources.length === 0, `No Cargo registry copy of tauri ${tauriVersion}; run cargo fetch in src-tauri`);
    for (const dir of sources) {
      for (const name of fixtureNames) {
        const normalize = (text: string) => text.replaceAll("\r\n", "\n");
        expect(normalize(fixture(name)), name).toBe(normalize(readFileSync(join(dir, name), "utf8")));
      }
    }
  });

  it("use only headers.get and the three body readers on a response", async () => {
    const touched = new Set<string>();
    const record = <T extends object>(name: string, target: T): T =>
      new Proxy(target, {
        get(inner, key) {
          touched.add(`${name}.${String(key)}`);
          const value = Reflect.get(inner, key);
          return typeof value === "function" ? value.bind(inner) : value;
        },
      });
    for (const contentType of ["application/json", "text/plain", "application/octet-stream"]) {
      const real = nativeResponse(contentType, "{}");
      const response = record("response", {
        headers: record("headers", real.headers),
        json: () => real.json(),
        text: () => real.text(),
        arrayBuffer: () => real.arrayBuffer(),
      });
      const { invoke } = setup("windows", async () => response, { withGuard: false });
      await invoke("synthetic_command");
    }
    expect([...touched].sort()).toEqual([
      "headers.get",
      "response.arrayBuffer",
      "response.headers",
      "response.json",
      "response.text",
      "response.then",
    ]);
  });

  it("switch to postMessage after a failed read without the guard", async () => {
    const { invoke, postMessage } = setup(
      "windows",
      async () => unreadable("application/json", new RangeError("Invalid string length")),
      { withGuard: false },
    );
    void invoke("synthetic_command");
    await vi.waitFor(() => expect(postMessage).toHaveBeenCalledOnce());
    expect(JSON.parse(postMessage.mock.calls[0][0]).options).toEqual({ customProtocolIpcBlocked: true });
  });
});

describe("IPC read guard", () => {
  it("passes non-IPC requests to the original fetch synchronously", () => {
    const { context, fetch } = setup("windows", async () => nativeResponse("text/plain", "fixture"));
    const init = { method: "POST", body: "fixture" };
    for (const input of [
      "https://example.invalid/api",
      "ipc://other-host/command",
      "http://asset.localhost/file",
      new URL("http://ipc.localhost/command"),
    ]) {
      const result = context.window.fetch(input, init);
      expect(result).toBe(fetch.mock.results.at(-1)?.value);
      expect(fetch.mock.lastCall).toEqual([input, init]);
    }
  });

  it.each([
    ["application/json", '{"value":[1,2]}', "json", { value: [1, 2] }],
    ["application/json, application/json", '{"value":3}', "json", { value: 3 }],
    ["text/plain", "plain text", "text", "plain text"],
  ] as const)("resolves %s responses after reading the body once", async (contentType, body, reader, expected) => {
    const response = nativeResponse(contentType, body);
    const read = vi.spyOn(response, reader);
    const { invoke, postMessage } = setup("windows", async () => response);
    await expect(invoke("synthetic_command")).resolves.toEqual(expected);
    expect(read).toHaveBeenCalledOnce();
    expect(postMessage).not.toHaveBeenCalled();
  });

  it("resolves raw responses as the bytes that arrived", async () => {
    const { invoke } = setup("macos", async () =>
      nativeResponse("application/octet-stream", Uint8Array.of(0, 255, 7)),
    );
    const result = await invoke("synthetic_command");
    expect([...new Uint8Array(result as ArrayBuffer)]).toEqual([0, 255, 7]);
  });

  it("keeps a native error response as a rejection", async () => {
    const { invoke } = setup("windows", async () => nativeResponse("text/plain", "synthetic failure", false));
    await expect(invoke("synthetic_command")).rejects.toBe("synthetic failure");
  });

  it.each([
    ["application/json", "json"],
    ["text/plain", "text"],
    ["application/octet-stream", "arrayBuffer"],
  ] as const)("rejects only the command whose %s body cannot be read", async (contentType, reader) => {
    const broken = unreadable(contentType, new RangeError("Invalid string length"));
    const responses = [broken, nativeResponse("application/json", '{"next":true}')];
    const { invoke, fetch, postMessage, warn } = setup("windows", async () => responses.shift()!);
    await expect(invoke("synthetic_command")).rejects.toBe(
      "IPC response could not be read (RangeError: Invalid string length)",
    );
    expect(broken[reader]).toHaveBeenCalledOnce();
    await expect(invoke("synthetic_command")).resolves.toEqual({ next: true });
    expect(fetch).toHaveBeenCalledTimes(2);
    expect(postMessage).not.toHaveBeenCalled();
    expect(warn).not.toHaveBeenCalled();
  });

  it.each(["synthetic_command", channelCommand])(
    "keeps Tauri's fallback when %s is blocked before any response off Android",
    async (cmd) => {
      const { invoke, fetch, postMessage } = setup("windows", () =>
        Promise.reject(new TypeError("Failed to fetch")),
      );
      void invoke(cmd, { value: 1 });
      await vi.waitFor(() => expect(postMessage).toHaveBeenCalledOnce());
      expect(fetch).toHaveBeenCalledOnce();
      expect(JSON.parse(postMessage.mock.calls[0][0]).options).toEqual({ customProtocolIpcBlocked: true });
      void invoke("synthetic_command");
      await vi.waitFor(() => expect(postMessage).toHaveBeenCalledTimes(2));
      expect(fetch).toHaveBeenCalledOnce();
    },
  );

  it("rejects an Android channel fetch that fails before any response instead of resending it", async () => {
    const responses: Array<() => Promise<NativeResponse>> = [
      () => Promise.reject(new TypeError("Failed to fetch")),
      async () => nativeResponse("application/octet-stream", Uint8Array.of(1, 2)),
    ];
    const { invoke, fetch, postMessage, warn } = setup("android", () => responses.shift()!());
    const options = { headers: { "Tauri-Channel-Id": "7" } };
    await expect(invoke(channelCommand, null, options)).rejects.toBe(
      "IPC response could not be read (TypeError: Failed to fetch)",
    );
    const next = await invoke(channelCommand, null, options);
    expect([...new Uint8Array(next as ArrayBuffer)]).toEqual([1, 2]);
    expect(fetch.mock.calls.map(([input]) => input)).toEqual([
      `http://ipc.localhost/${encodeURIComponent(channelCommand)}`,
      `http://ipc.localhost/${encodeURIComponent(channelCommand)}`,
    ]);
    expect(postMessage).not.toHaveBeenCalled();
    expect(warn).not.toHaveBeenCalled();
  });

  it("leaves other Android commands on postMessage", async () => {
    const { invoke, fetch, postMessage } = setup("android", async () => nativeResponse("text/plain", ""));
    void invoke("synthetic_command", { value: 1 });
    await vi.waitFor(() => expect(postMessage).toHaveBeenCalledOnce());
    expect(fetch).not.toHaveBeenCalled();
    expect(JSON.parse(postMessage.mock.calls[0][0]).options).toEqual({ customProtocolIpcBlocked: false });
  });

  it("composes with the iOS body encoder in builder order", async () => {
    const response = nativeResponse("application/json", '{"ok":1}');
    const json = vi.spyOn(response, "json");
    const { invoke, fetch } = setup("ios", async () => response);
    await expect(invoke("synthetic_command", { text: "é" })).resolves.toEqual({ ok: 1 });
    const [input, init] = fetch.mock.calls[0];
    expect(input).toBe("ipc://localhost/synthetic_command");
    expect(init?.body).toBeInstanceOf(Uint8Array);
    expect(new TextDecoder().decode(init?.body as Uint8Array)).toBe(JSON.stringify({ text: "é" }));
    expect(json).toHaveBeenCalledOnce();
  });

  it("keeps the desktop lifecycle bridge when an entry appends a script after it", () => {
    const { context } = setup("windows", async () => nativeResponse("application/json", "null"), {
      entryScript: "(() => { window.entryScriptRan = true })();",
    });
    expect(context.entryScriptRan).toBe(true);
    expect(context.RisuLifecycleBridge.onFlushComplete).toBeTypeOf("function");
  });
});
