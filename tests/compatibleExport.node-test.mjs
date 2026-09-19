import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
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
          assert.deepEqual(
            plain(await harness.decode(harness.encode(value))),
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
            { references: 2, distinctAssets: 1, bytes: png.length },
          );
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
                      createdAt: 42,
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
              assert.equal(
                JSON.parse(
                  harness.storage.get(`inlay_meta/${f.id}`).toString("utf8"),
                ).createdAt,
                42,
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
              harness.encode(result.resaved),
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

      if (target === "pocket")
        await t.test(
          "actual Pocket bootstrap purges group and folder references",
          async () => {
            const db = await harness.normalize({
              characters: [
                { chaId: "synthetic-a", type: "character", chats: [] },
                { chaId: "synthetic-group", type: "group", chats: [] },
              ],
              characterOrder: [
                "synthetic-group",
                {
                  type: "folder",
                  name: "Synthetic",
                  data: ["synthetic-a", "synthetic-group"],
                },
              ],
              formatversion: 5,
            });
            assert.deepEqual(plain(db.characters.map((c) => c.chaId)), [
              "synthetic-a",
            ]);
            assert.equal(
              JSON.stringify(db.characterOrder).includes("synthetic-group"),
              false,
            );
            assert.equal(
              JSON.stringify(db.characterOrder).includes("synthetic-a"),
              true,
            );
          },
        );
    },
  );
}
