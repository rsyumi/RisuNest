import { beforeEach, describe, expect, it, vi } from "vitest";
const { invoke } = vi.hoisted(() => ({ invoke: vi.fn(async () => undefined) }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("./platform", async () => (await import("./platform.testSupport")).platformModule({ runtime: "tauri", os: "ios" }));
import {
  isBackgroundExpiryReason,
  beginIOSGeneration,
  beginIOSBackgroundTask,
  initializeIOSNative,
  installIOSPersistenceLifecycle,
  type IOSNativeState,
} from "./iosNative";
beforeEach(() => {
  invoke.mockClear();
});

const state = (
  activeTasks: string[] = ["lease"],
  expiredTasks: string[] = [],
): IOSNativeState => ({
  activeTasks,
  expiredTasks,
  foreground: true,
  notifications: false,
  notificationStatus: 0,
  backgroundMode: "limited",
  continuedRegistered: false,
  continuedErrorCode: null,
  backgroundRefreshStatus: 2,
});
function harness() {
  return {
    enabled: () => true,
    begin: vi.fn(async () => ({ id: "lease" })),
    end: vi.fn(async () => {}),
    state: vi.fn(async () => state()),
    events: new EventTarget(),
  };
}
describe("iOS generation lifecycle", () => {
  it("resets previous renderer work before initialization completes", async () => {
    await initializeIOSNative();
    expect(invoke).toHaveBeenCalledExactlyOnceWith(
      "plugin:ios-native|reset_generation",
    );
  });
  it("retains the background save assertion until local persistence settles", async () => {
    let finish!: () => void;
    const flush = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          finish = resolve;
        }),
    );
    const remove = installIOSPersistenceLifecycle(flush);
    try {
      window.dispatchEvent(
        new CustomEvent("risunest-ios-lifecycle", {
          detail: { event: "background", id: "save-lease" },
        }),
      );
      await vi.waitFor(() => expect(flush).toHaveBeenCalledOnce());
      expect(invoke).not.toHaveBeenCalled();
      finish();
      await vi.waitFor(() =>
        expect(invoke).toHaveBeenCalledWith("plugin:ios-native|end", {
          id: "save-lease",
        }),
      );
    } finally {
      remove();
    }
  });
  it.each(["settled", "active", "expired"] as const)(
    "keeps the background save assertion while deferred work runs until it ends: %s",
    async (ending) => {
      let settle!: () => void;
      const holds = {
        settled: vi.fn(() => new Promise<void>((resolve) => { settle = resolve; })),
      };
      const lifecycle = (detail: { event: string; id?: string }) =>
        window.dispatchEvent(new CustomEvent("risunest-ios-lifecycle", { detail }));
      const remove = installIOSPersistenceLifecycle(vi.fn(async () => {}), holds);
      try {
        lifecycle({ event: "background", id: "save-lease" });
        await vi.waitFor(() => expect(holds.settled).toHaveBeenCalledOnce());
        lifecycle({ event: "expired", id: "other-lease" });
        await new Promise((resolve) => setTimeout(resolve));
        expect(invoke).not.toHaveBeenCalled();
        if (ending === "settled") settle();
        else lifecycle({ event: ending, ...(ending === "expired" ? { id: "save-lease" } : {}) });
        await vi.waitFor(() =>
          expect(invoke).toHaveBeenCalledWith("plugin:ios-native|end", { id: "save-lease" }),
        );
        // Returning to the screen also checks the media endpoint; let that call land inside this test.
        if (ending === "active") await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("native_media_ensure"));
      } finally {
        remove();
      }
    },
  );
  it("saves again at once while a background entry waits for deferred work", async () => {
    let settle!: () => void;
    const holds = {
      settled: vi.fn(() => new Promise<void>((resolve) => { settle = resolve; })),
    };
    const flush = vi.fn(async () => {});
    const lifecycle = (detail: { event: string; id?: string }) =>
      window.dispatchEvent(new CustomEvent("risunest-ios-lifecycle", { detail }));
    const remove = installIOSPersistenceLifecycle(flush, holds);
    try {
      lifecycle({ event: "background", id: "save-lease" });
      await vi.waitFor(() => expect(holds.settled).toHaveBeenCalledOnce());
      lifecycle({ event: "memory-warning" });
      await vi.waitFor(() => expect(flush).toHaveBeenCalledTimes(2));
      expect(invoke).not.toHaveBeenCalled();
      settle();
      await vi.waitFor(() =>
        expect(invoke).toHaveBeenCalledWith("plugin:ios-native|end", { id: "save-lease" }),
      );
    } finally {
      remove();
    }
  });
  it("records the foreground return in the generation diagnostics", async () => {
    const { generationDiagnostics } = await import("./generationDiagnostics");
    invoke.mockResolvedValueOnce(7 as never);
    generationDiagnostics.startDocument();
    expect(invoke).toHaveBeenCalledWith("native_generation_diagnostic", { event: { kind: "document-start" } });
    // Lets the document handshake settle before the event is sent.
    await new Promise((resolve) => setTimeout(resolve, 0));
    const remove = installIOSPersistenceLifecycle(vi.fn(async () => {}));
    try {
      window.dispatchEvent(new CustomEvent("risunest-ios-lifecycle", { detail: { event: "active" } }));
      expect(invoke).toHaveBeenCalledWith("native_generation_diagnostic", {
        event: expect.objectContaining({ kind: "lifecycle", documentSequence: 7, event: "active" }),
      });
    } finally {
      remove();
    }
  });
  it("ends the background save assertion without waiting when the app returned during the save", async () => {
    const holds = { settled: vi.fn(() => new Promise<void>(() => {})) };
    let finish!: () => void;
    const flush = vi.fn(() => new Promise<void>((resolve) => { finish = resolve; }));
    const lifecycle = (detail: { event: string; id?: string }) =>
      window.dispatchEvent(new CustomEvent("risunest-ios-lifecycle", { detail }));
    const remove = installIOSPersistenceLifecycle(flush, holds);
    try {
      lifecycle({ event: "background", id: "save-lease" });
      await vi.waitFor(() => expect(flush).toHaveBeenCalledOnce());
      lifecycle({ event: "active" });
      finish();
      await vi.waitFor(() =>
        expect(invoke).toHaveBeenCalledWith("plugin:ios-native|end", { id: "save-lease" }),
      );
      expect(holds.settled).not.toHaveBeenCalled();
      await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("native_media_ensure"));
    } finally {
      remove();
    }
  });
  it("reports actual progress in order and releases only after pending progress", async () => {
    const calls: string[] = [];
    const deps = {
      ...harness(),
      progress: vi.fn(async (_id: string, completed: number) => {
        calls.push(`progress:${completed}`);
      }),
      end: vi.fn(async (_id: string, success: boolean) => {
        calls.push(`end:${success}`);
      }),
    };
    const lease = await beginIOSGeneration(undefined, deps);
    lease.progress(1);
    lease.progress(1);
    lease.progress(0);
    lease.progress(2);
    lease.progress(3);
    await lease.dispose(true);
    lease.progress(3);
    expect(calls).toEqual([
      "progress:1",
      "progress:2",
      "progress:3",
      "end:true",
    ]);
  });
  it("preserves the caller cancellation and releases exactly once", async () => {
    const deps = harness();
    const caller = new AbortController();
    const lease = await beginIOSGeneration(caller.signal, deps);
    caller.abort("user-cancel");
    expect(lease.signal?.reason).toBe("user-cancel");
    await lease.dispose();
    await lease.dispose();
    expect(deps.end).toHaveBeenCalledExactlyOnceWith("lease", false);
  });
  it("expires only its own generation and removes the listener on release", async () => {
    const deps = harness();
    const lease = await beginIOSGeneration(undefined, deps);
    deps.events.dispatchEvent(
      new CustomEvent("risunest-ios-lifecycle", {
        detail: { event: "expired", id: "other" },
      }),
    );
    expect(lease.signal?.aborted).toBe(false);
    deps.events.dispatchEvent(
      new CustomEvent("risunest-ios-lifecycle", {
        detail: { event: "expired", id: "lease" },
      }),
    );
    expect(lease.signal?.aborted).toBe(true);
    expect(isBackgroundExpiryReason(lease.signal?.reason)).toBe(true);
    await lease.dispose();
    deps.events.dispatchEvent(
      new CustomEvent("risunest-ios-lifecycle", {
        detail: { event: "active" },
      }),
    );
    expect(deps.state).not.toHaveBeenCalled();
  });
  it("detects an expiration event missed while WebKit was suspended", async () => {
    const deps = harness();
    deps.state.mockResolvedValue(state([], ["lease"]));
    const lease = await beginIOSGeneration(undefined, deps);
    deps.events.dispatchEvent(
      new CustomEvent("risunest-ios-lifecycle", {
        detail: { event: "active" },
      }),
    );
    await vi.waitFor(() => expect(lease.signal?.aborted).toBe(true));
    await lease.dispose();
  });
  it("allows foreground work when the OS rejects additional runtime", async () => {
    const deps = harness();
    deps.begin.mockRejectedValue(new Error("unavailable"));
    const lease = await beginIOSGeneration(undefined, deps);
    expect(lease.signal?.aborted).toBe(false);
    await lease.dispose();
    expect(deps.end).not.toHaveBeenCalled();
  });
  it("does not touch native commands on other platforms", async () => {
    const deps = { ...harness(), enabled: () => false };
    const controller = new AbortController();
    const lease = await beginIOSGeneration(controller.signal, deps);
    expect(lease.signal).toBe(controller.signal);
    expect(deps.begin).not.toHaveBeenCalled();
  });
});

it("reports measured task progress across stages and releases expired runtime", async () => {
  await initializeIOSNative();
  invoke.mockResolvedValueOnce({ id: "background" } as never);
  const release = vi.fn();
  const task = await beginIOSBackgroundTask("backup", undefined, release);
  expect(invoke).toHaveBeenCalledWith("plugin:ios-native|begin", { kind: "backup", continued: false });
  task.progress(60);
  task.progress(null);
  task.progress(15);
  window.dispatchEvent(new CustomEvent("risunest-ios-lifecycle", { detail: { event: "expired", id: "background" } }));
  expect(task.signal?.aborted).toBe(true);
  expect(release).toHaveBeenCalledOnce();
  await task.dispose(false);
  expect(invoke).toHaveBeenCalledWith("plugin:ios-native|generation_progress", { id: "background", completed: 60, total: 100 });
  expect(invoke).toHaveBeenCalledWith("plugin:ios-native|generation_progress", { id: "background", completed: 0, total: 0 });
  expect(invoke).toHaveBeenCalledWith("plugin:ios-native|generation_progress", { id: "background", completed: 15, total: 100 });
  expect(invoke).toHaveBeenLastCalledWith("plugin:ios-native|end", { id: "background", success: false });
});

it('keeps a current lease on activation and opts explicit work into continued processing', async () => {
  invoke.mockResolvedValue({ id: 'current' } as never)
  const automatic = await beginIOSBackgroundTask('sync', undefined, () => {})
  const explicit = await beginIOSBackgroundTask('backup', undefined, () => {}, true)
  expect(invoke).toHaveBeenCalledWith('plugin:ios-native|begin', { kind: 'sync', continued: false })
  expect(invoke).toHaveBeenCalledWith('plugin:ios-native|begin', { kind: 'backup', continued: true })
  await automatic.dispose()
  await explicit.dispose()
  const deps = harness()
  const task = await beginIOSGeneration(undefined, deps)
  deps.events.dispatchEvent(new CustomEvent('risunest-ios-lifecycle', { detail: { event: 'active' } }))
  await vi.waitFor(() => expect(deps.state).toHaveBeenCalledOnce())
  expect(task.signal?.aborted).toBe(false)
  await task.dispose()
})
