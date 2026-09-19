import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { test } from 'node:test'
import { matchesSourcePin, references, sha256, verifyGeneratedContract } from '../scripts/compatibleExportContract.mjs'

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
  assert.doesNotMatch(action, /^\s+run:/m)
})
