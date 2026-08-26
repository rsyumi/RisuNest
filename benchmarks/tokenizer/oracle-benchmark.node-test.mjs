import assert from 'node:assert/strict'
import test from 'node:test'

import { parseArguments, runOracleBenchmark } from './oracle-benchmark.mjs'

test('parseArguments keeps samples bounded and accepts an optional output', () => {
    assert.deepEqual(parseArguments([]), { samples: 20, output: null })
    assert.deepEqual(parseArguments(['--samples', '3', '--output', 'result.json']), {
        samples: 3,
        output: 'result.json',
    })
    assert.throws(() => parseArguments(['--samples', '0']), /positive integer/)
    assert.throws(() => parseArguments(['--samples', '1001']), /at most 1000/)
})

test('the oracle benchmark reports parity and both output modes without native routing', async () => {
    const result = await runOracleBenchmark({
        samples: 2,
        fixtureNames: new Set(['short-segments-1']),
    })

    assert.equal(result.schemaVersion, 1)
    assert.equal(result.scope, 'javascript-oracle-baseline')
    assert.equal(result.productionRoutingChanged, false)
    assert.equal(result.nativeCandidateMeasured, false)
    assert.deepEqual(result.parity, {
        cl100k_base: { idCases: 34, errorCases: 10 },
        o200k_base: { idCases: 34, errorCases: 4 },
    })
    assert.equal(result.measurements.length, 4)
    assert.deepEqual(
        new Set(result.measurements.map((measurement) => measurement.mode)),
        new Set(['count', 'ids']),
    )
    for (const measurement of result.measurements) {
        assert.equal(measurement.samples, 2)
        assert.ok(measurement.p50Ms >= 0)
        assert.ok(measurement.p95Ms >= measurement.p50Ms)
        assert.ok(measurement.totalIds > 0)
    }
})
