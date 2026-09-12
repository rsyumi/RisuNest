// @vitest-environment node
import { IDBDatabase, IDBFactory, IDBKeyRange } from "fake-indexeddb";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  capturePluginDatabase,
  cleanupPluginDatabaseStages,
  countPluginDatabaseDeletions,
  enumeratePluginDatabases,
  PluginDatabaseBlockedError,
  restorePluginDatabase,
  stagePluginDatabase,
  type PluginDatabaseOptions,
  type PluginDatabaseRecord,
  type PluginDatabaseSnapshot,
  type PluginDatabaseSource,
  type PluginDatabaseStageOptions,
} from "./indexedDb";

function fixtureOptions(
  factory = new IDBFactory(),
): PluginDatabaseStageOptions {
  return {
    indexedDB: factory,
    keyRange: IDBKeyRange,
    barrier: { assertHeld: vi.fn() },
    batchSize: 2,
    blockedTimeoutMs: 2_000,
    requiredAdditionalBytes: 100,
    storageEstimate: async () => ({ quota: 10_000, usage: 0 }),
    verifyRecord: async (expected, actual) => {
      expect(actual).toEqual(expected);
    },
  };
}

function requestResult<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function createDatabase(
  factory: IDBFactory,
  name: string,
  upgrade: (database: IDBDatabase) => void,
  version = 1,
): Promise<void> {
  const request = factory.open(name, version);
  request.onupgradeneeded = () => upgrade(request.result);
  const db = await requestResult(request);
  db.close();
}

async function change(
  factory: IDBFactory,
  name: string,
  storeName: string,
  mutate: (store: IDBObjectStore) => void,
): Promise<void> {
  const db = await requestResult(factory.open(name));
  try {
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction(storeName, "readwrite");
      tx.oncomplete = () => resolve();
      tx.onabort = () => reject(tx.error);
      mutate(tx.objectStore(storeName));
    });
  } finally {
    db.close();
  }
}

async function nextKey(
  factory: IDBFactory,
  name: string,
  storeName: string,
  value: unknown = {},
): Promise<IDBValidKey> {
  let result!: IDBValidKey;
  await change(factory, name, storeName, (store) => {
    const request = store.add(value);
    request.onsuccess = () => {
      result = request.result;
    };
  });
  return result;
}

async function capture(
  name: string,
  opts: PluginDatabaseOptions,
): Promise<
  PluginDatabaseSource & { rows: Map<string, PluginDatabaseRecord[]> }
> {
  let metadata!: PluginDatabaseSnapshot;
  const rows = new Map<string, PluginDatabaseRecord[]>();
  const result = await capturePluginDatabase(name, opts, {
    metadata: async (value) => {
      metadata = value;
    },
    records: async (storeName, batch) => {
      rows.set(storeName, [...(rows.get(storeName) ?? []), ...batch]);
    },
  });
  expect(result).toEqual(metadata);
  return {
    metadata,
    rows,
    records: async function* (storeName) {
      yield* rows.get(storeName) ?? [];
    },
  };
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("logical plugin IndexedDB backup", () => {
  it("enumerates only the exact plugin prefix and fails when enumeration is unavailable", async () => {
    const opts = fixtureOptions();
    const factory = opts.indexedDB!;
    for (const name of [
      "safe_plugin_z",
      "safe_plugin_a",
      "app",
      "localforage",
      "safe_plugins_bad",
      "__risunest_device_backup_stage_fixture",
    ]) {
      await createDatabase(factory, name, () => {});
    }
    expect(await enumeratePluginDatabases(opts)).toEqual([
      "safe_plugin_a",
      "safe_plugin_z",
    ]);
    Object.defineProperty(factory, "databases", { value: undefined });
    await expect(enumeratePluginDatabases(opts)).rejects.toThrow(
      "enumeration is unavailable",
    );
    await expect(capture("app", opts)).rejects.toThrow(
      "outside plugin storage ownership",
    );
  });

  it("distinguishes absent databases, empty databases, and empty stores", async () => {
    const opts = fixtureOptions();
    const absent = await capture("safe_plugin_absent", opts);
    expect(absent.metadata).toEqual({
      name: "safe_plugin_absent",
      present: false,
    });
    expect(await opts.indexedDB!.databases()).toEqual([]);
    await createDatabase(opts.indexedDB!, "safe_plugin_empty", () => {}, 3);
    expect((await capture("safe_plugin_empty", opts)).metadata).toEqual({
      name: "safe_plugin_empty",
      present: true,
      version: 3,
      stores: [],
    });
    await createDatabase(opts.indexedDB!, "safe_plugin_store", (db) => {
      db.createObjectStore("");
    });
    const source = await capture("safe_plugin_store", opts);
    expect(source.metadata.present && source.metadata.stores[0]).toEqual({
      name: "",
      keyPath: null,
      autoIncrement: false,
      count: 0,
      generator: null,
      indexes: [],
    });
  });

  it("round-trips schema, typed keys, inline/composite keys, and rebuilt indexes", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_complex";
    await createDatabase(
      opts.indexedDB!,
      name,
      (db) => {
        const typed = db.createObjectStore("typed");
        for (const key of [
          7,
          new Date(7),
          "7",
          new Uint8Array([7]).buffer,
          [7, new Date(7), "7"],
        ])
          typed.add({ keyKind: typeof key }, key);
        const inline = db.createObjectStore("inline", { keyPath: "meta.id" });
        inline.createIndex("unique", "code", { unique: true });
        inline.createIndex("tags", "tags", { multiEntry: true });
        inline.createIndex("compound", ["meta.id", "code"]);
        inline.add({
          meta: { id: 4 },
          code: "a",
          tags: ["x", "x", "y", undefined],
        });
        inline.add({ meta: { id: 8 }, code: "b", tags: ["x", "z"] });
        db.createObjectStore("compound", { keyPath: ["id", "date"] }).add({
          id: "id",
          date: new Date(10),
          value: [undefined, null],
        });
        db.createObjectStore("empty");
      },
      4,
    );
    const source = await capture(name, opts);
    const old = await requestResult(opts.indexedDB!.deleteDatabase(name));
    expect(old).toBeUndefined();
    await createDatabase(
      opts.indexedDB!,
      name,
      (db) => {
        db.createObjectStore("obsolete").add("old", "old");
      },
      12,
    );
    await createDatabase(opts.indexedDB!, "app_unselected", (db) => {
      db.createObjectStore("keep").add("keep", 1);
    });
    const stage = await stagePluginDatabase(source, opts);
    expect(stage.temporaryName).toMatch(/^__risunest_device_backup_stage_/);
    expect(
      (await capture(name, opts)).metadata.present &&
        (await capture(name, opts)).rows.has("obsolete"),
    ).toBe(true);
    await stage.apply();
    const restored = await capture(name, opts);
    expect(restored.metadata).toEqual(source.metadata);
    expect(restored.rows).toEqual(source.rows);
    for (let i = 0; i < source.rows.get("typed")!.length; i++) {
      expect(
        opts.indexedDB!.cmp(
          source.rows.get("typed")![i].key,
          restored.rows.get("typed")![i].key,
        ),
      ).toBe(0);
    }
    const db = await requestResult(opts.indexedDB!.open(name));
    const keys = await requestResult(
      db
        .transaction("inline")
        .objectStore("inline")
        .index("tags")
        .getAllKeys("x"),
    );
    expect(keys).toEqual([4, 8]);
    db.close();
    expect(
      (await opts.indexedDB!.databases()).some(
        (entry) => entry.name === "app_unselected",
      ),
    ).toBe(true);
    await stage.cleanup();
    expect(
      (await opts.indexedDB!.databases()).some(
        (entry) => entry.name === stage.temporaryName,
      ),
    ).toBe(false);
  });

  it.each([false, true])(
    "preserves a deleted maximum and empty-store generator (inline=%s)",
    async (inline) => {
      const opts = fixtureOptions();
      const name = "safe_plugin_generator";
      await createDatabase(opts.indexedDB!, name, (db) => {
        const store = db.createObjectStore("records", {
          autoIncrement: true,
          keyPath: inline ? "nested.id" : null,
        });
        store.createIndex("unique", "unique", { unique: true });
        store.createIndex("generated", inline ? "nested.id" : "missing", {
          unique: true,
        });
        if (inline) {
          store.add({ nested: { id: 400 }, unique: "a" });
          store.add({ nested: { id: 3 }, unique: "b" });
        } else {
          store.add({ unique: "a" }, 400);
          store.add({ unique: "b" }, 3);
        }
        store.delete(400);
      });
      const captured = await capture(name, opts);
      expect(
        captured.metadata.present && captured.metadata.stores[0].generator,
      ).toEqual({ next: 401 });
      expect(await nextKey(opts.indexedDB!, name, "records")).toBe(401);
      await restorePluginDatabase(captured, opts);
      expect((await capture(name, opts)).rows.get("records")).toEqual(
        captured.rows.get("records"),
      );
      expect(await nextKey(opts.indexedDB!, name, "records")).toBe(401);
      await change(opts.indexedDB!, name, "records", (store) => {
        store.clear();
      });
      const empty = await capture(name, opts);
      expect(empty.metadata.present && empty.metadata.stores[0]).toMatchObject({
        count: 0,
        generator: { next: 402 },
      });
      await restorePluginDatabase(empty, opts);
      expect(await nextKey(opts.indexedDB!, name, "records")).toBe(402);
    },
  );

  it.each([2 ** 53 - 2, 2 ** 53 - 1, 2 ** 53])(
    "preserves generator state after deleting explicit boundary %s",
    async (boundary) => {
      const opts = fixtureOptions();
      const name = "safe_plugin_boundary";
      await createDatabase(opts.indexedDB!, name, (db) => {
        const store = db.createObjectStore("records", { autoIncrement: true });
        store.add({}, boundary);
        store.delete(boundary);
      });
      const source = await capture(name, opts);
      const expected =
        boundary === 2 ** 53 ? { exhausted: true } : { next: boundary + 1 };
      expect(
        source.metadata.present && source.metadata.stores[0].generator,
      ).toEqual(expected);
      // A second capture must observe the same state, proving that the first probe aborted.
      expect((await capture(name, opts)).metadata).toEqual(source.metadata);
      await restorePluginDatabase(source, opts);
      expect((await capture(name, opts)).metadata).toEqual(source.metadata);
      if (boundary === 2 ** 53)
        await expect(
          nextKey(opts.indexedDB!, name, "records"),
        ).rejects.toMatchObject({ name: "ConstraintError" });
      else
        expect(await nextKey(opts.indexedDB!, name, "records")).toBe(
          boundary + 1,
        );
    },
  );

  it("handles a compound unique index on the generated key without changing its existing row", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_constant_index";
    await createDatabase(opts.indexedDB!, name, (db) => {
      const store = db.createObjectStore("records", {
        keyPath: "nested.id",
        autoIncrement: true,
      });
      store.createIndex("one", ["nested.id"], { unique: true });
      store.add({ nested: { id: 90 }, body: "temporary" });
      store.delete(90);
      store.add({ nested: { id: 1 }, body: "survives" });
    });
    const source = await capture(name, opts);
    expect(
      source.metadata.present && source.metadata.stores[0].generator,
    ).toEqual({ next: 91 });
    expect((await capture(name, opts)).rows).toEqual(source.rows);
    await restorePluginDatabase(source, opts);
    expect((await capture(name, opts)).metadata).toEqual(source.metadata);
    expect((await capture(name, opts)).rows).toEqual(source.rows);
  });

  it("finishes every transaction before a slow sink or verifier runs", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_slow";
    await createDatabase(opts.indexedDB!, name, (db) => {
      const store = db.createObjectStore("records", { autoIncrement: true });
      for (let i = 0; i < 11; i++) store.add({ body: i });
    });
    let active = 0;
    const real = IDBDatabase.prototype.transaction;
    vi.spyOn(IDBDatabase.prototype, "transaction").mockImplementation(function (
      this: IDBDatabase,
      ...args
    ) {
      const tx = real.apply(this, args);
      active++;
      tx.addEventListener("complete", () => {
        active--;
      });
      tx.addEventListener("abort", () => {
        active--;
      });
      return tx;
    });
    const batches: number[] = [];
    await capturePluginDatabase(name, opts, {
      metadata: async () => {
        expect(active).toBe(0);
      },
      records: async (_name, records) => {
        expect(active).toBe(0);
        batches.push(records.length);
        await new Promise((resolve) => setTimeout(resolve, 2));
        expect(active).toBe(0);
      },
    });
    expect(batches).toEqual([2, 2, 2, 2, 2, 1]);
    const source = await capture(name, opts);
    opts.verifyRecord = async (expected, actual) => {
      expect(active).toBe(0);
      await new Promise((resolve) => setTimeout(resolve, 1));
      expect(active).toBe(0);
      expect(actual).toEqual(expected);
    };
    const stage = await stagePluginDatabase(source, opts);
    await stage.cleanup();
    expect(active).toBe(0);
  });

  it("checks quota before staging and keeps the live database unchanged when decode or verification fails", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_stage_failure";
    await createDatabase(opts.indexedDB!, name, (db) => {
      db.createObjectStore("records").add({ body: "live" }, 1);
    });
    const source = await capture(name, opts);
    await expect(
      stagePluginDatabase(source, {
        ...opts,
        storageEstimate: async () => ({ quota: 50, usage: 0 }),
      }),
    ).rejects.toThrow("quota");
    await expect(
      stagePluginDatabase(source, {
        ...opts,
        verifyRecord: async () => {
          throw new Error("clone mismatch");
        },
      }),
    ).rejects.toThrow("clone mismatch");
    const broken: PluginDatabaseSource = {
      metadata: source.metadata,
      records: async function* () {
        throw new Error("decode failed");
      },
    };
    await expect(stagePluginDatabase(broken, opts)).rejects.toThrow(
      "decode failed",
    );
    expect((await capture(name, opts)).rows).toEqual(source.rows);
    expect(
      (await opts.indexedDB!.databases()).map((entry) => entry.name),
    ).toEqual([name]);
  });

  it("restores absent rollback by deleting, and restores present-empty by leaving an empty database", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_absent_rollback";
    const source = await capture(name, opts);
    await createDatabase(opts.indexedDB!, name, (db) => {
      db.createObjectStore("new").add("new", 1);
    });
    const stage = await stagePluginDatabase(source, opts);
    expect(stage.temporaryName).toBeNull();
    await stage.apply();
    expect(await opts.indexedDB!.databases()).toEqual([]);
    const empty: PluginDatabaseSource = {
      metadata: { name, present: true, version: 2, stores: [] },
      records: async function* () {},
    };
    await restorePluginDatabase(empty, opts);
    expect((await capture(name, opts)).metadata).toEqual(empty.metadata);
  });

  it("rejects an inline primary-key mismatch and a missing inline key rather than injecting archived keys", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_bad_inline";
    await createDatabase(opts.indexedDB!, name, (db) => {
      db.createObjectStore("records", {
        keyPath: "id",
        autoIncrement: true,
      }).add({ id: 1 });
    });
    const original = await capture(name, opts);
    for (const value of [{ id: 2 }, {}]) {
      const source = {
        metadata: original.metadata,
        records: async function* () {
          yield { key: 1, value };
        },
      };
      await expect(stagePluginDatabase(source, opts)).rejects.toThrow(
        /primary key|Inline primary key/,
      );
    }
    expect((await capture(name, opts)).rows).toEqual(original.rows);
  });

  it("rejects duplicate unique values, out-of-order keys, and a generator that would need lowering", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_invalid";
    await createDatabase(opts.indexedDB!, name, (db) => {
      const store = db.createObjectStore("records", { autoIncrement: true });
      store.createIndex("unique", "code", { unique: true });
      store.add({ code: "a" }, 1);
      store.add({ code: "b" }, 2);
    });
    const original = await capture(name, opts);
    const duplicate = {
      metadata: original.metadata,
      records: async function* () {
        yield { key: 1, value: { code: "same" } };
        yield { key: 2, value: { code: "same" } };
      },
    };
    await expect(stagePluginDatabase(duplicate, opts)).rejects.toMatchObject({
      name: "ConstraintError",
    });
    const reversed = {
      metadata: original.metadata,
      records: async function* () {
        yield* [...original.rows.get("records")!].reverse();
      },
    };
    await expect(stagePluginDatabase(reversed, opts)).rejects.toThrow(
      "out of order",
    );
    const lowered = structuredClone(original.metadata);
    if (lowered.present) lowered.stores[0].generator = { next: 2 };
    await expect(
      stagePluginDatabase({ ...original, metadata: lowered }, opts),
    ).rejects.toThrow("lowering");
    expect((await capture(name, opts)).rows).toEqual(original.rows);
  });

  it("detects changed counts and a lost maintenance barrier between batches", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_changed";
    await createDatabase(opts.indexedDB!, name, (db) => {
      const store = db.createObjectStore("records");
      store.add("one", 1);
      store.add("two", 2);
    });
    let wrote = false;
    await expect(
      capturePluginDatabase(name, opts, {
        metadata: async () => {},
        records: async () => {
          if (!wrote) {
            wrote = true;
            await change(opts.indexedDB!, name, "records", (store) => {
              store.add("three", 3);
            });
          }
        },
      }),
    ).rejects.toThrow("changed during capture");
    let held = true;
    await expect(
      capturePluginDatabase(
        name,
        {
          ...opts,
          barrier: {
            assertHeld: () => {
              if (!held) throw new Error("fence lost");
            },
          },
        },
        {
          metadata: async () => {},
          records: async () => {
            held = false;
          },
        },
      ),
    ).rejects.toThrow("fence lost");
    expect((await capture(name, opts)).rows.get("records")).toHaveLength(3);
  });

  it("cancels after a completed capture batch without changing original records or generator", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_cancel";
    await createDatabase(opts.indexedDB!, name, (db) => {
      const store = db.createObjectStore("records", { autoIncrement: true });
      store.add("one");
      store.add("two");
    });
    const before = await capture(name, opts);
    const controller = new AbortController();
    await expect(
      capturePluginDatabase(
        name,
        { ...opts, signal: controller.signal },
        {
          metadata: async () => {},
          records: async () => {
            controller.abort();
          },
        },
      ),
    ).rejects.toMatchObject({ name: "AbortError" });
    expect((await capture(name, opts)).metadata).toEqual(before.metadata);
    expect((await capture(name, opts)).rows).toEqual(before.rows);
  });

  it("bounds blocked deletion and exposes the still-pending operation for fenced recovery", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_blocked";
    await createDatabase(opts.indexedDB!, name, (db) => {
      db.createObjectStore("records").add("old", 1);
    });
    const before = await capture(name, opts);
    const blocker = await requestResult(opts.indexedDB!.open(name));
    const pending = restorePluginDatabase(before, {
      ...opts,
      blockedTimeoutMs: 20,
    });
    const error = await pending.catch((error: unknown) => error);
    expect(error).toBeInstanceOf(PluginDatabaseBlockedError);
    expect((error as PluginDatabaseBlockedError).operation).toBe("delete");
    // The database still exists at timeout. Once the connection closes the uncancellable delete runs.
    expect(
      (await opts.indexedDB!.databases()).some((entry) => entry.name === name),
    ).toBe(true);
    blocker.close();
    await (error as PluginDatabaseBlockedError).pendingOperation;
    expect(await opts.indexedDB!.databases()).toEqual([]);
    await restorePluginDatabase(before, opts);
    expect((await capture(name, opts)).rows).toEqual(before.rows);
  });

  it("counts removed primary keys across typed keys and stores without reading live values", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_deletions";
    await createDatabase(opts.indexedDB!, name, (db) => {
      const store = db.createObjectStore("records");
      for (const key of [1, 2, new Date(1), "1", [1]])
        store.add({ body: "source" }, key);
    });
    const source = await capture(name, opts);
    await requestResult(opts.indexedDB!.deleteDatabase(name));
    await createDatabase(opts.indexedDB!, name, (db) => {
      const store = db.createObjectStore("records");
      for (const key of [1, 3, new Date(1), new Date(2), "1", "2", [1], [2]])
        store.add({ body: "current" }, key);
      const removed = db.createObjectStore("removed");
      removed.add("a", 1);
      removed.add("b", 2);
    });
    // Existing 3, Date(2), "2", [2], and the two removed-store records disappear.
    expect(await countPluginDatabaseDeletions(source, opts)).toBe(6);
    expect(
      await countPluginDatabaseDeletions(
        { metadata: { name, present: false }, records: async function* () {} },
        opts,
      ),
    ).toBe(10);
    expect(
      await countPluginDatabaseDeletions(
        {
          metadata: { name: "safe_plugin_missing", present: false },
          records: async function* () {},
        },
        opts,
      ),
    ).toBe(0);
    expect(
      (await opts.indexedDB!.databases()).map((entry) => entry.name),
    ).toEqual([name]);
  });

  it("cleans only abandoned database names in the reserved stage UUID namespace", async () => {
    const opts = fixtureOptions();
    const abandoned = `__risunest_device_backup_stage_${crypto.randomUUID()}`;
    const preserved = [
      "safe_plugin_live",
      "other",
      "__risunest_device_backup_stage_not-a-uuid",
      "__risunest_device_backup_stage_00000000-0000-0000-0000-000000000000",
    ];
    for (const name of [abandoned, ...preserved])
      await createDatabase(opts.indexedDB!, name, () => {});
    expect(await cleanupPluginDatabaseStages(opts)).toBe(1);
    expect(
      (await opts.indexedDB!.databases()).map((entry) => entry.name).sort(),
    ).toEqual(preserved.sort());
  });

  it("cleans a cancelled stage while retaining the maintenance fence", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_cancel_stage";
    await createDatabase(opts.indexedDB!, name, (db) => {
      db.createObjectStore("records").add("keep", 1);
    });
    const source = await capture(name, opts);
    const controller = new AbortController();
    const interrupted = {
      metadata: source.metadata,
      records: async function* (storeName: string) {
        controller.abort();
        yield* source.rows.get(storeName)!;
      },
    };
    await expect(
      stagePluginDatabase(interrupted, { ...opts, signal: controller.signal }),
    ).rejects.toMatchObject({ name: "AbortError" });
    expect(
      (await opts.indexedDB!.databases()).map((entry) => entry.name),
    ).toEqual([name]);
    expect((await capture(name, opts)).rows).toEqual(source.rows);
  });

  it("preserves clone graph identity and binary views while replaying an asynchronous record source", async () => {
    const opts = fixtureOptions();
    const name = "safe_plugin_graph";
    const buffer = new ArrayBuffer(128 * 1024);
    new Uint8Array(buffer).set([9, 8, 7, 6], 24);
    const shared = { body: "\ud800", number: -0, missing: undefined };
    const value: Record<string, unknown> = {
      first: shared,
      second: shared,
      buffer,
      bytes: new Uint8Array(buffer, 24, 4),
      view: new DataView(buffer, 24, 4),
      map: new Map([[shared, shared]]),
      set: new Set([shared]),
      sparse: new Array(4),
      bigint: 9007199254740993n,
      date: new Date(42),
    };
    value.self = value;
    (value.sparse as unknown[])[2] = undefined;
    await createDatabase(opts.indexedDB!, name, (db) => {
      db.createObjectStore("records").add(value, "key");
    });
    const source = await capture(name, opts);
    const delayed = {
      metadata: source.metadata,
      records: async function* (storeName: string) {
        await new Promise((resolve) => setTimeout(resolve, 2));
        yield* source.rows.get(storeName)!;
        await new Promise((resolve) => setTimeout(resolve, 2));
      },
    };
    const stage = await stagePluginDatabase(delayed, opts);
    await stage.apply();
    const restored = (await capture(name, opts)).rows.get("records")![0]
      .value as typeof value;
    expect(restored.self).toBe(restored);
    expect(restored.first).toBe(restored.second);
    expect((restored.bytes as Uint8Array).buffer).toBe(restored.buffer);
    expect((restored.view as DataView).buffer).toBe(restored.buffer);
    expect([...(restored.map as Map<unknown, unknown>).keys()][0]).toBe(
      restored.first,
    );
    expect(Object.hasOwn(restored.sparse as object, 0)).toBe(false);
    expect(Object.hasOwn(restored.sparse as object, 2)).toBe(true);
    expect(restored).toEqual(value);
    await stage.cleanup();
  });
});
