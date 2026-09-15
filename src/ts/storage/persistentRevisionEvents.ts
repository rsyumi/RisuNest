import type { DataRevision } from "./persistentDataStore";

export type PersistentRevisionCause = 'edit' | 'generation-complete'

const listeners = new Set<(
  revision: DataRevision,
  cause: PersistentRevisionCause,
) => void>();
export function subscribeLocalPersistentRevision(
  listener: (revision: DataRevision, cause: PersistentRevisionCause) => void,
): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}
export function notifyLocalPersistentRevision(
  revision: DataRevision,
  cause: PersistentRevisionCause = 'edit',
): void {
  for (const listener of listeners) {
    // An observer cannot turn a completed durable save into a failed save.
    try {
      listener(revision, cause);
    } catch {
      /* Polling observers can recover later. */
    }
  }
}
