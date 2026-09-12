import type { ServerSyncSnapshot } from "./serverSyncController";

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
    !status?.configured ||
    !identity ||
    status.registrationRequired ||
    status.reconciling ||
    status.operationPending ||
    status.fullScan ||
    status.dirtyRecords !== 0 ||
    result?.phase !== "idle" ||
    status.endpoint !== identity.endpoint ||
    status.libraryId !== identity.libraryId ||
    status.deviceId !== identity.deviceId ||
    status.localRevision !== result.localRevision ||
    !status.head ||
    status.head.headId !== result.head.headId ||
    status.head.seq !== result.head.seq ||
    status.head.epoch !== result.head.epoch ||
    status.head.libraryId !== result.head.libraryId
  )
    return null;
  return snapshot.attemptId;
}
