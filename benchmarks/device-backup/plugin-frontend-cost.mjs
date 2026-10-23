import { execFileSync } from 'node:child_process'
import { createHash, randomUUID } from 'node:crypto'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import path from 'node:path'
import { pathToFileURL } from 'node:url'
import ts from 'typescript'

// CPU-only diagnostic. The backend process boundary is an in-memory async KV;
// the production wrapper and codec are transpiled intact from both revisions.
// This complements the native measurement, never substitutes for its timing.
const root = path.resolve(import.meta.dirname, '../..')
const revision = process.argv[2] ?? 'HEAD'
const output = path.resolve(process.argv[3] ?? path.join(root, '.tmp/plugin-frontend-cost.json'))
const beforeDirectory = process.argv[4] ? path.resolve(process.argv[4]) : null
const directory = path.join(root, '.tmp', `plugin-frontend-cost-${randomUUID()}`)
await mkdir(directory, { recursive: true })
const file = 'src/ts/plugins/pluginSafeClass.ts'
const sources = {
  before: beforeDirectory ? await readFile(path.join(beforeDirectory, 'pluginSafeClass.ts'), 'utf8')
    : execFileSync('git', ['show', `${revision}:${file}`], { cwd: root, encoding: 'utf8' }),
  after: await readFile(path.join(root, file), 'utf8'),
}
const codec = await readFile(path.join(root, 'src/ts/plugins/localPluginStorageValue.ts'), 'utf8')
const beforeCodec = beforeDirectory ? await readFile(path.join(beforeDirectory, 'localPluginStorageValue.ts'), 'utf8') : codec
function transpile(source) {
  return ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2023, module: ts.ModuleKind.ESNext } }).outputText
}
const classes = {}
for (const [label, source] of Object.entries(sources)) {
  await writeFile(path.join(directory, `${label}-codec.mjs`), transpile(label === 'before' ? beforeCodec : codec))
  const isolated = source.replace(/^import .*from ["']\.\/pluginDeviceKeyspace["'];?\r?\n/m, 'const getPluginDeviceKeyspace = () => { throw new Error("Explicit benchmark boundary required") };\n')
    .replace("from './localPluginStorageValue'", `from './${label}-codec.mjs'`)
  const target = path.join(directory, `${label}.mjs`)
  await writeFile(target, transpile(isolated))
  classes[label] = (await import(pathToFileURL(target))).SafeLocalPluginStorage
}
const cases = [
  { name: 'small-json', value: { enabled: true, name: 'plugin', count: 42, tags: ['a', 'b'] }, iterations: 2000 },
  { name: 'array-1000', value: Array.from({ length: 1000 }, (_, i) => ({ id: i, name: `entry-${i}`, enabled: i % 2 === 0, score: i / 7 })), iterations: 50 },
  { name: 'array-50000', value: Array.from({ length: 50000 }, (_, i) => ({ id: i, name: `entry-${i}`, enabled: i % 2 === 0, score: i / 7 })), iterations: 2 },
  { name: 'flat-json-8MiB', value: { payload: 'x'.repeat(8 * 1024 * 1024) }, iterations: 1 },
]
const result = { environment: { node: process.version, v8: process.versions.v8 }, revision, beforeDirectory,
  beforeSource: beforeDirectory ?? `git:${revision}:${file}`,
  method: 'CPU-only production wrapper + codec, in-memory async backend; no IPC/SQLite/iframe; ABBA batch order; 3 warmups and 12 samples per side',
  sourceHashes: Object.fromEntries([...Object.entries(sources), ['codec', codec], ['beforeCodec', beforeCodec]].map(([key, value]) => [key, createHash('sha256').update(value).digest('hex')])), results: [] }
for (const spec of cases) {
  const fixtures = {}
  for (const label of ['before', 'after']) {
    let stored = null
    const boundary = { async getItem() { return stored }, async setItem(_space, _key, value) { stored = value } }
    fixtures[label] = new classes[label]('synthetic-frontend', boundary)
    await fixtures[label].setItem('value', spec.value)
    if (JSON.stringify(await fixtures[label].getItem('value')) !== JSON.stringify(spec.value)) throw new Error('Fixture differs')
  }
  for (const operation of ['write', 'read']) {
    const samples = { before: [], after: [] }
    // Interleave ABBA to reduce warm-up and time-order effects.
    for (let round = -3; round < 12; round++) {
      const order = round % 2 === 0 ? ['before', 'after'] : ['after', 'before']
      for (const label of order) {
        const started = performance.now()
        for (let i = 0; i < spec.iterations; i++) {
          if (operation === 'write') await fixtures[label].setItem('value', spec.value)
          else await fixtures[label].getItem('value')
        }
        if (round >= 0) samples[label].push((performance.now() - started) / spec.iterations)
      }
    }
    const median = values => [...values].sort((a, b) => a - b)[Math.floor(values.length / 2)]
    result.results.push({ name: `${spec.name}/${operation}`, iterations: spec.iterations, samplesMs: samples, beforeMs: median(samples.before), afterMs: median(samples.after) })
  }
}
await writeFile(output, JSON.stringify(result, null, 2))
console.log(JSON.stringify({ output, results: result.results.map(({ samplesMs, ...row }) => row) }, null, 2))
