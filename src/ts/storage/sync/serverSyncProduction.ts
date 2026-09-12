import { isTauri } from "../../platform";
import { invoke } from "@tauri-apps/api/core";
import {
  flushPendingData,
  capturePersistentMutationToken,
  acquireDestructiveReplacementFence,
} from "../persistentDataRuntime.svelte";
import {
  createServerSyncFacade,
  ServerSyncError,
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
  const controller = getServerSyncController();
  await controller.pause();
  await controller.waitForIdle();
  if (!controller.canRestore())
    throw new ServerSyncError("resolve-pending-operation-first");
  const path = await invoke<string>("server_sync_backup_source", { id, side });
  const { restoreLocalBackupFromNativeSource } = await import(
    "../losslessBackupFileRouteProduction.svelte"
  );
  await restoreLocalBackupFromNativeSource({ type: "desktopPath", path });
  await controller.initialize();
}
export function startServerSync(): void {
  if (started || !isTauri) return;
  started = true;
  const controller = getServerSyncController();
  const scheduler = createServerSyncScheduler(controller, {
    available: () => document.visibilityState !== "hidden" && navigator.onLine,
  });
  subscribeLocalPersistentRevision(() => scheduler.localCommit());
  void controller.initialize().then(() => scheduler.resume());
  window.addEventListener("online", () => scheduler.resume());
  window.addEventListener("offline", () => scheduler.suspend());
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") scheduler.suspend();
    else scheduler.resume();
  });
}
