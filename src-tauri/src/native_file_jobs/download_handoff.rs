use super::{cleanup_handoff_path, owned_handoff_file, NativeFileJobState, NativeJobError};
use crate::native_log::logged;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use tauri::State;
use uuid::Uuid;

// A frontend download on Android is written here in pieces, then copied to the
// user's destination by the SAF export bridge.
pub(crate) const PREFIX: &str = "risu-download-";
pub(crate) const SUFFIX: &str = ".bin";
/// Raw bytes per append; the base64 text of a full piece and its request
/// envelope stay within the 4 MiB plain invoke budget.
const MAX_PIECE_BYTES: usize = 3 * 1024 * 1024 - 3 * 1024;
const MAX_ENCODED_PIECE_BYTES: usize = MAX_PIECE_BYTES / 3 * 4;
const LABEL: &str = "download";

fn write_error(operation: &str, error: impl std::fmt::Display) -> NativeJobError {
    NativeJobError::new("destination-write-failed", format!("{operation}: {error}"))
}

fn create(root: &Path) -> Result<String, NativeJobError> {
    let directory = root.join("handoffs");
    fs::create_dir_all(&directory)
        .map_err(|error| write_error("create download handoff directory", error))?;
    let path = directory.join(format!("{PREFIX}{}{SUFFIX}", Uuid::new_v4()));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| write_error("create download handoff", error))?;
    Ok(path.to_string_lossy().into_owned())
}

fn append(root: &Path, path: &Path, offset: u64, chunk: &str) -> Result<u64, NativeJobError> {
    if chunk.len() > MAX_ENCODED_PIECE_BYTES {
        return Err(NativeJobError::new(
            "invalid-input",
            "download handoff pieces cannot exceed 3 MiB",
        ));
    }
    let bytes = STANDARD.decode(chunk).map_err(|error| {
        NativeJobError::new(
            "invalid-input",
            format!("download handoff piece is not base64: {error}"),
        )
    })?;
    if !owned_handoff_file(root, path, PREFIX, SUFFIX, LABEL, "destination-write-failed")? {
        return Err(NativeJobError::new("invalid-state", "download handoff is gone"));
    }
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| write_error("open download handoff", error))?;
    let length = file
        .metadata()
        .map_err(|error| write_error("read download handoff length", error))?
        .len();
    // A repeated or skipped piece must not shift the rest of the file.
    if length != offset {
        return Err(NativeJobError::new(
            "invalid-state",
            format!("download handoff holds {length} bytes, not {offset}"),
        ));
    }
    file.write_all(&bytes)
        .map_err(|error| write_error("append download handoff", error))?;
    Ok(length + bytes.len() as u64)
}

#[tauri::command(async)]
pub(crate) fn native_download_handoff_create(
    state: State<'_, NativeFileJobState>,
) -> Result<String, NativeJobError> {
    logged("native_download_handoff_create", (|| {
        let _cleanup_operation = state.admit_cleanup_operation()?;
        create(&state.root)
    })())
}

#[tauri::command(async)]
pub(crate) fn native_download_handoff_append(
    state: State<'_, NativeFileJobState>,
    path: String,
    offset: u64,
    chunk: String,
) -> Result<u64, NativeJobError> {
    logged("native_download_handoff_append", (|| {
        let _cleanup_operation = state.admit_cleanup_operation()?;
        append(&state.root, Path::new(&path), offset, &chunk)
    })())
}

#[tauri::command(async)]
pub(crate) fn native_download_handoff_cleanup(
    state: State<'_, NativeFileJobState>,
    path: String,
) -> Result<bool, NativeJobError> {
    logged(
        "native_download_handoff_cleanup",
        cleanup_handoff_path(&state.root, Path::new(&path), PREFIX, SUFFIX, LABEL),
    )
}

/// Writes a natively produced download to a new handoff for a mobile picker
/// to copy from; `failed` is the code every write failure returns.
pub(crate) fn write_native(state: &NativeFileJobState, failed: &str, write: impl FnOnce(&mut dyn Write) -> Result<(), String>) -> Result<String, String> {
    let path = create(&state.root).map_err(|_| failed.to_owned())?;
    let result = (|| {
        let output = OpenOptions::new().write(true).open(&path).map_err(|_| failed.to_owned())?;
        let mut writer = std::io::BufWriter::new(output);
        write(&mut writer)?;
        writer.flush().map_err(|_| failed.to_owned())?;
        writer.get_ref().sync_all().map_err(|_| failed.to_owned())?;
        Ok::<_, String>(())
    })();
    if let Err(error) = result { let _ = fs::remove_file(&path); return Err(error); }
    Ok(path)
}

/// Writes a natively produced download to the path a desktop picker chose.
/// The path must be absolute and outside the app's own folders, and an
/// existing file there is replaced only once the whole download is written.
/// Returns the bytes written.
#[cfg(desktop)]
pub(crate) fn write_destination(
    owned_roots: &[std::path::PathBuf],
    destination: &str,
    invalid: &str,
    failed: &str,
    write: impl FnOnce(&mut dyn Write) -> Result<(), String>,
) -> Result<u64, String> {
    let destination = std::path::PathBuf::from(destination);
    let parent = destination
        .parent()
        .ok_or_else(|| invalid.to_owned())?
        .canonicalize()
        .map_err(|_| invalid.to_owned())?;
    if !destination.is_absolute()
        || owned_roots.iter().any(|root| root.canonicalize().is_ok_and(|root| parent.starts_with(root)))
    {
        return Err(invalid.to_owned());
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&parent).map_err(|_| failed.to_owned())?;
    {
        let mut writer = std::io::BufWriter::new(temporary.as_file_mut());
        write(&mut writer)?;
        writer.flush().map_err(|_| failed.to_owned())?;
    }
    temporary.as_file().sync_all().map_err(|_| failed.to_owned())?;
    let bytes = temporary.as_file().metadata().map_err(|_| failed.to_owned())?.len();
    temporary.persist(destination).map_err(|_| failed.to_owned())?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, PathBuf) {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("native-file-jobs");
        fs::create_dir_all(&root).unwrap();
        (directory, root)
    }

    fn code(result: Result<u64, NativeJobError>) -> String {
        result.unwrap_err().code
    }

    #[test]
    fn appended_pieces_build_the_owned_handoff_in_order() {
        let (_directory, root) = fixture();
        let path = PathBuf::from(create(&root).unwrap());
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(super::super::handoff_name(&path, PREFIX, SUFFIX), "{name}");
        assert_eq!(path.parent().unwrap(), root.join("handoffs"));
        assert_eq!(fs::read(&path).unwrap(), b"");

        assert_eq!(append(&root, &path, 0, &STANDARD.encode(b"first ")).unwrap(), 6);
        assert_eq!(append(&root, &path, 6, &STANDARD.encode(b"second")).unwrap(), 12);
        assert_eq!(append(&root, &path, 12, "").unwrap(), 12);
        assert_eq!(fs::read(&path).unwrap(), b"first second");

        assert!(cleanup_handoff_path(&root, &path, PREFIX, SUFFIX, LABEL).unwrap());
        assert!(!path.exists());
        assert!(!cleanup_handoff_path(&root, &path, PREFIX, SUFFIX, LABEL).unwrap());
    }

    #[test]
    fn a_repeated_or_skipped_piece_is_refused_without_writing() {
        let (_directory, root) = fixture();
        let path = PathBuf::from(create(&root).unwrap());
        append(&root, &path, 0, &STANDARD.encode(b"abc")).unwrap();

        assert_eq!(code(append(&root, &path, 0, &STANDARD.encode(b"abc"))), "invalid-state");
        assert_eq!(code(append(&root, &path, 4, &STANDARD.encode(b"d"))), "invalid-state");
        assert_eq!(fs::read(&path).unwrap(), b"abc");
    }

    #[test]
    fn a_full_piece_fits_and_a_larger_or_malformed_piece_is_refused() {
        let (_directory, root) = fixture();
        let path = PathBuf::from(create(&root).unwrap());
        let full = vec![7u8; MAX_PIECE_BYTES];
        let encoded = STANDARD.encode(&full);
        assert_eq!(encoded.len(), MAX_ENCODED_PIECE_BYTES);
        // Room for the path, offset and the request envelope.
        assert!(MAX_ENCODED_PIECE_BYTES + 3 * 1024 <= 4 * 1024 * 1024);
        assert_eq!(append(&root, &path, 0, &encoded).unwrap(), MAX_PIECE_BYTES as u64);

        let larger = STANDARD.encode(vec![7u8; MAX_PIECE_BYTES + 3]);
        let offset = MAX_PIECE_BYTES as u64;
        assert_eq!(code(append(&root, &path, offset, &larger)), "invalid-input");
        assert_eq!(code(append(&root, &path, offset, "not base64!")), "invalid-input");
        assert_eq!(fs::metadata(&path).unwrap().len(), offset);
    }

    #[test]
    fn appends_reach_only_an_existing_owned_download_handoff() {
        let (directory, root) = fixture();
        let created = PathBuf::from(create(&root).unwrap());
        let handoffs = root.join("handoffs");
        let piece = STANDARD.encode(b"x");

        let other_kind = handoffs.join(format!("risu-backup-{}.bin", Uuid::new_v4()));
        fs::write(&other_kind, b"").unwrap();
        let outside = directory.path().join(format!("{PREFIX}{}{SUFFIX}", Uuid::new_v4()));
        fs::write(&outside, b"").unwrap();
        let relative = PathBuf::from(created.file_name().unwrap());
        for path in [&other_kind, &outside, &relative] {
            assert_eq!(code(append(&root, path, 0, &piece)), "invalid-input", "{path:?}");
        }
        assert_eq!(fs::read(&other_kind).unwrap(), b"");
        assert_eq!(fs::read(&outside).unwrap(), b"");

        fs::remove_file(&created).unwrap();
        assert_eq!(code(append(&root, &created, 0, &piece)), "invalid-state");
        assert!(!created.exists());
    }

    #[cfg(desktop)]
    fn write_text(owned: &[PathBuf], destination: &Path, text: &'static str) -> Result<u64, String> {
        write_destination(owned, destination.to_str().unwrap(), "invalid", "failed", |writer| {
            writer.write_all(text.as_bytes()).map_err(|_| "failed".to_owned())
        })
    }

    #[cfg(desktop)]
    #[test]
    fn a_chosen_destination_is_replaced_only_by_a_whole_download() {
        let directory = TempDir::new().unwrap();
        let chosen = directory.path().join("chosen");
        fs::create_dir_all(&chosen).unwrap();
        let destination = chosen.join("export.txt");
        fs::write(&destination, b"earlier").unwrap();

        assert_eq!(write_text(&[], &destination, "whole download").unwrap(), 14);
        assert_eq!(fs::read(&destination).unwrap(), b"whole download");

        let failed = write_destination(&[], destination.to_str().unwrap(), "invalid", "failed", |writer| {
            writer.write_all(b"partial").map_err(|_| "failed".to_owned())?;
            Err("producer-failed".to_owned())
        });
        assert_eq!(failed.unwrap_err(), "producer-failed");
        assert_eq!(fs::read(&destination).unwrap(), b"whole download");
        assert_eq!(fs::read_dir(&chosen).unwrap().count(), 1);
    }

    #[cfg(desktop)]
    #[test]
    fn a_destination_inside_the_app_folders_or_relative_is_refused() {
        let directory = TempDir::new().unwrap();
        let owned = directory.path().join("app-data");
        fs::create_dir_all(owned.join("logs")).unwrap();
        let inside = owned.join("logs").join("risunest.log");
        fs::write(&inside, b"source").unwrap();

        assert_eq!(write_text(&[owned.clone()], &inside, "overwrite").unwrap_err(), "invalid");
        assert_eq!(fs::read(&inside).unwrap(), b"source");
        assert_eq!(write_text(&[owned.clone()], Path::new("export.txt"), "relative").unwrap_err(), "invalid");
        assert_eq!(write_text(&[owned], &directory.path().join("missing").join("export.txt"), "x").unwrap_err(), "invalid");
    }
}
