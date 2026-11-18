import XCTest
import Foundation

final class NativeUITests: XCTestCase {
    private func legacyRestoreNumber(_ value: Any?) -> UInt64? {
        guard let number = value as? NSNumber,
              CFGetTypeID(number) != CFBooleanGetTypeID() else { return nil }
        let value: UInt64
        switch String(cString: number.objCType) {
        case "s", "i", "l", "q":
            guard number.int64Value >= 0 else { return nil }
            value = UInt64(number.int64Value)
        case "S", "I", "L", "Q":
            value = number.uint64Value
        default:
            return nil
        }
        guard value <= 9_007_199_254_740_991 else { return nil }
        return value
    }

    private func legacyRestoreBoolean(_ value: Any?) -> Bool? {
        guard let number = value as? NSNumber,
              CFGetTypeID(number) == CFBooleanGetTypeID() else { return nil }
        return number.boolValue
    }

    private func logLegacyRestoreNumbers(_ evidence: String, phase: String, megabytes: Int, encoding: String, invocation: Int) {
        guard [100, 300, 600].contains(megabytes),
              ["raw", "gzip"].contains(encoding),
              [1, 2].contains(invocation),
              ["prepared", "restore-terminal", "verified"].contains(phase) else {
            print("RISUNEST_CR228_METRIC valid=0")
            return
        }
        let prefix = phase == "prepared" ? "legacy-restore-ready:" : phase == "verified" ? "legacy-restore-result:" : "legacy-restore-event:"
        guard evidence.hasPrefix(prefix),
              let data = String(evidence.dropFirst(prefix.count)).data(using: .utf8),
              let payload = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              payload["schema"] as? String == "risunest.synthetic-legacy-restore/v1",
              legacyRestoreBoolean(payload["synthetic"]) == true,
              payload["phase"] as? String == phase,
              payload["encoding"] as? String == encoding,
              let decodedBytes = legacyRestoreNumber(payload["decodedBytes"]), decodedBytes > 0,
              let sourceBytes = legacyRestoreNumber(payload["sourceBytes"]), sourceBytes > 0,
              let characterCount = legacyRestoreNumber(payload["characterCount"]),
              let messageCount = legacyRestoreNumber(payload["messageCount"]) else {
            print("RISUNEST_CR228_METRIC valid=0")
            return
        }
        let fields = "phase=\(phase) case_mb=\(megabytes) encoding=\(encoding) invocation=\(invocation) decoded_bytes=\(decodedBytes) source_bytes=\(sourceBytes) character_count=\(characterCount) message_count=\(messageCount)"
        if phase == "prepared" {
            guard legacyRestoreBoolean(payload["resetVerified"]) == true,
                  let memory = payload["memoryBefore"] as? [String: Any],
                  memory["source"] as? String == "darwin-task-resident-and-getrusage-lifetime-bytes",
                  let beforePeak = legacyRestoreNumber(memory["peakRssBytes"]), beforePeak > 0,
                  let beforeResident = legacyRestoreNumber(memory["residentBytes"]), beforeResident > 0 else {
                print("RISUNEST_CR228_METRIC valid=0")
                return
            }
            print("RISUNEST_CR228_METRIC valid=1 \(fields) reset_verified=1 before_lifetime_peak_bytes=\(beforePeak) before_resident_bytes=\(beforeResident)")
        } else {
            guard payload["memorySource"] as? String == "darwin-task-resident-and-getrusage-lifetime-bytes",
                  payload["outcome"] as? String == "succeeded",
                  let peak = legacyRestoreNumber(payload["peakRssBytes"]), peak > 0,
                  let samples = legacyRestoreNumber(payload["memorySamples"]), samples > 0,
                  let baseline = legacyRestoreNumber(payload["restoreBaselineBytes"]), baseline > 0,
                  let increment = legacyRestoreNumber(payload["restoreIncrementBytes"]),
                  let elapsed = legacyRestoreNumber(payload["elapsedMs"]),
                  let aboveTwice = legacyRestoreBoolean(payload["aboveTwiceDecoded"]) else {
                print("RISUNEST_CR228_METRIC valid=0")
                return
            }
            let terminal = "succeeded=1 restore_baseline_bytes=\(baseline) restore_increment_bytes=\(increment) lifetime_peak_bytes=\(peak) memory_samples=\(samples) elapsed_ms=\(elapsed) above_twice_decoded=\(aboveTwice ? 1 : 0)"
            if phase == "verified" {
                guard let verifiedCharacters = legacyRestoreNumber(payload["verifiedCharacterCount"]),
                      let verifiedMessages = legacyRestoreNumber(payload["verifiedMessageCount"]) else {
                    print("RISUNEST_CR228_METRIC valid=0")
                    return
                }
                print("RISUNEST_CR228_METRIC valid=1 \(fields) \(terminal) verified_character_count=\(verifiedCharacters) verified_message_count=\(verifiedMessages)")
            } else {
                print("RISUNEST_CR228_METRIC valid=1 \(fields) \(terminal)")
            }
        }
    }

    private func logSyntheticStep(_ app: XCUIApplication, step: Int, reached: Bool) {
        let texts = app.webViews.staticTexts
        let failure = texts.containing(NSPredicate(format: "label BEGINSWITH %@", "verification-error:")).firstMatch
        let errorPrefixExists = failure.exists
        let knownErrors = [
            "Restore result counts differ from the generated fixture",
            "Restored synthetic message hash mismatch",
            "Restored synthetic message count mismatch",
            "Legacy restore did not verify",
        ]
        var category = 0
        if errorPrefixExists {
            category = knownErrors.firstIndex { texts["verification-error:" + $0].exists }.map { $0 + 1 } ?? 5
        }
        print("RISUNEST_CR228_STEP step=\(step) reached=\(reached ? 1 : 0) app_state=\(app.state.rawValue) webview_exists=\(app.webViews.firstMatch.exists ? 1 : 0) failed_exists=\(texts["failed"].exists ? 1 : 0) error_prefix_exists=\(errorPrefixExists ? 1 : 0) error_category=\(category)")
        let progress = texts.containing(NSPredicate(format: "label BEGINSWITH %@", "legacy-readback-stage:")).firstMatch
        guard progress.exists,
              let data = String(progress.label.dropFirst("legacy-readback-stage:".count)).data(using: .utf8),
              let values = try? JSONSerialization.jsonObject(with: data) as? [String: NSNumber],
              Set(values.keys) == Set(["stage", "index", "readReturned", "hashVerified", "messageCount", "elapsedMs"]),
              values.values.allSatisfy({ $0.doubleValue.isFinite && $0.doubleValue >= 0 && $0.doubleValue.rounded() == $0.doubleValue }),
              let stage = values["stage"], (1...14).contains(stage.intValue),
              let index = values["index"], index.intValue <= 1000,
              let returned = values["readReturned"], returned.intValue <= 1000,
              let verified = values["hashVerified"], verified.intValue <= returned.intValue,
              let messages = values["messageCount"], messages.intValue <= 500000,
              let elapsed = values["elapsedMs"], elapsed.int64Value <= 3600000 else {
            print("RISUNEST_CR228_READBACK valid=0 boundary=\(step)")
            return
        }
        print("RISUNEST_CR228_READBACK valid=1 boundary=\(step) stage=\(stage.intValue) index=\(index.intValue) read_returned=\(returned.intValue) hash_verified=\(verified.intValue) messages=\(messages.intValue) elapsed_ms=\(elapsed.int64Value)")
    }

    private func legacyRestoreMemory(_ megabytes: Int, _ encoding: String) throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        let options = XCTMeasureOptions()
        options.iterationCount = 1
        options.invocationOptions = [.manuallyStart, .manuallyStop]
        var invocations = 0
        var readyIdentities = Set<String>()
        measure(metrics: [XCTMemoryMetric(application: app)], options: options) {
            invocations += 1
            app.terminate()
            app.launchEnvironment["RISUNEST_IOS_PHASE"] = "legacy-restore-\(megabytes)-\(encoding)"
            app.launch()
            let start = app.webViews.buttons["Start synthetic restore"]
            XCTAssertTrue(start.waitForExistence(timeout: 1200), "Fresh synthetic fixture preparation failed")
            let ready = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "legacy-restore-ready:")).firstMatch
            let readyEvidence = ready.label
            XCTAssertTrue(readyEvidence.contains("\"resetVerified\":true"))
            XCTAssertTrue(readyIdentities.insert(readyEvidence).inserted, "Each invocation requires a distinct reset and fixture")
            let preparation = XCTAttachment(string: readyEvidence)
            preparation.name = "legacy-restore-prepared-\(megabytes)-\(encoding)-\(invocations)"
            preparation.lifetime = .keepAlways
            add(preparation)
            logLegacyRestoreNumbers(readyEvidence, phase: "prepared", megabytes: megabytes, encoding: encoding, invocation: invocations)
            startMeasuring()
            start.tap()
            let verify = app.webViews.buttons["Verify synthetic restore"]
            let restored = verify.waitForExistence(timeout: 1200)
            stopMeasuring()
            let terminal = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "legacy-restore-event:")).firstMatch
            let terminalEvidence = terminal.exists ? terminal.label : "legacy-restore-incomplete:appState=\(app.state.rawValue)"
            let restore = XCTAttachment(string: terminalEvidence)
            restore.name = "legacy-restore-native-rss-\(megabytes)-\(encoding)-\(invocations)"
            restore.lifetime = .keepAlways
            add(restore)
            logLegacyRestoreNumbers(terminalEvidence, phase: "restore-terminal", megabytes: megabytes, encoding: encoding, invocation: invocations)
            XCTAssertTrue(restored, "Collect native events and OS termination evidence before classifying a missing result as a memory kill")
            logSyntheticStep(app, step: 100, reached: false)
            verify.tap()
            let result = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "legacy-restore-result:")).firstMatch
            let failure = app.webViews.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "verification-error:")).firstMatch
            let failed = app.webViews.staticTexts["failed"]
            let completed = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in result.exists || failure.exists || failed.exists }, object: nil)
            _ = XCTWaiter.wait(for: [completed], timeout: 1200)
            logSyntheticStep(app, step: 101, reached: result.exists)
            XCTAssertTrue(result.exists, "Synthetic readback verification failed")
            let evidence = result.label
            let attachment = XCTAttachment(string: evidence)
            attachment.name = "legacy-restore-verified-\(megabytes)-\(encoding)-\(invocations)"
            attachment.lifetime = .keepAlways
            add(attachment)
            logLegacyRestoreNumbers(evidence, phase: "verified", megabytes: megabytes, encoding: encoding, invocation: invocations)
            XCTAssertTrue(evidence.contains("\"phase\":\"verified\""))
            app.terminate()
        }
        XCTAssertEqual(invocations, 2, "One discarded warmup and one measured restore are required")
        XCTAssertEqual(readyIdentities.count, 2)
    }
    func testLegacyRestore100Raw() throws { try legacyRestoreMemory(100, "raw") }
    func testLegacyRestore100Gzip() throws { try legacyRestoreMemory(100, "gzip") }
    func testLegacyRestore300Raw() throws { try legacyRestoreMemory(300, "raw") }
    func testLegacyRestore300Gzip() throws { try legacyRestoreMemory(300, "gzip") }
    func testLegacyRestore600Raw() throws { try legacyRestoreMemory(600, "raw") }
    func testLegacyRestore600Gzip() throws { try legacyRestoreMemory(600, "gzip") }

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
        XCTAssertTrue(result.waitForExistence(timeout: 300))
        let measurement = XCTAttachment(string: result.value as? String ?? result.label)
        measurement.lifetime = .keepAlways
        add(measurement)
        XCTAssertTrue(app.webViews.staticTexts["passed"].waitForExistence(timeout: 10))
    }

    func testLiveCloudTransition() throws { try liveCloud(cancel: false) }
    func testLiveCloudCancellation() throws { try liveCloud(cancel: true) }

    private final class SyncPreflight {
        var status: Int?
        var error: String?
        var errorCode: Int?
    }

    private static let registrationPrefix = "risunestlocal://sync-server/register#"

    private func syncInputs() throws -> (registration: String, expect: String, marker: String) {
        let environment = ProcessInfo.processInfo.environment
        guard let supplied = environment["RISUNEST_IOS_SYNC_REGISTRATION"], !supplied.isEmpty,
              let expect = environment["RISUNEST_IOS_SYNC_EXPECT"], !expect.isEmpty,
              let marker = environment["RISUNEST_IOS_SYNC_MARKER"], !marker.isEmpty else {
            throw XCTSkip("Sync registration not supplied")
        }
        return (Self.registrationURI(supplied), expect, marker)
    }

    // Test Lab regenerates the xctestrun it runs, so the registration payload may be supplied without its URI prefix.
    private static func registrationURI(_ supplied: String) -> String {
        supplied.contains("://") ? supplied : registrationPrefix + supplied
    }

    private func syncEnvironment(_ outcome: [String: Any]) {
        let data = try! JSONSerialization.data(withJSONObject: outcome, options: [.sortedKeys])
        let line = "sync-env: " + String(data: data, encoding: .utf8)!
        print(line)
        let attachment = XCTAttachment(string: line)
        attachment.name = "sync-env"
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    /// Reaches the registration endpoint from the device before the app does. Only the status is recorded.
    private func syncPreflight(_ registration: String, phase: String) {
        let prefix = Self.registrationPrefix
        var url: URL?
        if registration.hasPrefix(prefix) {
            var encoded = String(registration.dropFirst(prefix.count))
                .replacingOccurrences(of: "-", with: "+")
                .replacingOccurrences(of: "_", with: "/")
            encoded += String(repeating: "=", count: (4 - encoded.count % 4) % 4)
            if let data = Data(base64Encoded: encoded),
               let payload = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
               let endpoint = payload["endpoint"] as? String {
                url = URL(string: endpoint + "/head")
            }
        }
        guard let url else {
            syncEnvironment([
                "phase": phase, "stage": "preflight", "passed": false, "reason": "registration-unreadable",
                "length": registration.count, "prefixed": registration.hasPrefix(prefix),
                "hasFragment": registration.contains("#"),
            ])
            XCTFail("sync-env: the registration could not be read")
            return
        }
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 20
        configuration.timeoutIntervalForResource = 30
        let session = URLSession(configuration: configuration)
        let observed = SyncPreflight()
        let received = expectation(description: "Sync endpoint preflight")
        session.dataTask(with: url) { data, response, error in
            observed.status = (response as? HTTPURLResponse)?.statusCode
            observed.errorCode = error.map { ($0 as NSError).code }
            if let data, let body = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
                observed.error = body["error"] as? String
            }
            received.fulfill()
        }.resume()
        wait(for: [received], timeout: 40)
        session.invalidateAndCancel()
        // An unauthenticated request reaching the server is refused with this exact error.
        let reachable = observed.status == 401 && observed.error == "unauthorized"
        syncEnvironment([
            "phase": phase, "stage": "preflight", "passed": reachable, "https": url.scheme == "https",
            "status": observed.status as Any? ?? NSNull(), "errorCode": observed.errorCode as Any? ?? NSNull(),
        ])
        XCTAssertTrue(reachable, "sync-env: the registration endpoint is not reachable from the device")
    }

    private func runSync(phase: String, environment: [String: String]) {
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = phase
        for (key, value) in environment {
            app.launchEnvironment[key] = value
        }
        app.launch()
        let texts = app.webViews.staticTexts
        let result = texts.containing(NSPredicate(format: "label BEGINSWITH %@", "sync-result:")).firstMatch
        let failure = texts.containing(NSPredicate(format: "label BEGINSWITH %@", "verification-error:")).firstMatch
        let step = texts.containing(NSPredicate(format: "label BEGINSWITH %@", "sync-step:")).firstMatch
        var lastStep = "sync-step:unknown"
        // An app that stops running ends the wait at once, with the last step it showed.
        // The step label is replaced as the phase advances, so it is read without failing when it is gone.
        let settled = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            if result.exists || failure.exists || app.state == .notRunning { return true }
            if let label = try? step.snapshot().label { lastStep = label }
            return false
        }, object: nil)
        let outcome = XCTWaiter().wait(for: [settled], timeout: 600)
        if app.state == .notRunning {
            let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
            attachment.name = "\(phase)-final"
            attachment.lifetime = .keepAlways
            add(attachment)
        } else {
            attachScreenshot(app, name: "\(phase)-final")
        }
        guard outcome == .completed, result.exists || failure.exists else {
            let stopped = app.state == .notRunning
            let last = (try? step.snapshot().label) ?? lastStep
            let attachment = XCTAttachment(string: "\(stopped ? "app-stopped" : "timeout") \(last)")
            attachment.name = "\(phase)-last-step"
            attachment.lifetime = .keepAlways
            add(attachment)
            print(last)
            XCTFail(stopped ? "\(phase) stopped running; last \(last)" : "\(phase) did not finish; last \(last)")
            return
        }
        let evidence = result.exists ? result.label : failure.label
        let attachment = XCTAttachment(string: evidence)
        attachment.name = "\(phase)-result"
        attachment.lifetime = .keepAlways
        add(attachment)
        print(evidence)
        XCTAssertTrue(evidence.hasPrefix("sync-result:passed"), evidence)
    }

    func testSyncConnect() throws {
        continueAfterFailure = false
        let inputs = try syncInputs()
        syncPreflight(inputs.registration, phase: "sync")
        runSync(phase: "sync", environment: [
            "RISUNEST_IOS_SYNC_REGISTRATION": inputs.registration,
            "RISUNEST_IOS_SYNC_EXPECT": inputs.expect,
            "RISUNEST_IOS_SYNC_MARKER": inputs.marker,
        ])
    }

    /// Publishes a seeded library with `RISUNEST_IOS_SYNC_PUBLISH` in a message to an empty server library.
    func testSyncPublish() throws {
        continueAfterFailure = false
        let environment = ProcessInfo.processInfo.environment
        guard let supplied = environment["RISUNEST_IOS_SYNC_REGISTRATION"], !supplied.isEmpty,
              let marker = environment["RISUNEST_IOS_SYNC_PUBLISH"], !marker.isEmpty else {
            throw XCTSkip("Sync publication not requested")
        }
        let registration = Self.registrationURI(supplied)
        syncPreflight(registration, phase: "sync-publish")
        runSync(phase: "sync-publish", environment: [
            "RISUNEST_IOS_SYNC_REGISTRATION": registration,
            "RISUNEST_IOS_SYNC_PUBLISH": marker,
        ])
    }

    /// Runs after testSyncConnect in the same installation and relaunches without the registration.
    func testSyncRestart() throws {
        continueAfterFailure = false
        let inputs = try syncInputs()
        syncPreflight(inputs.registration, phase: "sync-restart")
        runSync(phase: "sync-restart", environment: ["RISUNEST_IOS_SYNC_MARKER": inputs.marker])
    }

    private func externalInputs() throws -> [String: String] {
        let environment = ProcessInfo.processInfo.environment
        let names = ["RISUNEST_IOS_WEBDAV_URL", "RISUNEST_IOS_WEBDAV_USER", "RISUNEST_IOS_WEBDAV_PASSWORD",
                     "RISUNEST_IOS_WEBDAV_ROOT", "RISUNEST_IOS_EXTERNAL_BEFORE", "RISUNEST_IOS_EXTERNAL_AFTER"]
        var inputs: [String: String] = [:]
        for name in names {
            guard let value = environment[name], !value.isEmpty else { throw XCTSkip("WebDAV server not supplied") }
            inputs[name] = value
        }
        return inputs
    }

    /// Reaches the WebDAV server from the device before the app does. It must refuse an unauthenticated request.
    private func webDavPreflight(_ endpoint: String, phase: String) {
        guard let url = URL(string: endpoint) else {
            syncEnvironment(["phase": phase, "stage": "webdav-preflight", "passed": false, "reason": "url-unreadable"])
            XCTFail("sync-env: the WebDAV URL could not be read")
            return
        }
        // A loopback server on the simulator host is plain HTTP, which the runner's URLSession refuses.
        guard url.scheme == "https" else {
            syncEnvironment(["phase": phase, "stage": "webdav-preflight", "passed": true, "skipped": "loopback"])
            return
        }
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 20
        configuration.timeoutIntervalForResource = 30
        let session = URLSession(configuration: configuration)
        let observed = SyncPreflight()
        let received = expectation(description: "WebDAV endpoint preflight")
        session.dataTask(with: url) { _, response, error in
            observed.status = (response as? HTTPURLResponse)?.statusCode
            observed.errorCode = error.map { ($0 as NSError).code }
            received.fulfill()
        }.resume()
        wait(for: [received], timeout: 40)
        session.invalidateAndCancel()
        let reachable = observed.status == 401
        syncEnvironment([
            "phase": phase, "stage": "webdav-preflight", "passed": reachable, "https": url.scheme == "https",
            "status": observed.status as Any? ?? NSNull(), "errorCode": observed.errorCode as Any? ?? NSNull(),
        ])
        XCTAssertTrue(reachable, "sync-env: the WebDAV server is not reachable from the device")
    }

    /// Connects a new WebDAV backup repository, backs up, changes the library and restores the backup.
    func testExternalStorageBackupRestore() throws {
        continueAfterFailure = false
        let inputs = try externalInputs()
        webDavPreflight(inputs["RISUNEST_IOS_WEBDAV_URL"]!, phase: "external-storage")
        runSync(phase: "external-storage", environment: inputs)
    }

    /// Runs after testExternalStorageBackupRestore in the same installation and relaunches without the password.
    func testExternalStorageRestart() throws {
        continueAfterFailure = false
        let inputs = try externalInputs()
        webDavPreflight(inputs["RISUNEST_IOS_WEBDAV_URL"]!, phase: "external-storage-restart")
        runSync(phase: "external-storage-restart",
                environment: ["RISUNEST_IOS_EXTERNAL_BEFORE": inputs["RISUNEST_IOS_EXTERNAL_BEFORE"]!])
    }

    private func suppliedInputs(_ names: [String], otherwise reason: String) throws -> [String: String] {
        let environment = ProcessInfo.processInfo.environment
        var inputs: [String: String] = [:]
        for name in names {
            guard let value = environment[name], !value.isEmpty else { throw XCTSkip(reason) }
            inputs[name] = value
        }
        return inputs
    }

    private static let externalSyncInputs = ["RISUNEST_IOS_WEBDAV_URL", "RISUNEST_IOS_WEBDAV_USER", "RISUNEST_IOS_WEBDAV_PASSWORD",
                                             "RISUNEST_IOS_WEBDAV_SYNC_ROOT", "RISUNEST_IOS_EXTERNAL_SYNC_FIRST",
                                             "RISUNEST_IOS_EXTERNAL_SYNC_SECOND"]

    /// Connects a new WebDAV sync repository, turns sync on, changes a message and publishes it.
    func testExternalSyncPublish() throws {
        continueAfterFailure = false
        let inputs = try suppliedInputs(Self.externalSyncInputs, otherwise: "WebDAV sync repository not supplied")
        webDavPreflight(inputs["RISUNEST_IOS_WEBDAV_URL"]!, phase: "external-sync")
        runSync(phase: "external-sync", environment: inputs)
    }

    /// Runs after testExternalSyncPublish in the same installation and relaunches without the password.
    func testExternalSyncRestart() throws {
        continueAfterFailure = false
        let inputs = try suppliedInputs(Self.externalSyncInputs, otherwise: "WebDAV sync repository not supplied")
        webDavPreflight(inputs["RISUNEST_IOS_WEBDAV_URL"]!, phase: "external-sync-restart")
        runSync(phase: "external-sync-restart", environment: [
            "RISUNEST_IOS_EXTERNAL_SYNC_FIRST": inputs["RISUNEST_IOS_EXTERNAL_SYNC_FIRST"]!,
            "RISUNEST_IOS_EXTERNAL_SYNC_SECOND": inputs["RISUNEST_IOS_EXTERNAL_SYNC_SECOND"]!,
        ])
    }

    private static let backupFileInputs = ["RISUNEST_IOS_BACKUP_FILE_BEFORE", "RISUNEST_IOS_BACKUP_FILE_AFTER"]

    /// Exports a RisuNest backup to a temporary file, changes a message and restores the file in the same app.
    func testBackupFileRestore() throws {
        continueAfterFailure = false
        let inputs = try suppliedInputs(Self.backupFileInputs, otherwise: "Backup file markers not supplied")
        runSync(phase: "backup-file", environment: inputs)
    }

    /// Runs after testBackupFileRestore in the same installation and checks the restored library after a relaunch.
    func testBackupFileRestoreRestart() throws {
        continueAfterFailure = false
        let inputs = try suppliedInputs(Self.backupFileInputs, otherwise: "Backup file markers not supplied")
        runSync(phase: "backup-file-restart", environment: inputs)
    }

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

    private func cancelPicker(_ app: XCUIApplication, step: Int) {
        // On iOS 26, export subfolders show a Back button; Cancel is at the root.
        let cancel = app.buttons["Cancel"]
        // Earlier iOS titles a folder's back button with its parent and gives it no BackButton identifier.
        let parentTitles = ["Back", "Browse", "Locations", "On My iPhone", "On My iPad", "RisuNest iOS Bench"]
        let titledBack = app.navigationBars.buttons.matching(NSPredicate(format: "label IN %@", parentTitles)).firstMatch
        for _ in 0..<4 {
            if cancel.waitForExistence(timeout: 2) {
                cancel.tap()
                return
            }
            let back = app.navigationBars.buttons.matching(identifier: "BackButton").firstMatch
            if back.waitForExistence(timeout: 15) {
                back.tap()
                continue
            }
            guard titledBack.waitForExistence(timeout: 2) else { break }
            titledBack.tap()
        }
        let back = app.navigationBars.buttons.matching(identifier: "BackButton").firstMatch
        let close = app.buttons["Close"]
        let cancelExists = cancel.exists
        let cancelHittable = cancelExists && cancel.isHittable
        let backExists = back.exists
        let backHittable = backExists && back.isHittable
        let closeExists = close.exists
        let closeHittable = closeExists && close.isHittable
        print("RISUNEST_CR228_PICKER_CAPABILITY step=\(step) cancel_exists=\(cancelExists ? 1 : 0) cancel_hittable=\(cancelHittable ? 1 : 0) back_exists=\(backExists ? 1 : 0) back_hittable=\(backHittable ? 1 : 0) close_exists=\(closeExists ? 1 : 0) close_hittable=\(closeHittable ? 1 : 0) titled_back_exists=\(titledBack.exists ? 1 : 0)")
        let navigation = app.navigationBars.buttons.allElementsBoundByIndex.prefix(8)
            .map { "\($0.identifier)/\($0.label)" }.joined(separator: ",")
        print("RISUNEST_CR228_PICKER_NAVIGATION step=\(step) bars=\(app.navigationBars.count) buttons=\(navigation)")
        if closeExists && closeHittable {
            close.tap()
            return
        }
        logSyntheticStep(app, step: 207, reached: false)
        XCTFail("The native file picker has no reachable Cancel action")
    }

    private func openPublishedFile(_ app: XCUIApplication, name: String = "synthetic.bin") {
        let file = app.cells.containing(NSPredicate(format: "label CONTAINS %@", name)).firstMatch
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

    func testAppLockRetainsDraft() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "app-lock-ui"
        app.launch()
        XCTAssertTrue(app.webViews.staticTexts["app-lock-ready"].waitForExistence(timeout: 90))
        let draft = app.webViews.textViews["Retained synthetic draft"]
        XCTAssertEqual(draft.value as? String, "retained synthetic draft 123456")
        XCUIDevice.shared.press(.home)
        app.activate()
        for digit in ["1", "2", "3", "4", "5", "6"] {
            let key = app.webViews.buttons[digit]
            XCTAssertTrue(key.waitForExistence(timeout: 30))
            key.tap()
        }
        XCTAssertTrue(app.webViews.staticTexts["app-lock-retained"].waitForExistence(timeout: 30))
        XCTAssertEqual(draft.value as? String, "retained synthetic draft 123456")
        attachScreenshot(app, name: "app-lock-retained-draft")
    }

    func testAppLockInactiveDepartures() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "app-lock-inactive"
        app.launch()
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let texts = app.webViews.staticTexts
        let failure = texts.containing(NSPredicate(format: "label BEGINSWITH %@", "verification-error:")).firstMatch
        let cardPredicate = NSPredicate(format: "identifier CONTAINS %@ OR label == %@", "io.github.rsyumi.risunest.ios.bench", "RisuNest iOS Bench")
        // Preview hiding with the lock off, a return before the one-minute limit, and an immediate lock,
        // first under Notification Center and then from the app switcher.
        var inactiveOnly: [String] = []
        for step in ["preview", "timed", "immediate", "switcher-immediate", "switcher-timed", "switcher-preview"] {
            let ready = texts["app-lock-inactive-\(step)-ready"]
            XCTAssertTrue(ready.waitForExistence(timeout: 90), "\(step) step did not start")
            let switcher = step.hasPrefix("switcher-")
            if switcher {
                // Holding the home gesture halfway up opens the app switcher without leaving the app.
                springboard.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.995))
                    .press(forDuration: 0.1, thenDragTo: springboard.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.6)),
                           withVelocity: .slow, thenHoldForDuration: 1)
            } else {
                // Notification Center over the app, which leaves it inactive where iOS does not move it to the background.
                springboard.coordinate(withNormalizedOffset: CGVector(dx: 0.25, dy: 0))
                    .press(forDuration: 0.1, thenDragTo: springboard.coordinate(withNormalizedOffset: CGVector(dx: 0.25, dy: 0.75)))
            }
            let away = expectation(description: "Stay away from the app")
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { away.fulfill() }
            wait(for: [away], timeout: 5)
            let screen = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
            screen.name = "app-lock-inactive-\(step)-away"
            screen.lifetime = .keepAlways
            add(screen)
            // Whether a departure stays inactive depends on the iOS version, so each one is recorded and
            // the test fails below only when none of them left the app in the foreground.
            let awayState = app.state
            print("RISUNEST_APP_LOCK_INACTIVE step=\(step) away_state=\(awayState.rawValue)")
            if awayState == .runningForeground { inactiveOnly.append(step) }
            if switcher {
                // Other apps in the switcher move the app's card off the middle, so the largest element
                // that names the app is the card. Its left part is on screen wherever the card sits.
                let screenArea = springboard.frame.width * springboard.frame.height
                let card = springboard.descendants(matching: .any).matching(cardPredicate).allElementsBoundByIndex
                    .max { $0.frame.width * $0.frame.height < $1.frame.width * $1.frame.height }
                guard let card, card.frame.width * card.frame.height > screenArea / 4 else {
                    let tree = XCTAttachment(string: springboard.debugDescription)
                    tree.name = "app-lock-inactive-\(step)-switcher"
                    tree.lifetime = .keepAlways
                    add(tree)
                    XCTFail("\(step): the app's card is not in the app switcher")
                    return
                }
                print("RISUNEST_APP_LOCK_INACTIVE step=\(step) card=\(card.identifier) frame=\(card.frame)")
                card.coordinate(withNormalizedOffset: CGVector(dx: 0.25, dy: 0.5)).tap()
            } else {
                springboard.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.995))
                    .press(forDuration: 0.1, thenDragTo: springboard.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.25)))
            }
            if step.hasSuffix("immediate") {
                for digit in ["1", "2", "3", "4", "5", "6"] {
                    let key = app.webViews.buttons[digit]
                    if !key.waitForExistence(timeout: 30) { attachScreenshot(app, name: "app-lock-inactive-\(step)-keypad-missing") }
                    XCTAssertTrue(key.exists, "PIN keypad did not appear after the immediate return")
                    key.tap()
                }
            }
            // The native cover hides the page from accessibility, so the button appears only once it is off.
            // Tapping it tells the fixture that this return is the real one.
            let back = app.webViews.buttons["app-lock-inactive-\(step)-back"]
            if back.waitForExistence(timeout: 60) { back.tap() }
            let returned = texts["app-lock-inactive-\(step)-returned"]
            let settled = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in returned.exists || failure.exists }, object: nil)
            // Longer than every wait in the fixture, so its own error shows first.
            if XCTWaiter().wait(for: [settled], timeout: 150) != .completed || failure.exists {
                attachScreenshot(app, name: "app-lock-inactive-\(step)-not-returned")
                if failure.exists {
                    print("RISUNEST_APP_LOCK_INACTIVE step=\(step) \(failure.label)")
                    let diagnostic = XCTAttachment(string: failure.label)
                    diagnostic.lifetime = .keepAlways
                    add(diagnostic)
                }
                XCTFail("\(step) return did not show the content")
                return
            }
            // The next step's PIN setup screen can hide the markers for a moment.
            let traceLine = texts.containing(NSPredicate(format: "label BEGINSWITH %@", "trace-\(step):")).firstMatch
            XCTAssertTrue(traceLine.waitForExistence(timeout: 30), "\(step) trace is missing")
            let trace = traceLine.label
            print("RISUNEST_APP_LOCK_INACTIVE step=\(step) \(trace)")
            let traceAttachment = XCTAttachment(string: trace)
            traceAttachment.name = "app-lock-inactive-\(step)-trace"
            traceAttachment.lifetime = .keepAlways
            add(traceAttachment)
        }
        print("RISUNEST_APP_LOCK_INACTIVE inactive_only=\(inactiveOnly.joined(separator: ","))")
        XCTAssertFalse(inactiveOnly.isEmpty, "No departure left the app in the foreground, so none was inactive only")
        XCTAssertEqual(app.webViews.textViews["Retained synthetic draft"].value as? String, "retained synthetic draft 123456")
        attachScreenshot(app, name: "app-lock-inactive-retained-draft")
    }

    func testProductKeyboard() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "app"
        app.launch()
        XCTAssertTrue(app.webViews.staticTexts.containing(NSPredicate(format: "label CONTAINS %@", "ios-synthetic-ui-edit")).firstMatch.waitForExistence(timeout: 90))
        let staleAlertButton = app.buttons["OK"]
        if staleAlertButton.waitForExistence(timeout: 2) {
            staleAlertButton.tap()
        }
        let input = app.webViews.textViews.firstMatch
        XCTAssertTrue(input.waitForExistence(timeout: 15))
        input.tap()
        input.typeText("synthetic draft")
        let typed = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            (input.value as? String)?.contains("synthetic draft") == true
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [typed], timeout: 5), .completed)
        attachScreenshot(app, name: "05-product-chat")
    }

    func testSidebarTouchGestures() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "sidebar-touch"
        app.launch()
        let web = app.webViews
        let reset = web.buttons["Reset sidebar"]
        XCTAssertTrue(reset.waitForExistence(timeout: 60))
        web.buttons["Bot 1"].tap()
        XCTAssertTrue(web.staticTexts["sidebar-clicks:1"].waitForExistence(timeout: 5))
        reset.tap()
        web.buttons["Bot 4"].press(forDuration: 0.05, thenDragTo: web.buttons["Bot 1"])
        XCTAssertTrue(web.staticTexts["sidebar-scrolled"].waitForExistence(timeout: 5))
        XCTAssertTrue(web.staticTexts["sidebar-drops:0"].exists)
        XCTAssertTrue(web.staticTexts["sidebar-clicks:0"].exists)
        reset.tap()
        web.buttons["Bot 3"].press(forDuration: 0.6, thenDragTo: web.buttons["Before Bot 1"])
        let events = web.staticTexts.containing(NSPredicate(format: "label BEGINSWITH %@", "sidebar-events:")).firstMatch
        if events.exists { print(events.label) }
        XCTAssertTrue(web.staticTexts["sidebar-drops:1"].waitForExistence(timeout: 5))
        XCTAssertTrue(web.staticTexts["sidebar-first:3"].exists)
        XCTAssertTrue(web.staticTexts["sidebar-clicks:0"].exists)
        reset.tap()
        web.buttons["Bot 1"].press(forDuration: 0.7)
        XCTAssertTrue(web.staticTexts["sidebar-menus:1"].waitForExistence(timeout: 5))
        XCTAssertTrue(web.staticTexts["sidebar-drops:0"].exists)
        XCTAssertTrue(web.staticTexts["sidebar-clicks:0"].exists)
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

        let importOption = webView.buttons.matching(
            NSPredicate(format: "label CONTAINS %@", "Import from a backup file")
        ).firstMatch
        XCTAssertTrue(importOption.waitForExistence(timeout: 15))
        importOption.tap()
        XCTAssertTrue(webView.staticTexts["Import a backup file"].waitForExistence(timeout: 15))
        attachScreenshot(app, name: "02-onboarding-import")

        let backToStart = webView.buttons["Back to start"]
        XCTAssertTrue(backToStart.waitForExistence(timeout: 15))
        backToStart.tap()
        XCTAssertTrue(home.waitForExistence(timeout: 15))

        let syncOption = webView.buttons.matching(
            NSPredicate(format: "label CONTAINS %@", "Connect to a sync server")
        ).firstMatch
        XCTAssertTrue(syncOption.waitForExistence(timeout: 15))
        syncOption.tap()
        XCTAssertTrue(webView.staticTexts["Choose how to sync."].waitForExistence(timeout: 15))
        attachScreenshot(app, name: "03-onboarding-sync")

        XCTAssertTrue(backToStart.waitForExistence(timeout: 15))
        backToStart.tap()
        XCTAssertTrue(home.waitForExistence(timeout: 15))
        let fresh = webView.buttons.matching(
            NSPredicate(format: "label CONTAINS %@", "Start right away")
        ).firstMatch
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
        let staleAlert = app.buttons["OK"]
        if staleAlert.waitForExistence(timeout: 3) { staleAlert.tap() }
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
        logSyntheticStep(app, step: 201, reached: false)
        let ready = app.webViews.staticTexts["ui-ready"].waitForExistence(timeout: 30)
        logSyntheticStep(app, step: 201, reached: ready)
        XCTAssertTrue(ready)

        logSyntheticStep(app, step: 202, reached: false)
        app.webViews.buttons["Import synthetic file"].tap()
        cancelPicker(app, step: 202)
        let imported = app.webViews.staticTexts["import-cancelled"].waitForExistence(timeout: 10)
        logSyntheticStep(app, step: 202, reached: imported)
        XCTAssertTrue(imported)

        logSyntheticStep(app, step: 203, reached: false)
        app.webViews.buttons["Export synthetic file"].tap()
        cancelPicker(app, step: 203)
        let exported = app.webViews.staticTexts["export-cancelled"].waitForExistence(timeout: 10)
        logSyntheticStep(app, step: 203, reached: exported)
        XCTAssertTrue(exported)

        logSyntheticStep(app, step: 204, reached: false)
        app.webViews.buttons["Allow notifications"].tap()
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let allow = springboard.alerts.buttons["Allow"]
        let prompted = allow.waitForExistence(timeout: 15)
        logSyntheticStep(app, step: 204, reached: prompted)
        XCTAssertTrue(prompted)
        allow.tap()
        logSyntheticStep(app, step: 205, reached: false)
        let permitted = app.webViews.staticTexts["notifications-allowed"].waitForExistence(timeout: 10)
        logSyntheticStep(app, step: 205, reached: permitted)
        XCTAssertTrue(permitted)
        logSyntheticStep(app, step: 206, reached: false)
        app.webViews.buttons["Open synthetic link"].tap()
        let safari = XCUIApplication(bundleIdentifier: "com.apple.mobilesafari")
        let opened = safari.wait(for: .runningForeground, timeout: 15)
        logSyntheticStep(app, step: 206, reached: opened)
        XCTAssertTrue(opened)
        app.activate()
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    func testNativeGeneralFileSelection() throws {
        continueAfterFailure = false
        let app = XCUIApplication(bundleIdentifier: "io.github.rsyumi.risunest.ios.bench")
        app.launchEnvironment["RISUNEST_IOS_PHASE"] = "general-files"
        app.launch()
        let single = app.webViews.buttons["General single synthetic file"]
        XCTAssertTrue(single.waitForExistence(timeout: 30))
        single.tap()
        openPublishedFile(app, name: "native-general.risup")
        XCTAssertTrue(app.webViews.staticTexts["general-single-exact"].waitForExistence(timeout: 30))
        app.webViews.buttons["General sliced synthetic file"].tap()
        openPublishedFile(app, name: "native-general.risup")
        XCTAssertTrue(app.webViews.staticTexts["general-slice-exact"].waitForExistence(timeout: 30))
        app.webViews.buttons["General multiple synthetic files"].tap()
        cancelPicker(app, step: 207)
        XCTAssertTrue(app.webViews.staticTexts["general-multiple-cancelled"].waitForExistence(timeout: 10))
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
