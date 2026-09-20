//! Places a non-painting child window over the maximize button drawn by the
//! webview. The shell hit-tests it as the real caption button, so the Snap
//! Layouts flyout opens on hover and the click maximizes or restores.
//!
//! The webview's own windows belong to the WebView2 process and cannot be
//! subclassed, so an overlay window is the only in-process hit-test target.

use std::{
    collections::HashMap,
    mem::size_of,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
};
use tauri::{AppHandle, Emitter, Manager, Runtime, Window};
use windows::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{BeginPaint, ClientToScreen, EndPaint, PAINTSTRUCT},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{
                TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, EnableMenuItem, GetClientRect, GetCursorPos,
                GetSystemMenu, GetWindowLongPtrW, IsZoomed, LoadCursorW, PostMessageW,
                RegisterClassW, SetMenuDefaultItem, SetWindowLongPtrW, SetWindowPos, ShowWindow,
                TrackPopupMenu, GWLP_USERDATA, HMENU, HTMAXBUTTON, HWND_TOP, IDC_ARROW, MF_ENABLED,
                MF_GRAYED, SC_CLOSE, SC_MAXIMIZE, SC_MINIMIZE, SC_MOVE, SC_RESTORE, SC_SIZE,
                SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_MAXIMIZE, SW_RESTORE, TPM_LEFTBUTTON,
                TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_ERASEBKGND, WM_NCHITTEST, WM_NCLBUTTONDBLCLK,
                WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE,
                WM_NCRBUTTONDOWN, WM_NCRBUTTONUP, WM_PAINT, WM_SYSCOMMAND, WNDCLASSW, WS_CHILD,
                WS_CLIPSIBLINGS,
            },
        },
    },
};

/// Logical geometry of the caption controls drawn by the frontend.
const BUTTON_WIDTH: f64 = 46.0;
const BAR_HEIGHT: f64 = 32.0;
pub const HOVER_EVENT: &str = "caption-maximize-hover";

struct State {
    top: isize,
    overlay: isize,
    hover: AtomicBool,
    emit: Box<dyn Fn(bool) + Send + Sync>,
}

fn states() -> &'static Mutex<HashMap<isize, &'static State>> {
    static STATES: OnceLock<Mutex<HashMap<isize, &'static State>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn class_name() -> windows::core::PCWSTR {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    let name = w!("RisuNestCaptionMaximize");
    REGISTERED.get_or_init(|| unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(overlay_proc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: name,
            ..Default::default()
        };
        RegisterClassW(&class);
    });
    name
}

/// Creates the overlay for the window once and positions it.
pub fn attach<R: Runtime>(window: &Window<R>) {
    let Ok(top) = window.hwnd() else {
        return;
    };
    let mut states = states().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if states.contains_key(&(top.0 as isize)) {
        return;
    }
    // The transparent host window rejects layered children, so the overlay is a
    // plain child that never paints; the webview keeps drawing the button.
    let overlay = unsafe {
        CreateWindowExW(
            Default::default(),
            class_name(),
            w!(""),
            WS_CHILD | WS_CLIPSIBLINGS,
            0,
            0,
            0,
            0,
            Some(top),
            None::<HMENU>,
            Some(GetModuleHandleW(None).unwrap_or_default().into()),
            None,
        )
    };
    let Ok(overlay) = overlay else {
        return;
    };
    let app: AppHandle<R> = window.app_handle().clone();
    let label = window.label().to_owned();
    let state: &'static State = Box::leak(Box::new(State {
        top: top.0 as isize,
        overlay: overlay.0 as isize,
        hover: AtomicBool::new(false),
        emit: Box::new(move |hover| {
            let _ = app.emit_to(label.as_str(), HOVER_EVENT, hover);
        }),
    }));
    unsafe {
        SetWindowLongPtrW(overlay, GWLP_USERDATA, state as *const State as isize);
    }
    states.insert(top.0 as isize, state);
    place(state);
}

/// Follows the window size and DPI so the overlay stays on the button.
pub fn layout<R: Runtime>(window: &Window<R>) {
    let Ok(top) = window.hwnd() else {
        return;
    };
    let state = states()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&(top.0 as isize))
        .copied();
    if let Some(state) = state {
        place(state);
    }
}

fn place(state: &State) {
    let top = HWND(state.top as *mut _);
    let overlay = HWND(state.overlay as *mut _);
    let mut client = RECT::default();
    unsafe {
        if GetClientRect(top, &mut client).is_err() {
            return;
        }
        let scale = GetDpiForWindow(top) as f64 / 96.0;
        let width = (BUTTON_WIDTH * scale).round() as i32;
        let height = (BAR_HEIGHT * scale).round() as i32;
        let _ = SetWindowPos(
            overlay,
            Some(HWND_TOP),
            client.right - 2 * width,
            0,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

/// Shows the window menu the way a right-click on a native caption does.
/// `client` is a position in logical CSS pixels inside the window; `None`
/// uses the cursor position.
pub fn show_system_menu<R: Runtime>(window: &Window<R>, client: Option<(f64, f64)>) {
    let Ok(top) = window.hwnd() else {
        return;
    };
    let mut point = POINT::default();
    unsafe {
        match client {
            Some((x, y)) => {
                let scale = GetDpiForWindow(top) as f64 / 96.0;
                point = POINT {
                    x: (x * scale).round() as i32,
                    y: (y * scale).round() as i32,
                };
                if !ClientToScreen(top, &mut point).as_bool() {
                    return;
                }
            }
            None => {
                if GetCursorPos(&mut point).is_err() {
                    return;
                }
            }
        }
    }
    track_system_menu(top, point);
}

fn track_system_menu(top: HWND, point: POINT) {
    unsafe {
        let menu = GetSystemMenu(top, false);
        if menu.is_invalid() {
            return;
        }
        let zoomed = IsZoomed(top).as_bool();
        let state = |enabled: bool| if enabled { MF_ENABLED } else { MF_GRAYED };
        for (item, enabled) in [
            (SC_RESTORE, zoomed),
            (SC_MOVE, !zoomed),
            (SC_SIZE, !zoomed),
            (SC_MINIMIZE, true),
            (SC_MAXIMIZE, !zoomed),
        ] {
            let _ = EnableMenuItem(menu, item, state(enabled));
        }
        let _ = SetMenuDefaultItem(menu, SC_CLOSE, 0);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_LEFTBUTTON | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            Some(0),
            top,
            None,
        );
        if command.0 != 0 {
            let _ = PostMessageW(Some(top), WM_SYSCOMMAND, WPARAM(command.0 as usize), LPARAM(0));
        }
    }
}

fn set_hover(state: &State, hover: bool) {
    if state.hover.swap(hover, Ordering::AcqRel) != hover {
        (state.emit)(hover);
    }
}

unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State;
    if state.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let state = &*state;
    match msg {
        WM_NCHITTEST => LRESULT(HTMAXBUTTON as isize),
        WM_NCMOUSEMOVE => {
            set_hover(state, true);
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE | TME_NONCLIENT,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut track);
            LRESULT(0)
        }
        WM_NCMOUSELEAVE => {
            set_hover(state, false);
            LRESULT(0)
        }
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK | WM_NCRBUTTONDOWN => LRESULT(0),
        WM_NCRBUTTONUP => {
            let mut point = POINT::default();
            if GetCursorPos(&mut point).is_ok() {
                track_system_menu(HWND(state.top as *mut _), point);
            }
            LRESULT(0)
        }
        WM_NCLBUTTONUP => {
            let top = HWND(state.top as *mut _);
            let _ = ShowWindow(
                top,
                if IsZoomed(top).as_bool() {
                    SW_RESTORE
                } else {
                    SW_MAXIMIZE
                },
            );
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut paint);
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
