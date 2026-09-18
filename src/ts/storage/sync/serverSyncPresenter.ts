import type { ServerSyncSnapshot } from "./serverSyncController";
import { matchesCompletedServerCycle } from "./serverSyncCompletion";

/** Completion belongs to the current identity and verified cycle, never to
 * Promise resolution or historical lastSuccessAt. */
export function completedServerSyncAttempt(
  snapshot: ServerSyncSnapshot,
): number | null {
  const { status, result, attemptIdentity: identity } = snapshot;
  if (
    !snapshot.initialSyncComplete ||
    !snapshot.attemptId ||
    snapshot.running ||
    snapshot.replacing ||
    snapshot.refreshPending ||
    snapshot.error ||
    !matchesCompletedServerCycle(status, result, identity)
  )
    return null;
  return snapshot.attemptId;
}
