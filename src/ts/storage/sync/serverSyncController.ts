import {
  serverSyncError,
  type ServerConfig,
  type ServerCycle,
  type ServerCycleOptions,
  type ServerStatus,
  type ServerSyncFacade,
  type ServerSyncProgress,
} from "./serverSync";

export interface ServerSyncSnapshot {
  status?: ServerStatus;
  result?: ServerCycle;
  running: boolean;
  paused: boolean;
  error: string;
  lastSuccessAt?: number;
  progress?: ServerSyncProgress;
  verifiedBytes?: string;
}
export function createServerSyncController(facade: ServerSyncFacade) {
  let state: ServerSyncSnapshot = { running: false, paused: false, error: "" };
  let active: Promise<void> | undefined;
  const listeners = new Set<(snapshot: ServerSyncSnapshot) => void>();
  const publish = (): void => {
    state = { ...state };
    for (const listener of listeners) listener(state);
  };
  const refreshStatus = async (): Promise<void> => {
    const status = await facade.status();
    state.status = status;
    publish();
  };
  const synchronize = async (
    options: ServerCycleOptions = {},
  ): Promise<void> => {
    state.running = true;
    state.progress = undefined;
    state.verifiedBytes = undefined;
    state.error = "";
    publish();
    try {
      // Bound each foreground invocation. The next scheduled run resumes
      // a persisted server job without creating a new logical operation.
      for (let attempt = 0; attempt < 4; attempt += 1) {
        // Progress publishes replace the snapshot while this promise is pending.
        // Resolve first, then assign to the current snapshot, not the old object
        // captured by the left-hand side of an assignment containing await.
        const result = await facade.cycle(attempt === 0 ? options : {});
        state.result = result;
        publish();
        if (result.phase !== "pending" || state.paused) break;
        await new Promise<void>((resolve) => setTimeout(resolve, 300));
      }
      await refreshStatus();
      if (state.result?.phase === "idle") state.lastSuccessAt = Date.now();
    } catch (cause) {
      state.error = serverSyncError(cause).code;
    } finally {
      state.running = false;
      state.progress = undefined;
      publish();
    }
  };
  return {
    reportVerifiedBytes(verifiedBytes: string): void {
      if (!state.running) return;
      state.verifiedBytes = verifiedBytes;
      publish();
    },
    reportProgress(progress: ServerSyncProgress): void {
      if (!state.running) return;
      state.progress = progress;
      publish();
    },
    snapshot: () => state,
    waitForIdle: () => active ?? Promise.resolve(),
    canRestore: () =>
      !state.running &&
      !state.status?.operationPending &&
      !facade.needsRefresh(),
    subscribe(listener: (snapshot: ServerSyncSnapshot) => void): () => void {
      listeners.add(listener);
      listener(state);
      return () => {
        listeners.delete(listener);
      };
    },
    async initialize(): Promise<void> {
      try {
        await refreshStatus();
      } catch (cause) {
        state.error = serverSyncError(cause).code;
        publish();
      }
    },
    async bind(config: ServerConfig): Promise<void> {
      const status = await facade.bind(config);
      state.status = status;
      state.lastSuccessAt = undefined;
      state.error = "";
      state.paused = false;
      publish();
    },
    async unbind(): Promise<void> {
      if (active) return;
      await facade.unbind();
      state.lastSuccessAt = undefined;
      state.result = undefined;
      await refreshStatus();
    },
    async reregister(config: ServerConfig): Promise<void> {
      const status = await facade.reregister(config);
      state.status = status;
      state.result = undefined;
      state.error = "";
      state.paused = false;
      publish();
    },
    async reconcile(): Promise<void> {
      const status = await facade.reconcile();
      state.status = status;
      state.result = undefined;
      state.error = "";
      state.paused = false;
      publish();
    },
    synchronize(options: ServerCycleOptions = {}): Promise<void> {
      if (active) return active;
      state.paused = false;
      active = synchronize(options).finally(() => {
        active = undefined;
      });
      return active;
    },
    async pause(): Promise<void> {
      state.paused = true;
      publish();
      try {
        await facade.cancel();
      } catch (cause) {
        state.error = serverSyncError(cause).code;
        publish();
      }
    },
    async suspend(): Promise<void> {
      try {
        await facade.cancel();
      } catch (cause) {
        state.error = serverSyncError(cause).code;
        publish();
      }
    },
    canAutoSync: () =>
      Boolean(
        state.status?.configured &&
          !state.status.registrationRequired &&
          !state.running &&
          !state.paused &&
          ![
            "epoch-reconciliation-required",
            "unauthorized",
            "new-device-registration-required",
            "device-credential-unavailable",
          ].includes(state.error) &&
          state.result?.phase !== "conflict" &&
          !facade.needsRefresh(),
      ),
  };
}
export type ServerSyncController = ReturnType<
  typeof createServerSyncController
>;
