// @vitest-environment node
import { IDBFactory, IDBKeyRange } from "fake-indexeddb";
import { describe, expect, it, vi } from "vitest";
import type { RisuNestDeviceSettings } from "../deviceSettings";
import {
  getAppUpdateSettings,
  updateAppUpdateSettings,
} from "../../update/settings";
import {
  captureDeviceSection,
  databaseSectionId,
  discoverDeviceSections,
  stageDeviceSection,
  type DeviceRow,
  type DeviceSectionId,
  type DeviceSectionInfo,
  type DeviceSectionMetadata,
  type DeviceSpool,
  type DeviceStorageEnvironment,
} from "./scopes";
import { encodeCloneGraph, encodeUtf16 } from "./cloneGraph";
import {
  runDeviceMaintenance,
  type DeviceMaintenanceBootstrap,
  type DeviceMaintenanceSession,
} from "./maintenance";
import type { DeviceNativeInvoke } from "./nativeSpool";
import {
  defaultDeviceExportChoices,
  defaultDeviceRestoreChoices,
  selectedDeviceSections,
} from "./selection";

class MemoryStorage implements Storage {
  private values = new Map<string, string>();
  get length() {
    return this.values.size;
  }
  clear() {
    this.values.clear();
  }
  getItem(key: string) {
    return this.values.get(key) ?? null;
  }
  key(index: number) {
    return [...this.values.keys()][index] ?? null;
  }
  removeItem(key: string) {
    this.values.delete(key);
  }
  setItem(key: string, value: string) {
    this.values.set(key, String(value));
  }
}

export function fixtureEnvironment(): DeviceStorageEnvironment {
  const values = new Map<string, unknown>();
  return {
    localStorage: new MemoryStorage(),
    localforage: {
      keys: async () => [...values.keys()],
      getItem: async (key: string) => values.get(key) ?? null,
      setItem: async (key: string, value: unknown) => {
        values.set(key, value);
        return value;
      },
      removeItem: async (key: string) => {
        values.delete(key);
      },
    } as DeviceStorageEnvironment["localforage"],
    indexedDB: new IDBFactory(),
    keyRange: IDBKeyRange,
    barrier: { assertHeld() {} },
    estimateStorage: async () => ({ usage: 0, quota: 2 ** 30 }),
  };
}

export function fixtureSpool(): DeviceSpool {
  const sections = new Map<
    DeviceSectionId,
    {
      metadata: DeviceSectionMetadata;
      rows: DeviceRow[];
      info?: DeviceSectionInfo;
    }
  >();
  const objects = new Map<string, Uint8Array>();
  return {
    async beginSection(sectionId, metadata) {
      sections.set(sectionId, { metadata, rows: [] });
    },
    async appendRow(sectionId, row) {
      sections.get(sectionId)!.rows.push(row);
    },
    async finishSection(sectionId) {
      const section = sections.get(sectionId)!;
      const info = {
        sectionId,
        metadata: section.metadata,
        recordCount: section.rows.length,
        digest: "a".repeat(64),
      };
      section.info = info;
      return info;
    },
    async sections() {
      return [...sections.values()].map((section) => section.info!);
    },
    async *rows(sectionId, range) {
      const rows = sections.get(sectionId)!.rows;
      yield* rows.slice(range?.startOrdinal ?? 0, range?.endOrdinalExclusive);
    },
    async putBinary(body) {
      const bytes = new Uint8Array(
        body instanceof Blob ? await body.arrayBuffer() : body,
      );
      const hash = Array.from(
        new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)),
        (byte) => byte.toString(16).padStart(2, "0"),
      ).join("");
      objects.set(hash, bytes);
      return hash;
    },
    async getBinary(reference, length) {
      const bytes = objects.get(reference)!;
      expect(bytes.byteLength).toBe(length);
      return bytes;
    },
  };
}

describe("device plugin storage scopes", () => {
  it("replaces selected prefixes, retains unselected scopes and preserves lone surrogates", async () => {
    const environment = fixtureEnvironment();
    const spool = fixtureSpool();
    const key = "safe_plugin_\ud800";
    environment.localStorage.setItem(key, "\udfff");
    environment.localStorage.setItem("appSetting", "untouched");
    const info = await captureDeviceSection(
      "local-storage",
      spool,
      environment,
    );
    environment.localStorage.setItem(key, "changed");
    environment.localStorage.setItem("safe_plugin_removed", "remove");
    await environment.localforage.setItem("safe_plugin_other", "untouched");
    const staged = await stageDeviceSection(info, spool, environment);
    expect(staged.deletionCount).toBe(1);
    expect(environment.localStorage.getItem(key)).toBe("changed");
    await staged.apply();
    expect(environment.localStorage.getItem(key)).toBe("\udfff");
    expect(environment.localStorage.getItem("safe_plugin_removed")).toBeNull();
    expect(environment.localStorage.getItem("appSetting")).toBe("untouched");
    expect(await environment.localforage.getItem("safe_plugin_other")).toBe(
      "untouched",
    );
  });

  it("distinguishes included empty sections from omitted sections", async () => {
    const environment = fixtureEnvironment();
    const spool = fixtureSpool();
    const empty = await captureDeviceSection("localforage", spool, environment);
    expect(empty.recordCount).toBe(0);
    expect(empty.metadata.present).toBe(true);
    await environment.localforage.setItem("safe_plugin_remove", { value: 1 });
    await environment.localforage.setItem("unowned", "retain");
    environment.localStorage.setItem("safe_plugin_omitted", "retain");
    const staged = await stageDeviceSection(empty, spool, environment);
    expect(staged.deletionCount).toBe(1);
    await staged.apply();
    expect(await environment.localforage.keys()).toEqual(["unowned"]);
    expect(environment.localStorage.getItem("safe_plugin_omitted")).toBe(
      "retain",
    );
    expect(
      (await spool.sections()).map((section) => section.sectionId),
    ).toEqual(["localforage"]);
  });

  it("preserves clone graphs in localforage and rejects unsupported selected values", async () => {
    const environment = fixtureEnvironment();
    const spool = fixtureSpool();
    const shared = { value: 12n };
    const value: Record<string, unknown> = {
      left: shared,
      right: shared,
      bytes: new Uint8Array([1, 255]),
      set: new Set([3, 1]),
    };
    value.self = value;
    Object.defineProperty(value, "__proto__", {
      value: "own",
      enumerable: true,
    });
    await environment.localforage.setItem("safe_plugin_value", value);
    const info = await captureDeviceSection("localforage", spool, environment);
    await environment.localforage.removeItem("safe_plugin_value");
    await (await stageDeviceSection(info, spool, environment)).apply();
    const result =
      await environment.localforage.getItem<Record<string, unknown>>(
        "safe_plugin_value",
      );
    expect(result.self).toBe(result);
    expect(result.left).toBe(result.right);
    expect(Object.getOwnPropertyDescriptor(result, "__proto__")?.value).toBe(
      "own",
    );
    await environment.localforage.setItem(
      "safe_plugin_unsupported",
      new WeakMap(),
    );
    await expect(
      captureDeviceSection("localforage", fixtureSpool(), environment),
    ).rejects.toThrow("unsupported");
  });

  it("rejects injected keys and duplicate rows before changing storage", async () => {
    const environment = fixtureEnvironment();
    environment.localStorage.setItem("safe_plugin_existing", "retain");
    const spool = fixtureSpool();
    await spool.beginSection("local-storage", {
      profile: "risunest.device-section/v1",
      sectionId: "local-storage",
      present: true,
    });
    await spool.appendRow("local-storage", {
      kind: "value",
      key: encodeUtf16("appCredential"),
      value: await encodeCloneGraph("injected", spool.putBinary),
    });
    const info = await spool.finishSection("local-storage");
    await expect(stageDeviceSection(info, spool, environment)).rejects.toThrow(
      "outside the plugin boundary",
    );
    expect(environment.localStorage.getItem("safe_plugin_existing")).toBe(
      "retain",
    );
  });

  it("preserves the current four-field settings exactly and normal startup accepts them", async () => {
    const environment = fixtureEnvironment();
    const spool = fixtureSpool();
    const settings = {
      schema: "risunest.device-settings/v1",
      performanceProfile: "low-spec",
      androidKeepAliveDuringGeneration: true,
      nativeFileLogEnabled: false,
    } satisfies RisuNestDeviceSettings;
    const raw = JSON.stringify(settings, null, 2);
    environment.localStorage.setItem("risuNestDeviceSettings", raw);
    const info = await captureDeviceSection(
      "device-settings",
      spool,
      environment,
    );
    environment.localStorage.removeItem("risuNestDeviceSettings");
    await (await stageDeviceSection(info, spool, environment)).apply();
    expect(environment.localStorage.getItem("risuNestDeviceSettings")).toBe(
      raw,
    );
    vi.stubGlobal("localStorage", environment.localStorage);
    vi.resetModules();
    try {
      const { getDeviceSettings } = await import("../deviceSettings");
      expect(getDeviceSettings()).toEqual(settings);
    } finally {
      vi.unstubAllGlobals();
      vi.resetModules();
    }
  });

  it("backs up and restores the separate app update settings record", async () => {
    const environment = fixtureEnvironment();
    const spool = fixtureSpool();
    const raw = JSON.stringify({
      schema: "risunest.app-update-settings/v1",
      autoUpdateCheck: false,
      skippedVersion: "2.3.4",
      lastCheckedAt: 123456,
    });
    environment.localStorage.setItem("risuNestUpdateSettings", raw);
    vi.stubGlobal("localStorage", environment.localStorage);
    expect(getAppUpdateSettings().autoUpdateCheck).toBe(false);
    const info = await captureDeviceSection(
      "device-settings",
      spool,
      environment,
    );
    updateAppUpdateSettings({
      autoUpdateCheck: true,
      skippedVersion: "",
      lastCheckedAt: 999999,
    });
    expect(getAppUpdateSettings().autoUpdateCheck).toBe(true);
    await (await stageDeviceSection(info, spool, environment)).apply();
    expect(environment.localStorage.getItem("risuNestUpdateSettings")).toBe(
      raw,
    );
    expect(getAppUpdateSettings()).toMatchObject({
      autoUpdateCheck: false,
      skippedVersion: "2.3.4",
      lastCheckedAt: 123456,
    });
    vi.unstubAllGlobals();
  });

  it.each([
    ["syncAutoListen", true],
    ["syncListenMethod", "lan"],
    ["syncFixedPort", 32145],
    ["syncPublicBaseUrl", "https://synthetic.invalid"],
  ])(
    "rejects removed peer setting %s before capture or restore",
    async (key, value) => {
      const environment = fixtureEnvironment();
      const raw = JSON.stringify({
        schema: "risunest.device-settings/v1",
        performanceProfile: "low-spec",
        androidKeepAliveDuringGeneration: true,
        nativeFileLogEnabled: false,
        [key]: value,
      });
      environment.localStorage.setItem("risuNestDeviceSettings", raw);
      await expect(
        captureDeviceSection("device-settings", fixtureSpool(), environment),
      ).rejects.toThrow("Unknown device setting");
      const spool = fixtureSpool();
      await spool.beginSection("device-settings", {
        profile: "risunest.device-section/v1",
        sectionId: "device-settings",
        present: true,
      });
      await spool.appendRow("device-settings", {
        kind: "value",
        key: encodeUtf16("risuNestDeviceSettings"),
        value: await encodeCloneGraph(raw, spool.putBinary),
      });
      const info = await spool.finishSection("device-settings");
      environment.localStorage.setItem(
        "risuNestDeviceSettings",
        "retain-current",
      );
      await expect(
        stageDeviceSection(info, spool, environment),
      ).rejects.toThrow("Unknown device setting");
      expect(environment.localStorage.getItem("risuNestDeviceSettings")).toBe(
        "retain-current",
      );
    },
  );

  it("rejects invalid settings rather than replacing them with defaults", async () => {
    const environment = fixtureEnvironment();
    environment.localStorage.setItem(
      "risuNestDeviceSettings",
      '{"schema":"invalid"}',
    );
    await expect(
      captureDeviceSection("device-settings", fixtureSpool(), environment),
    ).rejects.toThrow("invalid");
  });

  it("discovers default scopes from actual plugin databases only", async () => {
    const environment = fixtureEnvironment();
    for (const name of ["safe_plugin_synthetic", "app-owned", "plugin"]) {
      await new Promise<void>((resolve) => {
        const request = environment.indexedDB.open(name);
        request.onsuccess = () => {
          request.result.close();
          resolve();
        };
      });
    }
    expect(await discoverDeviceSections(environment)).toEqual([
      "local-storage",
      "localforage",
      databaseSectionId("safe_plugin_synthetic"),
    ]);
  });

  it("defaults included plugin scopes on and permits explicit omission", async () => {
    const environment = fixtureEnvironment();
    const choices = await defaultDeviceExportChoices(environment.indexedDB);
    expect(selectedDeviceSections(choices)).toEqual([
      "local-storage",
      "localforage",
    ]);
    choices[0].selected = false;
    expect(selectedDeviceSections(choices)).toEqual(["localforage"]);
    const empty = await captureDeviceSection(
      "local-storage",
      fixtureSpool(),
      environment,
    );
    expect(defaultDeviceRestoreChoices([empty])).toEqual([
      {
        sectionId: "local-storage",
        label: "Plugin local storage",
        included: true,
        selected: true,
        recordCount: 0,
      },
    ]);
  });

  it("restores original database absence without requiring space for a new database", async () => {
    const environment = fixtureEnvironment();
    const spool = fixtureSpool();
    const name = "safe_plugin_originally_absent";
    const sectionId = databaseSectionId(name);
    const info = await captureDeviceSection(sectionId, spool, environment);
    expect(info.metadata.present).toBe(false);
    await new Promise<void>((resolve) => {
      const request = environment.indexedDB.open(name);
      request.onsuccess = () => {
        request.result.close();
        resolve();
      };
    });
    environment.estimateStorage = async () => ({ usage: 100, quota: 100 });
    const staged = await stageDeviceSection(info, spool, environment);
    await staged.apply();
    expect(
      (await environment.indexedDB.databases()).some(
        (database) => database.name === name,
      ),
    ).toBe(false);
  });

  it("replays each plugin store from its bounded ordinal range", async () => {
    const environment = fixtureEnvironment();
    const spool = fixtureSpool();
    const name = "safe_plugin_bounded_ranges";
    await new Promise<void>((resolve, reject) => {
      const request = environment.indexedDB.open(name, 1);
      request.onupgradeneeded = () => {
        request.result.createObjectStore("a");
        request.result.createObjectStore("empty");
        request.result.createObjectStore("z");
      };
      request.onerror = () => reject(request.error);
      request.onsuccess = () => {
        const db = request.result;
        const tx = db.transaction(["a", "z"], "readwrite");
        tx.objectStore("a").put({ value: 1 }, "a-1");
        tx.objectStore("z").put({ value: 2 }, "z-1");
        tx.oncomplete = () => {
          db.close();
          resolve();
        };
        tx.onerror = () => reject(tx.error);
      };
    });
    const sectionId = databaseSectionId(name);
    const info = await captureDeviceSection(sectionId, spool, environment);
    const originalRows = spool.rows.bind(spool);
    const rows = vi.spyOn(spool, "rows").mockImplementation(
      (id, range) => originalRows(id, range),
    );

    const staged = await stageDeviceSection(info, spool, environment);
    await staged.apply();
    await staged.cleanup();

    expect(rows.mock.calls[0]).toEqual([sectionId]);
    expect(rows.mock.calls.slice(1).every(([, range]) => range !== undefined))
      .toBe(true);
    expect(rows.mock.calls.some(([, range]) =>
      range?.startOrdinal === 1 && range.endOrdinalExclusive === 1,
    )).toBe(true);
  });
});

function maintenanceFixture(
  environment: DeviceStorageEnvironment,
  source: DeviceSpool,
  rollback: DeviceSpool,
) {
  const session: DeviceMaintenanceSession = {
    sessionId: "synthetic-session",
    jobId: "synthetic-job",
    operation: "restore",
    phase: "preparing",
    includesLibrary: false,
    selectedSections: ["local-storage", "localforage"],
    action: "prepare",
  };
  let active = true;
  const events: string[] = [];
  const initial = (): DeviceMaintenanceBootstrap => ({
    mode: active ? "maintenance" : "normal",
    session: active ? { ...session } : null,
  });
  const invoke = (async (command: string, args?: Record<string, unknown>) => {
    events.push(command);
    if (command === "native_device_backup_bootstrap") return initial();
    if (command === "native_device_backup_prepared") {
      expect((await rollback.sections()).length).toBe(2);
      session.phase = "prepared";
    } else if (
      command === "native_device_backup_section_intent" &&
      !args.rollback
    )
      session.phase = "applying-device";
    else if (command === "native_device_backup_finish_device") {
      session.phase = "committed";
      session.action = "reapply-source";
    } else if (command === "native_device_backup_fail") {
      session.phase = "rolling-back";
      session.action = "rollback";
    } else if (command === "native_device_backup_recovery_complete")
      active = false;
    return undefined;
  }) as DeviceNativeInvoke;
  return {
    initial,
    session,
    events,
    run: () =>
      runDeviceMaintenance(initial(), {
        invoke,
        environment,
        spool: (_sessionId, kind) => (kind === "source" ? source : rollback),
        view: {
          progress() {},
          failed() {},
          async reviewReplacement() {
            events.push("user-reviewed-replacement");
            return true;
          },
          async completed() {
            events.push("user-reviewed-result");
          },
        },
        wait: async () => {},
      }),
  };
}

describe("device maintenance transaction", () => {
  it.each([
    {
      includesLibrary: true,
      action: "reapply-source",
      phase: "committed",
      expected: "true",
    },
    {
      includesLibrary: true,
      action: "complete",
      phase: "committed",
      expected: "true",
    },
    {
      includesLibrary: false,
      action: "reapply-source",
      phase: "committed",
      expected: "false",
    },
    {
      includesLibrary: true,
      action: "rollback",
      phase: "rolling-back",
      expected: "false",
    },
    {
      includesLibrary: true,
      action: "complete",
      phase: "rolled-back",
      expected: "false",
    },
  ] as const)(
    "holds automatic sync only after a committed library restore ($includesLibrary, $phase, $action)",
    async ({ includesLibrary, action, phase, expected }) => {
      const environment = fixtureEnvironment();
      const source = fixtureSpool();
      const rollback = fixtureSpool();
      environment.localStorage.setItem(
        "risuNestServerSyncRestoreHold",
        "false",
      );
      for (const section of ["local-storage", "localforage"] as const) {
        await captureDeviceSection(section, source, environment);
        await captureDeviceSection(section, rollback, environment);
      }
      const fixture = maintenanceFixture(environment, source, rollback);
      Object.assign(fixture.session, { includesLibrary, action, phase });
      const setItem = environment.localStorage.setItem.bind(
        environment.localStorage,
      );
      environment.localStorage.setItem = (key, value) => {
        if (key === "risuNestServerSyncRestoreHold")
          fixture.events.push("automatic-sync-held");
        setItem(key, value);
      };
      await fixture.run();
      expect(
        environment.localStorage.getItem("risuNestServerSyncRestoreHold"),
      ).toBe(expected);
      if (expected === "true") {
        expect(fixture.events.indexOf("automatic-sync-held")).toBeLessThan(
          fixture.events.indexOf("user-reviewed-result"),
        );
        expect(fixture.events.indexOf("automatic-sync-held")).toBeLessThan(
          fixture.events.indexOf("native_device_backup_recovery_complete"),
        );
      } else expect(fixture.events).not.toContain("automatic-sync-held");
      expect(fixture.initial().mode).toBe("normal");
    },
  );

  it("preserves rollback before any write intent and waits for result acknowledgement", async () => {
    const environment = fixtureEnvironment();
    const source = fixtureSpool();
    const rollback = fixtureSpool();
    environment.localStorage.setItem("safe_plugin_key", "new-local");
    await environment.localforage.setItem("safe_plugin_key", "new-forage");
    await captureDeviceSection("local-storage", source, environment);
    await captureDeviceSection("localforage", source, environment);
    environment.localStorage.setItem("safe_plugin_key", "old-local");
    await environment.localforage.setItem("safe_plugin_key", "old-forage");
    const fixture = maintenanceFixture(environment, source, rollback);
    await fixture.run();
    expect(environment.localStorage.getItem("safe_plugin_key")).toBe(
      "new-local",
    );
    expect(await environment.localforage.getItem("safe_plugin_key")).toBe(
      "new-forage",
    );
    expect(
      fixture.events.indexOf("native_device_backup_prepared"),
    ).toBeLessThan(
      fixture.events.indexOf("native_device_backup_section_intent"),
    );
    expect(fixture.events.indexOf("user-reviewed-result")).toBeLessThan(
      fixture.events.indexOf("native_device_backup_recovery_complete"),
    );
    expect(fixture.initial().mode).toBe("normal");
    expect(
      fixture.events.filter(
        (event) => event === "native_device_backup_section_intent",
      ),
    ).toHaveLength(2);
  });

  it("reapplies the committed source after a fresh maintenance bootstrap", async () => {
    const environment = fixtureEnvironment();
    const source = fixtureSpool();
    environment.localStorage.setItem("safe_plugin_key", "committed-local");
    await environment.localforage.setItem(
      "safe_plugin_key",
      "committed-forage",
    );
    await captureDeviceSection("local-storage", source, environment);
    await captureDeviceSection("localforage", source, environment);
    environment.localStorage.setItem("safe_plugin_key", "interrupted-local");
    await environment.localforage.setItem(
      "safe_plugin_key",
      "interrupted-forage",
    );
    const fixture = maintenanceFixture(environment, source, fixtureSpool());
    fixture.session.phase = "committed";
    fixture.session.action = "reapply-source";
    await fixture.run();
    expect(environment.localStorage.getItem("safe_plugin_key")).toBe(
      "committed-local",
    );
    expect(await environment.localforage.getItem("safe_plugin_key")).toBe(
      "committed-forage",
    );
    expect(
      fixture.events.filter(
        (event) => event === "native_device_backup_section_intent",
      ),
    ).toHaveLength(2);
    expect(fixture.events).not.toContain("native_device_backup_finish_device");
  });

  it("verifies already committed sections without rewriting them after a restart", async () => {
    const environment = fixtureEnvironment();
    const source = fixtureSpool();
    environment.localStorage.setItem("safe_plugin_key", "committed-local");
    await environment.localforage.setItem(
      "safe_plugin_key",
      "committed-forage",
    );
    await captureDeviceSection("local-storage", source, environment);
    await captureDeviceSection("localforage", source, environment);
    environment.localStorage.removeItem = () => {
      throw new Error("unexpected storage rewrite");
    };
    environment.localforage.removeItem = async () => {
      throw new Error("unexpected storage rewrite");
    };
    const fixture = maintenanceFixture(environment, source, fixtureSpool());
    fixture.session.phase = "committed";
    fixture.session.action = "reapply-source";
    await fixture.run();
    expect(
      fixture.events.filter(
        (event) => event === "native_device_backup_section_complete",
      ),
    ).toHaveLength(2);
    expect(fixture.events).not.toContain("native_device_backup_fail");
    expect(fixture.initial().mode).toBe("normal");
  });

  it("rolls all selected regions back after a later region fails", async () => {
    const environment = fixtureEnvironment();
    const source = fixtureSpool();
    const rollback = fixtureSpool();
    environment.localStorage.setItem("safe_plugin_key", "new-local");
    await environment.localforage.setItem("safe_plugin_key", "new-forage");
    await captureDeviceSection("local-storage", source, environment);
    await captureDeviceSection("localforage", source, environment);
    environment.localStorage.setItem("safe_plugin_key", "old-local");
    await environment.localforage.setItem("safe_plugin_key", "old-forage");
    const setItem = environment.localforage.setItem;
    let failOnce = true;
    environment.localforage.setItem = (async (key: string, value: unknown) => {
      if (failOnce) {
        failOnce = false;
        throw new Error("synthetic quota failure");
      }
      return setItem(key, value);
    }) as typeof setItem;
    const fixture = maintenanceFixture(environment, source, rollback);
    await fixture.run();
    expect(environment.localStorage.getItem("safe_plugin_key")).toBe(
      "old-local",
    );
    expect(await environment.localforage.getItem("safe_plugin_key")).toBe(
      "old-forage",
    );
    expect(fixture.events).toContain("native_device_backup_fail");
    expect(fixture.events).not.toContain("native_device_backup_finish_device");
    expect(fixture.initial().mode).toBe("normal");
  });
});
