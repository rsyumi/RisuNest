import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const startup = vi.hoisted(() => ({ cacheBudget: 0 }));

vi.mock("./ts/polyfill", () => ({}));
vi.mock("core-js/actual", () => ({}));
vi.mock("./ts/storage/database.svelte", () => ({}));
vi.mock("./App.svelte", () => ({ default: {} }));
vi.mock("./ts/bootstrap", async () => {
  const { getRuntimePerformanceBudgets } =
    await import("./ts/runtimePerformanceProfile");
  startup.cacheBudget =
    getRuntimePerformanceBudgets().browserAssetDataUrlCacheBytes;
  return { loadData: vi.fn() };
});
vi.mock("./ts/hotkey", () => ({ initHotkey: vi.fn() }));
vi.mock("./preload", () => ({ preLoadCheck: vi.fn() }));
vi.mock("svelte", () => ({ mount: vi.fn(() => ({})) }));
vi.mock("./ts/storage/deviceBackup/jobRecovery", () => ({
  resumePortableExportsAfterBootstrap: vi.fn(async () => {}),
}));

const deviceSettings = {
  schema: "risunest.device-settings/v1",
  performanceProfile: "low-spec",
  androidKeepAliveDuringGeneration: false,
  nativeFileLogEnabled: true,
};

describe("application startup performance profile", () => {
  beforeEach(() => {
    localStorage.clear();
    document.body.innerHTML = '<div id="app"></div><div id="preloading"></div>';
    startup.cacheBudget = 0;
  });

  afterEach(() => {
    vi.unstubAllEnvs();
    vi.resetModules();
    localStorage.clear();
  });

  it("loads a persisted low-spec profile before bootstrap constructs runtime caches", async () => {
    localStorage.setItem(
      "risuNestDeviceSettings",
      JSON.stringify(deviceSettings),
    );

    // The native maintenance gate resolves before normalMain is imported.
    await (
      await import("./main")
    ).default;

    expect(startup.cacheBudget).toBe(8 * 1024 * 1024);
  });

  it("starts the normal app even when obsolete harness flags are set", async () => {
    vi.stubEnv("MODE", "agent");
    vi.stubEnv("VITE_TOKENIZER_BENCHMARK", "true");
    vi.stubEnv("VITE_STREAMING_SMOKE", "true");
    const { mount } = await import("svelte");
    vi.mocked(mount).mockClear();

    await (
      await import("./main")
    ).default;

    expect(mount).toHaveBeenCalledTimes(1);
    expect(document.getElementById("preloading")).toBeNull();
    expect(window).not.toHaveProperty("__streamingSmoke");
    expect(window).not.toHaveProperty("__RISUNEST_TOKENIZER_BENCHMARK__");
  });
});
