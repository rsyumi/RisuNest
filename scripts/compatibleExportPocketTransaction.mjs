/** Actual Pocket server importer with real in-memory SQLite and virtual files. */
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import crypto from "node:crypto";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const ts = require("typescript");

export function createPocketTransactionHarness(directory) {
  const referenceRequire = createRequire(path.join(directory, "package.json"));
  const sources = new Set([
    "server/node/server.cjs",
    "package.json",
    "pnpm-lock.yaml",
  ]);
  const rejected = () => {
    throw new Error(
      "Pocket transaction harness forbids network and unmodeled I/O",
    );
  };
  const log = { info() {}, warn() {}, error: rejected };
  const savePath = path.resolve(directory, "__synthetic_virtual_server__/save");
  const files = new Map();
  const dirs = new Set([savePath]);
  const operations = [];
  let failRename = false;
  const checked = (file) => {
    const resolved = path.resolve(file);
    if (resolved !== savePath && !resolved.startsWith(savePath + path.sep))
      return rejected();
    return resolved;
  };
  const virtualFs = {
    existsSync(file) {
      file = checked(file);
      return files.has(file) || dirs.has(file);
    },
    mkdirSync(file) {
      dirs.add(checked(file));
    },
    writeFileSync(file, value) {
      file = checked(file);
      if (!dirs.has(path.dirname(file)))
        throw new Error("Missing virtual parent");
      files.set(file, Buffer.from(value));
      operations.push("write");
    },
    readFileSync(file, encoding) {
      const value = files.get(checked(file));
      if (!value) throw new Error("Missing virtual file");
      return encoding ? value.toString(encoding) : Buffer.from(value);
    },
    readdirSync(dir, options = {}) {
      dir = checked(dir);
      return [...files.keys()]
        .filter((file) => path.dirname(file) === dir)
        .map((file) =>
          options.withFileTypes
            ? {
                name: path.basename(file),
                isFile: () => true,
                isDirectory: () => false,
              }
            : path.basename(file),
        );
    },
    statSync(file) {
      const value = files.get(checked(file));
      if (!value) throw new Error("Missing virtual file");
      return { mtimeMs: 0, size: value.length };
    },
    unlinkSync(file) {
      files.delete(checked(file));
    },
    async mkdir(file) {
      virtualFs.mkdirSync(file);
    },
    async writeFile(file, value) {
      virtualFs.writeFileSync(file, value);
    },
    async readFile(file, encoding) {
      return virtualFs.readFileSync(file, encoding);
    },
    async readdir(dir, options) {
      return virtualFs.readdirSync(dir, options);
    },
    async stat(file) {
      return virtualFs.statSync(file);
    },
    async access(file) {
      if (!virtualFs.existsSync(file)) throw new Error("Missing virtual file");
    },
    async rm(dir) {
      dir = checked(dir);
      for (const file of files.keys())
        if (file === dir || file.startsWith(dir + path.sep)) files.delete(file);
      for (const file of dirs)
        if (file === dir || file.startsWith(dir + path.sep)) dirs.delete(file);
      operations.push("remove");
    },
    async rename(from, to) {
      from = checked(from);
      to = checked(to);
      if (failRename && from.endsWith("inlays_import_staging")) {
        failRename = false;
        throw new Error("Injected staging swap failure");
      }
      if (!virtualFs.existsSync(from))
        throw new Error("Missing virtual rename source");
      for (const [file, bytes] of [...files])
        if (file === from || file.startsWith(from + path.sep)) {
          files.delete(file);
          files.set(to + file.slice(from.length), bytes);
        }
      for (const dir of [...dirs])
        if (dir === from || dir.startsWith(from + path.sep)) {
          dirs.delete(dir);
          dirs.add(to + dir.slice(from.length));
        }
      operations.push("rename");
    },
  };
  const modules = new Map();
  function load(relative, injections = {}) {
    if (modules.has(relative)) return modules.get(relative);
    sources.add(relative);
    const module = { exports: {} };
    modules.set(relative, module.exports);
    const context = vm.createContext({
      Buffer,
      Uint8Array,
      TextEncoder,
      TextDecoder,
      CompressionStream,
      DecompressionStream,
      TransformStream,
      Response,
      console: { log() {}, warn() {}, error: rejected },
      module,
      exports: module.exports,
      process: { cwd: () => path.dirname(savePath), env: {} },
      require: (name) => {
        if (Object.hasOwn(injections, name)) return injections[name];
        if (["crypto", "node:crypto"].includes(name)) return crypto;
        if (name === "path") return path;
        if (name === "fs") return virtualFs;
        if (name === "fs/promises") return virtualFs;
        if (name === "msgpackr" || name === "fflate")
          return referenceRequire(name);
        if (name === "./logs.cjs") return { logger: log };
        if (name.startsWith("./"))
          return load(path.posix.join(path.posix.dirname(relative), name));
        return rejected();
      },
      fetch: rejected,
      setTimeout: rejected,
      setInterval: rejected,
    });
    vm.runInContext(
      fs.readFileSync(path.join(directory, relative), "utf8"),
      context,
      { timeout: 10000 },
    );
    modules.set(relative, module.exports);
    return module.exports;
  }
  const Database = referenceRequire("better-sqlite3");
  const dbModule = load("server/node/db.cjs", {
    "better-sqlite3": function (databasePath) {
      if (checked(databasePath) !== path.join(savePath, "risuai.db"))
        return rejected();
      return new Database(":memory:");
    },
  });
  const utils = load("server/node/utils.cjs");
  const pluginModule = load("server/node/plugin-storage-store.cjs");
  const pluginStorage = {
    PREFIX: pluginModule.PREFIX,
    ...pluginModule.createPluginStorageStore(dbModule),
  };
  const context = vm.createContext({
    Buffer,
    path,
    fs: virtualFs,
    ...virtualFs,
    ...utils,
    ...dbModule,
    sqliteDb: dbModule.db,
    pluginStorage,
    logger: log,
    nodeCrypto: crypto,
    zlib: require("node:zlib"),
    savePath,
    inlayDir: path.join(savePath, "inlays"),
    process: { env: {}, cwd: () => path.dirname(savePath) },
    fetch: rejected,
    setTimeout: rejected,
    setInterval: rejected,
    clearTimeout: rejected,
    console: { log() {}, warn() {}, error: rejected },
    // There are no pending live-client writes in an isolated import fixture.
    // The actual flush function still executes, and entering its persistence
    // branches is a hard failure rather than a pretend successful flush.
    persistDbCacheWithChats: rejected,
    hydrateDatabaseForDisk: rejected,
    stripDatabaseForClient: rejected,
    recordPersistFailure: rejected,
  });
  const entry = path.join(directory, "server/node/server.cjs");
  const program = ts.createProgram([entry], {
    allowJs: true,
    noResolve: true,
    target: ts.ScriptTarget.ESNext,
  });
  const checker = program.getTypeChecker();
  const source = program.getSourceFile(entry);
  const selected = new Set();
  const functions = [];
  const variables = [];
  function top(node) {
    if (ts.isFunctionDeclaration(node) && node.parent === source) return node;
    if (
      ts.isVariableDeclaration(node) &&
      node.parent?.parent?.parent === source &&
      ts.isIdentifier(node.name)
    )
      return node;
    return null;
  }
  function include(node) {
    node = top(node);
    if (!node || selected.has(node) || Object.hasOwn(context, node.name.text))
      return;
    selected.add(node);
    function visit(child) {
      if (ts.isIdentifier(child) && !Object.hasOwn(context, child.text)) {
        for (const declaration of checker.getSymbolAtLocation(child)
          ?.declarations ?? []) {
          const dep = top(declaration);
          if (dep && dep !== node) include(dep);
        }
      }
      ts.forEachChild(child, visit);
    }
    ts.forEachChild(node, visit);
    if (ts.isVariableDeclaration(node))
      variables.push(`var ${node.getText(source)};`);
    else functions.push(node.getText(source));
  }
  for (const name of ["importBackupFromSource", "buildFullExportDbValue"]) {
    const node = source.statements.find((node) => node.name?.text === name);
    if (!node) throw new Error("Reference importer declaration missing");
    include(node);
  }
  vm.runInContext([...functions, ...variables].join("\n"), context, {
    timeout: 10000,
  });
  return {
    sources: [...sources].sort(),
    operations,
    files,
    dirs,
    savePath,
    db: dbModule,
    probeNetwork: () => context.fetch("https://synthetic.invalid/blocked"),
    failNextSwap() {
      failRename = true;
    },
    readPluginStorage: () => pluginStorage.readAll(),
    async importArchive(bytes, options = {}) {
      let sourceDatabase;
      context.parseBackupChunk(Buffer.from(bytes), (name, value) => {
        if (name === "database.risudat") sourceDatabase = value;
      });
      async function* chunks() {
        for (let offset = 0; offset < bytes.length; offset += 317)
          yield Buffer.from(bytes.subarray(offset, offset + 317));
      }
      const result = await context.importBackupFromSource(chunks(), {
        totalBytes: bytes.length,
        ...options,
      });
      const database = await context.buildFullExportDbValue();
      if (!database)
        throw new Error("Pocket importer did not activate a database");
      return { result, database, sourceDatabase };
    },
    close() {
      dbModule.db.close();
    },
  };
}
