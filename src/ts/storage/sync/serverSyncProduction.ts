import { isTauri } from "../../platform";
import { invoke } from "@tauri-apps/api/core";
import {
  flushPendingData,
  capturePersistentMutationToken,
  acquireDestructiveReplacementFence,
} from "../persistentDataRuntime.svelte";
import {
  createServerSyncFacade,
  type ServerHead,
} from "./serverSync";
import { createServerSyncController } from "./serverSyncController";
import { createServerSyncScheduler } from "./serverSyncScheduler";
import { subscribeLocalPersistentRevision } from "../persistentRevisionEvents";

let controller: ReturnType<typeof createServerSyncController> | undefined;
export function getServerSyncController() {
  return (controller ??= createServerSyncController(
    createServerSyncFacade({
      onProgress: (phase) => controller?.reportProgress(phase),
      onVerifiedBytes: (bytes) => controller?.reportVerifiedBytes(bytes),
      runtime: {
        flushPendingData,
        capturePersistentMutationToken,
        acquireDestructiveReplacementFence,
      },
      restorePlugins: async () => {
        await (
          await import("../../plugins/plugins.svelte")
        ).loadPluginsAfterAuthoritativeRestore();
      },
    }),
  ));
}
let started = false;
let activeScheduler: ReturnType<typeof createServerSyncScheduler> | undefined;
/** A read-only file backup may outlive the scheduled timer. Resume the existing
 * scheduler when it settles; restoring a library deliberately does not do this. */
export function resumeServerSyncAfterBackup(): void { activeScheduler?.resume(); }
export interface ServerSyncBackup {
  id: string;
  createdAt: number;
  head: ServerHead;
  localRevision: number;
}
export const listServerSyncBackups = () =>
  invoke<ServerSyncBackup[]>("server_sync_backups");
export async function restoreServerSyncBackup(
  id: string,
  side: "local" | "remote",
): Promise<void> {
  const { restoreLocalBackupFromNativeSource } = await import("../losslessBackupFileRouteProduction.svelte");
  await restoreLocalBackupFromNativeSource(async () => ({
    type: "desktopPath",
    path: await invoke<string>("server_sync_backup_source", { id, side }),
  }));
}
export function startServerSync(): void {
  if (started || !isTauri) return;
  started = true;
  const controller = getServerSyncController();
  const scheduler = createServerSyncScheduler(controller, {
    available: () => document.visibilityState !== "hidden" && navigator.onLine,
  });
  activeScheduler = scheduler;
  subscribeLocalPersistentRevision(() => { controller.invalidateCompletion(); scheduler.localCommit(); });
  void controller.initialize().then(() => scheduler.resume());
  window.addEventListener("online", () => scheduler.resume());
  window.addEventListener("offline", () => scheduler.suspend());
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") scheduler.suspend();
    else scheduler.resume();
  });
}
