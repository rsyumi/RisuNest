import Tauri
import UIKit
import LocalAuthentication
import WebKit
import Darwin

private struct AppLockPolicyArgs: Decodable { let policy: AppLockPolicy }
private struct AppLockReadyArgs: Decodable { let epoch: UInt64; let locked: Bool }
private struct AppLockAuthenticationArgs: Decodable { let reason: String }

final class AppLockCoordinator {
    // A launch or preview cover has no lock decision behind it. Its frame confirmation runs through
    // async commands that can wait behind other native work, so the cover comes off after this long
    // even when no confirmation arrives. Normal confirmations take a few frames.
    private static let unconfirmedCoverLimit: TimeInterval = 3

    private weak var webView: WKWebView?
    private var state = AppLockState()
    private var observers: [NSObjectProtocol] = []
    private var covers: [UIView] = []
    private let accessibilityMask = AppLockAccessibilityMask<WKWebView>()
    private var authentication: LAContext?
    private var ownedPromptDepth = 0
    private var promptInactive = false
    private var readyGeneration: UInt64 = 0
    private var unconfirmedRelease: DispatchWorkItem?
    var hasOwnedFlow: () -> Bool = { false }

    private static func now() -> UInt64 {
        var info = mach_timebase_info_data_t()
        mach_timebase_info(&info)
        return UInt64(Double(mach_continuous_time()) * Double(info.numer) / Double(info.denom) / 1_000_000)
    }

    func load(_ webView: WKWebView) {
        self.webView = webView
        if observers.isEmpty {
            observe(UIApplication.willResignActiveNotification) { [weak self] in self?.willResignActive() }
            observe(UIApplication.didEnterBackgroundNotification) { [weak self] in self?.depart() }
            observe(UIApplication.willEnterForegroundNotification) { [weak self] in self?.renderCover() }
            observe(UIApplication.didBecomeActiveNotification) { [weak self] in self?.becameActive() }
            observe(UIWindow.didBecomeVisibleNotification) { [weak self] in self?.renderCover() }
            observe(Notification.Name("risunest-owned-prompt-begin")) { [weak self] in self?.beginOwnedPrompt() }
            observe(Notification.Name("risunest-owned-prompt-end")) { [weak self] in self?.endOwnedPrompt() }
        }
        renderCover()
        limitUnconfirmedCover()
    }

    private func limitUnconfirmedCover() {
        unconfirmedRelease?.cancel()
        guard state.coverRequired else { return }
        let epoch = state.epoch
        let release = DispatchWorkItem { [weak self] in
            guard let self = self, self.state.releaseUnconfirmed(epoch: epoch) else { return }
            NSLog("RisuNest app lock cover released after %.0f s without a confirmed frame", Self.unconfirmedCoverLimit)
            self.renderCover()
            self.emit()
        }
        unconfirmedRelease = release
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.unconfirmedCoverLimit, execute: release)
    }

    private func observe(_ name: Notification.Name, _ action: @escaping () -> Void) {
        observers.append(NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) { _ in action() })
    }

    deinit {
        observers.forEach(NotificationCenter.default.removeObserver)
        authentication?.invalidate()
        covers.forEach { $0.removeFromSuperview() }
        accessibilityMask.restore()
    }

    func beginOwnedPrompt() { ownedPromptDepth += 1 }
    func endOwnedPrompt() { ownedPromptDepth = max(0, ownedPromptDepth - 1) }

    private func willResignActive() {
        if promptInactive || authentication != nil || ownedPromptDepth > 0 || hasOwnedFlow() {
            promptInactive = true
            // The OS prompt owns the foreground. A subsequent background notification
            // still counts as leaving it and invalidates a pending authentication.
            if state.policy.hideRecentsPreview { renderCover(force: true) }
            return
        }
        depart()
    }

    private func depart() {
        guard state.depart(at: Self.now()) else { return }
        unconfirmedRelease?.cancel()
        promptInactive = false
        authentication?.invalidate()
        renderCover()
        emit()
    }

    private func becameActive() {
        if state.becomeActive() {
            promptInactive = false
            renderCover()
            emit()
            limitUnconfirmedCover()
        } else if promptInactive || state.coverRequired {
            // A later activation while the cover waits asks the page for its frame again.
            promptInactive = false
            renderCover()
            emit()
        }
    }

    private func windows() -> [UIWindow] {
        var windows = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .flatMap { $0.windows }
            .filter { !$0.isHidden }
        if let window = webView?.window, !windows.contains(where: { $0 === window }) { windows.append(window) }
        return windows
    }

    private func renderCover(force: Bool = false) {
        guard force || state.coverRequired || (promptInactive && state.policy.hideRecentsPreview) else {
            covers.forEach { $0.removeFromSuperview() }
            covers.removeAll()
            accessibilityMask.restore()
            return
        }
        if let webView = webView {
            accessibilityMask.hide(webView, isHidden: { $0.accessibilityElementsHidden },
                                   setHidden: { $0.accessibilityElementsHidden = $1 })
        }
        let color = Self.color(state.policy.backgroundColor)
        for window in windows() {
            if let cover = covers.first(where: { $0.superview === window }) {
                cover.backgroundColor = color
                window.bringSubviewToFront(cover)
                continue
            }
            let cover = UIView(frame: window.bounds)
            cover.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            cover.backgroundColor = color
            cover.isOpaque = true
            cover.isAccessibilityElement = true
            cover.accessibilityLabel = "RisuNest"
            cover.accessibilityViewIsModal = true
            let icon = UIImageView(image: Self.appIcon())
            icon.translatesAutoresizingMaskIntoConstraints = false
            icon.contentMode = .scaleAspectFit
            icon.layer.cornerRadius = 14
            icon.clipsToBounds = true
            cover.addSubview(icon)
            NSLayoutConstraint.activate([
                icon.widthAnchor.constraint(equalToConstant: 56),
                icon.heightAnchor.constraint(equalToConstant: 56),
                icon.centerXAnchor.constraint(equalTo: cover.centerXAnchor),
                icon.centerYAnchor.constraint(equalTo: cover.centerYAnchor)
            ])
            window.addSubview(cover)
            covers.append(cover)
        }
    }

    private static func appIcon() -> UIImage? {
        let icons = Bundle.main.object(forInfoDictionaryKey: "CFBundleIcons") as? [String: Any]
        let primary = icons?["CFBundlePrimaryIcon"] as? [String: Any]
        let names = primary?["CFBundleIconFiles"] as? [String]
        return names?.last.flatMap { UIImage(named: $0) } ?? UIImage(named: "AppIcon")
    }

    private static func color(_ value: String) -> UIColor {
        let hex = value.hasPrefix("#") ? String(value.dropFirst()) : value
        guard hex.count == 6, let rgb = UInt32(hex, radix: 16) else { return UIColor(red: 31 / 255, green: 32 / 255, blue: 41 / 255, alpha: 1) }
        return UIColor(red: CGFloat((rgb >> 16) & 255) / 255,
                       green: CGFloat((rgb >> 8) & 255) / 255,
                       blue: CGFloat(rgb & 255) / 255, alpha: 1)
    }

    private func emit() {
        guard let data = try? JSONSerialization.data(withJSONObject: state.snapshot(at: Self.now()).payload),
              let payload = String(data: data, encoding: .utf8) else { return }
        webView?.evaluateJavaScript("window.dispatchEvent(new CustomEvent('risunest-app-lock',{detail:\(payload)}))", completionHandler: nil)
    }

    func check(_ invoke: Invoke) {
        let context = LAContext()
        var error: NSError?
        let available = context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &error)
        var auth: [String: Any] = ["status": available ? "available" : "notEnrolled"]
        if available {
            switch context.biometryType {
            case .faceID: auth["method"] = "faceId"
            case .touchID: auth["method"] = "touchId"
            default: auth["method"] = "device"
            }
        }
        invoke.resolve(["auth": auth, "previewSupported": true])
    }

    func authenticate(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(AppLockAuthenticationArgs.self)
        guard !args.reason.isEmpty, authentication == nil, state.active,
              UIApplication.shared.applicationState == .active else {
            invoke.resolve(["status": "cancelled"])
            return
        }
        let context = LAContext()
        var error: NSError?
        guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &error) else {
            invoke.resolve(["status": "unavailable"])
            return
        }
        authentication = context
        let epoch = state.epoch
        context.evaluatePolicy(.deviceOwnerAuthentication, localizedReason: args.reason) { [weak self] success, error in
            DispatchQueue.main.async {
                guard let self = self else { invoke.resolve(["status": "cancelled"]); return }
                guard self.authentication === context else { invoke.resolve(["status": "cancelled"]); return }
                self.authentication = nil
                let status: String
                if self.state.epoch != epoch || !self.state.active {
                    status = "cancelled"
                } else if success {
                    status = "success"
                } else if let code = (error as? LAError)?.code,
                          [.userCancel, .systemCancel, .appCancel, .userFallback].contains(code) {
                    status = "cancelled"
                } else if let code = (error as? LAError)?.code,
                          [.passcodeNotSet, .biometryNotAvailable, .biometryNotEnrolled, .biometryLockout, .notInteractive].contains(code) {
                    status = "unavailable"
                } else {
                    status = "failed"
                }
                invoke.resolve(["status": status])
            }
        }
    }

    func setPolicy(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(AppLockPolicyArgs.self)
        if state.policy.gateRequired != args.policy.gateRequired { readyGeneration &+= 1 }
        state.policy = args.policy
        renderCover()
        invoke.resolve(state.snapshot(at: Self.now()).payload)
    }

    func ready(_ invoke: Invoke) throws {
        let args = try invoke.parseArgs(AppLockReadyArgs.self)
        readyGeneration &+= 1
        let generation = readyGeneration
        guard state.active, state.epoch == args.epoch, (args.locked || !state.policy.gateRequired), let webView = webView else {
            invoke.resolve(["accepted": false])
            return
        }
        let configuration = WKSnapshotConfiguration()
        configuration.afterScreenUpdates = true
        configuration.snapshotWidth = 1
        // WebKit incorporates pending content updates before completing this snapshot.
        // Discard the tiny image; only completion of the current frame releases the cover.
        webView.takeSnapshot(with: configuration) { [weak self, weak webView] image, error in
            DispatchQueue.main.async {
                guard let self = self, let webView = webView,
                      self.webView === webView, self.readyGeneration == generation,
                      image != nil, error == nil else {
                    invoke.resolve(["accepted": false])
                    return
                }
                let accepted = self.state.ready(epoch: args.epoch, locked: args.locked)
                if accepted { self.renderCover() }
                invoke.resolve(["accepted": accepted])
            }
        }
    }
}
