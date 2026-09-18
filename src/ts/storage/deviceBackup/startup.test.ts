import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const state = vi.hoisted(() => ({ invoke: vi.fn(), normalStarted: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: state.invoke }));

function nativeComplete(includesLibrary: boolean) {
  return {
    mode: "maintenance",
    session: {
      sessionId: "native-restore-1",
      jobId: "portable-restore-1",
      operation: "restore",
      phase: "committed",
      includesLibrary,
      selectedSections: ["hypa"],
      action: "native-complete",
    },
  };
}

beforeEach(() => {
  vi.resetModules();
  state.invoke.mockReset();
  state.normalStarted.mockClear();
  vi.doMock("../../../normalMain", () => {
    state.normalStarted();
    return { default: {} };
  });
  Object.defineProperty(window, "__TAURI_INTERNALS__", {
    configurable: true,
    value: {},
  });
  localStorage.removeItem("risuNestServerSyncRestoreHold");
});
afterEach(() => {
  delete (window as Window & { __TAURI_INTERNALS__?: unknown })
    .__TAURI_INTERNALS__;
  localStorage.removeItem("risuNestServerSyncRestoreHold");
  document.body.replaceChildren();
  vi.restoreAllMocks();
});

describe("device maintenance bootstrap ordering", () => {
  it("does not evaluate the normal app before the native recovery decision", async () => {
    let decide!: (value: unknown) => void;
    state.invoke.mockImplementation((command: string) => {
      if (command === "native_startup_status") return Promise.resolve();
      return new Promise((resolve) => {
        decide = resolve;
      });
    });
    const main = await import("../../../main");
    expect(state.invoke).toHaveBeenNthCalledWith(1, "native_startup_status");
    expect(state.invoke).toHaveBeenNthCalledWith(
      2,
      "native_device_backup_bootstrap",
    );
    expect(state.normalStarted).not.toHaveBeenCalled();
    decide({ mode: "normal", session: null });
    await main.default;
    expect(state.normalStarted).toHaveBeenCalledTimes(1);
  });

  it("blocks normal imports when the native recovery decision fails", async () => {
    state.invoke.mockImplementation((command: string) => command === "native_startup_status"
      ? Promise.resolve()
      : Promise.reject(new Error("synthetic journal failure")));
    await import("../../../main");
    await vi.waitFor(() =>
      expect(document.body.textContent).toContain(
        "Normal app startup is blocked",
      ),
    );
    expect(state.invoke).toHaveBeenNthCalledWith(1, "native_startup_status");
    expect(state.invoke).toHaveBeenNthCalledWith(
      2,
      "native_device_backup_bootstrap",
    );
    expect(state.normalStarted).not.toHaveBeenCalled();
  });

  it("retains ordinary web startup without invoking native maintenance", async () => {
    delete (window as Window & { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__;
    const main = await import("../../../main");
    await main.default;
    expect(state.invoke).not.toHaveBeenCalled();
    expect(state.normalStarted).toHaveBeenCalledTimes(1);
  });

  it("holds server sync before acknowledging a completed native library restore", async () => {
    let bootstraps = 0;
    state.invoke.mockImplementation((command: string, args?: unknown) => {
      if (command === "native_startup_status") return Promise.resolve();
      if (command === "native_device_backup_bootstrap")
        return Promise.resolve(
          bootstraps++ === 0
            ? nativeComplete(true)
            : { mode: "normal", session: null },
        );
      if (command === "native_device_backup_recovery_complete") {
        expect(localStorage.getItem("risuNestServerSyncRestoreHold")).toBe(
          "true",
        );
        expect(args).toEqual({ sessionId: "native-restore-1" });
        return Promise.resolve();
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    const main = await import("../../../main");
    await main.default;

    expect(state.invoke.mock.calls.map(([command]) => command)).toEqual([
      "native_startup_status",
      "native_device_backup_bootstrap",
      "native_device_backup_recovery_complete",
      "native_device_backup_bootstrap",
    ]);
    expect(state.normalStarted).toHaveBeenCalledTimes(1);
  });

  it("acknowledges a device-only native restore without holding server sync", async () => {
    let bootstraps = 0;
    state.invoke.mockImplementation((command: string) => {
      if (command === "native_startup_status") return Promise.resolve();
      if (command === "native_device_backup_bootstrap")
        return Promise.resolve(
          bootstraps++ === 0
            ? nativeComplete(false)
            : { mode: "normal", session: null },
        );
      if (command === "native_device_backup_recovery_complete") {
        expect(localStorage.getItem("risuNestServerSyncRestoreHold")).toBeNull();
        return Promise.resolve();
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    const main = await import("../../../main");
    await main.default;

    expect(state.normalStarted).toHaveBeenCalledTimes(1);
  });

  it("blocks a malformed native completion before writing the restore hold", async () => {
    state.invoke.mockImplementation((command: string) => {
      if (command === "native_startup_status") return Promise.resolve();
      if (command === "native_device_backup_bootstrap")
        return Promise.resolve({
          ...nativeComplete(true),
          session: { ...nativeComplete(true).session, sessionId: "" },
        });
      throw new Error(`Unexpected command: ${command}`);
    });

    await import("../../../main");
    await vi.waitFor(() =>
      expect(document.body.textContent).toContain(
        "Normal app startup is blocked",
      ),
    );

    expect(localStorage.getItem("risuNestServerSyncRestoreHold")).toBeNull();
    expect(state.invoke).not.toHaveBeenCalledWith(
      "native_device_backup_recovery_complete",
      expect.anything(),
    );
    expect(state.normalStarted).not.toHaveBeenCalled();
  });

  it("blocks on a failed acknowledgement and retries native completion on the next bootstrap", async () => {
    let acknowledgementAttempts = 0;
    let bootstraps = 0;
    state.invoke.mockImplementation((command: string) => {
      if (command === "native_startup_status") return Promise.resolve();
      if (command === "native_device_backup_bootstrap") {
        bootstraps++;
        return Promise.resolve(
          bootstraps < 3
            ? nativeComplete(true)
            : { mode: "normal", session: null },
        );
      }
      if (command === "native_device_backup_recovery_complete") {
        acknowledgementAttempts++;
        return acknowledgementAttempts === 1
          ? Promise.reject(new Error("synthetic acknowledgement failure"))
          : Promise.resolve();
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    await import("../../../main");
    await vi.waitFor(() =>
      expect(document.body.textContent).toContain(
        "Normal app startup is blocked",
      ),
    );
    expect(state.normalStarted).not.toHaveBeenCalled();
    expect(localStorage.getItem("risuNestServerSyncRestoreHold")).toBe("true");

    const { deviceMaintenanceBeforeBootstrap } = await import("./entry");
    await expect(deviceMaintenanceBeforeBootstrap()).resolves.toBeUndefined();

    expect(acknowledgementAttempts).toBe(2);
    expect(bootstraps).toBe(3);
    expect(state.normalStarted).not.toHaveBeenCalled();
  });

  it("reloads native completion directly instead of requesting clone recovery", async () => {
    const reload = vi.fn();
    state.invoke.mockImplementation((command: string) => {
      if (command === "native_startup_status") return Promise.resolve();
      if (command === "native_device_backup_bootstrap")
        return Promise.resolve(nativeComplete(true));
      if (command === "native_device_backup_recovery_complete")
        return Promise.reject(new Error("synthetic acknowledgement failure"));
      throw new Error(`Unexpected command: ${command}`);
    });

    const { deviceMaintenanceBeforeBootstrap } = await import("./entry");
    void deviceMaintenanceBeforeBootstrap({ reload });
    const retry = await vi.waitFor(() => {
      const button = [...document.querySelectorAll("button")].find(
        (candidate) => candidate.textContent === "Retry recovery",
      );
      expect(button).toBeTruthy();
      return button as HTMLButtonElement;
    });
    retry.click();
    await vi.waitFor(() => expect(reload).toHaveBeenCalledOnce());

    expect(state.invoke).not.toHaveBeenCalledWith(
      "native_device_backup_retry_recovery",
      expect.anything(),
    );
  });
});
