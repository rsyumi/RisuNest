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

let controller: ReturnType<typeof createServerSyncController> | undefined;
export function getServerSyncController() {
  return (controller ??= createServerSyncController(
    createServerSyncFacade({
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
  const tick = (): void => {
    if (
      document.visibilityState !== "hidden" &&
      navigator.onLine &&
      controller.canAutoSync()
    )
      void controller.synchronize();
  };
  void controller.initialize().then(tick);
  setInterval(tick, 30_000);
  window.addEventListener("online", tick);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden" && controller.snapshot().running)
      void controller.suspend();
    else tick();
  });
}
