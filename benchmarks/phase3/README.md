# Phase 3 Windows Tauri CDP benchmark

Run the release benchmark from the repository root:

```powershell
$env:VITE_DISABLE_REALM = 'true'
pnpm benchmark:phase3:tauri -- --output src-tauri/target/phase3-tauri-cdp.json
```

Run the runner unit tests with:

```powershell
$env:VITE_DISABLE_REALM = 'true'
node --test benchmarks/phase3/tauri-cdp.node-test.mjs
```

The runner uses Node 22 built-ins and raw CDP. It creates a temporary Tauri identifier and WebView data directory, builds a release executable, waits for `boot:interactive`, and records boot timing, JavaScript heap, Windows working-set samples, DOM nodes, mounted messages, and distinct DOM-referenced live resource URLs.

For a meaningful G6 interval, it explicitly stages a deterministic 64 MiB root through the existing `pds_replace_begin`, `pds_replace_put_root`, and `pds_replace_commit` commands. It then invokes the real `pds_snapshot_create` command and reports WebView long tasks that overlap the snapshot interval. The isolated profile is deleted after the app exits unless `--keep-profile` is passed.

This is benchmark-only explicit import setup. It does not inspect IndexedDB or LocalForage, migrate a legacy store, automate the public file picker, or claim the full `save-large` fixture. Live RisuRealm and account access are not used.
