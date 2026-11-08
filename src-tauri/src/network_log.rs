use risunest_network_log::{Query, Recorder, Settings, Side};
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use tauri::Manager;

/// Opens the recorder and installs it for transports and commands, replacing any earlier one.
pub(crate) fn initialize(paths: &crate::app_paths::AppPaths) -> Result<(), String> {
    let retained = paths.data.join("network-logs");
    std::fs::create_dir_all(&retained).map_err(|_| "network-log-unavailable".to_owned())?;
    exclude_backup(&retained)?;
    risunest_network_log::install(Recorder::open(paths.cache.join("network-logs"), retained)?);
    Ok(())
}
fn exclude_backup(path: &Path) -> Result<(), String> {
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    {
        unsafe extern "C" { fn risunest_network_log_exclude_backup(path: *const std::ffi::c_char) -> bool; }
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| "network-log-unavailable")?;
        if !unsafe { risunest_network_log_exclude_backup(path.as_ptr()) } { return Err("network-log-backup-exclusion-failed".into()); }
    }
    #[cfg(not(any(target_os = "ios", target_os = "macos")))]
    let _ = path;
    Ok(())
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub(crate) enum Operation {
    Settings,
    Configure { settings: Settings },
    List { query: Query },
    Detail { id: String },
    Body { id: String, side: Side, offset: u64, length: Option<usize>, hex: Option<bool> },
    Events { id: String, offset: u64 },
    ArrivalTimes { id: String, ends: Vec<u64>, from: u64 },
    Copy { id: String, side: Side },
    Retain { id: String },
    Delete { id: String },
    Export { id: String, destination: Option<String> },
}
fn json(value: impl serde::Serialize) -> Result<Value, String> { serde_json::to_value(value).map_err(|_| "network-log-response-failed".into()) }

#[tauri::command]
pub(crate) async fn network_log_command(
    webview: tauri::Webview,
    request: Operation,
) -> Result<Value, String> {
    if webview.label() != "main" { return Err("network-log-access-denied".into()); }
    let recorder = risunest_network_log::global().ok_or("network-log-unavailable")?;
    let app = webview.app_handle().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let jobs = app.state::<crate::native_file_jobs::NativeFileJobState>();
        let _cleanup_operation = jobs.admit_cleanup_operation().map_err(|_| "network-log-unavailable")?;
        match request {
        Operation::Settings => json(recorder.settings()),
        Operation::Configure { settings } => { recorder.configure(settings)?; json(recorder.settings()) },
        Operation::List { query } => json(recorder.list(query)?),
        Operation::Detail { id } => json(recorder.record(id)?),
        Operation::Body { id, side, offset, length, hex } => json(recorder.snapshot(id)?.body_range(side, offset, length.unwrap_or(risunest_network_log::PAGE_BYTES), hex)?),
        Operation::Events { id, offset } => json(recorder.snapshot(id)?.observations(offset)?),
        Operation::ArrivalTimes { id, ends, from } => json(recorder.snapshot(id)?.arrival_times(&ends, from)?),
        Operation::Copy { id, side } => json(recorder.snapshot(id)?.copy_body(side)?),
        Operation::Retain { id } => { recorder.retain(id)?; Ok(Value::Null) },
        Operation::Delete { id } => { recorder.delete(id)?; Ok(Value::Null) },
        Operation::Export { id, destination } => {
            let mut snapshot = recorder.snapshot(id)?;
            if let Some(destination) = destination {
                #[cfg(mobile)]
                { let _ = destination; return Err("network-log-native-picker-required".into()); }
                #[cfg(desktop)]
                {
                    let paths = app.state::<crate::app_paths::AppPaths>();
                    let bytes = crate::native_file_jobs::download_handoff::write_destination(
                        &paths.owned_roots(),
                        &destination,
                        "network-log-invalid-destination",
                        "network-log-export-failed",
                        |writer| snapshot.export_json(writer),
                    )?;
                    json(serde_json::json!({ "bytes": bytes, "path": null }))
                }
            } else {
                let state = app.state::<crate::native_file_jobs::NativeFileJobState>();
                let path = crate::native_file_jobs::download_handoff::write_native(&state, "network-log-export-failed", |writer| snapshot.export_json(writer))?;
                let bytes = std::fs::metadata(&path).map_err(|_| "network-log-export-failed")?.len();
                json(serde_json::json!({ "path": path, "bytes": bytes }))
            }
        }
    }}).await.map_err(|_| "network-log-worker-failed".to_string())?
}


#[cfg(test)]
pub(crate) static TEST_RECORDER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reinitializing_replaces_the_installed_recorder_and_keeps_settings() {
        let _guard = TEST_RECORDER_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let paths = crate::app_paths::AppPaths::isolated(root.path().join("synthetic-network-log")).unwrap();
        initialize(&paths).unwrap();
        let old = risunest_network_log::global().unwrap();
        old.configure(Settings { retain: true, ..Settings::default() }).unwrap();
        old.shutdown();
        initialize(&paths).unwrap();
        assert!(old.is_stopped());
        let current = risunest_network_log::global().unwrap();
        assert!(!current.is_stopped());
        assert!(!current.settings().enabled);
        assert!(current.settings().retain);
        current.shutdown();
    }
}
