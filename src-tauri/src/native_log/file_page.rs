//! Reads the saved log back in bounded pages, newest first, so neither a
//! response nor the viewer ever carries a whole file.

use super::{
    LogEntry, NativeLogState, LOG_FILE_NAME, NATIVE_LOG_FILE_PATH_UNAVAILABLE,
    PREVIOUS_LOG_FILE_NAME,
};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::Path;
use tauri::State;

/// Most bytes of the file one page reads.
pub(crate) const FILE_PAGE_BYTES: usize = 64 * 1024;
pub(crate) const NATIVE_LOG_FILE_READ_FAILED: &str = "native-log-file-read-failed";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LogFile {
    #[default]
    Current,
    Previous,
}

/// Where the next older page ends: the start of the page just read, or the
/// end of the previous file.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileCursor {
    pub file: LogFile,
    pub before: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FilePage {
    /// Whole entries in file order, oldest first.
    pub entries: Vec<LogEntry>,
    pub older: Option<FileCursor>,
}

pub(crate) fn read_page(
    root: &Path,
    file: LogFile,
    before: Option<u64>,
    page_bytes: usize,
) -> std::io::Result<FilePage> {
    let page_bytes = page_bytes.max(1) as u64;
    let (file, before) = match file {
        // A cursor past the end was handed out before the file was rotated;
        // the lines it points into are in the previous file now.
        LogFile::Current if before.is_some_and(|before| before > length(&root.join(LOG_FILE_NAME))) => {
            (LogFile::Previous, before)
        }
        _ => (file, before),
    };
    let path = root.join(match file {
        LogFile::Current => LOG_FILE_NAME,
        LogFile::Previous => PREVIOUS_LOG_FILE_NAME,
    });
    let opened = match File::open(&path) {
        Ok(handle) => {
            let size = handle.metadata()?.len();
            Some((handle, before.map_or(size, |before| before.min(size))))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let Some((mut handle, end)) = opened.filter(|(_, end)| *end > 0) else {
        return match file {
            LogFile::Current => read_page(root, LogFile::Previous, None, page_bytes as usize),
            LogFile::Previous => Ok(FilePage { entries: Vec::new(), older: None }),
        };
    };
    let window_start = end.saturating_sub(page_bytes);
    handle.seek(SeekFrom::Start(window_start))?;
    let mut bytes = vec![0; (end - window_start) as usize];
    handle.read_exact(&mut bytes)?;
    let skipped = if window_start == 0 { 0 } else { first_entry_start(&bytes) };
    let start = window_start + skipped as u64;
    let older = if start > 0 {
        Some(FileCursor { file, before: Some(start) })
    } else if file == LogFile::Current && length(&root.join(PREVIOUS_LOG_FILE_NAME)) > 0 {
        Some(FileCursor { file: LogFile::Previous, before: None })
    } else {
        None
    };
    Ok(FilePage { entries: parse_entries(&bytes[skipped..]), older })
}

fn length(path: &Path) -> u64 {
    std::fs::metadata(path).map(|metadata| metadata.len()).unwrap_or(0)
}

/// Where the first whole entry in a window starts: past the line its start
/// cuts, and past the rest of an entry that began before the window.
fn first_entry_start(bytes: &[u8]) -> usize {
    let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
        // One line longer than the page; it is shown cut rather than skipped.
        return 0;
    };
    let first_line = newline + 1;
    let mut offset = first_line;
    while offset < bytes.len() {
        let line_end = bytes[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| offset + index);
        if parse_line(&String::from_utf8_lossy(&bytes[offset..line_end])).is_some() {
            return offset;
        }
        offset = line_end + 1;
    }
    // No entry starts inside the window, so its whole lines are kept as they are.
    if first_line < bytes.len() {
        first_line
    } else {
        0
    }
}

/// Reads lines written by `file_line`; a line that does not start an entry
/// continues the message of the entry before it.
pub(crate) fn parse_entries(bytes: &[u8]) -> Vec<LogEntry> {
    let text = String::from_utf8_lossy(bytes);
    let mut entries: Vec<LogEntry> = Vec::new();
    if text.is_empty() {
        return entries;
    }
    for line in text.strip_suffix('\n').unwrap_or(&text).split('\n') {
        if let Some(entry) = parse_line(line) {
            entries.push(entry);
        } else if let Some(last) = entries.last_mut() {
            last.message.push('\n');
            last.message.push_str(line);
        } else {
            entries.push(LogEntry {
                ts_ms: 0,
                level: String::new(),
                target: String::new(),
                message: line.to_owned(),
            });
        }
    }
    entries
}

fn parse_line(line: &str) -> Option<LogEntry> {
    let (timestamp, rest) = line.split_once(' ')?;
    if timestamp.is_empty() || !timestamp.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let (level, rest) = rest.strip_prefix('[')?.split_once("] ")?;
    if level.is_empty() || level.contains(' ') {
        return None;
    }
    let (target, message) = rest.split_once(": ")?;
    Some(LogEntry {
        ts_ms: timestamp.parse().ok()?,
        level: level.to_owned(),
        target: target.to_owned(),
        message: message.to_owned(),
    })
}

pub(crate) fn read_page_for_command(
    state: &NativeLogState,
    file: LogFile,
    before: Option<u64>,
) -> Result<FilePage, String> {
    let Some(root) = state.root() else {
        state.record_ring_only(
            "error",
            "native_log",
            "file read unavailable: native log storage is not configured",
        );
        return Err(NATIVE_LOG_FILE_PATH_UNAVAILABLE.to_owned());
    };
    read_page(&root, file, before, FILE_PAGE_BYTES).map_err(|error| {
        state.record_ring_only("error", "native_log", format!("file read failed: {error}"));
        NATIVE_LOG_FILE_READ_FAILED.to_owned()
    })
}

#[tauri::command]
pub(crate) async fn native_log_file_read(
    state: State<'_, NativeLogState>,
    file: Option<LogFile>,
    before: Option<u64>,
) -> Result<FilePage, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        read_page_for_command(&state, file.unwrap_or_default(), before)
    })
    .await
    .map_err(|_| NATIVE_LOG_FILE_READ_FAILED.to_owned())?
}
