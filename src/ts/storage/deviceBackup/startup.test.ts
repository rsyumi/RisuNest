import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const state = vi.hoisted(() => ({ invoke: vi.fn(), normalStarted: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: state.invoke }));

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
});
afterEach(() => {
  delete (window as Window & { __TAURI_INTERNALS__?: unknown })
    .__TAURI_INTERNALS__;
  document.body.replaceChildren();
});

describe("device maintenance bootstrap ordering", () => {
  it("does not evaluate the normal app before the native recovery decision", async () => {
    let decide!: (value: unknown) => void;
    state.invoke.mockImplementation(
      () =>
        new Promise((resolve) => {
          decide = resolve;
        }),
    );
    const main = await import("../../../main");
    expect(state.invoke).toHaveBeenCalledExactlyOnceWith(
      "native_device_backup_bootstrap",
      { freshBootstrap: true },
    );
    expect(state.normalStarted).not.toHaveBeenCalled();
    decide({ mode: "normal", session: null });
    await main.default;
    expect(state.normalStarted).toHaveBeenCalledTimes(1);
  });

  it("blocks normal imports when the native recovery decision fails", async () => {
    state.invoke.mockRejectedValue(new Error("synthetic journal failure"));
    await import("../../../main");
    await vi.waitFor(() =>
      expect(document.body.textContent).toContain(
        "Normal app startup is blocked",
      ),
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
});
