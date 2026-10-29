# Compatible export acceptance harness

This harness is test tooling only. Product code must never import it. Run it only
against synthetic fixtures, never installed app data, real backups, or copies of
user data. It does not start either reference app, mount real app storage, or
allow network access.

Prepare the libraries after checking out the pinned reference revisions. This
installs only the libraries used by the harness in isolated directories; it does
not install or start either reference application. The preparation command copies
each reference lockfile so the selected libraries retain its resolved versions.

```powershell
node scripts/compatibleExportDependencies.mjs --prepare
pnpm --dir .tmp/test-reference-dependencies/risuai install --no-frozen-lockfile
pnpm --dir .tmp/test-reference-dependencies/pocket install --no-frozen-lockfile
node scripts/compatibleExportContract.mjs --check
node scripts/compatibleExportReferenceHarness.mjs --check
node --test tests/compatibleExport.node-test.mjs
node scripts/compatibleExportReferenceHarness.mjs --archive .tmp/compatible-fixtures/risuai.bin --target risuai
node scripts/compatibleExportReferenceHarness.mjs --archive .tmp/compatible-fixtures/pocket.bin --target pocket
```

The reference roots default to `.tmp/test-references/risuai` and
`.tmp/test-references/pocket`, populated by `.github/actions/test-references`.
`RISUAI_REFERENCE` and `POCKETRISU_REFERENCE` may select another checkout
at the exact pinned revision, and `RISUAI_REFERENCE_DEPENDENCIES` and
`POCKETRISU_REFERENCE_DEPENDENCIES` another library directory; `--prepare`
writes to the selected directories, and `--target risuai` or `--target pocket`
limits it to one target. The contract generator checks the revision and
hashes the actual source bytes. Its `referenceCorrections` replace the few
reference declarations that the reference's own code contradicts, and it stops
once a reference declares a corrected field itself, so the correction can be
removed. Running either generator without `--check`
updates its generated JSON; do this only as part of an explicit reference update
with acceptance review. Source checks accept LF and CRLF checkouts; other source
changes still fail.

## Reference update

The approved revisions are the `ref` values in
`.github/actions/test-references/action.yml` and the `revision` values in
`scripts/compatibleExportContract.mjs`; `tests/referencePins.node-test.mjs`
requires them to match. To move one target, check out its new revision without
installing or starting it, change both values, and regenerate that target only:

```powershell
node scripts/compatibleExportDependencies.mjs --prepare --target risuai --reference-update
pnpm --dir .tmp/test-reference-dependencies/risuai install --no-frozen-lockfile
node scripts/compatibleExportContract.mjs --target risuai
node scripts/compatibleExportReferenceHarness.mjs --target risuai
node scripts/compatibleExportContract.mjs --check
node scripts/compatibleExportReferenceHarness.mjs --check
pnpm test:node
```

`--reference-update` checks the checkout's revision instead of the manifest and
lockfile hashes in `runtime.json`, which describe the previous revision until
the harness regenerates them. With `--target`, both generators leave the other
target's generated data unchanged. The native projector reads `risuai.json` and
`pocket.json`, so review a regenerated contract as a change to compatible export
output: a field the reference adds starts being exported, and a field it
removes is reported as a loss.

## Generated recursive contract

`risuai.json` and `pocket.json` contain a root node, named interface/type roots,
and a graph of objects, arrays, tuples, unions, literals, scalars, `any`, and
`never`. Object fields carry optionality and a child node. Unknown object keys
are unsupported unless `additional` identifies an explicitly declared index
signature. `Record` and mapped types are resolved by the TypeScript checker.
Explicit `any` and `unknown` preserve arbitrary JSON at that location, including
plugin storage and extension dictionaries. They do not make parent application
objects permissive. Unresolved error types fail generation. Undefined values are
not JSON values and become `never` or are omitted from unions.

Present values are checked recursively before native archives enter reference
normalization. The acceptance oracle does not require every non-optional field
to be present because the reference database declares many fields that its
`setDatabase` supplies. Actual reference bootstrap execution separately checks
those omission/default paths for the synthetic cases in the test matrix.

## Actual reference execution

The harness extracts unmodified declarations and their source dependencies with
the TypeScript AST, then executes them in a VM. It reads the real reference
decoder and save function; it contains no copied replacement parser. Runtime
source hashes, including Pocket's bundled registry JSON, are pinned separately
in `runtime.json`. Unexpected external dependencies fail closed.

Both targets execute `decodeRisuSave`, the full `setDatabase`, the full
`checkNewFormat` bootstrap normalizer, a re-save, and a second decode. Pocket
re-saves with `encodeRisuSaveLegacy`. RisuAI's `decodeRisuSave` returns the
database and its SaveKV data separately; the harness loads the KV into the
reference's live `saveKV` store, as its bootstrap does, and re-saves with
`encodeRisuSave` and compression, the self-contained block writer its local
backup uses, which reads KV from that store.

RisuAI's block save starts with `RISUSAVE\0`, followed by blocks that each carry
a type, a compression flag, a name, and JSON content. The pinned revision
writes config (0), root (1), character (2), bot preset (4), module (5),
plugin (9), loadout (10) and plugin storage (11) blocks, and one SaveKV block
(12) per namespace, named `kv:<namespace>` and holding
`{"version":1,"entries":[{"k":...,"v":...}]}`. Its decoder also reads remote
(6), character-without-chat (7) and root component (8) blocks. The older
`\0RISUSAVE\0` saves with format byte 7, 8 or 9 (MessagePack: raw, compressed,
stream-compressed) still decode, with empty KV. The writer stores an absent
database part as an empty block, which its decoder reports and skips. The
harness allows that report only for parts it left absent in a save it wrote
itself; every other decode error still fails.

The MessagePack no-eval module runs without Node's Buffer fast path to match the
web clients. UI language updates and draft-cache sweeping are isolated, and
storage uses isolated memory. Libraries resolve from the isolated directories
above. `RISUAI_REFERENCE_DEPENDENCIES` and `POCKETRISU_REFERENCE_DEPENDENCIES`
may select another directory containing the required `node_modules`. Missing
packages, versions that differ from the reference lockfile, and parent-directory
fallbacks fail before reference execution. The reference package manifest and
lockfile are pinned with runtime sources.
Network, timers, application dialogs requiring a
decision, and unmodeled side effects throw.

RisuAI runs its complete `LoadLocalBackup` with a synthetic file input and split
stream chunks, including its plugin storage index lookup and SaveKV activation,
then its real `readImage`, `loadAsset`, and cold storage reader. Its
`pluginStorage` and `saveKV` singletons run unmodified on the isolated memory
storage; cold storage removal and Tauri file calls throw.
Pocket runs its complete `importBackupFromSource` transaction and staging
procedure, actual `decodeDatabaseWithPersistentChatIds`, and server
`buildFullExportDbValue` re-export. Its unmodified `db.cjs`, `chunkStore.cjs`,
`plugin-storage-store.cjs`, and `utils.cjs` run with the reference's real
`better-sqlite3` on `:memory:`. Filesystem calls operate on a virtual directory
tree; no reference or installed save directory is read or written. This needs
the reference dependencies installed, including its compatible native SQLite
binary. No HTTP server or full application module is started.

The actual server cold JSON canonicalization, cold character/chat restoration,
plugin storage split/reassembly, and latest inlay sidecar/media payload loaders
are covered. The actual pending-save flush function runs with no pending client
writes; entering a live-client persistence branch throws.

The Pocket edit/re-save test subsequently executes the current Nest
`normalizePocketFeatures`, `sanitizeToggleValues`, `snapshotResponse`, and
`safeStructuredClone` functions.
It verifies the edited selected body becomes the authoritative candidate on
reimport.

## Coverage limits

The tests cover wire scalars and UTF-8, recursive unknown fields and enum values,
explicit dictionaries, multiple characters/chats, selected swipes, defaults,
group/order cleanup, synthetic asset lookup, nested cold payload readers, inlay
sidecars, MIME dispatch, signature JSON bytes, metadata transport, and RisuAI
SaveKV namespaces with primitive values and entry order through block save
import and re-save. The CLI
also accepts the native writer's synthetic output for the same reference
decode/bootstrap/re-save path and prints only counts, lengths, and hashes.
For native archives it also checks declared root, preset, character, module,
persona, folder and owner-array asset references remain unchanged after re-save.
Folders with asset references are matched by ID, so removing an unrelated empty
folder does not look like an image change. Missing referenced folders, changed
images and duplicate folder IDs fail.
Both actual target `loadAsset` and `readImage` functions must return byte hashes
matching the imported storage entry at each exact mapped path. This walk never
searches prompts, scripts, plugin data or arbitrary strings.

Transactional cases include more than 5,000 entries, duplicate/truncated/missing
database entries, undecodable database shapes, encrypted markers, size limits,
and media staging-swap failure. The small invalid-stream cases verify actual
SQLite rollback preserves the prior active DB, assets, and plugin rows. A swap
failure verifies restoration of the prior inlay directory. The reference commits
SQLite before swapping inlay directories, so that later filesystem failure does
not undo its DB commit. Likewise its 5,000-entry intermediate commits mean an
arbitrarily late stream error cannot roll back earlier asset batches. These are
reference importer behaviors, not stronger atomicity guarantees from this harness.

Audio/video fixtures test loader dispatch and byte preservation, not playable
codecs. No graphical app, browser media playback, live plugins/providers,
RisuRealm content, or real user library is exercised. Arbitrary `.png`-named
payload acceptance by a byte loader does not establish media playback support.
Native projection, recovery, asset collision handling, omission reports,
entry-size limits, and failure cleanup require the native export tests as well.

## Report count interpretation

Compatibility report counts and byte lengths are decimal strings. The bridge's
`affectedConversations` is a decimal string when the count is known. `"0"` means
proven none; `null` explicitly means the effect cannot be determined, such as an
opaque plugin dependency. Unknown counts must not be displayed as zero.
