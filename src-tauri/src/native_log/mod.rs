use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::State;

pub(crate) const RING_CAPACITY: usize = 2_000;
pub(crate) const FILE_ROTATE_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const FILE_LOG_DISABLED_MARKER: &str = "file-log.enabled";

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
    root: Option<PathBuf>,
}

#[derive(Clone)]
pub(crate) struct NativeLogState(Arc<Mutex<Inner>>);

static GLOBAL_SINK: OnceLock<NativeLogState> = OnceLock::new();

impl NativeLogState {
    pub(crate) fn initialize(root: impl AsRef<Path>) -> Self {
        let state = Self::for_tests();
        state.configure_file_path(root);
        state
    }

    pub(crate) fn for_tests() -> Self {
        Self(Arc::new(Mutex::new(Inner {
            entries: VecDeque::with_capacity(RING_CAPACITY),
            root: None,
        })))
    }

    pub(crate) fn configure_file_path(&self, root: impl AsRef<Path>) {
        let root = root.as_ref().to_path_buf();
        let _ = fs::create_dir_all(&root);
        if let Ok(mut inner) = self.0.lock() {
            inner.root = Some(root);
        }
    }

    pub(crate) fn record(&self, level: &str, target: &str, message: impl AsRef<str>) {
        let entry = LogEntry {
            ts_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or_default(),
            level: level.to_owned(),
            target: target.to_owned(),
            message: mask(message.as_ref()),
        };
        if let Ok(mut inner) = self.0.lock() {
            if inner.entries.len() == RING_CAPACITY {
                inner.entries.pop_front();
            }
            inner.entries.push_back(entry.clone());
            write_file(&inner, &entry);
        }
    }

    pub(crate) fn record_panic(&self, payload: &str, file: &str, line: u32, column: u32) {
        self.record(
            "panic",
            "panic",
            format!("{payload} at {file}:{line}:{column}"),
        );
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
            .join("native.log")
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
            fs::create_dir_all(parent)?;
            fs::write(marker, b"disabled")
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
    if root.join(FILE_LOG_DISABLED_MARKER).exists() {
        return;
    }
    let path = root.join("native.log");
    if fs::metadata(&path)
        .map(|metadata| metadata.len() >= FILE_ROTATE_BYTES as u64)
        .unwrap_or(false)
    {
        let rotated = path.with_extension("log.1");
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(&path, rotated);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
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
    for label in ["bearer ", "authorization:", "x-api-key:", "sk-"] {
        masked = redact_after(&masked, label);
    }
    redact_long_runs(&masked)
}

fn redact_after(input: &str, label: &str) -> String {
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
        result.push_str("[REDACTED]");
        let end = value_start
            + trimmed
            + suffix[trimmed..]
                .find(|character: char| {
                    character.is_whitespace() || character == ',' || character == ';'
                })
                .unwrap_or(suffix[trimmed..].len());
        cursor = end;
    }
    result.push_str(&input[cursor..]);
    result
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
        output.push_str("[REDACTED]");
    } else {
        output.push_str(run);
    }
    run.clear();
}

pub(crate) fn global_state() -> NativeLogState {
    GLOBAL_SINK.get_or_init(NativeLogState::for_tests).clone()
}

pub(crate) fn log_global(level: &str, target: &str, message: String) {
    global_state().record(level, target, message);
}

pub(crate) fn install_panic_hook() {
    install_panic_hook_for(global_state());
}

fn install_panic_hook_for(state: NativeLogState) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic payload");
        if let Some(location) = info.location() {
            state.record_panic(payload, location.file(), location.line(), location.column());
        } else {
            state.record("panic", "panic", payload);
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
pub(crate) fn native_log_file_path(state: State<'_, NativeLogState>) -> String {
    state.file_path().display().to_string()
}

#[tauri::command]
pub(crate) fn native_log_set_file_enabled(
    state: State<'_, NativeLogState>,
    enabled: bool,
) -> Result<(), String> {
    state
        .set_file_enabled(enabled)
        .map_err(|error| error.to_string())
}

#[macro_export]
macro_rules! nlog {
    ($level:expr, $($arg:tt)*) => {{
        $crate::native_log::log_global($level, module_path!(), format!($($arg)*));
    }};
}

#[cfg(test)]
mod tests;
