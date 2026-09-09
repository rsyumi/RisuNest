# AGENTS.md

RisuNest is a cross-platform AI chat application, a fork of RisuAI focused on large-library performance and native app targets. Stack: Svelte 5 + TypeScript, Tauri 2 (Rust backend; generated Android project in `src-tauri/gen/android`), Vite 8, Tailwind CSS 4, pnpm. It lets users chat with many AI models through one interface, with themes, plugins, custom assets, and advanced memory systems.

Platform priorities: Android and Windows first; macOS, iOS, and Linux second. Every target keeps the Svelte WebView UI; a native Rust data core or Compose UI is adopted only when measurements justify it.

## Fork Scope

- Keep upstream RisuAI features and options in their original settings locations. The RisuNest settings tab is reserved for RisuNest-added features and options; never move upstream controls into it.
- Provider-specific request formatting, SSE decoders, and response parsers follow upstream RisuAI. Do not refactor them locally except for correctness fixes; merge-conflict cost outweighs the benefit.
- Optimization targets are the provider-independent paths: regex scripts, lorebook, Lua, stream postprocessing, rendering, storage, and sync.
- Prefer narrow platform adapters and compatibility seams that minimize conflicts with upstream RisuAI.

## Layout

- `src/ts/`: TypeScript logic, tests colocated as `*.test.ts`. `storage/` (revisioned persistent store, save coordinator, BlobStore, sync adapters), `process/` (chat, requests, memory, models, templates, MCP), `plugins/`, `gui/`, `drive/`, `translator/`, `model/`, `sync/`; entry points `bootstrap.ts`, `stores.svelte.ts`, `globalApi.svelte.ts`, `parser.svelte.ts`.
- `src/lib/`: Svelte UI (`ChatScreens/`, `UI/`, `Setting/`, `SideBars/`, `Others/`, `Mobile/`, `Playground/`, `VisualNovel/`, `LiteUI/`). `src/lang/`: i18n (en, ko, cn, zh-Hant, vi, de, es).
- `src-tauri/`: Rust backend and generated Android shell. `server/node/` and `server/hono/`: self-hosting servers. Also `public/`, `resources/`, `dist/`, `.github/workflows/`.

## Building and Testing

- Prerequisites: Node.js 20.19+ or 22.12+, pnpm, Rust and Cargo, Android SDK/NDK (compile and target SDK 36, min SDK 24, NDK 28).
- Development: `pnpm dev`, `pnpm tauri dev`. Builds: `pnpm build`, `pnpm buildsite`, `pnpm tauribuild` or `pnpm tauri build`, `pnpm android:build:emulator`, `pnpm android:build:arm64`, `pnpm hono:build`.
- `pnpm test` runs the Vitest suite, `pnpm check` type-checks, `pnpm benchmark:phase1` runs the deterministic regex benchmark with fixed output hashes. Android JVM tests live under `src-tauri/gen/android` (Gradle).
- Prettier formats the code. Run `pnpm check` and format changed files before committing or submitting a pull request.

## RisuRealm

RisuRealm serves third-party content this project does not control. The rule is that none of it reaches an agent's context, screenshots and page reads included. A rule alone cannot un-render a screen, so the block lives in the surrounding tooling rather than in product code, there is no build flag, and default builds are untouched:

- `pnpm test` blocks Realm requests from `vitest.setup.ts`: `fetch` throws synchronously naming the caller, and a happy-dom fetch interceptor stops iframe, script and XHR loads that never touch `fetch`. `src/ts/realmEndpoints.test.ts` pins both.
- Every benchmark CDP runner blocks Realm for the whole session with `Network.setBlockedURLs`, so they still measure the production bundle.
- Whenever an agent runs the app and looks at it, use the `:agent` scripts: `pnpm dev:agent`, `pnpm tauri:dev:agent`, `pnpm build:agent`, `pnpm windows:build:agent`, `pnpm tauri:build:agent`, `pnpm android:build:emulator:agent`, `pnpm android:build:arm64:agent`. They run vite with `--mode agent`, which swaps `src/ts/realmEndpoints.ts` for an unresolvable-address version; the Tauri variants route through `src-tauri/tauri.agent.conf.json`, and `.claude/launch.json` points the app-preview tooling at `dev:agent`. Vite loads only `.env.<mode>`, so `.env.agent` must carry what `.env.desktop` and `.env.android` carry. Plain `pnpm dev`, `pnpm tauri dev` and `pnpm build` stay unblocked for the user.
- `scripts/phase3AndroidSmoke.mjs` cuts device networking during `fresh-install` and refuses to continue until `dumpsys connectivity` reports no default network; `restore-network` turns it back on.

`scripts/realmBlocklist.mjs` holds the single path list. `sv.risuai.xyz` is not a Realm-only host: account backup keys, the Drive OAuth callback, the embedding-model CDN and account login share it and stay reachable, so Realm is matched by path, never by host. `/rs/` is the one dual-use path (account assets and Realm-shared assets); it is treated as Realm, and product code builds it only from `realmHubURL` so the vite swap and the blocklist agree.

## Conventions and Architecture

- Svelte 5 runes (`$state`, `$derived`, `$effect`) plus stores in `stores.svelte.ts` (`DBState`, `selectedCharID`, `settingsOpen`, `sideBarStore`, `MobileGUI`, `loadedStore`, `alertStore`, `DynamicGUI`). `.svelte.ts` for rune files, `.svelte` for components, camelCase file names.
- Use the custom theme colors from `src/styles.css` (`textcolor`, `textcolor2`, `bgcolor`, `darkbg`, `darkbutton`, `selected`, `borderc`, `darkborderc`, `draculared`; Tailwind opacity modifiers such as `text-textcolor/90` are fine). Consult `src/ts/gui/colorscheme.ts` only when working on theme logic.
- The revisioned persistent data store (`src/ts/storage/persistentDataStore.ts`) is authoritative; `DBState` is the in-memory compatibility working copy for existing UI and plugin paths. Blob and asset storage sits behind the BlobStore interface (Tauri FS, LocalForage, OPFS, Node backends). Sync adapters live in `src/ts/storage/sync/` (official account snapshot, Drive snapshot, manifest delta; the delta adapter has no server yet, do not extend it). `RisuSave` (`.bin`, encryption supported) is the import/export/migration format, not the live store; imports are staged and validated before activation.
- Plugin system (API v3.0): iframe sandboxing, SafeDocument/SafeElement wrappers, save-specific and device-specific storage, custom AI providers, hot reload, and two data-access profiles (scalable API v3 queries, and maximum-compatibility mode preserving the API v2.1 live Proxy). See `plugins.md` and `src/ts/plugins/migrationGuide.md`.
- UI: no router, conditional rendering in `App.svelte`; multiple UI modes (Classic, WaifuLike, WaifuCut) with viewport-based GUI switching; in-app drag-and-drop uses the custom MIME types in `src/ts/dragTypes.ts`.
