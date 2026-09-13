import { beforeEach, describe, expect, it, vi } from "vitest";
const { invoke } = vi.hoisted(() => ({ invoke: vi.fn(async () => undefined) }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("./platform", () => ({ isTauriIOS: true }));
import {
  beginIOSGeneration,
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
