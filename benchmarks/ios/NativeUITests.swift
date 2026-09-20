import XCTest

final class NativeUITests: XCTestCase {
    private func attachScreenshot(_ app: XCUIApplication, name: String) {
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testNetworkReachability() throws {
        continueAfterFailure = false
        // Independent URLSession control in the XCTest runner, without a key/body.
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 15
        configuration.timeoutIntervalForResource = 20
        let session = URLSession(configuration: configuration)
        let received = expectation(description: "Public endpoint connectivity control")
        session.dataTask(with: URL(string: "https://ollama.com/api/tags")!) { _, response, error in
            let diagnostic: [String: Any] = [
                "status": (response as? HTTPURLResponse)?.statusCode as Any? ?? NSNull(),
                "errorCode": error.map { ($0 as NSError).code as Any } ?? NSNull(),
            ]
            let data = try! JSONSerialization.data(withJSONObject: diagnostic, options: [.sortedKeys])
            let attachment = XCTAttachment(string: "network-control:" + String(data: data, encoding: .utf8)!)
            attachment.lifetime = .keepAlways
            self.add(attachment)
            received.fulfill()
        }.resume()
        wait(for: [received], timeout: 25)
        session.invalidateAndCancel()
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "network"
        app.launch()
        let start = app.webViews.buttons["Start network background work"]
        XCTAssertTrue(start.waitForExistence(timeout: 60))
        start.tap()
        let result = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "network-result:")).firstMatch
        XCTAssertTrue(result.waitForExistence(timeout: 60))
        let measurement = XCTAttachment(string: result.value as? String ?? result.label)
        measurement.lifetime = .keepAlways
        add(measurement)
        // This test collects connectivity evidence; Cloud tests enforce completion.
        XCTAssertTrue(app.webViews.staticTexts["passed"].waitForExistence(timeout: 10))
    }

    private func liveCloud(cancel: Bool) throws {
        continueAfterFailure = false
        guard let key = ProcessInfo.processInfo.environment["RISUNEST_IOS_CLOUD_KEY"], !key.isEmpty else {
            throw XCTSkip("Live Cloud credential not supplied")
        }
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = cancel ? "cloud-cancel" : "cloud"
        app.launchEnvironment["RISUNEST_IOS_CLOUD_KEY"] = key
        app.launch()
        let start = app.webViews.buttons["Start live request"]
        XCTAssertTrue(start.waitForExistence(timeout: 30))
        start.tap()
        let result = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "cloud-result:")).firstMatch
        let streaming = app.webViews.staticTexts["cloud-streaming"]
        let opened = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in streaming.exists || result.exists }, object: nil)
        wait(for: [opened], timeout: 90)
        if result.exists {
            let measurement = XCTAttachment(string: result.value as? String ?? result.label)
            measurement.lifetime = .keepAlways
            add(measurement)
        }
        XCTAssertTrue(streaming.exists, "Cloud must begin streaming before the transition/cancellation")
        if cancel {
            app.webViews.buttons["Cancel live request"].tap()
        } else {
            XCUIDevice.shared.press(.home)
            let elapsed = expectation(description: "Observe real cloud streaming during app transition")
            DispatchQueue.main.asyncAfter(deadline: .now() + 20) { elapsed.fulfill() }
            wait(for: [elapsed], timeout: 25)
            app.activate()
        }
        XCTAssertTrue(result.waitForExistence(timeout: 200))
        let measurement = XCTAttachment(string: result.value as? String ?? result.label)
        measurement.lifetime = .keepAlways
        add(measurement)
        XCTAssertTrue(app.webViews.staticTexts["passed"].waitForExistence(timeout: 10))
    }

    func testLiveCloudTransition() throws { try liveCloud(cancel: false) }
    func testLiveCloudCancellation() throws { try liveCloud(cancel: true) }

    func testOAuthCallbackReturn() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "oauth"
        app.launch()
        let start = app.webViews.buttons["Start OAuth callback"]
        XCTAssertTrue(start.waitForExistence(timeout: 60))
        start.tap()

        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let continuations = [
            app.buttons["Continue"],
            app.alerts.buttons["Continue"],
            springboard.buttons["Continue"],
            springboard.alerts.buttons["Continue"],
        ]
        for button in continuations where button.waitForExistence(timeout: 3) {
            button.tap()
            break
        }

        let result = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "oauth-result:")).firstMatch
        let failure = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "verification-error:")).firstMatch
        let completed = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in result.exists || failure.exists }, object: nil)
        wait(for: [completed], timeout: 90)
        if failure.exists {
            let diagnostic = XCTAttachment(string: failure.label)
            diagnostic.lifetime = .keepAlways
            add(diagnostic)
        }
        XCTAssertTrue(result.exists)
        let measurement = XCTAttachment(string: result.label)
        measurement.lifetime = .keepAlways
        add(measurement)
        XCTAssertTrue(app.webViews.staticTexts["passed"].waitForExistence(timeout: 10))
    }

    func testDeviceCore() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "device-core"
        app.launch()
        let result = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "device-core-result:")).firstMatch
        let failure = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "verification-error:")).firstMatch
        let completed = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in result.exists || failure.exists }, object: nil)
        wait(for: [completed], timeout: 420)
        if failure.exists {
            let diagnostic = XCTAttachment(string: failure.label)
            diagnostic.lifetime = .keepAlways
            add(diagnostic)
        }
        XCTAssertTrue(result.exists)
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
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "background-ui"
        app.launch()
        let start = app.webViews.buttons["Start background work"]
        XCTAssertTrue(start.waitForExistence(timeout: 30))
        start.tap()
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

    func testProductOnboardingScreenshots() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchArguments += ["-AppleLanguages", "(en)", "-AppleLocale", "en_US"]
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "onboarding"
        app.launch()

        let webView = app.webViews.firstMatch
        let home = webView.staticTexts["Choose how to start."]
        XCTAssertTrue(home.waitForExistence(timeout: 90))
        attachScreenshot(app, name: "01-onboarding-start")

        let importOption = webView.staticTexts["Import from a backup file"]
        XCTAssertTrue(importOption.waitForExistence(timeout: 15))
        importOption.tap()
        XCTAssertTrue(webView.staticTexts["Import a backup file"].waitForExistence(timeout: 15))
        attachScreenshot(app, name: "02-onboarding-import")

        let backToStart = webView.buttons["Back to start"]
        XCTAssertTrue(backToStart.waitForExistence(timeout: 15))
        backToStart.tap()
        XCTAssertTrue(home.waitForExistence(timeout: 15))

        let syncOption = webView.staticTexts["Connect to a sync server"]
        XCTAssertTrue(syncOption.waitForExistence(timeout: 15))
        syncOption.tap()
        XCTAssertTrue(webView.staticTexts["Choose how to sync."].waitForExistence(timeout: 15))
        attachScreenshot(app, name: "03-onboarding-sync")

        XCTAssertTrue(backToStart.waitForExistence(timeout: 15))
        backToStart.tap()
        XCTAssertTrue(home.waitForExistence(timeout: 15))
        let fresh = webView.staticTexts["Start right away"]
        XCTAssertTrue(fresh.waitForExistence(timeout: 15))
        fresh.tap()
        XCTAssertTrue(webView.staticTexts["Ready"].waitForExistence(timeout: 15))
        attachScreenshot(app, name: "04-onboarding-ready")

        let start = webView.buttons["Start RisuNest"]
        XCTAssertTrue(start.waitForExistence(timeout: 15))
        start.tap()
        XCTAssertTrue(webView.staticTexts["Ready"].waitForNonExistence(timeout: 30))
        let settled = expectation(description: "Product chat settles after onboarding")
        DispatchQueue.main.asyncAfter(deadline: .now() + 5) { settled.fulfill() }
        wait(for: [settled], timeout: 10)
        attachScreenshot(app, name: "05-product-after-onboarding")
    }

    func testProductSettingsScreenshots() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchArguments += ["-AppleLanguages", "(en)", "-AppleLocale", "en_US"]
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "settings"
        app.launch()

        let webView = app.webViews.firstMatch
        XCTAssertTrue(webView.staticTexts["Performance"].waitForExistence(timeout: 90))
        attachScreenshot(app, name: "06-risunest-settings")

        let platform = webView.staticTexts["Platform"]
        for _ in 0..<8 where !platform.isHittable {
            webView.swipeUp()
        }
        XCTAssertTrue(platform.waitForExistence(timeout: 15))
        let notifications = webView.staticTexts["Notifications"]
        XCTAssertTrue(notifications.waitForExistence(timeout: 15))
        attachScreenshot(app, name: "07-risunest-ios-platform")
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
