import { describe, expect, it, vi } from "vitest";
import { createServerSyncFacade, type ServerCycle } from "./serverSync";
import type { CommittedApplyOutcome } from '../persistentDataRuntime';

const head = {
  libraryId: "library",
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
const result: ServerCycle = {
  endpoint: "http://localhost",
  phase: "idle",
  localRevision: 8,
  head,
  conflictCount: 0,
  conflicts: [],
  appliedRecords: 1,
  proposedRecords: 0,
};
function fixture(
  onVerifiedBytes?: (bytes: string) => void,
  onRetryableFailure?: (code: string | undefined) => void,
) {
  const trace: string[] = [];
  let fenced = false;
  const fence = {
    revision: 7,
    refreshCommittedWorkingSet: vi.fn(async (): Promise<CommittedApplyOutcome> => {
      expect(fenced).toBe(true);
      trace.push("refresh");
      return { kind: 'committed', revision: 8, projection: 'applied' };
    }),
    release: vi.fn(() => {
      fenced = false;
      trace.push("release");
    }),
  };
  const runtime = {
    flushPendingData: vi.fn(async () => {
      expect(fenced).toBe(false);
    }),
    capturePersistentMutationToken: vi.fn(async () => ({ revision: 7, mutationGeneration: 1 })),
    acquireDestructiveReplacementFence: vi.fn(async () => {
      fenced = true;
      trace.push("fence");
      return fence;
    }),
    refreshActiveWorkingSetFromStore: vi.fn(async (revision: number): Promise<CommittedApplyOutcome> => {
      expect(fenced).toBe(false);
      trace.push('read-only-refresh');
      return { kind: 'committed', revision, projection: 'applied' };
    }),
  };
  const native = vi.fn(async (command: string) => {
    trace.push(command);
    if (command === "server_sync_prepare") {
      expect(fenced).toBe(false);
      return {
        kind: "ready",
        preparationId: "prepared",
        localRevision: 7,
        head,
        appliedRecords: 1,
      };
    }
    if (command === "server_sync_activate") {
      expect(fenced).toBe(true);
      return 8;
    }
    if (command === "server_sync_publish") {
      expect(fenced).toBe(false);
      return result;
    }
    return undefined;
  });
  const facade = createServerSyncFacade({
    runtime,
    invoke: native as never,
    onProgress: (phase) => progress.push(phase),
    onVerifiedBytes,
    onRetryableFailure,
  });
  const progress: string[] = [];
  return { facade, native, runtime, fence, trace, progress };
}
describe("server sync activation boundary", () => {
  it("does not let a stalled progress reply hold completion or update a later cycle", async () => {
    vi.useFakeTimers();
    try {
      const receive = vi.fn();
      const { facade, native } = fixture(receive);
      let finish!: (value: unknown) => void;
      let progress!: (value: unknown) => void;
      native.mockImplementation((command) => {
        if (command === "server_sync_prepare")
          return new Promise((resolve) => {
            finish = resolve;
          }) as never;
        if (command === "server_sync_verified_bytes")
          return new Promise((resolve) => {
            progress = resolve;
          }) as never;
        throw new Error("Unexpected command");
      });
      const active = facade.cycle();
      await vi.advanceTimersByTimeAsync(3000);
      expect(
        native.mock.calls.filter(
          ([command]) => command === "server_sync_verified_bytes",
        ),
      ).toHaveLength(1);
      finish({ kind: "report", result });
      await expect(active).resolves.toEqual(result);
      progress("1234");
      await Promise.resolve();
      expect(receive).not.toHaveBeenCalled();
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });
  it("reports a sanitized retryable server failure while the transfer keeps running", async () => {
    vi.useFakeTimers();
    try {
      const receive = vi.fn();
      const { facade, native } = fixture(undefined, receive);
      let finish!: (value: unknown) => void;
      native.mockImplementation((command) => {
        if (command === "server_sync_prepare")
          return new Promise((resolve) => {
            finish = resolve;
          }) as never;
        if (command === "server_sync_retryable_failure")
          return Promise.resolve("storage-io") as never;
        throw new Error("Unexpected command");
      });
      const active = facade.cycle();
      await vi.advanceTimersByTimeAsync(1000);
      expect(receive).toHaveBeenCalledWith("storage-io");
      finish({ kind: "report", result });
      await expect(active).resolves.toEqual(result);
    } finally {
      vi.useRealTimers();
    }
  });
  it("reports preparation, activation and publication in their actual order", async () => {
    const { facade, progress, native } = fixture();
    await facade.cycle();
    expect(progress).toEqual([
      "saving",
      "preparing",
      "applying",
      "refreshing",
      "publishing",
    ]);
    progress.length = 0;
    native.mockResolvedValueOnce({
      kind: "report",
      result: { ...result, phase: "conflict" },
    } as never);
    await facade.cycle();
    expect(progress).toEqual(["saving", "preparing"]);
  });
  it("flushes local edits and guards recovery with the freshly read revision", async () => {
    const { facade, native, runtime } = fixture();
    native.mockResolvedValue({ localRevision: 19 } as never);
    const config = {
      endpoint: "https://example.com",
      libraryId: "library",
      deviceId: "new-device",
      token: "synthetic",
    };
    await facade.reregister(config);
    expect(runtime.flushPendingData).toHaveBeenCalledWith(
      "server-sync-recovery",
    );
    expect(native).toHaveBeenLastCalledWith("server_sync_reregister", {
      config,
      expectedRevision: 19,
    });
    await facade.reconcile();
    expect(native).toHaveBeenLastCalledWith("server_sync_reconcile", {
      expectedRevision: 19,
    });
    expect(runtime.acquireDestructiveReplacementFence).not.toHaveBeenCalled();
  });
  it("allows edits during network work and fences only activation and refresh", async () => {
    const { facade, trace, fence } = fixture();
    await expect(facade.cycle()).resolves.toEqual(result);
    expect(trace).toEqual([
      "server_sync_prepare",
      "fence",
      "server_sync_activate",
      "refresh",
      "release",
      "server_sync_publish",
    ]);
    expect(fence.refreshCommittedWorkingSet).toHaveBeenCalledWith(8);
  });
  it.each(['returned', 'thrown'] as const)("releases the physical fence after a %s refresh failure and retries read-only", async (failure) => {
    const { facade, native, fence, runtime } = fixture();
    if (failure === 'returned') {
      fence.refreshCommittedWorkingSet.mockResolvedValueOnce({
        kind: 'committed', revision: 8, projection: 'refresh-required',
      });
    } else {
      fence.refreshCommittedWorkingSet.mockRejectedValueOnce(new Error('synthetic refresh failure'));
    }
    await expect(facade.cycle()).rejects.toMatchObject({
      code: "committed-refresh-pending",
    });
    expect(fence.release).toHaveBeenCalledOnce();
    expect(facade.needsRefresh()).toBe(true);
    await expect(facade.reconcile()).rejects.toMatchObject({
      code: "committed-refresh-pending",
    });
    await expect(facade.cancel()).rejects.toMatchObject({
      code: "committed-refresh-pending",
    });
    await expect(facade.cycle()).resolves.toEqual(result);
    expect(
      native.mock.calls.filter(
        ([command]) => command === "server_sync_activate",
      ),
    ).toHaveLength(1);
    expect(fence.release).toHaveBeenCalledTimes(1);
    expect(runtime.refreshActiveWorkingSetFromStore).toHaveBeenCalledExactlyOnceWith(8);
    expect(fence.refreshCommittedWorkingSet).toHaveBeenCalledOnce();
  });
  it("releases the fence and discards preparation if local edits invalidate the prepared revision", async () => {
    const { facade, native, fence } = fixture();
    const original = native.getMockImplementation()!;
    native.mockImplementation(async (command) => {
      if (command === "server_sync_activate")
        throw { code: "local-revision-changed", status: 409 };
      return original(command);
    });
    await expect(facade.cycle()).rejects.toMatchObject({
      code: "local-revision-changed",
    });
    expect(fence.release).toHaveBeenCalledTimes(1);
    expect(native).toHaveBeenCalledWith("server_sync_cancel");
    expect(
      native.mock.calls.some(([command]) => command === "server_sync_publish"),
    ).toBe(false);
  });
  it("uses one active operation for duplicate run requests", async () => {
    const { facade, native } = fixture();
    const first = facade.cycle();
    const second = facade.cycle();
    expect(first).toBe(second);
    await first;
    expect(
      native.mock.calls.filter(
        ([command]) => command === "server_sync_prepare",
      ),
    ).toHaveLength(1);
  });
  it("retains the fence after an uncertain activation reply and confirms the same preparation", async () => {
    const { facade, native, fence } = fixture();
    const original = native.getMockImplementation()!;
    let lost = true;
    native.mockImplementation(async (command) => {
      if (command === "server_sync_activate" && lost) {
        lost = false;
        throw new Error("synthetic lost IPC reply");
      }
      return original(command);
    });
    await expect(facade.cycle()).rejects.toMatchObject({
      code: "activation-confirmation-pending",
    });
    expect(fence.release).not.toHaveBeenCalled();
    expect(facade.needsRefresh()).toBe(true);
    await expect(facade.cancel()).rejects.toMatchObject({
      code: "activation-confirmation-pending",
    });
    await expect(facade.cycle()).resolves.toEqual(result);
    expect(
      native.mock.calls.filter(
        ([command]) => command === "server_sync_prepare",
      ),
    ).toHaveLength(1);
    expect(
      native.mock.calls.filter(
        ([command]) => command === "server_sync_activate",
      ),
    ).toHaveLength(2);
    expect(fence.refreshCommittedWorkingSet).toHaveBeenCalledOnce();
    expect(fence.release).toHaveBeenCalledOnce();
  });
});
