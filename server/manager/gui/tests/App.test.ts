import { mount, tick, unmount } from "svelte";
import { beforeEach, afterEach, expect, it, vi } from "vitest";
import App from "../src/App.svelte";
import type { Backend, Environment, Status } from "../src/api";

let component: ReturnType<typeof mount> | undefined;
let target: HTMLDivElement;
let snapshot: Status;
let environmentSnapshot: Environment;
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
  environmentSnapshot = {
    platform: "windows",
    dataDir: "synthetic",
    cloudflared: "C:\\synthetic\\cloudflared.exe",
    startup: { registered: false, enabled: false, actionMatches: true },
    startupError: null,
    trayStartup: false,
    updateSettings: { schema: "risunest-sync-update-settings/v1", policy: "automatic" },
    updateStatus: {
      schema: "risunest-sync-update-status/v1",
      phase: "idle",
      targetVersion: null,
      lastCheckedAt: null,
      lastCompletedAt: null,
      deferredUntil: null,
      reason: null,
      lastFailedVersion: null,
    },
    updateSchedule: { registered: true, enabled: true, actionMatches: true },
    updateScheduleError: null,
  };
  backend = {
    status: vi.fn(async () => structuredClone(snapshot)),
    environment: vi.fn(async () => structuredClone(environmentSnapshot)),
    mutate: vi.fn(async () => ({})),
    start: vi.fn(async () => {}),
    startup: vi.fn(async () => ({ registered: true, enabled: true })),
    trayStartup: vi.fn(async () => {}),
    updatePolicy: vi.fn(async () => {}),
    updateCheck: vi.fn(async () => ({ result: "current" })),
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

it("changes the shared update policy from run settings", async () => {
  await open();
  button("실행 설정").click();
  await settle();
  const select = target.querySelector<HTMLSelectElement>(
    'select[aria-label="업데이트 정책"]',
  )!;
  expect(select.value).toBe("automatic");
  select.value = "notify";
  select.dispatchEvent(new Event("change", { bubbles: true }));
  await settle();
  expect(backend.updatePolicy).toHaveBeenCalledWith("notify");
});

it("shows a schedule repair failure and enables retry after the next environment refresh", async () => {
  environmentSnapshot.updateSchedule = null;
  environmentSnapshot.updateScheduleError = "update-already-running";
  await open();
  button("실행 설정").click();
  await settle();
  const select = target.querySelector<HTMLSelectElement>(
    'select[aria-label="업데이트 정책"]',
  )!;
  expect(target.textContent).toContain("예약 업데이트 상태를 확인하지 못했습니다.");
  expect(select.disabled).toBe(true);

  environmentSnapshot.updateSchedule = {
    registered: true,
    enabled: true,
    actionMatches: true,
  };
  environmentSnapshot.updateScheduleError = null;
  await vi.advanceTimersByTimeAsync(15_000);
  await settle();
  expect(target.textContent).not.toContain("예약 업데이트 상태를 확인하지 못했습니다.");
  expect(select.disabled).toBe(false);
  expect(backend.environment).toHaveBeenCalledTimes(3);
});

it("keeps a dirty connection draft on its page and away from update actions", async () => {
  await open();
  button("연결").click();
  await settle();
  const input = target.querySelector<HTMLInputElement>("input[type=url]")!;
  input.value = "https://unsaved.example.com";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  button("실행 설정").click();
  await settle();
  expect(target.querySelector<HTMLInputElement>("input[type=url]")?.value).toBe(
    "https://unsaved.example.com",
  );
  expect(target.textContent).toContain("아직 적용하지 않은 변경");
  expect(backend.updateCheck).not.toHaveBeenCalled();
});

it("disables manual update checks while a dialog is open", async () => {
  await open();
  button("기기 등록").click();
  button("실행 설정").click();
  await settle();
  const check = button("업데이트 확인 및 적용");
  expect(check.disabled).toBe(true);
  check.click();
  expect(backend.updateCheck).not.toHaveBeenCalled();
});

it("reports a deferred manual update without claiming the app is current", async () => {
  vi.mocked(backend.updateCheck).mockResolvedValue({
    result: "deferred",
    value: "management-active",
  });
  await open();
  button("실행 설정").click();
  await settle();
  button("업데이트 확인 및 적용").click();
  await vi.waitFor(() =>
    expect(target.textContent).toContain("관리 작업이 끝난 뒤 다시 확인"),
  );
  expect(backend.updateCheck).toHaveBeenCalledWith(false);
  expect(target.textContent).not.toContain("현재 최신 버전입니다.");
});

it("coordinates an automatic update after the scheduled checker found the open GUI", async () => {
  environmentSnapshot.updateStatus.phase = "deferred";
  environmentSnapshot.updateStatus.reason =
    "management-app-open-or-install-locked";
  environmentSnapshot.updateStatus.targetVersion = "2.0.0";
  vi.mocked(backend.updateCheck).mockResolvedValue({
    result: "started",
    value: "2.0.0",
  });
  await open();
  await settle();
  expect(backend.updateCheck).toHaveBeenCalledTimes(1);
  expect(backend.updateCheck).toHaveBeenCalledWith(true);
  expect(target.textContent).toContain("관리 앱을 종료하고 적용");
});

it("does not hot-loop automatic handoff while another manager keeps replacement deferred", async () => {
  environmentSnapshot.updateStatus.phase = "deferred";
  environmentSnapshot.updateStatus.reason =
    "management-app-open-or-install-locked";
  environmentSnapshot.updateStatus.targetVersion = "2.0.0";
  vi.mocked(backend.updateCheck).mockResolvedValue({
    result: "deferred",
    value: "management-app-open-or-install-locked",
  });
  await open();
  await vi.waitFor(() => expect(backend.updateCheck).toHaveBeenCalledTimes(1));
  await vi.advanceTimersByTimeAsync(45_000);
  await settle();
  expect(backend.updateCheck).toHaveBeenCalledTimes(1);
});
