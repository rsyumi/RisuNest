import { describe, expect, it, vi } from "vitest";
import { languageEnglish } from "src/lang/en";
import type { ServerSyncSnapshot } from "./serverSyncController";
import {
  connectServerSync,
  serverSyncErrorHelp,
  serverSyncHostLabel,
  serverSyncProgressView,
  serverSyncStatus,
} from "./serverSyncConnectFlow";

const text = languageEnglish.risuNest.serverSync;
const config = {
  endpoint: "https://sync.example/base/",
  libraryId: "library",
  deviceId: "device",
  token: "a".repeat(64),
};
const idle = (): ServerSyncSnapshot => ({ running: false, paused: false, error: "" });
const bound = (): ServerSyncSnapshot => ({
  ...idle(),
  status: {
    localRevision: 3,
    reconciling: false,
    configured: true,
    endpoint: config.endpoint,
    libraryId: "library",
    deviceId: "device",
    head: null,
    dirtyRecords: 37,
    pendingDeviceSections: false,
    fullScan: false,
    registrationRequired: false,
    operationPending: false,
  },
});

describe("connecting a sync server", () => {
  it("binds, stores the asset policy, then runs the first sync in that order", async () => {
    const trace: string[] = [];
    const controller = {
      bind: vi.fn(async () => {
        trace.push("bind");
      }),
      reregister: vi.fn(async () => {
        trace.push("reregister");
      }),
      synchronize: vi.fn(async () => {
        trace.push("synchronize");
      }),
    };
    const setPolicy = vi.fn(async (policy: string) => {
      trace.push(`policy:${policy}`);
    });
    await connectServerSync(controller, setPolicy, { config, residency: "remote" });
    expect(trace).toEqual(["bind", "policy:remote", "synchronize"]);
    expect(controller.bind).toHaveBeenCalledWith(config);
  });
  it("re-registers instead of binding when replacing the device credentials", async () => {
    const controller = {
      bind: vi.fn(async () => {}),
      reregister: vi.fn(async () => {}),
      synchronize: vi.fn(async () => {}),
    };
    await connectServerSync(controller, async () => {}, {
      config,
      residency: "full",
      replacing: true,
    });
    expect(controller.reregister).toHaveBeenCalledWith(config);
    expect(controller.bind).not.toHaveBeenCalled();
    expect(controller.synchronize).toHaveBeenCalledOnce();
  });
  it("does not sync when binding fails", async () => {
    const controller = {
      bind: vi.fn(async () => {
        throw { code: "unauthorized" };
      }),
      reregister: vi.fn(async () => {}),
      synchronize: vi.fn(async () => {}),
    };
    const setPolicy = vi.fn(async () => {});
    await expect(
      connectServerSync(controller, setPolicy, { config, residency: "full" }),
    ).rejects.toMatchObject({ code: "unauthorized" });
    expect(setPolicy).not.toHaveBeenCalled();
    expect(controller.synchronize).not.toHaveBeenCalled();
  });
});

describe("status and error copy", () => {
  it("ranks attention states above activity and activity above the connection", () => {
    expect(serverSyncStatus(idle(), text)).toEqual({
      label: text.disconnected,
      tone: "idle",
    });
    expect(serverSyncStatus(bound(), text)).toEqual({
      label: text.ready,
      tone: "connected",
    });
    expect(
      serverSyncStatus({ ...bound(), running: true, progress: "applying" }, text),
    ).toEqual({ label: text.progress.applying, tone: "working" });
    expect(
      serverSyncStatus(
        {
          ...bound(),
          running: true,
          result: {
            endpoint: config.endpoint,
            phase: "conflict",
            localRevision: 3,
            head: {
              libraryId: "library",
              epoch: "e",
              seq: "1",
              headId: "h",
              minRetainedSeq: "0",
              sections: {
                hypa: { stateId: "hypa-state", changedSeq: "0", gcFloor: "0" },
                library: { stateId: "library-state", changedSeq: "0", gcFloor: "0" },
                "local-plugins": { stateId: "plugins-state", changedSeq: "0", gcFloor: "0" },
              },
            },
            conflictCount: 2,
            conflicts: [],
            appliedRecords: 0,
            proposedRecords: 0,
          },
        },
        text,
      ),
    ).toEqual({ label: text.conflict, tone: "attention" });
    expect(
      serverSyncStatus(bound(), text, "committed-refresh-pending"),
    ).toEqual({ label: text.refreshPending, tone: "attention" });
    expect(serverSyncStatus({ ...bound(), paused: true }, text)).toEqual({
      label: text.paused,
      tone: "paused",
    });
  });
  it("explains refresh and credential failures specifically and everything else generally", () => {
    expect(serverSyncErrorHelp("committed-refresh-pending", text)).toBe(
      text.refreshHelp,
    );
    expect(serverSyncErrorHelp("activation-confirmation-pending", text)).toBe(
      text.activationHelp,
    );
    expect(serverSyncErrorHelp("device-credential-unavailable", text)).toBe(
      text.credentialUnavailable,
    );
    expect(serverSyncErrorHelp("server-unreachable", text)).toBe(text.errorHelp);
  });
  it("shows the address without its scheme", () => {
    expect(serverSyncHostLabel("https://sync.example/base/")).toBe(
      "sync.example/base",
    );
    expect(serverSyncHostLabel("not a url")).toBe("not a url");
  });
});

describe("progress view", () => {
  it("is indeterminate until native reports the cycle's record count", () => {
    const view = serverSyncProgressView(
      { ...bound(), running: true, progress: "preparing", attemptStartedAt: 1000 },
      text,
      43_000,
    );
    expect(view.percent).toBeNull();
    expect(view.current).toBe(text.progress.preparing);
    expect(view.elapsed).toBe(`${text.elapsed} 00:42`);
    expect(view.stages.map((stage) => stage.state)).toEqual([
      "done",
      "active",
      "pending",
      "pending",
      "pending",
    ]);
    expect(view.counters.map((counter) => counter.value)).toEqual([
      "-",
      "-",
      "-",
      "37",
    ]);
  });
  it("derives the bar, the stage detail and the four tiles from the snapshot", () => {
    const view = serverSyncProgressView(
      {
        ...bound(),
        running: true,
        progress: "applying",
        verifiedBytes: String(13 * 1024 * 1024),
        bytesPerSecond: 1.8 * 1024 * 1024,
        cycleItems: { done: 1240, total: 3512 },
        attemptStartedAt: 0,
      },
      text,
      5_000,
    );
    expect(view.percent).toBe(35);
    expect(view.current).toBe(`${text.progress.applying} · 1,240 / 3,512`);
    expect(view.stages[1]).toMatchObject({ state: "done", detail: "3,512 items" });
    expect(view.stages[2]).toMatchObject({ state: "active", detail: "1,240 / 3,512" });
    expect(view.stages[4]).toMatchObject({ state: "pending", detail: "" });
    expect(view.counters).toEqual([
      { key: "bytes", label: text.verifiedBytes, value: "13.0 MiB" },
      { key: "rate", label: text.transferRate, value: "1.8 MiB/s" },
      { key: "items", label: text.progressItems, value: "1,240 / 3,512" },
      { key: "pending", label: text.pendingChanges, value: "37" },
    ]);
  });
  it("names the first full comparison instead of a change count", () => {
    const snapshot = bound();
    snapshot.status!.fullScan = true;
    const view = serverSyncProgressView({ ...snapshot, running: true }, text, 0);
    expect(view.counters[3].value).toBe(text.initialScan);
    expect(view.elapsed).toBe("");
  });
});
