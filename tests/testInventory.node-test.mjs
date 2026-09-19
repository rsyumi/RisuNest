import assert from 'node:assert/strict'
import { test } from 'node:test'
import { mkdtemp, mkdir, rm, symlink, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { assertInventory, discoverTestEntries, ownerOf, selectNodeTests } from './suiteOwnership.mjs'
import { parseVitestInventory } from './testInventory.mjs'

const ordinary = 'src/ts/storage/newRegression.test.ts'
const extended = 'src/ts/storage/selectedConversationEvictionCorpus.test.ts'

test('classifies ordinary, extended, harness, Node and independent entries', () => {
  for (const [path, expected] of [
    [ordinary, 'app'], [extended, 'app-extended'],
    ['scripts/example.spec.mjs', 'app'],
    ['src-tauri/src/ios_ipc.test.ts', 'app'],
    ['benchmarks/tokenizer/nativeTokenizerBenchmark.test.ts', 'harness'],
    ['src/ts/process/luaWorkerPilotClient.test.ts', 'harness'],
    ['tests/release/contract.node-test.mjs', 'node'],
    ['benchmarks/sync-server/transfer-comparison.test.mjs', 'node'],
    ['crates/sync-wire/tests/golden.test.mjs', 'node'],
    ['server/manager/gui/tests/App.test.ts', 'manager'],
    ['server/endpoint-registry/tests/registry.worker.ts', 'registry'],
    ['tests/browser/settings.browser.spec.ts', 'browser'],
    ['src/lib/Others/BookmarkDisplayState.test.svelte.ts', null],
    ['src/ts/storage/nativeCommitEncoder.worker.ts', null],
  ]) assert.equal(ownerOf(path), expected, path)
  assert.equal(ownerOf(ordinary.replaceAll('/', '\\')), 'app')
  assert.throws(() => ownerOf('new-package/missed.test.ts'), /Unowned/)
})

test('detects exclusions, duplicate ownership and missing runner owners', () => {
  assert.doesNotThrow(() => assertInventory([ordinary, extended], { app: [ordinary], 'app-extended': [extended] }))
  assert.throws(() => assertInventory([ordinary, extended], { app: [ordinary], 'app-extended': [] }), /omitted/)
  assert.throws(() => assertInventory([ordinary], { app: [ordinary, ordinary] }), /Duplicate/)
  assert.throws(() => assertInventory([ordinary], { app: [ordinary], harness: [ordinary] }), /Duplicate/)
  assert.throws(() => assertInventory([extended], { app: [] }), /Missing runner/)
  assert.throws(() => assertInventory([extended], { app: [extended] }), /Unexpected/)
})

test('accepts the installed CLI JSON shape and rejects unexpected output', () => {
  const root = join(tmpdir(), 'inventory')
  assert.deepEqual(parseVitestInventory(JSON.stringify([{ file: join(root, ordinary), projectName: 'app' }]), root), {
    app: [ordinary], 'app-extended': [], harness: [],
  })
  for (const invalid of ['warning\n[]', '{}', '[]', '[{}]', JSON.stringify([{ file: join(root, ordinary), projectName: 'unknown' }])]) {
    assert.throws(() => parseVitestInventory(invalid, root))
  }
  assert.throws(() => parseVitestInventory(JSON.stringify([{ file: join(root, '..', 'outside.test.ts'), projectName: 'app' }]), root), /outside/)
})

test('Node selection is bounded, deterministic and never selects measurement scripts', () => {
  assert.deepEqual(selectNodeTests([ordinary, 'tests/externalStorageWasm.mjs', 'tests/b.node-test.mjs', 'tests/a.node-test.mjs']), [
    'tests/a.node-test.mjs', 'tests/b.node-test.mjs',
  ])
  assert.throws(() => selectNodeTests([ordinary]), /No Node/)
  assert.throws(() => selectNodeTests(['tests/a.node-test.mjs', 'tests/a.node-test.mjs']), /Duplicate/)
})

test('filesystem discovery includes untracked unknown roots, but not helpers, builds or symlinks', async () => {
  const root = await mkdtemp(join(tmpdir(), 'risunest-test-inventory-'))
  try {
    for (const directory of ['src', 'new-package', 'target', 'node_modules']) await mkdir(join(root, directory))
    for (const file of ['src/new.test.ts', 'src/helper.test.svelte.ts', 'new-package/unknown.test.ts', 'target/generated.test.ts', 'node_modules/vendor.test.ts']) {
      await writeFile(join(root, file), '')
    }
    await symlink(join(root, 'src'), join(root, 'linked'), 'junction')
    assert.deepEqual(await discoverTestEntries(root), ['new-package/unknown.test.ts', 'src/new.test.ts'])
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})
