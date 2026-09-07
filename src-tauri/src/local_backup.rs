use crate::asset_repository::{PayloadCas, PreparedPayload};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const DATABASE_ENTRY_NAME: &str = "database.risudat";
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_NAME_BYTES: u32 = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LocalBackupErrorCode {
    InvalidPath,
    InvalidUtf8,
    DuplicatePath,
    DuplicateDatabase,
    MissingDatabase,
    LengthOverflow,
    TruncatedInput,
    SourceLengthMismatch,
    Cancelled,
    Io,
    DatabaseRestore,
    UnsupportedEncryption,
    UnsupportedFormat,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LocalBackupError {
    pub(crate) code: LocalBackupErrorCode,
    pub(crate) message: String,
}

impl LocalBackupError {
    pub(crate) fn new(code: LocalBackupErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_ERROR_BYTES {
            let mut end = MAX_ERROR_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        Self { code, message }
    }

    pub(crate) fn io(error: io::Error) -> Self {
        Self::new(LocalBackupErrorCode::Io, error.to_string())
    }

    pub(crate) fn database_restore(message: impl Into<String>) -> Self {
        Self::new(LocalBackupErrorCode::DatabaseRestore, message)
    }
}

impl std::fmt::Display for LocalBackupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for LocalBackupError {}

pub(crate) trait CancellationProbe {
    fn is_cancelled(&self) -> bool;
}

pub(crate) struct NeverCancelled;

impl CancellationProbe for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// Observes archive parsing so a job can report entry-level progress. The
/// methods take `&self` so one object can double as the cancellation probe.
pub(crate) trait LocalBackupParseObserver {
    fn entry_started(&self, _logical_name: &str, _byte_length: u64, _index: usize) {}
    fn bytes_read(&self, _total_read: u64) {}
    fn entries_complete(&self, _count: usize) {}
}

pub(crate) struct NoopParseObserver;

impl LocalBackupParseObserver for NoopParseObserver {}

pub(crate) struct AtomicCancellation {
    cancelled: Arc<AtomicBool>,
}

impl AtomicCancellation {
    pub(crate) fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled }
    }
}

impl CancellationProbe for AtomicCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
pub(crate) struct StagedLocalBackupEntry {
    pub(crate) logical_name: String,
    pub(crate) byte_length: u64,
    pub(crate) sha256: String,
    pub(crate) staged_path: Option<PathBuf>,
    pub(crate) immutable_object: Option<PreparedPayload>,
}

pub(crate) trait StrictLocalBackupDatabaseRestore {
    fn restore_database(
        &mut self,
        entry: &StagedLocalBackupEntry,
        entries: &[StagedLocalBackupEntry],
    ) -> Result<(), LocalBackupError>;
}

pub(crate) enum PayloadTarget<'a> {
    JobStaging,
    ImmutableCas(&'a PayloadCas),
}

#[derive(Debug)]
pub(crate) struct LegacyLocalBackupParseReport {
    pub(crate) source_bytes: u64,
    pub(crate) source_sha256: String,
    pub(crate) entries: Vec<StagedLocalBackupEntry>,
}

pub(crate) fn parse_legacy_local_backup_v1(
    reader: &mut impl Read,
    job_staging_root: &Path,
    payload_target: PayloadTarget<'_>,
    database_restore: &mut dyn StrictLocalBackupDatabaseRestore,
    cancellation: &dyn CancellationProbe,
) -> Result<LegacyLocalBackupParseReport, LocalBackupError> {
    parse_legacy_local_backup_v1_observed(
        reader,
        job_staging_root,
        payload_target,
        database_restore,
        cancellation,
        &NoopParseObserver,
    )
}

pub(crate) fn parse_legacy_local_backup_v1_observed(
    reader: &mut impl Read,
    job_staging_root: &Path,
    payload_target: PayloadTarget<'_>,
    database_restore: &mut dyn StrictLocalBackupDatabaseRestore,
    cancellation: &dyn CancellationProbe,
    observer: &dyn LocalBackupParseObserver,
) -> Result<LegacyLocalBackupParseReport, LocalBackupError> {
    check_cancelled(cancellation)?;
    let staging_directory = prepare_staging_directory(job_staging_root)?;
    let mut staging_ownership = ParseStagingOwnership::default();
    let mut source = TrackedReader::new(reader);
    let mut entries = Vec::new();
    let mut normalized_names = HashSet::new();
    let mut database_index = None;

    loop {
        let Some(name_length) = read_entry_name_length(&mut source, cancellation)? else {
            break;
        };
        if name_length > MAX_NAME_BYTES {
            return Err(name_length_error());
        }
        if name_length == 0 {
            return Err(LocalBackupError::new(
                LocalBackupErrorCode::InvalidPath,
                "legacy backup entry name length is invalid",
            ));
        }
        let mut name_bytes = vec![0_u8; name_length as usize];
        read_exact_checked(
            &mut source,
            &mut name_bytes,
            cancellation,
            "truncated legacy backup entry name",
        )?;
        let name = std::str::from_utf8(&name_bytes).map_err(|_| {
            LocalBackupError::new(
                LocalBackupErrorCode::InvalidUtf8,
                "legacy backup entry name is not valid UTF-8",
            )
        })?;
        let logical_name = normalize_logical_name(name)?;
        let is_database = logical_name == DATABASE_ENTRY_NAME;
        if is_database && database_index.is_some() {
            return Err(LocalBackupError::new(
                LocalBackupErrorCode::DuplicateDatabase,
                "legacy backup contains more than one database entry",
            ));
        }
        if !normalized_names.insert(logical_name.clone()) {
            return Err(LocalBackupError::new(
                LocalBackupErrorCode::DuplicatePath,
                format!("duplicate normalized legacy backup path: {logical_name}"),
            ));
        }

        let mut length_bytes = [0_u8; 4];
        read_exact_checked(
            &mut source,
            &mut length_bytes,
            cancellation,
            "truncated legacy backup entry length",
        )?;
        let byte_length = u32::from_le_bytes(length_bytes) as u64;
        observer.entry_started(&logical_name, byte_length, entries.len());
        let staged = if is_database || matches!(payload_target, PayloadTarget::JobStaging) {
            stage_entry(
                &mut source,
                &staging_directory,
                logical_name,
                byte_length,
                cancellation,
                observer,
                &mut staging_ownership,
            )?
        } else {
            let PayloadTarget::ImmutableCas(cas) = payload_target else {
                unreachable!("payload target was matched above")
            };
            prepare_entry_in_cas(
                &mut source,
                cas,
                logical_name,
                byte_length,
                cancellation,
                observer,
            )?
        };
        if is_database {
            database_index = Some(entries.len());
        }
        entries.push(staged);
    }

    let database_index = database_index.ok_or_else(|| {
        LocalBackupError::new(
            LocalBackupErrorCode::MissingDatabase,
            "legacy backup does not contain database.risudat",
        )
    })?;
    check_cancelled(cancellation)?;
    observer.entries_complete(entries.len());
    database_restore.restore_database(&entries[database_index], &entries)?;
    staging_ownership.release();

    Ok(LegacyLocalBackupParseReport {
        source_bytes: source.bytes_read,
        source_sha256: hex::encode(source.hasher.finalize()),
        entries,
    })
}

fn prepare_staging_directory(job_staging_root: &Path) -> Result<PathBuf, LocalBackupError> {
    let root = fs::canonicalize(job_staging_root).map_err(LocalBackupError::io)?;
    if !root.is_dir() {
        return Err(LocalBackupError::new(
            LocalBackupErrorCode::InvalidPath,
            "job staging root is not a directory",
        ));
    }
    let staging = root.join("local-backup-v1");
    fs::create_dir_all(&staging).map_err(LocalBackupError::io)?;
    let staging = fs::canonicalize(staging).map_err(LocalBackupError::io)?;
    if !staging.starts_with(&root) {
        return Err(LocalBackupError::new(
            LocalBackupErrorCode::InvalidPath,
            "job staging directory escapes its owned root",
        ));
    }
    Ok(staging)
}

fn stage_entry(
    source: &mut TrackedReader<'_, impl Read>,
    staging_directory: &Path,
    logical_name: String,
    byte_length: u64,
    cancellation: &dyn CancellationProbe,
    observer: &dyn LocalBackupParseObserver,
    staging_ownership: &mut ParseStagingOwnership,
) -> Result<StagedLocalBackupEntry, LocalBackupError> {
    let staged_path = staging_directory.join(format!("{}.entry", uuid::Uuid::new_v4()));
    let mut guard = IncompleteFile::new(staged_path.clone());
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&staged_path)
        .map_err(LocalBackupError::io)?;
    let mut remaining = byte_length;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        check_cancelled(cancellation)?;
        let wanted = usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64))
            .expect("bounded buffer length");
        let read = source
            .read(&mut buffer[..wanted])
            .map_err(LocalBackupError::io)?;
        if read == 0 {
            return Err(LocalBackupError::new(
                LocalBackupErrorCode::TruncatedInput,
                format!("truncated legacy backup entry body: {logical_name}"),
            ));
        }
        check_cancelled(cancellation)?;
        output
            .write_all(&buffer[..read])
            .map_err(LocalBackupError::io)?;
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
        observer.bytes_read(source.bytes_read);
    }
    check_cancelled(cancellation)?;
    output.flush().map_err(LocalBackupError::io)?;
    output.sync_all().map_err(LocalBackupError::io)?;
    drop(output);
    staging_ownership.track(staged_path.clone());
    guard.keep();
    Ok(StagedLocalBackupEntry {
        logical_name,
        byte_length,
        sha256: hex::encode(hasher.finalize()),
        staged_path: Some(staged_path),
        immutable_object: None,
    })
}

fn prepare_entry_in_cas(
    source: &mut TrackedReader<'_, impl Read>,
    cas: &PayloadCas,
    logical_name: String,
    byte_length: u64,
    cancellation: &dyn CancellationProbe,
    observer: &dyn LocalBackupParseObserver,
) -> Result<StagedLocalBackupEntry, LocalBackupError> {
    check_cancelled(cancellation)?;
    let mut entry_reader = DeclaredEntryReader {
        source,
        remaining: byte_length,
        cancellation,
        observer,
    };
    let prepared = cas.prepare_reader(&mut entry_reader).map_err(|error| {
        if cancellation.is_cancelled() {
            cancelled_error()
        } else if error.kind() == io::ErrorKind::UnexpectedEof {
            LocalBackupError::new(
                LocalBackupErrorCode::TruncatedInput,
                format!("truncated legacy backup entry body: {logical_name}"),
            )
        } else {
            LocalBackupError::io(error)
        }
    })?;
    check_cancelled(cancellation)?;
    if prepared.byte_size != byte_length {
        return Err(LocalBackupError::new(
            LocalBackupErrorCode::Io,
            "immutable payload result does not match its declared entry length",
        ));
    }
    Ok(StagedLocalBackupEntry {
        logical_name,
        byte_length,
        sha256: prepared.content_hash.clone(),
        staged_path: None,
        immutable_object: Some(prepared),
    })
}

struct DeclaredEntryReader<'a, 'b, R> {
    source: &'a mut TrackedReader<'b, R>,
    remaining: u64,
    cancellation: &'a dyn CancellationProbe,
    observer: &'a dyn LocalBackupParseObserver,
}

impl<R: Read> Read for DeclaredEntryReader<'_, '_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "local backup operation cancelled",
            ));
        }
        let wanted = usize::try_from(self.remaining.min(buffer.len() as u64))
            .expect("bounded reader length");
        let read = self.source.read(&mut buffer[..wanted])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated legacy backup entry body",
            ));
        }
        self.remaining -= read as u64;
        self.observer.bytes_read(self.source.bytes_read);
        Ok(read)
    }
}

struct IncompleteFile {
    path: PathBuf,
    keep: bool,
}

impl IncompleteFile {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for IncompleteFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Default)]
struct ParseStagingOwnership {
    paths: Vec<PathBuf>,
}

impl ParseStagingOwnership {
    fn track(&mut self, path: PathBuf) {
        self.paths.push(path);
    }

    fn release(&mut self) {
        self.paths.clear();
    }
}

impl Drop for ParseStagingOwnership {
    fn drop(&mut self) {
        for path in self.paths.iter().rev() {
            let _ = fs::remove_file(path);
        }
    }
}

struct TrackedReader<'a, R> {
    inner: &'a mut R,
    bytes_read: u64,
    hasher: Sha256,
}

impl<'a, R> TrackedReader<'a, R> {
    fn new(inner: &'a mut R) -> Self {
        Self {
            inner,
            bytes_read: 0,
            hasher: Sha256::new(),
        }
    }
}

impl<R: Read> Read for TrackedReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.bytes_read = self
            .bytes_read
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "source size overflow"))?;
        self.hasher.update(&buffer[..read]);
        Ok(read)
    }
}

fn read_entry_name_length(
    reader: &mut impl Read,
    cancellation: &dyn CancellationProbe,
) -> Result<Option<u32>, LocalBackupError> {
    let mut bytes = [0_u8; 4];
    let mut offset = 0;
    while offset < bytes.len() {
        check_cancelled(cancellation)?;
        let read = reader
            .read(&mut bytes[offset..])
            .map_err(LocalBackupError::io)?;
        if read == 0 {
            return if offset == 0 {
                Ok(None)
            } else {
                Err(LocalBackupError::new(
                    LocalBackupErrorCode::TruncatedInput,
                    "truncated legacy backup entry header",
                ))
            };
        }
        offset += read;
    }
    Ok(Some(u32::from_le_bytes(bytes)))
}

fn read_exact_checked(
    reader: &mut impl Read,
    bytes: &mut [u8],
    cancellation: &dyn CancellationProbe,
    truncated_message: &'static str,
) -> Result<(), LocalBackupError> {
    let mut offset = 0;
    while offset < bytes.len() {
        check_cancelled(cancellation)?;
        let read = reader
            .read(&mut bytes[offset..])
            .map_err(LocalBackupError::io)?;
        if read == 0 {
            return Err(LocalBackupError::new(
                LocalBackupErrorCode::TruncatedInput,
                truncated_message,
            ));
        }
        offset += read;
    }
    Ok(())
}

fn normalize_logical_name(name: &str) -> Result<String, LocalBackupError> {
    if name.len() > MAX_NAME_BYTES as usize {
        return Err(name_length_error());
    }
    if name.contains('\0') {
        return invalid_path("legacy backup path contains NUL");
    }
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return invalid_path("legacy backup path is absolute");
    }
    let mut components = normalized.split('/');
    let Some(first) = components.next() else {
        return invalid_path("legacy backup path is empty");
    };
    if is_windows_drive_prefix(first) {
        return invalid_path("legacy backup path has a drive prefix");
    }
    if first.is_empty() || first == "." || first == ".." {
        return invalid_path("legacy backup path has an invalid component");
    }
    for component in components {
        if component.is_empty() || component == "." || component == ".." {
            return invalid_path("legacy backup path has an invalid component");
        }
    }
    Ok(normalized)
}

fn name_length_error() -> LocalBackupError {
    LocalBackupError::new(
        LocalBackupErrorCode::LengthOverflow,
        "legacy backup entry name exceeds the bounded v1 name limit",
    )
}

fn is_windows_drive_prefix(component: &str) -> bool {
    let bytes = component.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn invalid_path<T>(message: &'static str) -> Result<T, LocalBackupError> {
    Err(LocalBackupError::new(
        LocalBackupErrorCode::InvalidPath,
        message,
    ))
}

fn check_cancelled(cancellation: &dyn CancellationProbe) -> Result<(), LocalBackupError> {
    if cancellation.is_cancelled() {
        Err(cancelled_error())
    } else {
        Ok(())
    }
}

fn cancelled_error() -> LocalBackupError {
    LocalBackupError::new(
        LocalBackupErrorCode::Cancelled,
        "local backup operation cancelled",
    )
}

pub(crate) fn validate_v1_entry_length(length: u64) -> Result<u32, LocalBackupError> {
    u32::try_from(length).map_err(|_| {
        LocalBackupError::new(
            LocalBackupErrorCode::LengthOverflow,
            "legacy backup v1 entry exceeds the u32 wire limit",
        )
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LegacyBackupWriteSource {
    File(PathBuf),
    MissingReference,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LegacyBackupWriteEntry {
    pub(crate) logical_name: String,
    pub(crate) source: LegacyBackupWriteSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LegacyBackupWriteReport {
    pub(crate) written_entries: u64,
    pub(crate) source_payload_bytes: u64,
    pub(crate) archive_bytes: u64,
    pub(crate) missing_references: Vec<String>,
}

impl LegacyBackupWriteReport {
    pub(crate) fn is_complete(&self) -> bool {
        self.missing_references.is_empty()
    }
}

struct PreparedWriteEntry {
    logical_name: String,
    source_path: PathBuf,
    byte_length: u32,
}

pub(crate) fn write_legacy_local_backup_v1(
    output: &mut impl Write,
    entries: &[LegacyBackupWriteEntry],
    cancellation: &dyn CancellationProbe,
) -> Result<LegacyBackupWriteReport, LocalBackupError> {
    check_cancelled(cancellation)?;
    let (prepared, missing_references) = prepare_write_entries(entries, cancellation)?;
    let mut report = LegacyBackupWriteReport {
        written_entries: 0,
        source_payload_bytes: 0,
        archive_bytes: 0,
        missing_references,
    };
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    for entry in prepared {
        check_cancelled(cancellation)?;
        let name_bytes = entry.logical_name.as_bytes();
        let name_length = validate_v1_entry_length(name_bytes.len() as u64)?;
        write_all_checked(output, &name_length.to_le_bytes(), cancellation)?;
        write_all_checked(output, name_bytes, cancellation)?;
        write_all_checked(output, &entry.byte_length.to_le_bytes(), cancellation)?;

        let mut source = File::open(&entry.source_path).map_err(LocalBackupError::io)?;
        let mut remaining = entry.byte_length as u64;
        while remaining > 0 {
            check_cancelled(cancellation)?;
            let wanted = usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64))
                .expect("bounded buffer length");
            let read = source
                .read(&mut buffer[..wanted])
                .map_err(LocalBackupError::io)?;
            if read == 0 {
                return Err(LocalBackupError::new(
                    LocalBackupErrorCode::SourceLengthMismatch,
                    format!("legacy backup source shrank: {}", entry.logical_name),
                ));
            }
            write_all_checked(output, &buffer[..read], cancellation)?;
            remaining -= read as u64;
        }
        let mut extra = [0_u8; 1];
        if source.read(&mut extra).map_err(LocalBackupError::io)? != 0 {
            return Err(LocalBackupError::new(
                LocalBackupErrorCode::SourceLengthMismatch,
                format!("legacy backup source grew: {}", entry.logical_name),
            ));
        }
        report.written_entries += 1;
        report.source_payload_bytes = report
            .source_payload_bytes
            .checked_add(entry.byte_length as u64)
            .ok_or_else(|| {
                LocalBackupError::new(
                    LocalBackupErrorCode::LengthOverflow,
                    "legacy backup aggregate payload length overflow",
                )
            })?;
        report.archive_bytes = report
            .archive_bytes
            .checked_add(8_u64 + name_bytes.len() as u64 + entry.byte_length as u64)
            .ok_or_else(|| {
                LocalBackupError::new(
                    LocalBackupErrorCode::LengthOverflow,
                    "legacy backup aggregate archive length overflow",
                )
            })?;
    }
    Ok(report)
}

fn prepare_write_entries(
    entries: &[LegacyBackupWriteEntry],
    cancellation: &dyn CancellationProbe,
) -> Result<(Vec<PreparedWriteEntry>, Vec<String>), LocalBackupError> {
    check_cancelled(cancellation)?;
    let mut normalized_names = HashSet::new();
    let mut prepared = Vec::new();
    let mut missing_references = Vec::new();
    let mut database_seen = false;
    for entry in entries {
        check_cancelled(cancellation)?;
        let logical_name = normalize_logical_name(&entry.logical_name)?;
        check_cancelled(cancellation)?;
        let is_database = logical_name == DATABASE_ENTRY_NAME;
        if is_database && database_seen {
            return Err(LocalBackupError::new(
                LocalBackupErrorCode::DuplicateDatabase,
                "legacy backup writer received more than one database entry",
            ));
        }
        if !normalized_names.insert(logical_name.clone()) {
            return Err(LocalBackupError::new(
                LocalBackupErrorCode::DuplicatePath,
                format!("duplicate normalized legacy backup path: {logical_name}"),
            ));
        }
        match &entry.source {
            LegacyBackupWriteSource::MissingReference => {
                if is_database {
                    return Err(LocalBackupError::new(
                        LocalBackupErrorCode::MissingDatabase,
                        "legacy backup database source is missing",
                    ));
                }
                missing_references.push(logical_name);
            }
            LegacyBackupWriteSource::File(path) => {
                check_cancelled(cancellation)?;
                let byte_length = validate_v1_entry_length(
                    fs::metadata(path).map_err(LocalBackupError::io)?.len(),
                )?;
                check_cancelled(cancellation)?;
                if is_database {
                    database_seen = true;
                }
                prepared.push(PreparedWriteEntry {
                    logical_name,
                    source_path: path.clone(),
                    byte_length,
                });
            }
        }
    }
    if !database_seen {
        return Err(LocalBackupError::new(
            LocalBackupErrorCode::MissingDatabase,
            "legacy backup writer requires database.risudat",
        ));
    }
    Ok((prepared, missing_references))
}

fn write_all_checked(
    output: &mut impl Write,
    bytes: &[u8],
    cancellation: &dyn CancellationProbe,
) -> Result<(), LocalBackupError> {
    let mut offset = 0;
    while offset < bytes.len() {
        check_cancelled(cancellation)?;
        let written = output
            .write(&bytes[offset..])
            .map_err(LocalBackupError::io)?;
        if written == 0 {
            return Err(LocalBackupError::io(io::Error::new(
                io::ErrorKind::WriteZero,
                "legacy backup destination stopped accepting bytes",
            )));
        }
        offset += written;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_repository::PayloadCas;
    use std::{
        fs,
        io::{self, Cursor, Read},
        path::Path,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };

    fn entry(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    fn archive(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        entries
            .iter()
            .flat_map(|(name, data)| entry(name, data))
            .collect()
    }

    #[derive(Default)]
    struct RestoreSpy {
        calls: usize,
        bytes: Vec<u8>,
        name: String,
    }

    impl StrictLocalBackupDatabaseRestore for RestoreSpy {
        fn restore_database(
            &mut self,
            entry: &StagedLocalBackupEntry,
            _entries: &[StagedLocalBackupEntry],
        ) -> Result<(), LocalBackupError> {
            self.calls += 1;
            self.bytes = fs::read(
                entry
                    .staged_path
                    .as_ref()
                    .expect("database entry uses job staging"),
            )
            .map_err(LocalBackupError::io)?;
            self.name = entry.logical_name.clone();
            Ok(())
        }
    }

    struct ShortReader<R> {
        inner: R,
        maximum: usize,
    }

    impl<R: Read> Read for ShortReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let maximum = buffer.len().min(self.maximum);
            self.inner.read(&mut buffer[..maximum])
        }
    }

    fn parse_staging(
        bytes: &[u8],
        restore: &mut dyn StrictLocalBackupDatabaseRestore,
    ) -> Result<LegacyLocalBackupParseReport, LocalBackupError> {
        let directory = tempfile::tempdir().expect("temporary job root");
        parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes),
            directory.path(),
            PayloadTarget::JobStaging,
            restore,
            &NeverCancelled,
        )
    }

    #[test]
    fn parses_arbitrary_boundaries_and_defers_database_restore_until_clean_eof() {
        let bytes = archive(&[
            (b"nested/images/avatar.PNG", b"opaque-asset"),
            (b"database.risudat", b"strict-database"),
        ]);
        let directory = tempfile::tempdir().expect("temporary job root");
        let mut reader = ShortReader {
            inner: Cursor::new(bytes.clone()),
            maximum: 1,
        };
        let mut restore = RestoreSpy::default();

        let report = parse_legacy_local_backup_v1(
            &mut reader,
            directory.path(),
            PayloadTarget::JobStaging,
            &mut restore,
            &NeverCancelled,
        )
        .expect("parse backup");

        assert_eq!(restore.calls, 1);
        assert_eq!(restore.name, "database.risudat");
        assert_eq!(restore.bytes, b"strict-database");
        assert_eq!(report.source_bytes, bytes.len() as u64);
        assert_eq!(report.source_sha256, hex::encode(Sha256::digest(&bytes)));
        assert_eq!(report.entries.len(), 2);
        assert_eq!(report.entries[0].byte_length, b"opaque-asset".len() as u64);
        assert_eq!(report.entries[0].logical_name, "nested/images/avatar.PNG");
        assert_eq!(
            fs::read(
                report.entries[0]
                    .staged_path
                    .as_ref()
                    .expect("job-staged payload"),
            )
            .expect("staged payload"),
            b"opaque-asset"
        );
    }

    #[test]
    fn normalizes_backslashes_and_rejects_duplicate_logical_paths() {
        let bytes = archive(&[
            (b"nested\\asset.bin", b"first"),
            (b"nested/asset.bin", b"second"),
            (b"database.risudat", b"db"),
        ]);
        let mut restore = RestoreSpy::default();

        let error = parse_staging(&bytes, &mut restore).expect_err("duplicate must fail");

        assert_eq!(error.code, LocalBackupErrorCode::DuplicatePath);
        assert_eq!(restore.calls, 0);
    }

    #[test]
    fn rejects_traversal_absolute_drive_and_empty_path_components() {
        for name in [
            b"../escape.bin".as_slice(),
            b"nested/../escape.bin".as_slice(),
            b"/absolute.bin".as_slice(),
            b"C:/drive.bin".as_slice(),
            b"C:drive.bin".as_slice(),
            b"nested//asset.bin".as_slice(),
            b"nested/./asset.bin".as_slice(),
        ] {
            let bytes = archive(&[(name, b"payload"), (b"database.risudat", b"db")]);
            let mut restore = RestoreSpy::default();
            let error = parse_staging(&bytes, &mut restore).expect_err("path must fail");
            assert_eq!(error.code, LocalBackupErrorCode::InvalidPath, "{name:?}");
            assert_eq!(restore.calls, 0);
        }
    }

    #[test]
    fn rejects_invalid_utf8_names_without_restoring_the_database() {
        let bytes = archive(&[(b"database.risudat", b"db"), (&[0xff], b"payload")]);
        let mut restore = RestoreSpy::default();

        let error = parse_staging(&bytes, &mut restore).expect_err("UTF-8 must fail");

        assert_eq!(error.code, LocalBackupErrorCode::InvalidUtf8);
        assert_eq!(restore.calls, 0);
    }

    #[test]
    fn requires_exactly_one_root_database_entry() {
        let mut missing_restore = RestoreSpy::default();
        let missing = parse_staging(
            &archive(&[(b"asset.bin", b"payload")]),
            &mut missing_restore,
        )
        .expect_err("database is required");
        assert_eq!(missing.code, LocalBackupErrorCode::MissingDatabase);

        let mut duplicate_restore = RestoreSpy::default();
        let duplicate = parse_staging(
            &archive(&[
                (b"database.risudat", b"db-one"),
                (b"database.risudat", b"db-two"),
            ]),
            &mut duplicate_restore,
        )
        .expect_err("second database must fail");
        assert_eq!(duplicate.code, LocalBackupErrorCode::DuplicateDatabase);
        assert_eq!(missing_restore.calls, 0);
        assert_eq!(duplicate_restore.calls, 0);
    }

    #[test]
    fn propagates_the_injected_strict_database_restore_failure() {
        struct FailingRestore;
        impl StrictLocalBackupDatabaseRestore for FailingRestore {
            fn restore_database(
                &mut self,
                _entry: &StagedLocalBackupEntry,
                _entries: &[StagedLocalBackupEntry],
            ) -> Result<(), LocalBackupError> {
                Err(LocalBackupError::database_restore(
                    "strict database rejection",
                ))
            }
        }
        let mut restore = FailingRestore;

        let error = parse_staging(
            &archive(&[(b"database.risudat", b"invalid-db")]),
            &mut restore,
        )
        .expect_err("strict restore rejection must propagate");

        assert_eq!(error.code, LocalBackupErrorCode::DatabaseRestore);
        assert_eq!(error.message, "strict database rejection");
    }

    #[test]
    fn truncated_tail_removes_every_completed_job_staging_file() {
        let large = vec![7_u8; COPY_BUFFER_BYTES * 3];
        let mut bytes = archive(&[
            (b"assets/one.bin", large.as_slice()),
            (b"assets/two.bin", large.as_slice()),
            (b"assets/three.bin", large.as_slice()),
            (b"database.risudat", b"db"),
        ]);
        bytes.extend_from_slice(&[1, 0]);
        let directory = tempfile::tempdir().expect("temporary job root");
        let staging = directory.path().join("local-backup-v1");
        let mut restore = RestoreSpy::default();

        let error = parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes),
            directory.path(),
            PayloadTarget::JobStaging,
            &mut restore,
            &NeverCancelled,
        )
        .expect_err("truncated tail must fail");

        assert_eq!(error.code, LocalBackupErrorCode::TruncatedInput);
        assert_eq!(restore.calls, 0);
        assert_eq!(
            fs::read_dir(&staging).expect("read job staging").count(),
            0,
            "all completed parser-owned files must be removed",
        );
        fs::remove_dir(staging).expect("staging directory has no leaked file descriptors");
    }

    #[test]
    fn database_restore_failure_removes_every_completed_job_staging_file() {
        struct FailingRestore;
        impl StrictLocalBackupDatabaseRestore for FailingRestore {
            fn restore_database(
                &mut self,
                _entry: &StagedLocalBackupEntry,
                _entries: &[StagedLocalBackupEntry],
            ) -> Result<(), LocalBackupError> {
                Err(LocalBackupError::database_restore(
                    "strict database rejection",
                ))
            }
        }

        let directory = tempfile::tempdir().expect("temporary job root");
        let staging = directory.path().join("local-backup-v1");
        let mut restore = FailingRestore;
        let bytes = archive(&[
            (b"assets/one.bin", &[1_u8; 128 * 1024]),
            (b"assets/two.bin", &[2_u8; 128 * 1024]),
            (b"database.risudat", b"invalid-db"),
        ]);

        let error = parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes),
            directory.path(),
            PayloadTarget::JobStaging,
            &mut restore,
            &NeverCancelled,
        )
        .expect_err("strict restore rejection must fail");

        assert_eq!(error.code, LocalBackupErrorCode::DatabaseRestore);
        assert_eq!(
            fs::read_dir(&staging).expect("read job staging").count(),
            0,
            "restore failure must remove every parser-owned file",
        );
        fs::remove_dir(staging).expect("staging directory has no leaked file descriptors");
    }

    #[test]
    fn bounds_error_messages_without_splitting_utf8() {
        let error = LocalBackupError::new(LocalBackupErrorCode::Io, "한".repeat(400));

        assert!(error.message.len() <= 512);
        assert!(error.message.is_char_boundary(error.message.len()));
    }

    #[test]
    fn rejects_every_truncated_parser_state_even_after_a_complete_database() {
        let complete_database = entry(b"database.risudat", b"db");
        let mut cases = vec![vec![1], vec![1, 0], vec![1, 0, 0]];
        cases.push([4_u32.to_le_bytes().as_slice(), b"ab"].concat());
        cases.push([1_u32.to_le_bytes().as_slice(), b"a", &[1, 0]].concat());
        cases.push(
            [
                1_u32.to_le_bytes().as_slice(),
                b"a",
                4_u32.to_le_bytes().as_slice(),
                b"ab",
            ]
            .concat(),
        );

        for tail in cases {
            let bytes = [complete_database.as_slice(), tail.as_slice()].concat();
            let mut restore = RestoreSpy::default();
            let error = parse_staging(&bytes, &mut restore).expect_err("tail must fail");
            assert_eq!(error.code, LocalBackupErrorCode::TruncatedInput);
            assert_eq!(restore.calls, 0);
        }
    }

    #[test]
    fn treats_u32_max_as_a_wire_length_without_allocating_it() {
        assert_eq!(validate_v1_entry_length(u32::MAX as u64), Ok(u32::MAX));
        assert_eq!(
            validate_v1_entry_length(u32::MAX as u64 + 1)
                .expect_err("larger length must fail")
                .code,
            LocalBackupErrorCode::LengthOverflow
        );

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.push(b'a');
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut restore = RestoreSpy::default();
        let error = parse_staging(&bytes, &mut restore).expect_err("body is truncated");
        assert_eq!(error.code, LocalBackupErrorCode::TruncatedInput);
    }

    #[test]
    fn applies_the_same_bounded_name_limit_to_parser_and_writer() {
        let declared_name_length = MAX_NAME_BYTES + 1;
        let parser_error = parse_staging(
            &declared_name_length.to_le_bytes(),
            &mut RestoreSpy::default(),
        )
        .expect_err("oversized parser name must fail before allocation");

        let directory = tempfile::tempdir().expect("temporary sources");
        let database = directory.path().join("database.bin");
        fs::write(&database, b"db").expect("database source");
        let writer_entries = vec![
            LegacyBackupWriteEntry {
                logical_name: "x".repeat(declared_name_length as usize),
                source: LegacyBackupWriteSource::MissingReference,
            },
            file_entry("database.risudat", &database),
        ];
        let writer_error =
            write_legacy_local_backup_v1(&mut Vec::new(), &writer_entries, &NeverCancelled)
                .expect_err("oversized writer name must fail");

        assert_eq!(parser_error.code, LocalBackupErrorCode::LengthOverflow);
        assert_eq!(writer_error.code, LocalBackupErrorCode::LengthOverflow);
    }

    #[test]
    fn cancellation_during_an_entry_does_not_restore_the_database() {
        struct CancellingReader {
            inner: Cursor<Vec<u8>>,
            cancelled: Arc<AtomicBool>,
            reads: usize,
        }
        impl Read for CancellingReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let maximum = buffer.len().min(17);
                let read = self.inner.read(&mut buffer[..maximum])?;
                self.reads += 1;
                if self.reads > 5 {
                    self.cancelled.store(true, Ordering::SeqCst);
                }
                Ok(read)
            }
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let bytes = archive(&[(b"database.risudat", b"db"), (b"large.bin", &[7; 1024])]);
        let mut reader = CancellingReader {
            inner: Cursor::new(bytes),
            cancelled: Arc::clone(&cancelled),
            reads: 0,
        };
        let directory = tempfile::tempdir().expect("temporary job root");
        let mut restore = RestoreSpy::default();
        let probe = AtomicCancellation::new(cancelled);

        let error = parse_legacy_local_backup_v1(
            &mut reader,
            directory.path(),
            PayloadTarget::JobStaging,
            &mut restore,
            &probe,
        )
        .expect_err("cancel must fail");

        assert_eq!(error.code, LocalBackupErrorCode::Cancelled);
        assert_eq!(restore.calls, 0);
    }

    #[test]
    fn cancellation_observed_after_database_restore_does_not_misreport_the_commit() {
        struct CancellingRestore {
            cancelled: Arc<AtomicBool>,
        }
        impl StrictLocalBackupDatabaseRestore for CancellingRestore {
            fn restore_database(
                &mut self,
                _entry: &StagedLocalBackupEntry,
                _entries: &[StagedLocalBackupEntry],
            ) -> Result<(), LocalBackupError> {
                self.cancelled.store(true, Ordering::SeqCst);
                Ok(())
            }
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut restore = CancellingRestore {
            cancelled: Arc::clone(&cancelled),
        };
        let directory = tempfile::tempdir().expect("temporary job root");

        let report = parse_legacy_local_backup_v1(
            &mut Cursor::new(archive(&[(b"database.risudat", b"db")])),
            directory.path(),
            PayloadTarget::JobStaging,
            &mut restore,
            &AtomicCancellation::new(cancelled),
        )
        .expect("a completed restore must remain successful");

        assert_eq!(report.entries.len(), 1);
    }

    #[test]
    fn prepares_opaque_payloads_in_cas_without_creating_logical_aliases() {
        let repository = tempfile::tempdir().expect("temporary repository");
        let job = tempfile::tempdir().expect("temporary job root");
        let cas = PayloadCas::new(repository.path()).expect("open CAS");
        let bytes = archive(&[
            (b"images/raw.asset", b"unchanged-bytes"),
            (b"database.risudat", b"db"),
        ]);
        let mut restore = RestoreSpy::default();

        let report = parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes),
            job.path(),
            PayloadTarget::ImmutableCas(&cas),
            &mut restore,
            &NeverCancelled,
        )
        .expect("parse into CAS");

        let payload = &report.entries[0];
        assert!(payload.staged_path.is_none());
        assert_eq!(
            fs::read_dir(job.path().join("local-backup-v1"))
                .expect("read job staging")
                .count(),
            1,
            "only the database entry remains in job staging",
        );
        assert_eq!(
            cas.read_object(&payload.sha256).expect("read CAS object"),
            Some(b"unchanged-bytes".to_vec())
        );
        assert_eq!(
            cas.stat_object(&payload.sha256).expect("stat CAS object"),
            Some(b"unchanged-bytes".len() as u64)
        );
        assert!(
            cas.prepare_bytes(b"unchanged-bytes")
                .expect("deduplicate exact bytes")
                .deduplicated
        );
        assert_eq!(
            payload
                .immutable_object
                .as_ref()
                .expect("CAS result")
                .content_hash,
            payload.sha256
        );
        assert!(!repository.path().join("assets-v2/aliases").exists());
    }

    #[test]
    fn truncated_cas_payload_cleans_partial_object_and_skips_database_restore() {
        let repository = tempfile::tempdir().expect("temporary repository");
        let job = tempfile::tempdir().expect("temporary job root");
        let cas = PayloadCas::new(repository.path()).expect("open CAS");
        let mut bytes = entry(b"database.risudat", b"db");
        bytes.extend_from_slice(&9_u32.to_le_bytes());
        bytes.extend_from_slice(b"asset.bin");
        bytes.extend_from_slice(&32_u32.to_le_bytes());
        bytes.extend_from_slice(b"short");
        let mut restore = RestoreSpy::default();

        let error = parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes),
            job.path(),
            PayloadTarget::ImmutableCas(&cas),
            &mut restore,
            &NeverCancelled,
        )
        .expect_err("truncated CAS payload must fail");

        assert_eq!(error.code, LocalBackupErrorCode::TruncatedInput);
        assert_eq!(restore.calls, 0);
        assert_eq!(
            fs::read_dir(repository.path().join("assets-v2/staging"))
                .expect("read CAS staging")
                .count(),
            0
        );
    }

    fn file_entry(name: &str, path: &Path) -> LegacyBackupWriteEntry {
        LegacyBackupWriteEntry {
            logical_name: name.to_owned(),
            source: LegacyBackupWriteSource::File(path.to_path_buf()),
        }
    }

    #[test]
    fn writer_preserves_the_exact_v1_wire_and_opaque_payload_bytes() {
        let directory = tempfile::tempdir().expect("temporary sources");
        let asset = directory.path().join("asset.bin");
        let inlay = directory.path().join("inlay.bin");
        let database = directory.path().join("database.bin");
        fs::write(&asset, [0, 255, 1, 254]).expect("asset source");
        fs::write(&inlay, [9, 8, 7, 0, 6]).expect("Inlay source");
        fs::write(&database, b"db").expect("database source");
        let entries = vec![
            file_entry("nested/assets/original.JPEG", &asset),
            file_entry("inlay_00.risuinlay", &inlay),
            file_entry("database.risudat", &database),
        ];
        let mut output = Vec::new();

        let report = write_legacy_local_backup_v1(&mut output, &entries, &NeverCancelled)
            .expect("write backup");

        let expected = archive(&[
            (b"nested/assets/original.JPEG", &[0, 255, 1, 254]),
            (b"inlay_00.risuinlay", &[9, 8, 7, 0, 6]),
            (b"database.risudat", b"db"),
        ]);
        assert_eq!(output, expected);
        assert_eq!(report.written_entries, 3);
        assert!(report.is_complete());
    }

    #[test]
    fn writer_reports_missing_references_without_claiming_completeness() {
        let directory = tempfile::tempdir().expect("temporary sources");
        let database = directory.path().join("database.bin");
        fs::write(&database, b"db").expect("database source");
        let entries = vec![
            LegacyBackupWriteEntry {
                logical_name: "assets/missing.webp".to_owned(),
                source: LegacyBackupWriteSource::MissingReference,
            },
            file_entry("database.risudat", &database),
        ];
        let mut output = Vec::new();

        let report = write_legacy_local_backup_v1(&mut output, &entries, &NeverCancelled)
            .expect("write incomplete legacy backup");

        assert!(!report.is_complete());
        assert_eq!(report.missing_references, ["assets/missing.webp"]);
        assert_eq!(output, entry(b"database.risudat", b"db"));
    }

    #[test]
    fn writer_rejects_duplicate_normalized_names_and_a_missing_database_source() {
        let directory = tempfile::tempdir().expect("temporary sources");
        let payload = directory.path().join("payload.bin");
        fs::write(&payload, b"payload").expect("payload source");
        let duplicate = vec![
            file_entry("nested\\same.bin", &payload),
            file_entry("nested/same.bin", &payload),
            file_entry("database.risudat", &payload),
        ];
        let missing_database = vec![LegacyBackupWriteEntry {
            logical_name: "database.risudat".to_owned(),
            source: LegacyBackupWriteSource::MissingReference,
        }];

        let duplicate_error =
            write_legacy_local_backup_v1(&mut Vec::new(), &duplicate, &NeverCancelled)
                .expect_err("duplicate must fail");
        let database_error =
            write_legacy_local_backup_v1(&mut Vec::new(), &missing_database, &NeverCancelled)
                .expect_err("database source is required");

        assert_eq!(duplicate_error.code, LocalBackupErrorCode::DuplicatePath);
        assert_eq!(database_error.code, LocalBackupErrorCode::MissingDatabase);
    }

    #[test]
    fn writer_honors_cancellation_before_writing() {
        let directory = tempfile::tempdir().expect("temporary sources");
        let database = directory.path().join("database.bin");
        let entries = vec![file_entry("database.risudat", &database)];
        let cancelled = Arc::new(AtomicBool::new(true));
        let mut output = Vec::new();

        let error = write_legacy_local_backup_v1(
            &mut output,
            &entries,
            &AtomicCancellation::new(cancelled),
        )
        .expect_err("cancel must fail");

        assert_eq!(error.code, LocalBackupErrorCode::Cancelled);
        assert!(output.is_empty());
    }

    #[test]
    fn writer_preflight_stops_visiting_a_large_entry_set_after_cancellation() {
        use std::sync::atomic::AtomicUsize;

        struct CancelAfterChecks {
            calls: AtomicUsize,
            cancel_at: usize,
        }
        impl CancellationProbe for CancelAfterChecks {
            fn is_cancelled(&self) -> bool {
                self.calls.fetch_add(1, Ordering::SeqCst) + 1 >= self.cancel_at
            }
        }

        let mut entries = (0..50_000)
            .map(|index| LegacyBackupWriteEntry {
                logical_name: format!("missing/{index}.bin"),
                source: LegacyBackupWriteSource::MissingReference,
            })
            .collect::<Vec<_>>();
        entries.push(LegacyBackupWriteEntry {
            logical_name: "database.risudat".to_owned(),
            source: LegacyBackupWriteSource::MissingReference,
        });
        let cancellation = CancelAfterChecks {
            calls: AtomicUsize::new(0),
            cancel_at: 8,
        };

        let error = write_legacy_local_backup_v1(&mut Vec::new(), &entries, &cancellation)
            .expect_err("preflight cancellation must stop the scan");

        assert_eq!(error.code, LocalBackupErrorCode::Cancelled);
        assert_eq!(cancellation.calls.load(Ordering::SeqCst), 8);
    }

    #[test]
    fn writer_rejects_sources_that_change_after_preflight() {
        struct MutatingOutput {
            bytes: Vec<u8>,
            source: PathBuf,
            replacement: Vec<u8>,
            mutated: bool,
        }
        impl Write for MutatingOutput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.mutated {
                    fs::write(&self.source, &self.replacement)?;
                    self.mutated = true;
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        for replacement in [b"".as_slice(), b"database-grew".as_slice()] {
            let directory = tempfile::tempdir().expect("temporary sources");
            let database = directory.path().join("database.bin");
            fs::write(&database, b"db").expect("database source");
            let entries = vec![file_entry("database.risudat", &database)];
            let mut output = MutatingOutput {
                bytes: Vec::new(),
                source: database,
                replacement: replacement.to_vec(),
                mutated: false,
            };

            let error = write_legacy_local_backup_v1(&mut output, &entries, &NeverCancelled)
                .expect_err("changed source length must fail");

            assert_eq!(error.code, LocalBackupErrorCode::SourceLengthMismatch);
        }
    }

    #[test]
    fn writer_checks_cancellation_between_partial_destination_writes() {
        struct CancellingOutput {
            cancelled: Arc<AtomicBool>,
        }
        impl Write for CancellingOutput {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.cancelled.store(true, Ordering::SeqCst);
                Ok(bytes.len().min(1))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let directory = tempfile::tempdir().expect("temporary sources");
        let database = directory.path().join("database.bin");
        fs::write(&database, b"db").expect("database source");
        let entries = vec![file_entry("database.risudat", &database)];
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut output = CancellingOutput {
            cancelled: Arc::clone(&cancelled),
        };

        let error = write_legacy_local_backup_v1(
            &mut output,
            &entries,
            &AtomicCancellation::new(cancelled),
        )
        .expect_err("mid-write cancellation must fail");

        assert_eq!(error.code, LocalBackupErrorCode::Cancelled);
    }
}
