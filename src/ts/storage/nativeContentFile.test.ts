import { beforeEach, expect, it, vi } from "vitest";
vi.mock("./contentImportOperation", () => ({
  runContentImport: (
    _name: string,
    _options: unknown,
    operation: (options: object) => unknown,
  ) => operation({ signal: new AbortController().signal }),
}));
vi.mock("@tauri-apps/api/path", () => ({
  appDataDir: async () => "/synthetic",
  join: async (...parts: string[]) => parts.join("/"),
}));
vi.mock("@tauri-apps/plugin-fs", () => ({
  mkdir: vi.fn(),
  open: vi.fn(),
  readDir: vi.fn(async () => []),
  stat: vi.fn(),
  remove: vi.fn(async () => {}),
}));
vi.mock("../process/modules", () => ({
  importPreparedNativeModuleContent: vi.fn(),
}));
import { open, readDir, remove, stat } from "@tauri-apps/plugin-fs";
import { importPreparedNativeModuleContent } from "../process/modules";
import { importNativeContentFile } from "./nativeContentFile";

beforeEach(() => {
  vi.clearAllMocks();
});

function source() {
  return {
    name: "synthetic.risum",
    size: 8,
    arrayBuffer: vi.fn(() => {
      throw new Error("Whole-file reads are forbidden");
    }),
    stream: () =>
      new ReadableStream<Uint8Array>({
        start(controller) {
          controller.enqueue(new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]));
          controller.close();
        },
      }),
  } as unknown as File;
}

it("spools partial writes correctly, closes before native import, and removes the private source", async () => {
  const written: number[] = [];
  const close = vi.fn();
  vi.mocked(open).mockResolvedValue({
    write: async (bytes: Uint8Array) => {
      const part = bytes.slice(0, 3);
      written.push(...part);
      return part.length;
    },
    close,
  } as never);
  vi.mocked(importPreparedNativeModuleContent).mockImplementation(async () => {
    expect(close).toHaveBeenCalledOnce();
    expect(written).toEqual([1, 2, 3, 4, 5, 6, 7, 8]);
    return { kind: "imported", value: "module" };
  });
  const file = source();
  expect(await importNativeContentFile(file, "module")).toBe("module");
  expect(file.arrayBuffer).not.toHaveBeenCalled();
  expect(remove).toHaveBeenCalledOnce();
  expect(importPreparedNativeModuleContent).toHaveBeenCalledWith(
    expect.objectContaining({ displayName: file.name }),
    expect.anything(),
  );
});

it("cleans a failed spool without activating content or touching unrelated inbox files", async () => {
  const stale = "11111111-1111-4111-8111-111111111111.risum";
  vi.mocked(readDir).mockResolvedValueOnce([
    { name: stale, isFile: true },
    { name: "unrelated.txt", isFile: true },
  ] as never);
  vi.mocked(stat).mockResolvedValue({ mtime: new Date(0) } as never);
  const close = vi.fn();
  vi.mocked(open).mockResolvedValue({
    write: async () => {
      throw new Error("disk full");
    },
    close,
  } as never);
  await expect(importNativeContentFile(source(), "module")).rejects.toThrow(
    "disk full",
  );
  expect(close).toHaveBeenCalledOnce();
  expect(importPreparedNativeModuleContent).not.toHaveBeenCalled();
  expect(remove).toHaveBeenCalledTimes(2);
  expect(remove).toHaveBeenCalledWith(
    "/synthetic/content-import-inbox/" + stale,
  );
  expect(remove).not.toHaveBeenCalledWith(
    expect.stringContaining("unrelated.txt"),
  );
});
