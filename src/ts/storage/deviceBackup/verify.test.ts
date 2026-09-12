// @vitest-environment node
import { IDBFactory, IDBKeyRange } from "fake-indexeddb";
import { describe, expect, it, vi } from "vitest";
import { CloneGraphError, encodeCloneGraph, encodeUtf16 } from "./cloneGraph";
import {
  captureDeviceSection,
  databaseSectionId,
  type DeviceRow,
  type DeviceSectionInfo,
  type DeviceSectionMetadata,
  type DeviceSpool,
  type DeviceStorageEnvironment,
} from "./scopes";
import { verifyCurrentDeviceSection } from "./verify";

function fixture() {
  const local = new Map<string, string>();
  const forage = new Map<string, unknown>();
  const environment: DeviceStorageEnvironment = {
    localStorage: {
      get length() {
        return local.size;
      },
      key: (index) => [...local.keys()][index] ?? null,
      getItem: (key) => local.get(key) ?? null,
      setItem: vi.fn((key, value) => {
        local.set(key, String(value));
      }),
      removeItem: vi.fn((key) => {
        local.delete(key);
      }),
      clear: vi.fn(() => {
        local.clear();
      }),
    },
    localforage: {
      keys: async () => [...forage.keys()],
      getItem: async (key) => forage.get(key) ?? null,
      setItem: vi.fn(async (key: string, value: unknown) => {
        forage.set(key, value);
        return value;
      }),
      removeItem: vi.fn(async (key: string) => {
        forage.delete(key);
      }),
    } as DeviceStorageEnvironment["localforage"],
    indexedDB: new IDBFactory(),
    keyRange: IDBKeyRange,
    barrier: { assertHeld: vi.fn() },
    estimateStorage: vi.fn(async () => ({ usage: 100, quota: 100 })),
  };
  const sections = new Map<
    string,
    { metadata: DeviceSectionMetadata; rows: DeviceRow[] }
  >();
  const binaries = new Map<string, Uint8Array>();
  const source: DeviceSpool = {
    beginSection: vi.fn(async (sectionId, metadata) => {
      sections.set(sectionId, { metadata, rows: [] });
    }),
    appendRow: vi.fn(async (sectionId, row) => {
      sections.get(sectionId)!.rows.push(row);
    }),
    finishSection: vi.fn(async (sectionId): Promise<DeviceSectionInfo> => {
      const section = sections.get(sectionId)!;
      return {
        sectionId,
        metadata: section.metadata,
        recordCount: section.rows.length,
        digest: "a".repeat(64),
      };
    }),
    sections: async () => [],
    async *rows(sectionId) {
      yield* sections.get(sectionId)!.rows;
    },
    putBinary: vi.fn(async (body) => {
      // The verifier must compare content, not the source's opaque reference.
      const reference = `synthetic-object-${binaries.size}`;
      binaries.set(
        reference,
        new Uint8Array(
          body instanceof Blob ? await body.arrayBuffer() : body.slice(0),
        ),
      );
      return reference;
    }),
    getBinary: vi.fn(async (reference, bytes) => {
      const value = binaries.get(reference);
      if (!value || value.byteLength !== bytes)
        throw new Error("Synthetic binary integrity failure");
      return value;
    }),
  };
  const assertReadOnly = () => {
    for (const method of [
      "beginSection",
      "appendRow",
      "finishSection",
      "putBinary",
    ] as const) {
      expect(source[method]).not.toHaveBeenCalled();
    }
    expect(environment.localStorage.setItem).not.toHaveBeenCalled();
    expect(environment.localStorage.removeItem).not.toHaveBeenCalled();
    expect(environment.localStorage.clear).not.toHaveBeenCalled();
    expect(environment.localforage.setItem).not.toHaveBeenCalled();
    expect(environment.localforage.removeItem).not.toHaveBeenCalled();
    expect(environment.estimateStorage).not.toHaveBeenCalled();
  };
  return {
    environment,
    source,
    local,
    forage,
    sections,
    binaries,
    assertReadOnly,
  };
}

async function openDatabase(
  environment: DeviceStorageEnvironment,
  name: string,
  version = 1,
  upgrade?: (db: IDBDatabase) => void,
): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = environment.indexedDB.open(name, version);
    request.onupgradeneeded = () => upgrade?.(request.result);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function writeDatabase(
  db: IDBDatabase,
  action: (store: IDBObjectStore) => void,
): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction("records", "readwrite");
    tx.oncomplete = () => resolve();
    tx.onabort = () => reject(tx.error);
    action(tx.objectStore("records"));
  });
}

describe("read-only device section verification", () => {
  it("matches selected local keys and values while ignoring unowned storage", async () => {
    const state = fixture();
    state.local.set("safe_plugin_\ud800", "\udfff");
    state.local.set("outside", "original");
    const info = await captureDeviceSection(
      "local-storage",
      state.source,
      state.environment,
    );
    state.local.set("outside", "different");
    vi.clearAllMocks();
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(true);
    state.assertReadOnly();
  });

  it.each(["value", "missing", "extra"])(
    "returns false for a %s difference without writing",
    async (difference) => {
      const state = fixture();
      state.local.set("safe_plugin_a", "original");
      const info = await captureDeviceSection(
        "local-storage",
        state.source,
        state.environment,
      );
      if (difference === "value") state.local.set("safe_plugin_a", "changed");
      else if (difference === "missing") state.local.delete("safe_plugin_a");
      else state.local.set("safe_plugin_b", "extra");
      vi.clearAllMocks();
      expect(
        await verifyCurrentDeviceSection(info, state.source, state.environment),
      ).toBe(false);
      state.assertReadOnly();
    },
  );

  it("compares binary content, shared identities, cycles and collection order", async () => {
    const state = fixture();
    const shared = { value: 1n };
    const bytes = new Uint8Array([1, 2, 3]);
    const value: Record<string, unknown> = {
      left: shared,
      right: shared,
      bytes,
      blob: new Blob([new Uint8Array([4, 5])]),
      map: new Map([
        [1, "first"],
        [2, "second"],
      ]),
    };
    value.self = value;
    state.forage.set("safe_plugin_a", value);
    const info = await captureDeviceSection(
      "localforage",
      state.source,
      state.environment,
    );
    vi.clearAllMocks();
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(true);
    bytes[2] = 4;
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(false);
    bytes[2] = 3;
    value.right = { value: 1n };
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(false);
    value.right = shared;
    value.map = new Map([
      [2, "second"],
      [1, "first"],
    ]);
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(false);
    state.assertReadOnly();
  });

  it("normalizes semantically equal source graph numbering", async () => {
    const state = fixture();
    state.forage.set("safe_plugin_a", { first: "one", second: "two" });
    const info = await captureDeviceSection(
      "localforage",
      state.source,
      state.environment,
    );
    const row = state.sections.get("localforage")!.rows[0];
    expect(row.value.nodes[0].type).toBe("object");
    const object = row.value.nodes[0];
    if (object.type !== "object")
      throw new Error("Synthetic fixture shape changed");
    [row.value.nodes[1], row.value.nodes[2]] = [
      row.value.nodes[2],
      row.value.nodes[1],
    ];
    object.properties[0][1] = 2;
    object.properties[1][1] = 1;
    vi.clearAllMocks();
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(true);
    state.assertReadOnly();
  });

  it("verifies the exact current device settings without changing their raw JSON", async () => {
    const state = fixture();
    const settings = {
      schema: "risunest.device-settings/v1",
      performanceProfile: "low-spec",
      androidKeepAliveDuringGeneration: true,
      nativeFileLogEnabled: false,
    };
    const raw = JSON.stringify(settings, null, 2);
    state.local.set("risuNestDeviceSettings", raw);
    const info = await captureDeviceSection(
      "device-settings",
      state.source,
      state.environment,
    );
    vi.clearAllMocks();
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(true);
    state.local.set("risuNestDeviceSettings", JSON.stringify(settings));
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(false);
    state.local.set(
      "risuNestDeviceSettings",
      JSON.stringify({ ...settings, nativeFileLogEnabled: true }, null, 2),
    );
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(false);
    state.assertReadOnly();
  });

  it("accepts an already-correct database at full quota without staging or advancing its generator", async () => {
    const state = fixture();
    const name = "safe_plugin_full_quota";
    const sectionId = databaseSectionId(name);
    let db = await openDatabase(state.environment, name, 1, (db) => {
      db.createObjectStore("records", { autoIncrement: true });
    });
    await writeDatabase(db, (store) => {
      store.put({ value: "kept" }, 1);
      store.put({ value: "deleted" }, 50);
      store.delete(50);
    });
    db.close();
    const info = await captureDeviceSection(
      sectionId,
      state.source,
      state.environment,
    );
    const before = await state.environment.indexedDB.databases();
    const open = vi.spyOn(state.environment.indexedDB, "open");
    const remove = vi.spyOn(state.environment.indexedDB, "deleteDatabase");
    vi.clearAllMocks();
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(true);
    expect(await state.environment.indexedDB.databases()).toEqual(before);
    expect(open.mock.calls.every(([actualName]) => actualName === name)).toBe(
      true,
    );
    expect(remove).not.toHaveBeenCalled();
    state.assertReadOnly();
    db = await openDatabase(state.environment, name);
    let next: IDBValidKey | undefined;
    await writeDatabase(db, (store) => {
      const request = store.add({ value: "next" });
      request.onsuccess = () => {
        next = request.result;
      };
    });
    db.close();
    expect(next).toBe(51);
  });

  it("distinguishes an absent database from an existing empty database", async () => {
    const state = fixture();
    const name = "safe_plugin_absent";
    const info = await captureDeviceSection(
      databaseSectionId(name),
      state.source,
      state.environment,
    );
    vi.clearAllMocks();
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(true);
    expect(await state.environment.indexedDB.databases()).toEqual([]);
    (await openDatabase(state.environment, name)).close();
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(false);
    state.assertReadOnly();
  });

  it("detects hidden generator changes even when all retained records are equal", async () => {
    const state = fixture();
    const name = "safe_plugin_generator";
    const db = await openDatabase(state.environment, name, 1, (db) => {
      db.createObjectStore("records", { autoIncrement: true });
    });
    await writeDatabase(db, (store) => {
      store.add({ value: "kept" });
    });
    db.close();
    const info = await captureDeviceSection(
      databaseSectionId(name),
      state.source,
      state.environment,
    );
    const current = await openDatabase(state.environment, name);
    await writeDatabase(current, (store) => {
      store.put({}, 200);
      store.delete(200);
    });
    current.close();
    vi.clearAllMocks();
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(false);
    state.assertReadOnly();
  });

  it("detects database version and index schema changes with unchanged values", async () => {
    const state = fixture();
    const name = "safe_plugin_schema";
    (
      await openDatabase(state.environment, name, 1, (db) => {
        db.createObjectStore("records");
      })
    ).close();
    const info = await captureDeviceSection(
      databaseSectionId(name),
      state.source,
      state.environment,
    );
    const changed = await new Promise<IDBDatabase>((resolve, reject) => {
      const request = state.environment.indexedDB.open(name, 2);
      request.onupgradeneeded = () => {
        request
          .transaction!.objectStore("records")
          .createIndex("new-index", "value", { unique: true });
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    });
    changed.close();
    vi.clearAllMocks();
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(false);
    state.assertReadOnly();
  });

  it.each(["index", "keyPath", "autoIncrement", "store"])(
    "detects a %s schema difference at the same database version",
    async (difference) => {
      const state = fixture();
      const name = "safe_plugin_same_version";
      (
        await openDatabase(state.environment, name, 1, (db) => {
          const store = db.createObjectStore("records");
          if (difference === "index")
            store.createIndex("index", "value", { unique: true });
        })
      ).close();
      const info = await captureDeviceSection(
        databaseSectionId(name),
        state.source,
        state.environment,
      );
      await new Promise<void>((resolve, reject) => {
        const request = state.environment.indexedDB.deleteDatabase(name);
        request.onsuccess = () => resolve();
        request.onerror = () => reject(request.error);
      });
      (
        await openDatabase(state.environment, name, 1, (db) => {
          if (difference !== "store")
            db.createObjectStore("records", {
              keyPath: difference === "keyPath" ? "id" : null,
              autoIncrement: difference === "autoIncrement",
            });
        })
      ).close();
      vi.clearAllMocks();
      expect(
        await verifyCurrentDeviceSection(info, state.source, state.environment),
      ).toBe(false);
      state.assertReadOnly();
    },
  );

  it("returns false for an unsupported current value while still checking every source row", async () => {
    const state = fixture();
    state.forage.set("safe_plugin_a", "first");
    state.forage.set("safe_plugin_b", new Uint8Array([1, 2]));
    const info = await captureDeviceSection(
      "localforage",
      state.source,
      state.environment,
    );
    state.forage.set("safe_plugin_a", new WeakMap());
    vi.clearAllMocks();
    expect(
      await verifyCurrentDeviceSection(info, state.source, state.environment),
    ).toBe(false);
    expect(state.source.getBinary).toHaveBeenCalledTimes(1);
    state.assertReadOnly();
  });

  it.each(["different", "unsupported"])(
    "propagates source decode errors after a %s current value",
    async (current) => {
      const state = fixture();
      state.forage.set("safe_plugin_a", "first");
      state.forage.set("safe_plugin_b", "second");
      const info = await captureDeviceSection(
        "localforage",
        state.source,
        state.environment,
      );
      state.forage.set(
        "safe_plugin_a",
        current === "unsupported" ? new WeakMap() : "changed",
      );
      state.sections.get("localforage")!.rows[1].value = {
        version: 1,
        root: 0,
        nodes: [{ type: "future-node" } as never],
      };
      vi.clearAllMocks();
      await expect(
        verifyCurrentDeviceSection(info, state.source, state.environment),
      ).rejects.toMatchObject({ code: "unsupported", valueType: "node-tag" });
      state.assertReadOnly();
    },
  );

  it("propagates binary integrity failures rather than treating them as mismatches", async () => {
    const state = fixture();
    state.forage.set("safe_plugin_a", new Uint8Array([1, 2]));
    const info = await captureDeviceSection(
      "localforage",
      state.source,
      state.environment,
    );
    state.forage.set("safe_plugin_a", new WeakMap());
    const failure = new Error("Synthetic binary integrity failure");
    state.source.getBinary = async () => {
      throw failure;
    };
    await expect(
      verifyCurrentDeviceSection(info, state.source, state.environment),
    ).rejects.toBe(failure);
  });

  it("propagates late source iterator errors and closes the stream", async () => {
    const state = fixture();
    state.local.set("safe_plugin_a", "first");
    const info = await captureDeviceSection(
      "local-storage",
      state.source,
      state.environment,
    );
    const rows = state.sections.get("local-storage")!.rows;
    state.local.set("safe_plugin_a", "changed");
    const closed = vi.fn();
    const failure = new Error("Synthetic row integrity failure");
    state.source.rows = async function* () {
      try {
        yield rows[0];
        throw failure;
      } finally {
        closed();
      }
    };
    await expect(
      verifyCurrentDeviceSection(info, state.source, state.environment),
    ).rejects.toBe(failure);
    expect(closed).toHaveBeenCalledTimes(1);
  });

  it("rejects truncated or surplus source rows against the sealed count", async () => {
    const state = fixture();
    state.local.set("safe_plugin_a", "first");
    const info = await captureDeviceSection(
      "local-storage",
      state.source,
      state.environment,
    );
    await expect(
      verifyCurrentDeviceSection(
        { ...info, recordCount: 2 },
        state.source,
        state.environment,
      ),
    ).rejects.toThrow("row count");
    await expect(
      verifyCurrentDeviceSection(
        { ...info, recordCount: 0 },
        state.source,
        state.environment,
      ),
    ).rejects.toThrow("row count");
  });

  it("propagates current storage errors, cancellation and non-unsupported clone failures", async () => {
    const state = fixture();
    const info = await captureDeviceSection(
      "localforage",
      state.source,
      state.environment,
    );
    const storageFailure = new Error("Synthetic storage read failure");
    state.environment.localforage.keys = async () => {
      throw storageFailure;
    };
    await expect(
      verifyCurrentDeviceSection(info, state.source, state.environment),
    ).rejects.toBe(storageFailure);
    const failure = new CloneGraphError("limit", "$", "nodes");
    state.environment.localforage.keys = async () => {
      throw failure;
    };
    await expect(
      verifyCurrentDeviceSection(info, state.source, state.environment),
    ).rejects.toBe(failure);
    const controller = new AbortController();
    controller.abort();
    state.environment.signal = controller.signal;
    await expect(
      verifyCurrentDeviceSection(info, state.source, state.environment),
    ).rejects.toMatchObject({ name: "AbortError" });
  });

  it("rejects invalid source metadata and settings without changing current storage", async () => {
    const state = fixture();
    const info = await captureDeviceSection(
      "device-settings",
      state.source,
      state.environment,
    );
    await expect(
      verifyCurrentDeviceSection(
        {
          ...info,
          metadata: { ...info.metadata, profile: "invalid" as never },
        },
        state.source,
        state.environment,
      ),
    ).rejects.toThrow("metadata");
    const invalid = await encodeCloneGraph(
      '{"schema":"invalid"}',
      state.source.putBinary,
    );
    state.sections.get("device-settings")!.rows.push({
      kind: "value",
      key: encodeUtf16("risuNestDeviceSettings"),
      value: invalid,
    });
    vi.clearAllMocks();
    await expect(
      verifyCurrentDeviceSection(
        {
          ...info,
          recordCount: 1,
          metadata: { ...info.metadata, present: true },
        },
        state.source,
        state.environment,
      ),
    ).rejects.toThrow("invalid");
    state.assertReadOnly();
  });
});
