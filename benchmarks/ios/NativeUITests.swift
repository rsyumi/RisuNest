import XCTest

final class NativeUITests: XCTestCase {
    func testNativePickersAndNotifications() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "ui"
        app.launch()
        XCTAssertTrue(app.webViews.staticTexts["ui-ready"].waitForExistence(timeout: 30))

        app.webViews.buttons["Import synthetic file"].tap()
        XCTAssertTrue(app.buttons["Cancel"].waitForExistence(timeout: 15))
        app.buttons["Cancel"].tap()
        XCTAssertTrue(app.webViews.staticTexts["import-cancelled"].waitForExistence(timeout: 10))

        app.webViews.buttons["Export synthetic file"].tap()
        XCTAssertTrue(app.buttons["Cancel"].waitForExistence(timeout: 15))
        app.buttons["Cancel"].tap()
        XCTAssertTrue(app.webViews.staticTexts["export-cancelled"].waitForExistence(timeout: 10))

        app.webViews.buttons["Allow notifications"].tap()
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let allow = springboard.alerts.buttons["Allow"]
        XCTAssertTrue(allow.waitForExistence(timeout: 15))
        allow.tap()
        XCTAssertTrue(app.webViews.staticTexts["notifications-allowed"].waitForExistence(timeout: 10))
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
        XCTAssertTrue(save.waitForExistence(timeout: 15))
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
    }
}
