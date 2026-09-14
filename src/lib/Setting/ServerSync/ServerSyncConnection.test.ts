import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, tick, unmount } from "svelte";
import type { ServerSyncSnapshot } from "src/ts/storage/sync/serverSyncController";

const state = vi.hoisted(() => {
  let listener: ((snapshot: unknown) => void) | undefined;
  const trace: string[] = [];
  const controller = {
    snapshot: vi.fn(),
    subscribe: vi.fn((next: (snapshot: unknown) => void) => {
      listener = next;
      return () => {
        listener = undefined;
      };
    }),
    initialize: vi.fn(),
    bind: vi.fn(async () => {
      trace.push("bind");
    }),
    reregister: vi.fn(async () => {
      trace.push("reregister");
    }),
    synchronize: vi.fn(async () => {
      trace.push("synchronize");
    }),
    unbind: vi.fn(async () => {}),
    pause: vi.fn(async () => {}),
    waitForIdle: vi.fn(async () => {}),
  };
  const setPolicy = vi.fn(async (policy: string) => {
    trace.push(`policy:${policy}`);
  });
  return { controller, setPolicy, trace, emit: (value: unknown) => listener?.(value) };
});
vi.mock("src/ts/storage/sync/serverSyncProduction", () => ({
  getServerSyncController: () => state.controller,
  getServerSyncBackupInventory: vi.fn(async () => ({
    items: [],
    next: null,
    completeCount: 0,
    completeBytes: 0,
    incompleteCount: 0,
    incompleteBytes: 0,
    diskBytes: 0,
  })),
  getServerSyncCacheUsage: vi.fn(async () => ({
    totalBytes: 0,
    protectedBytes: 0,
    reclaimableBytes: 0,
    blockedReason: null,
  })),
  cleanupServerSyncCache: vi.fn(),
  deleteServerSyncBackup: vi.fn(),
  listServerSyncBackups: vi.fn(async () => []),
  restoreServerSyncBackup: vi.fn(),
}));
vi.mock("src/ts/storage/sync/serverAssetResidency", () => ({
  getAssetResidencyStatus: vi.fn(async () => ({
    policy: "full",
    localBytes: 0,
    remoteBytes: 0,
    remoteObjects: 0,
    unavailableObjects: 0,
    evictedBytes: 0,
  })),
  setAssetResidencyPolicy: state.setPolicy,
  evictLocalAssets: vi.fn(),
  cancelAssetResidencyOperation: vi.fn(),
}));
vi.mock("src/lang", async () => ({
  language: (await import("src/lang/en")).languageEnglish,
}));
vi.mock("src/ts/alert", () => ({ alertConfirm: vi.fn() }));
import { serverRegistrationInbox } from "src/ts/storage/sync/serverSyncRegistrationInbox";
import { receiveServerRegistration } from "src/ts/storage/sync/serverSyncRegistrationDispatch";
import { languageEnglish } from "src/lang/en";
import vector from "../../../../crates/sync-connect/tests/registration-vector.json";
import ServerSyncConnection from "./ServerSyncConnection.svelte";

const text = languageEnglish.risuNest.serverSync;
let target: HTMLDivElement;
let component: ReturnType<typeof mount> | undefined;
const idle = (): ServerSyncSnapshot => ({
  running: false,
  paused: false,
  error: "",
});
const bound = (endpoint = "https://bound.test/", libraryId = "lib"): ServerSyncSnapshot => ({
  ...idle(),
  status: {
    localRevision: 1,
    reconciling: false,
    configured: true,
    endpoint,
    libraryId,
    deviceId: "device",
    head: null,
    dirtyRecords: 0,
    fullScan: false,
    registrationRequired: false,
    operationPending: false,
  },
});
const button = (label: string) =>
  [...target.querySelectorAll<HTMLButtonElement>("button")].find((b) =>
    b.textContent?.trim().startsWith(label),
  )!;
const manualInputs = () => [...target.querySelectorAll<HTMLInputElement>("form.fields input")];
function fillManual(values: string[]): void {
  manualInputs().forEach((input, index) => {
    input.value = values[index];
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  target
    .querySelector("form.fields")!
    .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
}
function submitReview(): void {
  target
    .querySelector("form.review-form")!
    .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
}
beforeEach(() => {
  vi.clearAllMocks();
  state.trace.length = 0;
  serverRegistrationInbox.clear();
  state.controller.snapshot.mockReturnValue(idle());
  target = document.createElement("div");
  document.body.append(target);
});
afterEach(async () => {
  if (component) await unmount(component);
  component = undefined;
  target.remove();
});
describe("settings server connection", () => {
  it("prefills public navigation in the manual entry without binding or initializing an engine", async () => {
    component = mount(ServerSyncConnection, {
      target,
      props: {
        origin: "deep-link",
        initialNavigation: {
          endpoint: "https://example.test/",
          libraryId: "lib",
        },
      },
    });
    await tick();
    expect(manualInputs().map((input) => input.value)).toEqual([
      "https://example.test/",
      "lib",
      "",
      "",
    ]);
    expect(target.querySelector("details")!.open).toBe(true);
    expect(state.controller.bind).not.toHaveBeenCalled();
    expect(state.controller.initialize).not.toHaveBeenCalled();
    expect(state.controller.synchronize).not.toHaveBeenCalled();
  });
  it("keeps an existing identity when another endpoint arrives", async () => {
    state.controller.snapshot.mockReturnValue(bound("https://bound.test/", "bound"));
    component = mount(ServerSyncConnection, {
      target,
      props: {
        initialNavigation: {
          endpoint: "https://other.test/",
          libraryId: "other",
        },
      },
    });
    await tick();
    expect(target.textContent).toContain("https://bound.test/");
    expect(target.textContent).not.toContain("https://other.test/");
    expect(target.querySelector("form")).toBeNull();
    expect(state.controller.unbind).not.toHaveBeenCalled();
    expect(state.controller.bind).not.toHaveBeenCalled();
    expect(state.controller.reregister).not.toHaveBeenCalled();
  });
  it("binds only after the check is confirmed, stores the policy before the first sync, and drops the token", async () => {
    component = mount(ServerSyncConnection, { target });
    await tick();
    fillManual(["https://example.test", "lib", "device", "a".repeat(64)]);
    await tick();
    expect(state.controller.bind).not.toHaveBeenCalled();
    expect(target.querySelector("dl.review")!.textContent).toContain("https://example.test/");
    expect(target.textContent).not.toContain("a".repeat(64));
    target
      .querySelector<HTMLButtonElement>('[role="radio"]:nth-of-type(2)')!
      .click();
    await tick();
    submitReview();
    await vi.waitFor(() =>
      expect(state.controller.synchronize).toHaveBeenCalledOnce(),
    );
    expect(state.controller.bind).toHaveBeenCalledExactlyOnceWith({
      endpoint: "https://example.test/",
      libraryId: "lib",
      deviceId: "device",
      token: "a".repeat(64),
    });
    expect(state.trace).toEqual(["bind", "policy:remote", "synchronize"]);
    await vi.waitFor(() => expect(target.querySelector("dl.review")).toBeNull());
    expect(
      target.querySelector<HTMLInputElement>('form input[type="password"]')!
        .value,
    ).toBe("");
  });
  it("keeps controls busy until an explicit pause has settled", async () => {
    let settle!: () => void;
    state.controller.waitForIdle.mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          settle = resolve;
        }),
    );
    state.controller.snapshot.mockReturnValue(bound());
    component = mount(ServerSyncConnection, { target });
    await tick();
    const pause = button(text.pause);
    pause.click();
    await vi.waitFor(() =>
      expect(state.controller.waitForIdle).toHaveBeenCalledOnce(),
    );
    await tick();
    expect(pause.disabled).toBe(true);
    settle();
    await vi.waitFor(() => expect(pause.disabled).toBe(false));
    expect(state.controller.pause).toHaveBeenCalledOnce();
  });
  it("unsubscribes on leaving without cancelling the installation's operation", async () => {
    state.controller.snapshot.mockReturnValue({ ...idle(), running: true });
    component = mount(ServerSyncConnection, { target });
    await tick();
    await unmount(component);
    component = undefined;
    expect(state.controller.pause).not.toHaveBeenCalled();
  });
  it("shows the stage, the counters and a retryable transfer failure while synchronization continues", async () => {
    component = mount(ServerSyncConnection, { target });
    await tick();
    state.emit({
      ...bound(),
      running: true,
      progress: "applying",
      cycleItems: { done: 12, total: 48 },
      verifiedBytes: String(2 * 1024 * 1024),
      retryableFailure: "corrupt-chunk",
    });
    await tick();
    expect(target.textContent).toContain(`${text.running}: (corrupt-chunk)`);
    expect(target.textContent).toContain(`${text.progress.applying} · 12 / 48 (25%)`);
    expect(target.textContent).toContain("2.0 MiB");
    expect(target.textContent).not.toContain("Sync could not finish");
    expect(button(text.syncNow).disabled).toBe(true);
    expect(button(text.disconnect).disabled).toBe(true);
  });
  it("re-registers through its row without binding a second server", async () => {
    state.controller.snapshot.mockReturnValue(bound("https://bound.test/", "bound"));
    component = mount(ServerSyncConnection, { target });
    await tick();
    button(text.register).click();
    await tick();
    expect(manualInputs().map((input) => input.value)).toEqual([
      "https://bound.test/",
      "bound",
      "",
      "",
    ]);
    fillManual(["https://bound.test/", "bound", "device2", "b".repeat(64)]);
    await tick();
    expect(button(text.reregister)).toBeDefined();
    submitReview();
    await vi.waitFor(() =>
      expect(state.controller.reregister).toHaveBeenCalledExactlyOnceWith({
        endpoint: "https://bound.test/",
        libraryId: "bound",
        deviceId: "device2",
        token: "b".repeat(64),
      }),
    );
    expect(state.controller.bind).not.toHaveBeenCalled();
    await vi.waitFor(() =>
      expect(state.trace).toEqual(["reregister", "policy:full", "synchronize"]),
    );
  });
});

it("receives a cold registration after mounting, shows the check without secrets, and discards them after binding", async () => {
  await receiveServerRegistration(vector.uri, () => {
    component = mount(ServerSyncConnection, {
      target,
      props: { initialNavigation: {} },
    });
  });
  await tick();
  const review = target.querySelector("dl.review")!;
  expect(review.textContent).toContain("https://sync.example/base/");
  expect(review.textContent).toContain(vector.registration.libraryId);
  expect(review.textContent).toContain(vector.registration.deviceId);
  expect(review.textContent).toContain(vector.registration.directory.baseUrl);
  expect(target.textContent).not.toContain(vector.registration.token);
  expect(target.textContent).not.toContain(vector.registration.directory.key);
  expect(state.controller.bind).not.toHaveBeenCalled();
  submitReview();
  await vi.waitFor(() =>
    expect(state.controller.bind).toHaveBeenCalledExactlyOnceWith({
      ...vector.registration,
      endpoint: "https://sync.example/base/",
    }),
  );
  await vi.waitFor(() => expect(target.querySelector("dl.review")).toBeNull());
  expect(target.textContent).not.toContain(
    vector.registration.directory.baseUrl,
  );
  expect(serverRegistrationInbox.take()).toBeUndefined();
});
it("clears an imported credential without connecting and accepts a deliberate reimport", async () => {
  component = mount(ServerSyncConnection, { target });
  await tick();
  await receiveServerRegistration(vector.uri, () => {});
  await tick();
  expect(target.querySelector("dl.review")).not.toBeNull();
  button(text.otherCode).click();
  await tick();
  expect(target.querySelector("dl.review")).toBeNull();
  expect(manualInputs().map((input) => input.value)).toEqual(["", "", "", ""]);
  expect(target.textContent).not.toContain(
    vector.registration.directory.baseUrl,
  );
  expect(state.controller.bind).not.toHaveBeenCalled();
  expect(await receiveServerRegistration(vector.uri, () => {})).toBe(true);
});
it("rejects OS registration for an existing identity without an implicit replacement", async () => {
  state.controller.snapshot.mockReturnValue(bound("https://bound.test/", "bound"));
  component = mount(ServerSyncConnection, { target });
  await tick();
  await receiveServerRegistration(vector.uri, () => {});
  await tick();
  expect(target.textContent).toContain("https://bound.test/");
  expect(target.textContent).toContain("Disconnect the current server");
  expect(state.controller.bind).not.toHaveBeenCalled();
  expect(state.controller.reregister).not.toHaveBeenCalled();
  expect(serverRegistrationInbox.take()).toBeUndefined();
});

it("retains cold-start credentials until local startup mounts the public destination", async () => {
  await receiveServerRegistration(vector.uri, () => {});
  component = mount(ServerSyncConnection, {
    target,
    props: { initialNavigation: {} },
  });
  await tick();
  await tick();
  const review = target.querySelector("dl.review")!;
  expect(review).not.toBeNull();
  expect(review.textContent).toContain(vector.registration.libraryId);
  expect(review.textContent).toContain(vector.registration.deviceId);
  expect(target.textContent).not.toContain(vector.registration.token);
  expect(state.controller.bind).not.toHaveBeenCalled();
});
