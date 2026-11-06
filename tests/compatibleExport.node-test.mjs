import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import os from "node:os";
import { createReferenceRequire } from "../scripts/compatibleExportDependencies.mjs";
import { gzipSync } from "node:zlib";
import { createRequire } from "node:module";
import {
  contractDirectory,
  generateContract,
  verifyGeneratedContract,
} from "../scripts/compatibleExportContract.mjs";
import {
  createReferenceHarness,
  verifyReferenceRuntimePins,
  createNestReimportHarness,
  verifyRoundTripAssets,
} from "../scripts/compatibleExportReferenceHarness.mjs";
const require = createRequire(import.meta.url);
const { Packr } = require("msgpackr");
const packr = new Packr({ useRecords: false });
const plain = (value) => JSON.parse(JSON.stringify(value));
const compressedWire = (value) =>
  Buffer.concat([
    Buffer.from([0, 82, 73, 83, 85, 83, 65, 86, 69, 0, 8]),
    gzipSync(packr.encode(value)),
  ]);
// Uncompressed RisuSave block file: [type][compression][name length][name][u32 LE length][data].
const risuSaveBlocks = (blocks) =>
  Buffer.concat([
    Buffer.from("RISUSAVE\0"),
    ...blocks.map(([type, name, value]) => {
      const nameBytes = Buffer.from(name);
      const data = Buffer.isBuffer(value) ? value : Buffer.from(JSON.stringify(value));
      const length = Buffer.alloc(4);
      length.writeUInt32LE(data.length);
      return Buffer.concat([Buffer.from([type, 0, nameBytes.length]), nameBytes, length, data]);
    }),
  ]);
const archiveEntry = (name, bytes) => {
  const filename = Buffer.from(name);
  const head = Buffer.alloc(4);
  head.writeUInt32LE(filename.length);
  const size = Buffer.alloc(4);
  size.writeUInt32LE(bytes.length);
  return Buffer.concat([head, filename, size, bytes]);
};

// A test oracle for the generated contract, independent of the Rust projector.
// Missing fields are accepted because reference setDatabase supplies defaults;
// present values and every structural boundary are checked recursively.
function accepts(contract, id, value) {
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
      return node.variants.some((variant) => accepts(contract, variant, value));
    case "array":
      return (
        Array.isArray(value) &&
        value.every((item) => accepts(contract, node.items, item))
      );
    case "tuple":
      return (
        Array.isArray(value) &&
        value.length <= node.items.length &&
        node.items.every((item, index) =>
          index < value.length
            ? accepts(contract, item, value[index])
            : node.optional[index],
        )
      );
    case "object":
      return (
        value !== null &&
        !Array.isArray(value) &&
        typeof value === "object" &&
        Object.entries(value).every(([key, item]) => {
          if (!Object.hasOwn(node.fields, key) && key.startsWith("__") && id === contract.types.Message)
            return true;
          const child = Object.hasOwn(node.fields, key)
            ? node.fields[key].node
            : node.additional;
          return (
            child !== null &&
            child !== undefined &&
            accepts(contract, child, item)
          );
        })
      );
    default:
      throw new Error(`Unknown contract node ${node.kind}`);
  }
}

test("reference libraries reject parent fallback and locked version drift before execution", (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "reference-dependencies-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const reference = path.join(root, "reference");
  fs.mkdirSync(reference);
  fs.writeFileSync(path.join(reference, "package.json"), JSON.stringify({ dependencies: { msgpackr: "^1.11.0" } }));
  fs.writeFileSync(path.join(reference, "pnpm-lock.yaml"), "lockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      msgpackr:\n        specifier: ^1.11.0\n        version: 1.11.9\n");
  const install = (directory, version, source) => {
    const module = path.join(directory, "node_modules", "msgpackr");
    fs.mkdirSync(module, { recursive: true });
    fs.writeFileSync(path.join(module, "package.json"), JSON.stringify({ name: "msgpackr", version, main: "index.cjs" }));
    fs.writeFileSync(path.join(module, "index.cjs"), source);
  };
  install(root, "1.11.9", 'throw new Error("parent library executed")');
  assert.throws(() => createReferenceRequire(reference, reference, ["msgpackr"]), /Missing reference dependency msgpackr@1\.11\.9/);
  install(reference, "1.10.1", 'throw new Error("wrong version executed")');
  assert.throws(() => createReferenceRequire(reference, reference, ["msgpackr"]), /msgpackr expected 1\.11\.9, found 1\.10\.1/);
  install(reference, "1.11.9", 'module.exports = { version: "1.11.9" }');
  const checkedRequire = createReferenceRequire(reference, reference, ["msgpackr"]);
  assert.equal(checkedRequire("msgpackr").version, "1.11.9");
  assert.throws(() => checkedRequire("fflate"), /Unapproved reference dependency/);
});

for (const target of ["risuai", "pocket"]) {
  test(
    `${target}: pinned recursive source contract and real reference round trip`,
    { timeout: 120000 },
    async (t) => {
      const contract = JSON.parse(
        fs.readFileSync(path.join(contractDirectory, `${target}.json`), "utf8"),
      );
      verifyGeneratedContract(generateContract(target), contract);
      const harness = createReferenceHarness(target);
      t.after(() => harness.close());
      const runtime = JSON.parse(
        fs.readFileSync(path.join(contractDirectory, "runtime.json"), "utf8"),
      );
      verifyReferenceRuntimePins(target, harness, runtime[target]);

      await t.test(
        "wire values decode with actual reference function",
        async () => {
          const value = {
            empty: "",
            zero: 0,
            false: false,
            null: null,
            integer: Number.MAX_SAFE_INTEGER,
            negative: -2147483649,
            decimal: 0.125,
            unicode: "합성 한국어 🐿️",
            array: [],
            map: {},
            nested: [{ data: "synthetic" }],
          };
          assert.deepEqual(
            plain(await harness.decode(compressedWire(value))),
            value,
          );
          // RisuAI writes a database as RisuSave blocks, one per database part,
          // so the re-save check needs every part present.
          const database = {
            ...value,
            botPresets: [{ name: "Synthetic preset" }],
            modules: [],
            loadouts: [],
            plugins: [],
            pluginCustomStorage: { synthetic: value },
          };
          assert.deepEqual(
            plain(await harness.decode(await harness.encode(database))),
            database,
          );
          if (target === "pocket")
            assert.deepEqual(
              plain(await harness.decode(await harness.encode(value))),
              value,
            );
        },
      );

      await t.test(
        "unknown structural fields and unsupported enum values are rejected at depth",
        () => {
          assert.equal(
            accepts(contract, contract.root, { risunestUnexpected: true }),
            false,
          );
          assert.equal(
            accepts(contract, contract.types.Message, {
              role: "assistant",
              data: "synthetic",
            }),
            false,
          );
          assert.equal(
            accepts(contract, contract.types.Message, {
              role: "char",
              data: "synthetic",
              responseVariants: {},
            }),
            false,
          );
          assert.equal(
            accepts(contract, contract.types.Chat, {
              message: [{ role: "char", data: "synthetic", surprise: 1 }],
            }),
            false,
          );
          assert.equal(
            accepts(contract, contract.root, {
              characters: [
                {
                  chats: [
                    {
                      message: [
                        { role: "char", data: "synthetic", surprise: 1 },
                      ],
                    },
                  ],
                },
              ],
            }),
            false,
          );
          assert.equal(
            accepts(contract, contract.root, {
              adaptiveThinkingEffort: "xhigh",
            }),
            target === "risuai",
          );
          assert.equal(
            accepts(contract, contract.root, {
              adaptiveThinkingEffort: "high",
            }),
            true,
          );
          assert.equal(
            accepts(contract, contract.types.Message, {
              role: "char",
              data: "synthetic",
              swipes: ["synthetic"],
              swipeId: 0,
            }),
            target === "pocket",
          );
          assert.equal(
            accepts(contract, contract.types.Chat, { rerollRecovery: {} }),
            false,
          );
          assert.equal(
            accepts(contract, contract.types.Chat, { __plugin: true }),
            false,
          );
        },
      );

      await t.test(
        "plugin-owned message fields survive the actual reference loader",
        async () => {
          const record = { kind: "synthetic", nested: ["합성", 1, null] };
          const fixture = {
            characters: [
              {
                chaId: "synthetic-plugin-fields",
                type: "character",
                name: "Synthetic",
                chats: [
                  {
                    id: "synthetic-plugin-chat",
                    name: "",
                    message: [
                      { role: "char", data: "합성 답변", chatId: "m1", __plugin: record },
                    ],
                  },
                ],
              },
            ],
            characterOrder: ["synthetic-plugin-fields"],
            formatversion: 5,
          };
          assert.equal(accepts(contract, contract.root, fixture), true);
          const result = await harness.roundTrip(compressedWire(fixture));
          assert.deepEqual(
            plain(result.resaved.characters[0].chats[0].message[0].__plugin),
            record,
          );
        },
      );

      await t.test(
        "nulls the references store survive the actual reference loader",
        async () => {
          const fixture = {
            characters: [
              {
                chaId: "synthetic-null-fields",
                type: "character",
                name: "Synthetic",
                postHistoryInstructions: null,
                globalLore: [{ key: "합성", content: "합성 설명", activationPercent: null, insertorder: null }],
                chats: [
                  {
                    id: "synthetic-null-chat",
                    name: "",
                    folderId: null,
                    message: [{ role: "user", data: "합성 입력", chatId: "m1", name: null }],
                  },
                ],
              },
            ],
            characterOrder: [
              { id: "synthetic-folder", name: "Folder", color: "", data: ["synthetic-null-fields"], imgFile: null },
            ],
            botPresets: [{
              name: "Synthetic preset", koboldURL: null, thinkingTokens: null, promptTemplate: null,
              maxContext: null, maxResponse: null,
            }],
            textScreenColor: null,
            textScreenBorder: null,
            localStopStrings: null,
            promptTemplate: null,
            thinkingTokens: null,
            formatversion: 5,
          };
          // Cleared number settings, which the loader replaces with its defaults.
          const numbers = [
            "loreBookDepth", "loreBookToken", "autoContinueMinTokens", "requestRetrys", "genTime",
            "assetMaxDifference", "localNetworkTimeoutSec", "maxContext", "maxResponse", "generationSeed",
          ];
          for (const key of numbers) fixture[key] = null;
          const nulls = (db) => {
            const [character] = db.characters;
            const [preset] = db.botPresets;
            return [
              db.textScreenColor, db.textScreenBorder, db.localStopStrings, db.promptTemplate, db.thinkingTokens,
              db.characterOrder[0].imgFile, preset.koboldURL, preset.thinkingTokens, preset.promptTemplate,
              preset.maxContext, preset.maxResponse,
              character.postHistoryInstructions, character.globalLore[0].activationPercent,
              character.globalLore[0].insertorder,
              character.chats[0].folderId, character.chats[0].message[0].name,
            ];
          };
          assert.equal(accepts(contract, contract.root, fixture), true);
          const { resaved } = await harness.roundTrip(compressedWire(fixture));
          assert.deepEqual(plain(nulls(resaved)), nulls(fixture));
          assert.deepEqual(
            numbers.filter((key) => !Number.isFinite(resaved[key])),
            [],
          );
        },
      );

      await t.test(
        "explicit dictionary/freeform values survive schema checks",
        () => {
          const value = {
            pluginCustomStorage: {
              syntheticPlugin: {
                deep: { arbitrary: ["text", null, 1] },
                script: "keep assets/example.png inside this string",
              },
            },
          };
          assert.equal(accepts(contract, contract.root, value), true);
          assert.equal(
            accepts(contract, contract.root, {
              globalChatVariables: { arbitrary_key: "value" },
            }),
            true,
          );
          assert.equal(
            accepts(contract, contract.root, {
              globalChatVariables: { arbitrary_key: {} },
            }),
            false,
          );
        },
      );

      await t.test(
        `${target === "risuai" ? "actual archive reader and byte loaders" : "actual transactional server importer"} preserve synthetic image bytes`,
        async () => {
          const png = Buffer.from(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg==",
            "base64",
          );
          const database = compressedWire({
            characterOrder: [
              { id: "empty-folder", name: "Empty", color: "", data: [] },
              { id: "image-folder", name: "Image", color: "", data: ["synthetic-asset-char"], img: "assets/synthetic.png" },
            ],
            characters: [
              {
                chaId: "synthetic-asset-char",
                type: "character",
                image: "assets/synthetic.png",
                additionalAssets: [
                  ["synthetic", "assets/synthetic.png", "png"],
                ],
                chats: [],
              },
            ],
            formatversion: 5,
          });
          const bytes = Buffer.concat([
            archiveEntry("synthetic.png", png),
            archiveEntry("database.risudat", database),
          ]);
          const imported = await harness.importArchive(bytes);
          const result = await harness.roundTrip(imported);
          assert.deepEqual(
            Buffer.from(
              await harness.readAsset(result.resaved.characters[0].image),
            ),
            png,
          );
          assert.deepEqual(
            Buffer.from(
              await harness.readImage(result.resaved.characters[0].image),
            ),
            png,
          );
          assert.deepEqual(
            plain(result.resaved.characters[0].additionalAssets),
            [["synthetic", "assets/synthetic.png", "png"]],
          );
          const source = await harness.decode(database);
          assert.deepEqual(
            await verifyRoundTripAssets(harness, source, result.resaved),
            { references: 3, distinctAssets: 1, bytes: png.length },
          );
          assert.equal(source.characterOrder[1].id, "image-folder");
          assert.equal(result.resaved.characterOrder[0].id, "image-folder");
          for (const change of [
            value => { value.characterOrder[0].img = "assets/changed.png"; },
            value => { value.characterOrder[0].id = "different-folder"; },
            value => { value.characterOrder.splice(0, 1); },
          ]) {
            const changedFolder = plain(result.resaved);
            change(changedFolder);
            await assert.rejects(verifyRoundTripAssets(harness, source, changedFolder), /changed a declared synthetic asset reference/);
          }
          const duplicateFolder = plain(result.resaved);
          duplicateFolder.characterOrder.push({ ...duplicateFolder.characterOrder[0] });
          await assert.rejects(verifyRoundTripAssets(harness, source, duplicateFolder), /Duplicate synthetic folder id/);
          const changed = plain(result.resaved);
          changed.characters[0].additionalAssets[0][1] = "assets/changed.png";
          await assert.rejects(
            verifyRoundTripAssets(harness, source, changed),
            /changed a declared synthetic asset reference/,
          );
          await assert.rejects(
            verifyRoundTripAssets(
              {
                ...harness,
                readAsset: async () =>
                  Buffer.from("incorrect synthetic loader bytes"),
              },
              source,
              result.resaved,
            ),
            /loader bytes differ/,
          );
        },
      );

      await t.test(
        "actual cold archive reader preserves nested synthetic character/chat payload",
        async () => {
          const id = "00000000-0000-4000-8000-000000000010";
          const chatId = "00000000-0000-4000-8000-000000000011";
          const cold = {
            character: {
              chaId: "synthetic-cold",
              type: "character",
              name: "Synthetic cold",
              chats: [
                {
                  id: "synthetic-cold-chat",
                  message: [
                    { role: "char", data: "\uEF01COLDSTORAGE\uEF01" + chatId },
                  ],
                },
              ],
            },
          };
          const messages = [{ role: "char", data: "합성 cold 본문" }];
          const bytes = Buffer.concat([
            archiveEntry(
              `coldstorage_${id}.json`,
              Buffer.from(JSON.stringify(cold)),
            ),
            archiveEntry(
              `coldstorage_${chatId}.json`,
              Buffer.from(JSON.stringify(messages)),
            ),
            archiveEntry(
              "database.risudat",
              compressedWire({
                characters: [
                  {
                    chaId: "synthetic-cold",
                    type: "character",
                    coldstorage: id,
                    chats: [],
                  },
                ],
                formatversion: 5,
              }),
            ),
          ]);
          const imported = await harness.importArchive(bytes);
          assert.deepEqual(plain(await harness.readCold(id)), cold);
          assert.deepEqual(plain(await harness.readCold(chatId)), messages);
          const result = await harness.roundTrip(imported);
          if (target === "pocket") {
            assert.equal(
              Object.hasOwn(result.resaved.characters[0], "coldstorage"),
              false,
            );
            assert.equal(
              result.resaved.characters[0].chats[0].message[0].data,
              "합성 cold 본문",
            );
          }
        },
      );

      if (target === "risuai")
        await t.test(
          "actual block save reader, importer and re-save carry SaveKV namespaces",
          async () => {
            const kv = {
              synthetic: {
                version: 1,
                entries: [
                  { k: "empty", v: "" },
                  { k: "zero", v: 0 },
                  { k: "false", v: false },
                  { k: "null", v: null },
                  { k: "nested", v: { text: "합성", list: [1, null] } },
                ],
              },
              other: { version: 1, entries: [{ k: "1", v: "one" }, { k: "0", v: "zero" }] },
            };
            const character = {
              chaId: "synthetic-kv-char",
              type: "character",
              name: "Synthetic KV",
              chats: [{ id: "synthetic-kv-chat", name: "", message: [] }],
            };
            const database = risuSaveBlocks([
              [1, "root", { formatversion: 5, characterOrder: [character.chaId] }],
              [2, character.chaId, character],
              ...Object.entries(kv).map(([ns, content]) => [12, `kv:${ns}`, content]),
              [0, "config", { version: 1 }],
            ]);
            const imported = await harness.importArchive(
              archiveEntry("database.risudat", database),
            );
            assert.deepEqual(harness.liveKV(), kv);
            const result = await harness.roundTrip(imported);
            assert.equal(result.resaved.characters[0].name, "Synthetic KV");
            assert.deepEqual(plain(result.kv), kv);
            assert.deepEqual(plain(result.resavedKV), kv);
            // The same empty part in a save the harness did not write fails closed.
            await assert.rejects(
              harness.decode(risuSaveBlocks([
                [1, "root", { formatversion: 5 }],
                [11, "pluginStorage", Buffer.alloc(0)],
                [0, "config", { version: 1 }],
              ])),
              /unsupported side effect/,
            );
          },
        );

      if (target === "pocket")
        await t.test(
          "actual transactional importer rolls back invalid streams and swaps media staging",
          async () => {
            const tx = harness.transaction;
            const dbBytes = compressedWire({
              characters: [],
              formatversion: 5,
              pluginCustomStorage: { syntheticPlugin: { retained: true } },
            });
            const priorMedia = Buffer.from("synthetic previous media");
            const baseline = Buffer.concat([
              archiveEntry(
                "previous.png",
                Buffer.from("synthetic previous asset"),
              ),
              archiveEntry("inlay/synthetic-previous.png", priorMedia),
              archiveEntry(
                "inlay_sidecar/synthetic-previous",
                Buffer.from(
                  '{"ext":"png","name":"Synthetic previous","type":"image"}',
                ),
              ),
              archiveEntry("database.risudat", dbBytes),
            ]);
            await harness.importArchive(baseline);
            const previousDatabase = Buffer.from(
              tx.db.kvGet("database/database.bin"),
            );
            const previousAsset = Buffer.from(
              tx.db.kvGet("assets/previous.png"),
            );
            const activeMedia = path.join(
              tx.savePath,
              "inlays",
              "synthetic-previous.png",
            );
            assert.deepEqual(tx.files.get(activeMedia), priorMedia);
            assert.deepEqual(plain(tx.readPluginStorage()), {
              syntheticPlugin: { retained: true },
            });
            const validDatabase = archiveEntry(
              "database.risudat",
              compressedWire({ characters: [], formatversion: 5 }),
            );
            const invalid = [
              Buffer.concat([
                archiveEntry("replacement.png", Buffer.from("synthetic")),
                validDatabase.subarray(0, -1),
              ]),
              Buffer.concat([validDatabase, validDatabase]),
              archiveEntry("replacement.png", Buffer.from("synthetic")),
              archiveEntry(
                "database.risudat",
                compressedWire("synthetic non-database primitive"),
              ),
              Buffer.concat([
                archiveEntry(
                  "encryption.risudat",
                  Buffer.from('{"type":"account","time":1}'),
                ),
                validDatabase,
              ]),
            ];
            for (const bytes of invalid) {
              await assert.rejects(tx.importArchive(bytes));
              assert.deepEqual(
                Buffer.from(tx.db.kvGet("database/database.bin")),
                previousDatabase,
              );
              assert.deepEqual(
                Buffer.from(tx.db.kvGet("assets/previous.png")),
                previousAsset,
              );
              assert.equal(tx.db.kvGet("assets/replacement.png"), null);
              assert.deepEqual(plain(tx.readPluginStorage()), {
                syntheticPlugin: { retained: true },
              });
              assert.deepEqual(tx.files.get(activeMedia), priorMedia);
              assert.equal(tx.db.db.inTransaction, false);
              assert.equal(
                tx.dirs.has(path.join(tx.savePath, "inlays_import_staging")),
                false,
              );
            }
            await assert.rejects(
              tx.importArchive(validDatabase, { maxBytes: 1 }),
            );
            assert.deepEqual(
              Buffer.from(tx.db.kvGet("database/database.bin")),
              previousDatabase,
            );
            assert.throws(tx.probeNetwork, /forbids network/);

            tx.failNextSwap();
            await assert.rejects(
              tx.importArchive(validDatabase),
              /Injected staging swap failure/,
            );
            assert.deepEqual(
              tx.files.get(activeMedia),
              priorMedia,
              "actual importer restores prior inlay directory on swap failure",
            );
            assert.equal(
              tx.dirs.has(path.join(tx.savePath, "inlays_import_staging")),
              false,
            );
            // Reference behavior: the final SQLite COMMIT precedes the filesystem
            // swap. A swap error restores media, but cannot roll back that commit.
            assert.notDeepEqual(
              Buffer.from(tx.db.kvGet("database/database.bin")),
              previousDatabase,
            );
          },
        );

      if (target === "pocket")
        await t.test(
          "actual importer completes multiple SQLite batches and reassembles plugin storage for export",
          async () => {
            const entries = Array.from({ length: 5001 }, (_, index) =>
              archiveEntry(
                `synthetic-batch-${index}.png`,
                Buffer.from([index % 256]),
              ),
            );
            entries.push(
              archiveEntry(
                "database.risudat",
                compressedWire({
                  characters: [],
                  formatversion: 5,
                  pluginCustomStorage: { syntheticBatch: ["value", 42] },
                }),
              ),
            );
            const tx = harness.transaction;
            const imported = await tx.importArchive(Buffer.concat(entries));
            assert.equal(imported.result.assetsRestored, 5001);
            assert.equal(imported.result.coldStorageFailed, 0);
            assert.equal(tx.db.kvList("assets/").length, 5001);
            assert.equal(tx.db.db.inTransaction, false);
            assert.deepEqual(
              plain(
                (await harness.decode(imported.database)).pluginCustomStorage,
              ),
              { syntheticBatch: ["value", 42] },
            );
            assert.deepEqual(plain(tx.readPluginStorage()), {
              syntheticBatch: ["value", 42],
            });
          },
        );

      if (target === "pocket")
        await t.test(
          "actual latest sidecar/media loaders preserve types, metadata and bytes",
          async () => {
            const entries = [];
            const fixtures = [
              {
                id: "synthetic-image",
                ext: "png",
                type: "image",
                mime: "image/png",
                bytes: Buffer.from("89504e470d0a1a0a", "hex"),
              },
              {
                id: "synthetic-audio",
                ext: "wav",
                type: "audio",
                mime: "audio/wav",
                bytes: Buffer.from("524946462400000057415645", "hex"),
              },
              {
                id: "synthetic-video",
                ext: "mp4",
                type: "video",
                mime: "video/mp4",
                bytes: Buffer.from("000000186674797069736f6d", "hex"),
              },
              {
                id: "synthetic-signature",
                ext: "json",
                type: "signature",
                bytes: Buffer.from('{"synthetic":true}'),
              },
            ];
            for (const f of fixtures) {
              entries.push(archiveEntry(`inlay/${f.id}.${f.ext}`, f.bytes));
              entries.push(
                archiveEntry(
                  `inlay_sidecar/${f.id}`,
                  Buffer.from(
                    JSON.stringify({
                      ext: f.ext,
                      type: f.type,
                      name: "Synthetic media",
                      width: 16,
                      height: 8,
                    }),
                  ),
                ),
              );
              entries.push(
                archiveEntry(
                  `inlay_meta/${f.id}`,
                  Buffer.from(
                    JSON.stringify({
                      charId: "synthetic-a",
                      chatId: "synthetic-chat",
                      createdAt: f.type === "signature" ? 0 : 42,
                      ...(f.type === "image" ? {} : { updatedAt: 43 }),
                    }),
                  ),
                ),
              );
            }
            entries.push(
              archiveEntry(
                "database.risudat",
                compressedWire({ characters: [], formatversion: 5 }),
              ),
            );
            await harness.importArchive(Buffer.concat(entries));
            for (const f of fixtures) {
              const info = await harness.readInlayInfo(f.id);
              const media = await harness.readInlay(f.id);
              assert.equal(info.ext, f.ext);
              assert.equal(info.type, f.type);
              assert.equal(media.name, "Synthetic media");
              assert.equal(media.width, 16);
              assert.equal(media.height, 8);
              if (f.type === "signature")
                assert.equal(media.data, f.bytes.toString("utf8"));
              else {
                assert.equal(
                  media.data.startsWith(`data:${f.mime};base64,`),
                  true,
                );
                assert.deepEqual(
                  Buffer.from(media.data.split(",")[1], "base64"),
                  f.bytes,
                );
              }
              assert.deepEqual(plain(await harness.readInlayMeta(f.id)), {
                charId: "synthetic-a",
                chatId: "synthetic-chat",
                createdAt: f.type === "signature" ? 0 : 42,
                updatedAt: f.type === "image" ? 42 : 43,
              });
              assert.deepEqual(
                JSON.parse(harness.transaction.db.kvGet(`inlay_meta/${f.id}`).toString("utf8")),
                {
                  charId: "synthetic-a", chatId: "synthetic-chat",
                  createdAt: f.type === "signature" ? 0 : 42,
                  ...(f.type === "image" ? {} : { updatedAt: 43 }),
                },
              );
            }
            // These are container/loader fixtures, not playable audio/video samples.
            // Successful MIME dispatch does not claim codec or playback support.
          },
        );

      await t.test(
        "actual bootstrap defaults, all synthetic chats, plugin data and re-save",
        async () => {
          const fixture = {
            characters: [
              {
                chaId: "synthetic-a",
                type: "character",
                name: "Synthetic A",
                chats: [
                  {
                    id: "synthetic-chat-1",
                    name: "",
                    message: [
                      { role: "user", data: "합성 질문", time: 1 },
                      {
                        role: "char",
                        data: "합성 답변",
                        ...(target === "pocket"
                          ? {
                              swipes: ["다른 합성 답변", "합성 답변"],
                              swipeId: 1,
                            }
                          : {}),
                      },
                    ],
                  },
                  { id: "synthetic-chat-2", name: "Empty", message: [] },
                ],
              },
              {
                chaId: "synthetic-b",
                type: "character",
                name: "Synthetic B",
                chats: [],
              },
            ],
            characterOrder: ["synthetic-a", "synthetic-b"],
            pluginCustomStorage: {
              syntheticPlugin: {
                arbitrary: null,
                nested: ["한글", 0.125, Number.MAX_SAFE_INTEGER],
                text: "do not rewrite assets/synthetic.png substring",
              },
            },
            personas: [
              {
                id: "synthetic-persona",
                name: "Synthetic Persona",
                personaPrompt: "Synthetic prompt",
                icon: "",
              },
            ],
            modules: [],
            botPresets: [],
            adaptiveThinkingEffort: "high",
            formatversion: 5,
          };
          assert.equal(accepts(contract, contract.root, fixture), true);
          const result = await harness.roundTrip(compressedWire(fixture));
          assert.equal(result.resaved.characters.length, 2);
          assert.equal(result.resaved.characters[0].chats.length, 2);
          assert.equal(result.resaved.characters[0].chats[0].message.length, 2);
          assert.equal(
            result.resaved.characters[0].chats[0].message[1].data,
            "합성 답변",
          );
          assert.deepEqual(
            plain(result.resaved.pluginCustomStorage),
            fixture.pluginCustomStorage,
          );
          assert.equal(result.resaved.adaptiveThinkingEffort, "high");
          assert.equal(
            typeof result.resaved.temperature,
            "number",
            "actual setDatabase default applied",
          );
          assert.equal(
            result.resaved.characters[0].firstMessage,
            "",
            "actual bootstrap normalization applied",
          );
          if (target === "pocket") {
            const message = result.resaved.characters[0].chats[0].message[1];
            assert.equal(message.swipes[message.swipeId], message.data);
            message.data = "edited synthetic response";
            message.swipes[message.swipeId] = message.data;
            const edited = await harness.roundTrip(
              await harness.encode(result.resaved),
            );
            assert.equal(
              edited.resaved.characters[0].chats[0].message[1].data,
              "edited synthetic response",
            );
            assert.equal(
              Object.hasOwn(
                edited.resaved.characters[0].chats[0].message[1],
                "responseVariants",
              ),
              false,
            );
            const reimported = createNestReimportHarness()(
              plain(edited.resaved),
            );
            const importedMessage =
              reimported.characters[0].chats[0].message[1];
            assert.equal(importedMessage.data, "edited synthetic response");
            assert.equal(
              importedMessage.responseVariants.candidates.find(
                (candidate) =>
                  candidate.id === importedMessage.responseVariants.selectedId,
              ).messages[0].data,
              "edited synthetic response",
            );
            assert.equal(importedMessage.responseVariants.candidates.length, 2);
          }
        },
      );
    },
  );
}
