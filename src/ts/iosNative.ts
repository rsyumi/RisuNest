import { invoke } from "@tauri-apps/api/core";
import { generationDiagnostics } from "./generationDiagnostics";
import { isTauriIOS } from "./platform";

export class BackgroundExecutionExpiredError extends DOMException {
  constructor() { super("Background execution expired", "AbortError"); }
}
export const isBackgroundExpiryReason = (reason: unknown): boolean =>
  reason instanceof BackgroundExecutionExpiredError;

export interface IOSNativeState {
  notifications: boolean;
  notificationStatus: number;
  activeTasks: string[];
  expiredTasks: string[];
  foreground: boolean;
  backgroundMode: "limited" | "continued";
  continuedRegistered: boolean;
  continuedErrorCode: number | null;
  backgroundRefreshStatus: number;
}

export const getIOSNativeState = () =>
  invoke<IOSNativeState>("plugin:ios-native|state");
/** A new main document cannot retain work owned by the previous renderer. */
let initialization: Promise<void> | undefined;
export async function initializeIOSNative(): Promise<void> {
  if (isTauriIOS) await (initialization ??= invoke<void>("plugin:ios-native|reset_generation"));
}
export const requestIOSNotifications = () =>
  invoke<{ granted: boolean }>("plugin:ios-native|request_notifications");
export const openIOSSettings = () =>
  invoke<{ opened: boolean }>("plugin:ios-native|open_settings");
export const notifyIOSGenerationComplete = () =>
  invoke("plugin:ios-native|notify", {
    body: navigator.language.startsWith("ko")
      ? "응답 생성이 완료되었습니다."
      : "Your response is ready.",
  });

interface IOSGenerationDependencies {
  enabled(): boolean;
  begin(): Promise<{ id: string | null }>;
  end(id: string, success: boolean): Promise<unknown>;
  progress?(id: string, completed: number): Promise<unknown>;
  state(): Promise<IOSNativeState>;
  events: EventTarget;
  monotonicProgress?: boolean;
  unavailable?(): void;
  expired?(): void;
}
const productionDependencies: IOSGenerationDependencies = {
  enabled: () => isTauriIOS,
  begin: async () => {
    await initializeIOSNative();
    return invoke("plugin:ios-native|begin", { continued: true });
  },
  end: (id, success) => invoke("plugin:ios-native|end", { id, success }),
  progress: (id, completed) =>
    invoke("plugin:ios-native|generation_progress", { id, completed }),
  state: getIOSNativeState,
  // Read on use, so model requests can load this module where no window exists.
  get events() { return window; },
};

export async function beginIOSBackgroundTask(
  kind: string, signal: AbortSignal | undefined, release: () => void, userInitiated = false,
) {
  const task = await beginIOSGeneration(signal, {
    ...productionDependencies,
    begin: async () => {
      await initializeIOSNative();
      return invoke("plugin:ios-native|begin", { kind, continued: userInitiated });
    },
    progress: (id, completed) => invoke("plugin:ios-native|generation_progress", {
      id, completed: Math.max(0, completed), total: completed < 0 ? 0 : 100,
    }),
    monotonicProgress: false,
    unavailable: release,
    expired: release,
  });
  return {
    ...task,
    protection: task.admitted()
      ? { state: "active" as const }
      : { state: "refused" as const, reason: "background" as const },
    progress: (percent: number | null) => task.progress(percent ?? -1),
  };
}

// Tasks this page began; each one's expiry ends only its own work.
const begunTasks = new Set<string>();
export const isIOSTaskBegun = (id: string): boolean => begunTasks.has(id);

/** Retain the caller's cancellation and pair every native assertion with release. */
export async function beginIOSGeneration(
  signal?: AbortSignal,
  deps: IOSGenerationDependencies = productionDependencies,
): Promise<{
  signal: AbortSignal | undefined;
  admitted(): boolean;
  expired(): boolean;
  progress(completed: number): void;
  dispose(success?: boolean): Promise<void>;
}> {
  if (!deps.enabled())
    return { signal, admitted: () => false, expired: () => false, progress: () => {}, dispose: async () => {} };
  const controller = new AbortController();
  const abort = () => controller.abort(signal?.reason);
  signal?.addEventListener("abort", abort, { once: true });
  if (signal?.aborted) abort();
  let id: string | null = null;
  let didExpire = false;
  const expired = () => {
    didExpire = true;
    controller.abort(new BackgroundExecutionExpiredError());
    deps.expired?.();
  };
  const listener = (event: Event) => {
    const detail = (event as CustomEvent<{ event: string; id?: string }>)
      .detail;
    if (detail?.event === "expired" && detail.id === id) expired();
    if (detail?.event === "active" && id) {
      // WebKit can suspend before delivery of the expiration event.
      void deps
        .state()
        .then((state) => {
          if (
            id &&
            (!state.activeTasks.includes(id) || state.expiredTasks.includes(id))
          )
            expired();
        })
        .catch(() => expired());
    }
  };
  deps.events.addEventListener("risunest-ios-lifecycle", listener);
  try {
    id = (await deps.begin()).id;
    if (id) begunTasks.add(id);
  } catch {
    /* Foreground generation remains available when iOS rejects extra runtime. */
  }
  if (!id) deps.unavailable?.();
  let disposed = false;
  let reported = 0;
  let pendingProgress = Promise.resolve();
  return {
    signal: controller.signal,
    admitted: () => id !== null,
    expired: () => didExpire,
    progress(completed) {
      if (!id || disposed || (deps.monotonicProgress !== false ? completed <= reported : completed === reported)) return;
      reported = completed;
      const activeId = id;
      pendingProgress = pendingProgress
        .then(async () => {
          await deps.progress?.(activeId, completed);
        })
        .catch(() => {});
    },
    async dispose(success = false) {
      if (disposed) return;
      disposed = true;
      deps.events.removeEventListener("risunest-ios-lifecycle", listener);
      signal?.removeEventListener("abort", abort);
      await pendingProgress;
      if (!id) return;
      try {
        await deps.end(id, success);
      } finally {
        begunTasks.delete(id);
      }
    },
  };
}

/** Work that keeps the background-entry assertion until it settles. */
export interface IOSBackgroundHolds {
  settled(): Promise<void>;
}

export function installIOSPersistenceLifecycle(
  flush: (reason: string) => Promise<void>,
  holds?: IOSBackgroundHolds,
): () => void {
  if (!isTauriIOS) return () => {};
  let pending = Promise.resolve();
  let backgroundId: string | undefined;
  // Each background entry counts up; returning to the screen marks every entry so far as returned.
  let entries = 0;
  let returnedThrough = 0;
  const waits = new Set<() => void>();
  const stopWaiting = () => {
    for (const stop of [...waits]) stop();
  };
  const save = (id?: string) => {
    if (id) backgroundId = id;
    const entry = id ? ++entries : entries;
    // Saves run in order and never wait for another entry's background work.
    const flushed = (pending = pending
      .then(() => flush("ios-lifecycle"))
      .catch((error) => {
        console.error("iOS lifecycle save failed", error);
      }));
    if (!id) return;
    void flushed
      .then(async () => {
        if (!holds || returnedThrough >= entry) return;
        // The assertion lasts until deferred holds settle, the app returns, or iOS expires it.
        await new Promise<void>((resolve) => {
          const stop = () => {
            waits.delete(stop);
            resolve();
          };
          waits.add(stop);
          void holds.settled().then(stop, stop);
        });
      })
      .finally(async () => {
        await invoke("plugin:ios-native|end", { id }).catch(() => {});
        if (backgroundId === id) backgroundId = undefined;
      });
  };
  const visibility = () => {
    if (document.hidden) save();
  };
  const native = (event: Event) => {
    const detail = (event as CustomEvent<{ event: string; id?: string }>)
      .detail;
    if (detail?.event === "active" || detail?.event === "background" || detail?.event === "memory-warning") {
      generationDiagnostics.lifecycle(detail.event);
    } else if (detail?.event === "expired") {
      generationDiagnostics.lifecycle("expiry");
    }
    if (detail?.event === "active") {
      returnedThrough = entries;
      stopWaiting();
      void import("./storage/platformBlobStore").then(({ getNativeMediaEndpoint }) =>
        getNativeMediaEndpoint.ensure()
      ).catch(error => console.error("Native media recovery failed", error));
    }
    if (detail?.event === "background") save(detail.id);
    else if (detail?.event === "expired" || detail?.event === "memory-warning") {
      if (detail.event === "expired" && detail.id && detail.id === backgroundId) stopWaiting();
      save();
    }
  };
  document.addEventListener("visibilitychange", visibility);
  window.addEventListener("risunest-ios-lifecycle", native);
  return () => {
    document.removeEventListener("visibilitychange", visibility);
    window.removeEventListener("risunest-ios-lifecycle", native);
  };
}
