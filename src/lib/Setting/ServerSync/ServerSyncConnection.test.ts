import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mount, tick, unmount } from "svelte";
import type { ServerSyncSnapshot } from "src/ts/storage/sync/serverSyncController";

const state = vi.hoisted(() => {
  let listener: ((snapshot: unknown) => void) | undefined;
  const controller = {
    snapshot: vi.fn(),
    subscribe: vi.fn((next: (snapshot: unknown) => void) => {
      listener = next;
      return () => {
        listener = undefined;
      };
    }),
    initialize: vi.fn(),
    bind: vi.fn(async () => {}),
    reregister: vi.fn(async () => {}),
    synchronize: vi.fn(async () => {}),
    unbind: vi.fn(async () => {}),
    pause: vi.fn(async () => {}),
    waitForIdle: vi.fn(async () => {}),
  };
  return { controller, emit: (value: unknown) => listener?.(value) };
});
vi.mock("src/ts/storage/sync/serverSyncProduction", () => ({
  getServerSyncController: () => state.controller,
  listServerSyncBackups: vi.fn(async () => []),
  restoreServerSyncBackup: vi.fn(),
}));
vi.mock("src/lang", async () => ({
  language: (await import("src/lang/en")).languageEnglish,
}));
vi.mock("src/ts/alert", () => ({ alertConfirm: vi.fn() }));
import { serverRegistrationInbox } from "src/ts/storage/sync/serverSyncRegistrationInbox";
import { receiveServerRegistration } from "src/ts/storage/sync/serverSyncRegistrationDispatch";
import vector from "../../../../crates/sync-connect/tests/registration-vector.json";
import ServerSyncConnection from "./ServerSyncConnection.svelte";

let target: HTMLDivElement;
let component: ReturnType<typeof mount> | undefined;
const idle = (): ServerSyncSnapshot => ({
  running: false,
  paused: false,
  error: "",
});
beforeEach(() => {
  vi.clearAllMocks();
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
describe("shared server connection view", () => {
  it("prefills public navigation without binding or initializing an engine", async () => {
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
    const inputs = [...target.querySelectorAll<HTMLInputElement>("form input")];
    expect(inputs.map((input) => input.value)).toEqual([
      "https://example.test/",
      "lib",
      "",
      "",
    ]);
    expect(state.controller.bind).not.toHaveBeenCalled();
    expect(state.controller.initialize).not.toHaveBeenCalled();
    expect(state.controller.synchronize).not.toHaveBeenCalled();
  });
  it("keeps an existing identity when another endpoint arrives", async () => {
    state.controller.snapshot.mockReturnValue({
      ...idle(),
      status: {
        configured: true,
        endpoint: "https://bound.test/",
        libraryId: "bound",
        deviceId: "device",
        dirtyRecords: 0,
        fullScan: false,
        registrationRequired: false,
        operationPending: false,
      },
    });
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
    expect(target.querySelector("form")).toBeNull();
    expect(state.controller.unbind).not.toHaveBeenCalled();
    expect(state.controller.bind).not.toHaveBeenCalled();
    expect(state.controller.reregister).not.toHaveBeenCalled();
  });
  it("requires explicit submission and clears the token after binding", async () => {
    component = mount(ServerSyncConnection, { target });
    await tick();
    const values = ["https://example.test", "lib", "device", "a".repeat(64)];
    [...target.querySelectorAll<HTMLInputElement>("form input")].forEach(
      (input, index) => {
        input.value = values[index];
        input.dispatchEvent(new Event("input", { bubbles: true }));
      },
    );
    target
      .querySelector("form")!
      .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await tick();
    await Promise.resolve();
    await tick();
    expect(state.controller.bind).toHaveBeenCalledExactlyOnceWith({
      endpoint: "https://example.test/",
      libraryId: "lib",
      deviceId: "device",
      token: "a".repeat(64),
    });
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
    state.controller.snapshot.mockReturnValue({
      ...idle(),
      status: {
        configured: true,
        endpoint: "https://bound.test/",
        libraryId: "lib",
        deviceId: "device",
      },
    });
    component = mount(ServerSyncConnection, { target });
    await tick();
    const pause = [...target.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Pause"),
    )!;
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
});

it("receives a cold registration after mounting, submits directory only explicitly, and discards secrets", async () => {
  await receiveServerRegistration(vector.uri, () => {
    component = mount(ServerSyncConnection, {
      target,
      props: { initialNavigation: {} },
    });
  });
  await tick();
  const inputs = [...target.querySelectorAll<HTMLInputElement>("form input")];
  expect(inputs.map((input) => input.value)).toEqual([
    vector.registration.endpoint,
    vector.registration.libraryId,
    vector.registration.deviceId,
    vector.registration.token,
  ]);
  expect(target.textContent).toContain(vector.registration.directory.baseUrl);
  expect(target.textContent).not.toContain(vector.registration.directory.key);
  expect(state.controller.bind).not.toHaveBeenCalled();
  target
    .querySelector("form")!
    .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await vi.waitFor(() =>
    expect(state.controller.bind).toHaveBeenCalledExactlyOnceWith({
      ...vector.registration,
      endpoint: "https://sync.example/base/",
    }),
  );
  await tick();
  expect(inputs.at(-1)!.value).toBe("");
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
  [...target.querySelectorAll("button")]
    .find((button) => button.textContent?.includes("Clear registration input"))!
    .click();
  await tick();
  expect(
    [...target.querySelectorAll<HTMLInputElement>("form input")].map(
      (input) => input.value,
    ),
  ).toEqual(["", "", "", ""]);
  expect(target.textContent).not.toContain(
    vector.registration.directory.baseUrl,
  );
  expect(state.controller.bind).not.toHaveBeenCalled();
  expect(await receiveServerRegistration(vector.uri, () => {})).toBe(true);
});
it("rejects OS registration for an existing identity without an implicit replacement", async () => {
  state.controller.snapshot.mockReturnValue({
    ...idle(),
    status: {
      configured: true,
      endpoint: "https://bound.test/",
      libraryId: "bound",
      deviceId: "device",
    },
  });
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
  expect(
    [...target.querySelectorAll<HTMLInputElement>("form input")].map(
      (input) => input.value,
    ),
  ).toEqual([
    vector.registration.endpoint,
    vector.registration.libraryId,
    vector.registration.deviceId,
    vector.registration.token,
  ]);
  expect(state.controller.bind).not.toHaveBeenCalled();
});
