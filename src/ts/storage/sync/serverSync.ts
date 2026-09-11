import { invoke } from "@tauri-apps/api/core";
import type { PersistentDestructiveReplacementFence } from "../persistentDataRuntime";
import type { PeerSyncMutationRuntime } from "./peerSyncShared";

export interface ServerHead {
  libraryId: string;
  epoch: string;
  seq: string;
  headId: string;
  minRetainedSeq: string;
}
export interface ServerConfig {
  endpoint: string;
  libraryId: string;
  deviceId: string;
  token: string;
}
export interface ServerStatus {
  localRevision: number;
  reconciling: boolean;
  configured: boolean;
  endpoint: string | null;
  libraryId: string | null;
  deviceId: string | null;
  head: ServerHead | null;
  dirtyRecords: number;
  fullScan: boolean;
  registrationRequired: boolean;
  operationPending: boolean;
}
export interface ServerCycle {
  phase: "idle" | "pending" | "conflict";
  localRevision: number;
  head: ServerHead;
  conflictCount: number;
  conflicts: string[];
  appliedRecords: number;
  proposedRecords: number;
}
export interface ServerCycleOptions {
  resolution?: "keep-local" | "keep-remote";
  expectedRevision?: number;
  expectedHead?: ServerHead;
}
type Prepared =
  | { kind: "report"; result: ServerCycle }
  | {
      kind: "ready";
      preparationId: string;
      localRevision: number;
      head: ServerHead;
      appliedRecords: number;
    };
type NativeInvoke = <T>(
  command: string,
  args?: Record<string, unknown>,
) => Promise<T>;
export class ServerSyncError extends Error {
  constructor(readonly code: string) {
    super(code);
    this.name = "ServerSyncError";
  }
}
export function serverSyncError(cause: unknown): ServerSyncError {
  if (cause instanceof ServerSyncError) return cause;
  const code =
    typeof cause === "object" && cause !== null && "code" in cause
      ? cause.code
      : undefined;
  return new ServerSyncError(
    typeof code === "string" && /^[a-z-]{1,64}$/.test(code)
      ? code
      : "server-sync-failed",
  );
}

export function createServerSyncFacade(options: {
  runtime: PeerSyncMutationRuntime;
  invoke?: NativeInvoke;
  restorePlugins?: () => Promise<void>;
}) {
  const native = options.invoke ?? invoke;
  let pendingRefresh:
    | {
        revision: number;
        preparationId: string;
        fence: PersistentDestructiveReplacementFence;
      }
    | undefined;
  let pendingActivation:
    | {
        prepared: Extract<Prepared, { kind: "ready" }>;
        fence: PersistentDestructiveReplacementFence;
      }
    | undefined;
  let active: Promise<ServerCycle> | undefined;
  let cancelled = false;
  const refresh = async (): Promise<string> => {
    const pending = pendingRefresh;
    if (!pending) throw new ServerSyncError("refresh-not-pending");
    try {
      await pending.fence.refreshCommittedWorkingSet(pending.revision);
      await options.restorePlugins?.();
    } catch {
      throw new ServerSyncError("committed-refresh-pending");
    }
    pendingRefresh = undefined;
    pending.fence.release();
    return pending.preparationId;
  };
  const activate = async (): Promise<string> => {
    const pending = pendingActivation;
    if (!pending) throw new ServerSyncError("activation-not-pending");
    let revision: number;
    try {
      revision = await native<number>("server_sync_activate", {
        preparationId: pending.prepared.preparationId,
      });
    } catch (cause) {
      const error = serverSyncError(cause);
      if (
        ["local-revision-changed", "stale-server-preparation"].includes(
          error.code,
        )
      ) {
        pendingActivation = undefined;
        pending.fence.release();
        await native("server_sync_cancel").catch(() => undefined);
        throw error;
      }
      // The IPC reply may be lost after native COMMIT. Keep editing fenced
      // until the idempotent activation confirms the committed revision.
      throw new ServerSyncError("activation-confirmation-pending");
    }
    pendingActivation = undefined;
    if (pending.prepared.appliedRecords > 0) {
      pendingRefresh = {
        revision,
        preparationId: pending.prepared.preparationId,
        fence: pending.fence,
      };
      return refresh();
    }
    pending.fence.release();
    return pending.prepared.preparationId;
  };
  const run = async (
    cycleOptions: ServerCycleOptions,
  ): Promise<ServerCycle> => {
    cancelled = false;
    if (pendingActivation) {
      const preparationId = await activate();
      return native<ServerCycle>("server_sync_publish", { preparationId });
    }
    if (pendingRefresh) {
      const preparationId = await refresh();
      return native<ServerCycle>("server_sync_publish", { preparationId });
    }
    await options.runtime.flushPendingData("server-sync-prepare");
    const prepared = await native<Prepared>("server_sync_prepare", {
      options: cycleOptions,
    });
    if (prepared.kind === "report") return prepared.result;
    let fence: PersistentDestructiveReplacementFence | undefined;
    try {
      if (cancelled) throw new ServerSyncError("cancelled");
      await options.runtime.flushPendingData("server-sync-activate");
      const token = await options.runtime.capturePersistentMutationToken(
        "server-sync-activate",
      );
      fence = await options.runtime.acquireDestructiveReplacementFence(token);
      if (cancelled) throw new ServerSyncError("cancelled");
      pendingActivation = { prepared, fence };
      fence = undefined;
      await activate();
    } catch (cause) {
      if (fence) {
        fence.release();
        await native("server_sync_cancel").catch(() => undefined);
      }
      throw serverSyncError(cause);
    }
    // The mutation fence has been released before any upload or server job
    // wait. New local edits become the durable outbox tail for the next run.
    return native<ServerCycle>("server_sync_publish", {
      preparationId: prepared.preparationId,
    });
  };
  const recover = async (
    command: string,
    config?: ServerConfig,
  ): Promise<ServerStatus> => {
    if (pendingRefresh) throw new ServerSyncError("committed-refresh-pending");
    if (pendingActivation)
      throw new ServerSyncError("activation-confirmation-pending");
    if (active) throw new ServerSyncError("server-sync-busy");
    await options.runtime.flushPendingData("server-sync-recovery");
    const status = await native<ServerStatus>("server_sync_status");
    return native<ServerStatus>(command, {
      expectedRevision: status.localRevision,
      ...(config ? { config } : {}),
    });
  };
  return {
    status: () => native<ServerStatus>("server_sync_status"),
    bind: (config: ServerConfig) =>
      native<ServerStatus>("server_sync_bind", { config }),
    unbind: () => native<void>("server_sync_unbind"),
    reregister: (config: ServerConfig) =>
      recover("server_sync_reregister", config),
    reconcile: () => recover("server_sync_reconcile"),
    needsRefresh: () =>
      pendingRefresh !== undefined || pendingActivation !== undefined,
    cycle(cycleOptions: ServerCycleOptions = {}): Promise<ServerCycle> {
      if (active) return active;
      active = run(cycleOptions)
        .catch((cause) => {
          throw serverSyncError(cause);
        })
        .finally(() => {
          active = undefined;
        });
      return active;
    },
    async cancel(): Promise<void> {
      if (pendingRefresh)
        throw new ServerSyncError("committed-refresh-pending");
      if (pendingActivation)
        throw new ServerSyncError("activation-confirmation-pending");
      cancelled = true;
      await native("server_sync_cancel");
    },
  };
}
export type ServerSyncFacade = ReturnType<typeof createServerSyncFacade>;
