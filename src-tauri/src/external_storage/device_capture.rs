//! Immutable device-maintenance capture files for backup-only repositories.
//!
//! The renderer chooses and captures the exact device sections through the
//! existing `device_backup` maintenance session. This adapter only seals that
//! verified source spool into native files which the external snapshot pipeline
//! can borrow. It never starts maintenance, reads WebView storage, or owns a
//! remote credential.

use crate::{
    device_backup::{
        validate_archive_catalog, DeviceBackupError, DeviceBackupState, Operation, Spool,
    },
    local_backup::CancellationProbe,
    trust_boundary::{is_link_like, open_regular_source, sync_directory},
};
use risunest_external_storage_format::content_identity::hash_reader;
use risunest_external_storage_format::format::Scope;
use rusqlite::{params, Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, DeviceBackupError>;

const CATALOG_KEY: &str = "device/catalog";
const CAPTURE_DOMAIN: &[u8] = b"RisuNest-external-device-capture-v1\0";
const COPY_BYTES: usize = 64 * 1024;

pub(crate) fn capture_root(repository_root: &Path) -> PathBuf {
    repository_root.join("external-device-captures")
}

/// Require the verified device inventory to match the immutable descriptor.
pub(crate) fn validate_snapshot_scope(scope: &Scope, sections: &[String]) -> Result<()> {
    if sections.is_empty() {
        return if scope.device_settings || scope.device_plugins {
            Err(invalid("Device snapshot section count is invalid"))
        } else {
            Ok(())
        };
    }
    if sections.len() > 1024 {
        return Err(invalid("Device snapshot section count is invalid"));
    }
    let mut unique = std::collections::HashSet::new();
    if sections
        .iter()
        .any(|section| !unique.insert(section.as_str()))
    {
        return Err(invalid("Device snapshot contains duplicate sections"));
    }
    let settings = sections.iter().any(|section| section == "device-settings");
    let local_storage = sections.iter().any(|section| section == "local-storage");
    let localforage = sections.iter().any(|section| section == "localforage");
    let invalid_plugin = sections.iter().any(|section| {
        section != "device-settings"
            && section != "local-storage"
            && section != "localforage"
            && !section.strip_prefix("indexed-db:").is_some_and(|name| {
                name.starts_with("0073006100660065005f0070006c007500670069006e005f")
                    && name.len() % 4 == 0
                    && name.len() <= 65536
                    && name.bytes().all(crate::trust_boundary::is_lower_hex_byte)
            })
    });
    if settings != scope.device_settings
        || local_storage != scope.device_plugins
        || localforage != scope.device_plugins
        || (!scope.device_plugins && sections.iter().any(|section| section != "device-settings"))
        || invalid_plugin
    {
        return Err(invalid(
            "Device snapshot sections differ from repository scope",
        ));
    }
    Ok(())
}

fn failure(code: &str, message: &str) -> DeviceBackupError {
    DeviceBackupError {
        code: code.into(),
        message: message.into(),
    }
}

fn invalid(message: &str) -> DeviceBackupError {
    failure("device-capture-invalid", message)
}

fn cancelled() -> DeviceBackupError {
    failure("device-cancelled", "External device capture was cancelled")
}

fn checkpoint(probe: &dyn CancellationProbe) -> Result<()> {
    if probe.is_cancelled() {
        Err(cancelled())
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceFile {
    /// Stable logical key used by the external catalog. This is never a path.
    pub(crate) section_id: String,
    pub(crate) content_hash: [u8; 32],
    pub(crate) byte_length: u64,
    pub(crate) path: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct DeviceSnapshot {
    pub(crate) capture_id: String,
    pub(crate) device_identity: [u8; 32],
    pub(crate) sqlite: DeviceFile,
    pub(crate) blobs: Vec<DeviceFile>,
    pub(crate) sections: Vec<String>,
}

struct PendingDirectory {
    path: PathBuf,
    owned: bool,
}

impl PendingDirectory {
    fn create(parent: &Path) -> Result<Self> {
        let path = parent.join(format!(".capture-{}.partial", uuid::Uuid::new_v4()));
        fs::create_dir(&path)?;
        Ok(Self { path, owned: true })
    }

    fn keep(&mut self) {
        self.owned = false;
    }
}

impl Drop for PendingDirectory {
    fn drop(&mut self) {
        if self.owned {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn checked_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    if is_link_like(&fs::symlink_metadata(path)?) {
        return Err(invalid("Device capture directory is a link"));
    }
    Ok(())
}

fn verify_file(path: &Path, expected_length: u64, expected_hash: &[u8; 32]) -> Result<()> {
    let mut file = open_regular_source(path)?;
    if file.metadata()?.len() != expected_length
        || hash_reader(&mut file, expected_length)
            .map_err(|_| invalid("Device capture file integrity failed"))?
            != *expected_hash
    {
        return Err(invalid("Device capture file integrity failed"));
    }
    Ok(())
}

fn publish_object(
    objects: &Path,
    expected_hash: &str,
    expected_length: u64,
    source: &mut dyn Read,
    probe: &dyn CancellationProbe,
) -> Result<DeviceFile> {
    let content_hash: [u8; 32] = hex::decode(expected_hash)
        .ok()
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| invalid("Device binary identity is invalid"))?;
    let pending_path = objects.join(format!(".object-{}.partial", uuid::Uuid::new_v4()));
    let pending_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&pending_path)?;
    let mut pending = Some(pending_file);
    let mut digest = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; COPY_BYTES];
    let copy_result = (|| {
        loop {
            checkpoint(probe)?;
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            copied = copied
                .checked_add(count as u64)
                .ok_or_else(|| invalid("Device binary length overflow"))?;
            if copied > expected_length {
                return Err(invalid("Device binary exceeds its sealed length"));
            }
            pending.as_mut().unwrap().write_all(&buffer[..count])?;
            digest.update(&buffer[..count]);
        }
        if copied != expected_length || digest.finalize().as_slice() != content_hash {
            return Err(invalid("Device binary differs from its sealed identity"));
        }
        pending.as_mut().unwrap().sync_all()?;
        drop(pending.take());
        let destination = objects.join(expected_hash);
        #[cfg(target_os = "android")]
        let publication =
            crate::trust_boundary::rename_without_replace(&pending_path, &destination);
        #[cfg(not(target_os = "android"))]
        let publication = fs::hard_link(&pending_path, &destination);
        match publication {
            Ok(()) => {
                sync_directory(objects)?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                verify_file(&destination, expected_length, &content_hash)?;
            }
            Err(error) => return Err(error.into()),
        }
        if pending_path.exists() {
            fs::remove_file(&pending_path)?;
            sync_directory(objects)?;
        }
        Ok(DeviceFile {
            section_id: format!("device/object/{expected_hash}"),
            content_hash,
            byte_length: expected_length,
            path: destination,
        })
    })();
    if copy_result.is_err() {
        drop(pending.take());
        let _ = fs::remove_file(&pending_path);
    }
    copy_result
}

fn create_catalog(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )?;
    db.execute_batch(
        "PRAGMA journal_mode=DELETE;
         PRAGMA synchronous=FULL;
         PRAGMA trusted_schema=OFF;
         CREATE TABLE device_sections (
             section TEXT PRIMARY KEY,
             schema_version INTEGER NOT NULL,
             included INTEGER NOT NULL,
             complete INTEGER NOT NULL,
             present INTEGER NOT NULL,
             record_count INTEGER NOT NULL,
             sha256 TEXT NOT NULL
         );
         CREATE TABLE device_records (
             section TEXT NOT NULL,
             ordinal INTEGER NOT NULL,
             metadata TEXT NOT NULL,
             PRIMARY KEY(section, ordinal)
         );
         CREATE TABLE device_objects (
             sha256 TEXT PRIMARY KEY,
             byte_length INTEGER NOT NULL
         );",
    )?;
    Ok(db)
}

fn logical_identity(db: &Connection, blobs: &[DeviceFile]) -> Result<([u8; 32], Vec<String>)> {
    let mut digest = Sha256::new();
    digest.update(CAPTURE_DOMAIN);
    let mut sections = Vec::new();
    let mut statement = db.prepare(
        "SELECT section,sha256 FROM device_sections
         WHERE included=1 AND complete=1 ORDER BY section",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let section: String = row.get(0)?;
        let section_hash: String = row.get(1)?;
        let section_hash: [u8; 32] = hex::decode(section_hash)
            .ok()
            .and_then(|value| value.try_into().ok())
            .ok_or_else(|| invalid("Device section identity is invalid"))?;
        digest.update((section.len() as u64).to_le_bytes());
        digest.update(section.as_bytes());
        digest.update(section_hash);
        sections.push(section);
    }
    if sections.is_empty() {
        return Err(invalid("Device capture contains no selected sections"));
    }
    for blob in blobs {
        digest.update(blob.content_hash);
        digest.update(blob.byte_length.to_le_bytes());
    }
    Ok((digest.finalize().into(), sections))
}

fn catalog_file(path: PathBuf) -> Result<DeviceFile> {
    let mut file = open_regular_source(&path)?;
    let byte_length = file.metadata()?.len();
    let content_hash = hash_reader(&mut file, byte_length)
        .map_err(|_| invalid("Device catalog integrity failed"))?;
    Ok(DeviceFile {
        section_id: CATALOG_KEY.into(),
        content_hash,
        byte_length,
        path,
    })
}

fn open_snapshot(root: &Path, capture_id: &str) -> Result<DeviceSnapshot> {
    if !crate::trust_boundary::is_lower_hex_256(capture_id) {
        return Err(invalid("Device capture identifier is invalid"));
    }
    let capture_root = root.join("captures");
    let object_root = root.join("objects");
    let directory = capture_root.join(capture_id);
    if is_link_like(&fs::symlink_metadata(&directory)?) {
        return Err(invalid("Device capture is a link"));
    }
    let catalog_path = directory.join("device.sqlite");
    let db = Connection::open_with_flags(&catalog_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    struct Never;
    impl CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    validate_archive_catalog(&db, &Never)?;
    let mut blobs = Vec::new();
    let mut statement =
        db.prepare("SELECT sha256,byte_length FROM device_objects ORDER BY sha256")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (hash, byte_length) = row?;
        let byte_length =
            u64::try_from(byte_length).map_err(|_| invalid("Device binary length is invalid"))?;
        let content_hash: [u8; 32] = hex::decode(&hash)
            .ok()
            .and_then(|value| value.try_into().ok())
            .ok_or_else(|| invalid("Device binary identity is invalid"))?;
        let path = object_root.join(&hash);
        verify_file(&path, byte_length, &content_hash)?;
        blobs.push(DeviceFile {
            section_id: format!("device/object/{hash}"),
            content_hash,
            byte_length,
            path,
        });
    }
    drop(statement);
    drop(db);
    verify_snapshot(DeviceSnapshot {
        capture_id: capture_id.into(),
        device_identity: hex::decode(capture_id)
            .ok()
            .and_then(|value| value.try_into().ok())
            .ok_or_else(|| invalid("Device capture identifier is invalid"))?,
        sqlite: catalog_file(catalog_path)?,
        blobs,
        sections: Vec::new(),
    })
}

/// Reverify an owned device snapshot regardless of where its downloaded files
/// are staged. This is the trust boundary shared by local reopen and restore.
pub(crate) fn verify_snapshot(mut snapshot: DeviceSnapshot) -> Result<DeviceSnapshot> {
    if !crate::trust_boundary::is_lower_hex_256(&snapshot.capture_id)
        || hex::encode(snapshot.device_identity) != snapshot.capture_id
        || snapshot.sqlite.section_id != CATALOG_KEY
    {
        return Err(invalid("Device capture identity is invalid"));
    }
    verify_file(
        &snapshot.sqlite.path,
        snapshot.sqlite.byte_length,
        &snapshot.sqlite.content_hash,
    )?;
    snapshot.blobs.sort_by_key(|file| file.content_hash);
    let db = Connection::open_with_flags(&snapshot.sqlite.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    struct Never;
    impl CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    validate_archive_catalog(&db, &Never)?;
    let mut expected = Vec::new();
    let mut statement =
        db.prepare("SELECT sha256,byte_length FROM device_objects ORDER BY sha256")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (hash, length) = row?;
        expected.push((
            hash,
            u64::try_from(length).map_err(|_| invalid("Device binary length is invalid"))?,
        ));
    }
    if expected.len() != snapshot.blobs.len() {
        return Err(invalid("Device capture binary inventory differs"));
    }
    for ((expected_hash, expected_length), file) in expected.iter().zip(&snapshot.blobs) {
        if file.section_id != format!("device/object/{expected_hash}")
            || hex::encode(file.content_hash) != *expected_hash
            || file.byte_length != *expected_length
        {
            return Err(invalid("Device capture binary inventory differs"));
        }
        verify_file(&file.path, file.byte_length, &file.content_hash)?;
    }
    let (device_identity, sections) = logical_identity(&db, &snapshot.blobs)?;
    if device_identity != snapshot.device_identity
        || (!snapshot.sections.is_empty() && snapshot.sections != sections)
    {
        return Err(invalid("Device capture identity differs"));
    }
    drop(statement);
    drop(db);
    snapshot.sections = sections;
    Ok(snapshot)
}

/// Seal a renderer-completed source spool. The caller keeps the existing
/// file(true) permit and maintenance guard until this returns successfully.
pub(crate) fn seal_device_snapshot(
    state: &DeviceBackupState,
    session_id: &str,
    root: &Path,
    probe: &dyn CancellationProbe,
) -> Result<DeviceSnapshot> {
    checkpoint(probe)?;
    let session = state.session(session_id)?;
    if session.operation != Operation::Capture || session.phase != "device-captured" {
        return Err(invalid("Device capture spool is not sealed"));
    }
    checked_directory(root)?;
    let captures = root.join("captures");
    let objects = root.join("objects");
    checked_directory(&captures)?;
    checked_directory(&objects)?;
    let mut pending = PendingDirectory::create(&captures)?;
    let catalog_path = pending.path.join("device.sqlite");
    let db = create_catalog(&catalog_path)?;
    let mut blobs = BTreeMap::<String, DeviceFile>::new();
    state.export_spool(session_id, Spool::Source, &db, |hash, length, reader| {
        let file = publish_object(&objects, hash, length, reader, probe)?;
        match blobs.get(hash) {
            Some(existing) if existing.byte_length != file.byte_length => {
                return Err(invalid("Duplicate device binary length differs"));
            }
            Some(_) => {}
            None => {
                blobs.insert(hash.into(), file);
            }
        }
        db.execute(
            "INSERT INTO device_objects VALUES(?1,?2)",
            params![
                hash,
                i64::try_from(length)
                    .map_err(|_| invalid("Device binary length exceeds SQLite range"))?
            ],
        )?;
        Ok(())
    })?;
    checkpoint(probe)?;
    validate_archive_catalog(&db, probe)?;
    let blobs: Vec<_> = blobs.into_values().collect();
    let (device_identity, sections) = logical_identity(&db, &blobs)?;
    drop(db);
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&catalog_path)?
        .sync_all()?;
    sync_directory(&pending.path)?;
    let capture_id = hex::encode(device_identity);
    let destination = captures.join(&capture_id);
    match fs::rename(&pending.path, &destination) {
        Ok(()) => {
            pending.keep();
            sync_directory(&captures)?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists || destination.exists() => {
            let existing = open_snapshot(root, &capture_id)?;
            return Ok(existing);
        }
        Err(error) => return Err(error.into()),
    }
    open_snapshot(root, &capture_id).map(|mut snapshot| {
        snapshot.sections = sections;
        snapshot
    })
}

/// Reopen and fully verify a shared immutable device capture after restart.
pub(crate) fn reopen_device_snapshot(root: &Path, capture_id: &str) -> Result<DeviceSnapshot> {
    open_snapshot(root, capture_id)
}

/// Import a verified device capture into an already-owned restore session.
/// Renderer preparation, rollback, section apply, and commit-marker recovery
/// continue through the existing device maintenance protocol.
pub(crate) fn import_device_snapshot(
    state: &DeviceBackupState,
    session_id: &str,
    snapshot: &DeviceSnapshot,
    probe: &dyn CancellationProbe,
) -> Result<()> {
    checkpoint(probe)?;
    let reopened = verify_snapshot(snapshot.clone())?;
    let db = Connection::open_with_flags(&reopened.sqlite.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let objects: Vec<_> = reopened
        .blobs
        .iter()
        .map(|file| (hex::encode(file.content_hash), file.byte_length))
        .collect();
    state.import_spool(session_id, &db, &objects, |hash, writer| {
        checkpoint(probe)?;
        let file = reopened
            .blobs
            .iter()
            .find(|file| hex::encode(file.content_hash) == hash)
            .ok_or_else(|| invalid("Device capture binary is missing"))?;
        let mut source = open_regular_source(&file.path)?;
        let mut digest = Sha256::new();
        let mut copied = 0u64;
        let mut buffer = [0u8; COPY_BYTES];
        loop {
            checkpoint(probe)?;
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            writer.write_all(&buffer[..count])?;
            digest.update(&buffer[..count]);
            copied += count as u64;
        }
        if copied != file.byte_length || digest.finalize().as_slice() != file.content_hash {
            return Err(invalid("Device capture binary changed during import"));
        }
        Ok(())
    })?;
    state.source_ready(session_id)
}

#[cfg(test)]
#[path = "device_capture_tests.rs"]
mod tests;
