use std::sync::Mutex;
use tauri::{AppHandle, Manager};

/// Keep both native writers and the old renderer excluded until navigation starts.
#[derive(Default)]
pub(crate) struct RestartState(Mutex<Option<RestartGuard>>);
struct RestartGuard {
    _renderer: crate::persistent_store::commands::DeviceMaintenanceGuard,
    _native: crate::native_file_jobs::admission::Permit,
}

#[tauri::command(async)]
pub(crate) fn ios_prepare_restart(app: AppHandle) -> Result<(), String> {
    let state = app.state::<RestartState>();
    let mut pending = state.0.lock().map_err(|_| "restart-state-unavailable")?;
    if pending.is_some() {
        return Err("restart-already-pending".into());
    }
    let native = app
        .state::<crate::native_file_jobs::NativeFileJobState>()
        .admission
        .file(true)
        .map_err(str::to_owned)?;
    let renderer = app
        .state::<crate::persistent_store::PersistentStoreState>()
        .acquire_device_maintenance()
        .map_err(|error| error.to_string())?;
    *pending = Some(RestartGuard {
        _renderer: renderer,
        _native: native,
    });
    Ok(())
}

pub(crate) fn main_document_started(app: &AppHandle) {
    if let Some(state) = app.try_state::<RestartState>() {
        if let Ok(mut pending) = state.0.lock() {
            pending.take();
        }
    }
}
