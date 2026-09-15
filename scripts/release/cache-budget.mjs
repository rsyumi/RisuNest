import { lstat, readdir } from "node:fs/promises";
import { resolve } from "node:path";

async function size(path) {
  let stats;
  try {
    stats = await lstat(path);
  } catch (error) {
    if (error.code === "ENOENT") return 0;
    throw error;
  }
  if (stats.isSymbolicLink()) return 0;
  if (stats.isFile()) return stats.size;
  if (!stats.isDirectory()) return 0;
  const entries = await readdir(path);
  let total = 0;
  for (const entry of entries) total += await size(resolve(path, entry));
  return total;
}

async function main(argv) {
  let maximum;
  const paths = [];
  for (let index = 0; index < argv.length; index += 2) {
    const name = argv[index];
    const value = argv[index + 1];
    if (!name?.startsWith("--") || value === undefined) throw new Error("Invalid cache budget arguments.");
    if (name === "--max-bytes") maximum = Number(value);
    else if (name === "--path") paths.push(resolve(value));
    else throw new Error(`Unknown cache budget argument ${name}.`);
  }
  if (!Number.isSafeInteger(maximum) || maximum <= 0 || !paths.length)
    throw new Error("Cache budget requires --max-bytes and at least one --path.");
  let total = 0;
  for (const path of paths) total += await size(path);
  if (total > maximum) throw new Error(`Download caches use ${total} bytes, above the ${maximum} byte per-runner share of the 8 GB repository target.`);
  process.stdout.write(`${JSON.stringify({ bytes: total, maximum, paths })}\n`);
}

await main(process.argv.slice(2));
