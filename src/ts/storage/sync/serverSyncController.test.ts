import { afterEach, describe, expect, it, vi } from "vitest";
import { createServerSyncController } from "./serverSyncController";
import type { ServerSyncFacade, ServerStatus } from "./serverSync";

function fixture() {
  const status: ServerStatus = {
    localRevision: 3,
    reconciling: false,
    configured: true,
    endpoint: "http://localhost",
    libraryId: "library",
    deviceId: "device",
    head: {
      libraryId: "library",
      epoch: "epoch",
      seq: "1",
      headId: "head",
      minRetainedSeq: "0",
      sections: {
        hypa: { stateId: "hypa-state", changedSeq: "0", gcFloor: "0" },
        library: { stateId: "library-state", changedSeq: "0", gcFloor: "0" },
        "local-plugins": { stateId: "plugins-state", changedSeq: "0", gcFloor: "0" },
      },
    },
    dirtyRecords: 0,
    fullScan: false,
    registrationRequired: false,
    operationPending: false,
  };
  const facade = {
    status: vi.fn(async () => status),
    cycle: vi.fn(async () => ({
      phase: "idle",
      conflictCount: 0,
      localRevision: 3,
      head: status.head,
    })),
    cancel: vi.fn(async () => {}),
    needsRefresh: vi.fn(() => false),
    bind: vi.fn(async () => status),
    unbind: vi.fn(async () => {}),
    reregister: vi.fn(async () => ({ ...status, reconciling: true })),
    reconcile: vi.fn(async () => ({ ...status, reconciling: true })),
  };
  const controller = createServerSyncController(
    facade as unknown as ServerSyncFacade,
  );
  return { controller, facade, status };
}
afterEach(() => vi.useRealTimers());
describe("server sync controller", () => {
  it("joins an existing synchronization and ignores hidden suspension during an exit drain", async () => {
    const { controller, facade, status } = fixture();
    let finish!: (result: unknown) => void;
    facade.cycle.mockImplementationOnce(
      () => new Promise((resolve) => { finish = resolve; }),
    );
    const running = controller.synchronize();
    const draining = controller.drainToRevision(3, new AbortController().signal);

    await vi.waitFor(() => expect(facade.cycle).toHaveBeenCalledOnce());
    await controller.suspend();
    expect(facade.cancel).not.toHaveBeenCalled();

    finish({
      phase: "idle",
      conflictCount: 0,
      localRevision: 3,
      head: status.head,
    });
    await running;
    await expect(draining).resolves.toEqual({ kind: "complete" });
  });

  it("drains through a newer target revision and restores the prior manual pause", async () => {
    vi.useFakeTimers();
    const { controller, facade, status } = fixture();
    const resumed = vi.fn();
    const pausedController = createServerSyncController(
      facade as unknown as ServerSyncFacade,
      { initiallyPaused: true, onExplicitResume: resumed },
    );
    facade.cycle
      .mockResolvedValueOnce({
        phase: "idle",
        conflictCount: 0,
        localRevision: 3,
        head: status.head,
      })
      .mockImplementationOnce(async () => {
        status.localRevision = 4;
        return {
          phase: "idle",
          conflictCount: 0,
          localRevision: 4,
          head: status.head,
        };
      });

    const draining = pausedController.drainToRevision(
      4,
      new AbortController().signal,
    );
    await vi.advanceTimersByTimeAsync(300);

    await expect(draining).resolves.toEqual({ kind: "complete" });
    expect(facade.cycle).toHaveBeenCalledTimes(2);
    expect(resumed).not.toHaveBeenCalled();
    expect(pausedController.snapshot().paused).toBe(true);
  });

  it("cancels an explicit revision drain through its abort signal", async () => {
    const { controller, facade } = fixture();
    let finish!: () => void;
    facade.cycle.mockImplementationOnce(
      () => new Promise((resolve) => {
        finish = () => resolve({
          phase: "pending",
          conflictCount: 0,
          localRevision: 3,
          head: controller.snapshot().status?.head,
        });
      }),
    );
    const abort = new AbortController();
    const draining = controller.drainToRevision(4, abort.signal);
    await vi.waitFor(() => expect(finish).toBeTypeOf("function"));
    abort.abort();
    expect(facade.cancel).toHaveBeenCalledOnce();
    finish();
    await expect(draining).rejects.toMatchObject({ name: "AbortError" });
  });

  it("holds a restored library across initialization until an explicit sync action", async () => {
    const { facade } = fixture();
    const resumed = vi.fn();
    const controller = createServerSyncController(
      facade as unknown as ServerSyncFacade,
      { initiallyPaused: true, onExplicitResume: resumed },
    );
    await controller.initialize();
    expect(controller.snapshot().paused).toBe(true);
    expect(controller.canAutoSync()).toBe(false);
    expect(facade.cycle).not.toHaveBeenCalled();
    expect(resumed).not.toHaveBeenCalled();
    await controller.synchronize();
    expect(resumed).toHaveBeenCalledOnce();
    expect(controller.canAutoSync()).toBe(true);
    controller.holdAutomaticSync();
    expect(controller.canAutoSync()).toBe(false);
  });
  it.each(["idle", "conflict"])(
    "keeps the %s result when progress publishes while the cycle awaits",
    async (phase) => {
      const { controller, facade } = fixture();
      const result = {
        phase,
        conflictCount: phase === "conflict" ? 1 : 0,
        localRevision: 3,
        head:
          controller.snapshot().status?.head ?? (await facade.status()).head,
      };
      facade.cycle.mockImplementationOnce(async () => {
        controller.reportProgress("preparing");
        await Promise.resolve();
        controller.reportVerifiedBytes("1234");
        controller.reportProgress("publishing");
        return result;
      });
      await controller.synchronize();
      expect(controller.snapshot().error).toBe("");
      expect(controller.snapshot().result).toEqual(result);
      expect(controller.snapshot().verifiedBytes).toBe("1234");
      expect(controller.snapshot().status?.configured).toBe(true);
      expect(controller.snapshot().progress).toBeUndefined();
      if (phase === "idle")
        expect(controller.snapshot().lastSuccessAt).toBeDefined();
      else expect(controller.canAutoSync()).toBe(false);
    },
  );
  it("shows progress only while a synchronization is active and clears it on failure", async () => {
    const { controller, facade } = fixture();
    controller.reportProgress("publishing");
    expect(controller.snapshot().progress).toBeUndefined();
    facade.cycle.mockImplementationOnce(async () => {
      controller.reportProgress("preparing");
      expect(controller.snapshot().progress).toBe("preparing");
      throw { code: "server-timeout" };
    });
    await controller.synchronize();
    expect(controller.snapshot().progress).toBeUndefined();
    expect(controller.snapshot().error).toBe("server-timeout");
  });
  it("shows the latest retryable failure only during the active synchronization", async () => {
    const { controller, facade } = fixture();
    controller.reportRetryableFailure("storage-io");
    expect(controller.snapshot().retryableFailure).toBeUndefined();
    facade.cycle.mockImplementationOnce(async () => {
      controller.reportRetryableFailure("storage-io");
      expect(controller.snapshot().retryableFailure).toBe("storage-io");
      controller.reportRetryableFailure(undefined);
      expect(controller.snapshot().retryableFailure).toBeUndefined();
      controller.reportRetryableFailure("storage-io");
      return {
        phase: "idle",
        conflictCount: 0,
        localRevision: 3,
        head: controller.snapshot().status!.head!,
      };
    });
    await controller.synchronize();
    expect(controller.snapshot().retryableFailure).toBeUndefined();
  });
  it("reports the last completed synchronization without replacing it on failure or conflict", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(1000);
    const { controller, facade } = fixture();
    await controller.initialize();
    expect(controller.snapshot().lastSuccessAt).toBeUndefined();
    await controller.synchronize();
    expect(controller.snapshot().lastSuccessAt).toBe(1000);
    vi.setSystemTime(2000);
    facade.cycle.mockRejectedValueOnce({ code: "server-unreachable" });
    await controller.synchronize();
    expect(controller.snapshot().lastSuccessAt).toBe(1000);
    facade.cycle.mockResolvedValueOnce({
      phase: "conflict",
      conflictCount: 1,
      localRevision: 3,
      head: controller.snapshot().status!.head!,
    });
    await controller.synchronize();
    expect(controller.snapshot().lastSuccessAt).toBe(1000);
  });
  it("holds conflicts for an explicit choice and forwards the preview fence", async () => {
    const { controller, facade } = fixture();
    await controller.initialize();
    facade.cycle.mockResolvedValueOnce({
      phase: "conflict",
      conflictCount: 2,
      localRevision: 3,
      head: controller.snapshot().status!.head!,
    });
    await controller.synchronize();
    expect(controller.canAutoSync()).toBe(false);
    const options = { resolution: "keep-local" as const, expectedRevision: 3 };
    await controller.synchronize(options);
    expect(facade.cycle).toHaveBeenLastCalledWith(options);
    expect(controller.canAutoSync()).toBe(true);
  });
  it("bounds polling and coalesces concurrent foreground requests", async () => {
    vi.useFakeTimers();
    const { controller, facade } = fixture();
    await controller.initialize();
    facade.cycle.mockResolvedValue({
      phase: "pending",
      conflictCount: 0,
      localRevision: 3,
      head: controller.snapshot().status!.head!,
    });
    const first = controller.synchronize();
    expect(controller.synchronize()).toBe(first);
    await vi.runAllTimersAsync();
    await first;
    expect(facade.cycle).toHaveBeenCalledTimes(4);
  });
  it("keeps manual pause across status refresh, but suspension does not pause scheduling", async () => {
    const { controller, facade } = fixture();
    await controller.initialize();
    await controller.suspend();
    expect(controller.canAutoSync()).toBe(true);
    await controller.pause();
    await controller.initialize();
    expect(controller.canAutoSync()).toBe(false);
    expect(facade.cancel).toHaveBeenCalledTimes(2);
  });
  it("stops epoch retry polling and clears the old result only after recovery succeeds", async () => {
    const { controller, facade } = fixture();
    await controller.initialize();
    facade.cycle.mockRejectedValueOnce({
      code: "epoch-reconciliation-required",
    });
    await controller.synchronize();
    expect(controller.canAutoSync()).toBe(false);
    facade.reconcile.mockRejectedValueOnce({ code: "local-revision-changed" });
    await expect(controller.reconcile()).rejects.toMatchObject({
      code: "local-revision-changed",
    });
    expect(controller.snapshot().error).toBe("epoch-reconciliation-required");
    await controller.reconcile();
    expect(controller.snapshot().error).toBe("");
    expect(controller.snapshot().status?.reconciling).toBe(true);
  });
  it("keeps credential loss actionable without repeatedly opening the key store", async () => {
    const { controller, facade } = fixture();
    await controller.initialize();
    facade.cycle.mockRejectedValueOnce({
      code: "device-credential-unavailable",
    });
    await controller.synchronize();
    expect(controller.snapshot().status?.configured).toBe(true);
    expect(controller.canAutoSync()).toBe(false);
    await controller.synchronize();
    expect(controller.snapshot().error).toBe("");
    expect(controller.canAutoSync()).toBe(true);
  });
});

describe("common completion and replacement contract", () => {
  it("unknown or failed native status never authorizes restoration", async () => {
    const { controller, facade } = fixture();
    expect(controller.canRestore()).toBe(false);
    await controller.initialize();
    expect(controller.canRestore()).toBe(true);
    facade.status.mockRejectedValue(new Error("offline local IPC"));
    await controller.initialize();
    expect(controller.canRestore()).toBe(false);
    const apply = vi.fn();
    await expect(controller.withReplacement(apply)).rejects.toThrow();
    expect(apply).not.toHaveBeenCalled();
    expect(controller.snapshot().replacing).toBe(false);
  });
  it.each([
    { dirtyRecords: 1 },
    { fullScan: true },
    { operationPending: true },
    { registrationRequired: true },
    { reconciling: true },
    { configured: false },
    { localRevision: 4 },
    { deviceId: "other" },
  ])(
    "does not complete an idle result with a new tail or changed status %o",
    async (patch) => {
      const { controller, facade, status } = fixture();
      facade.status
        .mockResolvedValueOnce(status)
        .mockResolvedValueOnce({ ...status, ...patch });
      await controller.synchronize();
      expect(controller.snapshot().initialSyncComplete).toBe(false);
      expect(controller.snapshot().lastSuccessAt).toBeUndefined();
    },
  );
  it("ties completion to a new attempt and clears the old result while retrying", async () => {
    const { controller, facade } = fixture();
    await controller.synchronize();
    expect(controller.snapshot().initialSyncComplete).toBe(true);
    const first = controller.snapshot().attemptId;
    facade.cycle.mockImplementationOnce(async () => {
      expect(controller.snapshot().result).toBeUndefined();
      expect(controller.snapshot().initialSyncComplete).toBe(false);
      throw new Error("failed");
    });
    await controller.synchronize();
    expect(controller.snapshot().attemptId).toBeGreaterThan(first!);
    expect(controller.snapshot().initialSyncComplete).toBe(false);
    expect(controller.snapshot().result).toBeUndefined();
  });
  it("refresh failure and local commits invalidate completion", async () => {
    const { controller, facade } = fixture();
    facade.needsRefresh.mockReturnValue(true);
    await controller.synchronize();
    expect(controller.snapshot().refreshPending).toBe(true);
    expect(controller.snapshot().initialSyncComplete).toBe(false);
    facade.needsRefresh.mockReturnValue(false);
    await controller.synchronize();
    expect(controller.snapshot().initialSyncComplete).toBe(true);
    controller.invalidateCompletion();
    expect(controller.snapshot().initialSyncComplete).toBe(false);
  });
  it("reserves replacement before awaits, preserves pause, and never automatically synchronizes", async () => {
    const { controller, facade, status } = fixture();
    await controller.pause();
    let release!: () => void;
    facade.cancel.mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          release = resolve;
        }),
    );
    const apply = vi.fn(async () => "restored");
    const replacement = controller.withReplacement(apply);
    expect(controller.canAutoSync()).toBe(false);
    await expect(controller.synchronize()).rejects.toMatchObject({
      code: "library-operation-busy",
    });
    expect(apply).not.toHaveBeenCalled();
    release();
    expect(await replacement).toBe("restored");
    expect(controller.snapshot().paused).toBe(true);
    expect(facade.cycle).not.toHaveBeenCalled();
    expect(controller.snapshot().replacing).toBe(false);
    facade.status.mockResolvedValue({ ...status, operationPending: true });
    await expect(controller.withReplacement(apply)).rejects.toMatchObject({
      code: "resolve-pending-operation-first",
    });
    expect(apply).toHaveBeenCalledTimes(1);
  });
  it("rejects a restore during an active cycle without automatically cancelling it", async () => {
    const { controller, facade } = fixture();
    let release!: () => void;
    facade.cycle.mockImplementationOnce(async () => {
      await new Promise<void>((r) => {
        release = r;
      });
      return {
        phase: "pending",
        conflictCount: 0,
        localRevision: 3,
        head: null,
      };
    });
    const cycle = controller.synchronize();
    await vi.waitFor(() => expect(release).toBeDefined());
    await expect(controller.withReplacement(vi.fn())).rejects.toMatchObject({
      code: "resolve-pending-operation-first",
    });
    expect(facade.cancel).not.toHaveBeenCalled();
    await controller.pause();
    release();
    await cycle;
  });
});

it("local commits retain conflict intent and the active attempt identity", async () => {
  const { controller, facade, status } = fixture();
  facade.cycle.mockImplementationOnce(async () => {
    controller.invalidateCompletion();
    expect(controller.snapshot().attemptIdentity?.deviceId).toBe(
      status.deviceId,
    );
    return {
      phase: "idle",
      conflictCount: 0,
      localRevision: 3,
      head: status.head,
    };
  });
  await controller.synchronize();
  expect(controller.snapshot().initialSyncComplete).toBe(true);
  facade.cycle.mockResolvedValueOnce({
    phase: "conflict",
    conflictCount: 1,
    localRevision: 3,
    head: status.head,
  });
  await controller.synchronize();
  controller.invalidateCompletion();
  expect(controller.snapshot().result?.phase).toBe("conflict");
  expect(controller.canAutoSync()).toBe(false);
});

it("coalesces a reentrant observer onto the admitted attempt", async () => {
  const { controller, facade } = fixture();
  let reentrant: Promise<void> | undefined;
  const unsubscribe = controller.subscribe((snapshot) => {
    if (snapshot.running && !reentrant) reentrant = controller.synchronize();
  });
  const first = controller.synchronize();
  expect(reentrant).toBe(first);
  await first;
  expect(facade.cycle).toHaveBeenCalledTimes(1);
  unsubscribe();
});
it("rechecks fresh native state immediately before replacement activation", async () => {
  const { controller, facade, status } = fixture();
  await expect(controller.confirmReplacement()).rejects.toMatchObject({
    code: "replacement-not-reserved",
  });
  await expect(
    controller.withReplacement(async () => {
      facade.status.mockResolvedValue({ ...status, operationPending: true });
      await controller.confirmReplacement();
      throw new Error("must not activate");
    }),
  ).rejects.toMatchObject({ code: "resolve-pending-operation-first" });
  expect(controller.snapshot().replacing).toBe(false);
});

it("allows local backup admission without a successful server status or network request", async () => {
  const { controller, facade } = fixture();
  facade.status.mockRejectedValue(new Error("status unavailable"));
  await controller.initialize();
  expect(controller.canRestore()).toBe(false);
  facade.status.mockClear();
  expect(() => controller.assertFileOperationAvailable()).not.toThrow();
  expect(facade.status).not.toHaveBeenCalled();
  expect(facade.cycle).not.toHaveBeenCalled();
  expect(facade.cancel).not.toHaveBeenCalled();
});

it("completes with the authenticated cycle endpoint while preserving the attempt identity", async () => {
  const { controller, facade, status } = fixture();
  const endpoint = "https://new-tunnel.example/";
  facade.status
    .mockResolvedValueOnce(status)
    .mockResolvedValueOnce({ ...status, endpoint });
  facade.cycle.mockResolvedValueOnce({
    phase: "idle",
    endpoint,
    conflictCount: 0,
    localRevision: 3,
    head: status.head,
  } as Awaited<ReturnType<typeof facade.cycle>>);
  await controller.synchronize();
  expect(controller.snapshot().initialSyncComplete).toBe(true);
  expect(controller.snapshot().attemptIdentity).toEqual({
    endpoint,
    libraryId: status.libraryId,
    deviceId: status.deviceId,
  });
});
it("does not complete an address change reported only by status", async () => {
  const { controller, facade, status } = fixture();
  facade.status
    .mockResolvedValueOnce(status)
    .mockResolvedValueOnce({
      ...status,
      endpoint: "https://unverified.example/",
    });
  await controller.synchronize();
  expect(controller.snapshot().initialSyncComplete).toBe(false);
  expect(controller.snapshot().attemptIdentity?.endpoint).toBe(status.endpoint);
});
