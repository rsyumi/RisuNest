use super::{AuthAvailability, AuthMethod, AuthOutcome, AuthStatus, Capabilities, CoverPolicy, LifecycleSnapshot, EVENT_NAME};
use block2::RcBlock;
use objc2::{class, msg_send, rc::Retained, runtime::{AnyObject, Bool, ProtocolObject}, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSApplication, NSApplicationDidBecomeActiveNotification, NSApplicationWillBecomeActiveNotification,
    NSApplicationWillHideNotification, NSApplicationWillResignActiveNotification,
    NSAutoresizingMaskOptions, NSBorderType, NSBox, NSBoxType, NSColor, NSImageScaling, NSImageView,
    NSTitlePosition, NSRunningApplication, NSWindow, NSWindowDidDeminiaturizeNotification, NSWindowDidBecomeKeyNotification,
    NSWindowWillMiniaturizeNotification, NSWindowOrderingMode, NSWorkspace,
    NSWorkspaceDidActivateApplicationNotification, NSWorkspaceApplicationKey, NSWorkspaceSessionDidResignActiveNotification,
    NSWorkspaceWillSleepNotification, NSWorkspaceDidWakeNotification, NSWorkspaceSessionDidBecomeActiveNotification,
};
use objc2_foundation::{NSError, NSNotification, NSNotificationCenter, NSNotificationName, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use std::{cell::RefCell, collections::HashMap, ptr::NonNull};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use tokio::sync::oneshot;

#[link(name = "LocalAuthentication", kind = "framework")]
extern "C" {}

#[repr(C)]
struct Timebase { numer: u32, denom: u32 }
extern "C" {
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}

fn now_ms() -> u64 {
    let mut timebase = Timebase { numer: 1, denom: 1 };
    unsafe {
        mach_timebase_info(&mut timebase);
        (u128::from(mach_continuous_time()) * u128::from(timebase.numer)
            / u128::from(timebase.denom.max(1)) / 1_000_000) as u64
    }
}

type Observer = (Retained<NSNotificationCenter>, Retained<ProtocolObject<dyn NSObjectProtocol>>);

// The launch cover has no lock decision behind it. Its frame confirmation runs through async
// commands that can wait behind other native work, so it comes off after this long even when no
// confirmation arrives. Normal confirmations take a few frames.
const UNCONFIRMED_COVER_LIMIT: std::time::Duration = std::time::Duration::from_secs(3);

struct State {
    app: AppHandle,
    policy: CoverPolicy,
    active: bool,
    epoch: u64,
    departed_at_ms: Option<u64>,
    awaiting_ready: bool,
    prompt_inactive: bool,
    ready_generation: u64,
    authentication: Option<Retained<AnyObject>>,
    observers: Vec<Observer>,
    covers: HashMap<String, Retained<NSBox>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

impl State {
    fn snapshot(&self) -> LifecycleSnapshot {
        LifecycleSnapshot { active: self.active, epoch: self.epoch, at_ms: now_ms(), departed_at_ms: self.departed_at_ms }
    }

    fn emit(&self) { let _ = self.app.emit(EVENT_NAME, self.snapshot()); }

    fn depart(&mut self, mtm: MainThreadMarker) {
        if !self.active { return; }
        self.active = false;
        self.prompt_inactive = false;
        self.epoch = self.epoch.wrapping_add(1);
        self.departed_at_ms = Some(now_ms());
        if self.policy.installs_return_cover() { self.awaiting_ready = true; }
        if let Some(context) = &self.authentication { unsafe { let _: () = msg_send![&**context, invalidate]; } }
        self.render_covers(mtm, false);
        self.emit();
    }

    fn become_active(&mut self, mtm: MainThreadMarker) {
        if !self.active {
            self.active = true;
            self.epoch = self.epoch.wrapping_add(1);
            self.render_covers(mtm, false);
            self.emit();
            self.limit_unconfirmed_cover();
        } else if self.prompt_inactive {
            self.prompt_inactive = false;
            self.render_covers(mtm, false);
            self.emit();
        }
    }

    /// Ends a cover whose frame was never confirmed, unless it waits for a lock decision or the PIN screen.
    fn release_unconfirmed(&mut self, mtm: MainThreadMarker, epoch: u64) -> bool {
        if !self.awaiting_ready || !self.active || self.epoch != epoch
            || self.policy.installs_return_cover() || self.policy.gate_required { return false; }
        self.awaiting_ready = false;
        self.render_covers(mtm, false);
        self.emit();
        true
    }

    fn limit_unconfirmed_cover(&self) {
        if !self.awaiting_ready || self.policy.installs_return_cover() || self.policy.gate_required { return; }
        let app = self.app.clone();
        let epoch = self.epoch;
        std::thread::spawn(move || {
            std::thread::sleep(UNCONFIRMED_COVER_LIMIT);
            let _ = app.run_on_main_thread(move || {
                let Some(mtm) = MainThreadMarker::new() else { return };
                let released = STATE.with(|cell| cell.borrow_mut().as_mut().is_some_and(|state| state.release_unconfirmed(mtm, epoch)));
                if released {
                    crate::nlog!("warn", "app-lock cover released after {} s without a confirmed frame", UNCONFIRMED_COVER_LIMIT.as_secs());
                }
            });
        });
    }

    fn render_covers(&mut self, mtm: MainThreadMarker, force: bool) {
        let cover_all = force || self.awaiting_ready || self.prompt_inactive;
        let app = NSApplication::sharedApplication(mtm);
        let image = app.applicationIconImage();
        let color = cover_color(&self.policy.background_color);
        let windows = self.app.webview_windows();
        self.covers.retain(|label, cover| {
            if windows.contains_key(label) && (cover_all || (self.policy.gate_required && label != "main")) { true }
            else { cover.removeFromSuperview(); false }
        });
        for (label, window) in windows {
            if !cover_all && !(self.policy.gate_required && label != "main") { continue; }
            let Ok(pointer) = window.ns_window() else { continue };
            let native = unsafe { &*pointer.cast::<NSWindow>() };
            let Some(root) = native.contentView() else { continue };
            let cover = self.covers.entry(label).or_insert_with(|| {
                let cover = NSBox::initWithFrame(NSBox::alloc(mtm), root.bounds());
                cover.setBoxType(NSBoxType::Custom);
                cover.setTitlePosition(NSTitlePosition::NoTitle);
                cover.setBorderType(NSBorderType::NoBorder);
                cover.setBorderWidth(0.0);
                cover.setCornerRadius(0.0);
                cover.setContentViewMargins(NSSize::new(0.0, 0.0));
                cover.setTransparent(false);
                cover.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
                let bounds = root.bounds();
                let icon = NSImageView::initWithFrame(NSImageView::alloc(mtm), NSRect::new(
                    NSPoint::new((bounds.size.width - 56.0) / 2.0, (bounds.size.height - 56.0) / 2.0), NSSize::new(56.0, 56.0)));
                icon.setImage(image.as_deref());
                icon.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
                icon.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMaxXMargin
                    | NSAutoresizingMaskOptions::ViewMinYMargin | NSAutoresizingMaskOptions::ViewMaxYMargin);
                icon.setWantsLayer(true);
                unsafe {
                    let layer: *mut AnyObject = msg_send![&*icon, layer];
                    if !layer.is_null() {
                        let _: () = msg_send![layer, setCornerRadius: 14.0f64];
                        let _: () = msg_send![layer, setMasksToBounds: Bool::YES];
                    }
                }
                cover.addSubview(&icon);
                cover
            });
            cover.setFillColor(&color);
            cover.setFrame(root.bounds());
            root.addSubview_positioned_relativeTo(cover, NSWindowOrderingMode::Above, None);
            cover.displayIfNeeded();
        }
    }
}

fn cover_color(value: &str) -> Retained<NSColor> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    let rgb = if hex.len() == 6 { u32::from_str_radix(hex, 16).unwrap_or(0x1f2029) } else { 0x1f2029 };
    NSColor::colorWithSRGBRed_green_blue_alpha(
        ((rgb >> 16) & 255) as f64 / 255.0, ((rgb >> 8) & 255) as f64 / 255.0, (rgb & 255) as f64 / 255.0, 1.0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal { Resign, WillActivate, Active, Hide, Minimize, WindowActive, WorkspaceActive, SessionAway }

fn workspace_notifications() -> [(&'static NSNotificationName, Signal); 5] {
    unsafe { [
        (NSWorkspaceDidActivateApplicationNotification, Signal::WorkspaceActive),
        (NSWorkspaceSessionDidResignActiveNotification, Signal::SessionAway),
        (NSWorkspaceWillSleepNotification, Signal::SessionAway),
        (NSWorkspaceDidWakeNotification, Signal::WindowActive),
        (NSWorkspaceSessionDidBecomeActiveNotification, Signal::WindowActive),
    ] }
}

fn handle_signal(signal: Signal, notification: &NSNotification) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(state) = slot.as_mut() else { return };
        match signal {
            Signal::Resign => {
                if state.authentication.is_some() {
                    state.prompt_inactive = state.policy.installs_return_cover();
                    state.render_covers(mtm, false);
                } else {
                    state.depart(mtm);
                }
            }
            Signal::WillActivate => state.render_covers(mtm, false),
            Signal::Active => state.become_active(mtm),
            Signal::Hide | Signal::SessionAway => state.depart(mtm),
            Signal::Minimize => {
                let minimizing = notification.object();
                let other_visible = state.app.webview_windows().values().any(|window| {
                    let Ok(pointer) = window.ns_window() else { return false };
                    if minimizing.as_ref().is_some_and(|object| Retained::as_ptr(object).cast::<std::ffi::c_void>() == pointer.cast_const()) { return false; }
                    let native = unsafe { &*pointer.cast::<NSWindow>() };
                    native.isVisible() && !native.isMiniaturized()
                });
                if !other_visible { state.depart(mtm); }
            }
            Signal::WindowActive => {
                state.render_covers(mtm, false);
                if NSApplication::sharedApplication(mtm).isActive() { state.become_active(mtm); }
            }
            Signal::WorkspaceActive => {
                let activated = notification.userInfo()
                    .and_then(|info| info.objectForKey(unsafe { NSWorkspaceApplicationKey }))
                    .and_then(|object| object.downcast::<NSRunningApplication>().ok());
                let Some(frontmost) = activated.or_else(|| NSWorkspace::sharedWorkspace().frontmostApplication()) else { return };
                if frontmost.processIdentifier() as u32 == std::process::id() {
                    state.become_active(mtm);
                    return;
                }
                let identifier = frontmost.bundleIdentifier().map(|name| name.to_string()).unwrap_or_default();
                // LocalAuthentication may temporarily activate its OS-owned credential UI.
                // Other applications still count as departure while that request is pending.
                if state.authentication.is_some() && matches!(identifier.as_str(),
                    "com.apple.SecurityAgent" | "com.apple.LocalAuthentication.UIAgent" | "com.apple.localAuthenticationRemoteService") {
                    return;
                }
                state.depart(mtm);
            }
        }
    });
}

fn observe(center: &NSNotificationCenter, name: &NSNotificationName, signal: Signal) -> Observer {
    let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
        handle_signal(signal, unsafe { notification.as_ref() });
    });
    let token = unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block) };
    (center.retain(), token)
}

pub(super) fn setup(app: &AppHandle) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("App-lock setup needs the main thread")?;
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_some() { return; }
        let mut state = State {
            app: app.clone(), policy: CoverPolicy { background_color: "#1f2029".into(), ..Default::default() },
            active: NSApplication::sharedApplication(mtm).isActive(), epoch: 0, departed_at_ms: None,
            awaiting_ready: true, prompt_inactive: false, ready_generation: 0, authentication: None, observers: Vec::new(), covers: HashMap::new(),
        };
        let center = NSNotificationCenter::defaultCenter();
        let workspace = NSWorkspace::sharedWorkspace().notificationCenter();
        unsafe {
            for (name, signal) in [
                (NSApplicationWillResignActiveNotification, Signal::Resign),
                (NSApplicationWillBecomeActiveNotification, Signal::WillActivate),
                (NSApplicationDidBecomeActiveNotification, Signal::Active),
                (NSApplicationWillHideNotification, Signal::Hide),
                (NSWindowWillMiniaturizeNotification, Signal::Minimize),
                (NSWindowDidBecomeKeyNotification, Signal::WindowActive),
                (NSWindowDidDeminiaturizeNotification, Signal::WindowActive),
            ] { state.observers.push(observe(&center, name, signal)); }
            for (name, signal) in workspace_notifications() { state.observers.push(observe(&workspace, name, signal)); }
        }
        state.render_covers(mtm, false);
        state.limit_unconfirmed_cover();
        *slot = Some(state);
    });
    Ok(())
}

async fn on_main<T: Send + 'static>(app: &AppHandle, operation: impl FnOnce(&mut State, MainThreadMarker) -> T + Send + 'static) -> Result<T, String> {
    let (sender, receiver) = oneshot::channel();
    app.run_on_main_thread(move || {
        let result = STATE.with(|cell| {
            let mut slot = cell.borrow_mut();
            let state = slot.as_mut().ok_or_else(|| "App lock is not initialized".to_string())?;
            let mtm = MainThreadMarker::new().ok_or_else(|| "App lock is not on the main thread".to_string())?;
            Ok(operation(state, mtm))
        });
        let _ = sender.send(result);
    }).map_err(|error| error.to_string())?;
    receiver.await.map_err(|_| "App-lock main-thread request ended".to_string())?
}

fn context_available(context: &AnyObject) -> bool {
    let mut error: *mut NSError = std::ptr::null_mut();
    let available: Bool = unsafe { msg_send![context, canEvaluatePolicy: 2isize, error: &mut error] };
    available.as_bool()
}

pub(super) async fn check(app: &AppHandle) -> Result<Capabilities, String> {
    on_main(app, |_, _| {
        let context: Retained<AnyObject> = unsafe { msg_send![class!(LAContext), new] };
        let available = context_available(&context);
        let method = if available {
            let biometry: isize = unsafe { msg_send![&*context, biometryType] };
            Some(if biometry == 1 { AuthMethod::TouchId } else { AuthMethod::Device })
        } else { None };
        Capabilities { auth: AuthAvailability { status: if available { AuthStatus::Available } else { AuthStatus::NotEnrolled }, method }, preview_supported: false }
    }).await
}

fn auth_outcome(success: bool, code: Option<isize>) -> AuthOutcome {
    if success { return AuthOutcome::Success; }
    match code {
        Some(-2 | -3 | -4 | -9) => AuthOutcome::Cancelled,
        Some(-5 | -6 | -7 | -8 | -1004) => AuthOutcome::Unavailable,
        _ => AuthOutcome::Failed,
    }
}

pub(super) async fn authenticate(window: &WebviewWindow, reason: String) -> Result<AuthOutcome, String> {
    let app = window.app_handle().clone();
    let callback_app = app.clone();
    let (sender, receiver) = oneshot::channel();
    on_main(&app, move |state, _| {
        if reason.is_empty() || state.authentication.is_some() || !state.active {
            let _ = sender.send(AuthOutcome::Cancelled);
            return;
        }
        let context: Retained<AnyObject> = unsafe { msg_send![class!(LAContext), new] };
        if !context_available(&context) {
            let _ = sender.send(AuthOutcome::Unavailable);
            return;
        }
        let epoch = state.epoch;
        let sender = std::sync::Arc::new(std::sync::Mutex::new(Some(sender)));
        let block = RcBlock::new(move |success: Bool, error: *mut NSError| {
            let code = unsafe { error.as_ref().map(|error| error.code()) };
            let result = auth_outcome(success.as_bool(), code);
            let sender = sender.clone();
            let _ = callback_app.run_on_main_thread(move || {
                let result = STATE.with(|cell| {
                    let mut slot = cell.borrow_mut();
                    let Some(state) = slot.as_mut() else { return AuthOutcome::Cancelled };
                    state.authentication.take();
                    if state.epoch == epoch && state.active { result } else { AuthOutcome::Cancelled }
                });
                if let Ok(mut sender) = sender.lock() {
                    if let Some(sender) = sender.take() { let _ = sender.send(result); }
                }
            });
        });
        state.authentication = Some(context.clone());
        unsafe {
            let _: () = msg_send![&*context, evaluatePolicy: 2isize, localizedReason: &*NSString::from_str(&reason), reply: &*block];
        }
    }).await?;
    receiver.await.map_err(|_| "App-lock authentication ended".to_string())
}

pub(super) async fn set_policy(app: &AppHandle, policy: &CoverPolicy) -> Result<LifecycleSnapshot, String> {
    let policy = policy.clone();
    on_main(app, move |state, mtm| {
        if state.policy.gate_required != policy.gate_required { state.ready_generation = state.ready_generation.wrapping_add(1); }
        state.policy = policy;
        state.render_covers(mtm, false);
        state.snapshot()
    }).await
}

pub(super) async fn ready(app: &AppHandle, epoch: u64, locked: bool) -> Result<bool, String> {
    let window = app.get_webview_window("main").ok_or("App-lock window is unavailable")?;
    let callback_app = app.clone();
    let (sender, receiver) = oneshot::channel();
    window.with_webview(move |native| {
        let generation = STATE.with(|cell| {
            let mut slot = cell.borrow_mut();
            let state = slot.as_mut()?;
            state.ready_generation = state.ready_generation.wrapping_add(1);
            (state.active && state.epoch == epoch && (locked || !state.policy.gate_required)).then_some(state.ready_generation)
        });
        let Some(generation) = generation else { let _ = sender.send(false); return };
        let sender = std::sync::Arc::new(std::sync::Mutex::new(Some(sender)));
        let block = RcBlock::new(move |image: *mut AnyObject, error: *mut NSError| {
            let frame_ready = !image.is_null() && error.is_null();
            let sender = sender.clone();
            let _ = callback_app.run_on_main_thread(move || {
                let accepted = STATE.with(|cell| {
                    let Some(mtm) = MainThreadMarker::new() else { return false };
                    let mut slot = cell.borrow_mut();
                    let Some(state) = slot.as_mut() else { return false };
                    if !frame_ready || !state.active || state.epoch != epoch || state.ready_generation != generation
                        || (!locked && state.policy.gate_required) { return false; }
                    state.policy.gate_required = locked;
                    state.awaiting_ready = false;
                    state.render_covers(mtm, false);
                    true
                });
                if let Ok(mut sender) = sender.lock() {
                    if let Some(sender) = sender.take() { let _ = sender.send(accepted); }
                }
            });
        });
        unsafe {
            let configuration: Retained<AnyObject> = msg_send![class!(WKSnapshotConfiguration), new];
            let _: () = msg_send![&*configuration, setAfterScreenUpdates: Bool::YES];
            let width: Retained<AnyObject> = msg_send![class!(NSNumber), numberWithDouble: 1.0f64];
            let _: () = msg_send![&*configuration, setSnapshotWidth: &*width];
            // A successful post-update snapshot is WebKit's presentation barrier.
            // Its image is discarded; no conversation image is stored or exported.
            let view = native.inner().cast::<AnyObject>();
            let _: () = msg_send![view, takeSnapshotWithConfiguration: &*configuration, completionHandler: &*block];
        }
    }).map_err(|error| error.to_string())?;
    receiver.await.map_err(|_| "App-lock presentation request ended".to_string())
}

pub(super) fn window_created(window: &WebviewWindow) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("App-lock window creation needs the main thread")?;
    STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() { state.render_covers(mtm, false); }
    });
    let _ = window;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn departure_notifications_follow_system_sleep_and_sessions_not_display_idle() {
        let notifications = workspace_notifications();
        let signal = |name: &NSNotificationName| notifications.iter()
            .find_map(|(observed, signal)| (*observed == name).then_some(*signal));
        unsafe {
            assert_eq!(signal(NSWorkspaceWillSleepNotification), Some(Signal::SessionAway));
            assert_eq!(signal(NSWorkspaceSessionDidResignActiveNotification), Some(Signal::SessionAway));
            assert_eq!(signal(NSWorkspaceDidWakeNotification), Some(Signal::WindowActive));
            assert_eq!(signal(NSWorkspaceSessionDidBecomeActiveNotification), Some(Signal::WindowActive));
            assert_eq!(signal(objc2_app_kit::NSWorkspaceScreensDidSleepNotification), None);
            assert_eq!(signal(objc2_app_kit::NSWorkspaceScreensDidWakeNotification), None);
        }
    }
}
