import { invoke } from "@tauri-apps/api/core";
import type {
  DeviceMaintenanceBootstrap,
  DeviceMaintenanceView,
} from "./maintenance";
import { checkNativeStartupStatus } from "../../nativeStartup";

function maintenanceView(
  cancel: () => void,
  retry: () => Promise<void>,
): DeviceMaintenanceView & { dispose(): void } {
  const host = document.createElement("main");
  host.setAttribute("role", "status");
  host.style.cssText =
    "font:16px system-ui;padding:2rem;max-width:42rem;margin:auto;line-height:1.6";
  const heading = document.createElement("h1");
  heading.textContent = "RisuNest backup maintenance";
  const message = document.createElement("p");
  const action = document.createElement("button");
  action.textContent = "Cancel";
  action.style.cssText = "font:inherit;padding:.5rem 1rem;cursor:pointer";
  action.onclick = () => {
    action.disabled = true;
    cancel();
  };
  const details = document.createElement("ul");
  const cancelAction = document.createElement("button");
  cancelAction.textContent = "Cancel restore";
  cancelAction.hidden = true;
  cancelAction.style.cssText = action.style.cssText;
  host.append(heading, message, details, action, cancelAction);
  document.getElementById("preloading")?.remove();
  document.body.append(host);
  return {
    progress(text) {
      message.textContent = text;
    },
    reviewReplacement(sections) {
      message.textContent =
        "The selected areas will be replaced, and keys absent from the backup will be removed. No automatic backup is kept after restoration. Temporary recovery data is kept only until the operation finishes.";
      details.replaceChildren(
        ...sections.map((section) => {
          const item = document.createElement("li");
          item.textContent = `${section.label}: ${section.deletionCount} keys will be removed`;
          return item;
        }),
      );
      action.textContent = "Replace selected data";
      action.disabled = false;
      cancelAction.hidden = false;
      return new Promise<boolean>((resolve) => {
        action.onclick = () => {
          details.replaceChildren();
          cancelAction.hidden = true;
          action.textContent = "Cancel";
          action.onclick = () => {
            action.disabled = true;
            cancel();
          };
          resolve(true);
        };
        cancelAction.onclick = () => {
          cancelAction.hidden = true;
          action.disabled = true;
          cancel();
          resolve(false);
        };
      });
    },
    completed(text) {
      details.replaceChildren();
      cancelAction.hidden = true;
      message.textContent = text;
      action.textContent = "Continue";
      action.disabled = false;
      return new Promise<void>((resolve) => {
        action.onclick = () => {
          action.disabled = true;
          resolve();
        };
      });
    },
    failed(text) {
      cancelAction.hidden = true;
      message.textContent = text;
      action.textContent = "Retry recovery";
      action.disabled = false;
      action.onclick = () => {
        action.disabled = true;
        void retry().catch(() => {
          action.disabled = false;
        });
      };
    },
    dispose() {
      host.remove();
    },
  };
}

/** Minimal startup entry, intentionally independent of settings, DBState and plugins. */
export async function deviceMaintenanceBeforeBootstrap(): Promise<void> {
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
  let view: ReturnType<typeof maintenanceView> | undefined;
  let sessionId: string | undefined;
  const retry = async () => {
    if (sessionId)
      await invoke("native_device_backup_retry_recovery", { sessionId });
    location.reload();
  };
  try {
    const bootstrap = await invoke<DeviceMaintenanceBootstrap>(
      "native_device_backup_bootstrap",
      { freshBootstrap: true },
    );
    if (bootstrap.mode === "normal") return;
    sessionId = bootstrap.session?.sessionId;
    const cancellation = new AbortController();
    view = maintenanceView(() => cancellation.abort(), retry);
    const [
      { runDeviceMaintenance, DeviceWriterBarrierError },
      { pluginDeviceStorage },
    ] = await Promise.all([
      import("./maintenance"),
      import("../../plugins/pluginDeviceStorage"),
    ]);
    await runDeviceMaintenance(bootstrap, {
      invoke,
      view,
      async assertExclusiveWriters() {
        // Unlike dedicated workers, service workers can survive a full page
        // navigation. The v2 API exposes navigator, so detect that writer class.
        if (
          navigator.serviceWorker?.controller ||
          (navigator.serviceWorker?.getRegistrations &&
            (await navigator.serviceWorker.getRegistrations()).length > 0)
        )
          throw new DeviceWriterBarrierError();
      },
      wait: (milliseconds) =>
        new Promise((resolve) => setTimeout(resolve, milliseconds)),
      environment: {
        localStorage,
        localforage: pluginDeviceStorage,
        indexedDB,
        keyRange: IDBKeyRange,
        signal: cancellation.signal,
        estimateStorage: navigator.storage?.estimate
          ? () => navigator.storage.estimate()
          : undefined,
      },
    });
    view.dispose();
  } catch {
    view ??= maintenanceView(() => {}, retry);
    view.failed(
      "Device storage recovery could not finish. Normal app startup is blocked. Retry recovery to continue.",
    );
    // Resolve only after a fresh native recovery decision, never import the app on failure.
    await new Promise<never>(() => {});
  }
}
