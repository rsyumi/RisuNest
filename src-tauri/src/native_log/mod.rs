use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::State;

pub(crate) const RING_CAPACITY: usize = 1_000;
pub(crate) const FILE_ROTATE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const FILE_LOG_DISABLED_MARKER: &str = "file-log.enabled";
pub(crate) const NATIVE_LOG_FILE_PATH_UNAVAILABLE: &str = "native-log-file-path-unavailable";
pub(crate) const NATIVE_LOG_FILE_UPDATE_FAILED: &str = "native-log-file-update-failed";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LogEntry {
    pub ts_ms: u128,
    pub level: String,
    pub target: String,
    pub message: String,
}

struct Inner {
    entries: VecDeque<LogEntry>,
    pending_file_entries: VecDeque<LogEntry>,
    root: Option<PathBuf>,
}

#[derive(Clone)]
pub(crate) struct NativeLogState(Arc<Mutex<Inner>>);

static GLOBAL_SINK: OnceLock<NativeLogState> = OnceLock::new();
static PANIC_HOOK_INSTALLED: Once = Once::new();

impl NativeLogState {
    pub(crate) fn initialize(root: impl AsRef<Path>) -> Self {
        let state = Self::for_tests();
        state.configure_file_path(root);
        state
    }

    pub(crate) fn for_tests() -> Self {
        Self(Arc::new(Mutex::new(Inner {
            entries: VecDeque::with_capacity(RING_CAPACITY),
            pending_file_entries: VecDeque::with_capacity(RING_CAPACITY),
            root: None,
        })))
    }

    pub(crate) fn configure_file_path(&self, root: impl AsRef<Path>) {
        let root = root.as_ref().join("logs");
        let _ = create_private_dir_all(&root);
        tighten_known_files(&root);
        if let Ok(mut inner) = self.0.lock() {
            if inner.root.as_ref() == Some(&root) {
                return;
            }
            inner.root = Some(root);
            let pending = std::mem::take(&mut inner.pending_file_entries);
            for entry in pending {
                write_file(&inner, &entry);
            }
        }
    }

    pub(crate) fn record(&self, level: &str, target: &str, message: impl AsRef<str>) {
        self.record_inner(level, target, message.as_ref(), true);
    }

    fn record_frontend_error(&self, message: &str) {
        let bounded: String = message.chars().take(2048).collect();
        self.record("error", "webview", bounded);
    }

    fn record_ring_only(&self, level: &str, target: &str, message: impl AsRef<str>) {
        self.record_inner(level, target, message.as_ref(), false);
    }

    fn record_inner(&self, level: &str, target: &str, message: &str, write_to_file: bool) {
        let entry = LogEntry {
            ts_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or_default(),
            level: level.to_owned(),
            target: target.to_owned(),
            message: mask(message),
        };
        if let Ok(mut inner) = self.0.lock() {
            if inner.entries.len() == RING_CAPACITY {
                inner.entries.pop_front();
            }
            inner.entries.push_back(entry.clone());
            if write_to_file {
                if inner.root.is_some() {
                    write_file(&inner, &entry);
                } else {
                    if inner.pending_file_entries.len() == RING_CAPACITY {
                        inner.pending_file_entries.pop_front();
                    }
                    inner.pending_file_entries.push_back(entry);
                }
            }
        }
    }

    pub(crate) fn record_panic(&self, payload: Option<&str>, file: &str, line: u32, column: u32) {
        let payload = payload.unwrap_or("non-string panic payload");
        let message = format!("panic captured at {file}:{line}:{column}: {payload}");
        self.record("panic", "panic", &message);
    }

    pub(crate) fn tail(&self, limit: Option<usize>) -> Vec<LogEntry> {
        let Ok(inner) = self.0.lock() else {
            return Vec::new();
        };
        let start = limit.unwrap_or(RING_CAPACITY).min(inner.entries.len());
        inner
            .entries
            .iter()
            .skip(inner.entries.len() - start)
            .cloned()
            .collect()
    }

    pub(crate) fn file_path(&self) -> PathBuf {
        self.0
            .lock()
            .ok()
            .and_then(|inner| inner.root.clone())
            .unwrap_or_default()
            .join("risunest.log")
    }

    pub(crate) fn file_enabled(&self) -> bool {
        let marker = self.marker_path();
        !marker.exists()
    }

    pub(crate) fn set_file_enabled(&self, enabled: bool) -> std::io::Result<()> {
        let marker = self.marker_path();
        if enabled {
            match fs::remove_file(marker) {
                Ok(()) => Ok(()),
                Err(ref error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        } else {
            let Some(parent) = marker.parent() else {
                return Ok(());
            };
            create_private_dir_all(parent)?;
            write_private_file(&marker, b"disabled")
        }
    }

    fn marker_path(&self) -> PathBuf {
        self.0
            .lock()
            .ok()
            .and_then(|inner| inner.root.clone())
            .unwrap_or_default()
            .join(FILE_LOG_DISABLED_MARKER)
    }
}

fn write_file(inner: &Inner, entry: &LogEntry) {
    let Some(root) = inner.root.as_ref() else {
        return;
    };
    let _ = create_private_dir_all(root);
    if root.join(FILE_LOG_DISABLED_MARKER).exists() {
        return;
    }
    let path = root.join("risunest.log");
    if fs::metadata(&path)
        .map(|metadata| metadata.len() > FILE_ROTATE_BYTES as u64)
        .unwrap_or(false)
    {
        let rotated = path.with_extension("log.1");
        let _ = fs::remove_file(&rotated);
        if fs::rename(&path, &rotated).is_ok() {
            tighten_file(&rotated);
        }
    }
    let Ok(mut file) = open_private_append(&path) else {
        return;
    };
    let _ = writeln!(
        file,
        "{} [{}] {}: {}",
        entry.ts_ms, entry.level, entry.target, entry.message
    );
}

fn mask(message: &str) -> String {
    let mut masked = message.to_owned();
    for key in [
        "authorization",
        "api_key",
        "apikey",
        "access_token",
        "accesstoken",
        "token",
        "x-api-key",
        "password",
        "secret",
    ] {
        masked = redact_json_value(&masked, key);
        masked = redact_query_value(&masked, key);
    }
    for label in ["authorization:", "x-api-key:"] {
        masked = redact_after_until(&masked, label, |character| {
            character == '\r' || character == '\n'
        });
    }
    masked = redact_after_until(&masked, "bearer ", |character| {
        character.is_whitespace() || character == ',' || character == ';'
    });
    for label in [
        "password is ",
        "secret is ",
        "password:",
        "secret:",
        "password=",
        "secret=",
    ] {
        masked = redact_after_until(&masked, label, |character| {
            character.is_whitespace()
                || character == ','
                || character == ';'
                || character == '&'
                || character == '#'
        });
    }
    redact_long_runs(&redact_sk_tokens(&masked))
}

fn redact_json_value(input: &str, key: &str) -> String {
    let needle = format!("\"{key}\"");
    let lower = input.to_ascii_lowercase();
    let mut result = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some(found) = lower[cursor..].find(&needle) {
        let start = cursor + found;
        let key_end = start + needle.len();
        let after_key = &input[key_end..];
        let whitespace = after_key.len() - after_key.trim_start().len();
        let colon = key_end + whitespace;
        if input.as_bytes().get(colon) != Some(&b':') {
            result.push_str(&input[cursor..key_end]);
            cursor = key_end;
            continue;
        }

        let after_colon = &input[colon + 1..];
        let value_whitespace = after_colon.len() - after_colon.trim_start().len();
        let value_start = colon + 1 + value_whitespace;
        result.push_str(&input[cursor..value_start]);

        if input.as_bytes().get(value_start) == Some(&b'\"') {
            result.push('\"');
            result.push_str("***");
            let value = &input[value_start + 1..];
            let end = find_json_string_end(value).unwrap_or(value.len());
            cursor = value_start + 1 + end;
        } else {
            result.push_str("***");
            let value = &input[value_start..];
            let end = value
                .find(|character: char| {
                    character == ',' || character == '}' || character.is_whitespace()
                })
                .unwrap_or(value.len());
            cursor = value_start + end;
        }
    }
    result.push_str(&input[cursor..]);
    result
}

fn find_json_string_end(input: &str) -> Option<usize> {
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if character == '\"' && !escaped {
            return Some(index);
        }
        escaped = character == '\\' && !escaped;
        if character != '\\' {
            escaped = false;
        }
    }
    None
}

fn redact_query_value(input: &str, key: &str) -> String {
    let mut masked = input.to_owned();
    for prefix in ['?', '&'] {
        let label = format!("{prefix}{key}=");
        masked = redact_after_until(&masked, &label, |character| {
            character == '&'
                || character == '#'
                || character == '\r'
                || character == '\n'
                || character.is_whitespace()
        });
    }
    masked
}

fn redact_sk_tokens(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(found) = input[cursor..].find("sk-") {
        let start = cursor + found;
        let value_start = start + "sk-".len();
        let previous_is_token_character = input[..start]
            .chars()
            .next_back()
            .is_some_and(is_secret_token_character);
        let next_is_token_character = input[value_start..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric());
        if previous_is_token_character || !next_is_token_character {
            output.push_str(&input[cursor..value_start]);
            cursor = value_start;
            continue;
        }

        output.push_str(&input[cursor..start]);
        output.push_str("***");
        let end = value_start
            + input[value_start..]
                .find(|character: char| !is_secret_token_character(character))
                .unwrap_or(input[value_start..].len());
        cursor = end;
    }
    output.push_str(&input[cursor..]);
    output
}

fn is_secret_token_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
}

fn redact_after_until(input: &str, label: &str, is_terminator: impl Fn(char) -> bool) -> String {
    let lower = input.to_ascii_lowercase();
    let label_lower = label.to_ascii_lowercase();
    let mut result = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(found) = lower[cursor..].find(&label_lower) {
        let start = cursor + found;
        let value_start = start + label.len();
        result.push_str(&input[cursor..value_start]);
        let suffix = &input[value_start..];
        let trimmed = suffix.len() - suffix.trim_start().len();
        result.push_str(&suffix[..trimmed]);
        result.push_str("***");
        let end = value_start
            + trimmed
            + suffix[trimmed..]
                .find(&is_terminator)
                .unwrap_or(suffix[trimmed..].len());
        cursor = end;
    }
    result.push_str(&input[cursor..]);
    result
}

fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        return Ok(());
    }

    #[cfg(not(unix))]
    fs::create_dir_all(path)
}

fn open_private_append(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    tighten_file(path);
    Ok(file)
}

fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    tighten_file(path);
    file.write_all(contents)
}

fn tighten_known_files(root: &Path) {
    for name in ["risunest.log", "risunest.log.1", FILE_LOG_DISABLED_MARKER] {
        let path = root.join(name);
        if path.exists() {
            tighten_file(&path);
        }
    }
}

fn tighten_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }

    #[cfg(not(unix))]
    let _ = path;
}

fn redact_long_runs(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut run = String::new();
    for character in input.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '+' | '/' | '=' | '_' | '-') {
            run.push(character);
        } else {
            append_run(&mut output, &mut run);
            output.push(character);
        }
    }
    append_run(&mut output, &mut run);
    output
}

fn append_run(output: &mut String, run: &mut String) {
    if run.len() >= 64 {
        output.push_str("***");
    } else {
        output.push_str(run);
    }
    run.clear();
}

pub(crate) fn global_state() -> NativeLogState {
    GLOBAL_SINK.get_or_init(NativeLogState::for_tests).clone()
}

pub(crate) fn log_global(level: &str, target: &str, message: String) {
    #[cfg(debug_assertions)]
    eprintln!("{}", format_console_line(level, target, &message));
    global_state().record(level, target, message);
}

fn format_console_line(level: &str, target: &str, message: &str) -> String {
    mask(&format!("[{level}] {target}: {message}"))
}

pub(crate) fn install_panic_hook() {
    install_panic_hook_once_for(&PANIC_HOOK_INSTALLED, global_state());
}

fn install_panic_hook_once_for(installed: &Once, state: NativeLogState) {
    installed.call_once(move || install_panic_hook_for(state));
}

fn install_panic_hook_for(state: NativeLogState) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str));
        if let Some(location) = info.location() {
            state.record_panic(payload, location.file(), location.line(), location.column());
        } else {
            state.record(
                "panic",
                "panic",
                format!(
                    "panic captured: {}",
                    payload.unwrap_or("non-string panic payload")
                ),
            );
        }
        previous(info);
    }));
}

#[tauri::command]
pub(crate) fn native_log_tail(
    state: State<'_, NativeLogState>,
    limit: Option<usize>,
) -> Vec<LogEntry> {
    state.tail(limit)
}

#[tauri::command]
pub(crate) fn native_log_error(state: State<'_, NativeLogState>, message: String) {
    state.record_frontend_error(&message);
}

fn file_path_for_command(state: &NativeLogState) -> Result<String, String> {
    let Some(path) = state
        .0
        .lock()
        .ok()
        .and_then(|inner| inner.root.clone())
        .map(|root| root.join("risunest.log"))
    else {
        state.record_ring_only(
            "error",
            "native_log",
            "file path unavailable: native log storage is not configured",
        );
        return Err(NATIVE_LOG_FILE_PATH_UNAVAILABLE.to_owned());
    };
    Ok(path.display().to_string())
}

#[tauri::command]
pub(crate) fn native_log_file_path(state: State<'_, NativeLogState>) -> Result<String, String> {
    file_path_for_command(&state)
}

fn set_file_enabled_for_command(state: &NativeLogState, enabled: bool) -> Result<(), String> {
    state.set_file_enabled(enabled).map_err(|error| {
        state.record_ring_only(
            "error",
            "native_log",
            format!("file logging update failed: {error}"),
        );
        NATIVE_LOG_FILE_UPDATE_FAILED.to_owned()
    })
}

#[tauri::command]
pub(crate) fn native_log_set_file_enabled(
    state: State<'_, NativeLogState>,
    enabled: bool,
) -> Result<(), String> {
    set_file_enabled_for_command(&state, enabled)
}

#[macro_export]
macro_rules! nlog {
    ($level:expr, $($arg:tt)*) => {{
        $crate::native_log::log_global($level, module_path!(), format!($($arg)*));
    }};
}

#[cfg(test)]
mod tests;
