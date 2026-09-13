import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const state = vi.hoisted(() => ({
  native: true,
  ready: true,
  running: false,
  available: undefined as undefined | (() => boolean),
  revisionListener: undefined as undefined | (() => void),
  scheduler: { resume: vi.fn(), suspend: vi.fn(), localCommit: vi.fn() },
  invoke: vi.fn(async (command: string) =>
    command === "server_sync_backup_source"
      ? { path: "synthetic-backup-path", lease: "synthetic-source-lease" }
      : undefined,
  ),
  restore: vi.fn(async (source?: () => Promise<unknown>) => {
    if (source) await source();
  }),
  controller: {
    initialize: vi.fn(async () => {}),
    invalidateCompletion: vi.fn(),
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
vi.mock("../portableBackupFileRouteProduction.svelte", () => ({
  restoreBackupFromNativeSource: state.restore,
}));
vi.mock("./serverSync", async (original) => ({
  ...(await original<object>()),
  createServerSyncFacade: vi.fn(() => ({})),
}));
vi.mock("./serverSyncController", () => ({
  createServerSyncController: () => state.controller,
}));
vi.mock("./serverSyncScheduler", () => ({
  createServerSyncScheduler: (
    _controller: unknown,
    options: { available(): boolean },
  ) => {
    state.available = options.available;
    return state.scheduler;
  },
}));
vi.mock("../persistentRevisionEvents", () => ({
  subscribeLocalPersistentRevision: (listener: () => void) => {
    state.revisionListener = listener;
    return () => {};
  },
}));
beforeEach(() => {
  vi.resetModules();
  vi.clearAllMocks();
  vi.useFakeTimers();
  state.native = true;
  state.available = undefined;
  state.revisionListener = undefined;
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
  it("resolves the selected ID inside the shared file route without automatically cancelling sync", async () => {
    const { restoreServerSyncBackup } = await import("./serverSyncProduction");
    await restoreServerSyncBackup("backup-id", "remote");
    expect(state.restore).toHaveBeenCalledWith(expect.any(Function));
    expect(state.invoke).toHaveBeenCalledWith("server_sync_backup_source", {
      id: "backup-id",
      side: "remote",
    });
    expect(state.invoke).toHaveBeenCalledWith("server_sync_backup_release", {
      lease: "synthetic-source-lease",
    });
    expect(state.controller.pause).not.toHaveBeenCalled();
    expect(state.controller.synchronize).not.toHaveBeenCalled();
    // Rejection by the file route must occur before the native source is opened.
    state.invoke.mockClear();
    state.restore.mockRejectedValueOnce({ code: "server-sync-busy" });
    await expect(
      restoreServerSyncBackup("backup-id", "local"),
    ).rejects.toMatchObject({ code: "server-sync-busy" });
    expect(state.invoke).not.toHaveBeenCalled();
  });
  it("keeps the native source pinned until restore or cancellation has settled", async () => {
    let finish!: () => void;
    state.restore.mockImplementationOnce(async (source) => {
      await source!();
      await new Promise<void>((resolve) => {
        finish = resolve;
      });
      throw new Error("synthetic cancelled restore");
    });
    const { restoreServerSyncBackup } = await import("./serverSyncProduction");
    const restoring = restoreServerSyncBackup("backup-id", "local");
    await vi.waitFor(() => expect(finish).toBeTypeOf("function"));
    expect(state.invoke).not.toHaveBeenCalledWith(
      "server_sync_backup_release",
      expect.anything(),
    );
    finish();
    await expect(restoring).rejects.toThrow("synthetic cancelled restore");
    expect(state.invoke).toHaveBeenLastCalledWith(
      "server_sync_backup_release",
      { lease: "synthetic-source-lease" },
    );
  });
  it("starts once after initialization and connects durable saves, visibility, and network signals", async () => {
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
    const online = vi.spyOn(navigator, "onLine", "get").mockReturnValue(true);
    const { startServerSync } = await import("./serverSyncProduction");
    startServerSync();
    startServerSync();
    await Promise.resolve();
    expect(state.controller.initialize).toHaveBeenCalledTimes(1);
    expect(state.scheduler.resume).toHaveBeenCalledTimes(1);
    expect(state.available!()).toBe(true);
    state.revisionListener!();
    expect(state.scheduler.localCommit).toHaveBeenCalledTimes(1);
    visibility.mockReturnValue("hidden");
    listeners.get("visibilitychange")!(new Event("visibilitychange"));
    expect(state.scheduler.suspend).toHaveBeenCalledTimes(1);
    expect(state.available!()).toBe(false);
    visibility.mockReturnValue("visible");
    listeners.get("visibilitychange")!(new Event("visibilitychange"));
    online.mockReturnValue(false);
    listeners.get("offline")!(new Event("offline"));
    expect(state.available!()).toBe(false);
    expect(state.scheduler.suspend).toHaveBeenCalledTimes(2);
    online.mockReturnValue(true);
    listeners.get("online")!(new Event("online"));
    expect(state.scheduler.resume).toHaveBeenCalledTimes(3);
    const { resumeServerSyncAfterBackup } =
      await import("./serverSyncProduction");
    resumeServerSyncAfterBackup();
    expect(state.scheduler.resume).toHaveBeenCalledTimes(4);
  });
  it("cleans in the background, coalesces concurrent attempts, and retries after failure", async () => {
    const { startServerSync, resumeServerSyncAfterBackup } = await import(
      "./serverSyncProduction"
    );
    let reject!: (reason: unknown) => void;
    state.invoke.mockImplementationOnce(
      () =>
        new Promise((_, failure) => {
          reject = failure;
        }),
    );
    startServerSync();
    await Promise.resolve();
    expect(state.invoke).toHaveBeenCalledWith("server_sync_backup_cleanup");
    expect(state.scheduler.resume).toHaveBeenCalledTimes(1);
    resumeServerSyncAfterBackup();
    expect(state.invoke).toHaveBeenCalledTimes(1);
    expect(state.scheduler.resume).toHaveBeenCalledTimes(2);
    reject(new Error("synthetic busy"));
    await vi.waitFor(() => expect(state.invoke).toHaveBeenCalledTimes(1));
    await Promise.resolve();
    await Promise.resolve();
    resumeServerSyncAfterBackup();
    expect(state.invoke).toHaveBeenCalledTimes(2);
    expect(state.controller.pause).not.toHaveBeenCalled();
  });
  it("does not install a native scheduler in the browser build", async () => {
    state.native = false;
    const interval = vi.spyOn(globalThis, "setInterval");
    const { startServerSync } = await import("./serverSyncProduction");
    startServerSync();
    expect(interval).not.toHaveBeenCalled();
    expect(state.controller.initialize).not.toHaveBeenCalled();
    expect(state.available).toBeUndefined();
  });
});
