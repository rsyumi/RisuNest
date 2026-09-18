import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const state = vi.hoisted(() => ({
  native: true,
  ready: true,
  running: false,
  restoredSource: undefined as unknown,
  exportedSource: undefined as unknown,
  available: undefined as undefined | (() => boolean),
  revisionListener: undefined as undefined | (() => void),
  scheduler: {
    resume: vi.fn(),
    suspend: vi.fn(),
    localCommit: vi.fn(),
    remoteHint: vi.fn(),
  },
  controllerListener: undefined as undefined | ((state: unknown) => void),
  nativeListeners: new Map<string, () => void>(),
  listen: vi.fn(async (event: string, handler: () => void) => {
    return (
      state.nativeListeners.set(event, handler), () => {}
    );
  }),
  invoke: vi.fn(async (command: string) =>
    command === "server_sync_backup_source"
      ? {
          source: {
            type: "conflictReference",
            token: "123e4567-e89b-42d3-a456-426614174000",
          },
          lease: "synthetic-source-lease",
        }
      : command === "server_sync_backup_delete"
        ? { localDeleted: true, cleanup: "pending" }
        : undefined,
  ),
  restore: vi.fn(async (source?: () => Promise<unknown>) => {
    if (source) state.restoredSource = await source();
  }),
  exportBackup: vi.fn(async (source?: () => Promise<unknown>) => {
    if (source) state.exportedSource = await source();
  }),
  controller: {
    initialize: vi.fn(async () => {}),
    invalidateCompletion: vi.fn(),
    canAutoSync: vi.fn(() => true),
    synchronize: vi.fn(async () => {}),
    suspend: vi.fn(async () => {}),
    snapshot: vi.fn(() => ({ running: false })),
    pause: vi.fn(async () => {}),
    drainToRevision: vi.fn(async () => ({ kind: "complete" as const })),
    cancelExitDrain: vi.fn(async () => {}),
    waitForIdle: vi.fn(async () => {}),
    canRestore: vi.fn(() => true),
    subscribe: vi.fn((listener: (state: unknown) => void) => {
      state.controllerListener = listener;
      return () => {};
    }),
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
  refreshActiveWorkingSetFromStore: vi.fn(),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: state.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: state.listen }));
vi.mock("../portableBackupFileRouteProduction.svelte", () => ({
  restoreBackupFromNativeSource: state.restore,
  exportPortableBackupFromReferenceSource: state.exportBackup,
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
  state.nativeListeners.clear();
  state.controllerListener = undefined;
  state.ready = true;
  state.running = false;
  state.restoredSource = undefined;
  state.exportedSource = undefined;
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
  it("adapts normal exit drains to the server revision controller", async () => {
    const { createServerSyncExitDrainAdapter } = await import(
      "./serverSyncProduction"
    );
    const adapter = createServerSyncExitDrainAdapter("server:selected:selection");
    const abort = new AbortController();
    const target = {
      revision: 17,
      libraryEpoch: "epoch",
      selectionEpoch: "selection",
      selectionId: "server:selected:selection",
    };

    await expect(adapter.drain(target, abort.signal)).resolves.toEqual({
      kind: "complete",
    });
    expect(state.controller.drainToRevision).toHaveBeenCalledWith(
      17,
      abort.signal,
    );
    await adapter.cancel("cancel-exit");
    expect(state.controller.cancelExitDrain).toHaveBeenCalledOnce();
  });

  it("rejects a server exit target captured for a different selection", async () => {
    const { createServerSyncExitDrainAdapter } = await import(
      "./serverSyncProduction"
    );
    const adapter = createServerSyncExitDrainAdapter("server:selected:selection");

    await expect(adapter.drain({
      revision: 17,
      libraryEpoch: "epoch",
      selectionEpoch: "other-selection",
      selectionId: "server:other:other-selection",
    }, new AbortController().signal)).resolves.toEqual({
      kind: "blocked",
      reason: "server-sync-selection-changed",
    });
    expect(state.controller.drainToRevision).not.toHaveBeenCalled();
  });

  it("resolves the selected ID inside the shared file route without automatically cancelling sync", async () => {
    const { restoreServerSyncBackup } = await import("./serverSyncProduction");
    await restoreServerSyncBackup("backup-id", "remote");
    expect(state.restore).toHaveBeenCalledWith(expect.any(Function));
    expect(state.invoke).toHaveBeenCalledWith("server_sync_backup_source", {
      id: "backup-id",
      side: "remote",
    });
    expect(state.restoredSource).toEqual({
      type: "conflictReference",
      token: "123e4567-e89b-42d3-a456-426614174000",
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
  it("returns the native delete outcome after removing the local backup", async () => {
    const { deleteServerSyncBackup } = await import("./serverSyncProduction");

    await expect(deleteServerSyncBackup("backup-id")).resolves.toEqual({
      localDeleted: true,
      cleanup: "pending",
    });
    expect(state.invoke).toHaveBeenCalledWith("server_sync_backup_delete", {
      id: "backup-id",
    });
  });
  it("holds the server source lease through portable export", async () => {
    const { exportServerSyncBackup } = await import("./serverSyncProduction");

    await exportServerSyncBackup("backup-id", "local");

    expect(state.exportedSource).toEqual({
      type: "conflictReference",
      token: "123e4567-e89b-42d3-a456-426614174000",
    });
    expect(state.invoke).toHaveBeenCalledWith("server_sync_backup_release", {
      lease: "synthetic-source-lease",
    });
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
    const cleanups = () =>
      state.invoke.mock.calls.filter(
        ([command]) => command === "server_sync_backup_cleanup",
      ).length;
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
    expect(cleanups()).toBe(1);
    expect(state.scheduler.resume).toHaveBeenCalledTimes(2);
    reject(new Error("synthetic busy"));
    await vi.waitFor(() => expect(cleanups()).toBe(1));
    await Promise.resolve();
    await Promise.resolve();
    resumeServerSyncAfterBackup();
    expect(cleanups()).toBe(2);
    expect(state.controller.pause).not.toHaveBeenCalled();
  });
  it("holds and releases notifications with the rest of foreground synchronization", async () => {
    const { startServerSync } = await import("./serverSyncProduction");
    const listeners = new Map<string, (event: Event) => void>();
    vi.spyOn(document, "addEventListener").mockImplementation(
      (type, listener) =>
        void listeners.set(type, listener as (event: Event) => void),
    );
    const visibility = vi
      .spyOn(document, "visibilityState", "get")
      .mockReturnValue("visible");
    startServerSync();
    await Promise.resolve();
    expect(state.invoke).toHaveBeenCalledWith("server_sync_events_start");

    // A device revision and a remote notification both wake the scheduler; the
    // notification only brings a head confirmation forward.
    state.nativeListeners.get("risu-server-sync-device-changed")!();
    expect(state.controller.invalidateCompletion).toHaveBeenCalled();
    expect(state.scheduler.localCommit).toHaveBeenCalledTimes(1);
    state.nativeListeners.get("risu-server-sync-remote-hint")!();
    expect(state.scheduler.remoteHint).toHaveBeenCalledTimes(1);

    visibility.mockReturnValue("hidden");
    listeners.get("visibilitychange")!(new Event("visibilitychange"));
    expect(state.invoke).toHaveBeenCalledWith("server_sync_events_stop");
  });
  it("starts holding as soon as this device has a binding", async () => {
    const { startServerSync } = await import("./serverSyncProduction");
    vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
    startServerSync();
    await Promise.resolve();
    state.invoke.mockClear();
    state.controllerListener!({ status: { configured: false } });
    expect(state.invoke).not.toHaveBeenCalledWith("server_sync_events_start");
    state.controllerListener!({ status: { configured: true } });
    expect(state.invoke).toHaveBeenCalledWith("server_sync_events_start");
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
