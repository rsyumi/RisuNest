/** Execute pinned reference declarations without starting either app or its services.
 * Source bodies are read from the reference checkout, never a copied decoder.
 * UI/platform boundaries are explicit; unexpected I/O fails closed.
 */
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { createPocketTransactionHarness } from "./compatibleExportPocketTransaction.mjs";
import {
  references,
  sha256,
  verifySourceFilePins,
  contractDirectory,
  acceptsContract,
  verifySourcePins,
} from "./compatibleExportContract.mjs";
const require = createRequire(import.meta.url);
const ts = require("typescript");
// Match the reference web client's no-eval/browser path. Exposing Node Buffer
// to msgpackr 1.10.1 selects its Node utf8Write fast path, which fails for large
// strings on Node 24 and is not the path either reference web client uses.
function browserMessagePack(referenceRequire) {
  const msgpackExports = {};
  const msgpackContext = vm.createContext({
    exports: msgpackExports,
    module: { exports: msgpackExports },
    TextEncoder,
    TextDecoder,
    Uint8Array,
    ArrayBuffer,
    DataView,
  });
  vm.runInContext(
    fs.readFileSync(referenceRequire.resolve("msgpackr/index-no-eval"), "utf8"),
    msgpackContext,
  );
  return msgpackExports;
}

export function createReferenceHarness(target) {
  const directory = path.resolve(references[target].directory);
  const referenceRequire = createRequire(path.join(directory, "package.json"));
  const msgpackr = browserMessagePack(referenceRequire);
  const fflate = referenceRequire("fflate");
  const transaction =
    target === "pocket" ? createPocketTransactionHarness(directory) : null;
  let lastSourceDatabase = null;
  const entryPaths = [
    "storage/risuSave.ts",
    "storage/database.svelte.ts",
    "bootstrap.ts",
    "globalApi.svelte.ts",
    ...(target === "risuai" ? ["drive/backuplocal.ts"] : []),
  ].map((p) => path.join(directory, "src/ts", p));
  if (target === "pocket")
    entryPaths.push(path.join(directory, "server/node/server.cjs"));
  const program = ts.createProgram(entryPaths, {
    target: ts.ScriptTarget.ESNext,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    baseUrl: directory,
    allowJs: true,
    skipLibCheck: true,
  });
  const checker = program.getTypeChecker();
  const forbidden = () => {
    throw new Error(
      "Reference harness attempted external I/O or an unsupported side effect",
    );
  };
  const io = new Proxy({}, { get: () => forbidden });
  const state = { db: {} };
  const memoryStorage = new Map();
  const storage = {
    isAccount: false,
    getItem: async (key) => memoryStorage.get(key) ?? null,
    setItem: async (key, value) => {
      memoryStorage.set(key, value);
    },
    keys: async () => [...memoryStorage.keys()],
  };
  storage.realStorage = storage;
  const inlayDir = path.resolve(directory, "__synthetic_in_memory_inlays__");
  const inlayKey = (file) => {
    if (path.dirname(file) !== inlayDir) return forbidden();
    const name = path.basename(file);
    return name.endsWith(".meta.json")
      ? `inlay_sidecar/${name.slice(0, -10)}`
      : `inlay/${name}`;
  };
  const inlayFs = {
    async readFile(file, encoding) {
      const value = memoryStorage.get(inlayKey(file));
      if (!value) throw new Error("Synthetic inlay missing");
      return encoding
        ? Buffer.from(value).toString(encoding)
        : Buffer.from(value);
    },
    async access(file) {
      if (!memoryStorage.has(inlayKey(file)))
        throw new Error("Synthetic inlay missing");
    },
    async stat(file) {
      if (!memoryStorage.has(inlayKey(file)))
        throw new Error("Synthetic inlay missing");
      return { mtimeMs: 0 };
    },
    async readdir(dir) {
      if (dir !== inlayDir) return forbidden();
      return [...memoryStorage.keys()]
        .filter((key) => key.startsWith("inlay/"))
        .map((key) => ({ name: key.slice(6), isFile: () => true }));
    },
  };
  let input;
  let upload;
  const bundledSources = [];
  const readGlob = (folder) => {
    const root = path.join(directory, "src/ts/preset/registry/bundled", folder);
    const result = {};
    if (!fs.existsSync(root)) return result;
    const walk = (dir) => {
      for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
        const file = path.join(dir, entry.name);
        if (entry.isDirectory()) walk(file);
        else if (entry.name.endsWith(".json")) {
          bundledSources.push(
            path.relative(directory, file).replaceAll("\\", "/"),
          );
          result[file] = JSON.parse(fs.readFileSync(file, "utf8"));
        }
      }
    };
    walk(root);
    return result;
  };
  const context = vm.createContext({
    Buffer,
    Uint8Array,
    ArrayBuffer,
    TextEncoder,
    TextDecoder,
    CompressionStream,
    DecompressionStream,
    TransformStream,
    Response,
    structuredClone,
    console: { log() {}, warn() {}, error: forbidden },
    fetch: forbidden,
    setTimeout: forbidden,
    setInterval: forbidden,
    ...msgpackr,
    fflate,
    rfdc: require("rfdc"),
    DBState: state,
    $state: { snapshot: structuredClone },
    fflateCompress: fflate.compress,
    fflateDecompress: fflate.decompress,
    isTauri: false,
    isNodeServer: true,
    uuidv4: () => "00000000-0000-4000-8000-000000000001",
    changeLanguage() {},
    forageStorage: storage,
    localforage: { createInstance: () => storage },
    // These are UI/cache effects, not database normalization. No real drafts,
    // storage, network, language bundles or app data are loaded in this harness.
    sweepOrphanDrafts: async () => {},
    writeFile: forbidden,
    exists: forbidden,
    mkdir: forbidden,
    readFile: forbidden,
    BaseDirectory: {},
    alertError: forbidden,
    alertConfirm: forbidden,
    alertMd: forbidden,
    waitAlert: forbidden,
    migrateLegacyTrash: forbidden,
    // Vite's eager JSON imports, using the real bundled reference definitions.
    baseProviderModules: readGlob("base-providers"),
    profileModules: readGlob("profiles"),
    document: {
      createElement: (kind) => {
        if (kind !== "input") return forbidden();
        input = {
          remove() {},
          click() {
            upload = input.onchange();
          },
        };
        return input;
      },
    },
    location: { search: "", origin: "https://synthetic.invalid" },
    alertStore: { set() {} },
    alertWait() {},
    alertNormal() {},
    sleep: async () => {},
    requiresFullEncoderReload: { state: false },
    relaunch: forbidden,
    appDataDir: forbidden,
    join: path.join,
    hubURL: "https://synthetic.invalid",
    realmHubURL: "https://synthetic.invalid",
    path,
    zlib: require("node:zlib"),
    fs: inlayFs,
    inlayDir,
    logger: { info() {}, warn() {}, error: forbidden },
    kvGet: (key) => memoryStorage.get(key),
    kvSet: (key, value) => memoryStorage.set(key, value),
    kvDel: (key) => memoryStorage.delete(key),
  });
  const overrides = new Set(Object.keys(context));
  for (const name of [
    "Set",
    "Map",
    "WeakSet",
    "WeakMap",
    "Object",
    "Array",
    "String",
    "Number",
    "Boolean",
    "Math",
    "Date",
    "JSON",
    "Promise",
    "RegExp",
    "Error",
    "TypeError",
    "parseInt",
    "parseFloat",
    "isNaN",
    "Infinity",
    "undefined",
  ])
    overrides.add(name);
  const selected = new Set();
  const functions = [];
  const variables = [];
  const sources = new Set([
    "package.json",
    "pnpm-lock.yaml",
    ...bundledSources,
    ...(transaction?.sources ?? []),
  ]);
  function topLevel(declaration) {
    if (
      ts.isFunctionDeclaration(declaration) ||
      ts.isClassDeclaration(declaration) ||
      ts.isEnumDeclaration(declaration)
    )
      return declaration.parent && ts.isSourceFile(declaration.parent)
        ? declaration
        : null;
    if (
      ts.isVariableDeclaration(declaration) &&
      ts.isVariableDeclarationList(declaration.parent) &&
      ts.isVariableStatement(declaration.parent.parent) &&
      ts.isSourceFile(declaration.parent.parent.parent)
    )
      return declaration;
    return null;
  }
  function include(declaration) {
    const node = topLevel(declaration);
    if (!node || selected.has(node)) return;
    const name = node.name?.getText();
    if (overrides.has(name)) return;
    const source = node.getSourceFile();
    if (
      /[/\\]typescript[/\\]lib[/\\]lib\.[^/\\]+\.d\.ts$/.test(source.fileName)
    )
      return;
    if (
      !path.resolve(source.fileName).startsWith(directory + path.sep) ||
      source.isDeclarationFile ||
      source.fileName.includes("node_modules")
    )
      throw new Error(`Unmocked external reference dependency: ${name}`);
    selected.add(node);
    sources.add(
      path.relative(directory, source.fileName).replaceAll("\\", "/"),
    );
    function visit(child) {
      if (ts.isTypeNode(child)) return;
      if (
        ts.isIdentifier(child) &&
        ((ts.isPropertyAccessExpression(child.parent) &&
          child.parent.name === child) ||
          (ts.isPropertyAssignment(child.parent) &&
            child.parent.name === child))
      )
        return;
      if (ts.isIdentifier(child) && !overrides.has(child.text)) {
        let symbol = checker.getSymbolAtLocation(child);
        if (symbol?.flags & ts.SymbolFlags.Alias)
          symbol = checker.getAliasedSymbol(symbol);
        for (const dep of symbol?.declarations ?? []) {
          const candidate = topLevel(dep);
          if (candidate && candidate !== node) include(candidate);
        }
      }
      ts.forEachChild(child, visit);
    }
    ts.forEachChild(node, visit);
    let text = node.getText(source).replace(/^export\s+/, "");
    if (ts.isVariableDeclaration(node)) {
      text = `var ${text};`;
      variables.push(text);
    } else functions.push(text);
  }
  const wanted = [
    "decodeRisuSave",
    "encodeRisuSaveLegacy",
    "setDatabase",
    "getDatabase",
    "checkNewFormat",
    "readImage",
    "loadAsset",
    ...(target === "risuai"
      ? ["LoadLocalBackup"]
      : [
          "parseBackupChunk",
          "resolveBackupStorageKey",
          "parseColdStorageJsonBuffer",
          "encodeColdStorageCanonicalBuffer",
          "restoreColdStorageCharactersInDb",
          "restoreColdStorageChat",
          "readInlayInfoPayload",
          "readInlayAssetPayload",
        ]),
  ];
  for (const name of wanted) {
    const node = entryPaths
      .flatMap((file) => program.getSourceFile(file).statements)
      .find((n) => n.name?.text === name);
    if (!node) throw new Error(`Reference declaration missing: ${name}`);
    include(node);
  }
  const source = [...functions, ...variables].join("\n");
  const javascript = ts.transpileModule(source, {
    compilerOptions: {
      target: ts.ScriptTarget.ESNext,
      module: ts.ModuleKind.None,
    },
  }).outputText;
  vm.runInContext(javascript, context, { timeout: 10000 });
  return {
    sources: [...sources].sort(),
    decode: (bytes) => context.decodeRisuSave(bytes),
    encode: (value) => context.encodeRisuSaveLegacy(value, "compression"),
    storage: memoryStorage,
    transaction,
    get lastSourceDatabase() {
      return lastSourceDatabase;
    },
    close() {
      transaction?.close();
    },
    readAsset: (key) => context.loadAsset(key),
    readImage: (key) => context.readImage(key),
    readInlay: async (id) =>
      target === "pocket"
        ? JSON.parse((await context.readInlayAssetPayload(id)).toString("utf8"))
        : forbidden(),
    readInlayInfo: async (id) =>
      target === "pocket"
        ? JSON.parse((await context.readInlayInfoPayload(id)).toString("utf8"))
        : forbidden(),
    readCold: (key) =>
      target === "risuai"
        ? context.getColdStorageItem(key)
        : context.readColdStorageJsonEntry(key).coldData,
    async importArchive(bytes) {
      if (target === "pocket") {
        const imported = await transaction.importArchive(bytes);
        lastSourceDatabase = imported.sourceDatabase;
        memoryStorage.clear();
        for (const key of transaction.db.kvList())
          memoryStorage.set(key, transaction.db.kvGet(key));
        const active = path.join(transaction.savePath, "inlays") + path.sep;
        for (const [file, payload] of transaction.files) {
          if (!file.startsWith(active)) continue;
          const name = file.slice(active.length);
          if (name.startsWith(".")) continue;
          memoryStorage.set(
            name.endsWith(".meta.json")
              ? `inlay_sidecar/${name.slice(0, -10)}`
              : `inlay/${name}`,
            payload,
          );
        }
        return imported.database;
      }
      const stream = () =>
        new ReadableStream({
          start(controller) {
            for (let offset = 0; offset < bytes.length; offset += 317)
              controller.enqueue(
                new Uint8Array(bytes.subarray(offset, offset + 317)),
              );
            controller.close();
          },
        });
      context.document.createElement = () => {
        input = {
          files: [{ size: bytes.length, stream }],
          remove() {},
          click() {
            upload = input.onchange();
          },
        };
        return input;
      };
      context.LoadLocalBackup();
      await upload;
      const database = memoryStorage.get("database/database.bin");
      if (!database)
        throw new Error("Reference importer did not activate a database");
      lastSourceDatabase = database;
      return database;
    },
    async normalize(value) {
      context.setDatabase(value);
      await context.checkNewFormat();
      return state.db;
    },
    async roundTrip(bytes) {
      const decoded = await context.decodeRisuSave(bytes);
      if (target === "pocket") {
        const restored = context.restoreColdStorageCharactersInDb(decoded);
        if (restored.failed)
          throw new Error("Pocket cold character restoration failed");
        for (const char of decoded.characters ?? [])
          for (const chat of char.chats ?? [])
            if (!context.restoreColdStorageChat(chat))
              throw new Error("Pocket cold chat restoration failed");
      }
      context.setDatabase(decoded);
      await context.checkNewFormat();
      const encoded = context.encodeRisuSaveLegacy(state.db, "compression");
      return {
        decoded,
        normalized: state.db,
        encoded,
        resaved: await context.decodeRisuSave(encoded),
      };
    },
  };
}

export function referenceRuntimePins(target, harness) {
  return Object.fromEntries(
    harness.sources.map((file) => [
      file,
      sha256(fs.readFileSync(path.join(references[target].directory, file))),
    ]),
  );
}

export function verifyReferenceRuntimePins(target, harness, expected) {
  verifySourceFilePins(target, harness.sources, expected);
}

/** Synthetic acceptance only: compare declared local asset references before
 * and after the target save, then read their bytes through actual target APIs.
 * This deliberately never searches prompts, scripts, plugin data or strings.
 */
export async function verifyRoundTripAssets(harness, source, resaved) {
  function collect(value, location = "$", out = new Map()) {
    if (!value || typeof value !== "object" || Array.isArray(value)) return out;
    const add = (item, at) => {
      if (typeof item === "string" && item.startsWith("assets/"))
        out.set(at, item);
    };
    for (const key of [
      "image",
      "icon",
      "customBackground",
      "userIcon",
      "img",
      "imgFile",
    ])
      add(value[key], `${location}.${key}`);
    for (const key of ["additionalAssets", "emotionImages", "assets"])
      if (Array.isArray(value[key]))
        value[key].forEach((tuple, index) =>
          add(tuple?.[1], `${location}.${key}[${index}][1]`),
        );
    if (Array.isArray(value.ccAssets))
      value.ccAssets.forEach((asset, index) =>
        add(asset?.uri, `${location}.ccAssets[${index}].uri`),
      );
    if (value.vits?.files && typeof value.vits.files === "object")
      for (const [key, item] of Object.entries(value.vits.files))
        add(item, `${location}.vits.files.${key}`);
    for (const key of [
      "characters",
      "botPresets",
      "modules",
      "personas",
      "characterOrder",
    ])
      if (Array.isArray(value[key]))
        value[key].forEach((child, index) =>
          collect(child, `${location}.${key}[${index}]`, out),
        );
    if (value.embeddedModule)
      collect(value.embeddedModule, `${location}.embeddedModule`, out);
    return out;
  }
  const before = collect(source);
  const after = collect(resaved);
  const assets = new Set();
  let bytes = 0;
  for (const [location, key] of before) {
    if (after.get(location) !== key)
      throw new Error(
        `Target re-save changed a declared synthetic asset reference at ${location}`,
      );
    const expected = harness.storage.get(key);
    if (!expected)
      throw new Error(
        `Imported synthetic asset bytes are missing at ${location}`,
      );
    const expectedHash = sha256(expected);
    const asset = await harness.readAsset(key);
    const image = await harness.readImage(key);
    if (
      !asset ||
      !image ||
      sha256(asset) !== expectedHash ||
      sha256(image) !== expectedHash
    )
      throw new Error(`Actual target loader bytes differ at ${location}`);
    if (!assets.has(key)) {
      assets.add(key);
      bytes += expected.length;
    }
  }
  // Also check target-added local references, if any. They must resolve, though
  // they need not have a source counterpart when supplied by target defaults.
  for (const [location, key] of after)
    if (!before.has(location)) {
      const expected = harness.storage.get(key);
      const asset = await harness.readAsset(key);
      if (!expected || !asset || sha256(expected) !== sha256(asset))
        throw new Error(
          `Target-added synthetic asset is unavailable at ${location}`,
        );
    }
  return { references: before.size, distinctAssets: assets.size, bytes };
}

/** Run the actual Nest import normalizer after a reference Pocket edit/re-save. */
export function createNestReimportHarness() {
  const select = (relative, names) => {
    const file = fileURLToPath(new URL(relative, import.meta.url));
    const source = ts.createSourceFile(
      file,
      fs.readFileSync(file, "utf8"),
      ts.ScriptTarget.Latest,
      true,
    );
    return source.statements
      .filter((statement) => {
        if (ts.isImportDeclaration(statement)) return false;
        if (!names) return true;
        if (statement.name && names.includes(statement.name.text)) return true;
        return (
          ts.isVariableStatement(statement) &&
          statement.declarationList.declarations.some((declaration) =>
            names.includes(declaration.name.getText()),
          )
        );
      })
      .map((statement) => statement.getText(source))
      .join("\n");
  };
  const source = [
    select("../src/ts/polyfill.ts", ["rfdcClone", "safeStructuredClone"]),
    select("../src/ts/responseVariants.ts", ["snapshotResponse"]),
    select("../src/ts/drive/pocketRisuFeatures.ts"),
  ].join("\n");
  const javascript = ts.transpileModule(source, {
    compilerOptions: {
      target: ts.ScriptTarget.ESNext,
      module: ts.ModuleKind.CommonJS,
    },
  }).outputText;
  const context = vm.createContext({
    structuredClone,
    rfdc: require("rfdc"),
    exports: {},
  });
  vm.runInContext(javascript, context, { timeout: 1000 });
  return (value) => context.exports.normalizePocketFeatures(value);
}

if (
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  if (process.argv.includes("--archive")) {
    const archive = process.argv[process.argv.indexOf("--archive") + 1];
    const target = process.argv[process.argv.indexOf("--target") + 1];
    if (!archive || !references[target])
      throw new Error(
        "Usage: --archive <synthetic native output.bin> --target risuai|pocket",
      );
    // This CLI is exclusively for synthetic Rust acceptance fixtures. Never pass
    // an installed database, real backup, or a copy of user data here.
    const harness = createReferenceHarness(target);
    const contract = JSON.parse(
      fs.readFileSync(path.join(contractDirectory, `${target}.json`), "utf8"),
    );
    verifySourcePins(contract);
    const bytes = fs.readFileSync(archive);
    const database = await harness.importArchive(bytes);
    if (
      !acceptsContract(
        contract,
        contract.root,
        await harness.decode(harness.lastSourceDatabase ?? database),
      )
    )
      throw new Error(
        "Native synthetic output violates the recursive target schema",
      );
    const result = await harness.roundTrip(database);
    const assetVerification = await verifyRoundTripAssets(
      harness,
      await harness.decode(harness.lastSourceDatabase ?? database),
      result.resaved,
    );
    const characters = result.resaved.characters ?? [];
    console.log(
      JSON.stringify({
        target,
        archiveBytes: bytes.length,
        storageEntries: harness.storage.size,
        assetVerification,
        characters: characters.length,
        chats: characters.reduce(
          (sum, char) => sum + (char.chats?.length ?? 0),
          0,
        ),
        messages: characters.reduce(
          (sum, char) =>
            sum +
            (char.chats ?? []).reduce(
              (n, chat) => n + (chat.message?.length ?? 0),
              0,
            ),
          0,
        ),
        resavedBytes: result.encoded.length,
        resavedSha256: sha256(result.encoded),
      }),
    );
    process.exit(0);
  }
  const pins = {};
  for (const target of Object.keys(references)) {
    const harness = createReferenceHarness(target);
    pins[target] = referenceRuntimePins(target, harness);
    console.log(
      `${target}: ${Object.keys(pins[target]).length} runtime source files`,
    );
  }
  const file = path.join(contractDirectory, "runtime.json");
  if (process.argv.includes("--check")) {
    if (
      JSON.stringify(JSON.parse(fs.readFileSync(file, "utf8"))) !==
      JSON.stringify(pins)
    )
      throw new Error("Reference runtime source drift");
  } else fs.writeFileSync(file, JSON.stringify(pins, null, 2) + "\n");
}
