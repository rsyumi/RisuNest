import { mount, tick, unmount } from "svelte";
import { beforeEach, afterEach, expect, it, vi } from "vitest";
import App from "../src/App.svelte";
import type { Backend, Status } from "../src/api";

let component: ReturnType<typeof mount> | undefined;
let target: HTMLDivElement;
let snapshot: Status;
let backend: Backend;
beforeEach(() => {
  vi.useFakeTimers();
  vi.stubGlobal(
    "fetch",
    vi.fn(() => {
      throw new Error("network prohibited in manager component tests");
    }),
  );
  target = document.createElement("div");
  document.body.append(target);
  snapshot = {
    revision: "synthetic:0",
    uptimeSeconds: 5,
    connection: {
      endpoint: "https://sync.example.com",
      cloudflared: null,
      registryUrl: "https://registry.example.com",
      registryEnabled: true,
      uuid: "synthetic-uuid",
    },
    connectionState: { mode: "fixed", publication: "published" },
    tunnel: { phase: "external", endpoint: null, error: null },
    publication: { phase: "published", error: null },
    storage: {
      measuredAt: 1,
      totalBytes: 100,
      availableBytes: 1000,
      dataBytes: 50,
      databaseBytes: 40,
      temporaryBytes: 5,
      otherBytes: 5,
      error: null,
    },
    devices: [],
    defaultRegistryUrl: "https://registry.example.com",
  };
  backend = {
    status: vi.fn(async () => structuredClone(snapshot)),
    environment: vi.fn(async () => ({
      platform: "windows",
      dataDir: "synthetic",
      cloudflared: "C:\\synthetic\\cloudflared.exe",
      startup: { registered: false, enabled: false, actionMatches: true },
      startupError: null,
      trayStartup: false,
    })),
    mutate: vi.fn(async () => ({})),
    start: vi.fn(async () => {}),
    startup: vi.fn(async () => ({ registered: true, enabled: true })),
    trayStartup: vi.fn(async () => {}),
    requestId: vi.fn(async () => "a".repeat(64)),
    qr: vi.fn(async () => '<svg aria-label="synthetic-qr"></svg>'),
  };
  HTMLDialogElement.prototype.showModal = function () {
    this.setAttribute("open", "");
  };
  HTMLDialogElement.prototype.close = function () {
    this.removeAttribute("open");
  };
});
afterEach(async () => {
  if (component) await unmount(component);
  component = undefined;
  target.remove();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});
async function settle() {
  await Promise.resolve();
  await tick();
  await Promise.resolve();
  await tick();
}
function button(text: string) {
  const result = [...target.querySelectorAll("button")].find(
    (b) => b.textContent?.trim() === text,
  );
  expect(result).toBeDefined();
  return result!;
}
async function open() {
  component = mount(App, { target, props: { backend } });
  await settle();
}

it("keeps a dirty form tied to its original revision during status refresh", async () => {
  await open();
  button("연결").click();
  await settle();
  const input = target.querySelector<HTMLInputElement>("input[type=url]")!;
  input.value = "https://changed.example.com";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  snapshot.revision = "synthetic:1";
  await vi.advanceTimersByTimeAsync(3000);
  await settle();
  expect(input.value).toBe("https://changed.example.com");
  target
    .querySelector("form")!
    .dispatchEvent(new Event("submit", { cancelable: true, bubbles: true }));
  await settle();
  expect(backend.mutate).toHaveBeenCalledWith(
    "connection",
    expect.objectContaining({
      revision: "synthetic:0",
      options: expect.objectContaining({
        endpoint: "https://changed.example.com",
      }),
    }),
  );
});

it("rebases a dirty connection draft after an action from the same form", async () => {
  vi.mocked(backend.mutate).mockImplementation(async (path) => {
    if (path === "tunnel/restart") {
      snapshot.revision = "synthetic:1";
      return structuredClone(snapshot);
    }
    return {};
  });
  snapshot.connectionState.mode = "managed";
  snapshot.connection.cloudflared = "C:\\synthetic\\cloudflared.exe";
  await open();
  button("연결").click();
  await settle();
  const registry = target.querySelector<HTMLInputElement>(
    'input[aria-label="레지스트리 사용"]',
  )!;
  registry.click();
  button("다시 시작").click();
  await vi.waitFor(() =>
    expect(target.textContent).toContain("변경 사항을 적용했습니다."),
  );
  await settle();
  target
    .querySelector("form")!
    .dispatchEvent(new Event("submit", { cancelable: true, bubbles: true }));
  await settle();
  expect(backend.mutate).toHaveBeenLastCalledWith(
    "connection",
    expect.objectContaining({ revision: "synthetic:1" }),
  );
});

it("labels retained overview data as last-known after losing the daemon", async () => {
  await open();
  expect(target.textContent).toContain("서버 실행 중");
  vi.mocked(backend.status).mockRejectedValue("daemon-unavailable");
  await vi.advanceTimersByTimeAsync(3000);
  await settle();
  expect(target.textContent).toContain("마지막으로 확인한 서버 정보");
  expect(target.textContent).not.toContain("서버 실행 중");
});

it("shows a registration URI once and clears it on modal close", async () => {
  const uri = "risunestlocal://sync-server/register#SYNTHETIC_SECRET_ONLY_TEST";
  vi.mocked(backend.mutate).mockResolvedValue({ uri });
  await open();
  button("기기 등록").click();
  await settle();
  const name = target.querySelector<HTMLInputElement>(
    'input[aria-label="기기 이름"]',
  )!;
  name.value = "시험 기기";
  name.dispatchEvent(new Event("input", { bubbles: true }));
  target
    .querySelector("dialog form")!
    .dispatchEvent(new Event("submit", { cancelable: true, bubbles: true }));
  await settle();
  expect(backend.mutate).toHaveBeenCalledTimes(1);
  expect(backend.qr).toHaveBeenCalledWith(uri);
  await vi.waitFor(() =>
    expect(target.querySelector<HTMLTextAreaElement>("textarea")?.value).toBe(
      uri,
    ),
  );
  button("완료").click();
  await settle();
  expect(target.querySelector("textarea")).toBeNull();
  expect(target.innerHTML).not.toContain("SYNTHETIC_SECRET_ONLY_TEST");
});

it("disables issuance while disconnected and does not invent storage values", async () => {
  vi.mocked(backend.status).mockRejectedValue("daemon-unavailable");
  await open();
  expect(button("기기 등록").disabled).toBe(true);
  expect(target.textContent).toContain("서버 연결 안 됨");
  expect(target.textContent).not.toContain("1.28");
  expect(backend.mutate).not.toHaveBeenCalled();
});

it("distinguishes an initial measurement failure from a zero or pending measurement", async () => {
  snapshot.storage = {
    ...snapshot.storage,
    measuredAt: null,
    totalBytes: null,
    availableBytes: null,
    error: "storage-measurement-failed",
  };
  await open();
  expect(target.textContent).toContain("측정할 수 없음");
  expect(target.textContent).not.toContain("마지막 측정값");
});
