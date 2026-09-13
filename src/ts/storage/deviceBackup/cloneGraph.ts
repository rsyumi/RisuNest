/** Versioned, JSON-safe profile for values read from device storage.
 * Strings are UTF-16 code units in ASCII hex, including lone surrogates. Binary
 * bodies never enter JSON. The caller owns chunking, hashing, cancellation and
 * cleanup of the binary sink/source. Object identity is local to one graph.
 */
export const CLONE_GRAPH_VERSION = 1 as const;

export interface CloneGraphLimits {
  maxNodes: number;
  maxEdges: number;
  maxStringCodeUnits: number;
  maxArrayLength: number;
  maxBinaryBytes: number;
  maxTotalBinaryBytes: number;
}

export const DEFAULT_CLONE_GRAPH_LIMITS: Readonly<CloneGraphLimits> =
  Object.freeze({
    maxNodes: 100_000,
    maxEdges: 1_000_000,
    maxStringCodeUnits: 16_777_216,
    maxArrayLength: 10_000_000,
    maxBinaryBytes: 536_870_912,
    maxTotalBinaryBytes: 1_073_741_824,
  });

export interface CloneBinaryContext {
  /** Structural path only. Property names and source values are not diagnostics. */
  location: string;
  byteLength: number;
}

export type CloneBinarySink = (
  body: Blob | ArrayBuffer,
  context: CloneBinaryContext,
) => Promise<string>;
export type CloneBinarySource = (
  reference: string,
  context: CloneBinaryContext,
) => Promise<Blob | ArrayBuffer | Uint8Array>;

type NumberValue = number | "nan" | "+infinity" | "-infinity" | "-zero";
type Properties = [string, number][];
const ERROR_TYPES = [
  "Error",
  "EvalError",
  "RangeError",
  "ReferenceError",
  "SyntaxError",
  "TypeError",
  "URIError",
  "AggregateError",
] as const;
type ErrorType = (typeof ERROR_TYPES)[number];
const VIEW_BYTES = {
  Int8Array: 1,
  Uint8Array: 1,
  Uint8ClampedArray: 1,
  Int16Array: 2,
  Uint16Array: 2,
  Int32Array: 4,
  Uint32Array: 4,
  Float32Array: 4,
  Float64Array: 8,
  BigInt64Array: 8,
  BigUint64Array: 8,
  DataView: 1,
} as const;
type ViewType = keyof typeof VIEW_BYTES;

export type CloneGraphNode =
  | { type: "null" | "undefined" }
  | { type: "boolean"; value: boolean }
  | { type: "string" | "bigint"; value: string }
  | { type: "number" | "date"; value: NumberValue }
  | { type: "object"; prototype: "object" | "null"; properties: Properties }
  | { type: "array"; length: number; properties: Properties }
  | { type: "regexp"; source: string; flags: string; lastIndex: NumberValue }
  | { type: "map"; entries: [number, number][] }
  | { type: "set"; entries: number[] }
  | { type: "buffer"; reference: string; byteLength: number }
  | {
      type: "view";
      viewType: ViewType;
      buffer: number;
      byteOffset: number;
      length: number;
    }
  | { type: "blob"; reference: string; byteLength: number; mimeType: string }
  | {
      type: "file";
      reference: string;
      byteLength: number;
      mimeType: string;
      name: string;
      lastModified: number;
    }
  | {
      type: "error";
      errorType: ErrorType;
      name: string;
      message: string;
      stack?: string;
      cause?: number;
      errors?: number;
    }
  | { type: "domexception"; name: string; message: string; stack?: string };

export interface CloneGraph {
  version: typeof CLONE_GRAPH_VERSION;
  root: number;
  nodes: CloneGraphNode[];
}

export class CloneGraphError extends Error {
  constructor(
    public readonly code: "unsupported" | "invalid" | "limit" | "binary",
    public readonly location: string,
    public readonly valueType: string,
  ) {
    super(`Clone graph ${code}: ${valueType} at ${location}`);
    this.name = "CloneGraphError";
  }
}

function fail(
  code: CloneGraphError["code"],
  location: string,
  type: string,
): never {
  throw new CloneGraphError(code, location, type);
}

/** Four lowercase hex digits per code unit; no UTF-8 transcoding takes place. */
export function encodeUtf16(value: string): string {
  const chunks: string[] = [];
  for (let offset = 0; offset < value.length; offset += 4096) {
    let chunk = "";
    for (let i = offset; i < Math.min(offset + 4096, value.length); i++) {
      chunk += value.charCodeAt(i).toString(16).padStart(4, "0");
    }
    chunks.push(chunk);
  }
  return chunks.join("");
}

function isUtf16(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value.length % 4 === 0 &&
    /^[0-9a-f]*$/.test(value)
  );
}

export function decodeUtf16(value: string): string {
  if (!isUtf16(value)) fail("invalid", "$", "UTF16");
  const chunks: string[] = [];
  for (let offset = 0; offset < value.length; offset += 16_384) {
    const units: number[] = [];
    for (let i = offset; i < Math.min(offset + 16_384, value.length); i += 4) {
      units.push(parseInt(value.slice(i, i + 4), 16));
    }
    chunks.push(String.fromCharCode(...units));
  }
  return chunks.join("");
}

function encodeNumber(value: number): NumberValue {
  if (Number.isNaN(value)) return "nan";
  if (value === Infinity) return "+infinity";
  if (value === -Infinity) return "-infinity";
  if (Object.is(value, -0)) return "-zero";
  return value;
}

function decodeNumber(value: NumberValue): number {
  switch (value) {
    case "nan":
      return NaN;
    case "+infinity":
      return Infinity;
    case "-infinity":
      return -Infinity;
    case "-zero":
      return -0;
    default:
      return value;
  }
}

function limitsFor(overrides: Partial<CloneGraphLimits>): CloneGraphLimits {
  const limits = { ...DEFAULT_CLONE_GRAPH_LIMITS, ...overrides };
  for (const value of Object.values(limits)) {
    if (!Number.isSafeInteger(value) || value < 0)
      fail("invalid", "$", "limits");
  }
  return limits;
}

function referenceValid(value: unknown): value is string {
  return typeof value === "string" && /^[\x21-\x7e]{1,1024}$/.test(value);
}

function isIndex(key: string): boolean {
  const n = Number(key);
  return Number.isInteger(n) && n >= 0 && n < 0xffff_ffff && String(n) === key;
}

function hasLoneSurrogate(value: string): boolean {
  for (let i = 0; i < value.length; i++) {
    const unit = value.charCodeAt(i);
    if (unit >= 0xd800 && unit <= 0xdbff) {
      const next = value.charCodeAt(++i);
      if (!(next >= 0xdc00 && next <= 0xdfff)) return true;
    } else if (unit >= 0xdc00 && unit <= 0xdfff) return true;
  }
  return false;
}

function unsupportedObjectType(prototype: object): string {
  // Only fixed platform type labels enter diagnostics, never custom class names
  // or Symbol.toStringTag values supplied by a stored object.
  const globals = globalThis as unknown as Record<
    string,
    { prototype?: object } | undefined
  >;
  for (const type of [
    "CryptoKey",
    "SharedArrayBuffer",
    "WeakMap",
    "WeakSet",
    "Promise",
    "URL",
    "FileSystemHandle",
    "FileSystemFileHandle",
    "FileSystemDirectoryHandle",
    "Boolean",
    "Number",
    "String",
  ]) {
    if (prototype === globals[type]?.prototype) return type;
  }
  return "object-outside-profile";
}

/** Rejects malformed/unknown tags, dangling references and limits before decode I/O. */
export function validateCloneGraph(
  input: unknown,
  overrides: Partial<CloneGraphLimits> = {},
): asserts input is CloneGraph {
  const limits = limitsFor(overrides);
  let edges = 0;
  let strings = 0;
  let binaryBytes = 0;
  const record = (
    value: unknown,
    required: string[],
    optional: string[],
    location: string,
  ): Record<string, any> => {
    if (!value || typeof value !== "object" || Array.isArray(value))
      fail("invalid", location, "record");
    const proto = Object.getPrototypeOf(value);
    if (proto !== Object.prototype && proto !== null)
      fail("invalid", location, "record");
    const keys = Reflect.ownKeys(value);
    if (
      required.some((key) => !Object.hasOwn(value, key)) ||
      keys.some(
        (key) =>
          typeof key !== "string" ||
          (!required.includes(key) && !optional.includes(key)),
      )
    )
      fail("invalid", location, "fields");
    if (
      keys.some(
        (key) =>
          !Object.hasOwn(Object.getOwnPropertyDescriptor(value, key)!, "value"),
      )
    )
      fail("invalid", location, "accessor");
    return value as Record<string, any>;
  };
  const graph = record(input, ["version", "root", "nodes"], [], "$");
  if (graph.version !== CLONE_GRAPH_VERSION || !Array.isArray(graph.nodes))
    fail("invalid", "$", "version-or-nodes");
  if (graph.nodes.length === 0 || graph.nodes.length > limits.maxNodes)
    fail("limit", "$.nodes", "nodes");
  const uint = (value: unknown, max: number, location: string) => {
    if (
      !Number.isSafeInteger(value) ||
      (value as number) < 0 ||
      Object.is(value, -0) ||
      (value as number) > max
    )
      fail("invalid", location, "integer");
  };
  const ref = (value: unknown, location: string) => {
    uint(value, graph.nodes.length - 1, location);
    if (++edges > limits.maxEdges) fail("limit", location, "edges");
  };
  const utf16 = (value: unknown, location: string) => {
    if (typeof value !== "string") fail("invalid", location, "UTF16");
    strings += value.length / 4;
    if (strings > limits.maxStringCodeUnits) fail("limit", location, "strings");
    if (!isUtf16(value)) fail("invalid", location, "UTF16");
  };
  const number = (value: unknown, location: string) => {
    if (
      !(
        typeof value === "number" &&
        Number.isFinite(value) &&
        !Object.is(value, -0)
      ) &&
      !["nan", "+infinity", "-infinity", "-zero"].includes(value as string)
    )
      fail("invalid", location, "number");
  };
  const props = (value: unknown, location: string, length?: number) => {
    if (!Array.isArray(value)) fail("invalid", location, "properties");
    const seen = new Set<string>();
    for (let i = 0; i < value.length; i++) {
      const pair = value[i];
      const path = `${location}[${i}]`;
      if (!Array.isArray(pair) || pair.length !== 2)
        fail("invalid", path, "property");
      utf16(pair[0], path);
      if (seen.has(pair[0])) fail("invalid", path, "duplicate-property");
      seen.add(pair[0]);
      if (length !== undefined) {
        const key = decodeUtf16(pair[0]);
        if (key === "length" || (isIndex(key) && Number(key) >= length))
          fail("invalid", path, "array-property");
      }
      ref(pair[1], path);
    }
  };
  ref(graph.root, "$.root");
  for (let i = 0; i < graph.nodes.length; i++) {
    const path = `$.nodes[${i}]`;
    const raw = graph.nodes[i];
    // Inspect the tag only after proving it is an own data property.
    const tag =
      raw &&
      typeof raw === "object" &&
      Object.getOwnPropertyDescriptor(raw, "type");
    if (!tag || !Object.hasOwn(tag, "value")) fail("invalid", path, "tag");
    const type = tag.value;
    const node = (required: string[], optional: string[] = []) =>
      record(raw, ["type", ...required], optional, path);
    switch (type) {
      case "null":
      case "undefined":
        node([]);
        break;
      case "boolean":
        if (typeof node(["value"]).value !== "boolean")
          fail("invalid", path, "boolean");
        break;
      case "string":
        utf16(node(["value"]).value, path);
        break;
      case "bigint": {
        const value = node(["value"]).value;
        if (typeof value !== "string") fail("invalid", path, "bigint");
        strings += value.length;
        if (strings > limits.maxStringCodeUnits) fail("limit", path, "strings");
        if (!/^(?:0|-?[1-9][0-9]*)$/.test(value))
          fail("invalid", path, "bigint");
        break;
      }
      case "number":
        number(node(["value"]).value, path);
        break;
      case "date": {
        const value = node(["value"]).value;
        if (
          value !== "nan" &&
          (typeof value !== "number" ||
            !Number.isInteger(value) ||
            Object.is(value, -0) ||
            Math.abs(value) > 8_640_000_000_000_000)
        )
          fail("invalid", path, "date");
        break;
      }
      case "object": {
        const value = node(["prototype", "properties"]);
        if (!["object", "null"].includes(value.prototype))
          fail("invalid", path, "prototype");
        props(value.properties, `${path}.properties`);
        break;
      }
      case "array": {
        const value = node(["length", "properties"]);
        uint(value.length, 0xffff_ffff, path);
        if (value.length > limits.maxArrayLength)
          fail("limit", path, "array-length");
        props(value.properties, `${path}.properties`, value.length);
        break;
      }
      case "regexp": {
        const value = node(["source", "flags", "lastIndex"]);
        utf16(value.source, path);
        if (
          typeof value.flags !== "string" ||
          !/^(?:d?g?i?m?s?u?v?y?)$/.test(value.flags) ||
          (value.flags.includes("u") && value.flags.includes("v"))
        )
          fail("invalid", path, "regexp-flags");
        number(value.lastIndex, path);
        try {
          new RegExp(decodeUtf16(value.source), value.flags);
        } catch {
          fail("unsupported", path, "RegExp");
        }
        break;
      }
      case "map":
      case "set": {
        const value = node(["entries"]);
        if (!Array.isArray(value.entries)) fail("invalid", path, "entries");
        for (let j = 0; j < value.entries.length; j++) {
          const entry = value.entries[j];
          if (type === "map") {
            if (!Array.isArray(entry) || entry.length !== 2)
              fail("invalid", path, "map-entry");
            ref(entry[0], `${path}.entries[${j}]`);
            ref(entry[1], `${path}.entries[${j}]`);
          } else ref(entry, `${path}.entries[${j}]`);
        }
        break;
      }
      case "buffer":
      case "blob":
      case "file": {
        const value = node([
          "reference",
          "byteLength",
          ...(type !== "buffer" ? ["mimeType"] : []),
          ...(type === "file" ? ["name", "lastModified"] : []),
        ]);
        if (!referenceValid(value.reference))
          fail("invalid", path, "binary-reference");
        uint(value.byteLength, Number.MAX_SAFE_INTEGER, path);
        binaryBytes += value.byteLength;
        if (
          value.byteLength > limits.maxBinaryBytes ||
          binaryBytes > limits.maxTotalBinaryBytes
        )
          fail("limit", path, "binary-bytes");
        if (type !== "buffer") {
          utf16(value.mimeType, path);
          const mime = decodeUtf16(value.mimeType);
          if (!/^[\x20-\x7e]*$/.test(mime) || mime !== mime.toLowerCase())
            fail("invalid", path, "mime-type");
        }
        if (type === "file") {
          utf16(value.name, path);
          if (!Number.isSafeInteger(value.lastModified))
            fail("invalid", path, "last-modified");
          const name = decodeUtf16(value.name);
          if (hasLoneSurrogate(name)) fail("invalid", path, "file-name");
        }
        break;
      }
      case "view": {
        const value = node(["viewType", "buffer", "byteOffset", "length"]);
        if (
          typeof value.viewType !== "string" ||
          !Object.hasOwn(VIEW_BYTES, value.viewType)
        )
          fail("unsupported", path, "ArrayBufferView");
        ref(value.buffer, path);
        uint(value.byteOffset, Number.MAX_SAFE_INTEGER, path);
        uint(value.length, Number.MAX_SAFE_INTEGER, path);
        break;
      }
      case "error":
      case "domexception": {
        const value = node(
          ["name", "message", ...(type === "error" ? ["errorType"] : [])],
          ["stack", ...(type === "error" ? ["cause", "errors"] : [])],
        );
        utf16(value.name, path);
        utf16(value.message, path);
        if (Object.hasOwn(value, "stack")) utf16(value.stack, path);
        if (type === "error") {
          if (!ERROR_TYPES.includes(value.errorType))
            fail("unsupported", path, "Error");
          if (Object.hasOwn(value, "cause")) ref(value.cause, `${path}.cause`);
          if (Object.hasOwn(value, "errors"))
            ref(value.errors, `${path}.errors`);
          if (
            (value.errorType === "AggregateError") !==
            Object.hasOwn(value, "errors")
          )
            fail("invalid", path, "aggregate-errors");
        }
        break;
      }
      default:
        fail("unsupported", path, "node-tag");
    }
  }
  // All target records have been checked before inspecting forward references.
  for (let i = 0; i < graph.nodes.length; i++) {
    const node = graph.nodes[i] as CloneGraphNode;
    if (node.type === "view") {
      const target = graph.nodes[node.buffer];
      const width = VIEW_BYTES[node.viewType];
      if (
        target.type !== "buffer" ||
        node.byteOffset % width !== 0 ||
        node.length > Math.floor((target.byteLength - node.byteOffset) / width)
      )
        fail("invalid", `$.nodes[${i}]`, "view-bounds");
    } else if (node.type === "map" || node.type === "set") {
      const seen = new Set<string>();
      for (const entry of node.entries) {
        const reference = typeof entry === "number" ? entry : entry[0];
        const target = graph.nodes[reference] as CloneGraphNode;
        let identity: string;
        switch (target.type) {
          case "null":
          case "undefined":
            identity = target.type;
            break;
          case "number":
            identity = `number:${target.value === "-zero" ? 0 : target.value}`;
            break;
          case "string":
          case "bigint":
          case "boolean":
            identity = `${target.type}:${target.value}`;
            break;
          default:
            identity = `reference:${reference}`;
        }
        if (seen.has(identity))
          fail(
            "invalid",
            `$.nodes[${i}]`,
            node.type === "map" ? "duplicate-map-key" : "duplicate-set-entry",
          );
        seen.add(identity);
      }
    }
  }
}

export async function encodeCloneGraph(
  value: unknown,
  binarySink: CloneBinarySink,
  overrides: Partial<CloneGraphLimits> = {},
): Promise<CloneGraph> {
  const limits = limitsFor(overrides);
  const nodes: CloneGraphNode[] = [];
  const queue: { value: unknown; location: string }[] = [];
  const identities = new Map<object, number>();
  const binaries: {
    node: Extract<CloneGraphNode, { reference: string }>;
    body: Blob | ArrayBuffer;
    context: CloneBinaryContext;
  }[] = [];
  let edges = 0;
  let strings = 0;
  const string = (text: string, path: string) => {
    strings += text.length;
    if (strings > limits.maxStringCodeUnits) fail("limit", path, "strings");
    return encodeUtf16(text);
  };
  const add = (item: unknown, location: string): number => {
    if (++edges > limits.maxEdges) fail("limit", location, "edges");
    if (item !== null && typeof item === "object" && identities.has(item))
      return identities.get(item)!;
    if (queue.length >= limits.maxNodes) fail("limit", location, "nodes");
    const id = queue.length;
    queue.push({ value: item, location });
    if (item !== null && typeof item === "object") identities.set(item, id);
    return id;
  };
  const properties = (item: object, path: string): Properties => {
    const result: Properties = [];
    for (const key of Reflect.ownKeys(item)) {
      const descriptor = Object.getOwnPropertyDescriptor(item, key)!;
      if (!descriptor.enumerable) continue;
      const location = `${path}.properties[${result.length}]`;
      if (typeof key !== "string") fail("unsupported", location, "symbol-key");
      if (!Object.hasOwn(descriptor, "value"))
        fail("unsupported", location, "accessor");
      result.push([string(key, location), add(descriptor.value, location)]);
    }
    return result;
  };
  const root = add(value, "$");
  for (let i = 0; i < queue.length; i++) {
    const { value: item } = queue[i];
    // Flat node paths keep diagnostics bounded even for deeply nested graphs.
    const path = i === 0 ? "$" : `$.nodes[${i}]`;
    if (item === null) {
      nodes.push({ type: "null" });
      continue;
    }
    switch (typeof item) {
      case "undefined":
        nodes.push({ type: "undefined" });
        continue;
      case "boolean":
        nodes.push({ type: "boolean", value: item });
        continue;
      case "string":
        nodes.push({ type: "string", value: string(item, path) });
        continue;
      case "number":
        nodes.push({ type: "number", value: encodeNumber(item) });
        continue;
      case "bigint": {
        const decimal = item.toString();
        strings += decimal.length;
        if (strings > limits.maxStringCodeUnits) fail("limit", path, "strings");
        nodes.push({ type: "bigint", value: decimal });
        continue;
      }
      case "symbol":
      case "function":
        fail("unsupported", path, typeof item);
    }
    const object = item as object;
    const prototype = Object.getPrototypeOf(object);
    if (Array.isArray(item)) {
      if (item.length > limits.maxArrayLength)
        fail("limit", path, "array-length");
      nodes.push({
        type: "array",
        length: item.length,
        properties: properties(item, path),
      });
    } else if (prototype === Object.prototype || prototype === null) {
      nodes.push({
        type: "object",
        prototype: prototype === null ? "null" : "object",
        properties: properties(object, path),
      });
    } else if (prototype === Date.prototype) {
      nodes.push({
        type: "date",
        value: encodeNumber(Date.prototype.getTime.call(item)),
      });
    } else if (prototype === RegExp.prototype) {
      const regexp = item as RegExp;
      if (typeof regexp.lastIndex !== "number")
        fail("unsupported", path, "RegExp.lastIndex");
      nodes.push({
        type: "regexp",
        source: string(regexp.source, path),
        flags: regexp.flags,
        lastIndex: encodeNumber(regexp.lastIndex),
      });
    } else if (prototype === Map.prototype) {
      const entries: [number, number][] = [];
      for (const [key, val] of Map.prototype.entries.call(item)) {
        const entryPath = `${path}.entries[${entries.length}]`;
        entries.push([
          add(key, `${entryPath}.key`),
          add(val, `${entryPath}.value`),
        ]);
      }
      nodes.push({ type: "map", entries });
    } else if (prototype === Set.prototype) {
      const entries: number[] = [];
      for (const val of Set.prototype.values.call(item))
        entries.push(add(val, `${path}.entries[${entries.length}]`));
      nodes.push({ type: "set", entries });
    } else if (prototype === ArrayBuffer.prototype) {
      const buffer = item as ArrayBuffer;
      if ((buffer as ArrayBuffer & { resizable?: boolean }).resizable)
        fail("unsupported", path, "ResizableArrayBuffer");
      try {
        new Uint8Array(buffer);
      } catch {
        fail("unsupported", path, "DetachedArrayBuffer");
      }
      const node = {
        type: "buffer" as const,
        reference: "pending",
        byteLength: buffer.byteLength,
      };
      nodes.push(node);
      binaries.push({
        node,
        body: buffer,
        context: { location: path, byteLength: buffer.byteLength },
      });
    } else if (ArrayBuffer.isView(item)) {
      const viewType = (Object.keys(VIEW_BYTES) as ViewType[]).find(
        (type) => prototype === globalThis[type]?.prototype,
      );
      if (!viewType) fail("unsupported", path, "ArrayBufferView");
      try {
        new Uint8Array(item.buffer, 0, 0);
      } catch {
        fail("unsupported", path, "DetachedArrayBufferView");
      }
      nodes.push({
        type: "view",
        viewType,
        buffer: add(item.buffer, `${path}.buffer`),
        byteOffset: item.byteOffset,
        length:
          viewType === "DataView"
            ? item.byteLength
            : (item as unknown as { length: number }).length,
      });
    } else if (
      (typeof File !== "undefined" && prototype === File.prototype) ||
      (typeof Blob !== "undefined" && prototype === Blob.prototype)
    ) {
      const blob = item as Blob;
      const base = {
        reference: "pending",
        byteLength: blob.size,
        mimeType: string(blob.type, path),
      };
      const node: Extract<CloneGraphNode, { type: "blob" | "file" }> =
        typeof File !== "undefined" && prototype === File.prototype
          ? {
              type: "file",
              ...base,
              name: string((item as File).name, path),
              lastModified: (item as File).lastModified,
            }
          : { type: "blob", ...base };
      nodes.push(node);
      binaries.push({
        node,
        body: blob,
        context: { location: path, byteLength: blob.size },
      });
    } else {
      const errorType = ERROR_TYPES.find(
        (type) => prototype === globalThis[type]?.prototype,
      );
      const isDomException =
        typeof DOMException !== "undefined" &&
        prototype === DOMException.prototype;
      if (!errorType && !isDomException)
        fail("unsupported", path, unsupportedObjectType(prototype));
      const error = item as Error;
      for (const field of ["name", "message", "cause", "errors"]) {
        const descriptor = Object.getOwnPropertyDescriptor(error, field);
        if (descriptor && !Object.hasOwn(descriptor, "value"))
          fail("unsupported", `${path}.${field}`, "accessor");
      }
      if (typeof error.name !== "string" || typeof error.message !== "string")
        fail("unsupported", path, "Error-metadata");
      const base = {
        name: string(error.name, path),
        message: string(error.message, path),
      };
      const node: Extract<CloneGraphNode, { type: "error" | "domexception" }> =
        isDomException
          ? { type: "domexception", ...base }
          : { type: "error", errorType: errorType!, ...base };
      if (Object.hasOwn(error, "stack")) {
        // V8 exposes native Error stacks through a lazy own accessor.
        if (typeof error.stack !== "string")
          fail("unsupported", `${path}.stack`, "Error.stack");
        node.stack = string(error.stack, path);
      }
      if (node.type === "error") {
        if (Object.hasOwn(error, "cause"))
          node.cause = add(error.cause, `${path}.cause`);
        if (errorType === "AggregateError")
          node.errors = add((error as AggregateError).errors, `${path}.errors`);
      }
      nodes.push(node);
    }
  }
  const graph: CloneGraph = { version: CLONE_GRAPH_VERSION, root, nodes };
  validateCloneGraph(graph, limits);
  for (const { node, body, context } of binaries) {
    const reference = await binarySink(body, context);
    if (!referenceValid(reference))
      fail("invalid", context.location, "binary-reference");
    node.reference = reference;
  }
  return graph;
}

function define(
  target: object,
  key: string,
  value: unknown,
  enumerable = true,
): void {
  Object.defineProperty(target, key, {
    value,
    enumerable,
    configurable: true,
    writable: true,
  });
}

export async function decodeCloneGraph(
  input: unknown,
  binarySource: CloneBinarySource,
  overrides: Partial<CloneGraphLimits> = {},
): Promise<unknown> {
  validateCloneGraph(input, overrides);
  const graph = input;
  const values: unknown[] = new Array(graph.nodes.length);
  for (let i = 0; i < graph.nodes.length; i++) {
    const node = graph.nodes[i];
    const path = `$.nodes[${i}]`;
    switch (node.type) {
      case "null":
        values[i] = null;
        break;
      case "undefined":
        values[i] = undefined;
        break;
      case "boolean":
        values[i] = node.value;
        break;
      case "string":
        values[i] = decodeUtf16(node.value);
        break;
      case "bigint":
        values[i] = BigInt(node.value);
        break;
      case "number":
        values[i] = decodeNumber(node.value);
        break;
      case "date":
        values[i] = new Date(decodeNumber(node.value));
        break;
      case "object":
        values[i] = node.prototype === "null" ? Object.create(null) : {};
        break;
      case "array":
        values[i] = new Array(node.length);
        break;
      case "map":
        values[i] = new Map();
        break;
      case "set":
        values[i] = new Set();
        break;
      case "view":
        break;
      case "regexp": {
        const value = new RegExp(decodeUtf16(node.source), node.flags);
        value.lastIndex = decodeNumber(node.lastIndex);
        values[i] = value;
        break;
      }
      case "buffer":
      case "blob":
      case "file": {
        const body = await binarySource(node.reference, {
          location: path,
          byteLength: node.byteLength,
        });
        const blob = typeof Blob !== "undefined" && body instanceof Blob;
        if (
          !(body instanceof ArrayBuffer) &&
          !(body instanceof Uint8Array) &&
          !blob
        )
          fail("binary", path, "body-type");
        if (
          (blob
            ? (body as Blob).size
            : (body as ArrayBuffer | Uint8Array).byteLength) !== node.byteLength
        )
          fail("binary", path, "byte-length");
        if (node.type === "buffer") {
          // A distinct node must own a distinct buffer, even when content hashes match.
          values[i] = blob
            ? await (body as Blob).arrayBuffer()
            : body instanceof ArrayBuffer
              ? body.slice(0)
              : (body as Uint8Array).slice().buffer;
        } else {
          const part: BlobPart =
            body instanceof Uint8Array
              ? body.slice().buffer
              : (body as Blob | ArrayBuffer);
          const type = decodeUtf16(node.mimeType);
          if (node.type === "file") {
            if (typeof File === "undefined") fail("unsupported", path, "File");
            values[i] = new File([part], decodeUtf16(node.name), {
              type,
              lastModified: node.lastModified,
            });
          } else {
            if (typeof Blob === "undefined") fail("unsupported", path, "Blob");
            values[i] = new Blob([part], { type });
          }
        }
        break;
      }
      case "error":
      case "domexception": {
        let error: Error | DOMException;
        if (node.type === "domexception") {
          if (typeof DOMException === "undefined")
            fail("unsupported", path, "DOMException");
          error = new DOMException(
            decodeUtf16(node.message),
            decodeUtf16(node.name),
          );
        } else {
          const Constructor = globalThis[node.errorType];
          if (typeof Constructor !== "function")
            fail("unsupported", path, "Error");
          error =
            node.errorType === "AggregateError"
              ? new AggregateError([])
              : new (Constructor as ErrorConstructor)();
          define(error, "name", decodeUtf16(node.name), false);
          define(error, "message", decodeUtf16(node.message), false);
        }
        if (node.stack !== undefined)
          define(error, "stack", decodeUtf16(node.stack), false);
        else delete error.stack;
        values[i] = error;
        break;
      }
    }
  }
  // Buffers exist before constructing any views, including forward references.
  for (let i = 0; i < graph.nodes.length; i++) {
    const node = graph.nodes[i];
    if (node.type !== "view") continue;
    const Constructor = globalThis[node.viewType];
    if (typeof Constructor !== "function")
      fail("unsupported", `$.nodes[${i}]`, "ArrayBufferView");
    values[i] = new (Constructor as typeof Uint8Array)(
      values[node.buffer] as ArrayBuffer,
      node.byteOffset,
      node.length,
    );
  }
  for (let i = 0; i < graph.nodes.length; i++) {
    const node = graph.nodes[i];
    const target = values[i];
    switch (node.type) {
      case "object":
      case "array":
        for (const [key, ref] of node.properties)
          define(target as object, decodeUtf16(key), values[ref]);
        break;
      case "map": {
        const map = target as Map<unknown, unknown>;
        for (const [key, value] of node.entries)
          map.set(values[key], values[value]);
        if (map.size !== node.entries.length)
          fail("invalid", `$.nodes[${i}]`, "duplicate-map-key");
        break;
      }
      case "set": {
        const set = target as Set<unknown>;
        for (const ref of node.entries) set.add(values[ref]);
        if (set.size !== node.entries.length)
          fail("invalid", `$.nodes[${i}]`, "duplicate-set-entry");
        break;
      }
      case "error":
        if (node.cause !== undefined)
          define(target as object, "cause", values[node.cause], false);
        if (node.errors !== undefined)
          define(target as object, "errors", values[node.errors], false);
        break;
    }
  }
  return values[graph.root];
}
