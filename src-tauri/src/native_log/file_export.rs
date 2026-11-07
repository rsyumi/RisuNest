//! Saves the whole saved log, the previous file and then the current one, as
//! one file, streamed natively so the log never crosses the WebView bridge.

use super::{
    Inner, NativeLogState, LOG_FILE_NAME, NATIVE_LOG_FILE_PATH_UNAVAILABLE, PREVIOUS_LOG_FILE_NAME,
};
use serde::Serialize;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use tauri::Manager;

pub(crate) const NATIVE_LOG_FILE_EMPTY: &str = "native-log-file-empty";
pub(crate) const NATIVE_LOG_FILE_EXPORT_FAILED: &str = "native-log-file-export-failed";
#[cfg(desktop)]
pub(crate) const NATIVE_LOG_FILE_INVALID_DESTINATION: &str = "native-log-file-invalid-destination";
/// Bytes read and written at a time.
pub(crate) const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// Both log files as they were at one moment. The handles keep reading the
/// bytes they opened even if the writer rotates the files afterwards.
pub(crate) struct WholeLog {
    /// Oldest first, each with its length when it was opened.
    parts: Vec<(File, u64)>,
}

impl WholeLog {
    /// Opens the files through the writer's locked state, so the writer
    /// cannot rotate them between the two opens.
    fn open(inner: &Inner) -> Result<Self, &'static str> {
        let Some(root) = inner.root.as_deref() else {
            return Err(NATIVE_LOG_FILE_PATH_UNAVAILABLE);
        };
        Self::open_files(root).map_err(|_| NATIVE_LOG_FILE_EXPORT_FAILED)
    }

    fn open_files(root: &Path) -> std::io::Result<Self> {
        let mut parts = Vec::with_capacity(2);
        for name in [PREVIOUS_LOG_FILE_NAME, LOG_FILE_NAME] {
            match File::open(root.join(name)) {
                Ok(file) => {
                    let length = file.metadata()?.len();
                    if length > 0 {
                        parts.push((file, length));
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Self { parts })
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.parts.iter().map(|(_, length)| length).sum()
    }

    /// Streams the files in order, ending a file that stops mid-line so the
    /// next one starts on its own line, and returns the bytes written.
    pub(crate) fn copy_to(self, output: &mut dyn Write) -> std::io::Result<u64> {
        let mut buffer = vec![0; COPY_BUFFER_BYTES];
        let mut written = 0u64;
        let mut last = b'\n';
        for (file, length) in self.parts {
            if last != b'\n' {
                output.write_all(b"\n")?;
                written += 1;
                last = b'\n';
            }
            let mut source = file.take(length);
            loop {
                let count = match source.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                };
                output.write_all(&buffer[..count])?;
                written += count as u64;
                last = buffer[count - 1];
            }
        }
        output.flush()?;
        Ok(written)
    }
}

impl NativeLogState {
    /// Opens both files while holding the lock the writer rotates under.
    pub(crate) fn open_whole_log(&self) -> Result<WholeLog, &'static str> {
        let inner = self.0.lock().map_err(|_| NATIVE_LOG_FILE_EXPORT_FAILED)?;
        WholeLog::open(&inner)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportedLog {
    /// The app-owned handoff a mobile platform copies from; none on desktop.
    pub path: Option<String>,
    pub bytes: u64,
}

fn export(app: &tauri::AppHandle, destination: Option<String>) -> Result<ExportedLog, String> {
    let jobs = app.state::<crate::native_file_jobs::NativeFileJobState>();
    let _cleanup_operation = jobs
        .admit_cleanup_operation()
        .map_err(|_| NATIVE_LOG_FILE_EXPORT_FAILED.to_owned())?;
    let log = app.state::<NativeLogState>().open_whole_log()?;
    if log.bytes() == 0 {
        return Err(NATIVE_LOG_FILE_EMPTY.to_owned());
    }
    let copy = |output: &mut dyn Write| {
        log.copy_to(output)
            .map(|_| ())
            .map_err(|_| NATIVE_LOG_FILE_EXPORT_FAILED.to_owned())
    };
    match destination {
        #[cfg(desktop)]
        Some(destination) => {
            let paths = app.state::<crate::app_paths::AppPaths>();
            let bytes = crate::native_file_jobs::download_handoff::write_destination(
                &paths.owned_roots(),
                &destination,
                NATIVE_LOG_FILE_INVALID_DESTINATION,
                NATIVE_LOG_FILE_EXPORT_FAILED,
                copy,
            )?;
            Ok(ExportedLog { path: None, bytes })
        }
        #[cfg(mobile)]
        Some(_) => Err("native-log-native-picker-required".to_owned()),
        None => {
            let path = crate::native_file_jobs::download_handoff::write_native(
                &jobs,
                NATIVE_LOG_FILE_EXPORT_FAILED,
                copy,
            )?;
            let bytes = std::fs::metadata(&path)
                .map_err(|_| NATIVE_LOG_FILE_EXPORT_FAILED.to_owned())?
                .len();
            Ok(ExportedLog { path: Some(path), bytes })
        }
    }
}

/// Writes the whole saved log to `destination` on desktop, or to a handoff
/// the platform picker copies from when no destination is given.
#[tauri::command]
pub(crate) async fn native_log_file_export(
    webview: tauri::Webview,
    destination: Option<String>,
) -> Result<ExportedLog, String> {
    if webview.label() != "main" {
        return Err("native-log-access-denied".to_owned());
    }
    let app = webview.app_handle().clone();
    tauri::async_runtime::spawn_blocking(move || {
        export(&app, destination).inspect_err(|code| {
            if code != NATIVE_LOG_FILE_EMPTY {
                // Kept out of the file so a failed save never changes what it saves.
                app.state::<NativeLogState>()
                    .record_ring_only("error", "native_log", format!("file export failed: {code}"));
            }
        })
    })
    .await
    .map_err(|_| NATIVE_LOG_FILE_EXPORT_FAILED.to_owned())?
}
