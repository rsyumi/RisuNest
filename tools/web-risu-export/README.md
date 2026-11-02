# Web RisuAI export for RisuNest

Export saved web RisuAI data, ordinary assets, archived chats, and **all inlays stored in the current browser**, including unreferenced inlays, into a RisuNest-compatible `.bin` backup.

Supports local IndexedDB/OPFS storage and RisuAI account storage. The storage adapter targets RisuAI **v2026.10.292**. The panel supports Korean and English, with a language selector.

## Use

1. Open your usual RisuAI page in the browser/profile containing your data. Finish saving, then stop generation and editing in every tab and device.
2. Choose either delivery method:
   - **Userscript:** install [dist/risunest-export.user.js](dist/risunest-export.user.js) in your userscript manager, then reload RisuAI.
   - **Console:** paste the complete contents of [dist/risunest-export.console.js](dist/risunest-export.console.js) into that page's developer console.
3. In the export panel, confirm that saving has finished and select **Export backup**.
4. Chrome/Edge: choose the destination file. Firefox: wait for **Backup file ready**, then select **Download backup**. Keep the tab open until the browser finishes the download.
5. Restore the downloaded `.bin` file through RisuNest's backup restore control.

The panel shows the current stage, completed item count, written bytes, elapsed time, and write speed. When the export finishes, it also lists any items that were skipped. Cancel stops the export. Installation only opens the panel; it does not start a backup. Re-running the script reuses the panel. Automatic userscript updates are disabled for the initial distribution; install a newer version manually.

For a self-hosted web RisuAI origin that keeps data in the browser, use the console bundle. The installed userscript matches `risuai.xyz` and `www.risuai.xyz`. The RisuAI Node server keeps saved data in its `save` folder on the server, not in the browser, so the panel stops on such a page; back up that folder instead.

## Storage and compatibility

- Chrome/Edge write incrementally to the selected file. Firefox uses an OPFS temporary file and needs additional browser storage roughly equal to the archive size. A normal download has no reliable page-level completion callback. Check browser downloads, then use **Remove temporary files**. If the browser reports insufficient storage, the panel offers **Request storage permission**; retry after permission is granted and enough space is available.
- Media is copied without re-encoding. Large files are streamed, and the database is parsed incrementally. A single IndexedDB byte-array value still has to be read whole, so a 1 GB database can require substantial memory.
- The archive may exceed 4 GiB in total. There is no fixed entry-count or combined filename-size limit. Each individual entry must be smaller than 4 GiB, and each filename is limited to 1 MiB of UTF-8. Available memory and storage still bound the backup size. Restore backups exceeding 200,000 entries or 64 MiB of combined filenames with RisuNest 1.0.5 or newer; older versions reject them. Database-block and record limits also apply.
- Account mode downloads missing referenced assets and archived chats using the current login. It also includes all local inlays. Unreferenced remote assets and data held only in another origin/profile are not discoverable from this browser. A download that stalls for 60 seconds or ends early restarts that item, up to three times with increasing delays.
- The exporter reads persisted data. It cannot include unsaved edits or obtain an atomic snapshot across browser stores and account servers. It checks database hashes and key inventories before completing; keep the application idle.
- An interrupted or failed export is not a complete backup. A database the exporter cannot read stops the export. Referenced assets or archived chats that cannot be found, archived chats that cannot be read, and inlays in an unsupported format are skipped and counted, and the rest of the backup is completed.
- Only archived chats referenced by the database are exported. Cold storage files nothing refers to are unused by RisuAI and are left out.
- Inlays saved before RisuAI recorded an inlay type are exported as images. An unset plugin storage block, which RisuAI writes as an empty block, is written as an empty object so RisuNest 1.0.4 and earlier can restore it.
- RisuAI keeps plugin storage values in cold storage. The exporter writes them back into the database's plugin storage as plain values, which RisuNest restores. Values that cannot be found or read, or that would push plugin storage past the 1 GiB block limit, are skipped and counted. A plugin storage block over 64 MiB, or one in the older MessagePack database format, is copied unchanged, and its cold-stored values are skipped and counted.
- SaveKV blocks (`kv:` names) are copied unchanged. RisuNest skips them on restore.
- Individual character fields and messages are limited to 64 MiB of serialized UTF-8 JSON. Database blocks have a 1 GiB decoded limit.
- Backups include saved settings and may contain credentials. Keep the file private. The exporter does not upload backup data or print record contents.
- Source data is not migrated, rewritten, or deleted. Account authentication may refresh its login token. Firefox cleanup removes only this tool's temporary files and their folder.

## Build and test

Requires Node.js 22.12 or newer. Run inside this folder:

```sh
npm ci --ignore-scripts --workspaces=false
npm run build
npm test
npm run test:browser
```

Browser tests use Playwright and a temporary synthetic profile. Install its Chromium browser first if necessary: `npx playwright install chromium`. No real account or user data is needed. `npm run test:firefox` runs the Firefox fallback after `npx playwright install firefox`. `npm run test:large` checks streaming with a synthetic 1 GiB database and 10 GiB of media without writing a large file. `npm run test:native` requires Cargo; it checks the archive with RisuNest's container parser and restores it through RisuNest's backup restore code, which compiles the `src-tauri` tests.

The two bundled scripts are generated from the same implementation. Commit the generated `dist` files with source changes, and distribute both scripts together with [dist/THIRD_PARTY_LICENSES.md](dist/THIRD_PARTY_LICENSES.md). Dependencies are pinned in `package-lock.json`; no external runtime script is loaded. This folder is independent of the RisuNest application build.
