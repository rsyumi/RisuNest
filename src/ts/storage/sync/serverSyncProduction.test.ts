import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const state = vi.hoisted(() => ({
  native: true,
  ready: true,
  running: false,
  invoke: vi.fn(async () => "synthetic-backup-path"),
  restore: vi.fn(async () => {}),
  controller: {
    initialize: vi.fn(async () => {}),
    canAutoSync: vi.fn(() => true),
    synchronize: vi.fn(async () => {}),
    suspend: vi.fn(async () => {}),
    snapshot: vi.fn(() => ({ running: false })),
    pause: vi.fn(async () => {}),
    waitForIdle: vi.fn(async () => {}),
    canRestore: vi.fn(() => true),
  },
}));
vi.mock("../../platform", () => ({
  get isTauri() {
    return state.native;
  },
}));
vi.mock("../persistentDataRuntime.svelte", () => ({
  flushPendingData: vi.fn(),
  capturePersistentMutationToken: vi.fn(),
  acquireDestructiveReplacementFence: vi.fn(),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: state.invoke }));
vi.mock("../losslessBackupFileRouteProduction.svelte", () => ({
  restoreLocalBackupFromNativeSource: state.restore,
}));
vi.mock("./serverSync", async (original) => ({
  ...(await original<object>()),
  createServerSyncFacade: vi.fn(() => ({})),
}));
vi.mock("./serverSyncController", () => ({
  createServerSyncController: () => state.controller,
}));
beforeEach(() => {
  vi.resetModules();
  vi.clearAllMocks();
  vi.useFakeTimers();
  state.native = true;
  state.ready = true;
  state.running = false;
  state.controller.canRestore.mockReturnValue(true);
  state.controller.canAutoSync.mockImplementation(() => state.ready);
  state.controller.snapshot.mockImplementation(() => ({
    running: state.running,
  }));
});
afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});
describe("native server synchronization scheduling", () => {
  it("waits for cancellation, verifies the selected archive and restores through the lossless job", async () => {
    const { restoreServerSyncBackup } = await import("./serverSyncProduction");
    await restoreServerSyncBackup("backup-id", "remote");
    expect(state.controller.pause).toHaveBeenCalledTimes(1);
    expect(state.controller.waitForIdle).toHaveBeenCalledTimes(1);
    expect(state.invoke).toHaveBeenCalledWith("server_sync_backup_source", {
      id: "backup-id",
      side: "remote",
    });
    expect(state.restore).toHaveBeenCalledWith({
      type: "desktopPath",
      path: "synthetic-backup-path",
    });
    expect(state.controller.initialize).toHaveBeenCalledTimes(1);
    expect(state.controller.synchronize).not.toHaveBeenCalled();
    state.controller.canRestore.mockReturnValue(false);
    await expect(
      restoreServerSyncBackup("backup-id", "local"),
    ).rejects.toMatchObject({ code: "resolve-pending-operation-first" });
    expect(state.restore).toHaveBeenCalledTimes(1);
  });
  it("starts once after initialization and pauses transport when the app is hidden", async () => {
    const listeners = new Map<string, EventListener>();
    vi.spyOn(document, "addEventListener").mockImplementation(
      (event, listener) => {
        listeners.set(event, listener as EventListener);
      },
    );
    vi.spyOn(window, "addEventListener").mockImplementation(
      (event, listener) => {
        listeners.set(event, listener as EventListener);
      },
    );
    const visibility = vi
      .spyOn(document, "visibilityState", "get")
      .mockReturnValue("visible");
    vi.spyOn(navigator, "onLine", "get").mockReturnValue(true);
    const { startServerSync } = await import("./serverSyncProduction");
    startServerSync();
    startServerSync();
    await Promise.resolve();
    expect(state.controller.initialize).toHaveBeenCalledTimes(1);
    expect(state.controller.synchronize).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(30_000);
    expect(state.controller.synchronize).toHaveBeenCalledTimes(2);
    visibility.mockReturnValue("hidden");
    state.running = true;
    listeners.get("visibilitychange")!(new Event("visibilitychange"));
    expect(state.controller.suspend).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(30_000);
    expect(state.controller.synchronize).toHaveBeenCalledTimes(2);
    visibility.mockReturnValue("visible");
    state.ready = false;
    listeners.get("online")!(new Event("online"));
    expect(state.controller.synchronize).toHaveBeenCalledTimes(2);
  });
  it("does not install a native scheduler in the browser build", async () => {
    state.native = false;
    const interval = vi.spyOn(globalThis, "setInterval");
    const { startServerSync } = await import("./serverSyncProduction");
    startServerSync();
    expect(interval).not.toHaveBeenCalled();
    expect(state.controller.initialize).not.toHaveBeenCalled();
  });
});
