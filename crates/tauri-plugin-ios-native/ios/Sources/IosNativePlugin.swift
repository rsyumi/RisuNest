import Tauri
import AuthenticationServices
import UIKit
import UserNotifications
import UniformTypeIdentifiers
import WebKit
import BackgroundTasks

private struct EndArgs: Decodable { let id: String; let success: Bool? }
private struct ProgressArgs: Decodable { let id: String; let completed: Int64 }
private struct PathArgs: Decodable { let path: String }
private struct ExportArgs: Decodable { let sourcePath: String; let suggestedName: String; let requestId: String }
private struct NotificationArgs: Decodable { let body: String }
private struct WebAuthenticationArgs: Decodable {
    let authorizationUrl: String
    let callbackScheme: String
    let prefersEphemeral: Bool
}

final class IosNativePlugin: Plugin, UIDocumentPickerDelegate, ASWebAuthenticationPresentationContextProviding {
    private weak var webView: WKWebView?
    private var tasks: [String: UIBackgroundTaskIdentifier] = [:]
    private var expired = Set<String>()
    private var observers: [NSObjectProtocol] = []
    private var pickerCall: Invoke?
    private var exportCopy: URL?
    private var exporting = false
    private var publicationId: String?
    private var continued: [String: BGTask] = [:]
    private var continuedPending: String?
    private var progress: [String: Int64] = [:]
    private var continuedRegistered = false
    private var continuedErrorCode: Int?
    private var authenticationSession: ASWebAuthenticationSession?
    private var taskIdentifier: String { Bundle.main.bundleIdentifier! + ".generation" }

    private var dataRoot: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent(Bundle.main.bundleIdentifier!, isDirectory: true)
    }
    private var staging: URL { dataRoot.appendingPathComponent("ios-file-staging", isDirectory: true) }

    override func load(webview: WKWebView) {
        webView = webview
        #if compiler(>=6.2)
        if #available(iOS 26.0, *) {
            continuedRegistered = BGTaskScheduler.shared.register(forTaskWithIdentifier: taskIdentifier, using: .main) { [weak self] task in
                guard let self = self, let id = self.continuedPending,
                      let task = task as? BGContinuedProcessingTask else { task.setTaskCompleted(success: false); return }
                self.continuedPending = nil
                self.continued[id] = task
                task.progress.totalUnitCount = 3
                task.progress.completedUnitCount = self.progress[id] ?? 0
                task.expirationHandler = { [weak self] in
                    DispatchQueue.main.async {
                        guard let self = self, let current = self.continued.removeValue(forKey: id) else { return }
                        self.expired.insert(id)
                        if let assertion = self.tasks.removeValue(forKey: id) { UIApplication.shared.endBackgroundTask(assertion) }
                        self.emit("expired", id: id)
                        current.setTaskCompleted(success: false)
                    }
                }
            }
            if !continuedRegistered {
                NSLog("RisuNest continued processing registration unavailable")
            }
        }
        #endif
        for (name, event) in [
            (UIApplication.didEnterBackgroundNotification, "background"),
            (UIApplication.didBecomeActiveNotification, "active"),
            (UIApplication.didReceiveMemoryWarningNotification, "memory-warning")
        ] {
            observers.append(NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                guard let self = self else { return }
                if event == "background" {
                    let id = UUID().uuidString
                    let task = UIApplication.shared.beginBackgroundTask(withName: "RisuNest local save") { [weak self] in
                        guard let self = self, let task = self.tasks.removeValue(forKey: id) else { return }
                        UIApplication.shared.endBackgroundTask(task)
                    }
                    if task != .invalid { self.tasks[id] = task }
                    self.emit(event, id: task == .invalid ? "" : id)
                } else { self.emit(event) }
            })
        }
    }

    private func emit(_ event: String, id: String = "") {
        // Fixed native event names and UUIDs only. No document or conversation content.
        webView?.evaluateJavaScript("window.dispatchEvent(new CustomEvent('risunest-ios-lifecycle',{detail:{event:'\(event)',id:'\(id)'}}))", completionHandler: nil)
    }

    @objc func state(_ invoke: Invoke) {
        UNUserNotificationCenter.current().getNotificationSettings { settings in
            DispatchQueue.main.async {
                invoke.resolve([
                    "notifications": settings.authorizationStatus == .authorized || settings.authorizationStatus == .provisional,
                    "notificationStatus": settings.authorizationStatus.rawValue,
                    "activeTasks": Array(Set(self.tasks.keys).union(self.continued.keys)),
                    "expiredTasks": Array(self.expired),
                    "foreground": UIApplication.shared.applicationState == .active,
                    "backgroundMode": !self.continued.isEmpty || self.continuedPending != nil ? "continued" : "limited",
                    "continuedRegistered": self.continuedRegistered,
                    "continuedErrorCode": self.continuedErrorCode.map { $0 as Any } ?? NSNull(),
                    "backgroundRefreshStatus": UIApplication.shared.backgroundRefreshStatus.rawValue,
                ])
            }
        }
    }

    @objc func begin(_ invoke: Invoke) {
        DispatchQueue.main.async {
            let id = UUID().uuidString
            guard UIApplication.shared.applicationState == .active else {
                invoke.resolve(["id": NSNull(), "mode": "unavailable"])
                return
            }
            let task = UIApplication.shared.beginBackgroundTask(withName: "RisuNest generation") { [weak self] in
                guard let self = self, let task = self.tasks.removeValue(forKey: id) else { return }
                if self.continued[id] != nil {
                    UIApplication.shared.endBackgroundTask(task)
                    return
                }
                if self.continuedPending == id {
                    BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: self.taskIdentifier)
                    self.continuedPending = nil
                }
                self.expired.insert(id)
                self.emit("expired", id: id)
                UIApplication.shared.endBackgroundTask(task)
            }
            guard task != .invalid else {
                invoke.resolve(["id": NSNull(), "mode": "unavailable"])
                return
            }
            self.tasks[id] = task
            self.continuedErrorCode = nil
            var mode = "limited"
            #if compiler(>=6.2)
            if #available(iOS 26.0, *), self.continuedRegistered,
               self.continued.isEmpty, self.continuedPending == nil {
                let request = BGContinuedProcessingTaskRequest(identifier: self.taskIdentifier, title: "RisuNest", subtitle: "Generating a response")
                request.strategy = .fail
                self.continuedPending = id
                do {
                    try BGTaskScheduler.shared.submit(request)
                    mode = "continued"
                } catch {
                    self.continuedPending = nil
                    self.continuedErrorCode = (error as NSError).code
                    NSLog("RisuNest continued processing rejected: code=%ld refresh=%ld", (error as NSError).code, UIApplication.shared.backgroundRefreshStatus.rawValue)
                }
            }
            #endif
            invoke.resolve(["id": id, "mode": mode])
        }
    }

    @objc func end(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(EndArgs.self)
        DispatchQueue.main.async {
            if let task = self.tasks.removeValue(forKey: args.id) {
                UIApplication.shared.endBackgroundTask(task)
            }
            if let task = self.continued.removeValue(forKey: args.id) { task.setTaskCompleted(success: args.success == true) }
            if self.continuedPending == args.id {
                BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: self.taskIdentifier)
                self.continuedPending = nil
            }
            self.progress.removeValue(forKey: args.id)
            self.expired.remove(args.id)
            invoke.resolve()
        }
    }

    @objc func resetGeneration(_ invoke: Invoke) {
        DispatchQueue.main.async {
            for task in self.tasks.values { UIApplication.shared.endBackgroundTask(task) }
            for task in self.continued.values { task.setTaskCompleted(success: false) }
            if self.continuedPending != nil { BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: self.taskIdentifier) }
            self.tasks.removeAll()
            self.continued.removeAll()
            self.continuedPending = nil
            self.progress.removeAll()
            self.expired.removeAll()
            invoke.resolve()
        }
    }

    @objc func generationProgress(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(ProgressArgs.self)
        guard (0...3).contains(args.completed) else { invoke.reject("Invalid generation progress"); return }
        DispatchQueue.main.async {
            if self.tasks[args.id] != nil || self.continued[args.id] != nil {
                let completed = max(self.progress[args.id] ?? 0, args.completed)
                self.progress[args.id] = completed
                #if compiler(>=6.2)
                if #available(iOS 26.0, *), let task = self.continued[args.id] as? BGContinuedProcessingTask {
                    task.progress.completedUnitCount = completed
                }
                #endif
            }
            invoke.resolve()
        }
    }

    @objc func requestNotifications(_ invoke: Invoke) {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { granted, error in
            if let error = error { invoke.reject(error.localizedDescription) }
            else { invoke.resolve(["granted": granted]) }
        }
    }

    @objc func notify(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(NotificationArgs.self)
        guard args.body.utf8.count <= 1024 else { invoke.reject("Notification is too long"); return }
        let content = UNMutableNotificationContent()
        content.title = "RisuNest"
        content.body = args.body
        content.sound = .default
        let request = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request) { error in
            if let error = error { invoke.reject(error.localizedDescription) } else { invoke.resolve() }
        }
    }

    @objc func openSettings(_ invoke: Invoke) {
        DispatchQueue.main.async {
            UIApplication.shared.open(URL(string: UIApplication.openSettingsURLString)!, options: [:]) { opened in
                invoke.resolve(["opened": opened])
            }
        }
    }

    @objc func authenticate(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(WebAuthenticationArgs.self)
        guard let authorizationUrl = URL(string: args.authorizationUrl),
              authorizationUrl.scheme == "https",
              URL(string: "\(args.callbackScheme):/")?.scheme == args.callbackScheme else {
            invoke.resolve(["status": "failed"])
            return
        }
        DispatchQueue.main.async { [weak self] in
            guard let self = self else {
                invoke.resolve(["status": "failed"])
                return
            }
            guard self.authenticationSession == nil else {
                invoke.resolve(["status": "busy"])
                return
            }
            guard self.authenticationPresentationAnchor() != nil else {
                invoke.resolve(["status": "presentation-unavailable"])
                return
            }
            let session = ASWebAuthenticationSession(
                url: authorizationUrl,
                callbackURLScheme: args.callbackScheme
            ) { [weak self] callbackUrl, error in
                DispatchQueue.main.async {
                    self?.authenticationSession = nil
                    if let callbackUrl = callbackUrl {
                        invoke.resolve([
                            "status": "succeeded",
                            "callbackUrl": callbackUrl.absoluteString
                        ])
                    } else if let authenticationError = error as? ASWebAuthenticationSessionError,
                              authenticationError.code == .canceledLogin {
                        invoke.resolve(["status": "cancelled"])
                    } else {
                        invoke.resolve(["status": "failed"])
                    }
                }
            }
            session.presentationContextProvider = self
            session.prefersEphemeralWebBrowserSession = args.prefersEphemeral
            self.authenticationSession = session
            if !session.start() {
                self.authenticationSession = nil
                invoke.resolve(["status": "failed"])
            }
        }
    }

    @objc func cancelAuthentication(_ invoke: Invoke) {
        DispatchQueue.main.async { [weak self] in
            self?.authenticationSession?.cancel()
            invoke.resolve()
        }
    }

    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        authenticationPresentationAnchor() ?? ASPresentationAnchor()
    }

    private func authenticationPresentationAnchor() -> ASPresentationAnchor? {
        if let window = webView?.window { return window }
        return UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .first { $0.activationState == .foregroundActive }?
            .windows
            .first { $0.isKeyWindow }
    }

    private func present(_ picker: UIDocumentPickerViewController, invoke: Invoke) {
        guard pickerCall == nil else { invoke.reject("A file picker is already open"); return }
        guard let controller = webView?.window?.rootViewController else {
            invoke.reject("The app window is unavailable"); return
        }
        var presenter = controller
        while let child = presenter.presentedViewController { presenter = child }
        pickerCall = invoke
        picker.delegate = self
        picker.allowsMultipleSelection = false
        presenter.present(picker, animated: true)
    }

    @objc func pickFile(_ invoke: Invoke) {
        DispatchQueue.main.async {
            guard self.pickerCall == nil else { invoke.reject("A file picker is already open"); return }
            self.exporting = false
            self.present(UIDocumentPickerViewController(forOpeningContentTypes: [.data], asCopy: false), invoke: invoke)
        }
    }

    private func ownedFile(_ path: String, under root: URL) throws -> URL {
        let url = URL(fileURLWithPath: path).standardizedFileURL
        let resolved = url.resolvingSymlinksInPath()
        let base = root.resolvingSymlinksInPath().path + "/"
        guard resolved.path.hasPrefix(base),
              (try resolved.resourceValues(forKeys: [.isRegularFileKey])).isRegularFile == true else {
            throw NSError(domain: "RisuNest", code: 1, userInfo: [NSLocalizedDescriptionKey: "Expected an app-owned regular file"])
        }
        return resolved
    }

    @objc func exportFile(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(ExportArgs.self)
        DispatchQueue.main.async {
            guard self.pickerCall == nil else { invoke.reject("A file picker is already open"); return }
            do {
                guard !args.suggestedName.isEmpty, args.suggestedName != ".", args.suggestedName != "..",
                      !args.suggestedName.contains("/"), !args.suggestedName.contains("\\"),
                      args.suggestedName.utf8.count <= 240 else {
                    invoke.reject("Invalid export filename"); return
                }
                let source = try self.ownedFile(args.sourcePath, under: self.dataRoot)
                guard UUID(uuidString: args.requestId) != nil else { invoke.reject("Invalid publication identifier"); return }
                let receipt = self.staging.appendingPathComponent("receipts/\(args.requestId).json")
                guard !FileManager.default.fileExists(atPath: receipt.path) else { invoke.reject("Publication identifier already used"); return }
                try FileManager.default.createDirectory(at: receipt.deletingLastPathComponent(), withIntermediateDirectories: true)
                try JSONSerialization.data(withJSONObject: ["state": "pending"]).write(to: receipt, options: .atomic)
                self.publicationId = args.requestId
                // Reserve the picker before copying so overlapping requests cannot race.
                self.pickerCall = invoke
                DispatchQueue.global(qos: .userInitiated).async {
                    do {
                        let folder = self.staging.appendingPathComponent(UUID().uuidString, isDirectory: true)
                        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
                        let copy = folder.appendingPathComponent(args.suggestedName)
                        try FileManager.default.copyItem(at: source, to: copy)
                        DispatchQueue.main.async {
                            self.pickerCall = nil
                            self.exportCopy = copy
                            self.exporting = true
                            self.present(UIDocumentPickerViewController(forExporting: [copy], asCopy: true), invoke: invoke)
                        }
                    } catch {
                        DispatchQueue.main.async { self.pickerCall = nil; invoke.reject(error.localizedDescription) }
                    }
                }
            } catch { invoke.reject(error.localizedDescription) }
        }
    }

    @objc func discardFile(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(PathArgs.self)
        do {
            let file = try ownedFile(args.path, under: staging)
            guard UUID(uuidString: file.deletingLastPathComponent().lastPathComponent) != nil,
                  file.deletingLastPathComponent().deletingLastPathComponent() == staging.resolvingSymlinksInPath() else {
                invoke.reject("Expected a staged import or export file"); return
            }
            try FileManager.default.removeItem(at: file)
            invoke.resolve()
        } catch { invoke.reject(error.localizedDescription) }
    }

    @objc func publication(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(EndArgs.self)
        guard UUID(uuidString: args.id) != nil else { invoke.reject("Invalid publication identifier"); return }
        let receipt = staging.appendingPathComponent("receipts/\(args.id).json")
        guard FileManager.default.fileExists(atPath: receipt.path) else { invoke.resolve(["state": "unknown"]); return }
        guard let result = try JSONSerialization.jsonObject(with: Data(contentsOf: receipt)) as? [String: Any] else {
            invoke.reject("Invalid publication receipt"); return
        }
        invoke.resolve(result)
    }

    func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
        finishPicker(["cancelled": true])
    }

    func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
        guard let url = urls.first, let call = pickerCall else { return }
        if exporting {
            let size = (try? exportCopy?.resourceValues(forKeys: [.fileSizeKey]))?.fileSize ?? 0
            finishPicker(["cancelled": false, "bytes": size])
            return
        }
        DispatchQueue.global(qos: .userInitiated).async {
            let access = url.startAccessingSecurityScopedResource()
            defer { if access { url.stopAccessingSecurityScopedResource() } }
            let folder = self.staging.appendingPathComponent(UUID().uuidString, isDirectory: true)
            do {
                try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
                let destination = folder.appendingPathComponent(url.lastPathComponent)
                var coordinationError: NSError?
                var copyError: Error?
                NSFileCoordinator().coordinate(readingItemAt: url, options: [], error: &coordinationError) { source in
                    do { try FileManager.default.copyItem(at: source, to: destination) }
                    catch { copyError = error }
                }
                if let error = coordinationError { throw error }
                if let error = copyError { throw error }
                let values = try destination.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey])
                guard values.isRegularFile == true else { throw CocoaError(.fileReadUnsupportedScheme) }
                DispatchQueue.main.async {
                    self.finishPicker(["cancelled": false, "path": destination.path, "name": url.lastPathComponent, "bytes": values.fileSize ?? 0])
                }
            } catch {
                try? FileManager.default.removeItem(at: folder)
                DispatchQueue.main.async { self.pickerCall = nil; call.reject(error.localizedDescription) }
            }
        }
    }

    private func finishPicker(_ result: [String: Any]) {
        let call = pickerCall
        pickerCall = nil
        if exporting, let id = publicationId {
            let receipt = staging.appendingPathComponent("receipts/\(id).json")
            do {
                var terminal = result
                terminal["state"] = result["cancelled"] as? Bool == true ? "cancelled" : "succeeded"
                try JSONSerialization.data(withJSONObject: terminal).write(to: receipt, options: .atomic)
            } catch {
                // Keep both the native handoff and pending receipt for explicit recovery.
                call?.reject("File publication receipt could not be saved")
                return
            }
        }
        publicationId = nil
        if let copy = exportCopy { try? FileManager.default.removeItem(at: copy.deletingLastPathComponent()) }
        exportCopy = nil
        call?.resolve(result)
    }

    deinit {
        for observer in observers { NotificationCenter.default.removeObserver(observer) }
    }
}

@_cdecl("init_plugin_ios_native")
func initPlugin() -> Plugin { IosNativePlugin() }
