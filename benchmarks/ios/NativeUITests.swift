import XCTest

final class NativeUITests: XCTestCase {
    private func liveCloud(cancel: Bool) throws {
        continueAfterFailure = false
        guard let key = ProcessInfo.processInfo.environment["RISUNEST_IOS_CLOUD_KEY"], !key.isEmpty else {
            throw XCTSkip("Live Cloud credential not supplied")
        }
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = cancel ? "cloud-cancel" : "cloud"
        app.launchEnvironment["RISUNEST_IOS_CLOUD_KEY"] = key
        app.launch()
        XCTAssertTrue(app.webViews.staticTexts["cloud-streaming"].waitForExistence(timeout: 90))
        if cancel {
            app.webViews.buttons["Cancel live request"].tap()
        } else {
            XCUIDevice.shared.press(.home)
            let elapsed = expectation(description: "Observe real cloud streaming during app transition")
            DispatchQueue.main.asyncAfter(deadline: .now() + 20) { elapsed.fulfill() }
            wait(for: [elapsed], timeout: 25)
            app.activate()
        }
        let result = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "cloud-result:")).firstMatch
        XCTAssertTrue(result.waitForExistence(timeout: 200))
        let measurement = XCTAttachment(string: result.label)
        measurement.lifetime = .keepAlways
        add(measurement)
        XCTAssertTrue(app.webViews.staticTexts["passed"].waitForExistence(timeout: 10))
    }

    func testLiveCloudTransition() throws { try liveCloud(cancel: false) }
    func testLiveCloudCancellation() throws { try liveCloud(cancel: true) }

    func testDeviceCore() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "device-core"
        app.launch()
        let result = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "device-core-result:")).firstMatch
        XCTAssertTrue(result.waitForExistence(timeout: 420))
        let measurement = XCTAttachment(string: result.label)
        measurement.lifetime = .keepAlways
        add(measurement)
        XCTAssertTrue(app.webViews.staticTexts["passed"].waitForExistence(timeout: 10))
    }

    private func cancelPicker(_ app: XCUIApplication) {
        // On iOS 26, export subfolders show a Back button; Cancel is at the root.
        let cancel = app.buttons["Cancel"]
        for _ in 0..<4 {
            if cancel.waitForExistence(timeout: 2) {
                cancel.tap()
                return
            }
            let back = app.navigationBars.buttons.matching(identifier: "BackButton").firstMatch
            guard back.waitForExistence(timeout: 15) else { break }
            back.tap()
        }
        let hierarchy = XCTAttachment(string: app.debugDescription)
        hierarchy.lifetime = .keepAlways
        add(hierarchy)
        XCTFail("The native file picker has no reachable Cancel action")
    }

    private func openPublishedFile(_ app: XCUIApplication) {
        let file = app.cells.containing(NSPredicate(format: "label CONTAINS %@", "synthetic.bin")).firstMatch
        // A new import picker opens Recents, which does not include a newly exported file.
        let browse = app.buttons["Browse"]
        if browse.waitForExistence(timeout: 5) { browse.tap() }
        if !file.waitForExistence(timeout: 3) {
            let local = app.staticTexts.matching(NSPredicate(format: "label IN %@", ["On My iPhone", "On My iPad"])).firstMatch
            XCTAssertTrue(local.waitForExistence(timeout: 15))
            local.tap()
            let folder = app.staticTexts["RisuNest iOS Bench"].firstMatch
            XCTAssertTrue(folder.waitForExistence(timeout: 15))
            folder.tap()
            if !file.waitForExistence(timeout: 3) {
                let exports = app.staticTexts["Exports"].firstMatch
                XCTAssertTrue(exports.waitForExistence(timeout: 15))
                exports.tap()
            }
        }
        XCTAssertTrue(file.waitForExistence(timeout: 15))
        file.tap()
    }

    func testAppTransition() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "background"
        app.launch()
        XCTAssertTrue(app.webViews.staticTexts["background-ready"].waitForExistence(timeout: 30))
        XCUIDevice.shared.press(.home)
        let elapsed = expectation(description: "Observe a ten second app transition")
        DispatchQueue.main.asyncAfter(deadline: .now() + 10) { elapsed.fulfill() }
        wait(for: [elapsed], timeout: 15)
        app.activate()
        let result = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "background-result:")).firstMatch
        XCTAssertTrue(result.waitForExistence(timeout: 60))
        let measurement = XCTAttachment(string: result.label)
        measurement.lifetime = .keepAlways
        add(measurement)
        XCTAssertTrue(app.webViews.staticTexts["passed"].waitForExistence(timeout: 10))
    }

    func testProductKeyboard() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "app"
        app.launch()
        XCTAssertTrue(app.webViews.staticTexts.containing(NSPredicate(format: "label CONTAINS %@", "ios-synthetic-ui-edit")).firstMatch.waitForExistence(timeout: 90))
        let input = app.webViews.textViews.firstMatch
        XCTAssertTrue(input.waitForExistence(timeout: 15))
        input.tap()
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 15))
        input.typeText("synthetic draft")
        XCTAssertTrue((input.value as? String)?.contains("synthetic draft") == true)
        let screenshot = XCTAttachment(screenshot: app.screenshot())
        screenshot.lifetime = .keepAlways
        add(screenshot)
    }

    func testNativePickersAndNotifications() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "ui"
        app.launch()
        XCTAssertTrue(app.webViews.staticTexts["ui-ready"].waitForExistence(timeout: 30))

        app.webViews.buttons["Import synthetic file"].tap()
        cancelPicker(app)
        XCTAssertTrue(app.webViews.staticTexts["import-cancelled"].waitForExistence(timeout: 10))

        app.webViews.buttons["Export synthetic file"].tap()
        cancelPicker(app)
        XCTAssertTrue(app.webViews.staticTexts["export-cancelled"].waitForExistence(timeout: 10))

        app.webViews.buttons["Allow notifications"].tap()
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let allow = springboard.alerts.buttons["Allow"]
        XCTAssertTrue(allow.waitForExistence(timeout: 15))
        allow.tap()
        XCTAssertTrue(app.webViews.staticTexts["notifications-allowed"].waitForExistence(timeout: 10))
        app.webViews.buttons["Open synthetic link"].tap()
        let safari = XCUIApplication(bundleIdentifier: "com.apple.mobilesafari")
        XCTAssertTrue(safari.wait(for: .runningForeground, timeout: 15))
        app.activate()
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testExportPublication() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "ui"
        app.launch()
        XCTAssertTrue(app.webViews.buttons["Export synthetic file"].waitForExistence(timeout: 30))
        app.webViews.buttons["Export synthetic file"].tap()
        let save = app.buttons["Save"]
        XCTAssertTrue(save.waitForExistence(timeout: 30))
        if !save.isEnabled {
            let location = app.staticTexts["On My iPhone"].firstMatch
            if location.exists { location.tap() }
            let folder = app.staticTexts["RisuNest iOS Bench"].firstMatch
            if folder.exists { folder.tap() }
            let exports = app.staticTexts["Exports"].firstMatch
            if exports.exists { exports.tap() }
        }
        let hierarchy = XCTAttachment(string: app.debugDescription)
        hierarchy.lifetime = .keepAlways
        add(hierarchy)
        XCTAssertTrue(save.isEnabled)
        save.tap()
        XCTAssertTrue(app.webViews.staticTexts["exported"].waitForExistence(timeout: 30))
        app.webViews.buttons["Import synthetic file"].tap()
        openPublishedFile(app)
        XCTAssertTrue(app.webViews.staticTexts["imported-exact"].waitForExistence(timeout: 30))
    }
}
