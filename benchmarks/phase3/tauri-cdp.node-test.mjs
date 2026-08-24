import assert from 'node:assert/strict'
import test from 'node:test'

import {
    aggregateProcessMemory,
    benchmarkAppDataDirectory,
    buildBenchmarkConfig,
    parseArguments,
    summarizeG6,
    tauriBuildInvocation,
} from './tauri-cdp.mjs'

test('buildBenchmarkConfig isolates the Tauri identifier and WebView profile', () => {
    const original = {
        identifier: 'co.aiclient.risu',
        bundle: { active: true },
        app: {
            windows: [{ label: 'main', title: 'RisuAI' }],
        },
    }

    const config = buildBenchmarkConfig(original, 9333, 'run-123')

    assert.equal(config.identifier, 'co.aiclient.risu.phase3benchmark.run123')
    assert.equal(config.bundle.active, false)
    assert.equal(config.app.windows[0].dataDirectory, 'phase3-benchmark-run-123')
    assert.match(config.app.windows[0].additionalBrowserArgs, /--remote-debugging-port=9333/)
    assert.match(config.app.windows[0].additionalBrowserArgs, /--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection/)
    assert.equal(original.identifier, 'co.aiclient.risu')
    assert.equal(original.app.windows[0].dataDirectory, undefined)
})

test('parseArguments defaults to the documented release measurement', () => {
    assert.deepEqual(parseArguments([]), {
        output: null,
        keepProfile: false,
        timeoutMs: 120_000,
        fixtureBytes: 64 * 1024 * 1024,
    })

    assert.deepEqual(
        parseArguments([
            '--output',
            'result.json',
            '--keep-profile',
            '--timeout-ms',
            '90000',
            '--padding-mib',
            '96',
        ]),
        {
            output: 'result.json',
            keepProfile: true,
            timeoutMs: 90_000,
            fixtureBytes: 96 * 1024 * 1024,
        },
    )
})

test('summarizeG6 counts only long tasks overlapping the snapshot interval', () => {
    const summary = summarizeG6(
        [
            { startTime: 80, duration: 30 },
            { startTime: 110, duration: 55 },
            { startTime: 180, duration: 60 },
            { startTime: 220, duration: 75 },
        ],
        100,
        200,
    )

    assert.deepEqual(summary, {
        passed: false,
        longTaskCount: 2,
        longTaskTotalMs: 115,
        longestLongTaskMs: 60,
    })
})

test('summarizeG6 passes when snapshot execution has no overlapping long task', () => {
    assert.deepEqual(summarizeG6([], 100, 200), {
        passed: true,
        longTaskCount: 0,
        longTaskTotalMs: 0,
        longestLongTaskMs: 0,
    })
})

test('aggregateProcessMemory sums unique app and WebView process IDs', () => {
    const summary = aggregateProcessMemory(
        [10, 11, 11],
        [
            { pid: 10, workingSetBytes: 100, privateBytes: 70 },
            { pid: 11, workingSetBytes: 200, privateBytes: 120 },
            { pid: 12, workingSetBytes: 400, privateBytes: 300 },
        ],
    )

    assert.deepEqual(summary, {
        processCount: 2,
        workingSetBytes: 300,
        privateBytes: 190,
        processes: [
            { pid: 10, workingSetBytes: 100, privateBytes: 70 },
            { pid: 11, workingSetBytes: 200, privateBytes: 120 },
        ],
    })
})

test('tauriBuildInvocation bypasses Windows command shims', () => {
    const invocation = tauriBuildInvocation('E:\\repo', 'C:\\temp\\benchmark.json')

    assert.equal(invocation.command, process.execPath)
    assert.deepEqual(invocation.args, [
        'E:\\repo\\node_modules\\@tauri-apps\\cli\\tauri.js',
        'build',
        '--no-bundle',
        '--ci',
        '--config',
        'C:\\temp\\benchmark.json',
    ])
})

test('benchmarkAppDataDirectory accepts only the exact isolated identifier layout', () => {
    const identifier = 'co.aiclient.risu.phase3benchmark.run123'
    const snapshot = `C:\\Users\\test\\AppData\\Roaming\\${identifier}\\persistent\\snapshots\\one.db`

    assert.equal(
        benchmarkAppDataDirectory(snapshot, identifier),
        `C:\\Users\\test\\AppData\\Roaming\\${identifier}`,
    )
    assert.throws(
        () => benchmarkAppDataDirectory(
            'C:\\Users\\test\\AppData\\Roaming\\co.aiclient.risu\\persistent\\snapshots\\one.db',
            identifier,
        ),
        /isolated benchmark identifier/,
    )
})
