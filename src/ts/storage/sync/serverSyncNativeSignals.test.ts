import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";
import {
  SERVER_SYNC_DEVICE_CHANGED_EVENT,
  subscribeNativeServerSyncSignals,
} from "./serverSyncNativeSignals";

const nativeSource = readFileSync(
  resolve("src-tauri/src/server_sync/events.rs"),
  "utf8",
);

describe("native server sync signals", () => {
  it("wakes synchronization when a device revision changes", async () => {
    const handlers = new Map<string, () => void>();
    const dispose = vi.fn();
    const deviceChanged = vi.fn();
    const stop = subscribeNativeServerSyncSignals(
      { deviceChanged },
      async (event, handler) => {
        handlers.set(event, handler);
        return dispose;
      },
    );
    await Promise.resolve();
    handlers.get(SERVER_SYNC_DEVICE_CHANGED_EVENT)?.();
    expect(deviceChanged).toHaveBeenCalledTimes(1);
    stop();
    expect(dispose).toHaveBeenCalledTimes(1);
  });
  it("names the same events the native side emits", () => {
    expect(nativeSource).toContain(
      `DEVICE_CHANGED_EVENT: &str = "${SERVER_SYNC_DEVICE_CHANGED_EVENT}"`,
    );
  });
});
