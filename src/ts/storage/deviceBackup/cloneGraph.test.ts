// @vitest-environment node
import { describe, expect, it, vi } from "vitest";
import {
  CLONE_GRAPH_VERSION,
  CloneGraphError,
  decodeCloneGraph,
  decodeUtf16,
  encodeCloneGraph,
  encodeUtf16,
  validateCloneGraph,
} from "./cloneGraph";
import type {
  CloneBinarySink,
  CloneBinarySource,
  CloneGraph,
} from "./cloneGraph";

function memoryBinaries() {
  const objects = new Map<string, ArrayBuffer>();
  const sink = vi.fn<CloneBinarySink>(async (body, context) => {
    expect(context.byteLength).toBe(
      body instanceof Blob ? body.size : body.byteLength,
    );
    const key = `object-${objects.size}`;
    objects.set(
      key,
      body instanceof Blob ? await body.arrayBuffer() : body.slice(0),
    );
    return key;
  });
  const source = vi.fn<CloneBinarySource>(async (key) => {
    const bytes = objects.get(key);
    if (!bytes) throw new Error("Missing synthetic binary");
    return bytes;
  });
  return { objects, sink, source };
}

async function roundTrip<T>(value: T): Promise<T> {
  const io = memoryBinaries();
  const graph = await encodeCloneGraph(value, io.sink);
  // Native serde sees only ASCII, with all binary content out of band.
  const serialized = JSON.stringify(graph);
  expect(serialized).toMatch(/^[\x00-\x7f]*$/);
  return (await decodeCloneGraph(JSON.parse(serialized), io.source)) as T;
}

function graph(nodes: CloneGraph["nodes"], root = 0): CloneGraph {
  return { version: CLONE_GRAPH_VERSION, root, nodes };
}

describe("device backup UTF-16 metadata", () => {
  it("preserves every UTF-16 code unit, including isolated surrogates, across JSON", () => {
    const source = Array.from({ length: 65_536 }, (_, unit) =>
      String.fromCharCode(unit),
    ).join("");
    const encoded = encodeUtf16(source);
    expect(encoded).toHaveLength(source.length * 4);
    expect(encoded).toMatch(/^[0-9a-f]+$/);
    expect(decodeUtf16(JSON.parse(JSON.stringify(encoded)))).toBe(source);
  });

  it.each(["0", "000", "gggg", "00AF", "\ud800"])(
    "rejects malformed encoding %j",
    (value) => {
      expect(() => decodeUtf16(value)).toThrow(CloneGraphError);
    },
  );
});

describe("device backup clone profile", () => {
  it("retains primitives without JSON coercion", async () => {
    const value = [
      undefined,
      null,
      true,
      false,
      "",
      "한글\ud800\u0000\udfff",
      NaN,
      Infinity,
      -Infinity,
      -0,
      0,
      1.25,
      9007199254740993n,
      -123n,
    ];
    const restored = await roundTrip(value);
    expect(restored).toEqual(value);
    expect(Object.is(restored[9], -0)).toBe(true);
  });

  it("preserves sparse arrays, own enumerable properties, cycles and aliases", async () => {
    const shared = { text: "shared" };
    const list = new Array(6) as unknown[] & { extra?: unknown };
    list[1] = undefined;
    list[3] = shared;
    list[5] = list;
    list.extra = shared;
    Object.defineProperty(list, "__proto__", {
      value: shared,
      enumerable: true,
    });
    const source = { first: shared, again: shared, list };
    const restored = await roundTrip(source);
    expect(restored).toEqual(source);
    expect(restored.first).toBe(restored.again);
    expect(restored.list[3]).toBe(restored.first);
    expect(restored.list.extra).toBe(restored.first);
    expect(restored.list[5]).toBe(restored.list);
    expect(Object.getPrototypeOf(restored.list)).toBe(Array.prototype);
    expect(
      Object.getOwnPropertyDescriptor(restored.list, "__proto__")!.value,
    ).toBe(restored.first);
    expect(Object.keys(restored.list)).toEqual([
      "1",
      "3",
      "5",
      "extra",
      "__proto__",
    ]);
    expect(0 in restored.list).toBe(false);
    expect(1 in restored.list).toBe(true);
    expect(2 in restored.list).toBe(false);
    expect(4 in restored.list).toBe(false);
  });

  it("defines prototype-sensitive keys as data properties", async () => {
    const source = JSON.parse(
      '{"__proto__":{"polluted":true},"constructor":"synthetic","prototype":2,"toString":3}',
    );
    const restored = await roundTrip(source);
    expect(Object.getPrototypeOf(restored)).toBe(Object.prototype);
    expect(Object.hasOwn(restored, "__proto__")).toBe(true);
    expect(restored.__proto__).toEqual({ polluted: true });
    expect(({} as Record<string, unknown>).polluted).toBeUndefined();
    expect(Object.keys(restored)).toEqual(Object.keys(source));
    expect(restored).toEqual(source);
    const nullObject = Object.assign(Object.create(null), { value: "\ud800" });
    expect(Object.getPrototypeOf(await roundTrip(nullObject))).toBeNull();
  });

  it("keeps insertion order and shared object identities across Maps and Sets", async () => {
    const object = { value: 1 };
    const map = new Map<unknown, unknown>();
    const set = new Set<unknown>([object, 4, "4", undefined, NaN]);
    map.set(object, set);
    map.set("4", object);
    map.set(4, map);
    set.add(map);
    const restored = await roundTrip({ object, map, set });
    expect([...restored.map.keys()]).toEqual([restored.object, "4", 4]);
    expect(restored.map.get(restored.object)).toBe(restored.set);
    expect(restored.map.get("4")).toBe(restored.object);
    expect(restored.map.get(4)).toBe(restored.map);
    expect([...restored.set]).toEqual([
      restored.object,
      4,
      "4",
      undefined,
      NaN,
      restored.map,
    ]);
  });

  it("round-trips valid/invalid Dates and RegExp source, flags, and observed index", async () => {
    const regexp = /(?:\ud800|a)+/dgimsuy;
    regexp.lastIndex = 8;
    const restored = await roundTrip([
      new Date(-123_456),
      new Date(NaN),
      regexp,
    ]);
    expect((restored[0] as Date).getTime()).toBe(-123_456);
    expect((restored[1] as Date).getTime()).toBeNaN();
    expect((restored[2] as RegExp).source).toBe(regexp.source);
    expect((restored[2] as RegExp).flags).toBe(regexp.flags);
    expect((restored[2] as RegExp).lastIndex).toBe(8);
  });

  it("preserves every supported typed view and a single shared backing buffer", async () => {
    const buffer = new ArrayBuffer(64);
    new Uint8Array(buffer).set(Array.from({ length: 64 }, (_, index) => index));
    const value = {
      buffer,
      views: [
        new Int8Array(buffer, 1, 8),
        new Uint8Array(buffer, 3, 7),
        new Uint8ClampedArray(buffer, 4, 8),
        new Int16Array(buffer, 2, 4),
        new Uint16Array(buffer, 6, 4),
        new Int32Array(buffer, 4, 4),
        new Uint32Array(buffer, 8, 4),
        new Float32Array(buffer, 12, 4),
        new Float64Array(buffer, 16, 4),
        new BigInt64Array(buffer, 24, 2),
        new BigUint64Array(buffer, 32, 2),
        new DataView(buffer, 5, 11),
      ],
    };
    const io = memoryBinaries();
    const encoded = await encodeCloneGraph(value, io.sink);
    expect(io.sink).toHaveBeenCalledTimes(1);
    const restored = (await decodeCloneGraph(
      JSON.parse(JSON.stringify(encoded)),
      io.source,
    )) as typeof value;
    expect(new Uint8Array(restored.buffer)).toEqual(new Uint8Array(buffer));
    for (let i = 0; i < value.views.length; i++) {
      expect(restored.views[i].constructor).toBe(value.views[i].constructor);
      expect(restored.views[i].buffer).toBe(restored.buffer);
      expect(restored.views[i].byteOffset).toBe(value.views[i].byteOffset);
      expect(restored.views[i].byteLength).toBe(value.views[i].byteLength);
    }
    (restored.views[1] as Uint8Array)[2] = 200;
    expect((restored.views[11] as DataView).getUint8(0)).toBe(200);
  });

  it("does not merge distinct buffers when binary references are deduplicated", async () => {
    const first = new Uint8Array([7, 8]).buffer;
    const second = first.slice(0);
    const encoded = await encodeCloneGraph(
      { first, second },
      async () => "same-hash",
    );
    const restored = (await decodeCloneGraph(encoded, async () => first)) as {
      first: ArrayBuffer;
      second: ArrayBuffer;
    };
    expect(restored.first).not.toBe(restored.second);
    expect(restored.first).not.toBe(first);
    new Uint8Array(restored.first)[0] = 99;
    expect(new Uint8Array(restored.second)[0]).toBe(7);
    expect(new Uint8Array(first)[0]).toBe(7);
  });

  it("passes large Blob/File bodies to the sink without embedding bytes in metadata", async () => {
    const bytes = new Uint8Array(2_000_017);
    for (let i = 0; i < bytes.length; i++) bytes[i] = i % 251;
    const blob = new Blob([bytes], { type: "application/octet-stream" });
    const file = new File([bytes], "한글 synthetic.bin", {
      type: "application/x-synthetic",
      lastModified: -1234,
    });
    const io = memoryBinaries();
    const encoded = await encodeCloneGraph(
      { blob, file, again: blob },
      io.sink,
    );
    expect(io.sink).toHaveBeenCalledTimes(2);
    expect(io.sink.mock.calls[0][0]).toBe(blob);
    expect(io.sink.mock.calls[1][0]).toBe(file);
    expect(JSON.stringify(encoded).length).toBeLessThan(2000);
    const restored = (await decodeCloneGraph(encoded, io.source)) as {
      blob: Blob;
      file: File;
      again: Blob;
    };
    expect(restored.blob).toBe(restored.again);
    expect(restored.blob.type).toBe(blob.type);
    expect(restored.file.name).toBe(file.name);
    expect(restored.file.type).toBe(file.type);
    expect(restored.file.lastModified).toBe(file.lastModified);
    expect(
      Buffer.compare(new Uint8Array(await restored.blob.arrayBuffer()), bytes),
    ).toBe(0);
    expect(
      Buffer.compare(new Uint8Array(await restored.file.arrayBuffer()), bytes),
    ).toBe(0);
  });

  it("reads binary sources provided as Blob or offset Uint8Array", async () => {
    const encoded = graph([
      { type: "buffer", reference: "bytes", byteLength: 2 },
    ]);
    const fromBlob = (await decodeCloneGraph(
      encoded,
      async () => new Blob([new Uint8Array([4, 5])]),
    )) as ArrayBuffer;
    const fromView = (await decodeCloneGraph(encoded, async () =>
      new Uint8Array([0, 4, 5, 0]).subarray(1, 3),
    )) as ArrayBuffer;
    expect([...new Uint8Array(fromBlob)]).toEqual([4, 5]);
    expect([...new Uint8Array(fromView)]).toEqual([4, 5]);
  });

  it("preserves supported errors, cyclic causes, aggregate entries and DOMException metadata", async () => {
    const constructors = [
      Error,
      EvalError,
      RangeError,
      ReferenceError,
      SyntaxError,
      TypeError,
      URIError,
    ];
    const errors = constructors.map(
      (Constructor) => new Constructor("synthetic \ud800"),
    );
    const cause = { errors };
    for (const error of errors) error.cause = cause;
    const aggregate = new AggregateError(errors, "aggregate", { cause });
    const exception = new DOMException("synthetic", "DataError");
    const restored = await roundTrip({ errors, cause, aggregate, exception });
    for (let i = 0; i < errors.length; i++) {
      expect(restored.errors[i]).toBeInstanceOf(constructors[i]);
      expect(restored.errors[i].name).toBe(errors[i].name);
      expect(restored.errors[i].message).toBe(errors[i].message);
      expect(restored.errors[i].stack).toBe(errors[i].stack);
      expect(restored.errors[i].cause).toBe(restored.cause);
    }
    expect(restored.cause.errors).toBe(restored.errors);
    expect(restored.aggregate).toBeInstanceOf(AggregateError);
    expect(restored.aggregate.errors[0]).toBe(restored.errors[0]);
    expect(restored.aggregate.cause).toBe(restored.cause);
    expect(restored.exception).toBeInstanceOf(DOMException);
    expect(restored.exception.name).toBe(exception.name);
    expect(restored.exception.message).toBe(exception.message);
    expect(restored.exception.code).toBe(exception.code);
  });

  it("distinguishes an absent Error cause and stack from explicit undefined cause", async () => {
    const absent = new Error();
    delete absent.stack;
    const present = new Error("", { cause: undefined });
    const restored = await roundTrip({ absent, present });
    expect(Object.hasOwn(restored.absent, "cause")).toBe(false);
    expect(Object.hasOwn(restored.absent, "stack")).toBe(false);
    expect(Object.hasOwn(restored.present, "cause")).toBe(true);
    expect(restored.present.cause).toBeUndefined();
  });

  it("handles deep graphs without recursive stack growth", async () => {
    let root: unknown = null;
    for (let i = 0; i < 12_000; i++) root = [root];
    let restored = await roundTrip(root);
    for (let i = 0; i < 12_000; i++) restored = (restored as unknown[])[0];
    expect(restored).toBeNull();
  });

  it.each([
    () => {},
    Symbol("synthetic"),
    new WeakMap(),
    Promise.resolve(1),
    new URL("https://example.invalid/"),
    new Number(3),
  ])("fails explicitly for values outside the profile", async (value) => {
    const io = memoryBinaries();
    await expect(
      encodeCloneGraph({ binary: new ArrayBuffer(4), value }, io.sink),
    ).rejects.toMatchObject({ code: "unsupported", location: "$.nodes[2]" });
    expect(io.sink).not.toHaveBeenCalled();
  });

  it("rejects a non-extractable CryptoKey and shared/detached/resizable buffers", async () => {
    const key = await crypto.subtle.generateKey(
      { name: "AES-GCM", length: 128 },
      false,
      ["encrypt"],
    );
    const detached = new ArrayBuffer(1);
    structuredClone(detached, { transfer: [detached] });
    const resizable = Reflect.construct(ArrayBuffer, [
      4,
      { maxByteLength: 8 },
    ]) as ArrayBuffer;
    for (const value of [key, new SharedArrayBuffer(4), detached, resizable]) {
      await expect(
        encodeCloneGraph(value, async () => "unused"),
      ).rejects.toMatchObject({ code: "unsupported", location: "$" });
    }
  });

  it("reports detached DataViews through the explicit unsupported-value diagnostic", async () => {
    const buffer = new ArrayBuffer(4);
    const view = new DataView(buffer);
    structuredClone(buffer, { transfer: [buffer] });
    await expect(
      encodeCloneGraph(view, async () => "unused"),
    ).rejects.toMatchObject({
      code: "unsupported",
      location: "$",
      valueType: "DetachedArrayBufferView",
    });
  });

  it("rejects accessors and symbol keys without evaluating or reporting source values", async () => {
    const getter = vi.fn(() => "secret synthetic value");
    const object = Object.defineProperty({}, "secret synthetic key", {
      enumerable: true,
      get: getter,
    });
    const error = await encodeCloneGraph(object, async () => "unused").catch(
      (error) => error,
    );
    expect(error).toBeInstanceOf(CloneGraphError);
    expect(error).toMatchObject({
      code: "unsupported",
      location: "$.properties[0]",
      valueType: "accessor",
    });
    expect(error.message).not.toContain("secret");
    expect(getter).not.toHaveBeenCalled();
    await expect(
      encodeCloneGraph({ [Symbol("secret")]: 2 }, async () => "unused"),
    ).rejects.toMatchObject({ code: "unsupported", valueType: "symbol-key" });
  });
});

describe("clone graph validation", () => {
  const noSource = vi.fn<CloneBinarySource>(async () => {
    throw new Error("Unexpected binary read");
  });

  it.each([
    { version: 2, root: 0, nodes: [{ type: "null" }] },
    { version: 1, root: 1, nodes: [{ type: "null" }] },
    graph([{ type: "future-type" } as never]),
    graph([{ type: "number", value: -0 }]),
    graph([{ type: "number", value: NaN }]),
    graph([{ type: "bigint", value: "01" }]),
    graph([{ type: "bigint", value: "-0" }]),
    graph([{ type: "date", value: "+infinity" }]),
    graph([{ type: "string", value: "000x" }]),
    graph([{ type: "null", extra: true } as never]),
    graph([{ type: "buffer", reference: "\ud800", byteLength: 0 }]),
    graph([{ type: "regexp", source: "", flags: "gg", lastIndex: 0 }]),
    graph([{ type: "regexp", source: "", flags: "uv", lastIndex: 0 }]),
    graph([
      { type: "array", length: 2, properties: [[encodeUtf16("length"), 0]] },
    ]),
    graph([{ type: "array", length: 2, properties: [[encodeUtf16("2"), 0]] }]),
    graph([
      {
        type: "object",
        prototype: "object",
        properties: [
          ["", 0],
          ["", 0],
        ],
      },
    ]),
    graph([
      { type: "error", errorType: "AggregateError", name: "", message: "" },
    ]),
    graph([
      { type: "error", errorType: "Error", name: "", message: "", errors: 0 },
    ]),
    graph([
      {
        type: "blob",
        reference: "bytes",
        byteLength: 0,
        mimeType: encodeUtf16("TEXT/PLAIN"),
      },
    ]),
    graph([
      {
        type: "file",
        reference: "bytes",
        byteLength: 0,
        mimeType: "",
        name: encodeUtf16("\ud800"),
        lastModified: 0,
      },
    ]),
  ])("rejects malformed metadata before binary I/O (%#)", async (input) => {
    await expect(decodeCloneGraph(input, noSource)).rejects.toBeInstanceOf(
      CloneGraphError,
    );
    expect(noSource).not.toHaveBeenCalled();
  });

  it.each([
    { viewType: "Uint16Array", byteOffset: 1, length: 1 },
    { viewType: "Uint16Array", byteOffset: 0, length: 5 },
    { viewType: "Uint8Array", byteOffset: 9, length: 0 },
    { viewType: "UnknownArray", byteOffset: 0, length: 1 },
  ])(
    "validates typed-view alignment, bounds and profile (%#)",
    async (view) => {
      const input = graph([
        { type: "view", ...view, buffer: 1 } as never,
        { type: "buffer", reference: "bytes", byteLength: 8 },
      ]);
      await expect(decodeCloneGraph(input, noSource)).rejects.toBeInstanceOf(
        CloneGraphError,
      );
      expect(noSource).not.toHaveBeenCalled();
    },
  );

  it("does not evaluate graph tag or forward-reference accessors", () => {
    const getter = vi.fn(() => "buffer");
    const input = graph([
      {
        type: "view",
        viewType: "Uint8Array",
        buffer: 1,
        byteOffset: 0,
        length: 0,
      },
      Object.defineProperty({ reference: "bytes", byteLength: 0 }, "type", {
        get: getter,
      }) as never,
    ]);
    expect(() => validateCloneGraph(input)).toThrow(CloneGraphError);
    expect(getter).not.toHaveBeenCalled();
  });

  it("rejects duplicate Map/Set entries that would silently collapse on restore", async () => {
    for (const node of [
      {
        type: "map" as const,
        entries: [
          [1, 1],
          [2, 2],
        ] as [number, number][],
      },
      { type: "set" as const, entries: [1, 2] },
    ]) {
      await expect(
        decodeCloneGraph(
          graph([
            node,
            { type: "number", value: "nan" },
            { type: "number", value: "nan" },
          ]),
          noSource,
        ),
      ).rejects.toMatchObject({ code: "invalid" });
    }
  });

  it("detects SameValueZero duplicate keys before reading binary values", async () => {
    const input = graph([
      {
        type: "map",
        entries: [
          [1, 3],
          [2, 3],
        ],
      },
      { type: "number", value: 0 },
      { type: "number", value: "-zero" },
      { type: "buffer", reference: "bytes", byteLength: 4 },
    ]);
    await expect(decodeCloneGraph(input, noSource)).rejects.toMatchObject({
      code: "invalid",
      valueType: "duplicate-map-key",
    });
    expect(noSource).not.toHaveBeenCalled();
  });

  it("rejects mismatched binary lengths and invalid sink references", async () => {
    const input = graph([
      { type: "buffer", reference: "bytes", byteLength: 4 },
    ]);
    await expect(
      decodeCloneGraph(input, async () => new Uint8Array(3)),
    ).rejects.toMatchObject({ code: "binary", valueType: "byte-length" });
    await expect(
      decodeCloneGraph(input, async () => new Blob([new Uint8Array(5)])),
    ).rejects.toMatchObject({ code: "binary", valueType: "byte-length" });
    await expect(
      encodeCloneGraph(new ArrayBuffer(1), async () => ""),
    ).rejects.toMatchObject({ code: "invalid", valueType: "binary-reference" });
  });

  it("enforces node, edge, string, sparse-array and binary limits on both directions", async () => {
    const cases: [unknown, Record<string, number>][] = [
      [[1, 2], { maxNodes: 2 }],
      [[1, 2], { maxEdges: 2 }],
      [{ ab: "cd" }, { maxStringCodeUnits: 3 }],
      [12345678n, { maxStringCodeUnits: 7 }],
      [new Array(20), { maxArrayLength: 19 }],
      [new ArrayBuffer(8), { maxBinaryBytes: 7 }],
      [[new ArrayBuffer(4), new ArrayBuffer(4)], { maxTotalBinaryBytes: 7 }],
    ];
    for (const [value, limits] of cases) {
      const io = memoryBinaries();
      await expect(
        encodeCloneGraph(value, io.sink, limits),
      ).rejects.toMatchObject({ code: "limit" });
      expect(io.sink).not.toHaveBeenCalled();
      const encoded = await encodeCloneGraph(value, io.sink);
      await expect(
        decodeCloneGraph(encoded, io.source, limits),
      ).rejects.toMatchObject({ code: "limit" });
      expect(io.source).not.toHaveBeenCalled();
    }
  });
});
