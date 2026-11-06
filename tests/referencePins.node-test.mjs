import assert from 'node:assert/strict'
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { keepMatchingPins, matchesSourcePin, references, sha256, verifyGeneratedContract } from '../scripts/compatibleExportContract.mjs'

test('source pins accept LF and CRLF but reject content changes; binary hashes remain exact', () => {
  const lf = Buffer.from('export const value = "synthetic";\n\n')
  const crlf = Buffer.from(lf.toString().replaceAll('\n', '\r\n'))
  for (const expected of [sha256(lf), sha256(crlf)]) {
    assert(matchesSourcePin(lf, expected))
    assert(matchesSourcePin(crlf, expected))
    assert(!matchesSourcePin(lf.toString().replace('synthetic', 'changed'), expected))
    assert(!matchesSourcePin(lf.toString().trimEnd(), expected))
  }
  assert.notEqual(sha256(lf), sha256(crlf))
})

test('regeneration keeps a committed pin of the other newline style and replaces a changed one', () => {
  const directory = mkdtempSync(join(tmpdir(), 'reference-pins-'))
  try {
    const lf = 'export const value = "synthetic";\n'
    const crlf = (text) => text.replaceAll('\n', '\r\n')
    writeFileSync(join(directory, 'same.ts'), lf)
    writeFileSync(join(directory, 'changed.ts'), lf)
    const committed = { reference: { files: {
      'same.ts': sha256(crlf(lf)),
      'changed.ts': sha256(crlf(lf.replace('synthetic', 'previous'))),
    } } }
    const generated = { target: 'pocket', reference: { files: { 'same.ts': sha256(lf), 'changed.ts': sha256(lf), 'new.ts': 'new' } } }
    assert.deepEqual(keepMatchingPins(generated, committed, directory).reference.files, {
      'same.ts': committed.reference.files['same.ts'],
      'changed.ts': sha256(lf),
      'new.ts': 'new',
    })
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
})

test('generated contract checks still reject revision, schema and dependency changes', () => {
  const expected = { target: 'risuai', reference: { revision: 'approved', files: {} }, nodes: { value: 'string' } }
  assert.doesNotThrow(() => verifyGeneratedContract(structuredClone(expected), expected))
  for (const actual of [
    { ...expected, reference: { revision: 'different', files: {} } },
    { ...expected, nodes: { value: 'number' } },
    { ...expected, reference: { ...expected.reference, files: { 'unexpected.ts': 'hash' } } },
  ]) assert.throws(() => verifyGeneratedContract(actual, expected), /contract drift/)
})

test('CI fetches the same approved source revisions without running reference applications', () => {
  const action = readFileSync(new URL('../.github/actions/test-references/action.yml', import.meta.url), 'utf8')
  for (const [target, reference] of Object.entries(references)) {
    assert(action.includes(`ref: ${reference.revision}`))
    assert(action.includes(`path: .tmp/test-references/${target}`))
  }
  assert.equal((action.match(/persist-credentials: false/g) ?? []).length, 2)
  // The only commands install the harness libraries in isolated directories, never either reference checkout.
  const lines = action.split(/\r?\n/)
  const commands = []
  lines.forEach((line, index) => {
    const run = line.match(/^(\s+(?:-\s+)?)run:(.*)$/)
    if (!run) return
    assert.equal(run[2], ' |')
    for (const next of lines.slice(index + 1)) {
      if (next.trim() && next.search(/\S/) <= run[1].length) break
      if (next.trim()) commands.push(next.trim())
    }
  })
  assert.deepEqual(commands, [
    'node scripts/compatibleExportDependencies.mjs --prepare',
    'pnpm --dir .tmp/test-reference-dependencies/risuai install --no-frozen-lockfile',
    'pnpm --dir .tmp/test-reference-dependencies/pocket install --no-frozen-lockfile',
  ])
})
