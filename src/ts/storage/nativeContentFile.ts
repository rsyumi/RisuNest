import { NativeFileJobError, type NativeFileJobOptions } from './nativeFileJobs'
import { language } from 'src/lang'
import { join } from "@tauri-apps/api/path";
import { nativeDataPath } from "./nativePaths";
import { mkdir, open, readDir, remove, stat } from "@tauri-apps/plugin-fs";
import { runContentImport } from "./contentImportOperation";
import { syntheticNativeFileJobStatus } from "./nativeFileJobs";

/** The private inbox for dropped files. Stale copies an interrupted process left behind are removed first. */
export async function openContentInbox(): Promise<string> {
  const directory = await nativeDataPath("content-import-inbox");
  await mkdir(directory, { recursive: true });
  await cleanupContentInbox(directory);
  return directory;
}

/** Spools a dropped file into the inbox in bounded chunks and returns the copy's path; a failed copy is removed. */
export async function copyFileToContentInbox(
  directory: string,
  file: File,
  options: Pick<NativeFileJobOptions, "signal" | "onStatus"> = {},
): Promise<string> {
  const suffix = file.name.split(".").at(-1)?.toLowerCase() ?? "bin";
  const path = await join(
    directory,
    `${crypto.randomUUID()}.${/^[a-z0-9]+$/.test(suffix) ? suffix : "bin"}`,
  );
  const handle = await open(path, { write: true, createNew: true });
  const reader = file.stream().getReader();
  let copied = 0;
  let lastUpdate = 0;
  try {
    try {
      while (true) {
        options.signal?.throwIfAborted();
        const { done, value } = await reader.read();
        if (done) break;
        for (let offset = 0; offset < value.length; ) {
          const written = await handle.write(value.subarray(offset));
          if (written === 0) throw new Error("Unable to spool import source");
          offset += written;
        }
        copied += value.length;
        if (performance.now() - lastUpdate >= 100) {
          lastUpdate = performance.now();
          options.onStatus?.(
            syntheticNativeFileJobStatus(
              { kind: "prepare-content-import" },
              "copying-source",
              {
                stageUnit: "bytes",
                stageCompleted: copied,
                stageTotal: file.size,
              },
            ),
          );
        }
      }
    } finally {
      await reader.cancel().catch(() => {});
      reader.releaseLock();
      await handle.close();
    }
  } catch (error) {
    await remove(path).catch((cleanup) =>
      console.warn("Import source cleanup failed", cleanup),
    );
    throw error;
  }
  return path;
}

/** HTML drops expose a File, not an OS path. Spool bounded chunks once, then let Rust extract. */
export async function importNativeContentFile(
  file: File,
  destination: "character" | "module",
): Promise<string | null> {
  return runContentImport(file.name, {}, async (options) => {
    const path = await copyFileToContentInbox(
      await openContentInbox(),
      file,
      options,
    );
    try {
      const input = {
        source: { type: "desktopPath" as const, path },
        displayName: file.name,
      };
      if (destination === "module") {
        const { importPreparedNativeModuleContent } = await import(
          "../process/modules"
        );
        const result = await importPreparedNativeModuleContent(input, options);
        return result.kind === "imported" ? result.value : null;
      }
      const [
        { importDesktopNativeCharacterPath },
        { importPreparedNativeCharacterContent, importCharacterProcess },
        { getDatabase },
      ] = await Promise.all([
        import("./nativeCharacterFileRoute"),
        import("../characterCards"),
        import("./database.svelte"),
      ]);
      // Preserve upstream-only card variants through the established fallback policy.
      const result = await importDesktopNativeCharacterPath(path, {
        nativeEnabled: () => true,
        readDesktopPath: async () => new Uint8Array(await file.arrayBuffer()),
        nativeImport: () =>
          importPreparedNativeCharacterContent(input, options),
        legacyImport: async ({ data }) => {
          const index = await importCharacterProcess({ name: file.name, data, quiet: true });
          return typeof index === "number"
            ? (getDatabase().characters[index]?.chaId ?? null)
            : null;
        },
      });
      if (result.kind === "destination-required")
        throw new NativeFileJobError(
          "destination-required", language.risuNest.importDialog.reasonPlainJpeg,
        );
      return result.kind === "imported" ? result.value : null;
    } finally {
      await remove(path).catch((error) =>
        console.warn("Import source cleanup failed", error),
      );
    }
  });
}

/** Recover only this adapter's stale, private source files after an interrupted process. */
async function cleanupContentInbox(directory: string): Promise<void> {
  const staleBefore = Date.now() - 24 * 60 * 60 * 1000;
  const entries = await readDir(directory);
  for (const entry of entries) {
    if (
      !entry.isFile ||
      entry.isSymlink ||
      !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\.[a-z0-9]+$/.test(
        entry.name,
      )
    )
      continue;
    const path = await join(directory, entry.name);
    try {
      const metadata = await stat(path);
      if (metadata.mtime && metadata.mtime.getTime() < staleBefore)
        await remove(path);
    } catch {
      /* A concurrent cleanup may already have removed the stale source. */
    }
  }
}
