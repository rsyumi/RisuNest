import type {
  DeviceRow,
  DeviceSectionId,
  DeviceSectionInfo,
  DeviceSectionMetadata,
  DeviceSpool,
} from "./scopes";

export type DeviceNativeInvoke = <T = unknown>(
  command: string,
  args?: Record<string, unknown>,
) => Promise<T>;
export type DeviceSpoolKind = "source" | "rollback";
const chunkBytes = 256 * 1024;
const maximumRowBytes = 64 * 1024 * 1024;

interface NativeSection {
  sectionId: DeviceSectionId;
  metadataJson: string;
  records: number;
  sha256: string;
  sealed: boolean;
}

function sectionInfo(section: NativeSection): DeviceSectionInfo {
  if (!section.sealed) throw new Error("Device spool section is incomplete");
  return {
    sectionId: section.sectionId,
    metadata: JSON.parse(section.metadataJson) as DeviceSectionMetadata,
    recordCount: section.records,
    digest: section.sha256,
  };
}

/** All IPC buffers are bounded, including a single oversized clone graph row. */
export function createNativeDeviceSpool(
  sessionId: string,
  spool: DeviceSpoolKind,
  transport: DeviceNativeInvoke,
  onTransferredBytes?: (bytes: number) => void,
  checkpoint?: () => void,
): DeviceSpool {
  const invoke: DeviceNativeInvoke = async (command, args) => {
    checkpoint?.();
    return transport(command, args);
  };
  const ordinals = new Map<string, number>();
  const scope = { sessionId, spool };
  const putBinary = async (body: Blob | ArrayBuffer): Promise<string> => {
    const objectId = crypto.randomUUID();
    const bytes = body instanceof Blob ? body.size : body.byteLength;
    await invoke("native_device_backup_blob_begin", { ...scope, objectId });
    for (let offset = 0; offset < bytes; offset += chunkBytes) {
      const buffer =
        body instanceof Blob
          ? await body.slice(offset, offset + chunkBytes).arrayBuffer()
          : body.slice(offset, offset + chunkBytes);
      await invoke("native_device_backup_blob_append", {
        ...scope,
        objectId,
        offset,
        bytes: Array.from(new Uint8Array(buffer)),
      });
      onTransferredBytes?.(buffer.byteLength);
    }
    const completed = await invoke<{ bytes: number; sha256: string }>(
      "native_device_backup_blob_finish",
      { ...scope, objectId },
    );
    if (completed.bytes !== bytes || !/^[0-9a-f]{64}$/.test(completed.sha256))
      throw new Error("Native device object completion is invalid");
    return completed.sha256;
  };
  const readBytes = async (
    bytes: number,
    read: (offset: number, length: number) => Promise<number[]>,
  ): Promise<Uint8Array<ArrayBuffer>> => {
    if (!Number.isSafeInteger(bytes) || bytes < 0)
      throw new Error("Invalid device object byte length");
    const result = new Uint8Array(bytes);
    for (let offset = 0; offset < bytes; offset += chunkBytes) {
      const length = Math.min(chunkBytes, bytes - offset);
      const part = await read(offset, length);
      if (
        part.length !== length ||
        part.some((byte) => !Number.isInteger(byte) || byte < 0 || byte > 255)
      )
        throw new Error("Device spool returned an invalid byte range");
      result.set(part, offset);
      onTransferredBytes?.(part.length);
    }
    return result;
  };
  return {
    async beginSection(sectionId, metadata) {
      const metadataJson = JSON.stringify(metadata);
      if (new TextEncoder().encode(metadataJson).byteLength > chunkBytes)
        throw new Error("Device schema exceeds the supported metadata limit");
      await invoke("native_device_backup_section_begin", {
        ...scope,
        sectionId,
        metadataJson,
      });
      onTransferredBytes?.(new TextEncoder().encode(metadataJson).byteLength);
      ordinals.set(sectionId, 0);
    },
    async appendRow(sectionId, row) {
      const ordinal = ordinals.get(sectionId);
      if (ordinal === undefined)
        throw new Error("Device section has not started");
      const payloadJson = JSON.stringify(row);
      const bytes = new TextEncoder().encode(payloadJson);
      if (bytes.byteLength > maximumRowBytes)
        throw new Error("Device clone graph exceeds the supported row limit");
      if (bytes.byteLength <= chunkBytes) {
        await invoke("native_device_backup_row_append", {
          ...scope,
          sectionId,
          ordinal,
          payloadJson,
        });
        onTransferredBytes?.(bytes.byteLength);
      } else {
        const sha256 = await putBinary(bytes.buffer);
        await invoke("native_device_backup_row_append_from_blob", {
          ...scope,
          sectionId,
          ordinal,
          sha256,
        });
      }
      ordinals.set(sectionId, ordinal + 1);
    },
    async finishSection(sectionId) {
      return sectionInfo(
        await invoke<NativeSection>("native_device_backup_section_finish", {
          ...scope,
          sectionId,
        }),
      );
    },
    async sections() {
      const sections: DeviceSectionInfo[] = [];
      let afterSectionId: string | null = null;
      while (true) {
        const page = await invoke<NativeSection[]>(
          "native_device_backup_section_list",
          { ...scope, afterSectionId, limit: 128 },
        );
        if (!page.length) return sections;
        for (const section of page) {
          if (afterSectionId !== null && section.sectionId <= afterSectionId)
            throw new Error("Device section pagination made no progress");
          sections.push(sectionInfo(section));
          afterSectionId = section.sectionId;
        }
      }
    },
    async *rows(sectionId, range): AsyncIterable<DeviceRow> {
      const startOrdinal = range?.startOrdinal ?? 0;
      const endOrdinalExclusive = range?.endOrdinalExclusive ?? Number.MAX_SAFE_INTEGER;
      if (
        !Number.isSafeInteger(startOrdinal)
        || startOrdinal < 0
        || !Number.isSafeInteger(endOrdinalExclusive)
        || endOrdinalExclusive < startOrdinal
      )
        throw new Error("Invalid device spool row range");
      if (startOrdinal === endOrdinalExclusive) return;
      let afterOrdinal = startOrdinal - 1;
      while (true) {
        const page = await invoke<{
          rows: {
            ordinal: number;
            payloadJson?: string;
            sha256: string;
            bytes: number;
          }[];
          hasMore: boolean;
        }>("native_device_backup_row_read", {
          ...scope,
          sectionId,
          afterOrdinal: afterOrdinal < 0 ? null : afterOrdinal,
          limit: Math.min(64, endOrdinalExclusive - afterOrdinal - 1),
        });
        if (page.hasMore && page.rows.length === 0)
          throw new Error("Device spool pagination made no progress");
        for (const row of page.rows) {
          if (row.ordinal !== afterOrdinal + 1 || row.bytes > maximumRowBytes)
            throw new Error("Device spool row sequence or size is invalid");
          const json =
            row.payloadJson ??
            new TextDecoder("utf-8", { fatal: true }).decode(
              await readBytes(row.bytes, (offset, length) =>
                invoke("native_device_backup_row_read_bytes", {
                  ...scope,
                  sectionId,
                  ordinal: row.ordinal,
                  offset,
                  length,
                }),
              ),
            );
          if (row.payloadJson !== undefined) onTransferredBytes?.(row.bytes);
          yield JSON.parse(json) as DeviceRow;
          afterOrdinal = row.ordinal;
          if (afterOrdinal + 1 === endOrdinalExclusive) return;
        }
        if (!page.hasMore) break;
      }
    },
    putBinary,
    async getBinary(sha256, bytes) {
      if (!/^[0-9a-f]{64}$/.test(sha256))
        throw new Error("Invalid device object reference");
      const result = await readBytes(bytes, (offset, length) =>
        invoke("native_device_backup_blob_read", {
          ...scope,
          objectId: sha256,
          offset,
          length,
        }),
      );
      const actual = Array.from(
        new Uint8Array(await crypto.subtle.digest("SHA-256", result)),
        (byte) => byte.toString(16).padStart(2, "0"),
      ).join("");
      if (actual !== sha256) throw new Error("Device object checksum mismatch");
      return result;
    },
  };
}
