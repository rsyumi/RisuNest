import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";
import {
  SERVER_SYNC_DEVICE_CHANGED_EVENT,
  SERVER_SYNC_REMOTE_HINT_EVENT,
  subscribeNativeServerSyncSignals,
} from "./serverSyncNativeSignals";

const nativeSource = readFileSync(
  resolve("src-tauri/src/server_sync/events.rs"),
  "utf8",
);

describe("native server sync signals", () => {
  it("wakes synchronization when a device revision changes or the remote may have moved", async () => {
    const handlers = new Map<string, () => void>();
    const dispose = vi.fn();
    const deviceChanged = vi.fn();
    const remoteHint = vi.fn();
    const stop = subscribeNativeServerSyncSignals(
      { deviceChanged, remoteHint },
      async (event, handler) => {
        handlers.set(event, handler);
        return dispose;
      },
    );
    await Promise.resolve();
    handlers.get(SERVER_SYNC_DEVICE_CHANGED_EVENT)?.();
    handlers.get(SERVER_SYNC_REMOTE_HINT_EVENT)?.();
    expect(deviceChanged).toHaveBeenCalledTimes(1);
    expect(remoteHint).toHaveBeenCalledTimes(1);
    stop();
    expect(dispose).toHaveBeenCalledTimes(2);
  });
  it("names the same events the native side emits", () => {
    expect(nativeSource).toContain(
      `DEVICE_CHANGED_EVENT: &str = "${SERVER_SYNC_DEVICE_CHANGED_EVENT}"`,
    );
    expect(nativeSource).toContain(
      `REMOTE_HINT_EVENT: &str = "${SERVER_SYNC_REMOTE_HINT_EVENT}"`,
    );
  });
  it("carries no payload from the native side to the renderer", () => {
    // Invariant 22: a notification says that something moved and nothing more.
    expect(nativeSource).toContain("app.emit(DEVICE_CHANGED_EVENT, ())");
    expect(nativeSource).toContain("app.emit(REMOTE_HINT_EVENT, ())");
  });
});
