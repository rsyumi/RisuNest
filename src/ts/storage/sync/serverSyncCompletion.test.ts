import { describe, expect, it } from "vitest";
import type { ServerCycle, ServerStatus } from "./serverSync";
import {
  matchesCompletedServerCycle,
  type ServerAttemptIdentity,
} from "./serverSyncCompletion";

function fixture(): {
  status: ServerStatus;
  result: ServerCycle;
  identity: ServerAttemptIdentity;
} {
  const identity = {
    endpoint: "https://example.test/",
    libraryId: "library",
    deviceId: "device",
  };
  const head = {
    libraryId: identity.libraryId,
    epoch: "epoch",
    seq: "7",
    headId: "head",
    minRetainedSeq: "0",
    sections: {
      library: { stateId: "library", changedSeq: "7", gcFloor: "0" },
      hypa: { stateId: "hypa", changedSeq: "6", gcFloor: "0" },
      "local-plugins": { stateId: "plugins", changedSeq: "5", gcFloor: "0" },
    },
  };
  return {
    identity,
    status: {
      ...identity,
      configured: true,
      localRevision: 9,
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
      localRevision: 9,
      head,
      conflictCount: 0,
      conflicts: [],
      appliedRecords: 0,
      proposedRecords: 0,
    },
  };
}

describe("matchesCompletedServerCycle", () => {
  it("accepts only an idle cycle matching current status and attempt identity", () => {
    const { status, result, identity } = fixture();
    expect(matchesCompletedServerCycle(status, result, identity)).toBe(true);
  });

  it.each([
    ["dirty records", (status: ServerStatus) => ({ ...status, dirtyRecords: 1 })],
    ["full scan", (status: ServerStatus) => ({ ...status, fullScan: true })],
    ["reconciliation", (status: ServerStatus) => ({ ...status, reconciling: true })],
    ["pending operation", (status: ServerStatus) => ({ ...status, operationPending: true })],
    ["changed endpoint", (status: ServerStatus) => ({ ...status, endpoint: "https://other.test/" })],
    ["changed revision", (status: ServerStatus) => ({ ...status, localRevision: 10 })],
    ["changed head", (status: ServerStatus) => ({ ...status, head: { ...status.head!, seq: "8" } })],
  ])("rejects %s", (_name, mutate) => {
    const { status, result, identity } = fixture();
    expect(matchesCompletedServerCycle(mutate(status), result, identity)).toBe(false);
  });
});
