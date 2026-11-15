//! The native error dialog a desktop startup shows when the user has to repair
//! the data root before RisuNest can start.

use crate::app_paths::LinkRefusal;
use std::path::{Path, PathBuf};

/// A startup refusal the user can resolve.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    Link(LinkRefusal),
    /// Another instance holds the data root lock.
    InUse,
}

/// Managed only by the product entry, so a verification harness that builds
/// the app another way never waits on a dialog.
pub(crate) struct Enabled;

/// A setup error that also calls for the dialog. It reads as the original
/// error, so the diagnostic and the exit code stay the same.
#[derive(Debug)]
pub(crate) struct Refused {
    message: String,
    refusal: Refusal,
    path: PathBuf,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Refused {}

impl Refused {
    pub(crate) fn show(&self) {
        show(self.refusal, &self.path);
    }
}

/// The data root lock's error, carrying the in-use dialog for the product entry.
pub(crate) fn lock_error<R: tauri::Runtime>(
    app: &impl tauri::Manager<R>,
    error: String,
    root: &Path,
) -> Box<dyn std::error::Error> {
    if error == crate::app_cleanup::DATA_ROOT_IN_USE && app.try_state::<Enabled>().is_some() {
        Box::new(Refused { message: error, refusal: Refusal::InUse, path: root.to_owned() })
    } else {
        error.into()
    }
}

/// Waits until the user closes the dialog.
pub(crate) fn show(refusal: Refusal, path: &Path) {
    let korean = korean();
    let (body, label) = match (refusal, korean) {
        (Refusal::Link(LinkRefusal::TargetMissing), true) => (
            "데이터 폴더 링크가 가리키는 폴더를 찾을 수 없습니다. 해당 폴더가 있는 드라이브를 연결하거나 링크를 다시 만드세요.",
            "데이터 폴더 링크",
        ),
        (Refusal::Link(LinkRefusal::TargetMissing), false) => (
            "The folder the data folder link points to can't be found. Connect the drive it is on, or recreate the link.",
            "Data folder link",
        ),
        (Refusal::Link(LinkRefusal::NetworkTarget), true) => (
            "데이터 폴더 링크가 네트워크 위치를 가리킵니다. 로컬 드라이브에 있는 폴더를 가리키도록 링크를 다시 만드세요.",
            "데이터 폴더 링크",
        ),
        (Refusal::Link(LinkRefusal::NetworkTarget), false) => (
            "The data folder link points to a network location. Recreate the link to point to a folder on a local drive.",
            "Data folder link",
        ),
        (Refusal::Link(LinkRefusal::NotDirectory), true) => (
            "데이터 폴더 링크가 폴더가 아닌 항목을 가리킵니다. 폴더를 가리키도록 링크를 다시 만드세요.",
            "데이터 폴더 링크",
        ),
        (Refusal::Link(LinkRefusal::NotDirectory), false) => (
            "The data folder link points to something that isn't a folder. Recreate the link to point to a folder.",
            "Data folder link",
        ),
        (Refusal::Link(LinkRefusal::NoPlainPath), _) => return,
        (Refusal::InUse, true) => (
            "같은 데이터 폴더를 사용하는 RisuNest가 이미 실행 중입니다. 실행 중인 RisuNest를 종료한 뒤 다시 실행하세요.",
            "데이터 폴더",
        ),
        (Refusal::InUse, false) => (
            "RisuNest is already running with the same data folder. Close it, then start RisuNest again.",
            "Data folder",
        ),
    };
    let title = if korean { "RisuNest를 시작할 수 없습니다" } else { "RisuNest can't start" };
    alert(title, &format!("{body}\n\n{label}: {}", path.display()));
}

#[cfg(windows)]
fn korean() -> bool {
    // SAFETY: takes no arguments and returns the user's display language.
    let language = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
    language & 0x3FF == 0x12 // LANG_KOREAN
}

#[cfg(target_os = "macos")]
fn korean() -> bool {
    objc2_foundation::NSLocale::preferredLanguages()
        .firstObject()
        .is_some_and(|language| language.to_string().starts_with("ko"))
}

#[cfg(target_os = "linux")]
fn korean() -> bool {
    ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .is_some_and(|language| language.starts_with("ko"))
}

#[cfg(windows)]
fn alert(title: &str, message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND};
    let wide = |text: &str| text.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (title, message) = (wide(title), wide(message));
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call.
    unsafe { MessageBoxW(std::ptr::null_mut(), message.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR | MB_SETFOREGROUND) };
}

#[cfg(target_os = "macos")]
fn alert(title: &str, message: &str) {
    use objc2_app_kit::{NSAlert, NSAlertStyle, NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::NSString;
    let Some(main_thread) = objc2::MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(main_thread);
    // Before the event loop exists an unbundled process cannot show a window.
    if app.activationPolicy() == NSApplicationActivationPolicy::Prohibited {
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let alert = NSAlert::new(main_thread);
    alert.setAlertStyle(NSAlertStyle::Critical);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.runModal();
}

#[cfg(target_os = "linux")]
fn alert(title: &str, message: &str) {
    use gtk::prelude::*;
    // No display to show it on.
    if gtk::init().is_err() { return; }
    let dialog = gtk::MessageDialog::new(
        None::<&gtk::Window>,
        gtk::DialogFlags::MODAL,
        gtk::MessageType::Error,
        gtk::ButtonsType::Ok,
        title,
    );
    dialog.set_title("RisuNest");
    dialog.set_secondary_text(Some(message));
    dialog.run();
    dialog.close();
}
