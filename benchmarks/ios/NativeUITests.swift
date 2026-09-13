import XCTest

final class NativeUITests: XCTestCase {
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
        app.webViews.buttons["Import synthetic file"].tap()
        let file = app.cells.containing(NSPredicate(format: "label CONTAINS %@", "synthetic")).firstMatch
        XCTAssertTrue(file.waitForExistence(timeout: 15))
        file.tap()
        XCTAssertTrue(app.webViews.staticTexts["imported-exact"].waitForExistence(timeout: 30))
    }
}
