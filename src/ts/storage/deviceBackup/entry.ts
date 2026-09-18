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
}

function showStartupFailure(retry: () => void): void {
  const host = document.createElement("main");
  host.setAttribute("role", "status");
  host.style.cssText =
    "font:16px system-ui;padding:2rem;max-width:42rem;margin:auto;line-height:1.6";
  const heading = document.createElement("h1");
  heading.textContent = "RisuNest backup maintenance";
  const message = document.createElement("p");
  message.textContent =
    "Device storage recovery could not finish. Normal app startup is blocked. Retry recovery to continue.";
  const action = document.createElement("button");
  action.textContent = "Retry recovery";
  action.style.cssText = "font:inherit;padding:.5rem 1rem;cursor:pointer";
  action.onclick = () => {
    action.disabled = true;
    retry();
  };
  host.append(heading, message, action);
  document.getElementById("preloading")?.remove();
  document.body.append(host);
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
    if (session.includesLibrary === true)
      localStorage.setItem("risuNestServerSyncRestoreHold", "true");
    await invoke("native_device_backup_recovery_complete", { sessionId });
    bootstrap = await invoke<NativeDeviceBackupBootstrap>(
      "native_device_backup_bootstrap",
    );
    if (bootstrap.mode !== "normal")
      throw new Error("Native device restore completion is still pending");
  } catch {
    showStartupFailure(reload);
    // Resolve only after a fresh native recovery decision, never import the app on failure.
    await new Promise<never>(() => {});
  }
}
