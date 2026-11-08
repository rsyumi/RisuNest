import assert from 'node:assert/strict'
const expectedCases = {
    'official-account-benchmark': ['official-account-assets-and-ledger'],
    'asset-benchmark': ['asset-bytes-aliases-and-inlays'],
    'asset-benchmark-small': ['small-asset-bytes-aliases'],
    contract: ['delta-lease-pagination-large-commit'],
    'read-contract': ['contract-restart-without-reseed'],
    write: ['commit', 'readback', 'stale-rejected'],
    read: ['readback', 'restart-without-reseed'],
    abort: ['commit', 'readback', 'abort-released'],
    'read-abort': ['readback', 'restart-without-reseed'],
    'close-escape': ['unanswered-close-kept', 'early-repeat-kept'],
    'session-end': ['commit', 'stop-requested', 'saved'],
    'read-session-end': ['saved-before-exit'],
    'session-end-unanswered': ['commit', 'stop-requested'],
}
// The failure record keeps the error's cause chain, which `error` alone drops.
const failureText = report => report.failure ? JSON.stringify(report.failure) : report.error
export function verifyReport(report, runId, phase, status) {
    assert.ok(Object.hasOwn(expectedCases, phase), 'unknown phase')
    assert.equal(status, 0, failureText(report) ?? 'native process exit')
    assert.equal(report.runId, runId, 'run identity')
    assert.equal(report.phase, phase, 'process phase')
    assert.equal(report.success, true, failureText(report) ?? 'native assertions failed')
    assert.deepEqual(report.cases, expectedCases[phase])
}
