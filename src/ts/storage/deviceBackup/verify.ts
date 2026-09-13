import {
  CloneGraphError,
  decodeCloneGraph,
  decodeUtf16,
  encodeCloneGraph,
} from "./cloneGraph";
import type { CloneGraph } from "./cloneGraph";
import {
  captureDeviceSection,
  validateDeviceSectionId,
  validateSettings,
} from "./scopes";
import type {
  DeviceRow,
  DeviceSectionInfo,
  DeviceSectionMetadata,
  DeviceSpool,
  DeviceStorageEnvironment,
} from "./scopes";

/** Source failures must never be mistaken for an unsupported current value. */
class SourceFailure {
  constructor(readonly error: unknown) {}
}

async function sourceOperation<T>(operation: () => Promise<T>): Promise<T> {
  try {
    return await operation();
  } catch (error) {
    throw new SourceFailure(error);
  }
}

async function fingerprintBinary(body: Blob | ArrayBuffer): Promise<string> {
  const bytes = body instanceof Blob ? await body.arrayBuffer() : body;
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(digest), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

function validateInfo(info: DeviceSectionInfo): void {
  validateDeviceSectionId(info.sectionId);
  const { metadata, sectionId } = info;
  const database = sectionId.startsWith("indexed-db:");
  if (
    !metadata ||
    metadata.profile !== "risunest.device-section/v1" ||
    metadata.sectionId !== sectionId ||
    typeof metadata.present !== "boolean" ||
    !Number.isSafeInteger(info.recordCount) ||
    info.recordCount < 0 ||
    Object.keys(metadata).some(
      (key) =>
        ![
          "profile",
          "sectionId",
          "present",
          ...(database ? ["database"] : []),
        ].includes(key),
    ) ||
    (database && !metadata.database) ||
    (!database && sectionId !== "device-settings" && !metadata.present) ||
    (!metadata.present && info.recordCount !== 0) ||
    (sectionId === "device-settings" &&
      info.recordCount !== (metadata.present ? 1 : 0))
  )
    throw new Error("Invalid sealed device section metadata");
}

/**
 * Compare a sealed source with the current logical storage under the maintenance
 * barrier. The adapter writes neither a native spool nor target storage. IDB's
 * capture path may observe an autoIncrement generator through an aborted probe.
 *
 * At most the current and source record are normalized together. Binary bodies
 * obey cloneGraph's per-record limits; WebCrypto needs one complete binary body.
 * The source owns sealed-spool integrity checks. Its failures always propagate,
 * including failures after an earlier difference or unsupported current value.
 */
export async function verifyCurrentDeviceSection(
  info: DeviceSectionInfo,
  source: DeviceSpool,
  environment: DeviceStorageEnvironment,
): Promise<boolean> {
  validateInfo(info);
  const checkpoint = () => {
    environment.barrier.assertHeld();
    environment.signal?.throwIfAborted();
  };
  checkpoint();
  const decodeSource = (graph: CloneGraph) =>
    decodeCloneGraph(graph, (reference, context) =>
      source.getBinary(reference, context.byteLength),
    );
  const normalizeGraph = async (graph: CloneGraph, settings = false) => {
    const value = await decodeSource(graph);
    if (settings) validateSettings(value);
    return encodeCloneGraph(value, fingerprintBinary);
  };
  // Decode source metadata outside the current-capture catch. Unsupported source
  // graphs are damaged/non-portable input, not a reason to overwrite live data.
  const expectedMetadata: DeviceSectionMetadata = {
    profile: info.metadata.profile,
    sectionId: info.sectionId,
    present: info.metadata.present,
    ...(info.metadata.database
      ? { database: await normalizeGraph(info.metadata.database) }
      : {}),
  };
  const iterator = source.rows(info.sectionId)[Symbol.asyncIterator]();
  let sourceFinished = false;
  let sourceCount = 0;
  let actualCount = 0;
  let matches = true;
  let actualMetadata: DeviceSectionMetadata | undefined;
  const nextSource = () =>
    sourceOperation(async (): Promise<DeviceRow | undefined> => {
      checkpoint();
      if (sourceFinished) return undefined;
      const next = await iterator.next();
      if (next.done) {
        sourceFinished = true;
        return undefined;
      }
      const row = next.value;
      if (!row || typeof row !== "object")
        throw new Error("Invalid sealed device row");
      sourceCount++;
      if (sourceCount > info.recordCount)
        throw new Error(
          "Device source row count disagrees with sealed metadata",
        );
      if (info.sectionId.startsWith("indexed-db:")) {
        if (row.kind !== "record")
          throw new Error("Invalid sealed database row");
        decodeUtf16(row.storeName);
        return {
          kind: "record",
          storeName: row.storeName,
          key: await normalizeGraph(row.key),
          value: await normalizeGraph(row.value),
        };
      }
      if (row.kind !== "value")
        throw new Error("Invalid sealed device value row");
      const key = decodeUtf16(row.key);
      if (
        info.sectionId === "device-settings" &&
        key !== "risuNestDeviceSettings"
      )
        throw new Error("Invalid sealed device settings key");
      return {
        kind: "value",
        key: row.key,
        value: await normalizeGraph(
          row.value,
          info.sectionId === "device-settings",
        ),
      };
    });
  const unused = async (): Promise<never> => {
    throw new Error("Unsupported comparison adapter operation");
  };
  const comparison: DeviceSpool = {
    async beginSection(sectionId, metadata) {
      if (sectionId !== info.sectionId || actualMetadata)
        throw new Error("Invalid device comparison lifecycle");
      actualMetadata = metadata;
      if (JSON.stringify(metadata) !== JSON.stringify(expectedMetadata))
        matches = false;
    },
    async appendRow(sectionId, row) {
      if (sectionId !== info.sectionId || !actualMetadata)
        throw new Error("Invalid device comparison lifecycle");
      actualCount++;
      const expected = await nextSource();
      if (!expected || JSON.stringify(row) !== JSON.stringify(expected))
        matches = false;
    },
    async finishSection(sectionId) {
      if (sectionId !== info.sectionId || !actualMetadata)
        throw new Error("Invalid device comparison lifecycle");
      return {
        sectionId,
        metadata: actualMetadata,
        recordCount: actualCount,
        digest: "",
      };
    },
    putBinary: fingerprintBinary,
    getBinary: unused,
    sections: unused,
    rows() {
      throw new Error("Unsupported comparison adapter operation");
    },
  };
  try {
    try {
      await captureDeviceSection(info.sectionId, comparison, environment);
    } catch (error) {
      if (!(error instanceof CloneGraphError) || error.code !== "unsupported")
        throw error;
      matches = false;
    }
    // Consume all sealed rows even if the current section is absent, shorter,
    // different, or contains an unsupported value. Do not mask late corruption.
    while (await nextSource()) matches = false;
    if (sourceCount !== info.recordCount)
      throw new Error("Device source row count disagrees with sealed metadata");
    checkpoint();
    return matches && actualCount === sourceCount;
  } catch (error) {
    if (error instanceof SourceFailure) throw error.error;
    throw error;
  } finally {
    await iterator.return?.();
  }
}
