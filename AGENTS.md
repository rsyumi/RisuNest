# AGENTS.md

RisuNest is a cross-platform AI chat application, a fork of RisuAI focused on large-library performance and native app targets. Stack: Svelte 5 + TypeScript, Tauri 2 (Rust backend; generated Android project in `src-tauri/gen/android`), Vite 8, Tailwind CSS 4, pnpm. It lets users chat with many AI models through one interface, with themes, plugins, custom assets, and advanced memory systems.

Platform targets: Windows, macOS, Linux, Android, and iOS, all equal. A feature, and the optimization behind it, belongs on all five; when a task cannot build one platform's equivalent, record the gap instead of letting the platforms diverge. Every target keeps the Svelte WebView UI; a native Rust data core or native UI is adopted only when measurements justify it. The web build only has to stay error-free: a feature that is easy natively but unavailable on the web is skipped, disabled, or guarded there, never worked around and never held back for web parity.

## Fork Scope

- Keep upstream RisuAI features and options in their original settings locations. The RisuNest settings tab is reserved for RisuNest-added features and options; never move upstream controls into it.
- Provider-specific request formatting, SSE decoders, and response parsers follow upstream RisuAI. Do not refactor them locally except for correctness fixes.
- Optimization targets are the provider-independent paths: regex scripts, lorebook, Lua, stream postprocessing, rendering, storage, and sync.
- Prefer narrow platform adapters and compatibility seams that minimize conflicts with upstream RisuAI.

## Layout

- `src/ts/` TypeScript logic with tests colocated as `*.test.ts`; `src/lib/` Svelte components; `src/lang/` i18n; `src-tauri/` Rust backend and the generated Android shell; `crates/` RisuNest's own Rust crates and the patched upstream crates that `[patch.crates-io]` substitutes.
- `server/` holds RisuNest Sync: the Rust server and manager, plus two independent frontends (`server/manager/gui`, `server/endpoint-registry`) with their own lockfiles, installed with `pnpm --ignore-workspace`.
- `tests/` and `benchmarks/` hold verification harnesses that consume product code; `tests/README.md` describes the product/verification boundary.

## Building and Testing

- Build and run commands are in `README.md` and `package.json`. CI builds with `--locked` and `--frozen-lockfile`; a dependency change updates every lockfile that resolves it, including `crates/*`, `tests/native/native` and `benchmarks/*/native`.
- The repository has no formatter except `cargo fmt` for `server/sync` and `server/manager`, which CI checks, and CI runs `cargo clippy --all-targets -- -D warnings` on `server/sync`. Match the style of the surrounding file, and run `pnpm check` and the tests relevant to the changed area before committing or submitting a pull request.
- The changed area's tests do not reach everything CI runs. Before committing, `git grep` each changed product file's path and changed exports across `tests/`, `scripts/`, `benchmarks/`, `tools/` and `.github/`, and run what those consumers drive; some harnesses read or transpile product source as text (`scripts/compatibleExportReferenceHarness.mjs` strips imports), so a new import or renamed export breaks them without a type error.
- A check this machine cannot run gets a review of the code it executes. A check that cannot start (missing install, reference checkout or toolchain) is not a pass: prepare it the way the workflow does, or report it as unverified.
- `src-tauri` Rust needs `cargo test --lib`, since `cargo check` does not compile tests; code behind a feature, `cfg(unix)` or a target compiles only where CI builds it, so check that configuration or review it.
- Added, moved or removed test files: `pnpm check:test-inventory`. Edits under `.github/`, `scripts/release/` or `release-notes/`: `pnpm test:release`.
- `pnpm test:full:shared` and `pnpm test:full` are the complete gates. They fail on missing prerequisites and never claim verification of another OS. `pnpm test:related` is local feedback only, not acceptance evidence.
- Benchmark and measurement harness tests stay out of `pnpm test`, `pnpm test:node` and CI; `pnpm test:harness` runs them.
- Platform parity is not a per-task test matrix. A shared-path change is verified by `pnpm check` and the tests for the changed area; build or run a platform only when the change reaches platform-conditional code (`isTauri`, `isMobile`, `isAndroid`, `isIOS`), `src-tauri` Rust, or the Android/iOS project files, and then only the platforms that code serves. Never run all five targets for one task.
- Import, export or RisuSave changes run `pnpm test:node` with the pinned references. Node compatibility contracts compare against pinned RisuAI and PocketRisu checkouts; `.github/actions/test-references` and `tests/compatibleExportREADME.md` give the commits and the override variables. Never start or install the reference applications.
- `package.json` keeps the upstream RisuAI scripts first, then RisuNest additions grouped by area. Name a new script `<area>:<action>[:<variant>]` and hyphenate multi-word segments. `tauribuild*` are the Vite steps that `beforeBuildCommand` runs; a harness keeps its own Vite build inline in its Tauri config or runner instead of adding a script.

## Writing Tests

- A test belongs only if it fails when a product behavior breaks and no other test would catch it. Temporarily break the product line a new test guards and confirm it fails; a test that passes either way, or because a mock returns the answer, does not count.
- A fix extends the nearest existing test instead of adding a near-copy. Never add a test whose only assertion is that removed code or a never-released shape stays gone.
- Assert what a user or caller observes (stored or returned data, the command sent, what the screen offers), not call counts, argument arrays or private helpers, unless that is the contract. When a change breaks a pinned internal detail, move the assertion to the outcome instead of updating the detail.
- Mock only process boundaries (Tauri `invoke`, network, file system, clock) and use real modules for pure code and `src/lang`. Share one factory typed with `satisfies typeof import(...)` instead of copying a mock, so an export change fails `pnpm check` in one place.
- Assert UI text only where code chooses or builds it, through language keys, and source or workflow text only for a named rule or cross-file contract, never style. Read product files through `node:fs`, not `?raw` imports, which can hide uncovered Svelte lines from V8 coverage.
- Browser and E2E tests drive the real UI or public commands; a fixture adapter never reimplements a product module.
- Keep tests small and portable: inject a limit to prove the bound at small N, wait on conditions instead of sleeping, and depend on no machine path, port or tool on PATH.
- Unit tests must not make unplanned network requests: the fetch and happy-dom guards fail teardown even for caught errors. A test that owns a loopback server allows one exact origin with `allowTestLoopbackOrigin` from `tests/support/testNetwork.ts` and revokes it in teardown.

## Conventions and Architecture

- Svelte 5 runes (`$state`, `$derived`, `$effect`); shared stores live in `src/ts/stores.svelte.ts`. Use `.svelte.ts` for rune modules, `.svelte` for components, and camelCase file names.
- Use the custom theme colors from `src/styles.css` (`textcolor`, `textcolor2`, `bgcolor`, `darkbg`, `darkbutton`, `selected`, `borderc`, `darkborderc`, `draculared`; Tailwind opacity modifiers such as `text-textcolor/90` are fine). Secondary text on RisuNest-added screens uses `subtext`; upstream screens keep `textcolor2`. Consult `src/ts/gui/colorscheme.ts` only when working on theme logic.
- The revisioned persistent data store (`src/ts/storage/persistentDataStore.ts`) is authoritative; `DBState` is the in-memory compatibility working copy for existing UI and plugin paths. Blob and asset storage sits behind the BlobStore interface. Sync implementations stay separate adapters under `src/ts/storage/sync/`. `RisuSave` is the import/export/migration format, not the live store; imports are staged and validated before activation.
- Plugin system (API v3.0 only): iframe sandboxing, SafeDocument/SafeElement wrappers, save-specific and device-specific storage, custom AI providers, hot reload, and scalable API v3 queries. API v2.1 bundles cannot be installed and stored v2.1 records are skipped at load. See `plugins.md` and `src/ts/plugins/migrationGuide.md`.
- UI: no router, conditional rendering in `App.svelte`; the `theme` setting selects the GUI layout; in-app drag-and-drop uses the custom MIME types in `src/ts/dragTypes.ts`.
- Write source comments in English, matching the surrounding file's density, and only when a comment is genuinely needed.
- Product runtime must not import or enable test or benchmark code, even behind flags. Verification harnesses, test-only access and instrumentation live in separate entries and builds that consume product code and use separate output directories. Colocated tests and Rust `#[cfg(test)]` are fine when excluded from normal builds; `pnpm check:production-bundle` proves the shipped bundles, workers and source maps contain none of it; run it when a change adds an entry point or worker.
