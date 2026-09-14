/** Connect caller and SDK cancellation without requiring AbortSignal.any on iOS 16. */
export function createRequestAbortScope(caller?: AbortSignal | null) {
  const controller = new AbortController();
  const listeners = new Map<AbortSignal, () => void>();
  const link = (signal?: AbortSignal | null) => {
    if (!signal || listeners.has(signal)) return;
    if (signal.aborted) {
      controller.abort(signal.reason);
      return;
    }
    const abort = () => controller.abort(signal.reason);
    listeners.set(signal, abort);
    signal.addEventListener("abort", abort, { once: true });
  };
  link(caller);
  return {
    signal: controller.signal,
    fetch(fetcher: typeof fetch): typeof fetch {
      return (input: RequestInfo | URL, init: RequestInit = {}) => {
        link(
          init.signal ?? (input instanceof Request ? input.signal : undefined),
        );
        return fetcher(input, { ...init, signal: controller.signal });
      };
    },
    dispose() {
      for (const [signal, listener] of listeners)
        signal.removeEventListener("abort", listener);
      listeners.clear();
      // SDK completion markers may precede HTTP EOF. Release the remaining body too.
      controller.abort();
    },
  };
}
