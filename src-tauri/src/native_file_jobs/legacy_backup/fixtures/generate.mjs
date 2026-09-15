// Synthetic fixtures only. Run with a read-only PocketRisu checkout containing both tags:
// node src-tauri/src/native_file_jobs/legacy_backup/fixtures/generate.mjs <PocketRisu checkout>
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { resolve } from "node:path";
import { capturePocketExport } from "./export.mjs";

const repository = process.argv[2];
if (!repository) throw new Error("Provide the PocketRisu reference checkout");
const requireReference = createRequire(resolve(repository, "package.json"));
const { Packr } = requireReference("msgpackr");
const fflate = requireReference("fflate");
const coldKey = "9f8b7c6d-1a2b-3c4d-5e6f-a1b2c3d4e5f6";
// Restoring expands the cold payload, so the archive stores a stub instead.
const coldGroup = {
  type: "group",
  chaId: "synthetic-group",
  name: "Synthetic group",
  characters: ["synthetic-active"],
  chats: [
    {
      id: "synthetic-cold-chat",
      name: "Synthetic cold chat",
      message: [{ role: "user", data: "Synthetic cold message" }],
    },
  ],
};
const expected = {
  username: "Synthetic PocketRisu restore",
  characters: [
    {
      type: "character",
      chaId: "synthetic-active",
      name: "Synthetic active",
      image: "assets/portrait.png",
      chats: [
        {
          id: "synthetic-chat",
          name: "Synthetic chat",
          message: [
            {
              role: "user",
              data: "{{inlay::picture}} {{inlay::voice}} {{inlay::movie}} {{inlay::signature}}",
            },
          ],
        },
      ],
      additionalAssets: [["module-art", "assets/module.png", "png"]],
      emotionImages: [],
    },
    {
      type: "character",
      chaId: "synthetic-archived-inline",
      name: "Synthetic archived export",
      chats: [
        {
          id: "archived-chat",
          name: "Archived chat",
          message: [{ role: "char", data: "Archived chat preserved" }],
        },
      ],
      trashTime: 1234,
    },
    coldGroup,
  ],
  botPresets: [
    { id: "synthetic-preset", name: "Synthetic preset", apiType: "openai" },
  ],
  modules: [
    {
      id: "synthetic-module",
      name: "Synthetic module",
      assets: [["art", "assets/module.png", "png"]],
    },
  ],
  plugins: [
    { name: "synthetic-plugin", script: "// synthetic, never executed" },
  ],
  pluginCustomStorage: {
    "synthetic-plugin": { text: "한글 보존", values: [1, false, null] },
  },
  loadouts: [],
  personas: [
    {
      id: "synthetic-persona",
      name: "Synthetic persona",
      icon: "assets/portrait.png",
      customField: { nested: [true, null, "한글"] },
    },
  ],
  lorebook: [
    {
      name: "Synthetic lore",
      data: [{ key: "test", content: "Synthetic lore text" }],
    },
  ],
  globalRegex: [{ in: "synthetic", out: "replacement", type: "editdisplay" }],
  characterOrder: [
    "synthetic-active",
    { name: "Synthetic folder", data: ["synthetic-group"] },
  ],
  unknownPocketField: { nested: [{ value: "preserve unknown fields" }] },
};
const stored = {
  ...expected,
  characters: expected.characters.map((character) =>
    character === coldGroup
      ? {
          type: coldGroup.type,
          chaId: coldGroup.chaId,
          name: coldGroup.name,
          characters: coldGroup.characters,
          chats: [
            { id: "group-chat", name: "Synthetic group chat", message: [] },
          ],
          coldstorage: coldKey,
        }
      : character,
  ),
};
const directory = fileURLToPath(new URL(".", import.meta.url));
writeFileSync(
  `${directory}expected.json`,
  `${JSON.stringify(expected, null, 2)}\n`,
);
for (const [tag, compression] of [
  ["v1.10.0", "compression"],
  ["v1.12.0", "noCompression"],
]) {
  const source = execFileSync(
    "git",
    [
      "-C",
      repository,
      "-c",
      `safe.directory=${repository.replaceAll("\\", "/")}`,
      "show",
      `${tag}:server/node/utils.cjs`,
    ],
    { encoding: "utf8" },
  );
  const encoder = source.match(
    /function encodeRisuSaveLegacy\(data, compression = 'noCompression'\) \{[\s\S]*?\n\}/,
  )?.[0];
  if (!encoder) throw new Error(`Encoder changed in ${tag}`);
  // Execute only the public serialization function, without starting a server or loading its data.
  const encode = new Function(
    "packr",
    "fflate",
    "magicHeader",
    "magicCompressedHeader",
    `${encoder}; return encodeRisuSaveLegacy`,
  )(
    new Packr({ useRecords: false }),
    // Fix only the gzip timestamp to make the checked-in fixture reproducible.
    {
      ...fflate,
      compressSync: (data) => fflate.compressSync(data, { mtime: 0 }),
    },
    Uint8Array.from([0, 82, 73, 83, 85, 83, 65, 86, 69, 0, 7]),
    Uint8Array.from([0, 82, 73, 83, 85, 83, 65, 86, 69, 0, 8]),
  );
  const inlays = [];
  for (const [id, ext, type, payload] of [
    ["picture", "webp", "image", "synthetic picture"],
    ["voice", "mp3", "audio", "synthetic voice"],
    ["movie", "mp4", "video", "synthetic movie"],
    ["signature", "json", "signature", '{"strokes":[]}'],
  ]) {
    inlays.push({ id, ext, type, payload });
  }
  const serverSource = execFileSync(
    "git",
    [
      "-C",
      repository,
      "-c",
      `safe.directory=${repository.replaceAll("\\", "/")}`,
      "show",
      `${tag}:server/node/server.cjs`,
    ],
    { encoding: "utf8" },
  );
  const archive = await capturePocketExport(
    serverSource,
    encode(stored, compression),
    new Map([
      ["assets/portrait.png", Buffer.from("synthetic portrait")],
      ["assets/module.png", Buffer.from("synthetic module")],
    ]),
    inlays,
    new Map([
      [`coldstorage/${coldKey}`, { character: coldGroup }],
    ]),
  );
  writeFileSync(`${directory}pocket-risu-${tag}.bin`, archive);
}
