// Run only the reference export handler against synthetic storage, never a server.
import path from "node:path";

export async function capturePocketExport(
  source,
  database,
  assets,
  inlays,
  cold,
) {
  const namedFunction = (name) => {
    const start = source.indexOf(`function ${name}(`);
    if (start < 0) throw new Error(`Reference helper missing: ${name}`);
    return source.slice(start, source.indexOf("\n}", start) + 2);
  };
  const start = source.indexOf("app.get('/api/backup/export',");
  if (start < 0) throw new Error("Reference exporter missing");
  const handler = source.slice(start, source.indexOf("\n});", start) + 4);
  const kv = new Map([
    ...assets,
    ["database/database.bin", database],
    [
      "inlay_meta/picture",
      Buffer.from(
        '{"createdAt":123,"updatedAt":456,"charId":"synthetic-active","chatId":"synthetic-chat"}',
      ),
    ],
  ]);
  const files = new Map(
    inlays.flatMap(({ id, ext, type, payload }) => [
      [`${id}.${ext}`, Buffer.from(payload)],
      [
        `${id}.meta.json`,
        Buffer.from(
          JSON.stringify({
            ext,
            name: `${id}.${ext}`,
            type,
            ...(type === "image" ? { width: 2, height: 3 } : {}),
          }),
        ),
      ],
    ]),
  );
  let run;
  const env = {
    Buffer,
    path,
    app: {
      get: (_route, callback) => {
        run = callback;
      },
    },
    checkAuth: async () => true,
    flushPendingDb: async () => {},
    buildFullExportDbValue: async () => database,
    kvGet: (key) => kv.get(key),
    kvSize: (key) => kv.get(key)?.length ?? 0,
    kvList: () => [...cold.keys()],
    kvListWithSizes: (prefix) =>
      [...kv]
        .filter(([key]) => key.startsWith(prefix))
        .map(([key, value]) => ({ key, size: value.length })),
    readColdStorageJsonEntry: (key) => ({ coldData: cold.get(key) }),
    normalizeColdStorageStorageKey: (key) => key,
    listInlayFiles: async () =>
      inlays.map(({ id, ext }) => ({ id, ext, filePath: `${id}.${ext}` })),
    getInlaySidecarPath: (id) => `${id}.meta.json`,
    fs: {
      stat: async (key) => ({ size: files.get(key).length }),
      readFile: async (key) => files.get(key),
    },
  };
  new Function(
    ...Object.keys(env),
    [
      namedFunction("encodeBackupEntry"),
      namedFunction("toColdStorageBackupName"),
      namedFunction("listColdStorageBackupEntries"),
      handler,
    ].join("\n"),
  )(...Object.values(env));
  const chunks = [];
  const headers = new Map();
  let ended = false;
  await run(
    { query: {} },
    {
      setHeader: (key, value) => headers.set(key, value),
      once: () => {},
      write: (value) => {
        chunks.push(value);
        return true;
      },
      end: () => {
        ended = true;
      },
    },
    (error) => {
      throw error;
    },
  );
  const archive = Buffer.concat(chunks);
  if (!ended || archive.length !== headers.get("content-length"))
    throw new Error("Incomplete synthetic export");
  return archive;
}
