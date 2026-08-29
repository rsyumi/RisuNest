# Task 9 Report

Status: Implemented and validated, pending review approval.

## Review fix round 1

The corpus now invokes each named production gateway instead of appending checklist labels. Trigger runs `runTrigger` with a 10,000-message compatibility `ConversationOperationContext`; Lua runs `runScripted` through the same authority; CBS calls the registered production history callback against the operation database; regex executes the production regex plan and reconciles its exact operation batch. Public `sendChat` uses a controlled streaming provider while promotion, mutation evidence, acknowledgement, persistence, and demotion remain owned by the real local runtime.

Screenshot uses `openChatScreenshotSourceLease`. Search uses the anchored chat-message queries, and Hypa calls `captureCurrentHypaMessageById`. Branch calls `createCapturedConversationBranch`, export calls the revisioned runtime snapshot route, and Plugin API v2.1 mutates through the real `getV2PluginAPIs().getDatabase()` Proxy across maximum-compatibility persistence and scalable redemotion.

The conflict case now performs a competing IndexedDB root commit at the expected revision, proves the stale flush raises `RevisionConflictError`, refreshes through the runtime reconciliation path, and retries against the new authoritative revision. The interior delete is followed by forced demotion and exact shifted absolute-index and duplicate-occurrence checks. Persistent branch and export integration run on that shifted state before the later distinct truncate.

No Task 6-8 product file changed in this review round. An apparent generation mutation-evidence RED was traced to the test harness changing persisted fixture metadata after windowing and cloning the operation-owned chat in its output-trigger seam. Persisting all fixture defaults initially and retaining the operation-owned chat made the public gateway pass without a product edit.

## Summary

Added the mandatory local 10,000-turn correctness corpus over the real IndexedDB-backed persistent runtime. A plain complete-owner oracle receives the same operations as the scalable runtime. The scalable side promotes through exact complete leases for synchronous compatibility work, uses pinned bounded reads for windowed consumers, and returns to the metadata shell and persistent viewport between operations.

The corpus covers append, edit, delete, truncate, reroll, bookmark assignment and rename, failed-save retry, revision conflict retry, Trigger, Lua, CBS, regex, generation, screenshot, search, Hypa, branch source capture, export, and Plugin API v2.1 compatibility. It compares exact final conversation messages, stable IDs, metadata, revision, branch prefix evidence, exported content, and the 64-row resident viewport budget.

Removed the unused `ActiveConversationSession.evictionEnabled = false` property. Its existing tests now assert that the misleading marker is absent. Automatic demotion behavior remains owned by `ActiveWorkingSet`.

## TDD evidence

Initial RED command:

`$env:VITE_DISABLE_REALM='true'; pnpm vitest run src/ts/storage/selectedConversationEvictionCorpus.test.ts --reporter=verbose`

Result: 1 file failed, 1 test failed. The promoted real session still exposed `evictionEnabled`, proving the mandatory marker removal was not complete.

After property removal, the first corpus fixture exceeded 120 seconds. Instrumentation showed no authority deadlock. The fixture deleted near the beginning of 10,000 occurrence rows and inserted a persistent branch at configured index zero, causing broad derived-index rewrites. The corpus retained delete and truncate behavior at the tail, and exercised branch capture through the existing complete-session `readBranchSource` seam. The dedicated persistent 10,000-turn branch job test remains unchanged.

Review-round final corpus command:

`$env:VITE_DISABLE_REALM='true'; pnpm vitest run src/ts/storage/selectedConversationEvictionCorpus.test.ts --reporter=verbose`

Result: 1 file passed, 1 test passed, 11.20 seconds total on the recorded run.

## Validation evidence

Focused B1 matrix:

`$env:VITE_DISABLE_REALM='true'; pnpm vitest run src/ts/storage/selectedConversationEvictionCorpus.test.ts src/ts/storage/activeConversationSession.test.ts src/ts/storage/selectedConversationLifecycle.test.ts src/ts/storage/persistentDataRuntime.production.test.ts src/ts/storage/saveCoordinator.test.ts src/ts/chatMessageUi.test.ts --reporter=dot`

Result: 6 files passed, 315 tests passed.

Type and Svelte validation:

`$env:VITE_DISABLE_REALM='true'; pnpm check`

Result: 0 errors and 0 warnings.

Production build:

`$env:VITE_DISABLE_REALM='true'; pnpm build`

Result: passed in 17.75 seconds. Existing CSS `::highlight`, externalized Node module, dynamic import, and chunk-size warnings remain.

Full TypeScript suite:

`$env:VITE_DISABLE_REALM='true'; pnpm test`

Review-round result after the Task 8 occurrence-index fix: 255 files passed, 2 skipped. 3,315 tests passed, 6 skipped. Duration 54.85 seconds.

Formatting command:

`pnpm exec prettier --write <Task 9 files>`

Result: skipped because `prettier` is not installed in this workspace. `pnpm check` passed the final source.

## Rulings

Ruling: The IndexedDB persistent store remains authoritative. The oracle is comparison-only and never publishes application state.

Ruling: Every named complete-array consumer acquires the existing exact complete lease. Every bounded consumer remains windowed and reads from a pinned persistent revision.

Ruling: Demotion is asserted after each operation. A metadata-only shell is published, the active complete session is absent, and the selected authority carries the exact current revision and message count.

Ruling: Failed save and revision conflict attempts preserve the complete dirty owner, retry the same mutation, and increment the oracle revision only after a successful commit.

Ruling: Screenshot uses the screenshot source lease. Search uses anchored exact-ID queries. Hypa uses its production anchored adapter and releases its read lease before redemotion.

Ruling: Branch uses the persistent UI gateway after the shifted interior delete, then verifies the exact persisted prefix and branch marker before returning to the source.

Ruling: Export materializes the authoritative persistent snapshot at the exact modeled revision and compares its selected conversation with the complete-owner oracle.

Ruling: Plugin API v2.1 compatibility uses the actual live database Proxy, persists the proxy mutation during the maximum-to-scalable transition, reinitializes the selected working set, and returns to the resident budget.

Ruling: The misleading `evictionEnabled=false` marker is removed rather than renamed because no production code reads it and automatic demotion is already active.

Ruling: The earlier full-suite branch timeout was not hidden. The responsible Task 8 occurrence-index fix landed independently, and the mandatory full suite now passes.

## Changed files

- `src/ts/storage/selectedConversationEvictionCorpus.test.ts`
- `src/ts/storage/selectedConversationEvictionNodeDom.ts`
- `src/ts/storage/activeConversationSession.ts`
- `src/ts/storage/activeConversationSession.test.ts`
- `.superpowers/sdd/2026-08-30-remaining-track-bc/task-9-report.md` (ignored coordination report)
