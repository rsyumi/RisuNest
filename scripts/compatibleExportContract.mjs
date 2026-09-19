/** Test/build-time contract generator. Never imported by the product runtime. */
import fs from "node:fs";
import assert from "node:assert/strict";
import path from "node:path";
import crypto from "node:crypto";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";

const require = createRequire(import.meta.url);
const ts = require("typescript");
export const contractDirectory = fileURLToPath(
  new URL(
    "../src-tauri/src/native_file_jobs/legacy_backup/compatibility_contracts/",
    import.meta.url,
  ),
);
export const references = {
  risuai: {
    directory:
      process.env.RISUAI_REFERENCE ?? fileURLToPath(new URL("../.tmp/test-references/risuai/", import.meta.url)),
    revision: "c454df882aaf32e02a22da26d3718c8cadc97814",
  },
  pocket: {
    directory:
      process.env.POCKETRISU_REFERENCE ?? fileURLToPath(new URL("../.tmp/test-references/pocket/", import.meta.url)),
    revision: "b315d898abd543fffaf5346d8eb3246b20da92cd",
  },
};
export const sha256 = (value) =>
  crypto.createHash("sha256").update(value).digest("hex");

// Approved source pins may have either checkout newline style. Only text line
// endings are interchangeable; revisions, source contents and binary hashes are not.
export function matchesSourcePin(bytes, expected) {
  const text = Buffer.from(bytes).toString("utf8");
  return [bytes, text.replace(/\r\n/g, "\n"), text.replace(/\r?\n/g, "\r\n")]
    .some(value => sha256(value) === expected);
}

export function verifySourceFilePins(target, actualFiles, pins) {
  assert.deepEqual([...actualFiles].sort(), Object.keys(pins).sort(), `${target} source dependency drift`);
  for (const [file, expected] of Object.entries(pins)) {
    if (!matchesSourcePin(fs.readFileSync(path.join(references[target].directory, file)), expected)) {
      throw new Error(`${target} reference drift: ${file}`);
    }
  }
}

export function verifyGeneratedContract(actual, expected) {
  const shape = contract => ({
    ...contract,
    reference: { ...contract.reference, files: Object.keys(contract.reference.files).sort() },
  });
  assert.deepEqual(shape(actual), shape(expected), `${actual.target} generated contract drift`);
  verifySourceFilePins(actual.target, Object.keys(actual.reference.files), expected.reference.files);
}

export function generateContract(target) {
  const reference = references[target];
  if (!reference) throw new Error(`Unknown target: ${target}`);
  const rootPath = path.resolve(reference.directory);
  const revision = execFileSync(
    "git",
    [
      "-c",
      `safe.directory=${rootPath.replaceAll("\\", "/")}`,
      "-C",
      rootPath,
      "rev-parse",
      "HEAD",
    ],
    { encoding: "utf8", windowsHide: true },
  ).trim();
  if (revision !== reference.revision)
    throw new Error(
      `${target} reference revision differs from the approved contract`,
    );
  const databasePath = path.join(rootPath, "src/ts/storage/database.svelte.ts");
  const program = ts.createProgram([databasePath], {
    target: ts.ScriptTarget.ESNext,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    baseUrl: rootPath,
    strictNullChecks: true,
    skipLibCheck: true,
    allowJs: true,
    noEmit: true,
  });
  const checker = program.getTypeChecker();
  const source = program.getSourceFile(databasePath);
  const nodes = {};
  const types = {};
  const seen = new Map();
  const files = new Map();
  const pin = (declaration) => {
    const file = declaration.getSourceFile();
    const relative = path
      .relative(rootPath, file.fileName)
      .replaceAll("\\", "/");
    if (!relative.startsWith("../") && !relative.includes("node_modules/"))
      files.set(relative, sha256(fs.readFileSync(file.fileName)));
  };
  function visit(type) {
    if (seen.has(type)) return seen.get(type);
    const id = `n${seen.size}`;
    seen.set(type, id);
    nodes[id] = {};
    for (const declaration of type.aliasSymbol?.declarations ??
      type.symbol?.declarations ??
      [])
      pin(declaration);
    const flags = type.flags;
    let node;
    if (flags & ts.TypeFlags.Any) {
      if (type.intrinsicName === "error")
        throw new Error(`Unresolved type: ${checker.typeToString(type)}`);
      node = {
        kind: "any",
        reason: "Reference explicitly permits arbitrary JSON (any).",
      };
    } else if (flags & ts.TypeFlags.Unknown)
      node = {
        kind: "any",
        reason: "Reference explicitly permits unknown JSON.",
      };
    else if (flags & ts.TypeFlags.Never) node = { kind: "never" };
    else if (flags & ts.TypeFlags.Undefined) node = { kind: "never" };
    else if (flags & ts.TypeFlags.Null) node = { kind: "scalar", type: "null" };
    else if (flags & ts.TypeFlags.StringLiteral)
      node = { kind: "literal", value: type.value };
    else if (flags & ts.TypeFlags.NumberLiteral)
      node = { kind: "literal", value: type.value };
    else if (flags & ts.TypeFlags.BooleanLiteral)
      node = { kind: "literal", value: type.intrinsicName === "true" };
    else if (flags & ts.TypeFlags.String)
      node = { kind: "scalar", type: "string" };
    else if (flags & ts.TypeFlags.Number)
      node = { kind: "scalar", type: "number" };
    else if (flags & ts.TypeFlags.Boolean)
      node = { kind: "scalar", type: "boolean" };
    else if (type.isUnion())
      node = {
        kind: "union",
        variants: type.types
          .filter((t) => !(t.flags & ts.TypeFlags.Undefined))
          .map(visit),
      };
    else if (checker.isTupleType(type)) {
      node = {
        kind: "tuple",
        items: checker.getTypeArguments(type).map(visit),
        optional: type.target.elementFlags.map((f) =>
          Boolean(f & ts.ElementFlags.Optional),
        ),
      };
      if (
        type.target.elementFlags.some(
          (f) => f & (ts.ElementFlags.Rest | ts.ElementFlags.Variadic),
        )
      )
        throw new Error("Variable tuple needs explicit wire semantics");
    } else if (checker.isArrayType(type))
      node = { kind: "array", items: visit(checker.getTypeArguments(type)[0]) };
    else if (flags & ts.TypeFlags.Object || type.isIntersection()) {
      if (type.getCallSignatures().length) node = { kind: "never" };
      else {
        const fields = {};
        for (const property of checker
          .getPropertiesOfType(type)
          .sort((a, b) => a.name.localeCompare(b.name, "en"))) {
          const declaration =
            property.valueDeclaration ?? property.declarations?.[0];
          if (declaration) pin(declaration);
          // Mapped types (for example Partial<Record<Emotion, string>>) have
          // synthetic property symbols rather than individual declarations.
          fields[property.name] = {
            node: visit(
              declaration
                ? checker.getTypeOfSymbolAtLocation(property, declaration)
                : checker.getTypeOfSymbol(property),
            ),
            optional: Boolean(property.flags & ts.SymbolFlags.Optional),
          };
        }
        const index =
          checker.getIndexTypeOfType(type, ts.IndexKind.String) ??
          checker.getIndexTypeOfType(type, ts.IndexKind.Number);
        node = {
          kind: "object",
          fields,
          additional: index ? visit(index) : null,
        };
      }
    } else
      throw new Error(
        `Unsupported type ${checker.typeToString(type)} (${flags})`,
      );
    nodes[id] = node;
    return id;
  }
  const declarations = source.statements.filter(
    (s) => ts.isInterfaceDeclaration(s) || ts.isTypeAliasDeclaration(s),
  );
  const rootDeclaration = declarations.find((s) => s.name.text === "Database");
  const root = visit(checker.getTypeAtLocation(rootDeclaration));
  for (const declaration of declarations)
    types[declaration.name.text] = visit(
      checker.getTypeAtLocation(declaration),
    );
  types.DataBase = root;
  for (const name of [
    "src/ts/storage/risuSave.ts",
    "src/ts/bootstrap.ts",
    "src/ts/drive/backuplocal.ts",
    ...(target === "pocket"
      ? ["server/node/utils.cjs", "server/node/server.cjs"]
      : []),
  ]) {
    files.set(name, sha256(fs.readFileSync(path.join(rootPath, name))));
  }
  return {
    version: 1,
    target,
    reference: {
      revision: reference.revision,
      files: Object.fromEntries(
        [...files].sort(([a], [b]) => a.localeCompare(b, "en")),
      ),
    },
    root,
    types,
    nodes,
  };
}

export function verifySourcePins(contract) {
  verifySourceFilePins(contract.target, Object.keys(contract.reference.files), contract.reference.files);
}

// Present-field acceptance oracle independent of the native projector. Omitted
// fields are handled separately by the actual reference bootstrap/defaults.
export function acceptsContract(contract, id, value) {
  const node = contract.nodes[id];
  switch (node.kind) {
    case "any":
      return true;
    case "never":
      return false;
    case "literal":
      return value === node.value;
    case "scalar":
      return node.type === "null" ? value === null : typeof value === node.type;
    case "union":
      return node.variants.some((variant) =>
        acceptsContract(contract, variant, value),
      );
    case "array":
      return (
        Array.isArray(value) &&
        value.every((item) => acceptsContract(contract, node.items, item))
      );
    case "tuple":
      return (
        Array.isArray(value) &&
        value.length <= node.items.length &&
        node.items.every((item, index) =>
          index < value.length
            ? acceptsContract(contract, item, value[index])
            : node.optional[index],
        )
      );
    case "object":
      return (
        value !== null &&
        !Array.isArray(value) &&
        typeof value === "object" &&
        Object.entries(value).every(([key, item]) => {
          const child = Object.hasOwn(node.fields, key)
            ? node.fields[key].node
            : node.additional;
          return (
            child !== null &&
            child !== undefined &&
            acceptsContract(contract, child, item)
          );
        })
      );
    default:
      throw new Error(`Unknown contract node ${node.kind}`);
  }
}

if (
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  const check = process.argv.includes("--check");
  fs.mkdirSync(contractDirectory, { recursive: true });
  for (const target of Object.keys(references)) {
    const contract = generateContract(target);
    const file = path.join(contractDirectory, `${target}.json`);
    if (check) {
      const committed = JSON.parse(fs.readFileSync(file, "utf8"));
      verifyGeneratedContract(contract, committed);
    } else fs.writeFileSync(file, JSON.stringify(contract, null, 2) + "\n");
    console.log(
      `${target}: ${Object.keys(contract.nodes).length} nodes, ${Object.keys(contract.reference.files).length} pinned sources`,
    );
  }
}
