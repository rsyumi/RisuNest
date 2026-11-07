import { beforeEach, describe, expect, it, vi } from "vitest";

const native = vi.hoisted(() => ({
  invoke: vi.fn(),
  getIdentifier: vi.fn(),
  exists: vi.fn(),
  mkdir: vi.fn(),
  readFile: vi.fn(),
  writeFile: vi.fn(),
  revision: 0,
  opened: false,
  owner: undefined as Uint8Array | undefined,
  calls: [] as string[],
  settings: new Map<string, unknown>(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: native.invoke }));
vi.mock("@tauri-apps/api/app", () => ({ getIdentifier: native.getIdentifier }));
vi.mock("@tauri-apps/api/path", () => ({ join: async (...parts: string[]) => parts.join("/") }));
vi.mock("@tauri-apps/plugin-fs", () => ({
  exists: native.exists,
  mkdir: native.mkdir,
  readFile: native.readFile,
  writeFile: native.writeFile,
}));
vi.mock("@tauri-apps/plugin-os", () => ({ platform: () => "ios" }));
vi.mock("../../src/ts/storage/nativePaths", () => ({ nativeDataPath: async () => "/synthetic/ios-bench" }));
// A failed Sync step also reads the product library's shape, which these records leave unavailable.
vi.mock("../../src/ts/stores.svelte", () => ({}));

import { guard, initialize } from "./contracts";
import { externalStorageContract } from "./externalStorageContracts";
import { syncContract } from "./syncContracts";
import type { FailureRecord } from "../failureRecord";

const owner = "io.github.rsyumi.risunest.ios.bench:legacy-restore-v1";

beforeEach(() => {
  vi.clearAllMocks();
  native.revision = 0;
  native.opened = false;
  native.owner = undefined;
  native.calls = [];
  native.settings.clear();
  native.getIdentifier.mockResolvedValue("io.github.rsyumi.risunest.ios.bench");
  native.exists.mockImplementation(async () => native.owner !== undefined);
  native.readFile.mockImplementation(async () => native.owner);
  native.mkdir.mockResolvedValue(undefined);
  native.writeFile.mockImplementation(async (_path: string, bytes: Uint8Array) => {
    native.calls.push("owner-write");
    native.owner = bytes.slice();
  });
  native.invoke.mockImplementation(async (command: string, args?: { key: string; value: unknown }) => {
    native.calls.push(command);
    if (command === "pds_set_device_setting" && args) {
      if (!native.opened) throw new Error("StoreNotOpen");
      native.settings.set(args.key, args.value);
      return;
    }
    if (command === "pds_open") {
      native.opened = true;
      return { revision: native.revision };
    }
    if (command === "pds_replace_begin") return { stagingId: "synthetic-stage" };
    if (command.startsWith("pds_replace_put_") || command === "pds_replace_add_characters") return;
    if (command === "pds_replace_commit") {
      expect(new TextDecoder().decode(native.owner)).toBe(owner);
      return { revision: ++native.revision };
    }
    if (command === "pds_commit_raw") throw new Error("Malformed synthetic request");
    throw new Error(`Unexpected synthetic command: ${command}`);
  });
});

describe("iOS synthetic profile ownership", () => {
  it("establishes ownership before writing and reopens the retained profile after restart", async () => {
    await initialize();
    expect(native.revision).toBe(1);
    expect(native.settings.get("risuNestDefaultPluginsAttempted")).toBe(JSON.stringify(["provider-manager", "yumi-translator"]));
    expect(native.calls.indexOf("owner-write")).toBeLessThan(native.calls.indexOf("pds_replace_begin"));
    await guard();
    native.opened = false;
    await initialize();
    expect(native.revision).toBe(2);
    expect(native.writeFile).toHaveBeenCalledTimes(1);
    expect(new TextDecoder().decode(native.owner)).toBe(owner);
  });

  it("refuses an unknown nonempty installed profile without claiming or mutating it", async () => {
    native.revision = 7;
    await expect(initialize()).rejects.toThrow("Install a fresh isolated benchmark profile before memory measurement");
    expect(native.mkdir).not.toHaveBeenCalled();
    expect(native.writeFile).not.toHaveBeenCalled();
    expect(native.calls).toEqual(["pds_open"]);
    expect(native.revision).toBe(7);
  });

  it("refuses a foreign bundle before accessing profile authority", async () => {
    native.getIdentifier.mockResolvedValue("synthetic.foreign");
    await expect(guard()).rejects.toThrow("isolated benchmark identifier required");
    expect(native.exists).not.toHaveBeenCalled();
    expect(native.invoke).not.toHaveBeenCalled();
    expect(native.writeFile).not.toHaveBeenCalled();
  });

  it("refuses a mismatched owner without replacing its proof or writing the store", async () => {
    native.owner = new TextEncoder().encode("synthetic-foreign-owner");
    await expect(initialize()).rejects.toThrow("Synthetic profile ownership mismatch");
    expect(native.writeFile).not.toHaveBeenCalled();
    expect(native.invoke).not.toHaveBeenCalled();
    expect(new TextDecoder().decode(native.owner)).toBe("synthetic-foreign-owner");
  });

  it("accepts the exact retained owner after earlier synthetic revisions without replacing proof", async () => {
    native.owner = new TextEncoder().encode(owner);
    native.revision = 9;
    await guard();
    expect(native.writeFile).not.toHaveBeenCalled();
    expect(native.invoke).not.toHaveBeenCalled();
    expect(native.revision).toBe(9);
  });
});

describe("iOS Sync contract failure record", () => {
  const secret = "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789abcd";
  const container = "0C6B1F2E-4A5B-4C3D-9E8F-112233445566";
  const path = `/private/var/mobile/Containers/Data/Application/${container}/Library`;

  it.each([
    ["sync-restart", () => syncContract("sync-restart")],
    ["external-storage-restart", () => externalStorageContract("external-storage-restart")],
  ])("%s records the cause chain of a failed step with paths and secrets redacted", async (_phase, run) => {
    const reports: { stage: string; result: FailureRecord }[] = [];
    native.invoke.mockImplementation(async (command: string, args?: unknown) => {
      if (command === "ios_bench_sync_input" || command === "ios_bench_external_input") {
        return { marker: "synthetic-marker", before: "synthetic-before" };
      }
      if (command === "pds_open") {
        throw new Error("synthetic open failure", { cause: new Error(`refused ${secret} at ${path}`) });
      }
      if (command === "ios_bench_report") {
        reports.push(args as { stage: string; result: FailureRecord });
        return;
      }
      throw new Error(`Unexpected synthetic command: ${command}`);
    });
    const consoleError = console.error;
    try {
      await run();
    } finally {
      console.error = consoleError;
    }
    const failure = reports.find((report) => report.stage === "failure")?.result;
    expect(failure?.causes?.map(({ name, message }) => ({ name, message }))).toEqual([
      { name: "Error", message: "synthetic open failure" },
      { name: "Error", message: "refused <redacted> at <redacted>" },
    ]);
    expect(failure?.causes?.[1].stack).toMatch(/^Error: refused <redacted> at /);
    expect(JSON.stringify(reports)).not.toContain(secret);
    expect(JSON.stringify(reports)).not.toContain(container);
  });
});
