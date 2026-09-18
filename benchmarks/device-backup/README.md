# Synthetic native device backup smoke

This separate agent build exercises the production native file-job wrappers and native device stores. Product entries never import this directory. The fixture writes synthetic Hypa vectors, plugin-local values and device settings through their product APIs. The Hypa fixture carries 8 MiB of vector data. A browser-local sentinel remains outside the native sections.

The baseline run exports `hypa`, `local-plugins` and `local-settings` through `runNativeArchiveExport`, publishes the archive to an app-owned path, reloads the WebView, changes every selected section, and restores through `runNativeArchiveRestore`. It compares product-visible fingerprints, confirms the browser-local sentinel survived, then cancels a second native export while its status is `writing-export`. The Hypa fingerprint covers keys, dimensions and exact vector bytes. Full Hypa metadata and owner codec roundtrips remain native Rust coverage because the product read API does not expose those fields. Reports retain native job phase transitions, progress, durations, renderer heap, and platform process memory where available.

Build the web entry with `node node_modules/vite/bin/vite.js build --mode agent --config benchmarks/device-backup/vite.config.ts`. Native Android builds use `benchmarks/device-backup/tauri.android.conf.json`. Keep the shared Cargo target directory required by the project. Platform runners and their `.tmp` result files provide the execution evidence. A successful build alone does not establish a passing smoke test.

Windows runs require a newly generated synthetic native identifier and a separate WebView data directory. The runner verifies the title, identifier and native directory before starting. Android runs require the dedicated `risunest_vm_retest` AVD, the documented private ADB server, sustained health and transfer checks, and the agent harness APK. Read the main checkout's `docs/android-testing.md` before running Android. Never point these runners at an existing installed app profile, a user's AVD, or a user-provided backup.

Run a fresh Windows fixture with `node benchmarks/device-backup/windows-smoke.mjs`. Its checked `result.json` identifies the unique profile and executable hash. The runner retains the published synthetic archive and an `exchange.json` descriptor with its SHA-256 and three section fingerprints. After a successful baseline, reuse only that generated profile:

```text
node benchmarks/device-backup/windows-smoke.mjs --reuse=<baseline-result.json> --peer=<other-platform-exchange.json>
node benchmarks/device-backup/windows-smoke.mjs --reuse=<baseline-result.json> --fault=activating-database
```

The fault run waits until the production restore job reports `state=running`, `phase=activating-database`, then terminates only the owned synthetic process. The next process runs the product startup recovery entry before the harness. Because that phase starts after the restore is accepted, recovery must leave the complete committed section set, and normal PDS access must reopen. Exact section and commit-marker crash windows remain deterministic Rust recovery tests because the native file-job status contract does not expose individual section boundaries.

A hard kill can discard the last test-control localStorage write. The Windows runner handles only that exact control failure when its external kill record proves the observed phase and the product startup recovery completed. It restores the checked synthetic control and reloads for an independent fingerprint comparison. Normal completed-run shutdown requests a graceful browser close. Injected fault shutdown remains a hard process kill.

The Android runner accepts `--adb=<adb-executable> --apk=<synthetic-apk> --health=<health-summary.json> --output=<result.json>`. It requires `ANDROID_ADB_SERVER_PORT=15037` and `ADB_SERVER_SOCKET=tcp:127.0.0.1:15037`. Its `--exchange=<descriptor.json>` output can be used by Windows. A later Android invocation with `--peer=<windows-exchange.json>` imports that exact checked synthetic archive into the still-running completed fixture without reinstalling it. Never reuse this mode against an arbitrary installed package or device.

Only an actual passing result establishes coverage for the chosen scenario. This smoke does not establish every native section boundary, library commit-marker crash window, additional native windows, large-library limits, or exact peak memory. Those remain separate acceptance checks.
