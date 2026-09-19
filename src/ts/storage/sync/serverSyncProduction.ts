import { isTauri } from "../../platform";
import { invalidatePluginDeviceKeyspaces } from "../../plugins/pluginDeviceKeyspace";
import { subscribeLibraryFileOperationReleased } from "../libraryFileOperation";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  flushPendingData,
  capturePersistentMutationToken,
  acquireDestructiveReplacementFence,
  refreshActiveWorkingSetFromStore,
} from "../persistentDataRuntime.svelte";
import { createServerSyncFacade, type ServerHead } from "./serverSync";
import { createServerSyncController } from "./serverSyncController";
import { createServerSyncScheduler } from "./serverSyncScheduler";
import { subscribeLocalPersistentRevision } from "../persistentRevisionEvents";
import { subscribeNativeServerSyncSignals } from "./serverSyncNativeSignals";
import type { SyncExitDrainAdapter } from "../syncExitCoordinator";

let controller: ReturnType<typeof createServerSyncController> | undefined;
export function getServerSyncController() {
  return (controller ??= createServerSyncController(
    createServerSyncFacade({
      onProgress: (phase) => controller?.reportProgress(phase),
      onVerifiedBytes: (bytes) => controller?.reportVerifiedBytes(bytes),
      onRetryableFailure: (code) => controller?.reportRetryableFailure(code),
      onCycleItems: (items) => controller?.reportCycleItems(items),
      runtime: {
        flushPendingData,
        capturePersistentMutationToken,
        acquireDestructiveReplacementFence,
        refreshActiveWorkingSetFromStore,
      },
      invalidateDevicePlugins: invalidatePluginDeviceKeyspaces,
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
export function createServerSyncExitDrainAdapter(
  id = "server",
  syncController = getServerSyncController(),
): SyncExitDrainAdapter {
  return {
    id,
    drain: (target, signal) =>
      target.selectionId === id
        ? syncController.drainToRevision(target.revision, signal)
        : Promise.resolve({
            kind: "blocked",
            reason: "server-sync-selection-changed",
          }),
    cancel: () => syncController.cancelExitDrain(),
  };
}
/** A restored library waits for an explicit sync action, including across maintenance reloads. */
export function holdServerSyncAfterRestore(): void {
  localStorage.setItem("risuNestServerSyncRestoreHold", "true");
  getServerSyncController().holdAutomaticSync();
}
let started = false;
let activeScheduler: ReturnType<typeof createServerSyncScheduler> | undefined;
let syncAvailable: (() => boolean) | undefined;
/** A read-only file backup may outlive the scheduled timer. Resume the existing
 * scheduler when it settles; restoring a library deliberately does not do this. */
export function resumeServerSyncAfterBackup(): void {
  activeScheduler?.resume();
  if (activeScheduler) {
    cleanupDeletedBackups();
    if (syncAvailable?.()) void invoke("server_sync_events_start").catch(() => {});
  }
}
export type ServerSyncBackupAvailability =
  | "local-complete"
  | "connection-required"
  | "unavailable";
export interface ServerSyncBackupSide {
  localRequiredBytes: number;
  remoteDependentBytes: number;
  availability: ServerSyncBackupAvailability;
}
export interface ServerSyncBackup {
  id: string;
  createdAt: number;
  head: ServerHead;
  localRevision: number;
  local: ServerSyncBackupSide;
  remote: ServerSyncBackupSide;
  preservationScope: "library";
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
    return await invoke<{
      localDeleted: true;
      cleanup: "complete" | "pending";
    }>("server_sync_backup_delete", { id });
  } finally {
    cleanupDeletedBackups();
  }
};
export const getServerSyncCacheUsage = () =>
  invoke<ServerSyncCacheUsage>("server_sync_cache_usage");
export const cleanupServerSyncCache = () =>
  invoke<ServerSyncCacheUsage>("server_sync_cache_cleanup");
export const listServerSyncBackups = (before?: ServerSyncBackupCursor) =>
  invoke<{ items: ServerSyncBackup[]; next: ServerSyncBackupCursor | null }>(
    "server_sync_backups",
    { before: before ?? null },
  );
export async function restoreServerSyncBackup(
  id: string,
  side: "local" | "remote",
): Promise<void> {
  const { restoreBackupFromNativeSource } =
    await import("../portableBackupFileRouteProduction.svelte");
  let lease: string | undefined;
  try {
    await restoreBackupFromNativeSource(async () => {
      const result = await invoke<{
        source: { type: "conflictReference"; token: string };
        lease: string;
      }>(
        "server_sync_backup_source",
        { id, side },
      );
      lease = result.lease;
      return result.source;
    });
  } finally {
    if (lease) await invoke<void>("server_sync_backup_release", { lease });
  }
}
export async function exportServerSyncBackup(
  id: string,
  side: "local" | "remote",
): Promise<void> {
  const { exportPortableBackupFromReferenceSource } =
    await import("../portableBackupFileRouteProduction.svelte");
  let lease: string | undefined;
  try {
    await exportPortableBackupFromReferenceSource(async () => {
      const result = await invoke<{
        source: { type: "conflictReference"; token: string };
        lease: string;
      }>("server_sync_backup_source", { id, side });
      lease = result.lease;
      return result.source;
    });
  } finally {
    if (lease) await invoke<void>("server_sync_backup_release", { lease });
  }
}
/** Notifications and polling stop together: neither runs while the app is not
 * in a state to act on them. */
function suspendServerSync(
  scheduler: ReturnType<typeof createServerSyncScheduler>,
): void {
  scheduler.suspend();
  void invoke("server_sync_events_stop").catch(() => {});
}
export function startServerSync(): void {
  if (started || !isTauri) return;
  started = true;
  const controller = getServerSyncController();
  const available = () =>
    document.visibilityState !== "hidden" && navigator.onLine;
  const scheduler = createServerSyncScheduler(controller, { available });
  activeScheduler = scheduler;
  syncAvailable = available;
  subscribeLibraryFileOperationReleased(() => {
    if (controller.canAutoSync()) resumeServerSyncAfterBackup();
  });
  subscribeLocalPersistentRevision(() => {
    controller.invalidateCompletion();
    scheduler.localCommit();
  });
  subscribeNativeServerSyncSignals(
    {
      // Device sections have their own revision, so their writes wake the
      // scheduler the same way a library revision does.
      deviceChanged: () => {
        controller.invalidateCompletion();
        scheduler.localCommit();
      },
      remoteHint: () => scheduler.remoteHint(),
    },
    (event, handler) => listen(event, handler),
  );
  let configured = false;
  controller.subscribe((state) => {
    const bound = Boolean(state.status?.configured);
    // A connection can only be held once this device has a binding to hold.
    if (bound && !configured) resumeServerSyncAfterBackup();
    configured = bound;
  });
  void controller.initialize().then(() => resumeServerSyncAfterBackup());
  window.addEventListener("online", () => resumeServerSyncAfterBackup());
  window.addEventListener("offline", () => suspendServerSync(scheduler));
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") suspendServerSync(scheduler);
    else resumeServerSyncAfterBackup();
  });
}
