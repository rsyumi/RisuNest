import type { DataRevision } from "./persistentDataStore";

const listeners = new Set<(revision: DataRevision) => void>();
export function subscribeLocalPersistentRevision(
  listener: (revision: DataRevision) => void,
): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}
export function notifyLocalPersistentRevision(revision: DataRevision): void {
  for (const listener of listeners) {
    // An observer cannot turn a completed durable save into a failed save.
    try {
      listener(revision);
    } catch {
      /* Polling observers can recover later. */
    }
  }
}
