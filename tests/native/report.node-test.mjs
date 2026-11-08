import { test } from 'node:test'
import assert from 'node:assert/strict'
import { verifyReport } from './report.mjs'
test('native reports reject stale, incomplete, wrong-phase and failed process evidence', () => {
    const report = { runId: 'synthetic', phase: 'read', success: true, cases: ['readback', 'restart-without-reseed'] }
    verifyReport(report, 'synthetic', 'read', 0)
    for (const changed of [{ runId: 'old' }, { phase: 'write' }, { success: false }, { cases: ['readback'] }]) {
        assert.throws(() => verifyReport({ ...report, ...changed }, 'synthetic', 'read', 0))
    }
    assert.throws(() => verifyReport(report, 'synthetic', 'read', 1))
    // A failed run names the cause chain its failure record holds.
    const failure = { message: 'Lua Worker could not start', causes: [{ name: 'SecurityError', message: 'Worker blocked' }] }
    for (const status of [0, 1]) {
        assert.throws(() => verifyReport({ ...report, success: false, error: 'Error: Lua Worker could not start', failure }, 'synthetic', 'read', status),
            /SecurityError.*Worker blocked/)
    }
})
