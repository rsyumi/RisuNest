import { describe, expect, it } from "vitest";
import type { ServerSyncSnapshot } from "./serverSyncController";
import { completedServerSyncAttempt } from "./serverSyncPresenter";

function complete(): ServerSyncSnapshot {
  const head = {
    libraryId: "lib",
    epoch: "epoch",
    seq: "1",
    headId: "a".repeat(64),
    minRetainedSeq: "0",
    sections: {
      hypa: { stateId: "hypa-state", changedSeq: "0", gcFloor: "0" },
      library: { stateId: "library-state", changedSeq: "0", gcFloor: "0" },
      "local-plugins": { stateId: "plugins-state", changedSeq: "0", gcFloor: "0" },
    },
  };
  const identity = {
    endpoint: "https://example.test/",
    libraryId: "lib",
    deviceId: "device",
  };
  return {
    attemptId: 2,
    attemptIdentity: identity,
    running: false,
    paused: false,
    error: "",
    initialSyncComplete: true,
    refreshPending: false,
    status: {
      ...identity,
      localRevision: 3,
      configured: true,
      reconciling: false,
      head,
      dirtyRecords: 0,
      fullScan: false,
      registrationRequired: false,
      operationPending: false,
    },
    result: {
      endpoint: identity.endpoint,
      phase: "idle",
      localRevision: 3,
      head,
      conflictCount: 0,
      conflicts: [],
      appliedRecords: 1,
      proposedRecords: 0,
    },
  };
}
describe("server completion presenter", () => {
  it("identifies the verified current attempt", () => {
    expect(completedServerSyncAttempt(complete())).toBe(2);
  });
  it.each<Partial<ServerSyncSnapshot>>([
    { running: true },
    { replacing: true },
    { refreshPending: true },
    { error: "unauthorized" },
    { initialSyncComplete: false },
    { attemptId: undefined },
    { attemptIdentity: undefined },
    { status: undefined },
    { result: undefined },
  ])(
    "does not infer completion from stale success when state is %j",
    (change) => {
      expect(
        completedServerSyncAttempt({
          ...complete(),
          lastSuccessAt: 123,
          ...change,
        }),
      ).toBeNull();
    },
  );
  it.each([
    { dirtyRecords: 1 },
    { registrationRequired: true },
    { operationPending: true },
    { reconciling: true },
    { fullScan: true },
    { configured: false },
    { deviceId: "other" },
    { endpoint: "https://other.test/" },
    { libraryId: "other" },
    { localRevision: 4 },
    { head: null },
  ])(
    "rejects status that no longer represents the completed attempt %j",
    (change) => {
      const snapshot = complete();
      snapshot.status = { ...snapshot.status!, ...change };
      expect(completedServerSyncAttempt(snapshot)).toBeNull();
    },
  );
});
