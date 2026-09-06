import assert from 'node:assert/strict'
import test from 'node:test'

import {
    buildBenchmarkConfig,
    resolveCargoTargetDirectory,
    summarizeGates,
} from './runner.mjs'

test('benchmark config isolates the profile and standalone frontend', () => {
    const original = {
        identifier: 'RisuNest',
        build: { frontendDist: '../dist' },
        bundle: { active: true },
        app: { windows: [{}] },
    }
    const config = buildBenchmarkConfig(original, 9333, 'run-123')

    assert.equal(config.identifier, 'RisuNest.regexnativepilot.run123')
    assert.equal(config.bundle.active, false)
    assert.equal(config.build.frontendDist, '../node_modules/.cache/regex-native-pilot')
    assert.match(config.app.windows[0].additionalBrowserArgs, /--remote-debugging-port=9333/)
    assert.equal(original.app.windows[0].additionalBrowserArgs, undefined)
})

test('gate summary requires every 100 to 500 rule production cell to pass', () => {
    const cells = [
        { rules: 20, inputBytes: 32 * 1024, gate: { passed: false } },
        { rules: 100, inputBytes: 32 * 1024, gate: { passed: true } },
        { rules: 500, inputBytes: 32 * 1024, gate: { passed: true } },
    ]

    assert.deepEqual(summarizeGates(cells), {
        passingCells: [
            { rules: 100, inputBytes: 32 * 1024 },
            { rules: 500, inputBytes: 32 * 1024 },
        ],
        productionShapedPassed: true,
        productionShapedCells: 2,
        productionShapedPassingCells: 2,
    })
    cells[2].gate.passed = false
    assert.equal(summarizeGates(cells).productionShapedPassed, false)
})

test('release executable follows the configured shared Cargo target', () => {
    assert.equal(
        resolveCargoTargetDirectory('E:\\repo', 'E:\\shared-target'),
        'E:\\shared-target',
    )
})
