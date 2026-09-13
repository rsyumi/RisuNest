//! macOS window lifetime and an acknowledged, cancellable application quit.
use std::sync::Mutex;

#[derive(Default)]
struct ExitDecision {
    ready: bool,
    pending: Option<(String, i32)>,
    allowed: bool,
}

impl ExitDecision {
    fn request(&mut self, code: i32) -> Option<String> {
        if !self.ready || self.allowed || self.pending.is_some() {
            return None;
        }
        let token = uuid::Uuid::new_v4().to_string();
        self.pending = Some((token.clone(), code));
        Some(token)
    }

    fn respond(&mut self, token: &str, exit: bool) -> Result<Option<i32>, String> {
        if self.pending.as_ref().map(|pending| pending.0.as_str()) != Some(token) {
            return Err("No matching macOS quit request".into());
        }
        let (_, code) = self.pending.take().unwrap();
        self.allowed = exit;
        Ok(exit.then_some(code))
    }
}

#[derive(Default)]
pub(crate) struct ExitState(Mutex<ExitDecision>);

#[cfg(target_os = "macos")]
use tauri::{Emitter, Manager};

#[cfg(target_os = "macos")]
#[tauri::command]
pub(crate) fn macos_lifecycle_ready(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, ExitState>,
) -> Result<(), String> {
    if window.label() != "main" {
        return Err("macOS lifecycle belongs to the main window".into());
    }
    state.0.lock().map_err(|_| "Quit state unavailable")?.ready = true;
    Ok(())
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub(crate) fn macos_exit_response(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, ExitState>,
    token: String,
    exit: bool,
) -> Result<(), String> {
    if window.label() != "main" {
        return Err("macOS lifecycle belongs to the main window".into());
    }
    let code = state
        .0
        .lock()
        .map_err(|_| "Quit state unavailable")?
        .respond(&token, exit)?;
    if let Some(code) = code {
        window.app_handle().exit(code);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn show_main(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn handle_run_event(app: &tauri::AppHandle, event: tauri::RunEvent) {
    match event {
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::CloseRequested { api, .. },
            ..
        } if label == "main" => {
            api.prevent_close();
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.hide();
            }
        }
        tauri::RunEvent::Reopen { .. } => show_main(app),
        tauri::RunEvent::Opened { urls } => {
            crate::opened_files::deliver_opened_urls(app, &urls);
            show_main(app);
        }
        tauri::RunEvent::ExitRequested { api, code, .. }
            // Tauri explicitly cannot prevent restart. Existing relaunch callers
            // settle their storage before requesting it.
            if code != Some(tauri::RESTART_EXIT_CODE) =>
        {
            let state = app.state::<ExitState>();
            let mut decision = state.0.lock().unwrap_or_else(|error| error.into_inner());
            if !decision.ready || decision.allowed {
                return;
            }
            api.prevent_exit();
            if let Some(token) = decision.request(code.unwrap_or(0)) {
                // A hidden window must become visible for save/sync confirmation.
                show_main(app);
                if let Err(error) = app.emit_to("main", "risu-macos-exit-requested", &token) {
                    decision.pending = None;
                    crate::nlog!("warn", "macOS quit notification failed: {error}");
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_requires_readiness_and_a_matching_single_response() {
        let mut state = ExitDecision::default();
        assert!(state.request(0).is_none());
        state.ready = true;
        let token = state.request(7).unwrap();
        assert!(state.request(0).is_none());
        assert!(state.respond("stale", true).is_err());
        assert!(!state.allowed);
        assert_eq!(state.respond(&token, true).unwrap(), Some(7));
        assert!(state.allowed);
        assert!(state.respond(&token, true).is_err());
    }

    #[test]
    fn cancellation_leaves_the_app_running_and_allows_retry() {
        let mut state = ExitDecision {
            ready: true,
            ..Default::default()
        };
        let first = state.request(0).unwrap();
        assert_eq!(state.respond(&first, false).unwrap(), None);
        assert!(!state.allowed);
        let next = state.request(0).unwrap();
        assert_ne!(first, next);
        assert!(state.respond(&first, true).is_err());
        assert_eq!(state.respond(&next, true).unwrap(), Some(0));
    }
}
