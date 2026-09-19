use super::{JobControl, JobPhase, JobProgress, JobResultSummary, NativeJobError};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use zip::write::FileOptions;

const FORMAT: &str = "risunest-raw-recovery";
const VERSION: u8 = 1;
const BUFFER_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAIN_DATABASE: &str = "persistent/persistent.sqlite";
const DATABASE_FILES: [&str; 6] = [
    MAIN_DATABASE,
    "persistent/persistent.sqlite-wal",
    "persistent/persistent.sqlite-shm",
    "persistent/device.sqlite",
    "persistent/device.sqlite-wal",
    "persistent/device.sqlite-shm",
];
const DATA_DIRECTORIES: [&str; 4] = ["assets-v2/objects", "assets", "blobstore", "coldstorage"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryManifest {
    format: &'static str,
    version: u8,
    app_version: String,
    captured_at_millis: u64,
    status: &'static str,
    paths: Vec<ManifestPath>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestPath {
    original_relative_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive_entry_path: Option<String>,
    captured_bytes: u64,
    status: &'static str,
}

#[derive(Debug)]
pub(super) struct CapturedArchive {
    pub(super) path: PathBuf,
    pub(super) bytes: u64,
    pub(super) sha256: String,
    pub(super) partial: bool,
}

pub(crate) fn is_raw_recovery_archive(
    reader: &mut (impl Read + Seek),
) -> Result<bool, NativeJobError> {
    reader.seek(SeekFrom::Start(0)).map_err(|_| {
        NativeJobError::new(
            "invalid-input",
            "Recovery format probe could not seek the source",
        )
    })?;
    let detected = match zip::ZipArchive::new(&mut *reader) {
        Ok(mut archive) => match archive.by_name("manifest.json") {
            Ok(manifest) if manifest.size() <= MAX_MANIFEST_BYTES => {
                let mut bytes = Vec::with_capacity(manifest.size() as usize);
                manifest
                    .take(MAX_MANIFEST_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| {
                        NativeJobError::new(
                            "invalid-input",
                            "Recovery format manifest could not be read",
                        )
                    })?;
                serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .is_some_and(|value| {
                        value.get("format").and_then(serde_json::Value::as_str) == Some(FORMAT)
                            && value.get("version").and_then(serde_json::Value::as_u64)
                                == Some(VERSION as u64)
                    })
            }
            _ => false,
        },
        Err(_) => false,
    };
    reader.seek(SeekFrom::Start(0)).map_err(|_| {
        NativeJobError::new(
            "invalid-input",
            "Recovery format probe could not reset the source",
        )
    })?;
    Ok(detected)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified_nanos: Option<u128>,
    platform: Vec<u64>,
}

pub(super) fn capture(
    data_root: &Path,
    owned_directory: &Path,
    app_version: &str,
    job: &JobControl,
) -> Result<CapturedArchive, NativeJobError> {
    capture_with_source(
        data_root,
        owned_directory,
        app_version,
        job,
        &mut DirectCaptureSource,
    )
}

trait CaptureSource {
    fn before_enumerate(&mut self, _relative: &Path) -> std::io::Result<()> {
        Ok(())
    }

    fn before_open(&mut self, _root: &Path, _relative: &Path) {}

    fn after_open(&mut self, _root: &Path, _relative: &Path) {}

    fn read(
        &mut self,
        _relative: &Path,
        source: &mut File,
        buffer: &mut [u8],
    ) -> std::io::Result<usize> {
        source.read(buffer)
    }
}

struct DirectCaptureSource;

impl CaptureSource for DirectCaptureSource {}

fn capture_with_source(
    data_root: &Path,
    owned_directory: &Path,
    app_version: &str,
    job: &JobControl,
    capture_source: &mut impl CaptureSource,
) -> Result<CapturedArchive, NativeJobError> {
    job.start(JobPhase::ReadingSource).map_err(store_error)?;
    let archive_path = owned_directory.join("raw-recovery.zip.part");
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&archive_path)
        .map_err(|_| output_error("create recovery archive"))?;
    let mut archive = zip::ZipWriter::new(output);
    let options = FileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .large_file(true);
    let mut paths = Vec::new();
    let mut candidates = Vec::new();
    let mut main_database_observed = false;

    for relative in DATABASE_FILES {
        let relative = PathBuf::from(relative);
        match safe_kind(data_root, &relative) {
            SafeKind::Missing => {}
            SafeKind::Regular => {
                if relative == Path::new(MAIN_DATABASE) {
                    main_database_observed = true;
                }
                candidates.push(relative);
            }
            SafeKind::Unsafe => {
                main_database_observed |= relative == Path::new(MAIN_DATABASE);
                paths.push(problem(&relative, "unsafe-path"));
            }
            SafeKind::Unreadable => {
                main_database_observed |= relative == Path::new(MAIN_DATABASE);
                paths.push(problem(&relative, "unreadable"));
            }
            SafeKind::Directory => {
                main_database_observed |= relative == Path::new(MAIN_DATABASE);
                paths.push(problem(&relative, "unsafe-path"));
            }
        }
    }
    for relative in DATA_DIRECTORIES {
        collect_directory(
            data_root,
            Path::new(relative),
            &mut candidates,
            &mut paths,
            job,
            capture_source,
        )?;
    }
    candidates.sort();
    candidates.dedup();

    let total_items = candidates.len() as u64;
    job.set_progress(JobProgress {
        completed_bytes: 0,
        total_bytes: None,
        completed_items: 0,
        total_items: Some(total_items),
    })
    .map_err(store_error)?;
    let mut completed_items = 0u64;
    let mut captured_total = 0u64;
    let mut captured_files = 0u64;
    for relative in candidates {
        cancelled(job)?;
        capture_source.before_open(data_root, &relative);
        let before = match open_source(data_root, &relative) {
            Ok(source) => source,
            Err(status) => {
                paths.push(problem(&relative, status));
                completed_items += 1;
                report_progress(job, captured_total, completed_items, total_items)?;
                continue;
            }
        };
        let entry_name = encoded_entry_path(&relative);
        archive
            .start_file(&entry_name, options)
            .map_err(|_| output_error("start recovery archive entry"))?;
        let mut source = before.0;
        let initial = before.1;
        capture_source.after_open(data_root, &relative);
        let mut copied = 0u64;
        let mut status = "captured";
        let mut buffer = vec![0u8; BUFFER_BYTES];
        loop {
            cancelled(job)?;
            match capture_source.read(&relative, &mut source, &mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    archive
                        .write_all(&buffer[..read])
                        .map_err(|_| output_error("write recovery archive entry"))?;
                    copied = copied.saturating_add(read as u64);
                    captured_total = captured_total.saturating_add(read as u64);
                }
                Err(_) => {
                    status = "truncated";
                    break;
                }
            }
        }
        let changed = source.metadata().ok().map(|metadata| identity(&metadata)) != Some(initial)
            || safe_kind(data_root, &relative) != SafeKind::Regular;
        if status == "captured" && changed {
            status = "changed";
        }
        captured_files += 1;
        paths.push(ManifestPath {
            original_relative_path: display_relative(&relative),
            archive_entry_path: Some(entry_name),
            captured_bytes: copied,
            status,
        });
        completed_items += 1;
        report_progress(job, captured_total, completed_items, total_items)?;
    }

    if captured_files == 0 {
        return Err(NativeJobError::new(
            "no-recovery-source",
            "No recoverable local data source is available",
        ));
    }
    if !main_database_observed {
        paths.push(problem(Path::new(MAIN_DATABASE), "missing-during-capture"));
    }
    let partial = paths.iter().any(|path| path.status != "captured");
    let manifest = RecoveryManifest {
        format: FORMAT,
        version: VERSION,
        app_version: app_version.to_owned(),
        captured_at_millis: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64,
        status: if partial { "partial" } else { "complete" },
        paths,
    };
    archive
        .start_file("manifest.json", options)
        .map_err(|_| output_error("start recovery manifest"))?;
    serde_json::to_writer(&mut archive, &manifest)
        .map_err(|_| output_error("write recovery manifest"))?;
    let output = archive
        .finish()
        .map_err(|_| output_error("finalize recovery archive"))?;
    output
        .sync_all()
        .map_err(|_| output_error("sync recovery archive"))?;
    let bytes = output
        .metadata()
        .map_err(|_| output_error("inspect recovery archive"))?
        .len();
    drop(output);
    let sha256 = hash_file(&archive_path)?;
    Ok(CapturedArchive {
        path: archive_path,
        bytes,
        sha256,
        partial,
    })
}

pub(super) fn publish(
    captured: CapturedArchive,
    owned_directory: &Path,
    handoffs: &Path,
    destination: Option<&Path>,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    job.set_phase(JobPhase::PublishingDestination)
        .map_err(store_error)?;
    let handoff_path = if let Some(destination) = destination {
        let destination_root = destination.parent().ok_or_else(|| {
            NativeJobError::new(
                "invalid-destination",
                "Recovery export destination has no parent",
            )
        })?;
        crate::persistent_store::export::destination::write_raw_recovery_destination_controlled(
            owned_directory,
            &captured.path,
            destination_root,
            destination,
            || job.is_cancel_requested(),
            |_| {},
            || {
                if job.is_cancel_requested() {
                    Err(crate::persistent_store::export::destination::DestinationWriteError::Cancelled)
                } else {
                    Ok(())
                }
            },
        )
        .map_err(destination_error)?;
        None
    } else {
        cancelled(job)?;
        fs::create_dir_all(handoffs).map_err(|_| output_error("create recovery handoff root"))?;
        let handoff = handoffs.join(format!(
            "risunest-rescue-{}.risunest-rescue.zip",
            Uuid::new_v4()
        ));
        fs::rename(&captured.path, &handoff)
            .map_err(|_| output_error("retain recovery archive for publication"))?;
        Some(handoff.to_string_lossy().into_owned())
    };
    Ok(JobResultSummary {
        export_exclusions: None,
        revision: 0,
        source_bytes: captured.bytes,
        source_sha256: captured.sha256,
        character_count: 0,
        preset_count: 0,
        warning_codes: if captured.partial {
            vec!["source-problems".to_owned()]
        } else {
            Vec::new()
        },
        handoff_path,
        publication: None,
    })
}

fn collect_directory(
    root: &Path,
    relative: &Path,
    candidates: &mut Vec<PathBuf>,
    problems: &mut Vec<ManifestPath>,
    job: &JobControl,
    capture_source: &mut impl CaptureSource,
) -> Result<(), NativeJobError> {
    cancelled(job)?;
    match safe_kind(root, relative) {
        SafeKind::Missing => return Ok(()),
        SafeKind::Unsafe | SafeKind::Regular => {
            problems.push(problem(relative, "unsafe-path"));
            return Ok(());
        }
        SafeKind::Unreadable => {
            problems.push(problem(relative, "unreadable"));
            return Ok(());
        }
        SafeKind::Directory => {}
    }
    if capture_source.before_enumerate(relative).is_err() {
        problems.push(problem(relative, "unreadable"));
        return Ok(());
    }
    let entries = match fs::read_dir(root.join(relative)) {
        Ok(entries) => entries,
        Err(_) => {
            problems.push(problem(relative, "unreadable"));
            return Ok(());
        }
    };
    for entry in entries {
        cancelled(job)?;
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                problems.push(problem(relative, "unreadable"));
                continue;
            }
        };
        let child = relative.join(entry.file_name());
        match safe_kind(root, &child) {
            SafeKind::Regular => candidates.push(child),
            SafeKind::Directory => {
                collect_directory(root, &child, candidates, problems, job, capture_source)?
            }
            SafeKind::Missing => problems.push(problem(&child, "missing-during-capture")),
            SafeKind::Unsafe => problems.push(problem(&child, "unsafe-path")),
            SafeKind::Unreadable => problems.push(problem(&child, "unreadable")),
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SafeKind {
    Missing,
    Regular,
    Directory,
    Unsafe,
    Unreadable,
}

fn safe_kind(root: &Path, relative: &Path) -> SafeKind {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return SafeKind::Unsafe;
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return SafeKind::Missing,
            Err(_) => return SafeKind::Unreadable,
        };
        if unsafe_metadata(&metadata) {
            return SafeKind::Unsafe;
        }
    }
    match fs::symlink_metadata(&current) {
        Ok(metadata) if metadata.is_file() => SafeKind::Regular,
        Ok(metadata) if metadata.is_dir() => SafeKind::Directory,
        Ok(_) => SafeKind::Unsafe,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SafeKind::Missing,
        Err(_) => SafeKind::Unreadable,
    }
}

fn unsafe_metadata(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

fn open_source(root: &Path, relative: &Path) -> Result<(File, FileIdentity), &'static str> {
    if safe_kind(root, relative) != SafeKind::Regular {
        return Err("missing-during-capture");
    }
    let path = root.join(relative);
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "missing-during-capture"
        } else {
            "unreadable"
        }
    })?;
    let metadata = file.metadata().map_err(|_| "unreadable")?;
    if unsafe_metadata(&metadata) || !metadata.is_file() {
        return Err("unsafe-path");
    }
    Ok((file, identity(&metadata)))
}

fn identity(metadata: &Metadata) -> FileIdentity {
    let modified_nanos = metadata.modified().ok().and_then(|value| {
        value
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|value| value.as_nanos())
    });
    #[cfg(unix)]
    let platform = {
        use std::os::unix::fs::MetadataExt;
        vec![
            metadata.dev(),
            metadata.ino(),
            metadata.mtime() as u64,
            metadata.mtime_nsec() as u64,
        ]
    };
    #[cfg(windows)]
    let platform = {
        use std::os::windows::fs::MetadataExt;
        vec![
            metadata.file_attributes() as u64,
            metadata.last_write_time(),
        ]
    };
    #[cfg(not(any(unix, windows)))]
    let platform = Vec::new();
    FileIdentity {
        len: metadata.len(),
        modified_nanos,
        platform,
    }
}

fn encoded_entry_path(relative: &Path) -> String {
    let encoded = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(hex::encode(os_bytes(value))),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    format!("files/{encoded}")
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(not(any(unix, windows)))]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

fn display_relative(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn problem(path: &Path, status: &'static str) -> ManifestPath {
    ManifestPath {
        original_relative_path: display_relative(path),
        archive_entry_path: None,
        captured_bytes: 0,
        status,
    }
}

fn report_progress(
    job: &JobControl,
    completed_bytes: u64,
    completed_items: u64,
    total_items: u64,
) -> Result<(), NativeJobError> {
    job.set_progress(JobProgress {
        completed_bytes,
        total_bytes: None,
        completed_items,
        total_items: Some(total_items),
    })
    .map_err(store_error)
}

fn cancelled(job: &JobControl) -> Result<(), NativeJobError> {
    if job.is_cancel_requested() {
        Err(NativeJobError::new(
            "cancelled",
            "Recovery export was cancelled",
        ))
    } else {
        Ok(())
    }
}

fn hash_file(path: &Path) -> Result<String, NativeJobError> {
    let mut file = File::open(path).map_err(|_| output_error("open finalized recovery archive"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| output_error("hash recovery archive"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn destination_error(
    error: crate::persistent_store::export::destination::DestinationWriteError,
) -> NativeJobError {
    match error {
        crate::persistent_store::export::destination::DestinationWriteError::Cancelled => {
            NativeJobError::new(
                "cancelled",
                "Recovery export was cancelled before publication",
            )
        }
        crate::persistent_store::export::destination::DestinationWriteError::InvalidDestination => {
            NativeJobError::new(
                "invalid-destination",
                "Recovery export destination is invalid",
            )
        }
        _ => NativeJobError::new(
            "destination-write-failed",
            "Recovery archive could not be published",
        ),
    }
}

fn output_error(operation: &'static str) -> NativeJobError {
    NativeJobError::new("archive-output-failed", operation)
}

fn store_error(error: String) -> NativeJobError {
    NativeJobError::new("store-error", error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    struct SyntheticSourceProblems {
        truncated_once: bool,
    }

    impl CaptureSource for SyntheticSourceProblems {
        fn before_enumerate(&mut self, relative: &Path) -> std::io::Result<()> {
            if relative == Path::new("blobstore") {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "synthetic enumeration failure",
                ))
            } else {
                Ok(())
            }
        }

        fn before_open(&mut self, root: &Path, relative: &Path) {
            if relative == Path::new("assets/disappeared.bin") {
                fs::remove_file(root.join(relative)).unwrap();
            }
        }

        fn after_open(&mut self, root: &Path, relative: &Path) {
            if relative == Path::new("assets/changed.bin") {
                OpenOptions::new()
                    .append(true)
                    .open(root.join(relative))
                    .unwrap()
                    .write_all(b"!")
                    .unwrap();
            }
        }

        fn read(
            &mut self,
            relative: &Path,
            source: &mut File,
            buffer: &mut [u8],
        ) -> std::io::Result<usize> {
            if relative == Path::new("assets/truncated.bin") {
                if self.truncated_once {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "synthetic read failure",
                    ));
                }
                self.truncated_once = true;
                return source.read(&mut buffer[..3]);
            }
            source.read(buffer)
        }
    }

    fn job() -> std::sync::Arc<JobControl> {
        super::super::JobRegistry::default()
            .create(super::super::JobKind::ExportRawRecovery)
            .unwrap()
    }

    #[test]
    fn invalid_database_and_allowed_files_round_trip_without_opening_sqlite() {
        let root = tempfile::tempdir().unwrap();
        let owned = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("persistent")).unwrap();
        fs::create_dir_all(root.path().join("assets/nested")).unwrap();
        fs::write(root.path().join(MAIN_DATABASE), b"not sqlite").unwrap();
        fs::write(root.path().join("assets/nested/image.bin"), [0, 1, 2, 255]).unwrap();
        fs::write(root.path().join("credentials.json"), b"excluded").unwrap();

        let captured = capture(root.path(), owned.path(), "1.2.3", &job()).unwrap();
        let mut renamed = File::open(&captured.path).unwrap();
        assert!(is_raw_recovery_archive(&mut renamed).unwrap());
        let mut zip = zip::ZipArchive::new(File::open(captured.path).unwrap()).unwrap();
        let database = encoded_entry_path(Path::new(MAIN_DATABASE));
        let asset = encoded_entry_path(Path::new("assets/nested/image.bin"));
        let mut bytes = Vec::new();
        zip.by_name(&database)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"not sqlite");
        bytes.clear();
        zip.by_name(&asset)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, [0, 1, 2, 255]);
        assert!(zip.by_name("credentials.json").is_err());
    }

    #[test]
    fn missing_main_database_is_partial_but_empty_sources_fail() {
        let root = tempfile::tempdir().unwrap();
        let owned = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("assets")).unwrap();
        fs::write(root.path().join("assets/only.bin"), b"recoverable").unwrap();
        let captured = capture(root.path(), owned.path(), "1.2.3", &job()).unwrap();
        assert!(captured.partial);

        let empty = tempfile::tempdir().unwrap();
        let empty_owned = tempfile::tempdir().unwrap();
        assert_eq!(
            capture(empty.path(), empty_owned.path(), "1.2.3", &job())
                .unwrap_err()
                .code,
            "no-recovery-source"
        );
    }

    #[test]
    fn source_problems_finalize_extractable_entries_and_bounded_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        let owned = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("persistent")).unwrap();
        fs::create_dir_all(root.path().join("assets")).unwrap();
        fs::create_dir_all(root.path().join("blobstore")).unwrap();
        fs::write(root.path().join(MAIN_DATABASE), b"database bytes").unwrap();
        fs::write(root.path().join("assets/changed.bin"), b"old").unwrap();
        fs::write(root.path().join("assets/disappeared.bin"), b"gone").unwrap();
        fs::write(root.path().join("assets/truncated.bin"), b"abcdef").unwrap();
        fs::write(root.path().join("blobstore/unknown.bin"), b"not enumerated").unwrap();
        let captured = capture_with_source(
            root.path(),
            owned.path(),
            "1.2.3",
            &job(),
            &mut SyntheticSourceProblems {
                truncated_once: false,
            },
        )
        .unwrap();
        assert!(captured.partial);

        let mut zip = zip::ZipArchive::new(File::open(captured.path).unwrap()).unwrap();
        let mut truncated = Vec::new();
        zip.by_name(&encoded_entry_path(Path::new("assets/truncated.bin")))
            .unwrap()
            .read_to_end(&mut truncated)
            .unwrap();
        assert_eq!(truncated, b"abc");
        let mut manifest = String::new();
        zip.by_name("manifest.json")
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(manifest["status"], "partial");
        let paths = manifest["paths"].as_array().unwrap();
        let entry = |relative: &str| {
            paths
                .iter()
                .find(|entry| entry["originalRelativePath"] == relative)
                .unwrap()
        };
        assert_eq!(entry("assets/changed.bin")["status"], "changed");
        assert_eq!(entry("assets/changed.bin")["capturedBytes"], 4);
        assert_eq!(
            entry("assets/disappeared.bin")["status"],
            "missing-during-capture"
        );
        assert_eq!(entry("assets/disappeared.bin")["capturedBytes"], 0);
        assert_eq!(entry("assets/truncated.bin")["status"], "truncated");
        assert_eq!(entry("assets/truncated.bin")["capturedBytes"], 3);
        assert_eq!(entry("blobstore")["status"], "unreadable");
        assert!(paths
            .iter()
            .all(|entry| entry["originalRelativePath"] != "blobstore/unknown.bin"));
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_diagnostics_and_are_not_followed() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let owned = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("persistent")).unwrap();
        fs::create_dir_all(root.path().join("assets")).unwrap();
        fs::write(root.path().join(MAIN_DATABASE), b"db").unwrap();
        fs::write(root.path().join("outside"), b"secret").unwrap();
        symlink(root.path().join("outside"), root.path().join("assets/link")).unwrap();
        let captured = capture(root.path(), owned.path(), "1.2.3", &job()).unwrap();
        assert!(captured.partial);
    }
}
