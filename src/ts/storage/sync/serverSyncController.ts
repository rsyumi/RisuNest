import { isLibraryFileOperationReserved } from "../libraryFileOperation";
import {
  serverSyncError,
  ServerSyncError,
  type ServerConfig,
  type ServerCycle,
  type ServerCycleItems,
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
  /** Throughput of the current attempt, from successive byte samples. */
  bytesPerSecond?: number;
  cycleItems?: ServerCycleItems;
  retryableFailure?: string;
  attemptId?: number;
  /** Wall-clock start of the current attempt, for the elapsed time. */
  attemptStartedAt?: number;
  attemptIdentity?: { endpoint: string; libraryId: string; deviceId: string };
  initialSyncComplete?: boolean;
  refreshPending?: boolean;
  replacing?: boolean;
}
export function createServerSyncController(
  facade: ServerSyncFacade,
  controllerOptions: {
    initiallyPaused?: boolean;
    onExplicitResume?(): void;
  } = {},
) {
  let state: ServerSyncSnapshot = {
    running: false,
    paused: controllerOptions.initiallyPaused ?? false,
    error: "",
  };
  let active: Promise<void> | undefined;
  let attemptSequence = 0;
  let statusFresh = false;
  let statusSequence = 0;
  let byteSample: { at: number; bytes: bigint } | undefined;
  const listeners = new Set<(snapshot: ServerSyncSnapshot) => void>();
  const publish = (): void => {
    state = {
      ...state,
      refreshPending: facade.needsRefresh(),
      initialSyncComplete: Boolean(
        state.initialSyncComplete && !state.error && !facade.needsRefresh(),
      ),
    };
    for (const listener of listeners) listener(state);
  };
  const refreshStatus = async (): Promise<void> => {
    const request = ++statusSequence;
    statusFresh = false;
    const status = await facade.status();
    if (request !== statusSequence)
      throw new ServerSyncError("server-status-changed");
    statusFresh = true;
    state.status = status;
    publish();
  };
  const synchronize = async (
    options: ServerCycleOptions = {},
  ): Promise<void> => {
    state.running = true;
    state.attemptId = ++attemptSequence;
    state.attemptStartedAt = Date.now();
    state.attemptIdentity = undefined;
    state.initialSyncComplete = false;
    state.result = undefined;
    state.progress = undefined;
    state.verifiedBytes = undefined;
    state.bytesPerSecond = undefined;
    state.cycleItems = undefined;
    state.retryableFailure = undefined;
    state.error = "";
    byteSample = undefined;
    publish();
    try {
      await refreshStatus();
      const identity = state.status;
      if (identity?.endpoint && identity.libraryId && identity.deviceId) {
        state.attemptIdentity = {
          endpoint: identity.endpoint,
          libraryId: identity.libraryId,
          deviceId: identity.deviceId,
        };
      }
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
      const status = state.status;
      const result = state.result;
      // Native reports the address used by this cycle after authenticated discovery.
      // Do not accept a status-only endpoint change or a different library/device.
      if (
        state.attemptIdentity &&
        status?.endpoint &&
        result?.endpoint === status.endpoint &&
        status.libraryId === state.attemptIdentity.libraryId &&
        status.deviceId === state.attemptIdentity.deviceId
      ) {
        state.attemptIdentity = {
          ...state.attemptIdentity,
          endpoint: result.endpoint,
        };
      }
      const identityNow = state.attemptIdentity;
      state.initialSyncComplete = Boolean(
        !state.error &&
        identityNow &&
        status?.configured &&
        result?.phase === "idle" &&
        status.endpoint === identityNow.endpoint &&
        status.libraryId === identityNow.libraryId &&
        status.deviceId === identityNow.deviceId &&
        !status.registrationRequired &&
        !status.reconciling &&
        !status.operationPending &&
        !status.fullScan &&
        status.dirtyRecords === 0 &&
        !facade.needsRefresh() &&
        status.localRevision === result.localRevision &&
        status.head &&
        status.head.libraryId === result.head?.libraryId &&
        status.head.epoch === result.head?.epoch &&
        status.head.seq === result.head?.seq &&
        status.head.headId === result.head?.headId,
      );
      if (state.initialSyncComplete) state.lastSuccessAt = Date.now();
    } catch (cause) {
      state.error = serverSyncError(cause).code;
      state.initialSyncComplete = false;
    } finally {
      state.running = false;
      state.progress = undefined;
      state.bytesPerSecond = undefined;
      state.cycleItems = undefined;
      state.retryableFailure = undefined;
      state.attemptStartedAt = undefined;
      publish();
    }
  };
  const requireAvailable = (): void => {
    if (state.running || state.replacing || isLibraryFileOperationReserved())
      throw new ServerSyncError("library-operation-busy");
  };
  const invalidateCompletion = (): void => {
    state.initialSyncComplete = false;
    state.result = undefined;
    state.attemptIdentity = undefined;
  };
  return {
    holdAutomaticSync(): void {
      state.paused = true;
      publish();
    },
    invalidateCompletion(): void {
      // A local commit invalidates completion, not the current conflict preview or attempt identity.
      state.initialSyncComplete = false;
      publish();
    },
    assertFileOperationAvailable(): void {
      if (state.running || state.replacing || facade.needsRefresh())
        throw new ServerSyncError("server-sync-busy");
    },
    async confirmReplacement(): Promise<void> {
      if (!state.replacing)
        throw new ServerSyncError("replacement-not-reserved");
      await refreshStatus();
      if (
        !statusFresh ||
        !state.status ||
        state.status.operationPending ||
        facade.needsRefresh()
      ) {
        throw new ServerSyncError("resolve-pending-operation-first");
      }
    },
    async withReplacement<T>(operation: () => Promise<T>): Promise<T> {
      if (state.running || state.replacing || facade.needsRefresh())
        throw new ServerSyncError("resolve-pending-operation-first");
      // Reservation precedes asynchronous status/cancellation checks, without a PDS fence.
      state.replacing = true;
      invalidateCompletion();
      publish();
      try {
        await facade.cancel();
        await refreshStatus();
        if (
          !statusFresh ||
          !state.status ||
          state.status.operationPending ||
          facade.needsRefresh()
        ) {
          throw new ServerSyncError("resolve-pending-operation-first");
        }
        return await operation();
      } finally {
        try {
          await refreshStatus();
        } catch (cause) {
          state.error = serverSyncError(cause).code;
        }
        state.replacing = false;
        publish();
      }
    },
    reportVerifiedBytes(verifiedBytes: string): void {
      if (!state.running) return;
      state.verifiedBytes = verifiedBytes;
      const now = Date.now();
      const bytes = BigInt(verifiedBytes);
      if (byteSample && now > byteSample.at) {
        const rate =
          (Number(bytes - byteSample.bytes) * 1000) / (now - byteSample.at);
        // Smooth the one-second samples so the rate does not flicker.
        state.bytesPerSecond = Math.max(
          0,
          state.bytesPerSecond === undefined
            ? rate
            : (state.bytesPerSecond + rate) / 2,
        );
      }
      byteSample = { at: now, bytes };
      publish();
    },
    reportCycleItems(items: ServerCycleItems): void {
      if (!state.running) return;
      state.cycleItems = items;
      publish();
    },
    reportRetryableFailure(code: string | undefined): void {
      if (!state.running) return;
      state.retryableFailure = code;
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
      !state.replacing &&
      statusFresh &&
      Boolean(state.status) &&
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
      invalidateCompletion();
      try {
        await refreshStatus();
      } catch (cause) {
        state.error = serverSyncError(cause).code;
        publish();
      }
    },
    async bind(config: ServerConfig): Promise<void> {
      requireAvailable();
      invalidateCompletion();
      const status = await facade.bind(config);
      state.status = status;
      state.lastSuccessAt = undefined;
      state.error = "";
      controllerOptions.onExplicitResume?.();
      state.paused = false;
      publish();
    },
    async unbind(): Promise<void> {
      requireAvailable();
      invalidateCompletion();
      await facade.unbind();
      state.lastSuccessAt = undefined;
      state.result = undefined;
      await refreshStatus();
    },
    async reregister(config: ServerConfig): Promise<void> {
      requireAvailable();
      invalidateCompletion();
      const status = await facade.reregister(config);
      state.status = status;
      state.result = undefined;
      state.error = "";
      controllerOptions.onExplicitResume?.();
      state.paused = false;
      publish();
    },
    async reconcile(): Promise<void> {
      requireAvailable();
      invalidateCompletion();
      const status = await facade.reconcile();
      state.status = status;
      state.result = undefined;
      state.error = "";
      controllerOptions.onExplicitResume?.();
      state.paused = false;
      publish();
    },
    synchronize(options: ServerCycleOptions = {}): Promise<void> {
      if (active) return active;
      if (state.replacing || isLibraryFileOperationReserved())
        return Promise.reject(new ServerSyncError("library-operation-busy"));
      controllerOptions.onExplicitResume?.();
      state.paused = false;
      let complete!: () => void;
      let fail!: (cause: unknown) => void;
      const attempt = new Promise<void>((resolve, reject) => {
        complete = resolve;
        fail = reject;
      });
      active = attempt;
      void synchronize(options).then(
        () => {
          active = undefined;
          complete();
        },
        (cause) => {
          active = undefined;
          fail(cause);
        },
      );
      return attempt;
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
        !state.replacing &&
        !isLibraryFileOperationReserved() &&
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
