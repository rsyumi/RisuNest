import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const root = mkdtempSync(join(tmpdir(), 'risunest-portable-source-'))
const sources = join(root, 'Sources/tauri-plugin-ios-native')
const tests = join(root, 'Tests/NativeFileStateTests')
mkdirSync(sources, { recursive: true })
mkdirSync(tests, { recursive: true })
const source = join(sources, 'PortableSourceCustody.swift')
copyFileSync(new URL('../crates/tauri-plugin-ios-native/ios/Sources/PortableSourceCustody.swift', import.meta.url), source)
// Entered dispatch groups retain themselves, so dropping a Source need not trap.
// Observe the real group's pending work before the isolated test copy releases it.
const sourceClass = 'private final class Source {'
const sourceContents = readFileSync(source, 'utf8')
assert.equal(sourceContents.split(sourceClass).length, 2)
writeFileSync(source, sourceContents.replace(sourceClass, `${sourceClass}
        deinit {
            precondition(cleanupFinished.wait(timeout: .now()) == .success,
                         "Portable source dropped with unfinished cleanup")
        }`))
copyFileSync(new URL('../crates/tauri-plugin-ios-native/ios/Tests/PortableSourceCustodyTests.swift', import.meta.url), join(tests, 'PortableSourceCustodyTests.swift'))
const fileState = join(sources, 'NativeFileState.swift')
copyFileSync(new URL('../crates/tauri-plugin-ios-native/ios/Sources/NativeFileState.swift', import.meta.url), fileState)
copyFileSync(new URL('../crates/tauri-plugin-ios-native/ios/Tests/NativeFileStateTests.swift', import.meta.url), join(tests, 'NativeFileStateTests.swift'))
const appLockState = join(sources, 'AppLockState.swift')
copyFileSync(new URL('../crates/tauri-plugin-ios-native/ios/Sources/AppLockState.swift', import.meta.url), appLockState)
copyFileSync(new URL('../crates/tauri-plugin-ios-native/ios/Tests/AppLockStateTests.swift', import.meta.url), join(tests, 'AppLockStateTests.swift'))
writeFileSync(join(root, 'Package.swift'), `// swift-tools-version:5.3
import PackageDescription
let package = Package(
    name: "PortableSourceContracts",
    platforms: [.macOS("14.0")],
    targets: [
        .target(name: "tauri-plugin-ios-native"),
        .testTarget(name: "NativeFileStateTests", dependencies: ["tauri-plugin-ios-native"])
    ]
)
`)
function run(filter, fails = false, cleanupInvariant = false) {
    const result = spawnSync('swift', ['test', '--package-path', root, '--filter', filter], {
        encoding: 'utf8', timeout: 300_000, maxBuffer: 8 * 1024 * 1024,
    })
    const output = (result.stdout ?? '') + (result.stderr ?? '')
    process.stdout.write(output)
    assert.ifError(result.error)
    if (cleanupInvariant) {
        assert.notEqual(result.status, 0, 'Pending source cleanup was not detected')
        assert.match(output, /testOwnerRetiredBeforeInstallationFailsWithoutAnEnteredCleanupGroup.*started/)
        assert.match(output, /Portable source dropped with unfinished cleanup/)
        return
    }
    assert.match(output, /Executed [1-9]\d* tests?, with \d+ failures?/)
    if (fails) {
        assert.notEqual(result.status, 0, 'Mutation was not detected')
        assert.match(output, /Executed [1-9]\d* tests?, with [1-9]\d* failures?/)
    } else assert.equal(result.status, 0, 'Swift contracts failed')
}
try {
    run('PortableSourceCustodyTests')
    run('NativeFileStateTests')
    run('AppLockStateTests')
    const originalAppLockState = readFileSync(appLockState, 'utf8')
    for (const [before, after, test] of [
        ['setHidden(content, true)', 'setHidden(content, false)', 'testNativeCoverHidesContentUntilRestoringItsPreviousAccessibility'],
        ['restoreContent = { setHidden($0, previous) }', 'restoreContent = { setHidden($0, false) }', 'testNativeCoverHidesContentUntilRestoringItsPreviousAccessibility'],
        ['guard self.content !== content else { return }', '', 'testNativeCoverHidesContentUntilRestoringItsPreviousAccessibility'],
        ['deinit { restore() }', 'deinit {}', 'testReplacingAndReleasingNativeCoverRestoresTheCoveredContent'],
    ]) {
        assert.equal(originalAppLockState.split(before).length, 2)
        writeFileSync(appLockState, originalAppLockState.replace(before, after))
        try { run(`AppLockStateTests/${test}`, true) }
        finally { writeFileSync(appLockState, originalAppLockState) }
    }
    const epochGuard = 'guard active && epoch == renderedEpoch && (locked || !policy.gateRequired) else { return false }'
    assert.equal(originalAppLockState.split(epochGuard).length, 2)
    writeFileSync(appLockState, originalAppLockState.replace(epochGuard, 'guard active && (locked || !policy.gateRequired) else { return false }'))
    try { run('AppLockStateTests/testDepartureAndReturnKeepCoverUntilCurrentFrameIsReady', true) }
    finally { writeFileSync(appLockState, originalAppLockState) }
    writeFileSync(appLockState, originalAppLockState.replace(' && (locked || !policy.gateRequired)', ''))
    try { run('AppLockStateTests/testUnlockedFrameCannotReplaceARequiredGateInTheSameEpoch', true) }
    finally { writeFileSync(appLockState, originalAppLockState) }
    for (const [before, after] of [
        ['guard active, epoch == coveredEpoch, awaitingReady,', 'guard epoch == coveredEpoch, awaitingReady,'],
        ['guard active, epoch == coveredEpoch, awaitingReady,', 'guard active, awaitingReady,'],
        ['guard active, epoch == coveredEpoch, awaitingReady,', 'guard active, epoch == coveredEpoch,'],
        ['!policy.installsReturnCover, !policy.gateRequired else { return false }', '!policy.gateRequired else { return false }'],
        ['!policy.installsReturnCover, !policy.gateRequired else { return false }', '!policy.installsReturnCover else { return false }'],
    ]) {
        assert.equal(originalAppLockState.split(before).length, 2)
        writeFileSync(appLockState, originalAppLockState.replace(before, after))
        try { run('AppLockStateTests/testUnconfirmedReleaseEndsOnlyCoversWithoutALockDecision', true) }
        finally { writeFileSync(appLockState, originalAppLockState) }
    }
    const original = readFileSync(source, 'utf8')
    for (const [before, after, test, occurrence = 0, count = 1] of [
        ['source.copyCancelled = true', 'source.copyCancelled = false', 'testCopyCancelledBeforeOrDuringTheCopyRemovesPartialFiles', 0, 2],
        ['time - previous < 0.25', 'time - previous < 0.0', 'testCopyProgressThrottlesLargeByteCountsAndAlwaysSendsCompletion'],
        ['progress.report(copied: copied, total: source.bytes)', '_ = progress', 'testOnlyConfirmedUpstreamSourceCanCopyTheHeldDescriptor'],
        ['guard !source.copying, source.copiedFolder == nil else { lock.unlock(); throw PortableSourceError.busy }', '', 'testCopyFailuresReportStableCodes'],
        ['let deferred = !waitForCleanup && jobId == nil && (copying || probe != nil)', 'let deferred = false', 'testReleaseReturnsWhileCopyIsStalledAndRemovesTheLateCopy'],
        ['do { try removeCopy(folder) }', 'do { throw PortableSourceError.writeFailed }', 'testOwnerResetRemovesItsDeliveredCopyAndPreservesTheSelectedSource'],
        ['return cleaned', 'return true', 'testReleaseReportsCopyCleanupFailureAfterReleasingTheSource'],
        ['if !cleaned { cleanupFailurePending = true }', '', 'testDeferredCopyCleanupFailureReachesTheNextQueryAndNextStartupSweepsOnlyCustody'],
        ['if FileManager.default.fileExists(atPath: folder.path) { try FileManager.default.removeItem(at: folder) }', '', 'testDeferredCopyCleanupFailureReachesTheNextQueryAndNextStartupSweepsOnlyCustody'],
        ['guard swept.insert(folder).inserted else { return }', '', 'testStartupSweepDoesNotRemoveAnotherOwnerOrCustodyInstancesActiveCopy'],
        ['catch { lock.lock(); cleanupFailurePending = true; lock.unlock() }', 'catch {}', 'testStartupSweepFailureIsReportedAndRetriedByTheNextProcess'],
        ['if case .writeFailed = failure, isFullStorage(error as NSError)', 'if false', 'testFullStorageDuringCopyDoesNotPublishAndRemovesItsPartialFile'],
        // The loop, post-loop and post-synchronize checks share one line; each mutation removes one of them.
        ...[
            'testCopyCancelledBeforeOrDuringTheCopyRemovesPartialFiles',
            'testCopyCancelledAfterItsLastByteIsNotSynchronized',
            'testCopyCancelledWhileSynchronizingRemovesTheFinishedFile',
        ].map((test, occurrence) => ['try checkCopyCancellation(source)', '', test, occurrence, 3]),
    ]) {
        const parts = original.split(before)
        assert.equal(parts.length, count + 1, 'Mutation must match its expected occurrences')
        writeFileSync(source, parts.slice(0, occurrence + 1).join(before) + after + parts.slice(occurrence + 1).join(before))
        try { run(`PortableSourceCustodyTests/${test}`, true) }
        finally { writeFileSync(source, original) }
    }
    assert.equal(original.split('source.cleanupFinished.enter()').length, 2)
    assert.equal(original.split('self.owner = owner').length, 2)
    writeFileSync(source, original.replace('source.cleanupFinished.enter()', '')
        .replace('self.owner = owner', 'self.owner = owner\n            cleanupFinished.enter()'))
    try { run('PortableSourceCustodyTests/testOwnerRetiredBeforeInstallationFailsWithoutAnEnteredCleanupGroup', true, true) }
    finally { writeFileSync(source, original) }
    const originalFileState = readFileSync(fileState, 'utf8')
    const custodyParent = ' || folder.deletingLastPathComponent() == root.appendingPathComponent("custody", isDirectory: true)'
    assert.equal(originalFileState.split(custodyParent).length, 2)
    writeFileSync(fileState, originalFileState.replace(custodyParent, ''))
    try { run('NativeFileStateTests/testDiscardAcceptsLegacyAndCustodyCopiesButRefusesOtherNestedFolders', true) }
    finally { writeFileSync(fileState, originalFileState) }
    console.log('Portable source contracts and mutation checks passed')
} finally {
    rmSync(root, { recursive: true, force: true })
}
