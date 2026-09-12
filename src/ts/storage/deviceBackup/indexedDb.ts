/** Logical plugin IndexedDB backup. Call only from the isolated maintenance bootstrap. */
export interface PluginDatabaseBarrier {
  /** Throw unless all same-origin writers are stopped and the native job still owns its fence. */
  assertHeld(): void;
}

export type PluginKeyGenerator = { next: number } | { exhausted: true };
export interface PluginDatabaseIndex {
  name: string;
  keyPath: string | string[];
  unique: boolean;
  multiEntry: boolean;
}
export interface PluginDatabaseStore {
  name: string;
  keyPath: string | string[] | null;
  autoIncrement: boolean;
  indexes: PluginDatabaseIndex[];
  count: number;
  generator: PluginKeyGenerator | null;
}
export type PluginDatabaseSnapshot =
  | { name: string; present: false }
  | {
      name: string;
      present: true;
      version: number;
      stores: PluginDatabaseStore[];
    };
export interface PluginDatabaseRecord {
  key: IDBValidKey;
  value: unknown;
}
export interface PluginDatabaseOptions {
  barrier: PluginDatabaseBarrier;
  indexedDB?: IDBFactory;
  keyRange?: typeof IDBKeyRange;
  batchSize?: number;
  blockedTimeoutMs?: number;
  signal?: AbortSignal;
}
export interface PluginDatabaseSink {
  metadata(metadata: PluginDatabaseSnapshot): Promise<void>;
  records(storeName: string, records: PluginDatabaseRecord[]): Promise<void>;
}
export interface PluginDatabaseSource {
  metadata: PluginDatabaseSnapshot;
  /** Each call must replay the validated, immutable spool in ascending primary-key order. */
  records(storeName: string): AsyncIterable<PluginDatabaseRecord>;
}
export interface PluginDatabaseRestoreOptions extends PluginDatabaseOptions {
  /** Compare the complete clone graph, including binary bytes. Throw on any difference. */
  verifyRecord(
    expected: PluginDatabaseRecord,
    actual: PluginDatabaseRecord,
    context: { databaseName: string; storeName: string },
  ): Promise<void>;
}
export interface PluginDatabaseStageOptions extends PluginDatabaseRestoreOptions {
  /** Caller includes staging, rollback, and final-copy headroom in this estimate. */
  requiredAdditionalBytes: number;
  storageEstimate?: () => Promise<{ quota?: number; usage?: number }>;
}
export interface StagedPluginDatabase {
  metadata: PluginDatabaseSnapshot;
  temporaryName: string | null;
  /** Caller must durably preserve rollback and write apply intent before calling this. */
  apply(): Promise<void>;
  cleanup(): Promise<void>;
}

/** A timed-out delete is still queued by IndexedDB and cannot be cancelled. */
export class PluginDatabaseBlockedError extends Error {
  constructor(
    public readonly operation: "open" | "delete",
    /** Keep maintenance closed until this settles, then restore/verify before normal bootstrap. */
    public readonly pendingOperation: Promise<void>,
    public readonly databaseName: string,
  ) {
    super(`Plugin database ${operation} did not settle before its timeout`);
    this.name = "PluginDatabaseBlockedError";
  }
}

const PREFIX = "safe_plugin_";
const STAGE_PREFIX = "__risunest_device_backup_stage_";
const LAST_GENERATED_KEY = 2 ** 53;
type Options = PluginDatabaseOptions & {
  indexedDB: IDBFactory;
  keyRange: typeof IDBKeyRange;
  batchSize: number;
  blockedTimeoutMs: number;
};

function options(input: PluginDatabaseOptions): Options {
  const result = {
    ...input,
    indexedDB: input.indexedDB ?? globalThis.indexedDB,
    keyRange: input.keyRange ?? globalThis.IDBKeyRange,
    batchSize: input.batchSize ?? 64,
    blockedTimeoutMs: input.blockedTimeoutMs ?? 5_000,
  };
  if (
    !result.indexedDB ||
    !result.keyRange ||
    !Number.isSafeInteger(result.batchSize) ||
    result.batchSize < 1 ||
    result.batchSize > 4096 ||
    !Number.isFinite(result.blockedTimeoutMs) ||
    result.blockedTimeoutMs < 1
  )
    throw new Error("Invalid plugin database backup options");
  check(result);
  return result;
}

function check(opts: PluginDatabaseOptions): void {
  opts.barrier.assertHeld();
  if (opts.signal?.aborted)
    throw new DOMException("Plugin database backup cancelled", "AbortError");
}

function pluginName(name: string): void {
  if (typeof name !== "string" || !name.startsWith(PREFIX)) {
    throw new Error("Database is outside plugin storage ownership");
  }
}

export async function enumeratePluginDatabases(
  input: PluginDatabaseOptions,
): Promise<string[]> {
  const opts = options(input);
  if (typeof opts.indexedDB.databases !== "function") {
    throw new Error("Plugin database enumeration is unavailable");
  }
  const databases = await opts.indexedDB.databases();
  check(opts);
  return databases
    .flatMap(({ name }) =>
      typeof name === "string" && name.startsWith(PREFIX) ? [name] : [],
    )
    .sort();
}

/** Only run at isolated maintenance startup, before this job creates any staging databases. */
export async function cleanupPluginDatabaseStages(
  input: PluginDatabaseOptions,
): Promise<number> {
  const opts = options(input);
  if (typeof opts.indexedDB.databases !== "function")
    throw new Error("Plugin database enumeration is unavailable");
  const databases = await opts.indexedDB.databases();
  const ownedStage = new RegExp(
    `^${STAGE_PREFIX}[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$`,
  );
  let deleted = 0;
  for (const { name } of databases) {
    check(opts);
    if (typeof name === "string" && ownedStage.test(name)) {
      await deleteDatabase(name, opts);
      deleted++;
    }
  }
  return deleted;
}

/** Attach completion before enqueuing requests. No caller/IPC promise runs inside this transaction. */
function transaction<T>(
  db: IDBDatabase,
  storeName: string,
  mode: IDBTransactionMode,
  opts: Options,
  enqueue: (
    store: IDBObjectStore,
    guard: (fn: () => void) => () => void,
  ) => () => T,
): Promise<T> {
  check(opts);
  return new Promise((resolve, reject) => {
    const tx = db.transaction(storeName, mode);
    let failure: unknown;
    let result: () => T;
    const guard = (fn: () => void) => () => {
      try {
        check(opts);
        fn();
      } catch (error) {
        failure = error;
        tx.abort();
      }
    };
    tx.oncomplete = () => {
      try {
        check(opts);
        resolve(result());
      } catch (error) {
        reject(error);
      }
    };
    tx.onabort = () =>
      reject(
        failure ?? tx.error ?? new Error("Plugin database transaction aborted"),
      );
    guard(() => {
      result = enqueue(tx.objectStore(storeName), guard);
    })();
  });
}

/** Open-existing aborts the initial upgrade if the database is absent, leaving no empty DB. */
function openDatabase(
  name: string,
  opts: Options,
  create?: { version: number; stores: PluginDatabaseStore[] },
): Promise<IDBDatabase | null> {
  check(opts);
  return new Promise((resolve, reject) => {
    const request = create
      ? opts.indexedDB.open(name, create.version)
      : opts.indexedDB.open(name);
    let absent = false;
    let expired = false;
    let created = false;
    let failure: unknown;
    let settle!: () => void;
    const pending = new Promise<void>((done) => {
      settle = done;
    });
    const timer = setTimeout(() => {
      expired = true;
      reject(new PluginDatabaseBlockedError("open", pending, name));
    }, opts.blockedTimeoutMs);
    const finish = () => {
      clearTimeout(timer);
      settle();
    };
    request.onupgradeneeded = (event) => {
      try {
        check(opts);
        if (expired || !create || event.oldVersion !== 0) {
          absent = !create && event.oldVersion === 0;
          if (create && !expired)
            failure = new Error("Staging database already exists");
          request.transaction!.abort();
          return;
        }
        created = true;
        for (const schema of create.stores) {
          const store = request.result.createObjectStore(schema.name, {
            keyPath: schema.keyPath,
            autoIncrement: schema.autoIncrement,
          });
          for (const index of schema.indexes)
            store.createIndex(index.name, index.keyPath, index);
        }
      } catch (error) {
        failure = error;
        request.transaction!.abort();
      }
    };
    request.onerror = () => {
      finish();
      if (absent && !failure) resolve(null);
      else reject(failure ?? request.error);
    };
    request.onsuccess = () => {
      finish();
      if (expired) {
        request.result.close();
        return;
      }
      try {
        check(opts);
        const db = request.result;
        if (create && !created)
          throw new Error("Staging database already exists");
        db.onversionchange = () => db.close();
        resolve(db);
      } catch (error) {
        request.result.close();
        reject(error);
      }
    };
  });
}

function deleteDatabase(name: string, opts: Options): Promise<void> {
  check(opts);
  const request = opts.indexedDB.deleteDatabase(name);
  const pending = new Promise<void>((resolve, reject) => {
    request.onsuccess = () => resolve();
    request.onerror = () => reject(request.error);
  });
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new PluginDatabaseBlockedError("delete", pending, name)),
      opts.blockedTimeoutMs,
    );
    pending.then(
      () => {
        clearTimeout(timer);
        try {
          check(opts);
          resolve();
        } catch (error) {
          reject(error);
        }
      },
      (error) => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}

function probeValue(keyPath: string | string[] | null, key?: number): object {
  const root = Object.create(null) as Record<string, unknown>;
  if (keyPath !== null) {
    if (typeof keyPath !== "string" || keyPath === "")
      throw new Error("Unsupported automatic key path");
    const parts = keyPath.split(".");
    let parent = root;
    for (const part of parts.slice(0, -1)) {
      const child = Object.create(null) as Record<string, unknown>;
      Object.defineProperty(parent, part, {
        value: child,
        enumerable: true,
        writable: true,
        configurable: true,
      });
      parent = child;
    }
    if (key !== undefined)
      Object.defineProperty(parent, parts.at(-1)!, {
        value: key,
        enumerable: true,
        writable: true,
        configurable: true,
      });
  }
  return root;
}

/** Observe the generator only after an intentional abort has completed. Never commits a probe. */
function generator(
  db: IDBDatabase,
  schema: PluginDatabaseStore,
  opts: Options,
): Promise<PluginKeyGenerator> {
  check(opts);
  return new Promise((resolve, reject) => {
    const tx = db.transaction(schema.name, "readwrite");
    const store = tx.objectStore(schema.name);
    let observation: PluginKeyGenerator | undefined;
    let failure: unknown;
    const guard = (fn: () => void) => () => {
      try {
        check(opts);
        fn();
      } catch (error) {
        failure = error;
        tx.abort();
      }
    };
    tx.oncomplete = () =>
      reject(new Error("Automatic key probe unexpectedly committed"));
    tx.onabort = () => {
      if (failure || !observation)
        reject(failure ?? tx.error ?? new Error("Automatic key probe failed"));
      else {
        try {
          check(opts);
          resolve(observation);
        } catch (error) {
          reject(error);
        }
      }
    };
    guard(() => {
      const request = store.add(probeValue(store.keyPath));
      request.onsuccess = guard(() => {
        const next = request.result;
        if (
          typeof next !== "number" ||
          !Number.isInteger(next) ||
          next < 1 ||
          next > LAST_GENERATED_KEY
        ) {
          throw new Error("Automatic key probe returned an unsupported key");
        }
        observation = { next };
        tx.abort();
      });
      request.onerror = (event) => {
        event.preventDefault();
        event.stopPropagation();
        // The minimal value has no indexed keys other than its newly generated key.
        // Unique indexes cannot collide, so ConstraintError means exhaustion.
        if (request.error?.name === "ConstraintError")
          observation = { exhausted: true };
        else failure = request.error;
        tx.abort();
      };
    })();
  });
}

async function metadata(
  db: IDBDatabase,
  name: string,
  opts: Options,
): Promise<PluginDatabaseSnapshot & { present: true }> {
  const stores: PluginDatabaseStore[] = [];
  for (const name of Array.from(db.objectStoreNames)) {
    const schema = await transaction(db, name, "readonly", opts, (store) => {
      const result: PluginDatabaseStore = {
        name,
        keyPath: store.keyPath,
        autoIncrement: store.autoIncrement,
        count: 0,
        generator: null,
        indexes: Array.from(store.indexNames, (indexName) => {
          const index = store.index(indexName);
          return {
            name: index.name,
            keyPath: index.keyPath,
            unique: index.unique,
            multiEntry: index.multiEntry,
          };
        }),
      };
      store.count().onsuccess = (event) => {
        result.count = (event.target as IDBRequest<number>).result;
      };
      return () => result;
    });
    if (schema.autoIncrement)
      schema.generator = await generator(db, schema, opts);
    stores.push(schema);
  }
  const result = { name, present: true as const, version: db.version, stores };
  validateSnapshot(result);
  return result;
}

function readBatch(
  db: IDBDatabase,
  storeName: string,
  after: IDBValidKey | undefined,
  opts: Options,
): Promise<PluginDatabaseRecord[]> {
  return transaction(db, storeName, "readonly", opts, (store, guard) => {
    const records: PluginDatabaseRecord[] = [];
    const request = store.openCursor(
      after === undefined ? undefined : opts.keyRange.lowerBound(after, true),
    );
    request.onsuccess = guard(() => {
      const cursor = request.result;
      if (!cursor) return;
      records.push({ key: cursor.primaryKey, value: cursor.value });
      if (records.length < opts.batchSize) cursor.continue();
    });
    return () => records;
  });
}

async function* primaryKeys(
  db: IDBDatabase,
  name: string,
  opts: Options,
): AsyncGenerator<IDBValidKey> {
  let after: IDBValidKey | undefined;
  while (true) {
    const keys = await transaction(
      db,
      name,
      "readonly",
      opts,
      (store, guard) => {
        const keys: IDBValidKey[] = [];
        const request = store.openKeyCursor(
          after === undefined
            ? undefined
            : opts.keyRange.lowerBound(after, true),
        );
        request.onsuccess = guard(() => {
          const cursor = request.result;
          if (!cursor) return;
          keys.push(cursor.primaryKey);
          if (keys.length < opts.batchSize) cursor.continue();
        });
        return () => keys;
      },
    );
    if (!keys.length) return;
    after = structuredClone(keys.at(-1)!);
    yield* keys;
  }
}

/** Count keys actually removed by replacement, including every key in a removed store.
 * Values are decoded only by the replayable source; live records are read with key-only cursors.
 */
export async function countPluginDatabaseDeletions(
  source: PluginDatabaseSource,
  input: PluginDatabaseOptions,
): Promise<number> {
  validateSnapshot(source.metadata);
  const opts = options(input);
  const db = await openDatabase(source.metadata.name, opts);
  if (!db) return 0;
  const incomingStores = new Set(
    source.metadata.present
      ? source.metadata.stores.map((store) => store.name)
      : [],
  );
  let deleted = 0;
  try {
    for (const name of Array.from(db.objectStoreNames)) {
      if (!incomingStores.has(name)) {
        deleted += await transaction(db, name, "readonly", opts, (store) => {
          const count = store.count();
          return () => count.result;
        });
        continue;
      }
      const iterator = source.records(name)[Symbol.asyncIterator]();
      let previous: IDBValidKey | undefined;
      const advance = async () => {
        const record = await iterator.next();
        check(opts);
        if (!record.done) {
          compareKeys(record.value.key, record.value.key, opts);
          if (
            previous !== undefined &&
            compareKeys(previous, record.value.key, opts) >= 0
          )
            throw new Error("Archive primary keys are out of order");
          previous = structuredClone(record.value.key);
        }
        return record;
      };
      try {
        let incoming = await advance();
        for await (const current of primaryKeys(db, name, opts)) {
          while (
            !incoming.done &&
            compareKeys(incoming.value.key, current, opts) < 0
          )
            incoming = await advance();
          if (
            incoming.done ||
            compareKeys(incoming.value.key, current, opts) !== 0
          )
            deleted++;
        }
      } finally {
        await iterator.return?.();
      }
    }
    check(opts);
    return deleted;
  } finally {
    db.close();
  }
}

export async function capturePluginDatabase(
  name: string,
  input: PluginDatabaseOptions,
  sink: PluginDatabaseSink,
): Promise<PluginDatabaseSnapshot> {
  pluginName(name);
  const opts = options(input);
  const db = await openDatabase(name, opts);
  if (!db) {
    const absent = { name, present: false as const };
    await sink.metadata(absent);
    check(opts);
    return absent;
  }
  try {
    const snapshot = await metadata(db, name, opts);
    const signature = JSON.stringify(snapshot);
    await sink.metadata(structuredClone(snapshot));
    for (const store of snapshot.stores) {
      let after: IDBValidKey | undefined;
      let count = 0;
      while (true) {
        const records = await readBatch(db, store.name, after, opts);
        if (!records.length) break;
        // Remember a private copy: a sink must not be able to alter pagination by mutating a key.
        after = structuredClone(records.at(-1)!.key);
        count += records.length;
        await sink.records(store.name, records);
        check(opts);
      }
      if (count !== store.count)
        throw new Error("Plugin database changed during capture");
    }
    if (JSON.stringify(await metadata(db, name, opts)) !== signature)
      throw new Error("Plugin database changed during capture");
    return snapshot;
  } finally {
    db.close();
  }
}

function keyPathValid(value: unknown, nullable: boolean): boolean {
  return (
    (nullable && value === null) ||
    typeof value === "string" ||
    (Array.isArray(value) &&
      value.length > 0 &&
      value.every((part) => typeof part === "string"))
  );
}

function validateSnapshot(snapshot: PluginDatabaseSnapshot): void {
  if (!snapshot || typeof snapshot !== "object")
    throw new Error("Invalid plugin database snapshot");
  pluginName(snapshot.name);
  if (snapshot.present === false) return;
  if (
    snapshot.present !== true ||
    !Number.isSafeInteger(snapshot.version) ||
    snapshot.version < 1 ||
    !Array.isArray(snapshot.stores)
  )
    throw new Error("Invalid plugin database schema");
  const stores = new Set<string>();
  for (const store of snapshot.stores) {
    if (
      !store ||
      typeof store.name !== "string" ||
      stores.has(store.name) ||
      !keyPathValid(store.keyPath, true) ||
      typeof store.autoIncrement !== "boolean" ||
      !Number.isSafeInteger(store.count) ||
      store.count < 0 ||
      !Array.isArray(store.indexes)
    )
      throw new Error("Invalid plugin object store schema");
    stores.add(store.name);
    if (store.autoIncrement) {
      const state = store.generator;
      if (
        !state ||
        typeof state !== "object" ||
        ((!("exhausted" in state) ||
          state.exhausted !== true ||
          "next" in state) &&
          (!("next" in state) ||
            !Number.isInteger(state.next) ||
            state.next < 1 ||
            state.next > LAST_GENERATED_KEY ||
            "exhausted" in state))
      )
        throw new Error("Invalid automatic key generator state");
      if (Array.isArray(store.keyPath) || store.keyPath === "")
        throw new Error("Invalid automatic key path");
    } else if (store.generator !== null)
      throw new Error("Unexpected automatic key generator state");
    const indexes = new Set<string>();
    for (const index of store.indexes) {
      if (
        !index ||
        typeof index.name !== "string" ||
        indexes.has(index.name) ||
        !keyPathValid(index.keyPath, false) ||
        typeof index.unique !== "boolean" ||
        typeof index.multiEntry !== "boolean" ||
        (index.multiEntry && Array.isArray(index.keyPath))
      )
        throw new Error("Invalid plugin index schema");
      indexes.add(index.name);
    }
  }
}

function extractPath(value: unknown, path: string): unknown {
  if (path === "") return value;
  for (const part of path.split(".")) {
    if (typeof value === "string" && part === "length") value = value.length;
    else if (
      typeof Blob !== "undefined" &&
      value instanceof Blob &&
      (part === "size" || part === "type")
    )
      value = value[part];
    else if (
      typeof File !== "undefined" &&
      value instanceof File &&
      (part === "name" || part === "lastModified")
    )
      value = value[part];
    else if (
      value !== null &&
      typeof value === "object" &&
      Object.hasOwn(value, part)
    )
      value = (value as Record<string, unknown>)[part];
    else return undefined;
  }
  return value;
}

function extractKey(value: unknown, path: string | string[]): unknown {
  return Array.isArray(path)
    ? path.map((part) => extractPath(value, part))
    : extractPath(value, path);
}

function compareKeys(first: unknown, second: unknown, opts: Options): number {
  try {
    return opts.indexedDB.cmp(first as IDBValidKey, second as IDBValidKey);
  } catch {
    throw new Error("Invalid plugin database primary key");
  }
}

function writeBatch(
  db: IDBDatabase,
  schema: PluginDatabaseStore,
  records: PluginDatabaseRecord[],
  opts: Options,
): Promise<void> {
  return transaction(db, schema.name, "readwrite", opts, (store, guard) => {
    for (const record of records) {
      compareKeys(record.key, record.key, opts);
      if (
        schema.keyPath !== null &&
        compareKeys(
          extractKey(record.value, schema.keyPath),
          record.key,
          opts,
        ) !== 0
      )
        throw new Error("Inline primary key differs from the archived key");
      const request =
        schema.keyPath === null
          ? store.add(record.value, record.key)
          : store.add(record.value);
      request.onsuccess = guard(() => {
        if (compareKeys(request.result, record.key, opts) !== 0)
          throw new Error("Restored primary key differs from the archived key");
      });
    }
    return () => undefined;
  });
}

async function restoreGenerator(
  db: IDBDatabase,
  schema: PluginDatabaseStore,
  opts: Options,
): Promise<void> {
  if (!schema.autoIncrement) return;
  const expected = schema.generator!;
  const actual = await generator(db, schema, opts);
  if (JSON.stringify(expected) === JSON.stringify(actual)) return;
  if (
    "exhausted" in actual ||
    ("next" in expected && actual.next > expected.next)
  )
    throw new Error(
      "Restoring this automatic key generator would require lowering it",
    );
  const boundary =
    "exhausted" in expected ? LAST_GENERATED_KEY : expected.next - 1;
  await transaction(db, schema.name, "readwrite", opts, (store, guard) => {
    const exists = store.count(boundary);
    exists.onsuccess = guard(() => {
      if (exists.result !== 0)
        throw new Error("Automatic key boundary record already exists");
      const value = probeValue(schema.keyPath, boundary);
      const request =
        schema.keyPath === null ? store.add(value, boundary) : store.add(value);
      request.onsuccess = guard(() => {
        store.delete(boundary);
      });
    });
    return () => undefined;
  });
  if (
    JSON.stringify(expected) !==
    JSON.stringify(await generator(db, schema, opts))
  )
    throw new Error("Automatic key generator verification failed");
}

function indexKeys(
  value: unknown,
  index: PluginDatabaseIndex,
  opts: Options,
): IDBValidKey[] {
  const key = extractKey(value, index.keyPath);
  const keys: IDBValidKey[] = [];
  for (const candidate of index.multiEntry && Array.isArray(key)
    ? key
    : [key]) {
    try {
      opts.indexedDB.cmp(candidate as IDBValidKey, candidate as IDBValidKey);
      if (
        !keys.some(
          (previous) =>
            opts.indexedDB.cmp(previous, candidate as IDBValidKey) === 0,
        )
      )
        keys.push(candidate as IDBValidKey);
    } catch {
      /* Invalid index keys are omitted by IndexedDB itself. */
    }
  }
  return keys;
}

function verifyIndexBatch(
  db: IDBDatabase,
  schema: PluginDatabaseStore,
  records: PluginDatabaseRecord[],
  totals: number[],
  opts: Options,
): Promise<void> {
  return transaction(db, schema.name, "readonly", opts, (store, guard) => {
    for (const record of records) {
      schema.indexes.forEach((index, position) => {
        for (const key of indexKeys(record.value, index, opts)) {
          totals[position]++;
          const request = store
            .index(index.name)
            .openKeyCursor(opts.keyRange.only(key));
          request.onsuccess = guard(() => {
            const cursor = request.result;
            if (!cursor) throw new Error("Restored index entry is missing");
            const order = compareKeys(cursor.primaryKey, record.key, opts);
            if (order > 0)
              throw new Error("Restored index primary key differs");
            if (order < 0) cursor.continuePrimaryKey(key, record.key);
          });
        }
      });
    }
    return () => undefined;
  });
}

async function verifyDatabase(
  db: IDBDatabase,
  source: PluginDatabaseSource,
  opts: Options,
  verifyRecord: PluginDatabaseRestoreOptions["verifyRecord"],
): Promise<void> {
  if (!source.metadata.present)
    throw new Error("Cannot verify an absent database as present");
  if (
    JSON.stringify(await metadata(db, source.metadata.name, opts)) !==
    JSON.stringify(source.metadata)
  )
    throw new Error("Restored database schema, counts, or generators differ");
  for (const schema of source.metadata.stores) {
    const iterator = source.records(schema.name)[Symbol.asyncIterator]();
    let after: IDBValidKey | undefined;
    const totals = schema.indexes.map(() => 0);
    try {
      while (true) {
        const batch = await readBatch(db, schema.name, after, opts);
        if (!batch.length) break;
        after = structuredClone(batch.at(-1)!.key);
        await verifyIndexBatch(db, schema, batch, totals, opts);
        for (const actual of batch) {
          const expected = await iterator.next();
          check(opts);
          if (
            expected.done ||
            compareKeys(expected.value.key, actual.key, opts) !== 0
          )
            throw new Error("Restored database primary-key sequence differs");
          await verifyRecord(expected.value, actual, {
            databaseName: source.metadata.name,
            storeName: schema.name,
          });
          check(opts);
        }
      }
      if (!(await iterator.next()).done)
        throw new Error("Restored database is missing records");
      await transaction(db, schema.name, "readonly", opts, (store, guard) => {
        schema.indexes.forEach((index, position) => {
          const count = store.index(index.name).count();
          count.onsuccess = guard(() => {
            if (count.result !== totals[position])
              throw new Error("Restored index entry count differs");
          });
        });
        return () => undefined;
      });
    } finally {
      await iterator.return?.();
    }
  }
}

async function populateDatabase(
  name: string,
  source: PluginDatabaseSource,
  opts: Options,
  verifyRecord: PluginDatabaseRestoreOptions["verifyRecord"],
): Promise<void> {
  if (!source.metadata.present)
    throw new Error("Cannot populate an absent database");
  const db = await openDatabase(name, opts, source.metadata);
  if (!db) throw new Error("Plugin database creation failed");
  try {
    for (const schema of source.metadata.stores) {
      let batch: PluginDatabaseRecord[] = [];
      let count = 0;
      let previous: IDBValidKey | undefined;
      for await (const record of source.records(schema.name)) {
        check(opts);
        if (
          !record ||
          compareKeys(record.key, record.key, opts) !== 0 ||
          (previous !== undefined &&
            compareKeys(previous, record.key, opts) >= 0)
        )
          throw new Error("Archive primary keys are invalid or out of order");
        previous = structuredClone(record.key);
        if (++count > schema.count)
          throw new Error("Archive has more records than its schema declares");
        batch.push(record);
        if (batch.length === opts.batchSize) {
          await writeBatch(db, schema, batch, opts);
          batch = [];
        }
      }
      if (count !== schema.count)
        throw new Error("Archive record count differs from its schema");
      if (batch.length) await writeBatch(db, schema, batch, opts);
      await restoreGenerator(db, schema, opts);
    }
    await verifyDatabase(db, source, opts, verifyRecord);
  } finally {
    db.close();
  }
}

/** Recovery/apply primitive for an already validated immutable source. It is not an atomic replacement.
 * The caller owns durable rollback and must keep the maintenance barrier through failure recovery.
 */
export async function restorePluginDatabase(
  source: PluginDatabaseSource,
  input: PluginDatabaseRestoreOptions,
): Promise<void> {
  validateSnapshot(source.metadata);
  if (typeof input.verifyRecord !== "function")
    throw new Error("Clone-value verification is required");
  const opts = options(input);
  await deleteDatabase(source.metadata.name, opts);
  if (source.metadata.present)
    await populateDatabase(
      source.metadata.name,
      source,
      opts,
      input.verifyRecord,
    );
  else {
    const unexpected = await openDatabase(source.metadata.name, opts);
    if (unexpected) {
      unexpected.close();
      throw new Error("Absent database restoration verification failed");
    }
  }
}

export async function stagePluginDatabase(
  source: PluginDatabaseSource,
  input: PluginDatabaseStageOptions,
): Promise<StagedPluginDatabase> {
  validateSnapshot(source.metadata);
  if (typeof input.verifyRecord !== "function")
    throw new Error("Clone-value verification is required");
  const opts = options(input);
  if (
    !Number.isSafeInteger(input.requiredAdditionalBytes) ||
    input.requiredAdditionalBytes < 0
  )
    throw new Error("Invalid database staging quota estimate");
  const estimate =
    input.storageEstimate ??
    (() => {
      if (!globalThis.navigator?.storage?.estimate)
        throw new Error("Database staging quota estimate is unavailable");
      return globalThis.navigator.storage.estimate();
    });
  const { quota, usage } = await estimate();
  check(opts);
  if (
    typeof quota !== "number" ||
    !Number.isFinite(quota) ||
    typeof usage !== "number" ||
    !Number.isFinite(usage) ||
    usage < 0 ||
    quota - usage < input.requiredAdditionalBytes
  )
    throw new Error(
      "Insufficient or unavailable quota for database staging and rollback",
    );
  const temporaryName = source.metadata.present
    ? `${STAGE_PREFIX}${crypto.randomUUID()}`
    : null;
  let cleaned = false;
  const cleanup = async () => {
    if (temporaryName && !cleaned) {
      await deleteDatabase(temporaryName, { ...opts, signal: undefined });
      cleaned = true;
    }
  };
  try {
    if (temporaryName)
      await populateDatabase(temporaryName, source, opts, input.verifyRecord);
  } catch (error) {
    // A blocked request may still be queued. Do not queue another mutation until recovery settles it.
    if (!(error instanceof PluginDatabaseBlockedError)) {
      try {
        await cleanup();
      } catch (cleanupError) {
        throw new AggregateError(
          [error, cleanupError],
          "Database staging and cleanup failed",
        );
      }
    }
    throw error;
  }
  return {
    metadata: source.metadata,
    temporaryName,
    cleanup,
    apply: async () => {
      if (cleaned && temporaryName)
        throw new Error("Database staging was already cleaned up");
      await restorePluginDatabase(source, input);
    },
  };
}
