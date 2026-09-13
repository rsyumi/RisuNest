import { invoke } from "@tauri-apps/api/core";
import { isTauriIOS } from "./platform";

export interface IOSNativeState {
  notifications: boolean;
  notificationStatus: number;
  activeTasks: string[];
  expiredTasks: string[];
  foreground: boolean;
  backgroundMode: "limited" | "continued";
}

export const getIOSNativeState = () =>
  invoke<IOSNativeState>("plugin:ios-native|state");
/** A new main document cannot retain work owned by the previous renderer. */
export async function initializeIOSNative(): Promise<void> {
  if (isTauriIOS) await invoke("plugin:ios-native|reset_generation");
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
}
const productionDependencies: IOSGenerationDependencies = {
  enabled: () => isTauriIOS,
  begin: () => invoke("plugin:ios-native|begin"),
  end: (id, success) => invoke("plugin:ios-native|end", { id, success }),
  progress: (id, completed) =>
    invoke("plugin:ios-native|generation_progress", { id, completed }),
  state: getIOSNativeState,
  events: window,
};

/** Retain the caller's cancellation and pair every native assertion with release. */
export async function beginIOSGeneration(
  signal?: AbortSignal,
  deps: IOSGenerationDependencies = productionDependencies,
): Promise<{
  signal: AbortSignal | undefined;
  progress(completed: number): void;
  dispose(success?: boolean): Promise<void>;
}> {
  if (!deps.enabled())
    return { signal, progress: () => {}, dispose: async () => {} };
  const controller = new AbortController();
  const abort = () => controller.abort(signal?.reason);
  signal?.addEventListener("abort", abort, { once: true });
  if (signal?.aborted) abort();
  let id: string | null = null;
  const expired = () =>
    controller.abort(
      new DOMException("iOS background execution expired", "AbortError"),
    );
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
  } catch {
    /* Foreground generation remains available when iOS rejects extra runtime. */
  }
  let disposed = false;
  let reported = 0;
  let pendingProgress = Promise.resolve();
  return {
    signal: controller.signal,
    progress(completed) {
      if (!id || disposed || completed <= reported) return;
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
      if (id) await deps.end(id, success);
    },
  };
}

export function installIOSPersistenceLifecycle(
  flush: (reason: string) => Promise<void>,
): () => void {
  if (!isTauriIOS) return () => {};
  let pending = Promise.resolve();
  const save = (id?: string) => {
    pending = pending
      .then(() => flush("ios-lifecycle"))
      .catch((error) => {
        console.error("iOS lifecycle save failed", error);
      })
      .finally(async () => {
        if (id) await invoke("plugin:ios-native|end", { id }).catch(() => {});
      });
  };
  const visibility = () => {
    if (document.hidden) save();
  };
  const native = (event: Event) => {
    const detail = (event as CustomEvent<{ event: string; id?: string }>)
      .detail;
    if (detail?.event === "background") save(detail.id);
    else if (detail?.event === "expired" || detail?.event === "memory-warning")
      save();
  };
  document.addEventListener("visibilitychange", visibility);
  window.addEventListener("risunest-ios-lifecycle", native);
  return () => {
    document.removeEventListener("visibilitychange", visibility);
    window.removeEventListener("risunest-ios-lifecycle", native);
  };
}
