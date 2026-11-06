import Foundation
import XCTest
@testable import tauri_plugin_ios_native

final class AppLockStateTests: XCTestCase {
    private final class AccessibilityContent {
        var tracksExposure = false
        private(set) var exposed = false
        var hidden = false { didSet { if tracksExposure && !hidden { exposed = true } } }
    }

    func testNativeCoverHidesContentUntilRestoringItsPreviousAccessibility() {
        for previous in [false, true] {
            let content = AccessibilityContent()
            content.hidden = previous
            let mask = AppLockAccessibilityMask<AccessibilityContent>()
            mask.hide(content, isHidden: { $0.hidden }, setHidden: { $0.hidden = $1 })
            XCTAssertTrue(content.hidden)
            content.tracksExposure = true
            mask.hide(content, isHidden: { $0.hidden }, setHidden: { $0.hidden = $1 })
            XCTAssertFalse(content.exposed)
            content.tracksExposure = false
            mask.restore()
            XCTAssertEqual(content.hidden, previous)
            content.hidden = !previous
            mask.restore()
            XCTAssertEqual(content.hidden, !previous)
        }
    }

    func testReplacingAndReleasingNativeCoverRestoresTheCoveredContent() {
        let oldContent = AccessibilityContent()
        let newContent = AccessibilityContent()
        var mask: AppLockAccessibilityMask<AccessibilityContent>? = AppLockAccessibilityMask()
        mask?.hide(oldContent, isHidden: { $0.hidden }, setHidden: { $0.hidden = $1 })
        XCTAssertTrue(oldContent.hidden)
        mask?.hide(newContent, isHidden: { $0.hidden }, setHidden: { $0.hidden = $1 })
        XCTAssertFalse(oldContent.hidden)
        XCTAssertTrue(newContent.hidden)
        mask = nil
        XCTAssertFalse(newContent.hidden)
    }

    func testUnlockedFrameCannotReplaceARequiredGateInTheSameEpoch() {
        var state = AppLockState()
        state.policy.gateRequired = true
        XCTAssertFalse(state.ready(epoch: state.epoch, locked: false))
        XCTAssertTrue(state.coverRequired)
        XCTAssertTrue(state.ready(epoch: state.epoch, locked: true))
        XCTAssertFalse(state.coverRequired)
    }

    func testDepartureAndReturnKeepCoverUntilCurrentFrameIsReady() {
        var state = AppLockState()
        state.policy.enabled = true
        state.policy.timing = "immediate"
        XCTAssertTrue(state.ready(epoch: 0, locked: false))
        XCTAssertFalse(state.coverRequired)
        XCTAssertTrue(state.depart(at: 100))
        XCTAssertTrue(state.coverRequired)
        XCTAssertFalse(state.ready(epoch: 1, locked: false))
        XCTAssertTrue(state.becomeActive())
        XCTAssertFalse(state.ready(epoch: 0, locked: false))
        XCTAssertFalse(state.ready(epoch: 1, locked: false))
        XCTAssertTrue(state.coverRequired)
        XCTAssertTrue(state.ready(epoch: 2, locked: true))
        XCTAssertFalse(state.coverRequired)
        XCTAssertTrue(state.policy.gateRequired)
    }

    func testPreviewWorksWithoutAppLockAndWaitsForTheReturningFrame() {
        var state = AppLockState()
        state.policy.hideRecentsPreview = true
        XCTAssertTrue(state.ready(epoch: 0, locked: false))
        XCTAssertTrue(state.depart(at: 50))
        XCTAssertTrue(state.coverRequired)
        XCTAssertTrue(state.becomeActive())
        XCTAssertTrue(state.coverRequired)
        XCTAssertTrue(state.ready(epoch: 2, locked: false))
        XCTAssertFalse(state.coverRequired)
        XCTAssertFalse(state.policy.enabled)
    }

    func testUnconfirmedReleaseEndsOnlyCoversWithoutALockDecision() {
        var launch = AppLockState()
        XCTAssertTrue(launch.releaseUnconfirmed(epoch: 0))
        XCTAssertFalse(launch.coverRequired)

        var away = AppLockState()
        XCTAssertTrue(away.depart(at: 5))
        XCTAssertFalse(away.releaseUnconfirmed(epoch: 1))
        XCTAssertTrue(away.becomeActive())
        XCTAssertTrue(away.releaseUnconfirmed(epoch: 2))

        var preview = AppLockState()
        preview.policy.hideRecentsPreview = true
        XCTAssertTrue(preview.ready(epoch: 0, locked: false))
        XCTAssertTrue(preview.depart(at: 10))
        XCTAssertFalse(preview.releaseUnconfirmed(epoch: 1))
        XCTAssertTrue(preview.becomeActive())
        XCTAssertFalse(preview.releaseUnconfirmed(epoch: 1))
        XCTAssertTrue(preview.coverRequired)
        XCTAssertTrue(preview.releaseUnconfirmed(epoch: 2))
        XCTAssertFalse(preview.coverRequired)
        XCTAssertFalse(preview.releaseUnconfirmed(epoch: 2))

        var timed = AppLockState()
        timed.policy.enabled = true
        timed.policy.timing = "after-1m"
        timed.policy.hideRecentsPreview = true
        XCTAssertTrue(timed.ready(epoch: 0, locked: false))
        XCTAssertTrue(timed.depart(at: 10))
        XCTAssertTrue(timed.becomeActive())
        XCTAssertFalse(timed.releaseUnconfirmed(epoch: 2))
        XCTAssertTrue(timed.coverRequired)

        var gated = AppLockState()
        gated.policy.enabled = true
        gated.policy.gateRequired = true
        gated.policy.hideRecentsPreview = true
        XCTAssertFalse(gated.releaseUnconfirmed(epoch: 0))
        XCTAssertTrue(gated.coverRequired)
        XCTAssertTrue(gated.ready(epoch: 0, locked: true))
        XCTAssertTrue(gated.depart(at: 10))
        XCTAssertTrue(gated.becomeActive())
        XCTAssertFalse(gated.releaseUnconfirmed(epoch: 2))
        XCTAssertTrue(gated.coverRequired)
    }

    func testOneDepartureTimestampPerAwayPeriodAndSleepInclusiveInput() {
        var state = AppLockState()
        state.policy.enabled = true
        state.policy.timing = "after-1m"
        _ = state.ready(epoch: 0, locked: false)
        XCTAssertTrue(state.depart(at: 1_000))
        XCTAssertFalse(state.depart(at: 2_000))
        XCTAssertEqual(state.snapshot(at: 61_000).departedAtMs, 1_000)
        XCTAssertEqual(state.snapshot(at: 61_000).atMs, 61_000)
        XCTAssertEqual(state.epoch, 1)
        _ = state.becomeActive()
        _ = state.ready(epoch: 2, locked: false)
        XCTAssertTrue(state.depart(at: 62_000))
        XCTAssertEqual(state.snapshot(at: 62_000).departedAtMs, 62_000)
    }

    func testLaunchOnlyDoesNotInstallReturnCover() {
        var state = AppLockState()
        state.policy.enabled = true
        _ = state.ready(epoch: 0, locked: false)
        _ = state.depart(at: 10)
        XCTAssertFalse(state.coverRequired)
        _ = state.becomeActive()
        XCTAssertFalse(state.coverRequired)
    }
}
