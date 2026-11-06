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

    pub(crate) fn record_ring_only(&self, level: &str, target: &str, message: impl AsRef<str>) {
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

pub(crate) fn mask(message: &str) -> String {
    let mut masked = without_temporary_file_paths(message);
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

/// tempfile appends the path it failed on, which names the user's folders.
fn without_temporary_file_paths(input: &str) -> String {
    const MARKER: &str = " at path \"";
    let mut kept = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find(MARKER) {
        kept.push_str(&rest[..start]);
        let quoted = &rest[start + MARKER.len()..];
        let mut escaped = false;
        let end = quoted.char_indices().find_map(|(index, character)| {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                return Some(index + 1);
            }
            None
        });
        rest = &quoted[end.unwrap_or(quoted.len())..];
    }
    kept.push_str(rest);
    kept
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

/// Records to the in-memory log only, for failures raised while the log
/// directory may be removed.
pub(crate) fn log_ring_only(level: &str, target: &str, message: String) {
    #[cfg(debug_assertions)]
    eprintln!("{}", format_console_line(level, target, &message));
    global_state().record_ring_only(level, target, message);
}

/// Target of the one line a failed native command leaves in the device log.
pub(crate) const COMMAND_TARGET: &str = "native-command";
const MAX_COMMAND_DETAIL_CHARS: usize = 2048;

/// What a failed command reports to the device log. The detail is the failure
/// behind the code; it must never carry user content.
pub(crate) trait CommandFailure {
    fn code(&self) -> std::borrow::Cow<'_, str>;
    fn detail(&self) -> Option<std::borrow::Cow<'_, str>> {
        None
    }
    /// A routine refusal, logged as a warning without its detail.
    fn expected(&self) -> bool {
        false
    }
}

/// A command that fails with a bare string returns either a code or a message.
impl CommandFailure for String {
    fn code(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(if is_code(self) { self } else { "error" })
    }
    fn detail(&self) -> Option<std::borrow::Cow<'_, str>> {
        (!is_code(self)).then(|| std::borrow::Cow::Borrowed(self.as_str()))
    }
    fn expected(&self) -> bool {
        matches!(self.as_str(), "cancelled" | "library-operation-busy" | "cleanup-pending" | "native-media-busy")
    }
}

fn is_code(text: &str) -> bool {
    text.len() <= 64
        && text.starts_with(|character: char| character.is_ascii_lowercase())
        && !text.ends_with('-')
        && text.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Logs a failed command once, with where it was called, and returns the
/// result unchanged.
#[track_caller]
pub(crate) fn logged<T, E: CommandFailure>(command: &str, result: Result<T, E>) -> Result<T, E> {
    if let Err(error) = &result {
        record_command_failure(command, error, std::panic::Location::caller());
    }
    result
}

/// Keeps a command's response intact without copying unaudited error text to diagnostics.
#[track_caller]
pub(crate) fn logged_without_detail<T, E: CommandFailure>(command: &str, result: Result<T, E>) -> Result<T, E> {
    struct Summary<'a, E>(&'a E);
    impl<E: CommandFailure> CommandFailure for Summary<'_, E> {
        fn code(&self) -> std::borrow::Cow<'_, str> { self.0.code() }
        fn expected(&self) -> bool { self.0.expected() }
    }
    if let Err(error) = &result {
        record_command_failure(command, &Summary(error), std::panic::Location::caller());
    }
    result
}

pub(crate) fn record_command_failure(
    command: &str,
    error: &(impl CommandFailure + ?Sized),
    at: &std::panic::Location<'_>,
) {
    let expected = error.expected();
    let cause = match error.detail() {
        Some(detail) if !expected => {
            let bounded: String = detail.chars().take(MAX_COMMAND_DETAIL_CHARS).collect();
            format!(" cause={bounded}")
        }
        _ => String::new(),
    };
    log_global(
        if expected { "warn" } else { "error" },
        COMMAND_TARGET,
        format!(
            "{command} failed: code={}{cause} at={}:{}",
            error.code(),
            at.file(),
            at.line()
        ),
    );
}

/// Names a JSON decoding failure by its category and position only. The
/// decoder's own text can quote the input it rejected; a reader's failure
/// cannot, so it is kept.
pub(crate) fn json_failure(error: &serde_json::Error) -> String {
    let category = match error.classify() {
        serde_json::error::Category::Io => return format!("json-io: {error}"),
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "shape",
        serde_json::error::Category::Eof => "incomplete",
    };
    format!("json-{category} at line {} column {}", error.line(), error.column())
}

/// An io failure's text, with a JSON failure carried inside it named by
/// [`json_failure`] instead.
pub(crate) fn io_failure(error: &std::io::Error) -> String {
    match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<serde_json::Error>())
    {
        Some(json) => json_failure(json),
        None => error.to_string(),
    }
}

/// A SQLite failure's text, with a JSON failure met while reading a row named
/// by [`json_failure`] instead.
pub(crate) fn sqlite_failure(error: &rusqlite::Error) -> String {
    if let rusqlite::Error::FromSqlConversionFailure(index, _, inner) = error {
        if let Some(json) = inner.downcast_ref::<serde_json::Error>() {
            return format!("column {index} is not readable: {}", json_failure(json));
        }
    }
    error.to_string()
}

/// The text of a failure kept for the device log. A JSON failure keeps only its
/// position, and a network failure only its kind, since its text can carry a
/// signed URL. The type is matched by name so borrowed errors such as a
/// poisoned lock are accepted too.
pub(crate) fn failure_text<E: std::fmt::Display + ?Sized>(error: &E) -> String {
    let mut name = std::any::type_name::<E>();
    loop {
        if let Some(inner) = name.strip_prefix('&') {
            name = inner.strip_prefix("mut ").unwrap_or(inner);
        } else if let Some(inner) = name.strip_prefix("alloc::boxed::Box<").and_then(|inner| inner.strip_suffix('>')) {
            name = inner;
        } else {
            break;
        }
    }
    if name == std::any::type_name::<serde_json::Error>() {
        let text = error.to_string();
        match text.rfind(" at line ") {
            Some(position) => format!("json failure{}", &text[position..]),
            None => "json failure".to_owned(),
        }
    } else if name == std::any::type_name::<reqwest::Error>() {
        "network failure".to_owned()
    } else {
        error.to_string()
    }
}

fn format_console_line(level: &str, target: &str, message: &str) -> String {
    mask(&format!("[{level}] {target}: {message}"))
}

pub(crate) fn install_panic_hook() {
    install_panic_hook_once_for(&PANIC_HOOK_INSTALLED, global_state());
}

pub(crate) fn record_startup_failure(message: &str) {
    global_state().record("error", "startup", message);
    #[cfg(desktop)]
    append_startup_diagnostic(&std::env::temp_dir().join("risunest-startup-failure.log"), message);
    #[cfg(any(target_os = "android", target_os = "ios", target_os = "macos"))]
    platform_startup_diagnostic(message);
    eprintln!("{}", format_console_line("error", "startup", message));
}

#[cfg(any(target_os = "android", target_os = "ios", target_os = "macos"))]
fn platform_startup_diagnostic(message: &str) {
    let bounded: String = format_console_line("error", "startup", message).chars().take(4096).collect();
    let message = std::ffi::CString::new(bounded.replace('\0', " ")).unwrap();
    #[cfg(target_os = "android")]
    {
        #[link(name = "log")]
        unsafe extern "C" {
            fn __android_log_write(priority: i32, tag: *const std::ffi::c_char, text: *const std::ffi::c_char) -> i32;
        }
        unsafe { __android_log_write(6, c"RisuNest".as_ptr(), message.as_ptr()); }
    }
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    {
        unsafe extern "C" { fn risunest_startup_log(message: *const std::ffi::c_char); }
        unsafe { risunest_startup_log(message.as_ptr()); }
    }
}

#[cfg(any(desktop, test))]
fn append_startup_diagnostic(path: &Path, message: &str) {
    let line: String = format_console_line("error", "startup", message).chars().take(4096).collect();
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
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
        #[cfg(desktop)]
        if state.0.lock().map(|inner| inner.root.is_none()).unwrap_or(true) {
            append_startup_diagnostic(&std::env::temp_dir().join("risunest-startup-failure.log"),
                payload.unwrap_or("native startup panic"));
        }
        #[cfg(any(target_os = "android", target_os = "ios", target_os = "macos"))]
        platform_startup_diagnostic(payload.unwrap_or("native startup panic"));
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
    crate::slow_log::summarize_repeats();
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
