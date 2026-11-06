import { invoke } from "@tauri-apps/api/core";
import { checkNativeStartupStatus } from "../../nativeStartup";

interface NativeDeviceBackupSession {
  sessionId?: unknown;
  action?: unknown;
  includesLibrary?: unknown;
}

interface NativeDeviceBackupBootstrap {
  mode: "normal" | "maintenance";
  session: NativeDeviceBackupSession | null;
}

export interface DeviceMaintenanceEntryDependencies {
  reload?: () => void;
  /** Runs once the recovery panel covers the window, which never shows library content. */
  onPanelShown?: () => void;
}

/** The app stylesheet resets buttons to bare text, so the button carries the panel buttons' look. */
const STARTUP_BUTTON_CLASS =
  "rounded-md border border-darkborderc bg-darkbutton px-4 py-2 text-textcolor hover:bg-selected disabled:cursor-not-allowed disabled:opacity-50";

function showStartupFailure(retry: () => void, onPanelShown?: () => void): void {
  const ko = navigator.language.startsWith("ko");
  const host = document.createElement("main");
  host.setAttribute("role", "status");
  // The body does not scroll and #app fills it, so the panel covers the viewport itself.
  host.style.cssText =
    "position:fixed;inset:0;z-index:100;overflow:auto;background:var(--risu-theme-bgcolor);color:var(--risu-theme-textcolor)";
  const panel = document.createElement("section");
  panel.style.cssText =
    "font:16px system-ui;padding:2rem;max-width:42rem;margin:auto;line-height:1.6";
  const heading = document.createElement("h1");
  heading.className = "mb-2 text-2xl font-bold";
  heading.textContent = ko ? "RisuNest 백업 복원" : "RisuNest backup maintenance";
  const message = document.createElement("p");
  message.className = "mb-4";
  message.textContent = ko
    ? "백업 복원을 완료하지 못해 앱을 시작할 수 없습니다. 다시 시도해주세요."
    : "Device storage recovery could not finish. Normal app startup is blocked. Retry recovery to continue.";
  const action = document.createElement("button");
  action.textContent = ko ? "다시 시도" : "Retry recovery";
  action.className = STARTUP_BUTTON_CLASS;
  action.onclick = () => {
    action.disabled = true;
    retry();
  };
  panel.append(heading, message, action);
  host.append(panel);
  document.getElementById("preloading")?.remove();
  document.body.append(host);
  onPanelShown?.();
}

/** Minimal startup entry, intentionally independent of settings, DBState and plugins. */
export async function deviceMaintenanceBeforeBootstrap(
  dependencies: DeviceMaintenanceEntryDependencies = {},
): Promise<void> {
  if (
    !(window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__
  )
    return;
  try {
    await checkNativeStartupStatus();
  } catch {
    // normalMain mounts the shared startup panel, whose bootstrap observes the
    // latched error before it can reach storage, media, or plugin operations.
    return;
  }
  const reload = dependencies.reload ?? (() => location.reload());
  try {
    let bootstrap = await invoke<NativeDeviceBackupBootstrap>(
      "native_device_backup_bootstrap",
    );
    if (bootstrap.mode === "normal") return;
    const session = bootstrap.session;
    if (!session || session.action !== "native-complete")
      throw new Error("Native device restore completion is unavailable");
    if (
      typeof session.sessionId !== "string" ||
      session.sessionId.trim().length === 0
    )
      throw new Error("Native device restore completion has no session ID");
    const sessionId = session.sessionId;
    await invoke("native_device_backup_recovery_complete", { sessionId });
    bootstrap = await invoke<NativeDeviceBackupBootstrap>(
      "native_device_backup_bootstrap",
    );
    if (bootstrap.mode !== "normal")
      throw new Error("Native device restore completion is still pending");
  } catch {
    showStartupFailure(reload, dependencies.onPanelShown);
    // Resolve only after a fresh native recovery decision, never import the app on failure.
    await new Promise<never>(() => {});
  }
}
