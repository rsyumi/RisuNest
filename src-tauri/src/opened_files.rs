//! Desktop file association delivery.
//!
//! The installer registers the `risum`, `risup` and `charx` associations, so opening one of
//! those files launches `RisuNest.exe <path>`, or hands the path to the already running instance
//! through the single instance plugin. Both entry points park the paths here and the frontend
//! drains them once with `opened_files_take`, so a file is never delivered twice.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_fs::{FilePath, FsExt};

/// Frontend event announcing that new opened files are waiting to be drained.
pub(crate) const OPENED_FILES_EVENT: &str = "risu-opened-files";

/// Files opened through a desktop file association that the frontend has not drained yet.
#[derive(Default)]
pub(crate) struct OpenedFilesState {
    pending: Mutex<Vec<PathBuf>>,
}

impl OpenedFilesState {
    /// Seeds the state with the paths this process was launched with.
    pub(crate) fn from_launch_arguments() -> Self {
        Self {
            pending: Mutex::new(collect_opened_files(std::env::args_os())),
        }
    }

    fn push_all(&self, files: Vec<PathBuf>) {
        if files.is_empty() {
            return;
        }
        let mut pending = lock(&self.pending);
        for file in files {
            if !pending.contains(&file) {
                pending.push(file);
            }
        }
    }

    fn take(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *lock(&self.pending))
    }
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records the files a second launch carried and wakes the running frontend.
pub(crate) fn deliver_single_instance_arguments(app: &AppHandle, args: &[String]) {
    let files = collect_opened_files(args);
    if files.is_empty() {
        return;
    }
    let payload: Vec<String> = files
        .iter()
        .map(|file| file.to_string_lossy().into_owned())
        .collect();
    app.state::<OpenedFilesState>().push_all(files);
    if let Err(error) = app.emit(OPENED_FILES_EVENT, json!({ "files": payload })) {
        crate::nlog!("warn", "opened file notification failed: {error}");
    }
}

/// Drains the pending opened files, granting each one read access before handing it over.
#[tauri::command]
pub(crate) fn opened_files_take(app: AppHandle) -> Vec<String> {
    let files = app.state::<OpenedFilesState>().take();
    let mut allowed = Vec::with_capacity(files.len());
    for file in files {
        if let Some(scope) = app.try_fs_scope() {
            if let Err(error) = scope.allow_file(&file) {
                crate::nlog!("warn", "opened file read scope grant failed: {error}");
                continue;
            }
        }
        if let Err(error) = app.asset_protocol_scope().allow_file(&file) {
            crate::nlog!("warn", "opened file asset scope grant failed: {error}");
        }
        allowed.push(file.to_string_lossy().into_owned());
    }
    allowed
}

/// Keeps the launch arguments that name an existing file, skipping the executable itself.
///
/// Deep link URLs stay with the deep link plugin, switches are ignored, and every kept path is
/// canonicalized so the filesystem scope grant matches what the frontend later reads.
fn collect_opened_files<I, S>(args: I) -> Vec<PathBuf>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut collected: Vec<PathBuf> = Vec::new();
    for arg in args.into_iter().skip(1) {
        let Some(path) = normalize_opened_file(arg.as_ref()) else {
            continue;
        };
        if !collected.contains(&path) {
            collected.push(path);
        }
    }
    collected
}

fn normalize_opened_file(arg: &OsStr) -> Option<PathBuf> {
    let text = arg.to_string_lossy();
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') || has_url_scheme(trimmed) {
        return None;
    }
    let path = Path::new(arg);
    if !path.is_file() {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    match FilePath::Path(canonical).simplified() {
        FilePath::Path(path) => Some(path),
        FilePath::Url(_) => None,
    }
}

/// Reports whether the argument looks like a URL rather than a path.
///
/// Windows drive letters are a single character, so a scheme needs at least two.
fn has_url_scheme(value: &str) -> bool {
    let Some(separator) = value.find(':') else {
        return false;
    };
    if separator < 2 {
        return false;
    }
    let scheme = &value[..separator];
    scheme
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn opened_files_keep_only_existing_files_after_the_executable() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let executable = directory.path().join("RisuNest.exe");
        std::fs::write(&executable, b"executable").expect("executable fixture");
        let card = directory.path().join("card.charx");
        std::fs::write(&card, b"card").expect("card fixture");
        let preset = directory.path().join("preset.risup");
        std::fs::write(&preset, b"preset").expect("preset fixture");
        let missing = directory.path().join("missing.risum");

        let collected = collect_opened_files([
            OsString::from(executable.to_string_lossy().into_owned()),
            OsString::from(card.to_string_lossy().into_owned()),
            OsString::from(missing.to_string_lossy().into_owned()),
            OsString::from("risunestlocal://hub/1234"),
            OsString::from("risunestlocal:device-sync"),
            OsString::from("--flag"),
            OsString::from("   "),
            OsString::from(preset.to_string_lossy().into_owned()),
        ]);

        let names: Vec<&str> = collected
            .iter()
            .filter_map(|file| file.file_name().and_then(|name| name.to_str()))
            .collect();
        assert_eq!(names, vec!["card.charx", "preset.risup"]);
    }

    #[test]
    fn opened_files_deduplicate_the_same_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let module = directory.path().join("module.risum");
        std::fs::write(&module, b"module").expect("module fixture");
        let argument = OsString::from(module.to_string_lossy().into_owned());

        let collected = collect_opened_files([
            OsString::from("RisuNest.exe"),
            argument.clone(),
            argument.clone(),
        ]);

        assert_eq!(collected.len(), 1);
    }

    #[test]
    fn url_schemes_are_told_apart_from_windows_paths() {
        assert!(has_url_scheme("risunestlocal://hub/1"));
        assert!(has_url_scheme("risunestlocal:hub"));
        assert!(has_url_scheme("https://example.invalid/a.charx"));
        assert!(!has_url_scheme("C:\\Users\\risu\\card.charx"));
        assert!(!has_url_scheme("/home/risu/card.charx"));
        assert!(!has_url_scheme("card.charx"));
    }
}
