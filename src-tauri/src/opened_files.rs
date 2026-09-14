//! Desktop file association delivery.
//!
//! The installer registers Risu file associations, so opening one of
//! those files launches the app with a path, or hands the path to the already running instance
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
        let launch_directory = std::env::current_dir().ok();
        Self {
            pending: Mutex::new(collect_opened_files(
                std::env::args_os(),
                launch_directory.as_deref(),
            )),
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
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records the files a second launch carried and wakes the running frontend.
pub(crate) fn deliver_single_instance_arguments(
    app: &AppHandle,
    args: &[String],
    launch_directory: &str,
) {
    let files = collect_opened_files(args, Some(Path::new(launch_directory)));
    deliver_files(app, files);
}

#[cfg(target_os = "macos")]
pub(crate) fn deliver_opened_urls(app: &AppHandle, urls: &[url::Url]) {
    let files = urls
        .iter()
        .filter_map(|url| normalize_opened_file(OsStr::new(url.as_str()), None))
        .collect();
    deliver_files(app, files);
}

fn deliver_files(app: &AppHandle, files: Vec<PathBuf>) {
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
fn collect_opened_files<I, S>(args: I, launch_directory: Option<&Path>) -> Vec<PathBuf>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut collected: Vec<PathBuf> = Vec::new();
    for arg in args.into_iter().skip(1) {
        let Some(path) = normalize_opened_file(arg.as_ref(), launch_directory) else {
            continue;
        };
        if !collected.contains(&path) {
            collected.push(path);
        }
    }
    collected
}

fn normalize_opened_file(arg: &OsStr, launch_directory: Option<&Path>) -> Option<PathBuf> {
    let text = arg.to_string_lossy();
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return None;
    }
    // Linux launchers and macOS Finder deliver local files as encoded URLs.
    // Keep all other protocols with the deep-link plugin and only grant local files.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let file_url_path = if has_url_scheme(trimmed) {
        let uri = url::Url::parse(trimmed).ok()?;
        if uri.scheme() != "file" || uri.query().is_some() || uri.fragment().is_some() {
            return None;
        }
        Some(uri.to_file_path().ok()?)
    } else {
        None
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    if has_url_scheme(trimmed) {
        return None;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let path = file_url_path.as_deref().unwrap_or_else(|| Path::new(arg));
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let path = Path::new(arg);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let launch_directory =
            launch_directory.filter(|directory| directory.is_absolute() && directory.is_dir())?;
        launch_directory.join(path)
    };
    if !path.is_absolute() || !path.is_file() {
        return None;
    }
    let canonical = std::fs::canonicalize(&path).ok()?;
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
        && scheme.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn desktop_file_urls_decode_and_deduplicate_with_plain_paths() {
        let directory = tempfile::tempdir().unwrap();
        let card = directory.path().join("synthetic 한글 # %.charx");
        std::fs::write(&card, b"synthetic fixture").unwrap();
        let uri = url::Url::from_file_path(&card).unwrap();
        assert_eq!(
            normalize_opened_file(OsStr::new(uri.as_str()), None),
            Some(std::fs::canonicalize(&card).unwrap())
        );
        let collected = collect_opened_files(
            [
                OsString::from("RisuNest"),
                OsString::from(uri.as_str()),
                card.clone().into_os_string(),
            ],
            None,
        );
        assert_eq!(collected, vec![std::fs::canonicalize(card).unwrap()]);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn desktop_file_urls_reject_remote_hosts_queries_fragments_and_missing_files() {
        let directory = tempfile::tempdir().unwrap();
        let card = directory.path().join("synthetic.charx");
        std::fs::write(&card, b"synthetic fixture").unwrap();
        let uri = url::Url::from_file_path(&card).unwrap();
        let mut remote = uri.clone();
        remote.set_host(Some("example.invalid")).unwrap();
        for value in [
            remote.to_string(),
            format!("{uri}?query"),
            format!("{uri}#fragment"),
            url::Url::from_file_path(directory.path())
                .unwrap()
                .to_string(),
            url::Url::from_file_path(directory.path().join("missing.charx"))
                .unwrap()
                .to_string(),
        ] {
            assert!(
                normalize_opened_file(OsStr::new(&value), None).is_none(),
                "{value}"
            );
        }
    }

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

        let collected = collect_opened_files(
            [
                OsString::from(executable.to_string_lossy().into_owned()),
                OsString::from(card.to_string_lossy().into_owned()),
                OsString::from(missing.to_string_lossy().into_owned()),
                OsString::from("risunestlocal://hub/1234"),
                OsString::from("risunestlocal:device-sync"),
                OsString::from("--flag"),
                OsString::from("   "),
                OsString::from(preset.to_string_lossy().into_owned()),
            ],
            None,
        );

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

        let collected = collect_opened_files(
            [
                OsString::from("RisuNest.exe"),
                argument.clone(),
                argument.clone(),
            ],
            None,
        );

        assert_eq!(collected.len(), 1);
    }

    #[test]
    fn relative_opened_file_uses_the_supplied_launch_directory() {
        let first_directory = tempfile::tempdir().expect("first temporary directory");
        let second_directory = tempfile::tempdir().expect("second temporary directory");
        let first_card = first_directory.path().join("card.charx");
        let second_card = second_directory.path().join("card.charx");
        std::fs::write(&first_card, b"first card").expect("first card fixture");
        std::fs::write(&second_card, b"second card").expect("second card fixture");

        let collected = collect_opened_files(
            [OsString::from("RisuNest.exe"), OsString::from("card.charx")],
            Some(second_directory.path()),
        );

        assert_eq!(collected.len(), 1);
        assert_eq!(
            std::fs::read(&collected[0]).expect("read selected card"),
            b"second card"
        );
    }

    #[test]
    fn missing_relative_opened_file_is_ignored() {
        let directory = tempfile::tempdir().expect("temporary directory");

        let collected = collect_opened_files(
            [
                OsString::from("RisuNest.exe"),
                OsString::from("missing.charx"),
            ],
            Some(directory.path()),
        );

        assert!(collected.is_empty());
    }

    #[test]
    fn absolute_opened_file_does_not_require_a_launch_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let card = directory.path().join("card.charx");
        std::fs::write(&card, b"card").expect("card fixture");

        let collected = collect_opened_files(
            [
                OsString::from("RisuNest.exe"),
                OsString::from(card.as_os_str()),
            ],
            None,
        );

        assert_eq!(collected.len(), 1);
        assert_eq!(
            std::fs::read(&collected[0]).expect("read selected card"),
            b"card"
        );
    }

    #[test]
    fn relative_opened_file_requires_a_valid_launch_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let card = directory.path().join("card.charx");
        std::fs::write(&card, b"card").expect("card fixture");
        let missing_directory = directory.path().join("missing");
        let relative_directory = Path::new("relative-directory");

        for launch_directory in [
            None,
            Some(Path::new("")),
            Some(relative_directory),
            Some(&missing_directory),
        ] {
            let collected = collect_opened_files(
                [OsString::from("RisuNest.exe"), OsString::from("card.charx")],
                launch_directory,
            );
            assert!(collected.is_empty());
        }
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
