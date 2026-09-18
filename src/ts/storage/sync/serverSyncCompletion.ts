import type { ServerCycle, ServerStatus } from "./serverSync";

export interface ServerAttemptIdentity {
  endpoint: string;
  libraryId: string;
  deviceId: string;
}

export function matchesCompletedServerCycle(
  status: ServerStatus | undefined,
  result: ServerCycle | undefined,
  identity: ServerAttemptIdentity | undefined,
): boolean {
  if (!status?.configured || !identity || result?.phase !== "idle") return false;
  if (
    status.registrationRequired ||
    status.reconciling ||
    status.operationPending ||
    status.fullScan ||
    status.dirtyRecords !== 0
  )
    return false;
  const head = status.head;
  return Boolean(
    head &&
      status.endpoint === identity.endpoint &&
      status.libraryId === identity.libraryId &&
      status.deviceId === identity.deviceId &&
      status.localRevision === result.localRevision &&
      head.libraryId === result.head.libraryId &&
      head.epoch === result.head.epoch &&
      head.seq === result.head.seq &&
      head.headId === result.head.headId,
  );
}
