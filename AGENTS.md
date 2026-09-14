# AGENTS.md

RisuNest is a cross-platform AI chat application, a fork of RisuAI focused on large-library performance and native app targets. Stack: Svelte 5 + TypeScript, Tauri 2 (Rust backend; generated Android project in `src-tauri/gen/android`), Vite 8, Tailwind CSS 4, pnpm. It lets users chat with many AI models through one interface, with themes, plugins, custom assets, and advanced memory systems.

Platform targets: Windows, macOS, Linux, Android, and iOS, all equal. A feature, and the optimization behind it, belongs on all five; when a task cannot build one platform's equivalent, record the gap instead of letting the platforms diverge. Every target keeps the Svelte WebView UI; a native Rust data core or Compose UI is adopted only when measurements justify it. The web build only has to stay error-free: a feature that is easy natively but unavailable on the web is skipped, disabled, or guarded there, never worked around and never held back for web parity.

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
- Development: `pnpm dev`, `pnpm tauri dev`. Web builds: `pnpm build`, `pnpm buildsite`, `pnpm hono:build`. Desktop: `pnpm tauri build`, `pnpm windows:build`, `pnpm linux:build`, `pnpm macos:build`. Android: `pnpm android:build:emulator` and `pnpm android:build:arm64` (debug APKs), `pnpm android:build:release:arm64`, `pnpm android:build:release:aab`. Release builds pass `--locked` to cargo, so update `Cargo.lock` before building.
- `pnpm test` runs the Vitest suite, `pnpm check` type-checks, `pnpm check:plugin-dts` compiles the plugin API declarations, `pnpm check:production-bundle` builds product bundles and proves they contain no verification code, `pnpm check:benchmark-harnesses` type-checks `benchmarks/`. `pnpm benchmark:phase1` runs the deterministic regex benchmark with fixed output hashes; the other `benchmark:*` scripts are documented in their harness README. Android JVM tests live under `src-tauri/gen/android` (Gradle).
- `package.json` keeps the upstream RisuAI scripts first, then RisuNest additions grouped by area: Vite variants (`tauribuild:*`, `dev:agent`, `build:agent`), checks and tests, desktop (`desktop:*:agent`, `windows:*`, `linux:*`, `macos:*`), `android:*`, `ios:*`, `benchmark:*`. Name a new script `<area>:<action>[:<variant>]`, hyphenate multi-word segments, and add the `:agent` suffix for the agent variant. `tauribuild*` are the Vite steps that `beforeBuildCommand` runs; a harness keeps its own Vite build inline in its Tauri config or runner as `pnpm exec vite build ...` instead of adding a script.
- The repository has no formatter. Match the style of the surrounding file, and run `pnpm check` and the tests relevant to the changed area before committing or submitting a pull request.
- Platform parity is not a per-task test matrix. A shared-path change is verified by `pnpm check` and the tests for the changed area; build or run a platform only when the change reaches platform-conditional code (`isTauri`, `isMobile`, `isAndroid`, `isIOS`, `isNodeServer`), `src-tauri` Rust, or the Android/iOS project files, and then only the platforms that code serves. Never run all five targets for one task.

## RisuRealm

RisuRealm serves third-party content this project does not control. The rule is that none of it reaches an agent's context, screenshots and page reads included. A rule alone cannot un-render a screen, so the block lives in the surrounding tooling rather than in product code: `vitest.setup.ts` fails any Realm request, every benchmark CDP runner blocks Realm for the whole session, vite `--mode agent` swaps `src/ts/realmEndpoints.ts` for an unresolvable-address version, and `scripts/phase3AndroidSmoke.mjs` cuts device networking during `fresh-install`. There is no build flag and default builds are untouched, so there is nothing for an agent to switch on.

- Whenever an agent runs the app and looks at it, use the `:agent` variant of the script: `pnpm dev:agent`, `pnpm desktop:dev:agent`, `pnpm build:agent`, `pnpm desktop:build:agent`, `pnpm windows:build:agent`, `pnpm linux:build:agent`, `pnpm macos:build:agent`, `pnpm android:build:emulator:agent`, `pnpm android:build:arm64:agent`, `pnpm ios:build:simulator:agent`. Plain `pnpm dev`, `pnpm tauri dev` and `pnpm build` stay unblocked for the user.
- Vite loads only `.env.<mode>`, so `.env.agent` must carry what `.env.desktop` and `.env.android` carry.
- `scripts/realmBlocklist.mjs` holds the single path list. Match by path, never by host: `sv.risuai.xyz` also serves account login, backup keys, the Drive OAuth callback and the embedding-model CDN, which stay reachable. `/rs/` is dual-use (account assets and Realm-shared assets), counts as Realm, and must be built only from `realmHubURL` so the vite swap and the blocklist agree.

## Conventions and Architecture

- Svelte 5 runes (`$state`, `$derived`, `$effect`) plus stores in `stores.svelte.ts` (`DBState`, `selectedCharID`, `settingsOpen`, `sideBarStore`, `MobileGUI`, `loadedStore`, `alertStore`, `DynamicGUI`). `.svelte.ts` for rune files, `.svelte` for components, camelCase file names.
- Use the custom theme colors from `src/styles.css` (`textcolor`, `textcolor2`, `bgcolor`, `darkbg`, `darkbutton`, `selected`, `borderc`, `darkborderc`, `draculared`; Tailwind opacity modifiers such as `text-textcolor/90` are fine). Consult `src/ts/gui/colorscheme.ts` only when working on theme logic.
- The revisioned persistent data store (`src/ts/storage/persistentDataStore.ts`) is authoritative; `DBState` is the in-memory compatibility working copy for existing UI and plugin paths. Blob and asset storage sits behind the BlobStore interface (Tauri FS, LocalForage, OPFS, Node backends). Sync implementations stay separate adapters under `src/ts/storage/sync/`; the manifest-delta adapter has no server, do not extend it. `RisuSave` (`.bin`, encryption supported) is the import/export/migration format, not the live store; imports are staged and validated before activation.
- Plugin system (API v3.0): iframe sandboxing, SafeDocument/SafeElement wrappers, save-specific and device-specific storage, custom AI providers, hot reload, and two data-access profiles (scalable API v3 queries, and maximum-compatibility mode preserving the API v2.1 live Proxy). See `plugins.md` and `src/ts/plugins/migrationGuide.md`.
- UI: no router, conditional rendering in `App.svelte`; multiple UI modes (Classic, WaifuLike, WaifuCut) with viewport-based GUI switching; in-app drag-and-drop uses the custom MIME types in `src/ts/dragTypes.ts`.
