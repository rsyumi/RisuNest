import Tauri
import AuthenticationServices
import UIKit
import UserNotifications
import UniformTypeIdentifiers
import WebKit
import BackgroundTasks

private struct EndArgs: Decodable { let id: String; let success: Bool? }
private struct BeginArgs: Decodable { let kind: String?; let continued: Bool? }
private struct ProgressArgs: Decodable { let id: String; let completed: Int64; let total: Int64? }
private struct TaskProgress { let completed: Int64; let total: Int64 }
private struct OpenedArgs: Decodable { let urls: [String] }
private struct PathArgs: Decodable { let path: String }
private struct PickFilesArgs: Decodable { let multiple: Bool; let partial: Bool? }
private struct PortableSourceArgs: Decodable { let token: String; let jobId: String? }
private struct BackupSourceCopyArgs: Decodable { let token: String; let onProgress: Channel? }
private struct PortableSourceFormatArgs: Decodable { let token: String; let format: String }
private struct PortableSourceProbeArgs: Decodable { let token: String; let probeId: String }
private struct DataRootArgs: Decodable { let dataRoot: String }
private struct ExportArgs: Decodable { let sourcePath: String; let suggestedName: String; let requestId: String }
private struct RewritableArgs: Decodable { let suggestedName: String }
private struct RewriteArgs: Decodable { let text: String }
private struct NotificationArgs: Decodable { let body: String }
private struct WebAuthenticationArgs: Decodable {
    let authorizationUrl: String
    let callbackScheme: String
    let prefersEphemeral: Bool
}

final class IosNativePlugin: Plugin, UIDocumentPickerDelegate, ASWebAuthenticationPresentationContextProviding {
    private weak var webView: WKWebView?
    private let appLock = AppLockCoordinator()
    private var tasks: [String: UIBackgroundTaskIdentifier] = [:]
    private var expired = Set<String>()
    private var observers: [NSObjectProtocol] = []
    private let fileQueue = DispatchQueue(label: "io.github.rsyumi.risunest.files", qos: .userInitiated)
    private var stagingSweepStarted = false
    private var openedFiles: [[String: String]] = []
    private var pickerCall: Invoke?
    private var portablePicker = false
    private var fileBatchPicker = false
    /// Reports each file that cannot be staged instead of failing the whole pick.
    private var filePartialPicker = false
    private let portableSources = PortableSourceCustody.shared
    private var sourceOwner = UUID().uuidString
    private var sourceOwnerLoaded = false
    private var exportCopy: URL?
    private var exporting = false
    private var publicationId: String?
    private var rewritablePicker = false
    // The destination of the last move-mode export, which stays writable through its security scope.
    private var rewritableFile: URL?
    private var continued: [String: BGTask] = [:]
    private var continuedPending: String?
    // User actions that asked for continued processing, oldest first, with their subtitles.
    private var continuedCandidates: [(id: String, label: String)] = []
    private var progress: [String: TaskProgress] = [:]
    private var continuedRegistered = false
    private var continuedErrorCode: Int?
    private var authenticationSession: ASWebAuthenticationSession?
    private var taskIdentifier: String { Bundle.main.bundleIdentifier! + ".generation" }

    // Supplied by Rust, which owns the path manifest. File ownership is
    // enforced against this root, so deriving it here would check nothing.
    private var dataRoot: URL?
    private var staging: URL? { dataRoot?.appendingPathComponent("ios-file-staging", isDirectory: true) }

    override func load(webview: WKWebView) {
        if sourceOwnerLoaded && webView !== webview {
            portableSources.retireUnclaimed(owner: sourceOwner)
            sourceOwner = UUID().uuidString
        }
        sourceOwnerLoaded = true
        webView = webview
        appLock.hasOwnedFlow = { [weak self] in
            guard let self = self else { return false }
            return self.pickerCall != nil || self.authenticationSession != nil
        }
        appLock.load(webview)
        #if compiler(>=6.2)
        if #available(iOS 26.0, *) {
            continuedRegistered = BGTaskScheduler.shared.register(forTaskWithIdentifier: taskIdentifier, using: .main) { [weak self] task in
                guard let self = self, let id = self.continuedPending,
                      let task = task as? BGContinuedProcessingTask else { task.setTaskCompleted(success: false); return }
                self.continuedPending = nil
                self.continued[id] = task
                self.showContinued(task, for: id)
                task.expirationHandler = { [weak self, weak task] in
                    DispatchQueue.main.async {
                        // The task may have passed to a later user action since it launched.
                        guard let self = self, let task = task,
                              let id = self.continued.first(where: { $0.value === task })?.key,
                              let current = self.continued.removeValue(forKey: id) else { return }
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
                        // Work that waits on this assertion stops when it expires.
                        self.expired.insert(id)
                        self.emit("expired", id: id)
                        UIApplication.shared.endBackgroundTask(task)
                    }
                    if task != .invalid { self.tasks[id] = task }
                    self.emit(event, id: task == .invalid ? "" : id)
                } else { self.emit(event) }
            })
        }
    }

    private func emit(_ event: String, id: String = "") {
        guard let script = NativeFileState.lifecycleScript(event, id: id) else { return }
        webView?.evaluateJavaScript(script, completionHandler: nil)
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

    @objc func begin(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(BeginArgs.self)
        let kind = args.kind ?? "generation"
        let korean = Locale.preferredLanguages.first?.hasPrefix("ko") == true
        let labels = [
            "generation": korean ? "응답 생성 중" : "Generating a response",
            "backup": korean ? "백업 중" : "Backing up",
            "restore": korean ? "복원 중" : "Restoring",
            "sync": korean ? "동기화 중" : "Syncing",
            "import": korean ? "가져오는 중" : "Importing",
            "export": korean ? "내보내는 중" : "Exporting",
            "maintenance": korean ? "정리 중" : "Cleaning up",
        ]
        guard let label = labels[kind] else { invoke.reject("Invalid background task kind"); return }
        DispatchQueue.main.async {
            let id = UUID().uuidString
            guard UIApplication.shared.applicationState == .active else {
                invoke.resolve(["id": NSNull(), "mode": "unavailable"])
                return
            }
            let task = UIApplication.shared.beginBackgroundTask(withName: "RisuNest \(kind)") { [weak self] in
                guard let self = self, let task = self.tasks.removeValue(forKey: id) else { return }
                if self.continued[id] != nil {
                    UIApplication.shared.endBackgroundTask(task)
                    return
                }
                self.releaseContinued(id, success: false)
                self.expired.insert(id)
                self.emit("expired", id: id)
                UIApplication.shared.endBackgroundTask(task)
            }
            guard task != .invalid else {
                invoke.resolve(["id": NSNull(), "mode": "unavailable"])
                return
            }
            self.tasks[id] = task
            self.progress[id] = TaskProgress(completed: 0, total: kind == "generation" ? 3 : 0)
            self.continuedErrorCode = nil
            var mode = "limited"
            #if compiler(>=6.2)
            if #available(iOS 26.0, *), args.continued == true, self.continuedRegistered {
                self.continuedCandidates.append((id: id, label: label))
                if self.continued.isEmpty, self.continuedPending == nil {
                    let request = BGContinuedProcessingTaskRequest(identifier: self.taskIdentifier, title: "RisuNest", subtitle: label)
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
            self.releaseContinued(args.id, success: args.success == true)
            self.progress.removeValue(forKey: args.id)
            self.expired.remove(args.id)
            invoke.resolve()
        }
    }

    /// Passes continued processing to the next running user action that asked for it, or ends it.
    private func releaseContinued(_ id: String, success: Bool) {
        continuedCandidates.removeAll { $0.id == id }
        let next = continuedCandidates.first { tasks[$0.id] != nil && !expired.contains($0.id) }?.id
        if let task = continued.removeValue(forKey: id) {
            if let next = next {
                continued[next] = task
                showContinued(task, for: next)
            } else {
                task.setTaskCompleted(success: success)
            }
        }
        if continuedPending == id {
            if let next = next {
                continuedPending = next
            } else {
                BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: taskIdentifier)
                continuedPending = nil
            }
        }
    }

    /// Shows the owning action's subtitle and progress on the continued-processing task.
    private func showContinued(_ task: BGTask, for id: String) {
        #if compiler(>=6.2)
        if #available(iOS 26.0, *), let task = task as? BGContinuedProcessingTask {
            let progress = self.progress[id] ?? TaskProgress(completed: 0, total: 0)
            task.progress.totalUnitCount = progress.total
            task.progress.completedUnitCount = progress.completed
            if let label = continuedCandidates.first(where: { $0.id == id })?.label {
                task.updateTitle("RisuNest", subtitle: label)
            }
        }
        #endif
    }

    @objc func resetGeneration(_ invoke: Invoke) {
        DispatchQueue.main.async {
            for task in self.tasks.values { UIApplication.shared.endBackgroundTask(task) }
            for task in self.continued.values { task.setTaskCompleted(success: false) }
            if self.continuedPending != nil { BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: self.taskIdentifier) }
            self.tasks.removeAll()
            self.continued.removeAll()
            self.continuedPending = nil
            self.continuedCandidates.removeAll()
            self.progress.removeAll()
            self.expired.removeAll()
            invoke.resolve()
        }
    }

    @objc func generationProgress(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(ProgressArgs.self)
        let total = args.total ?? 3
        guard (0...100).contains(total), (0...total).contains(args.completed) else {
            invoke.reject("Invalid background progress"); return
        }
        DispatchQueue.main.async {
            if self.tasks[args.id] != nil || self.continued[args.id] != nil {
                let completed = args.total == nil ? max(self.progress[args.id]?.completed ?? 0, args.completed) : args.completed
                self.progress[args.id] = TaskProgress(completed: completed, total: total)
                #if compiler(>=6.2)
                if #available(iOS 26.0, *), let task = self.continued[args.id] as? BGContinuedProcessingTask {
                    task.progress.totalUnitCount = total
                    task.progress.completedUnitCount = completed
                }
                #endif
            }
            invoke.resolve()
        }
    }

    @objc func requestNotifications(_ invoke: Invoke) {
        DispatchQueue.main.async { [weak self] in
            self?.appLock.beginOwnedPrompt()
            UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { granted, error in
                DispatchQueue.main.async {
                    self?.appLock.endOwnedPrompt()
                    if let error = error { invoke.reject(error.localizedDescription) }
                    else { invoke.resolve(["granted": granted]) }
                }
            }
        }
    }

    @objc func appLockCheck(_ invoke: Invoke) {
        DispatchQueue.main.async { self.appLock.check(invoke) }
    }

    @objc func appLockAuthenticate(_ invoke: Invoke) {
        DispatchQueue.main.async {
            do { try self.appLock.authenticate(invoke) }
            catch { invoke.reject(error.localizedDescription) }
        }
    }

    @objc func appLockSetPolicy(_ invoke: Invoke) {
        DispatchQueue.main.async {
            do { try self.appLock.setPolicy(invoke) }
            catch { invoke.reject(error.localizedDescription) }
        }
    }

    @objc func appLockReady(_ invoke: Invoke) {
        DispatchQueue.main.async {
            do { try self.appLock.ready(invoke) }
            catch { invoke.reject(error.localizedDescription) }
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

    private func sweepStaging() {
        guard !stagingSweepStarted, let staging = staging else { return }
        stagingSweepStarted = true
        portableSources.sweepCopies(staging: staging)
        let manager = FileManager.default
        let receipts = staging.appendingPathComponent("receipts", isDirectory: true)
        var protected = Set<String>()
        let receiptFiles: [URL]
        if manager.fileExists(atPath: receipts.path) {
            guard let files = try? manager.contentsOfDirectory(at: receipts, includingPropertiesForKeys: nil) else { return }
            receiptFiles = files
        } else { receiptFiles = [] }
        for receipt in receiptFiles {
            guard let data = try? Data(contentsOf: receipt),
                  let value = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
            if value["state"] as? String == "pending" {
                guard let folder = value["folder"] as? String, UUID(uuidString: folder) != nil else { return }
                protected.insert(folder)
            }
        }
        let staleBefore = Date().addingTimeInterval(-60)
        for folder in (try? manager.contentsOfDirectory(
            at: staging,
            includingPropertiesForKeys: [.contentModificationDateKey]
        )) ?? [] {
            let modified = try? folder.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate
            if UUID(uuidString: folder.lastPathComponent) != nil
                && !protected.contains(folder.lastPathComponent)
                && modified.map({ $0 < staleBefore }) == true {
                try? manager.removeItem(at: folder)
            }
        }
    }

    private func stageFile(_ url: URL) throws -> [String: Any] {
        sweepStaging()
        guard let staging = staging else { throw rootUnavailable() }
        let access = url.startAccessingSecurityScopedResource()
        defer { if access { url.stopAccessingSecurityScopedResource() } }
        let folder = staging.appendingPathComponent(UUID().uuidString, isDirectory: true)
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
            let handle = try FileHandle(forWritingTo: destination)
            handle.synchronizeFile()
            handle.closeFile()
            return ["cancelled": false, "path": destination.path, "name": url.lastPathComponent,
                    "bytes": values.fileSize ?? 0, "mimeType": UTType(filenameExtension: url.pathExtension)?.preferredMIMEType ?? ""]
        } catch {
            if FileManager.default.fileExists(atPath: folder.path) {
                do { try FileManager.default.removeItem(at: folder) }
                catch let cleanupError {
                    throw NSError(domain: "RisuNest.FileSelection", code: 1, userInfo: [
                        NSLocalizedDescriptionKey: error.localizedDescription + "\n" + cleanupError.localizedDescription,
                        NSUnderlyingErrorKey: error
                    ])
                }
            }
            throw error
        }
    }

    @objc func receiveOpenedFiles(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(OpenedArgs.self)
        fileQueue.async {
            for text in args.urls {
                guard let url = URL(string: text), url.isFileURL,
                      ["risum", "risup", "charx", "risunest", "risudat"].contains(url.pathExtension.lowercased()) else { continue }
                let entry: [String: String]
                do {
                    let staged = try self.stageFile(url)
                    entry = ["path": staged["path"] as! String]
                    if let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first {
                        try? NativeFileState.removeStagedInboxSource(url, inbox: documents.appendingPathComponent("Inbox", isDirectory: true))
                    }
                } catch { entry = ["error": "The selected file could not be prepared for import"] }
                DispatchQueue.main.async {
                    self.openedFiles.append(entry)
                    self.webView?.evaluateJavaScript("window.dispatchEvent(new Event('risunest-ios-opened-files'))")
                }
            }
            invoke.resolve()
        }
    }

    @objc func takeOpenedFiles(_ invoke: Invoke) {
        DispatchQueue.main.async {
            let files = self.openedFiles
            self.openedFiles.removeAll()
            invoke.resolve(["files": files])
        }
    }

    private func failPreparation(_ invoke: Invoke, folder: URL?, error: Error) {
        if let folder = folder { try? FileManager.default.removeItem(at: folder) }
        if let id = publicationId, let staging = staging {
            try? FileManager.default.removeItem(at: staging.appendingPathComponent("receipts/\(id).json"))
        }
        pickerCall = nil
        publicationId = nil
        exportCopy = nil
        exporting = false
        rewritablePicker = false
        invoke.reject(error.localizedDescription)
    }

    private func present(_ picker: UIDocumentPickerViewController, invoke: Invoke, multiple: Bool = false, batch: Bool = false, partial: Bool = false) {
        guard pickerCall == nil else { invoke.reject("A file picker is already open"); return }
        guard let controller = webView?.window?.rootViewController else {
            failPreparation(invoke, folder: exportCopy?.deletingLastPathComponent(), error: CocoaError(.featureUnsupported)); return
        }
        var presenter = controller
        while let child = presenter.presentedViewController { presenter = child }
        pickerCall = invoke
        fileBatchPicker = batch
        filePartialPicker = partial
        picker.delegate = self
        picker.allowsMultipleSelection = multiple
        presenter.present(picker, animated: true)
    }

    @objc func pickFile(_ invoke: Invoke) {
        DispatchQueue.main.async {
            guard self.pickerCall == nil else { invoke.reject("A file picker is already open"); return }
            self.exporting = false
            self.rewritablePicker = false
            self.portablePicker = false
            self.present(UIDocumentPickerViewController(forOpeningContentTypes: [.data], asCopy: false), invoke: invoke)
        }
    }

    @objc func pickFiles(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(PickFilesArgs.self)
        DispatchQueue.main.async {
            guard self.pickerCall == nil else { invoke.reject("A file picker is already open"); return }
            self.exporting = false
            self.rewritablePicker = false
            self.portablePicker = false
            self.present(UIDocumentPickerViewController(forOpeningContentTypes: [.data], asCopy: false),
                         invoke: invoke, multiple: args.multiple, batch: true, partial: args.partial ?? false)
        }
    }

    @objc func pickBackupSource(_ invoke: Invoke) {
        DispatchQueue.main.async {
            guard self.pickerCall == nil else { invoke.reject("A file picker is already open"); return }
            self.exporting = false
            self.rewritablePicker = false
            self.portablePicker = true
            self.present(UIDocumentPickerViewController(forOpeningContentTypes: [.data], asCopy: false), invoke: invoke)
        }
    }

    @objc func portableSourceDescriptor(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(PortableSourceArgs.self)
        do { invoke.resolve(try portableSources.descriptor(token: args.token, jobId: args.jobId)) }
        catch { invoke.reject("Portable source is unavailable") }
    }

    @objc func releasePortableSource(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(PortableSourceArgs.self)
        // Signal a running copy before queueing the descriptor and scope cleanup.
        _ = portableSources.cancelCopy(token: args.token, jobId: args.jobId)
        if args.jobId == nil { invoke.resolve(portableSources.release(token: args.token, jobId: nil)) }
        else { fileQueue.async { invoke.resolve(self.portableSources.release(token: args.token, jobId: args.jobId)) } }
    }

    @objc func portableSourceOrphans(_ invoke: Invoke) {
        fileQueue.async {
            do { invoke.resolve(try self.portableSources.orphanReceipts().map { $0.receipt }) }
            catch { invoke.reject("Backup source cleanup failed", code: "cleanup-failed") }
        }
    }
    @objc func acknowledgePortableSourceOrphan(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(PortableSourceArgs.self)
        invoke.resolve(portableSources.acknowledgeOrphan(token: args.token))
    }
    @objc func beginPortableSourceProbe(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(PortableSourceProbeArgs.self)
        do { try portableSources.beginProbe(token: args.token, probeId: args.probeId); invoke.resolve(true) }
        catch { invoke.resolve(false) }
    }
    @objc func endPortableSourceProbe(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(PortableSourceProbeArgs.self)
        invoke.resolve(portableSources.endProbe(token: args.token, probeId: args.probeId))
    }

    @objc func discardBackupSource(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(PortableSourceArgs.self)
        _ = portableSources.cancelCopy(token: args.token, jobId: nil)
        invoke.resolve(portableSources.release(token: args.token, jobId: nil))
    }

    @objc func confirmPortableSourceFormat(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(PortableSourceFormatArgs.self)
        do { try portableSources.confirm(token: args.token, format: args.format); invoke.resolve() }
        catch { invoke.reject("Portable source ownership changed") }
    }

    @objc func materializeBackupSource(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(BackupSourceCopyArgs.self)
        guard let staging = staging else { invoke.reject(rootUnavailable().localizedDescription); return }
        fileQueue.async {
            do {
                invoke.resolve(try self.portableSources.materialize(token: args.token, staging: staging) { progress in
                    try? args.onProgress?.send(progress)
                })
            }
            catch {
                let code = (error as? PortableSourceError)?.copyFailureCode ?? "spool-write-failed"
                invoke.reject("Backup source could not be copied", code: code)
            }
        }
    }

    @objc func setDataRoot(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(DataRootArgs.self)
        let root = URL(fileURLWithPath: args.dataRoot, isDirectory: true).standardizedFileURL
        guard root.path.hasPrefix("/") else { invoke.reject("Expected an absolute data root"); return }
        DispatchQueue.main.async {
            self.dataRoot = root
            self.fileQueue.async { self.sweepStaging() }
            invoke.resolve()
        }
    }

    private func rootUnavailable() -> NSError {
        NSError(
            domain: "RisuNest",
            code: 2,
            userInfo: [NSLocalizedDescriptionKey: "The application data root has not been received yet"]
        )
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
                guard let root = self.dataRoot, let staging = self.staging else {
                    invoke.reject(self.rootUnavailable().localizedDescription); return
                }
                let source = try self.ownedFile(args.sourcePath, under: root)
                guard UUID(uuidString: args.requestId) != nil else { invoke.reject("Invalid publication identifier"); return }
                let receipt = staging.appendingPathComponent("receipts/\(args.requestId).json")
                guard !FileManager.default.fileExists(atPath: receipt.path) else { invoke.reject("Publication identifier already used"); return }
                try FileManager.default.createDirectory(at: receipt.deletingLastPathComponent(), withIntermediateDirectories: true)
                let folder = staging.appendingPathComponent(UUID().uuidString, isDirectory: true)
                try JSONSerialization.data(withJSONObject: ["state": "pending", "folder": folder.lastPathComponent]).write(to: receipt, options: .atomic)
                self.publicationId = args.requestId
                // Reserve the picker before copying so overlapping requests cannot race.
                self.pickerCall = invoke
                self.fileQueue.async {
                    self.sweepStaging()
                    do {
                        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
                        let copy = folder.appendingPathComponent(args.suggestedName)
                        try FileManager.default.copyItem(at: source, to: copy)
                        DispatchQueue.main.async {
                            self.pickerCall = nil
                            self.exportCopy = copy
                            self.exporting = true
                            self.rewritablePicker = false
                            self.present(UIDocumentPickerViewController(forExporting: [copy], asCopy: true), invoke: invoke)
                        }
                    } catch {
                        DispatchQueue.main.async { self.failPreparation(invoke, folder: folder, error: error) }
                    }
                }
            } catch { invoke.reject(error.localizedDescription) }
        }
    }

    @objc func pickRewritableFile(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(RewritableArgs.self)
        DispatchQueue.main.async {
            guard self.pickerCall == nil else { invoke.reject("A file picker is already open"); return }
            guard !args.suggestedName.isEmpty, args.suggestedName != ".", args.suggestedName != "..",
                  !args.suggestedName.contains("/"), !args.suggestedName.contains("\\"),
                  args.suggestedName.utf8.count <= 240 else {
                invoke.reject("Invalid export filename"); return
            }
            guard let staging = self.staging else { invoke.reject(self.rootUnavailable().localizedDescription); return }
            let folder = staging.appendingPathComponent(UUID().uuidString, isDirectory: true)
            do {
                try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
                let placeholder = folder.appendingPathComponent(args.suggestedName)
                try Data().write(to: placeholder)
                self.exportCopy = placeholder
                self.exporting = false
                self.portablePicker = false
                self.rewritablePicker = true
                // Moving the placeholder, unlike exporting a copy, returns a security-scoped destination the app can write again.
                self.present(UIDocumentPickerViewController(forExporting: [placeholder], asCopy: false), invoke: invoke)
            } catch {
                try? FileManager.default.removeItem(at: folder)
                invoke.reject(error.localizedDescription)
            }
        }
    }

    @objc func rewriteFile(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(RewriteArgs.self)
        DispatchQueue.main.async {
            guard let url = self.rewritableFile else { invoke.reject("No rewritable file was picked"); return }
            self.fileQueue.async {
                let access = url.startAccessingSecurityScopedResource()
                defer { if access { url.stopAccessingSecurityScopedResource() } }
                var coordinationError: NSError?
                var writeError: Error?
                NSFileCoordinator().coordinate(writingItemAt: url, options: .forReplacing, error: &coordinationError) { target in
                    do { try Data(args.text.utf8).write(to: target) }
                    catch { writeError = error }
                }
                if let error = coordinationError { invoke.reject(error.localizedDescription); return }
                if let error = writeError { invoke.reject(error.localizedDescription); return }
                invoke.resolve()
            }
        }
    }

    @objc func discardFile(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(PathArgs.self)
        do {
            guard let staging = staging else { invoke.reject(rootUnavailable().localizedDescription); return }
            let file = try ownedFile(args.path, under: staging)
            try NativeFileState.discardStagedFile(file, staging: staging)
            invoke.resolve()
        } catch { invoke.reject(error.localizedDescription) }
    }

    @objc func publication(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(EndArgs.self)
        guard UUID(uuidString: args.id) != nil else { invoke.reject("Invalid publication identifier"); return }
        guard let staging = staging else { invoke.reject(rootUnavailable().localizedDescription); return }
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
        guard let call = pickerCall else { return }
        guard let url = urls.first else {
            pickerCall = nil
            call.reject("The file picker returned no document")
            return
        }
        if rewritablePicker {
            rewritableFile = url
            finishPicker(["cancelled": false])
            return
        }
        if exporting {
            let size = (try? exportCopy?.resourceValues(forKeys: [.fileSizeKey]))?.fileSize ?? 0
            finishPicker(["cancelled": false, "bytes": size])
            return
        }
        if portablePicker {
            portableSources.select(url, owner: sourceOwner) { result in
                DispatchQueue.main.async {
                    switch result {
                    case .success(let value): self.finishPicker(value)
                    case .failure: self.pickerCall = nil; call.reject("Portable source must be a seekable file")
                    }
                }
            }
            return
        }
        let batch = fileBatchPicker
        let partial = filePartialPicker
        fileQueue.async {
            var staged: [[String: Any]] = []
            var failures: [[String: Any]] = []
            do {
                for source in batch ? urls : [url] {
                    do { staged.append(try self.stageFile(source)) }
                    catch let error where partial {
                        failures.append(["name": source.lastPathComponent, "message": error.localizedDescription])
                    }
                }
                let result: [String: Any] = batch ? ["cancelled": false, "files": staged, "failures": failures] : staged[0]
                DispatchQueue.main.async { self.finishPicker(result) }
            } catch {
                var message = error.localizedDescription
                for file in staged {
                    if let path = file["path"] as? String {
                        do { try FileManager.default.removeItem(at: URL(fileURLWithPath: path).deletingLastPathComponent()) }
                        catch { message += "\n" + error.localizedDescription }
                    }
                }
                let failure = message
                DispatchQueue.main.async { self.pickerCall = nil; call.reject(failure) }
            }
        }
    }

    @objc func acknowledgePublication(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(EndArgs.self)
        guard UUID(uuidString: args.id) != nil else { invoke.reject("Invalid publication identifier"); return }
        guard let staging = staging else { invoke.reject(rootUnavailable().localizedDescription); return }
        try NativeFileState(staging: staging).acknowledge(args.id)
        invoke.resolve()
    }

    private func finishPicker(_ result: [String: Any]) {
        let call = pickerCall
        pickerCall = nil
        if exporting, let id = publicationId, let staging = staging {
            do {
                try NativeFileState(staging: staging).complete(id, result: result)
            } catch {
                // Keep both the native handoff and pending receipt for explicit recovery.
                publicationId = nil
                exportCopy = nil
                exporting = false
                call?.reject("File publication receipt could not be saved")
                return
            }
        }
        publicationId = nil
        if let copy = exportCopy { try? FileManager.default.removeItem(at: copy.deletingLastPathComponent()) }
        exportCopy = nil
        exporting = false
        rewritablePicker = false
        call?.resolve(result)
    }

    deinit {
        portableSources.retireUnclaimed(owner: sourceOwner)
        for observer in observers { NotificationCenter.default.removeObserver(observer) }
    }
}

@_cdecl("init_plugin_ios_native")
public func initPlugin() -> Plugin { IosNativePlugin() }
