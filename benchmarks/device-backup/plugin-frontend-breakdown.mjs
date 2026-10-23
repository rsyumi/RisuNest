import { execFileSync } from 'node:child_process'
import { createHash, randomUUID } from 'node:crypto'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { Session } from 'node:inspector'
import path from 'node:path'
import { pathToFileURL } from 'node:url'
import ts from 'typescript'

// JSON-only diagnostic copies. Disabling detection/ordering is valid for these
// known plain JSON, sequential fixtures only; no product sources are modified.
const root = path.resolve(import.meta.dirname, '../..')
const revision = process.argv[2] ?? 'HEAD'
const output = path.resolve(process.argv[3] ?? path.join(root, '.tmp/plugin-frontend-breakdown.json'))
const directory = path.join(root, '.tmp', `plugin-frontend-breakdown-${randomUUID()}`)
await mkdir(directory, { recursive: true })
const sourcePath = 'src/ts/plugins/pluginSafeClass.ts'
const baseline = execFileSync('git', ['show', `${revision}:${sourcePath}`], { cwd: root, encoding: 'utf8' })
const current = await readFile(path.join(root, sourcePath), 'utf8')
const codec = await readFile(path.join(root, 'src/ts/plugins/localPluginStorageValue.ts'), 'utf8')
const transpile = source => ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2023, module: ts.ModuleKind.ESNext } }).outputText
await writeFile(path.join(directory, 'codec.mjs'), transpile(codec))
const { requiresLocalPluginStorageEncoding: detect } = await import(pathToFileURL(path.join(directory, 'codec.mjs')))
function ablate(source, { detection = true, queue = true }) {
    if (!detection) {
        const statement = 'const binary = requiresLocalPluginStorageEncoding(value);'
        if (source.split(statement).length !== 2) throw new Error('Storage detection statement changed')
        source = source.replace(statement, 'const binary = false; // JSON-only diagnostic')
    }
    if (!queue) {
        const ast = ts.createSourceFile('wrapper.ts', source, ts.ScriptTarget.Latest, true)
        const declaration = ast.statements.find(node => ts.isClassDeclaration(node) && node.name?.text === 'SafeLocalPluginStorage')
        const method = declaration?.members.find(node => ts.isMethodDeclaration(node) && node.name.getText(ast) === '#queue')
        if (!method) throw new Error('Storage queue method changed')
        source = source.slice(0, method.getStart(ast)) + '#queue<T>(_key: string | null, operation: () => Promise<T>, _mutation = true): Promise<T> { return operation(); }' + source.slice(method.getEnd())
    }
    return source
}
const variants = {
    baseline,
    current,
    'without-detection': ablate(current, { detection: false }),
    'without-queue': ablate(current, { queue: false }),
    'without-both': ablate(current, { detection: false, queue: false }),
}
const classes = {}
for (const [label, source] of Object.entries(variants)) {
    const isolated = source.replace(/^import .*from ["']\.\/pluginDeviceKeyspace["'];?\r?\n/m, 'const getPluginDeviceKeyspace = () => { throw new Error("Explicit benchmark boundary required") };\n')
        .replace("from './localPluginStorageValue'", "from './codec.mjs'")
    const file = path.join(directory, `${label}.mjs`)
    await writeFile(file, transpile(isolated))
    classes[label] = (await import(pathToFileURL(file))).SafeLocalPluginStorage
}
const cases = [
    { name: 'small-json', value: { enabled: true, name: 'plugin', count: 42, tags: ['a', 'b'] }, iterations: 2000 },
    { name: 'array-1000', value: Array.from({ length: 1000 }, (_, i) => ({ id: i, name: `entry-${i}`, enabled: i % 2 === 0, score: i / 7 })), iterations: 30 },
    { name: 'array-50000', value: Array.from({ length: 50000 }, (_, i) => ({ id: i, name: `entry-${i}`, enabled: i % 2 === 0, score: i / 7 })), iterations: 2 },
    { name: 'flat-json-8MiB', value: { payload: 'x'.repeat(8 * 1024 * 1024) }, iterations: 1 },
]
const median = samples => [...samples].sort((a, b) => a - b)[Math.floor(samples.length / 2)]
const result = {
    environment: { node: process.version, v8: process.versions.v8 }, revision,
    method: 'Production wrapper + codec, async memory KV boundary; JSON-only ablations; 4 warmups + 15 samples, rotating/reversing variant order; batch-average medians; sync stages timed without per-call await',
    sourceHashes: Object.fromEntries(Object.entries({ baseline, current, codec }).map(([key, source]) => [key, createHash('sha256').update(source).digest('hex')])),
    directory, results: [],
}
let retained
for (const spec of cases) {
    if (detect(spec.value)) throw new Error('Ablation requires ordinary JSON')
    const expected = JSON.stringify(spec.value, null)
    const operations = {}
    for (const [label, Class] of Object.entries(classes)) {
        let stored = null
        const boundary = { async getItem() { return stored }, async setItem(_space, _key, value) { stored = value } }
        const storage = new Class('synthetic-breakdown', boundary)
        await storage.setItem('value', spec.value)
        if (stored !== expected || JSON.stringify(await storage.getItem('value')) !== expected) throw new Error(`${label}: JSON differs`)
        operations[label] = () => storage.setItem('value', spec.value)
        if (spec.name === 'array-50000' && label === 'current') retained = { storage, value: spec.value }
    }
    const samples = Object.fromEntries([...Object.keys(operations), 'detection-only', 'stringify-only'].map(label => [label, []]))
    const labels = Object.keys(samples)
    for (let round = -4; round < 15; round++) {
        const offset = (round + 4) % labels.length
        let order = [...labels.slice(offset), ...labels.slice(0, offset)]
        if (round % 2) order = order.reverse()
        for (const label of order) {
            const started = performance.now()
            if (label === 'detection-only') {
                for (let i = 0; i < spec.iterations; i++) if (detect(spec.value)) throw new Error('Unexpected rich value')
            } else if (label === 'stringify-only') {
                for (let i = 0; i < spec.iterations; i++) if (JSON.stringify(spec.value, null).length !== expected.length) throw new Error('JSON length differs')
            } else {
                for (let i = 0; i < spec.iterations; i++) await operations[label]()
            }
            if (round >= 0) samples[label].push((performance.now() - started) / spec.iterations)
        }
    }
    result.results.push({ name: spec.name, iterations: spec.iterations,
        timings: Object.fromEntries(Object.entries(samples).map(([label, values]) => [label, { medianMs: median(values), minMs: Math.min(...values), maxMs: Math.max(...values), samplesMs: values }])) })
}

// Profile only the unmodified current wrapper after all timed samples. Module
// loading, TypeScript transpilation, and diagnostic ablations are excluded.
const session = new Session()
session.connect()
const post = (method, params = {}) => new Promise((resolve, reject) => session.post(method, params, (error, response) => error ? reject(error) : resolve(response)))
await post('Profiler.enable')
await post('Profiler.setSamplingInterval', { interval: 1000 })
await post('Profiler.start')
for (let i = 0; i < 180; i++) await retained.storage.setItem('value', retained.value)
const { profile } = await post('Profiler.stop')
session.disconnect()
const profilePath = output.replace(/\.json$/, '') + '.cpuprofile'
await writeFile(profilePath, JSON.stringify(profile))
const nodes = new Map(profile.nodes.map(node => [node.id, node]))
const self = new Map()
for (let i = 0; i < profile.samples.length; i++) {
    const node = nodes.get(profile.samples[i]), frame = node.callFrame
    const label = `${frame.functionName || '(anonymous)'} ${frame.url ? path.basename(frame.url) : ''}:${frame.lineNumber + 1}`
    self.set(label, (self.get(label) ?? 0) + profile.timeDeltas[i])
}
result.profile = { path: profilePath, measuredSelfTimeMs: [...self.values()].reduce((sum, value) => sum + value, 0) / 1000,
    topSelf: [...self].sort((a, b) => b[1] - a[1]).slice(0, 12).map(([name, time]) => ({ name, selfMs: time / 1000 })) }
await writeFile(output, JSON.stringify(result, null, 2))
console.log(JSON.stringify({ output, results: result.results.map(row => ({ name: row.name, medianMs: Object.fromEntries(Object.entries(row.timings).map(([key, value]) => [key, value.medianMs])) })), profile: result.profile }, null, 2))
