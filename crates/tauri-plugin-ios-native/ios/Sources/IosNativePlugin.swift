import Tauri
import UIKit
import UserNotifications
import UniformTypeIdentifiers
import WebKit

private struct EndArgs: Decodable { let id: String }
private struct PathArgs: Decodable { let path: String }
private struct ExportArgs: Decodable { let sourcePath: String; let suggestedName: String; let requestId: String }
private struct NotificationArgs: Decodable { let body: String }

final class IosNativePlugin: Plugin, UIDocumentPickerDelegate {
    private weak var webView: WKWebView?
    private var tasks: [String: UIBackgroundTaskIdentifier] = [:]
    private var expired = Set<String>()
    private var observers: [NSObjectProtocol] = []
    private var pickerCall: Invoke?
    private var exportCopy: URL?
    private var exporting = false
    private var publicationId: String?

    private var dataRoot: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent(Bundle.main.bundleIdentifier!, isDirectory: true)
    }
    private var staging: URL { dataRoot.appendingPathComponent("ios-file-staging", isDirectory: true) }

    override func load(webview: WKWebView) {
        webView = webview
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
                    "activeTasks": Array(self.tasks.keys),
                    "expiredTasks": Array(self.expired),
                    "foreground": UIApplication.shared.applicationState == .active,
                    "backgroundMode": "limited",
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
                self.expired.insert(id)
                self.emit("expired", id: id)
                UIApplication.shared.endBackgroundTask(task)
            }
            guard task != .invalid else {
                invoke.resolve(["id": NSNull(), "mode": "unavailable"])
                return
            }
            self.tasks[id] = task
            invoke.resolve(["id": id, "mode": "limited"])
        }
    }

    @objc func end(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(EndArgs.self)
        DispatchQueue.main.async {
            if let task = self.tasks.removeValue(forKey: args.id) {
                UIApplication.shared.endBackgroundTask(task)
            }
            self.expired.remove(args.id)
            invoke.resolve()
        }
    }

    @objc func request_notifications(_ invoke: Invoke) {
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

    @objc func open_settings(_ invoke: Invoke) {
        DispatchQueue.main.async {
            UIApplication.shared.open(URL(string: UIApplication.openSettingsURLString)!, options: [:]) { opened in
                invoke.resolve(["opened": opened])
            }
        }
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

    @objc func pick_file(_ invoke: Invoke) {
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

    @objc func export_file(_ invoke: Invoke) throws {
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

    @objc func discard_file(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(PathArgs.self)
        do {
            let file = try ownedFile(args.path, under: staging)
            try FileManager.default.removeItem(at: file)
            invoke.resolve()
        } catch { invoke.reject(error.localizedDescription) }
    }

    @objc func publication(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(EndArgs.self)
        guard UUID(uuidString: args.id) != nil else { invoke.reject("Invalid publication identifier"); return }
        let receipt = staging.appendingPathComponent("receipts/\(args.id).json")
        guard FileManager.default.fileExists(atPath: receipt.path) else { invoke.resolve(["state": "unknown"]); return }
        let result = try JSONSerialization.jsonObject(with: Data(contentsOf: receipt)) as! [String: Any]
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
