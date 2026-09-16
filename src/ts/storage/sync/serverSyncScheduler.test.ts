import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createServerSyncController } from "./serverSyncController";
import { createServerSyncScheduler } from "./serverSyncScheduler";
import type { ServerCycle, ServerStatus, ServerSyncFacade } from "./serverSync";

const head = {
  libraryId: "library",
  epoch: "epoch",
  seq: "0",
  headId: "head",
  minRetainedSeq: "0",
  sections: {
    hypa: { stateId: "hypa-state", changedSeq: "0", gcFloor: "0" },
    library: { stateId: "library-state", changedSeq: "0", gcFloor: "0" },
    "local-plugins": { stateId: "plugins-state", changedSeq: "0", gcFloor: "0" },
  },
};
function fixture() {
  const status: ServerStatus = {
    localRevision: 0,
    reconciling: false,
    configured: true,
    endpoint: "http://localhost",
    libraryId: "library",
    deviceId: "device",
    head,
    dirtyRecords: 0,
    fullScan: false,
    registrationRequired: false,
    operationPending: false,
  };
  const result: ServerCycle = {
    endpoint: "http://localhost",
    phase: "idle",
    localRevision: 0,
    head,
    conflictCount: 0,
    conflicts: [],
    appliedRecords: 0,
    proposedRecords: 0,
  };
  const cycle = vi.fn(async () => result);
  const controller = createServerSyncController({
    status: async () => status,
    cycle,
    cancel: async () => {},
    needsRefresh: () => false,
  } as unknown as ServerSyncFacade);
  let available = true;
  const scheduler = createServerSyncScheduler(controller, {
    available: () => available,
    random: () => 0.5,
  });
  return {
    controller,
    scheduler,
    cycle,
    result,
    setAvailable: (value: boolean) => {
      available = value;
    },
  };
}
beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());
describe("server sync scheduler", () => {
  it("checks after initialization and lengthens idle polls without reading every local object", async () => {
    const f = fixture();
    await f.controller.initialize();
    await vi.advanceTimersByTimeAsync(0);
    expect(f.cycle).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(59_999);
    expect(f.cycle).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(f.cycle).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(119_999);
    expect(f.cycle).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(1);
    expect(f.cycle).toHaveBeenCalledTimes(3);
    f.scheduler.stop();
  });
  it("debounces saves at 500 ms and bounds continuous edits to five seconds", async () => {
    const f = fixture();
    await f.controller.initialize();
    await vi.advanceTimersByTimeAsync(0);
    for (let i = 0; i < 12; i += 1) {
      f.scheduler.localCommit();
      await vi.advanceTimersByTimeAsync(400);
    }
    expect(f.cycle).toHaveBeenCalledTimes(1);
    f.scheduler.localCommit();
    await vi.advanceTimersByTimeAsync(200);
    expect(f.cycle).toHaveBeenCalledTimes(2);
    f.scheduler.localCommit();
    await vi.advanceTimersByTimeAsync(499);
    expect(f.cycle).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(1);
    expect(f.cycle).toHaveBeenCalledTimes(3);
    f.scheduler.stop();
  });
  it("keeps a local revision arriving during transport for the next single-flight run", async () => {
    const f = fixture();
    let finish!: (value: ServerCycle) => void;
    f.cycle.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    );
    await f.controller.initialize();
    await vi.advanceTimersByTimeAsync(0);
    f.scheduler.localCommit();
    await vi.advanceTimersByTimeAsync(800);
    expect(f.cycle).toHaveBeenCalledTimes(1);
    finish(f.result);
    await f.controller.waitForIdle();
    await vi.advanceTimersByTimeAsync(0);
    expect(f.cycle).toHaveBeenCalledTimes(2);
    f.scheduler.stop();
  });
  it("backs off failures despite edits, resets on manual retry, and stops unauthorized automatic attempts", async () => {
    const f = fixture();
    f.cycle.mockRejectedValue({ code: "server-unreachable" });
    await f.controller.initialize();
    await vi.advanceTimersByTimeAsync(0);
    f.scheduler.localCommit();
    await vi.advanceTimersByTimeAsync(999);
    expect(f.cycle).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(f.cycle).toHaveBeenCalledTimes(2);
    f.scheduler.localCommit();
    await vi.advanceTimersByTimeAsync(1999);
    expect(f.cycle).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(1);
    expect(f.cycle).toHaveBeenCalledTimes(3);
    await f.controller.synchronize();
    await vi.advanceTimersByTimeAsync(1000);
    expect(f.cycle).toHaveBeenCalledTimes(5);
    f.cycle.mockRejectedValue({ code: "unauthorized" });
    await f.controller.synchronize();
    await vi.advanceTimersByTimeAsync(300_000);
    expect(f.cycle).toHaveBeenCalledTimes(6);
    f.scheduler.stop();
  });
  it("reaches the same state from polling alone when no notification arrives", async () => {
    const hinted = fixture();
    const silent = fixture();
    await hinted.controller.initialize();
    await silent.controller.initialize();
    await vi.advanceTimersByTimeAsync(0);
    expect(hinted.cycle).toHaveBeenCalledTimes(1);
    expect(silent.cycle).toHaveBeenCalledTimes(1);
    // One side is told the remote moved; the other is told nothing at all.
    hinted.scheduler.remoteHint();
    await vi.advanceTimersByTimeAsync(250);
    expect(hinted.cycle).toHaveBeenCalledTimes(2);
    expect(silent.cycle).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(silent.cycle).toHaveBeenCalledTimes(2);
    expect(hinted.controller.snapshot().result).toEqual(
      silent.controller.snapshot().result,
    );
    hinted.scheduler.stop();
    silent.scheduler.stop();
  });
  it("coalesces repeated notifications and keeps a failing connection backed off", async () => {
    const f = fixture();
    await f.controller.initialize();
    await vi.advanceTimersByTimeAsync(0);
    for (let i = 0; i < 10; i += 1) f.scheduler.remoteHint();
    await vi.advanceTimersByTimeAsync(250);
    expect(f.cycle).toHaveBeenCalledTimes(2);
    f.cycle.mockRejectedValue({ code: "server-unreachable" });
    f.scheduler.remoteHint();
    await vi.advanceTimersByTimeAsync(250);
    expect(f.cycle).toHaveBeenCalledTimes(3);
    f.scheduler.remoteHint();
    await vi.advanceTimersByTimeAsync(999);
    expect(f.cycle).toHaveBeenCalledTimes(3);
    await vi.advanceTimersByTimeAsync(1);
    expect(f.cycle).toHaveBeenCalledTimes(4);
    f.scheduler.stop();
  });
  it("does no background work and resumes immediately while preserving manual pause", async () => {
    const f = fixture();
    await f.controller.initialize();
    await vi.advanceTimersByTimeAsync(0);
    f.setAvailable(false);
    f.scheduler.suspend();
    f.scheduler.localCommit();
    await vi.advanceTimersByTimeAsync(300_000);
    expect(f.cycle).toHaveBeenCalledTimes(1);
    f.setAvailable(true);
    f.scheduler.resume();
    await vi.advanceTimersByTimeAsync(0);
    expect(f.cycle).toHaveBeenCalledTimes(2);
    await f.controller.pause();
    f.scheduler.resume();
    await vi.advanceTimersByTimeAsync(300_000);
    expect(f.cycle).toHaveBeenCalledTimes(2);
    f.scheduler.stop();
  });
});
