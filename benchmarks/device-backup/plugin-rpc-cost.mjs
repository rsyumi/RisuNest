import { readFile, readdir, mkdir, writeFile } from 'node:fs/promises'
import { createHash } from 'node:crypto'
import os from 'node:os'
import path from 'node:path'
import { chromium, webkit } from '@playwright/test'
import { build, preview } from 'vite'

// The browser fixture uses real SandboxHost/SafeLocalPluginStorage/keyspace.
// Only its persistence process boundary is an asynchronous in-memory backend.
// Build and measure the separate browser verification entry in agent mode.
const root = path.resolve(import.meta.dirname, '../..')
const output = path.resolve(process.argv[2] ?? path.join(root, '.tmp/plugin-rpc-cost.json'))
const sourceFiles = ['src/ts/plugins/apiV3/factory.ts', 'src/ts/plugins/pluginSafeClass.ts', 'src/ts/plugins/localPluginStorageValue.ts', 'src/ts/plugins/pluginDeviceKeyspace.ts', 'src/ts/plugins/pluginReadBaselines.ts', 'tests/browser/main.ts', 'benchmarks/device-backup/plugin-rpc-cost.mjs', 'pnpm-lock.yaml']
const sha = data => createHash('sha256').update(data).digest('hex')
const sourceHashes = Object.fromEntries(await Promise.all(sourceFiles.map(async file => [file, sha(await readFile(path.join(root, file)))])))
await build({ configFile: path.join(root, 'tests/browser/vite.config.ts'), mode: 'agent' })
const dist = path.join(root, '.tmp/test-results/browser/dist')
const buildHashes = {}
async function hashBuild(directory) {
    for (const entry of await readdir(directory, { withFileTypes: true })) {
        const file = path.join(directory, entry.name)
        if (entry.isDirectory()) await hashBuild(file)
        else buildHashes[path.relative(dist, file).replaceAll('\\', '/')] = sha(await readFile(file))
    }
}
await hashBuild(dist)
const server = await preview({ configFile: path.join(root, 'tests/browser/vite.config.ts'), mode: 'agent', preview: { host: '127.0.0.1', port: 0, strictPort: false } })
const address = server.httpServer.address()
if (!address || typeof address === 'string') throw new Error('Preview address unavailable')
const origin = `http://127.0.0.1:${address.port}`
const guestScript = `
    const before = await risuai.getOrdinaryLocalPluginStorage();
    const after = await risuai.getLocalPluginStorage();
    const stores = { before, after };
    const specs = [
        { name: 'small-json', value: { enabled: true, name: 'plugin', count: 42, tags: ['a', 'b'] }, iterations: 20 },
        { name: 'array-256', value: Array.from({ length: 256 }, (_, id) => ({ id, name: 'entry-' + id, enabled: id % 2 === 0, score: id / 7 })), iterations: 5 },
        { name: 'array-1000', value: Array.from({ length: 1000 }, (_, id) => ({ id, name: 'entry-' + id, enabled: id % 2 === 0, score: id / 7 })), iterations: 3 },
        { name: 'array-1024', value: Array.from({ length: 1024 }, (_, id) => ({ id, name: 'entry-' + id, enabled: id % 2 === 0, score: id / 7 })), iterations: 3 },
        { name: 'array-50000', value: Array.from({ length: 50000 }, (_, id) => ({ id, name: 'entry-' + id, enabled: id % 2 === 0, score: id / 7 })), iterations: 1 },
        { name: 'flat-json-8MiB', value: { payload: 'x'.repeat(8 * 1024 * 1024) }, iterations: 1 },
    ];
    const results = [];
    for (const spec of specs) {
        const expected = JSON.stringify(spec.value, null);
        for (const store of [before, after]) {
            await store.setItem('value', spec.value);
            if (JSON.stringify(await store.getItem('value'), null) !== expected) throw new Error('JSON correctness: ' + spec.name);
        }
        const samples = { before: [], after: [] };
        for (let round = -3; round < 12; round++) {
            for (const label of round % 2 ? ['after', 'before'] : ['before', 'after']) {
                const started = performance.now();
                for (let i = 0; i < spec.iterations; i++) await stores[label].setItem('value', spec.value);
                if (round >= 0) samples[label].push((performance.now() - started) / spec.iterations);
            }
        }
        results.push({ name: spec.name, iterations: spec.iterations, samplesMs: samples });
    }
    for (const kind of ['ArrayBuffer', 'Blob']) {
        const samples = { before: [], after: [] };
        for (let round = -2; round < 7; round++) {
            for (const label of round % 2 ? ['after', 'before'] : ['before', 'after']) {
                const bytes = new Uint8Array(1024 * 1024).fill(173);
                const value = Array(1024).fill(null);
                value[1023] = { nested: kind === 'ArrayBuffer' ? bytes.buffer : new Blob([bytes], { type: 'application/octet-stream' }) };
                const started = performance.now();
                await stores[label].setItem('value', value);
                const elapsed = performance.now() - started;
                const restored = (await stores[label].getItem('value'))[1023].nested;
                if (kind === 'ArrayBuffer' ? !(restored instanceof ArrayBuffer) : !(restored instanceof Blob)) throw new Error('Binary type: ' + kind);
                const decoded = new Uint8Array(kind === 'ArrayBuffer' ? restored : await restored.arrayBuffer());
                if (decoded.length !== 1024 * 1024 || decoded.some(byte => byte !== 173)) throw new Error('Binary bytes: ' + kind);
                if (round >= 0) samples[label].push(elapsed);
            }
        }
        results.push({ name: 'nested-' + kind + '-1MiB', iterations: 1, samplesMs: samples });
    }
    await risuai.result({ userAgent: navigator.userAgent, results });
`
const result = { measuredAt: new Date().toISOString(), environment: { node: process.version, os: os.release(), cpu: os.cpus()[0].model },
    method: 'Built real opaque plugin iframe + SandboxHost + actual storage/keyspace; async memory persistence. Before uses forwarding remote reference without local-JSON capability; after uses actual SafeLocalPluginStorage capability. Same receiver storage/queues/cache. Includes guest preparation, postMessage, host JSON validation and responses; excludes SQLite/native IPC. JSON: 3 warmups + 12 alternating measured batches. Binary: 2 warmups + 7 samples, fixture creation and full correctness checks outside timing.',
    sourceHashes, buildHashes, browsers: [] }
try {
    for (const [name, type] of [['chromium', chromium], ['webkit', webkit]]) {
        const browser = await type.launch({ headless: true })
        try {
            const page = await browser.newPage()
            const errors = []
            page.on('pageerror', error => errors.push(error.stack ?? String(error)))
            await page.route('**/*', async route => {
                const url = route.request().url()
                if (!url.startsWith(origin + '/')) return route.abort()
                // Match the browser fixture: serve owned HTML without machine HTTP injectors.
                if (new URL(url).pathname === '/') return route.fulfill({ contentType: 'text/html', body: await readFile(path.join(dist, 'index.html'), 'utf8') })
                return route.continue()
            })
            await page.goto(origin)
            await page.evaluate(() => window.boundary.captureBridgeMessages(false))
            await page.evaluate(script => window.boundary.loadLocal('try {' + script + '} catch (error) { await risuai.result({ error: String(error) }); }'), guestScript)
            await page.waitForFunction(() => window.boundary.localResult !== null, null, { timeout: 180_000 })
            const measurement = await page.evaluate(() => window.boundary.localResult)
            if (measurement.error) throw new Error(measurement.error)
            if (errors.length) throw new Error(errors.join('\n'))
            const median = values => [...values].sort((a, b) => a - b)[Math.floor(values.length / 2)]
            result.browsers.push({ name, version: browser.version(), ...measurement,
                mediansMs: measurement.results.map(row => ({ name: row.name, before: median(row.samplesMs.before), after: median(row.samplesMs.after) })) })
            console.log(JSON.stringify({ browser: name, mediansMs: result.browsers.at(-1).mediansMs }, null, 2))
        } finally { await browser.close() }
    }
    for (const file of sourceFiles) if (sha(await readFile(path.join(root, file))) !== sourceHashes[file]) throw new Error('Source changed during measurement: ' + file)
    await mkdir(path.dirname(output), { recursive: true })
    await writeFile(output, JSON.stringify(result, null, 2))
    console.log('Output: ' + output)
} finally { await new Promise(resolve => server.httpServer.close(resolve)) }
