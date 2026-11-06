import Foundation

struct AppLockPolicy: Codable {
    var enabled = false
    var timing = "launch"
    var gateRequired = false
    var hideRecentsPreview = false
    var backgroundColor = "#1f2029"

    var installsReturnCover: Bool { enabled && timing != "launch" }
}

struct AppLockSnapshot: Codable {
    let active: Bool
    let epoch: UInt64
    let atMs: UInt64
    let departedAtMs: UInt64?

    var payload: [String: Any] {
        ["active": active, "epoch": epoch, "atMs": atMs,
         "departedAtMs": departedAtMs.map { $0 as Any } ?? NSNull()]
    }
}

struct AppLockState {
    var policy = AppLockPolicy()
    private(set) var active = true
    private(set) var epoch: UInt64 = 0
    private(set) var departedAtMs: UInt64?
    private(set) var awaitingReady = true

    var coverRequired: Bool {
        awaitingReady || (!active && policy.hideRecentsPreview)
    }

    func snapshot(at now: UInt64) -> AppLockSnapshot {
        AppLockSnapshot(active: active, epoch: epoch, atMs: now, departedAtMs: departedAtMs)
    }

    @discardableResult mutating func depart(at now: UInt64) -> Bool {
        guard active else { return false }
        active = false
        epoch &+= 1
        departedAtMs = now
        if policy.installsReturnCover { awaitingReady = true }
        return true
    }

    @discardableResult mutating func becomeActive() -> Bool {
        guard !active else { return false }
        active = true
        epoch &+= 1
        if policy.hideRecentsPreview { awaitingReady = true }
        return true
    }

    mutating func ready(epoch renderedEpoch: UInt64, locked: Bool) -> Bool {
        guard active && epoch == renderedEpoch && (locked || !policy.gateRequired) else { return false }
        policy.gateRequired = locked
        awaitingReady = false
        return true
    }

    /// Ends a launch or preview cover whose frame was never confirmed. A cover that waits for a lock
    /// decision or the PIN screen stays until its frame is ready.
    mutating func releaseUnconfirmed(epoch coveredEpoch: UInt64) -> Bool {
        guard active, epoch == coveredEpoch, awaitingReady,
              !policy.installsReturnCover, !policy.gateRequired else { return false }
        awaitingReady = false
        return true
    }
}

final class AppLockAccessibilityMask<Content: AnyObject> {
    private weak var content: Content?
    private var restoreContent: ((Content) -> Void)?

    func hide(_ content: Content, isHidden: (Content) -> Bool, setHidden: @escaping (Content, Bool) -> Void) {
        guard self.content !== content else { return }
        restore()
        let previous = isHidden(content)
        self.content = content
        restoreContent = { setHidden($0, previous) }
        setHidden(content, true)
    }

    func restore() {
        if let content = content { restoreContent?(content) }
        content = nil
        restoreContent = nil
    }

    deinit { restore() }
}
