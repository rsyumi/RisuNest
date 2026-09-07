// Synthetic fixtures only. Run with a read-only PocketRisu checkout containing both tags:
// node src-tauri/src/native_file_jobs/legacy_backup/fixtures/generate.mjs <PocketRisu checkout>
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { resolve } from "node:path";

const repository = process.argv[2];
if (!repository) throw new Error("Provide the PocketRisu reference checkout");
const requireReference = createRequire(resolve(repository, "package.json"));
const { Packr } = requireReference("msgpackr");
const fflate = requireReference("fflate");
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
};
const directory = fileURLToPath(new URL(".", import.meta.url));
writeFileSync(
  `${directory}expected.json`,
  `${JSON.stringify(expected, null, 2)}\n`,
);
function entry(name, data) {
  const nameBytes = Buffer.from(name);
  const header = Buffer.alloc(4);
  const size = Buffer.alloc(4);
  header.writeUInt32LE(nameBytes.length);
  size.writeUInt32LE(data.length);
  return Buffer.concat([header, nameBytes, size, data]);
}
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
  const files = [
    entry("portrait.png", Buffer.from("synthetic portrait")),
    entry("module.png", Buffer.from("synthetic module")),
  ];
  const metadata = [];
  for (const [id, ext, type, payload] of [
    ["picture", "webp", "image", "synthetic picture"],
    ["voice", "mp3", "audio", "synthetic voice"],
    ["movie", "mp4", "video", "synthetic movie"],
    ["signature", "json", "signature", '{"strokes":[]}'],
  ]) {
    files.push(entry(`inlay/${id}.${ext}`, Buffer.from(payload)));
    metadata.push(
      entry(
        `inlay_sidecar/${id}`,
        Buffer.from(
          JSON.stringify({
            ext,
            name: `${id}.${ext}`,
            type,
            ...(type === "image" ? { width: 2, height: 3 } : {}),
          }),
        ),
      ),
    );
  }
  files.push(
    entry(
      "inlay_meta/picture",
      Buffer.from(
        '{"createdAt":123,"updatedAt":456,"charId":"synthetic-active","chatId":"synthetic-chat"}',
      ),
    ),
    ...metadata,
    entry("database.risudat", encode(expected, compression)),
  );
  writeFileSync(`${directory}pocket-risu-${tag}.bin`, Buffer.concat(files));
}
