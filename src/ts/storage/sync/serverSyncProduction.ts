import { isTauri } from "../../platform";
import { invoke } from "@tauri-apps/api/core";
import {
  flushPendingData,
  capturePersistentMutationToken,
  acquireDestructiveReplacementFence,
} from "../persistentDataRuntime.svelte";
import { createServerSyncFacade, type ServerHead } from "./serverSync";
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
    {
      initiallyPaused:
        localStorage.getItem("risuNestServerSyncRestoreHold") === "true",
      onExplicitResume: () =>
        localStorage.removeItem("risuNestServerSyncRestoreHold"),
    },
  ));
}
/** A restored library waits for an explicit sync action, including across maintenance reloads. */
export function holdServerSyncAfterRestore(): void {
  localStorage.setItem("risuNestServerSyncRestoreHold", "true");
  getServerSyncController().holdAutomaticSync();
}
let started = false;
let activeScheduler: ReturnType<typeof createServerSyncScheduler> | undefined;
/** A read-only file backup may outlive the scheduled timer. Resume the existing
 * scheduler when it settles; restoring a library deliberately does not do this. */
export function resumeServerSyncAfterBackup(): void {
  activeScheduler?.resume();
  if (activeScheduler) cleanupDeletedBackups();
}
export interface ServerSyncBackup {
  id: string;
  createdAt: number;
  head: ServerHead;
  localRevision: number;
  localBytes: number;
  remoteBytes: number;
  preservationScope: "library";
  recoveryReady: boolean;
}
export interface ServerSyncBackupCursor {
  createdAt: number;
  id: string;
}
export interface ManagedServerSyncBackup extends ServerSyncBackup {
  diskBytes: number;
  deletable: boolean;
  blockedReason: string | null;
}
export interface ServerSyncBackupInventory {
  items: ManagedServerSyncBackup[];
  next: ServerSyncBackupCursor | null;
  completeCount: number;
  completeBytes: number;
  incompleteCount: number;
  incompleteBytes: number;
  diskBytes: number;
}
export interface ServerSyncCacheUsage {
  totalBytes: number;
  protectedBytes: number;
  reclaimableBytes: number;
  blockedReason: string | null;
}
export const getServerSyncBackupInventory = (before?: ServerSyncBackupCursor) =>
  invoke<ServerSyncBackupInventory>("server_sync_backup_inventory", {
    before: before ?? null,
  });
let deletionCleanup: Promise<void> | undefined;
function cleanupDeletedBackups(): void {
  if (!isTauri || deletionCleanup) return;
  // Native admission rejects busy attempts. Retry on the next safe lifecycle
  // signal, without cancelling work or making startup depend on cleanup.
  deletionCleanup = invoke("server_sync_backup_cleanup")
    .then(
      () => {},
      () => {},
    )
    .finally(() => {
      deletionCleanup = undefined;
    });
}
export const deleteServerSyncBackup = async (id: string) => {
  try {
    await invoke<void>("server_sync_backup_delete", { id });
  } finally {
    cleanupDeletedBackups();
  }
};
export const getServerSyncCacheUsage = () =>
  invoke<ServerSyncCacheUsage>("server_sync_cache_usage");
export const cleanupServerSyncCache = () =>
  invoke<ServerSyncCacheUsage>("server_sync_cache_cleanup");
export const listServerSyncBackups = () =>
  invoke<ServerSyncBackup[]>("server_sync_backups");
export async function restoreServerSyncBackup(
  id: string,
  side: "local" | "remote",
): Promise<void> {
  const { restoreBackupFromNativeSource } =
    await import("../portableBackupFileRouteProduction.svelte");
  let lease: string | undefined;
  try {
    await restoreBackupFromNativeSource(async () => {
      const source = await invoke<{ path: string; lease: string }>(
        "server_sync_backup_source",
        { id, side },
      );
      lease = source.lease;
      return { type: "desktopPath", path: source.path };
    });
  } finally {
    if (lease) await invoke<void>("server_sync_backup_release", { lease });
  }
}
export function startServerSync(): void {
  if (started || !isTauri) return;
  started = true;
  const controller = getServerSyncController();
  const scheduler = createServerSyncScheduler(controller, {
    available: () => document.visibilityState !== "hidden" && navigator.onLine,
  });
  activeScheduler = scheduler;
  subscribeLocalPersistentRevision(() => {
    controller.invalidateCompletion();
    scheduler.localCommit();
  });
  void controller.initialize().then(() => resumeServerSyncAfterBackup());
  window.addEventListener("online", () => resumeServerSyncAfterBackup());
  window.addEventListener("offline", () => scheduler.suspend());
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") scheduler.suspend();
    else resumeServerSyncAfterBackup();
  });
}
