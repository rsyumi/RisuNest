import assert from 'node:assert/strict'
import { EventEmitter } from 'node:events'
import test from 'node:test'

import {
    buildBenchmarkConfig,
    buildShortSegments,
    installSignalCleanup,
    parseArguments,
    percentile,
    resolveCargoTargetDirectory,
} from './tauri-cdp.mjs'

test('parseArguments selects bounded warm samples and an optional output', () => {
    assert.deepEqual(parseArguments([]), {
        output: null,
        keepProfile: false,
        samples: 20,
        timeoutMs: 300_000,
    })
    assert.deepEqual(
        parseArguments(['--output', 'result.json', '--samples', '7', '--timeout-ms', '90000', '--keep-profile']),
        { output: 'result.json', keepProfile: true, samples: 7, timeoutMs: 90_000 },
    )
})

test('percentile uses the nearest-rank sample without mutating input', () => {
    const values = [9, 1, 5, 3]
    assert.equal(percentile(values, 0.5), 3)
    assert.equal(percentile(values, 0.95), 9)
    assert.deepEqual(values, [9, 1, 5, 3])
})

test('short segment fixtures preserve the requested batch size and order', () => {
    const segments = buildShortSegments(1_000)
    assert.equal(segments.length, 1_000)
    assert.match(segments[0], /segment 0/)
    assert.match(segments[999], /segment 999/)
})

test('benchmark config isolates the Windows profile and disables bundling', () => {
    const original = { identifier: 'RisuNest', bundle: { active: true }, app: { windows: [{}] } }
    const config = buildBenchmarkConfig(original, 9333, 'run-123')
    assert.equal(config.identifier, 'RisuNest.tokenizerbenchmark.run123')
    assert.equal(config.bundle.active, false)
    assert.equal(config.build.beforeBuildCommand, 'pnpm exec vite build --mode agent --config benchmarks/tokenizer/vite.config.ts')
    assert.match(config.build.frontendDist.replaceAll('\\', '/'), /benchmarks\/tokenizer\/dist$/)
    assert.match(config.app.windows[0].additionalBrowserArgs, /--remote-debugging-port=9333/)
    assert.equal(original.app.windows[0].additionalBrowserArgs, undefined)
})

test('release executable follows the shared absolute Cargo target directory', () => {
    assert.equal(
        resolveCargoTargetDirectory('E:\\repo', 'E:\\shared-target'),
        'E:\\shared-target',
    )
    assert.equal(resolveCargoTargetDirectory('E:\\repo', undefined), 'E:\\repo\\src-tauri\\target')
})

test('SIGINT and SIGTERM clean owned resources once before exiting', async () => {
    const processLike = new EventEmitter()
    const exits = []
    let cleanupCalls = 0
    const remove = installSignalCleanup(
        processLike,
        async () => {
            cleanupCalls++
        },
        (code) => exits.push(code),
    )

    processLike.emit('SIGTERM')
    processLike.emit('SIGINT')
    await new Promise((resolve) => setImmediate(resolve))

    assert.equal(cleanupCalls, 1)
    assert.deepEqual(exits, [143])
    assert.equal(processLike.listenerCount('SIGINT'), 0)
    assert.equal(processLike.listenerCount('SIGTERM'), 0)
    remove()
})

test('SIGINT exits with its conventional code after cleanup', async () => {
    const processLike = new EventEmitter()
    const exits = []
    installSignalCleanup(processLike, async () => {}, (code) => exits.push(code))

    processLike.emit('SIGINT')
    await new Promise((resolve) => setImmediate(resolve))

    assert.deepEqual(exits, [130])
})
