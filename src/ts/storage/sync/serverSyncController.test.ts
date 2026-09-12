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
    head: null,
    dirtyRecords: 1,
    fullScan: false,
    registrationRequired: false,
    operationPending: false,
  };
  const facade = {
    status: vi.fn(async () => status),
    cycle: vi.fn(async () => ({ phase: "idle", conflictCount: 0 })),
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
  it.each(["idle", "conflict"])(
    "keeps the %s result when progress publishes while the cycle awaits",
    async (phase) => {
      const { controller, facade } = fixture();
      const result = { phase, conflictCount: phase === "conflict" ? 1 : 0 };
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
    facade.cycle.mockResolvedValueOnce({ phase: "conflict", conflictCount: 1 });
    await controller.synchronize();
    expect(controller.snapshot().lastSuccessAt).toBe(1000);
  });
  it("holds conflicts for an explicit choice and forwards the preview fence", async () => {
    const { controller, facade } = fixture();
    await controller.initialize();
    facade.cycle.mockResolvedValueOnce({ phase: "conflict", conflictCount: 2 });
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
    facade.cycle.mockResolvedValue({ phase: "pending", conflictCount: 0 });
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
