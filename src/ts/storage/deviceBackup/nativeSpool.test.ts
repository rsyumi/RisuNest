// @vitest-environment node
import { describe, expect, it } from "vitest";
import { encodeCloneGraph } from "./cloneGraph";
import {
  createNativeDeviceSpool,
  type DeviceNativeInvoke,
} from "./nativeSpool";

async function hash(bytes: Uint8Array<ArrayBuffer>): Promise<string> {
  return Array.from(
    new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)),
    (byte) => byte.toString(16).padStart(2, "0"),
  ).join("");
}

function nativeFixture() {
  const objects = new Map<string, Uint8Array<ArrayBuffer>>();
  const sections = new Map<
    string,
    { metadataJson: string; rows: string[]; sealed: boolean }
  >();
  const calls: { command: string; args: Record<string, unknown> }[] = [];
  const manifest = (sectionId: string) => {
    const section = sections.get(sectionId)!;
    return {
      sectionId,
      metadataJson: section.metadataJson,
      records: section.rows.length,
      sha256: "a".repeat(64),
      sealed: section.sealed,
      present: true,
    };
  };
  const invoke = (async (command: string, args: Record<string, unknown>) => {
    calls.push({ command, args });
    expect(args.sessionId).toBe("synthetic-session");
    expect(args.spool).toBe("source");
    const id = args.objectId as string;
    const sectionId = args.sectionId as string;
    if (command === "native_device_backup_blob_begin")
      objects.set(id, new Uint8Array());
    else if (command === "native_device_backup_blob_append") {
      const bytes = args.bytes as number[];
      expect(bytes.length).toBeLessThanOrEqual(256 * 1024);
      const previous = objects.get(id)!;
      expect(args.offset).toBe(previous.length);
      const next = new Uint8Array(previous.length + bytes.length);
      next.set(previous);
      next.set(bytes, previous.length);
      objects.set(id, next);
    } else if (command === "native_device_backup_blob_finish") {
      const bytes = objects.get(id)!;
      const sha256 = await hash(bytes);
      objects.set(sha256, bytes);
      return { objectId: id, bytes: bytes.length, sha256 };
    } else if (command === "native_device_backup_blob_read") {
      expect(args.length).toBeLessThanOrEqual(256 * 1024);
      return Array.from(
        objects
          .get(id)!
          .slice(
            args.offset as number,
            (args.offset as number) + (args.length as number),
          ),
      );
    } else if (command === "native_device_backup_section_begin")
      sections.set(sectionId, {
        metadataJson: args.metadataJson as string,
        rows: [],
        sealed: false,
      });
    else if (
      command === "native_device_backup_row_append" ||
      command === "native_device_backup_row_append_from_blob"
    ) {
      const section = sections.get(sectionId)!;
      expect(args.ordinal).toBe(section.rows.length);
      section.rows.push(
        command === "native_device_backup_row_append"
          ? (args.payloadJson as string)
          : new TextDecoder().decode(objects.get(args.sha256 as string)),
      );
    } else if (command === "native_device_backup_section_finish") {
      sections.get(sectionId)!.sealed = true;
      return manifest(sectionId);
    } else if (command === "native_device_backup_section_list")
      return [...sections.keys()]
        .sort()
        .filter(
          (key) =>
            args.afterSectionId === null ||
            key > (args.afterSectionId as string),
        )
        .slice(0, 1)
        .map(manifest);
    else if (command === "native_device_backup_row_read") {
      const values = sections.get(sectionId)!.rows;
      const ordinal =
        args.afterOrdinal === null ? 0 : (args.afterOrdinal as number) + 1;
      const json = values[ordinal];
      if (json === undefined) return { rows: [], hasMore: false };
      const bytes = new TextEncoder().encode(json);
      return {
        rows: [
          {
            ordinal,
            payloadJson: bytes.length <= 256 * 1024 ? json : undefined,
            sha256: await hash(bytes),
            bytes: bytes.length,
          },
        ],
        hasMore: ordinal + 1 < values.length,
      };
    } else if (command === "native_device_backup_row_read_bytes") {
      expect(args.length).toBeLessThanOrEqual(256 * 1024);
      const bytes = new TextEncoder().encode(
        sections.get(sectionId)!.rows[args.ordinal as number],
      );
      return Array.from(
        bytes.slice(
          args.offset as number,
          (args.offset as number) + (args.length as number),
        ),
      );
    } else throw new Error(`Unexpected synthetic command ${command}`);
  }) as DeviceNativeInvoke;
  return {
    invoke,
    spool: createNativeDeviceSpool("synthetic-session", "source", invoke),
    calls,
    objects,
  };
}

describe("native device spool transport", () => {
  it("checks cancellation before every bounded binary transfer", async () => {
    const fixture = nativeFixture();
    const cancellation = new AbortController();
    const spool = createNativeDeviceSpool(
      "synthetic-session",
      "source",
      fixture.invoke,
      () => cancellation.abort(),
      () => cancellation.signal.throwIfAborted(),
    );
    await expect(
      spool.putBinary(new Blob([new Uint8Array(700001)])),
    ).rejects.toMatchObject({ name: "AbortError" });
    expect(
      fixture.calls.filter(
        ({ command }) => command === "native_device_backup_blob_append",
      ),
    ).toHaveLength(1);
    expect(
      fixture.calls.some(
        ({ command }) => command === "native_device_backup_blob_finish",
      ),
    ).toBe(false);
  });
  it("round trips binary through bounded chunks and verified SHA256 aliases", async () => {
    const fixture = nativeFixture();
    const bytes = new Uint8Array(700_001);
    bytes.fill(127);
    bytes[0] = 255;
    bytes[bytes.length - 1] = 3;
    const reference = await fixture.spool.putBinary(new Blob([bytes]));
    const actual = await fixture.spool.getBinary(reference, bytes.length);
    expect(Buffer.compare(Buffer.from(actual), Buffer.from(bytes))).toBe(0);
    expect(
      fixture.calls.filter(
        ({ command }) => command === "native_device_backup_blob_append",
      ),
    ).toHaveLength(3);
    expect(
      fixture.calls.filter(
        ({ command }) => command === "native_device_backup_blob_read",
      ),
    ).toHaveLength(3);
    fixture.objects.get(reference)![0] = 0;
    await expect(
      fixture.spool.getBinary(reference, bytes.length),
    ).rejects.toThrow("checksum mismatch");
  });

  it("streams oversized graph JSON and keeps ordinal pagination exact", async () => {
    const fixture = nativeFixture();
    await fixture.spool.beginSection("localforage", {
      profile: "risunest.device-section/v1",
      sectionId: "localforage",
      present: true,
    });
    const graph = await encodeCloneGraph(
      "x".repeat(80_000),
      fixture.spool.putBinary,
    );
    const row = {
      kind: "value" as const,
      key: "0073006100660065",
      value: graph,
    };
    await fixture.spool.appendRow("localforage", row);
    await fixture.spool.finishSection("localforage");
    const actual = [];
    for await (const value of fixture.spool.rows("localforage"))
      actual.push(value);
    expect(actual).toEqual([row]);
    expect(
      fixture.calls.some(
        ({ command }) =>
          command === "native_device_backup_row_append_from_blob",
      ),
    ).toBe(true);
    expect(
      fixture.calls.find(
        ({ command }) => command === "native_device_backup_row_read",
      )!.args.afterOrdinal,
    ).toBeNull();
    expect(
      fixture.calls.filter(
        ({ command }) => command === "native_device_backup_row_read_bytes",
      ),
    ).toHaveLength(2);
  });

  it("reads only the requested ordinal range and skips empty ranges", async () => {
    const fixture = nativeFixture();
    await fixture.spool.beginSection("localforage", {
      profile: "risunest.device-section/v1",
      sectionId: "localforage",
      present: true,
    });
    for (const key of ["a", "b", "c"]) {
      await fixture.spool.appendRow("localforage", {
        kind: "value",
        key,
        value: await encodeCloneGraph(key, fixture.spool.putBinary),
      });
    }
    await fixture.spool.finishSection("localforage");

    const values = [];
    for await (const value of fixture.spool.rows("localforage", {
      startOrdinal: 1,
      endOrdinalExclusive: 2,
    })) values.push(value);
    expect(values).toHaveLength(1);
    expect(values[0]).toMatchObject({ key: "b" });
    const rangeReads = fixture.calls.filter(
      ({ command }) => command === "native_device_backup_row_read",
    );
    expect(rangeReads).toHaveLength(1);
    expect(rangeReads[0].args).toMatchObject({ afterOrdinal: 0, limit: 1 });

    const readsBeforeEmpty = rangeReads.length;
    for await (const _value of fixture.spool.rows("localforage", {
      startOrdinal: 2,
      endOrdinalExclusive: 2,
    })) throw new Error("Empty range returned a row");
    expect(
      fixture.calls.filter(({ command }) => command === "native_device_backup_row_read"),
    ).toHaveLength(readsBeforeEmpty);
  });

  it("paginates section metadata and rejects incomplete sections", async () => {
    const fixture = nativeFixture();
    for (const sectionId of ["local-storage", "localforage"] as const) {
      await fixture.spool.beginSection(sectionId, {
        profile: "risunest.device-section/v1",
        sectionId,
        present: true,
      });
      await fixture.spool.finishSection(sectionId);
    }
    expect(
      (await fixture.spool.sections()).map((section) => section.sectionId),
    ).toEqual(["local-storage", "localforage"]);
    await fixture.spool.beginSection("device-settings", {
      profile: "risunest.device-section/v1",
      sectionId: "device-settings",
      present: false,
    });
    await expect(fixture.spool.sections()).rejects.toThrow("incomplete");
  });
});
