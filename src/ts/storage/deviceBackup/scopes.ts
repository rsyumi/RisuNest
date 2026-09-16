import { pluginDevicePrefix } from "../../plugins/pluginDeviceStorage";
import {
  reloadAppUpdateSettings,
  validateAppUpdateSettings,
} from "../../update/settings";
import {
  decodeCloneGraph,
  decodeUtf16,
  encodeCloneGraph,
  encodeUtf16,
  validateCloneGraph,
  CloneGraphError,
  type CloneGraph,
} from "./cloneGraph";
import {
  capturePluginDatabase,
  countPluginDatabaseDeletions,
  enumeratePluginDatabases,
  stagePluginDatabase,
  type PluginDatabaseSnapshot,
  type PluginDatabaseRecord,
} from "./indexedDb";

export type DeviceSectionId =
  "local-storage" | "localforage" | "device-settings" | `indexed-db:${string}`;
export interface DeviceSectionMetadata {
  profile: "risunest.device-section/v1";
  sectionId: DeviceSectionId;
  present: boolean;
  database?: CloneGraph;
}
export type DeviceRow =
  | { kind: "value"; key: string; value: CloneGraph }
  | { kind: "record"; storeName: string; key: CloneGraph; value: CloneGraph };
export interface DeviceSectionInfo {
  sectionId: DeviceSectionId;
  metadata: DeviceSectionMetadata;
  recordCount: number;
  digest: string;
}
export interface DeviceRowRange {
  startOrdinal: number;
  endOrdinalExclusive: number;
}
export interface DeviceSpool {
  beginSection(
    sectionId: DeviceSectionId,
    metadata: DeviceSectionMetadata,
  ): Promise<void>;
  appendRow(sectionId: DeviceSectionId, row: DeviceRow): Promise<void>;
  finishSection(sectionId: DeviceSectionId): Promise<DeviceSectionInfo>;
  sections(): Promise<DeviceSectionInfo[]>;
  rows(sectionId: DeviceSectionId, range?: DeviceRowRange): AsyncIterable<DeviceRow>;
  putBinary(body: Blob | ArrayBuffer): Promise<string>;
  getBinary(reference: string, bytes: number): Promise<Uint8Array>;
}
export interface DeviceStorageEnvironment {
  localStorage: Storage;
  localforage: Pick<LocalForage, "keys" | "getItem" | "setItem" | "removeItem">;
  indexedDB: IDBFactory;
  keyRange: typeof IDBKeyRange;
  barrier: { assertHeld(): void };
  signal?: AbortSignal;
  estimateStorage?: () => Promise<{ usage?: number; quota?: number }>;
  onProgress?: (progress: {
    sectionId: DeviceSectionId;
    records: number;
  }) => void;
}

function checkpoint(environment: DeviceStorageEnvironment): void {
  environment.barrier.assertHeld();
  environment.signal?.throwIfAborted();
}

export function databaseSectionId(name: string): DeviceSectionId {
  if (!name.startsWith(pluginDevicePrefix))
    throw new Error("Device database is outside the plugin boundary");
  return `indexed-db:${encodeUtf16(name)}`;
}

export function sectionDatabaseName(sectionId: DeviceSectionId): string {
  if (!sectionId.startsWith("indexed-db:"))
    throw new Error("Expected a plugin database section");
  const name = decodeUtf16(sectionId.slice("indexed-db:".length));
  if (!name.startsWith(pluginDevicePrefix))
    throw new Error("Device database is outside the plugin boundary");
  return name;
}

export function validateDeviceSectionId(
  value: string,
): asserts value is DeviceSectionId {
  if (
    value === "local-storage" ||
    value === "localforage" ||
    value === "device-settings"
  )
    return;
  sectionDatabaseName(value as DeviceSectionId);
}

export async function discoverDeviceSections(
  environment: DeviceStorageEnvironment,
): Promise<DeviceSectionId[]> {
  return [
    "local-storage",
    "localforage",
    ...(await enumeratePluginDatabases(environment)).map(databaseSectionId),
  ];
}

function localKeys(storage: Storage): string[] {
  const keys: string[] = [];
  for (let i = 0; i < storage.length; i++) {
    const key = storage.key(i);
    if (key?.startsWith(pluginDevicePrefix)) keys.push(key);
  }
  return keys.sort();
}

function requirePluginKey(encoded: string): string {
  const key = decodeUtf16(encoded);
  if (!key.startsWith(pluginDevicePrefix))
    throw new Error("Device key is outside the plugin boundary");
  return key;
}

const settingsKey = "risuNestDeviceSettings";
const updateSettingsKey = "risuNestUpdateSettings";
const settingsKeys = [settingsKey, updateSettingsKey] as const;
export function validateSettings(raw: unknown): Record<string, unknown> {
  if (typeof raw !== "string")
    throw new Error("Device settings must be a JSON string");
  const value: unknown = JSON.parse(raw);
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error("Invalid device settings");
  const settings = value as Record<string, unknown>;
  if (
    settings.schema !== "risunest.device-settings/v2" ||
    !["normal", "low-spec"].includes(settings.performanceProfile as string) ||
    typeof settings.androidKeepAliveDuringGeneration !== "boolean" ||
    typeof settings.nativeFileLogEnabled !== "boolean" ||
    !Array.isArray(settings.startupExclusions) ||
    settings.startupExclusions.some((item) => typeof item !== "string")
  )
    throw new Error("Device settings schema or values are invalid");
  if (
    Object.keys(settings).some(
      (key) =>
        ![
          "schema",
          "performanceProfile",
          "androidKeepAliveDuringGeneration",
          "nativeFileLogEnabled",
          "startupExclusions",
        ].includes(key),
    )
  )
    throw new Error("Unknown device setting");
  return settings;
}

function validateUpdateSettings(raw: unknown) {
  if (typeof raw !== "string")
    throw new Error("App update settings must be a JSON string");
  return validateAppUpdateSettings(JSON.parse(raw));
}

export async function captureDeviceSection(
  sectionId: DeviceSectionId,
  spool: DeviceSpool,
  environment: DeviceStorageEnvironment,
): Promise<DeviceSectionInfo> {
  validateDeviceSectionId(sectionId);
  checkpoint(environment);
  let records = 0;
  const encode = async (value: unknown) => {
    try {
      return await encodeCloneGraph(value, (body) => spool.putBinary(body));
    } catch (error) {
      if (error instanceof CloneGraphError)
        throw new CloneGraphError(
          error.code,
          `record[${records}]${error.location}`,
          error.valueType,
        );
      throw error;
    }
  };
  const append = async (row: DeviceRow) => {
    checkpoint(environment);
    await spool.appendRow(sectionId, row);
    environment.onProgress?.({ sectionId, records: ++records });
  };
  if (sectionId.startsWith("indexed-db:")) {
    await capturePluginDatabase(sectionDatabaseName(sectionId), environment, {
      async metadata(database) {
        await spool.beginSection(sectionId, {
          profile: "risunest.device-section/v1",
          sectionId,
          present: database.present,
          database: await encode(database),
        });
      },
      async records(storeName, batch) {
        for (const record of batch)
          await append({
            kind: "record",
            storeName: encodeUtf16(storeName),
            key: await encode(record.key),
            value: await encode(record.value),
          });
      },
    });
  } else {
    const settings = sectionId === "device-settings"
      ? settingsKeys.map((key) => [key, environment.localStorage.getItem(key)] as const)
      : [];
    for (const [key, value] of settings) {
      if (value === null) continue;
      if (key === settingsKey) validateSettings(value);
      else validateUpdateSettings(value);
    }
    await spool.beginSection(sectionId, {
      profile: "risunest.device-section/v1",
      sectionId,
      present: sectionId !== "device-settings" || settings.some(([, value]) => value !== null),
    });
    if (sectionId === "local-storage") {
      const keys = localKeys(environment.localStorage);
      for (const key of keys)
        await append({
          kind: "value",
          key: encodeUtf16(key),
          value: await encode(environment.localStorage.getItem(key)),
        });
      if (
        JSON.stringify(keys) !==
        JSON.stringify(localKeys(environment.localStorage))
      )
        throw new Error("Device local storage changed during capture");
    } else if (sectionId === "localforage") {
      const keys = (await environment.localforage.keys())
        .filter((key) => key.startsWith(pluginDevicePrefix))
        .sort();
      for (const key of keys)
        await append({
          kind: "value",
          key: encodeUtf16(key),
          value: await encode(await environment.localforage.getItem(key)),
        });
      if (
        JSON.stringify(keys) !==
        JSON.stringify(
          (await environment.localforage.keys())
            .filter((key) => key.startsWith(pluginDevicePrefix))
            .sort(),
        )
      )
        throw new Error("Device plugin storage changed during capture");
    } else {
      for (const [key, value] of settings) {
        if (value === null) continue;
        await append({ kind: "value", key: encodeUtf16(key), value: await encode(value) });
      }
    }
  }
  checkpoint(environment);
  return spool.finishSection(sectionId);
}

async function equivalent(expected: unknown, actual: unknown): Promise<void> {
  const fingerprint = async (value: unknown) =>
    JSON.stringify(
      await encodeCloneGraph(value, async (body) => {
        const bytes = body instanceof Blob ? await body.arrayBuffer() : body;
        const digest = await crypto.subtle.digest("SHA-256", bytes);
        return Array.from(new Uint8Array(digest), (byte) =>
          byte.toString(16).padStart(2, "0"),
        ).join("");
      }),
    );
  if ((await fingerprint(expected)) !== (await fingerprint(actual)))
    throw new Error("Restored device value differs from its source");
}

export interface StagedDeviceSection {
  sectionId: DeviceSectionId;
  deletionCount: number;
  apply(): Promise<void>;
  cleanup(): Promise<void>;
}

/** Validate all source rows before exposing an apply operation. Caller owns durable rollback. */
export async function stageDeviceSection(
  info: DeviceSectionInfo,
  spool: DeviceSpool,
  environment: DeviceStorageEnvironment,
  options: { requiredAdditionalBytes?: number } = {},
): Promise<StagedDeviceSection> {
  const { sectionId, metadata } = info;
  validateDeviceSectionId(sectionId);
  checkpoint(environment);
  if (
    metadata.profile !== "risunest.device-section/v1" ||
    metadata.sectionId !== sectionId ||
    typeof metadata.present !== "boolean"
  )
    throw new Error("Invalid device section metadata");
  const decode = (graph: CloneGraph) =>
    decodeCloneGraph(graph, (reference, context) =>
      spool.getBinary(reference, context.byteLength),
    );
  if (sectionId.startsWith("indexed-db:")) {
    const database = (await decode(
      metadata.database,
    )) as PluginDatabaseSnapshot;
    if (
      database.name !== sectionDatabaseName(sectionId) ||
      database.present !== metadata.present
    )
      throw new Error("Device database metadata disagrees with section");
    const storeCounts = new Map<string, number>();
    const storeNames = new Set(
      database.present ? database.stores.map((store) => store.name) : [],
    );
    const storeRanges = new Map<string, DeviceRowRange>();
    let nextOrdinal = 0;
    if (database.present) {
      for (const store of database.stores) {
        const endOrdinalExclusive = nextOrdinal + store.count;
        if (!Number.isSafeInteger(endOrdinalExclusive))
          throw new Error("Device database record count exceeds the supported range");
        storeRanges.set(store.name, {
          startOrdinal: nextOrdinal,
          endOrdinalExclusive,
        });
        nextOrdinal = endOrdinalExclusive;
      }
    }
    let recordCount = 0;
    let validationStoreIndex = 0;
    let estimatedBytes = 0;
    for await (const row of spool.rows(sectionId)) {
      checkpoint(environment);
      const storeName = row.kind === "record" ? decodeUtf16(row.storeName) : "";
      while (
        database.present
        && validationStoreIndex < database.stores.length
        && recordCount >= storeRanges.get(
          database.stores[validationStoreIndex].name,
        )!.endOrdinalExclusive
      ) validationStoreIndex++;
      const expectedStore = database.present
        ? database.stores[validationStoreIndex]
        : undefined;
      if (
        row.kind !== "record"
        || !storeNames.has(storeName)
        || expectedStore?.name !== storeName
      )
        throw new Error("Device record references an unknown store");
      validateCloneGraph(row.key);
      validateCloneGraph(row.value);
      storeCounts.set(storeName, (storeCounts.get(storeName) ?? 0) + 1);
      recordCount++;
      estimatedBytes += JSON.stringify(row).length * 2 + 1024;
      for (const graph of [row.key, row.value])
        for (const node of graph.nodes) {
          if (
            node.type === "buffer" ||
            node.type === "blob" ||
            node.type === "file"
          )
            estimatedBytes += node.byteLength;
        }
    }
    if (
      recordCount !== info.recordCount ||
      (database.present &&
        database.stores.some(
          (store) => (storeCounts.get(store.name) ?? 0) !== store.count,
        ))
    )
      throw new Error("Device database record count disagrees with metadata");
    const schemaUnits = database.present
      ? database.stores.reduce(
          (total, store) => total + 1 + store.indexes.length,
          1,
        )
      : 1;
    // Logical byte estimate includes two source copies (stage and final). Quota errors
    // remain recoverable because the original logical state lives in the native spool.
    const requiredAdditionalBytes = database.present
      ? estimatedBytes * 2 + schemaUnits * 65536
      : 0;
    if (!Number.isSafeInteger(requiredAdditionalBytes))
      throw new Error("Device database exceeds the supported staging size");
    const source = {
      metadata: database,
      async *records(storeName: string): AsyncIterable<PluginDatabaseRecord> {
        const range = storeRanges.get(storeName);
        if (!range) throw new Error("Database source requested an unknown store");
        for await (const row of spool.rows(sectionId, range)) {
          if (row.kind !== "record")
            throw new Error("Invalid database record row");
          if (decodeUtf16(row.storeName) !== storeName) continue;
          yield {
            key: (await decode(row.key)) as IDBValidKey,
            value: await decode(row.value),
          };
        }
      },
    };
    const staged = await stagePluginDatabase(source, {
      ...environment,
      verifyRecord: async (expected, actual) => {
        await equivalent(expected.key, actual.key);
        await equivalent(expected.value, actual.value);
      },
      storageEstimate: environment.estimateStorage,
      requiredAdditionalBytes: Math.max(
        options.requiredAdditionalBytes ?? 0,
        requiredAdditionalBytes,
      ),
    });
    try {
      const deletionCount = await countPluginDatabaseDeletions(
        source,
        environment,
      );
      return {
        sectionId,
        deletionCount,
        apply: () => staged.apply(),
        cleanup: () => staged.cleanup(),
      };
    } catch (error) {
      await staged.cleanup();
      throw error;
    }
  }
  const sourceKeys = new Set<string>();
  let rowCount = 0;
  for await (const row of spool.rows(sectionId)) {
    checkpoint(environment);
    if (row.kind !== "value") throw new Error("Invalid device value row");
    const key =
      sectionId === "device-settings"
        ? decodeUtf16(row.key)
        : requirePluginKey(row.key);
    if (sourceKeys.has(key)) throw new Error("Duplicate device key");
    sourceKeys.add(key);
    const value = await decode(row.value);
    if (sectionId === "local-storage" && typeof value !== "string")
      throw new Error("Local storage values must be strings");
    if (sectionId === "device-settings") {
      if (!settingsKeys.includes(key as typeof settingsKeys[number]))
        throw new Error("Invalid device settings key");
      if (key === settingsKey) validateSettings(value);
      else validateUpdateSettings(value);
    }
    rowCount++;
  }
  if (
    rowCount !== info.recordCount ||
    (!metadata.present && rowCount !== 0) ||
    (sectionId === "device-settings" && (rowCount > settingsKeys.length || (metadata.present ? rowCount === 0 : rowCount !== 0)))
  )
    throw new Error("Device section row count disagrees with metadata");
  const currentKeys =
    sectionId === "localforage"
      ? (await environment.localforage.keys()).filter((key) =>
          key.startsWith(pluginDevicePrefix),
        )
      : sectionId === "local-storage"
        ? localKeys(environment.localStorage)
        : settingsKeys.filter((key) => environment.localStorage.getItem(key) !== null);
  return {
    sectionId,
    deletionCount: currentKeys.filter((key) => !sourceKeys.has(key)).length,
    async apply() {
      checkpoint(environment);
      const keys =
        sectionId === "localforage"
          ? (await environment.localforage.keys()).filter((key) =>
              key.startsWith(pluginDevicePrefix),
            )
          : sectionId === "local-storage"
            ? localKeys(environment.localStorage)
            : [...settingsKeys];
      for (const key of keys) {
        checkpoint(environment);
        if (sectionId === "localforage")
          await environment.localforage.removeItem(key);
        else environment.localStorage.removeItem(key);
      }
      for await (const row of spool.rows(sectionId)) {
        checkpoint(environment);
        if (row.kind !== "value") throw new Error("Invalid device value row");
        const key = decodeUtf16(row.key);
        const value = await decode(row.value);
        if (sectionId === "localforage") {
          await environment.localforage.setItem(key, value);
          await equivalent(value, await environment.localforage.getItem(key));
        } else {
          environment.localStorage.setItem(key, value as string);
          if (environment.localStorage.getItem(key) !== value)
            throw new Error("Restored local value differs from its source");
        }
      }
      const appliedKeys =
        sectionId === "localforage"
          ? (await environment.localforage.keys()).filter((key) =>
              key.startsWith(pluginDevicePrefix),
            )
          : sectionId === "local-storage"
            ? localKeys(environment.localStorage)
            : settingsKeys.filter((key) => environment.localStorage.getItem(key) !== null);
      if (
        appliedKeys.length !== sourceKeys.size ||
        appliedKeys.some((key) => !sourceKeys.has(key))
      )
        throw new Error("Restored device key set differs from its source");
      if (
        sectionId === "device-settings" &&
        sourceKeys.size > 0 &&
        typeof localStorage !== "undefined" &&
        environment.localStorage === localStorage
      ) reloadAppUpdateSettings();
    },
    async cleanup() {},
  };
}
