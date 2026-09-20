<script lang="ts">
  import { onMount } from "svelte";
  import {
    LayoutDashboard,
    MonitorSmartphone,
    Link,
    SlidersHorizontal,
    Plus,
    RefreshCw,
    X,
    Copy,
  } from "@lucide/svelte";
  import {
    native,
    message,
    updatePhase,
    type Backend,
    type Status,
    type Environment,
    type Device,
  } from "./api";
  import Overview from "./Overview.svelte";
  import Connections from "./Connections.svelte";
  import Network from "./Network.svelte";
  import Titlebar from "./Titlebar.svelte";
  import type { NetworkSettings } from "./api";
  import logo from "./logo.svg";
  let { backend = native }: { backend?: Backend } = $props();
  let page = $state("overview");
  let status = $state<Status | null>(null);
  let environment = $state<Environment | null>(null);
  let connected = $state(false);
  let busy = $state(false);
  let notice = $state("");
  let dialog = $state<"register" | "issued" | "revoke" | "stop" | "leave" | null>(null);
  let name = $state("");
  let selected = $state<Device | null>(null);
  let uri = $state("");
  let svg = $state("");
  let connectionDraftDirty = $state(false);
  let networkDirty = $state(false);
  let connectionDirty = $derived(connectionDraftDirty || networkDirty);
  let pendingPage = "";
  let updating = $state(false);
  let dialogElement: HTMLDialogElement;
  // The webview draws the title bar where the OS frame is hidden (Windows) or overlaid (macOS).
  const titlebar: "windows" | "macos" | null = /Windows/.test(navigator.userAgent)
    ? "windows"
    : /Mac/.test(navigator.userAgent)
      ? "macos"
      : null;
  let automaticUpdateRetryAfter = 0;
  const navigation = [
    { id: "overview", label: "개요", icon: LayoutDashboard, description: "" },
    {
      id: "devices",
      label: "기기",
      icon: MonitorSmartphone,
      description: "이 라이브러리에 등록된 기기를 관리합니다.",
    },
    {
      id: "connection",
      label: "연결",
      icon: Link,
      description: "고정 주소와 임시 주소, 주소 레지스트리를 설정합니다.",
    },
    {
      id: "settings",
      label: "실행 설정",
      icon: SlidersHorizontal,
      description: "서버 실행 상태와 로그인 시 자동 실행 여부를 관리합니다.",
    },
  ];
  const current = $derived(navigation.find((n) => n.id === page)!);
  let refreshSequence = 0;
  async function refresh() {
    const sequence = ++refreshSequence;
    try {
      const next = await backend.status();
      if (sequence === refreshSequence) {
        status = next;
        connected = true;
      }
    } catch {
      if (sequence === refreshSequence) connected = false;
    }
  }
  async function loadEnvironment() {
    try {
      const next = await backend.environment();
      environment = next;
      if (
        next.updateSettings.policy === "automatic" &&
        next.updateStatus.phase === "deferred" &&
        [
          "management-active",
          "management-app-open-or-install-locked",
        ].includes(next.updateStatus.reason ?? "") &&
        !busy &&
        !updating &&
        dialog === null &&
        !connectionDirty &&
        Date.now() >= automaticUpdateRetryAfter
      ) {
        automaticUpdateRetryAfter = Date.now() + 60 * 60 * 1000;
        queueMicrotask(() => void checkUpdate(true));
      }
    } catch {
      notice = "실행 설정을 확인하지 못했습니다.";
    }
  }
  function navigate(id: string) {
    if (id !== page && connectionDirty) {
      pendingPage = id;
      open("leave");
      return;
    }
    page = id;
    if (id === "settings") void loadEnvironment();
  }
  function leavePage() {
    const next = pendingPage;
    close();
    connectionDraftDirty = false;
    networkDirty = false;
    navigate(next);
  }
  async function saveNetwork(settings: NetworkSettings): Promise<boolean> {
    busy = true;
    notice = "";
    try {
      await backend.network(settings);
      await loadEnvironment();
      notice = "네트워크 설정을 저장했습니다. 다음 서버 시작부터 적용됩니다.";
      return true;
    } catch (error) {
      notice = message(error);
      return false;
    } finally { busy = false; }
  }
  async function refreshAll() {
    await Promise.all([refresh(), loadEnvironment()]);
  }
  onMount(() => {
    let stopped = false;
    async function poll() {
      await refresh();
      if (!stopped) timer = setTimeout(poll, 3000);
    }
    let timer: ReturnType<typeof setTimeout>;
    const environmentTimer = setInterval(() => void loadEnvironment(), 15000);
    void poll();
    void loadEnvironment();
    return () => {
      stopped = true;
      clearTimeout(timer);
      clearInterval(environmentTimer);
      uri = "";
      svg = "";
    };
  });
  async function mutate(
    path: string,
    body: Record<string, unknown> = {},
  ): Promise<string | null> {
    if (!connected || busy || !status) return null;
    busy = true;
    notice = "";
    try {
      const result = await backend.mutate(path, {
        revision: status.revision,
        ...body,
      });
      const resultRevision =
        result &&
        typeof result === "object" &&
        "revision" in result &&
        typeof result.revision === "string"
          ? result.revision
          : null;
      await refresh();
      notice = "변경 사항을 적용했습니다.";
      return resultRevision ?? status?.revision ?? null;
    } catch (error) {
      notice = message(error);
      await refresh();
      return null;
    } finally {
      busy = false;
    }
  }
  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      notice = "복사했습니다.";
    } catch {
      notice = "복사하지 못했습니다. 입력란의 내용을 선택해 복사하세요.";
    }
  }
  function open(kind: typeof dialog, device: Device | null = null) {
    dialog = kind;
    selected = device;
    name = "";
    uri = "";
    svg = "";
    notice = "";
    dialogElement.showModal();
  }
  function close() {
    if (busy) return;
    dialogElement.close();
    dialog = null;
    uri = "";
    svg = "";
    selected = null;
    name = "";
  }
  async function register() {
    if (!status || !connected || busy) return;
    busy = true;
    notice = "";
    try {
      const requestId = await backend.requestId();
      const result = (await backend.mutate("devices", {
        revision: status.revision,
        name,
        requestId,
      })) as { uri: string };
      let code = "";
      try {
        code = await backend.qr(result.uri);
      } catch {
        code = "";
      }
      await refresh();
      uri = result.uri;
      svg = code;
      dialog = "issued";
    } catch (error) {
      notice = message(error) + " 다시 발급하기 전에 기기 목록을 확인하세요.";
      await refresh();
    } finally {
      busy = false;
    }
  }
  async function start() {
    busy = true;
    notice = "";
    try {
      await backend.start();
      await refresh();
    } catch (error) {
      notice = message(error);
    } finally {
      busy = false;
    }
  }
  async function startup(enabled: boolean) {
    busy = true;
    notice = "";
    try {
      await backend.startup(enabled ? "install" : "remove");
      await loadEnvironment();
      notice = "자동 실행 설정을 변경했습니다.";
    } catch (error) {
      notice = message(error);
    } finally {
      busy = false;
    }
  }
  async function tray(enabled: boolean) {
    busy = true;
    notice = "";
    try {
      await backend.trayStartup(enabled);
      await loadEnvironment();
    } catch (error) {
      notice = message(error);
    } finally {
      busy = false;
    }
  }
  async function updatePolicy(policy: "automatic" | "notify" | "off") {
    busy = true;
    notice = "";
    try {
      await backend.updatePolicy(policy);
      await loadEnvironment();
      notice = "업데이트 정책과 예약 확인 설정을 변경했습니다.";
    } catch (error) {
      notice = message(error);
      await loadEnvironment();
    } finally {
      busy = false;
    }
  }
  function deferredUpdateMessage(reason?: string): string {
    if (reason === "management-active")
      return "관리 작업이 끝난 뒤 다시 확인하세요.";
    if (reason === "management-app-open-or-install-locked")
      return "다른 관리 앱을 닫거나 설치 잠금이 해제된 뒤 다시 확인하세요.";
    if (reason === "server-busy")
      return "서버 작업이 끝난 뒤 업데이트를 다시 확인합니다.";
    return `업데이트가 연기되었습니다${reason ? `: ${reason}` : "."}`;
  }
  async function checkUpdate(automatic = false) {
    if (busy || updating || dialog !== null || connectionDirty) {
      if (!automatic)
        notice = "진행 중인 작업이나 적용하지 않은 설정을 마친 뒤 업데이트를 확인하세요.";
      return;
    }
    updating = true;
    busy = true;
    let exitingForUpdate = false;
    notice = automatic
      ? "관리 앱을 종료하고 예약된 업데이트를 안전하게 적용합니다."
      : "업데이트를 확인하고 있습니다.";
    try {
      const outcome = await backend.updateCheck(automatic);
      if (outcome.result === "started") {
        exitingForUpdate = true;
        notice = "관리 앱을 종료하고 적용합니다.";
      } else if (outcome.result === "available") {
        notice = `Sync ${outcome.value ?? "새 버전"} 업데이트를 사용할 수 있습니다.`;
      } else if (outcome.result === "deferred") {
        notice = deferredUpdateMessage(outcome.value);
      } else if (outcome.result === "skipped") {
        notice = "예약된 업데이트 확인 시간이 아직 되지 않았습니다.";
      } else if (outcome.result === "completed") {
        notice = `Sync ${outcome.value ?? "새 버전"} 업데이트를 완료했습니다.`;
      } else {
        notice = "현재 최신 버전입니다.";
      }
      await loadEnvironment();
    } catch (error) {
      notice = message(error);
      await loadEnvironment();
    } finally {
      if (!exitingForUpdate) {
        busy = false;
        updating = false;
      }
    }
  }
</script>

<div class="app-shell" class:mac={titlebar === "macos"}>
  {#if titlebar}<Titlebar platform={titlebar} />{/if}
  <div class="body">
  <aside>
    <div class="identity">
      <img src={logo} alt="RisuNest" />
      <div><strong>RisuNest</strong><small>동기화 서버</small></div>
    </div>
    <small class="nav-label">서버 관리</small>
    <nav aria-label="서버 관리">
      {#each navigation as item}<button
          class:active={page === item.id}
          aria-current={page === item.id ? "page" : undefined}
          onclick={() => navigate(item.id)}
          ><item.icon size={17} />{item.label}</button
        >{/each}
    </nav>
    <div class="sidebar-bottom">
      <span class:offline={!connected}
        >{connected ? "로컬 서버 연결됨" : "서버 연결 안 됨"}</span
      ><small>RisuNest Sync · 0.1</small>
    </div>
  </aside>
  <main>
    <header class="page-heading">
      <div>
        <h1>{current.label}</h1>
        {#if current.description}<p>{current.description}</p>{/if}
      </div>
      <div class="actions">
        <button
          class="icon-button"
          aria-label="새로 고침"
          onclick={refreshAll}
          disabled={busy}><RefreshCw size={17} /></button
        >{#if page === "overview" || page === "devices"}<button
            class="primary"
            disabled={!connected || busy}
            onclick={() => open("register")}
            ><Plus size={17} /> 기기 등록</button
          >{/if}
      </div>
    </header>
    {#if notice}<div role="status" class="notice-banner">{notice}</div>{/if}
    {#if !connected}<section class="card disconnected">
        <h2>서버 연결 안 됨</h2>
        <p>
          서버를 시작하거나 연결 상태를 다시 확인하세요.{status
            ? " 마지막으로 확인한 정보를 표시합니다."
            : ""}
        </p>
        <button class="primary" disabled={busy} onclick={start}
          >서버 시작</button
        >
      </section>{/if}
    {#if page === "connection" && environment}
      <Network settings={environment.network} listener={connected ? status?.listener ?? null : null} {busy} save={saveNetwork} activity={(dirty) => (networkDirty = dirty)} />
    {/if}
    {#if status}
      {#if page === "overview"}<Overview
          {status}
          {connected}
          {copy}
          showDevices={() => (page = "devices")}
        />
      {:else if page === "devices"}
        <div class="card device-list">
          {#each status.devices as device}<div class="device-row">
              <span class="device-icon"><MonitorSmartphone size={18} /></span>
              <div>
                <strong>{device.name || device.id.slice(0, 12)}</strong><small
                  >{device.id}</small
                >{#if device.pending}<small>처리 중인 작업이 있습니다.</small
                  >{/if}
              </div>
              {#if device.revoked}<span class="pill neutral">해제됨</span
                >{:else}<button
                  class="danger text-button"
                  disabled={!connected || busy}
                  onclick={() => open("revoke", device)}>해제</button
                >{/if}
            </div>{:else}<p class="empty">등록된 기기가 없습니다.</p>{/each}
        </div>
        <p class="notice">
          해제한 기기는 다시 등록하기 전까지 연결할 수 없습니다.<br
          />라이브러리의 대화와 파일은 그대로 유지됩니다.
        </p>
      {:else if page === "connection"}<Connections
          {status}
          {environment}
          busy={busy || !connected}
          {mutate}
          {copy}
          activity={(active) => (connectionDraftDirty = active)}
        />{/if}
    {/if}
    {#if page === "settings"}
      <section class="card settings-group">
        <div class="setting">
          <div class="setting-heading">
            <h2>Sync 업데이트</h2>
            <span class="pill">{updatePhase(environment?.updateStatus.phase ?? "idle")}</span>
          </div>
          <label
            >업데이트 정책<select
              aria-label="업데이트 정책"
              value={environment?.updateSettings.policy ?? "automatic"}
              disabled={busy || !environment || !!environment.updateScheduleError}
              onchange={(e) =>
                updatePolicy(
                  e.currentTarget.value as "automatic" | "notify" | "off",
                )}
              ><option value="automatic">안전할 때 자동 적용</option><option
                value="notify">새 버전만 알림</option
              ><option value="off">예약 확인 끄기</option></select
            ></label
          >
          <p>
            관리 화면을 닫아도 예약 확인이 실행됩니다. 자동 적용은 작업이
            없고 서버가 안전하게 종료될 때만 진행합니다.
          </p>
          {#if environment?.updateStatus.targetVersion}<p>
              대상 버전: {environment.updateStatus.targetVersion}
            </p>{/if}
          {#if environment?.updateStatus.reason}<p class="warning">
              마지막 결과: {environment.updateStatus.reason}
            </p>{/if}
          {#if environment?.updateScheduleError}<p class="warning">
              예약 업데이트 상태를 확인하지 못했습니다.
            </p>{/if}
          <div class="actions">
            <button
              disabled={busy || updating || dialog !== null || connectionDirty}
              onclick={() => void checkUpdate(false)}
              >{environment?.updateSettings.policy === "notify"
                ? "업데이트 확인"
                : "업데이트 확인 및 적용"}</button
            >
          </div>
        </div>
        <div class="setting">
          <div class="setting-heading">
            <h2>로그인 시 서버 자동 실행</h2>
            <label class="switch"
              ><input
                type="checkbox"
                aria-label="서버 자동 실행"
                checked={(environment?.startup?.enabled ?? false) &&
                  (environment?.startup?.actionMatches ?? false)}
                disabled={busy || !environment || !!environment.startupError}
                onchange={(e) => startup(e.currentTarget.checked)}
              /><span></span></label
            >
          </div>
          <p>관리 화면을 닫아도 서버는 계속 동작합니다.</p>
          {#if environment?.startupError}<p class="warning">
              자동 실행 상태를 확인하지 못했습니다.
            </p>{:else if environment?.startup?.registered &&
              !environment.startup.actionMatches}<p class="warning">
              이전 설치 위치의 자동 실행 설정입니다. 다시 등록하거나 해제하세요.
            </p>{/if}
        </div>
        <div class="setting">
          <div class="setting-heading">
            <h2>로그인 시 트레이 표시</h2>
            <label class="switch"
              ><input
                type="checkbox"
                aria-label="트레이 표시"
                checked={environment?.trayStartup ?? false}
                disabled={busy || !environment}
                onchange={(e) => tray(e.currentTarget.checked)}
              /><span></span></label
            >
          </div>
          <p>트레이에서 서버 상태를 확인하고 관리 화면을 열 수 있습니다.</p>
        </div>
      </section>
      <section class="card settings-group">
        <div class="setting">
          <div class="setting-heading">
            <h2>서버 실행 상태</h2>
            <span class="pill">{connected ? "실행 중" : "연결 안 됨"}</span>
          </div>
          <p>서버를 중지하면 연결된 기기의 동기화가 멈춥니다.</p>
          <div class="actions">
            {#if connected}<button disabled={busy} onclick={() => open("stop")}
                >서버 중지</button
              >{:else}<button disabled={busy} onclick={start}>서버 시작</button
              >{/if}
          </div>
        </div>
      </section>
      <p class="notice">
        자동 실행 설정을 꺼도 실행 중인 서버는 중지되지 않습니다.
      </p>
    {/if}
  </main>
  </div>
</div>
<dialog
  bind:this={dialogElement}
  oncancel={(e) => {
    e.preventDefault();
    close();
  }}
>
  <button
    class="dialog-close icon-button"
    aria-label="닫기"
    onclick={close}
    disabled={busy}><X size={20} /></button
  >
  {#if dialog === "leave"}
    <p>저장하지 않은 변경사항이 있습니다. 정말로 이동하시겠습니까? 변경한 내용이 초기화됩니다.</p>
    <div class="actions"><button onclick={leavePage}>네</button><button onclick={close}>아니오</button></div>
  {:else if dialog === "register"}<h2>새 기기 등록</h2>
    <p>등록할 기기의 이름을 입력하세요.</p>
    <form
      onsubmit={(e) => {
        e.preventDefault();
        void register();
      }}
    >
      <input
        aria-label="기기 이름"
        bind:value={name}
        maxlength="80"
        required
        disabled={busy}
      />
      <div class="dialog-actions">
        <button type="button" onclick={close} disabled={busy}>취소</button
        ><button class="primary" disabled={busy || !connected}
          >등록 코드 만들기</button
        >
      </div>
    </form>
  {:else if dialog === "issued"}<h2>기기 등록 코드</h2>
    <p>
      등록할 기기의 RisuNest 앱에서 QR 코드를 스캔하거나 등록 링크를 붙여
      넣으세요.
    </p>
    {#if svg}<div class="qr">{@html svg}</div>{/if}<textarea
      readonly
      aria-label="기기 등록 링크"
      value={uri}
    ></textarea>
    <p>이 화면을 닫으면 등록 코드를 다시 확인할 수 없습니다.</p>
    <div class="dialog-actions">
      <button onclick={() => copy(uri)}><Copy size={15} /> 링크 복사</button
      ><button class="primary" onclick={close}>완료</button>
    </div>
  {:else if dialog === "revoke" && selected}<h2>이 기기를 해제할까요?</h2>
    <p>
      {selected.name || selected.id}의 접근 권한을 해제합니다. 다시 연결하려면
      새로 등록해야 합니다. 라이브러리의 대화와 파일은 삭제되지 않습니다.
    </p>
    <div class="dialog-actions">
      <button onclick={close} disabled={busy}>취소</button><button
        class="primary"
        disabled={busy || !connected}
        onclick={async () => {
          if (await mutate(`devices/${selected!.id}/revoke`)) close();
        }}>해제</button
      >
    </div>
  {:else if dialog === "stop"}<h2>서버를 중지할까요?</h2>
    <p>
      연결된 기기의 동기화가 멈춥니다. 저장된 데이터와 기기 등록 정보는
      유지됩니다.
    </p>
    <div class="dialog-actions">
      <button onclick={close} disabled={busy}>취소</button><button
        class="primary"
        disabled={busy || !connected}
        onclick={async () => {
          if (await mutate("shutdown")) close();
        }}>중지</button
      >
    </div>{/if}
  {#if notice}<p role="status">{notice}</p>{/if}
</dialog>
