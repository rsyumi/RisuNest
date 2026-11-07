//! Local canonical capture spool. The SQLite catalog contains only hashes and
//! lengths; unchanged record bodies are immutable shared files, never copied as
//! part of another small-edit capture. This module performs no remote requests.
use crate::{
    local_backup::{CancellationProbe, NeverCancelled},
    persistent_store::{
        content_capture::ContentCaptureSink, sync_selection::CaptureIdentity, StoreError,
    },
    trust_boundary::{is_link_like, sync_directory},
};
use super::content_store::ContentStore;
use risunest_external_storage_format::content_identity::{hash, hash_reader};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, StoreError>;

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}
fn checked_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    if is_link_like(&fs::symlink_metadata(path)?) {
        return Err(invalid("Capture directory must not be a link"));
    }
    Ok(())
}

pub(super) struct OwnedCaptureFile {
    path: PathBuf,
    directory: PathBuf,
    owned: bool,
}

impl OwnedCaptureFile {
    pub(super) fn new(path: PathBuf, directory: &Path) -> Self {
        Self {
            path,
            directory: directory.to_path_buf(),
            owned: true,
        }
    }

    fn keep(&mut self) {
        self.owned = false;
    }

    pub(super) fn remove_and_sync(&mut self) -> io::Result<bool> {
        if !self.owned {
            return Ok(true);
        }
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.owned = false;
        sync_directory(&self.directory)
    }
}

impl Drop for OwnedCaptureFile {
    fn drop(&mut self) {
        if self.owned && fs::remove_file(&self.path).is_ok() {
            let _ = sync_directory(&self.directory);
        }
    }
}

pub(crate) struct CaptureCatalog {
    pub(crate) db: Connection,
    path: PathBuf,
    content: ContentStore,
    identity: Option<CaptureIdentity>,
    pub(crate) rebuilt: bool,
    finalized: bool,
    backup_inputs: Option<BackupCaptureInputs>,
}

#[derive(Clone, Debug)]
pub(crate) struct OriginalBackupUnits {
    path: PathBuf,
    count: u64,
    _scratch: Option<std::sync::Arc<tempfile::NamedTempFile>>,
}

pub(crate) struct OriginalBackupUnitRows {
    db: Connection,
    _scratch: Option<std::sync::Arc<tempfile::NamedTempFile>>,
    after: String,
    remaining: u64,
    page: std::collections::VecDeque<(String, String)>,
    finished: bool,
}
impl OriginalBackupUnits {
    pub(crate) fn capture(directory: &Path, produce: impl FnOnce(&mut dyn FnMut(risunest_sync_wire::unit::UnitKey, risunest_sync_wire::unit::UnitValue) -> Result<()>) -> Result<()>) -> Result<Self> {
        fs::create_dir_all(directory)?;
        let file = tempfile::NamedTempFile::new_in(directory)?;
        let mut db = crate::sqlite_open::open(file.path())?;
        db.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-4096; PRAGMA temp_store=FILE; CREATE TABLE original_units(key TEXT PRIMARY KEY,value TEXT NOT NULL)")?;
        let tx = db.transaction()?;
        let mut count = 0u64;
        produce(&mut |key, value| {
            value.validate().map_err(|_| invalid("Invalid original backup value"))?;
            tx.execute("INSERT INTO original_units VALUES(?1,?2)", params![key.as_str(),serde_json::to_string(&value)?])?;
            count = count.checked_add(1).ok_or_else(|| invalid("Original backup count overflow"))?;
            Ok(())
        })?;
        tx.commit()?;
        drop(db);
        Ok(Self { path: file.path().into(), count, _scratch: Some(std::sync::Arc::new(file)) })
    }
    pub(crate) fn len(&self) -> u64 { self.count }
    pub(crate) fn units(&self) -> Result<OriginalBackupUnitRows> {
        let db = crate::sqlite_open::open_with_flags(&self.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.execute_batch("PRAGMA cache_size=-4096; PRAGMA temp_store=FILE")?;
        let count: i64 = db.query_row("SELECT count(*) FROM original_units", [], |row| row.get(0))?;
        if u64::try_from(count).ok() != Some(self.count) { return Err(invalid("Original backup unit count differs")); }
        Ok(OriginalBackupUnitRows { db, _scratch:self._scratch.clone(), after: String::new(), remaining: self.count, page: Default::default(), finished: false })
    }
}
impl Iterator for OriginalBackupUnitRows {
    type Item = Result<(risunest_sync_wire::unit::UnitKey, risunest_sync_wire::unit::UnitValue)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.finished { return None; }
        let read = (|| {
            if self.page.is_empty() {
                let mut query = self.db.prepare("SELECT key,value FROM original_units WHERE key>?1 ORDER BY key LIMIT 128")?;
                self.page = query.query_map([&self.after], |row| Ok((row.get(0)?,row.get(1)?)))?.collect::<std::result::Result<_,_>>()?;
            }
            let Some((key,value)) = self.page.pop_front() else {
                self.finished = true;
                if self.remaining != 0 { return Err(invalid("Original backup unit count differs")); }
                return Ok(None);
            };
            self.after = key.clone();
            self.remaining = self.remaining.checked_sub(1).ok_or_else(|| invalid("Original backup unit count differs"))?;
            let key = risunest_sync_wire::unit::UnitKey::try_from(key).map_err(|_| invalid("Invalid original backup key"))?;
            let value: risunest_sync_wire::unit::UnitValue = serde_json::from_str(&value)?;
            value.validate().map_err(|_| invalid("Invalid original backup value"))?;
            Ok(Some((key,value)))
        })();
        match read { Ok(value) => value.map(Ok), Err(error) => {self.finished=true;Some(Err(error))} }
    }
}
#[cfg(test)]
impl From<std::collections::BTreeMap<risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue>> for OriginalBackupUnits {
    fn from(units: std::collections::BTreeMap<risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue>) -> Self {
        Self::capture(&std::env::temp_dir(), |emit| {for (key,value) in units {emit(key,value)?;} Ok(())}).unwrap()
    }
}

struct BackupCaptureInputs {
    units: OriginalBackupUnits,
    closure: crate::persistent_store::external_capture::BackupDependencyClosure,
    sections: Vec<super::sections::CapturedSection>,
    streamed: Option<BackupDependencySpool>,
}

pub(crate) struct BackupDependencySpool {
    db: Connection,
    file: tempfile::NamedTempFile,
    sealed: bool,
}

impl BackupDependencySpool {
    pub(crate) fn new(directory: &Path) -> Result<Self> {
        fs::create_dir_all(directory)?;
        if is_link_like(&fs::symlink_metadata(directory)?) { return Err(invalid("Backup spool directory must not be a link")); }
        let file = tempfile::NamedTempFile::new_in(directory)?;
        let db = crate::sqlite_open::open(file.path())?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=OFF; PRAGMA cache_size=-4096; PRAGMA temp_store=FILE;
            CREATE TABLE bodies(hash TEXT PRIMARY KEY,body BLOB NOT NULL,role INTEGER NOT NULL CHECK(role IN(1,2,3)));
            BEGIN IMMEDIATE;")?;
        Ok(Self {file,db,sealed:false})
    }
    pub(crate) fn push(&mut self, hash:&str,bytes:&[u8],role:crate::persistent_store::external_capture::BackupBodyRole) -> Result<()> {
        if self.sealed || crate::persistent_store::external_capture::hash_backup_body(bytes,"native_backup_spool_write") != hash {
            return Err(invalid("Backup spool body identity differs"));
        }
        let role = match role {
            crate::persistent_store::external_capture::BackupBodyRole::Control if bytes.len()>risunest_sync_wire::MAX_METADATA_BYTES => {
                crate::persistent_store::external_capture::verified_oversized_control(bytes)?;3
            },
            crate::persistent_store::external_capture::BackupBodyRole::Control => 1,
            crate::persistent_store::external_capture::BackupBodyRole::Payload => 2,
        };
        if role!=3 && bytes.len()>risunest_sync_wire::MAX_METADATA_BYTES {return Err(invalid("Backup spool body exceeds its bound"));}
        self.db.execute("INSERT OR IGNORE INTO bodies VALUES(?1,?2,?3)",params![hash,bytes,role])?;
        let actual: i64 = self.db.query_row("SELECT role FROM bodies WHERE hash=?1",[hash],|row| row.get(0))?;
        if actual != role {return Err(invalid("Backup spool body role differs"))}
        Ok(())
    }
    pub(crate) fn seal(&mut self) -> Result<()> {
        if self.sealed {return Err(invalid("Backup spool was already sealed"))}
        self.db.execute_batch("COMMIT; PRAGMA synchronous=FULL;")?;
        self.file.as_file().sync_all()?;
        self.sealed = true;
        Ok(())
    }
    pub(crate) fn push_ranged_control(&mut self, hash: &str, input: &mut (impl std::io::Read + std::io::Seek), length: u64) -> Result<bool> {
        use crate::persistent_store::external_capture::verified_string_control;
        if self.sealed { return Err(invalid("Backup spool was already sealed")); }
        if !verified_string_control(input, hash, length, "native_backup_spool_write")? { return Ok(false); }
        let length = i64::try_from(length).map_err(|_| invalid("Backup control length is invalid"))?;
        let existing: Option<(i64, i64)> = self.db.query_row("SELECT rowid,role FROM bodies WHERE hash=?1", [hash], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        if let Some((rowid, role)) = existing {
            let mut stored = self.db.blob_open(rusqlite::MAIN_DB, "bodies", "body", rowid, true)?;
            if role != 3 || stored.len() as i64 != length || hex::encode(hash_reader(&mut stored, length as u64).map_err(|_| invalid("Backup spool body is unreadable"))?) != hash {
                return Err(invalid("Backup spool body identity or role differs"));
            }
            return Ok(true);
        }
        self.db.execute("INSERT INTO bodies(hash,body,role) VALUES(?1,zeroblob(?2),3)", params![hash, length])?;
        let (rowid, role): (i64, i64) = self.db.query_row("SELECT rowid,role FROM bodies WHERE hash=?1", [hash], |row| Ok((row.get(0)?, row.get(1)?)))?;
        if role != 3 { return Err(invalid("Backup spool body role differs")); }
        let mut output = self.db.blob_open(rusqlite::MAIN_DB, "bodies", "body", rowid, false)?;
        if output.len() as i64 != length || io::copy(input, &mut output)? != length as u64 { return Err(invalid("Backup spool body length differs")); }
        output.close()?;
        let mut stored = self.db.blob_open(rusqlite::MAIN_DB, "bodies", "body", rowid, true)?;
        if hex::encode(hash_reader(&mut stored, length as u64).map_err(|_| invalid("Backup spool body is unreadable"))?) != hash { return Err(invalid("Backup spool body identity differs")); }
        Ok(true)
    }

    pub(crate) fn control_reader(&self, hash: &str) -> Result<Option<rusqlite::blob::Blob<'_>>> {
        if !self.sealed { return Err(invalid("Backup dependency spool is not sealed")); }
        let row: Option<i64> = self.db.query_row("SELECT rowid FROM bodies WHERE hash=?1 AND role IN(1,3)", [hash], |row| row.get(0)).optional()?;
        row.map(|row| self.db.blob_open(rusqlite::MAIN_DB, "bodies", "body", row, true).map_err(Into::into)).transpose()
    }

    pub(crate) fn visit_ranged(&self, visitor: &mut dyn FnMut(&str, &mut rusqlite::blob::Blob<'_>, u64, crate::persistent_store::external_capture::BackupBodyRole) -> Result<()>) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        if !self.sealed { return Err(invalid("Backup dependency spool is not sealed")); }
        let mut statement = self.db.prepare("SELECT hash,rowid,role FROM bodies ORDER BY hash")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?; let stored_role: i64 = row.get(2)?;
            let mut input = self.db.blob_open(rusqlite::MAIN_DB, "bodies", "body", row.get(1)?, true)?;
            let length = input.len() as u64;
            let role = match stored_role {
                1 | 3 => crate::persistent_store::external_capture::BackupBodyRole::Control,
                2 => crate::persistent_store::external_capture::BackupBodyRole::Payload,
                _ => return Err(invalid("Backup dependency role is invalid")),
            };
            if stored_role != 3 && length > risunest_sync_wire::MAX_METADATA_BYTES as u64 { return Err(invalid("Backup dependency spool body exceeds its bound")); }
            if !crate::persistent_store::external_capture::verified_string_control(&mut input, &hash, length, "native_backup_spool_read")? {
                let mut bytes = Vec::new(); input.read_to_end(&mut bytes)?;
                if crate::persistent_store::external_capture::hash_backup_body(&bytes, "native_backup_spool_read") != hash { return Err(invalid("Backup dependency spool integrity failed")); }
                if stored_role == 3 { crate::persistent_store::external_capture::verified_oversized_control(&bytes)?; }
            }
            input.seek(SeekFrom::Start(0))?;
            visitor(&hash, &mut input, length, role)?;
        }
        Ok(())
    }
    pub(crate) fn count(&self) -> Result<u64> {
        let count: i64 = self.db.query_row("SELECT COUNT(*) FROM bodies", [], |row| row.get(0))?;
        u64::try_from(count).map_err(|_| invalid("Backup dependency spool count is invalid"))
    }
    pub(crate) fn control(&self,hash:&str)->Result<Option<Vec<u8>>> {
        self.body_with_role(hash,1)
    }
    pub(crate) fn payload(&self,hash:&str)->Result<Option<Vec<u8>>> {
        self.body_with_role(hash,2)
    }
    fn body_with_role(&self,hash:&str,expected_role:i64)->Result<Option<Vec<u8>>> {
        if !self.sealed {return Err(invalid("Backup dependency spool is not sealed"))}
        let metadata:Option<(i64,i64)>=self.db.query_row("SELECT length(body),role FROM bodies WHERE hash=?1",[hash],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        let Some((length,role))=metadata else {return Ok(None)};
        if role!=expected_role && !(expected_role==1 && role==3) {return Ok(None)}
        if length<0 || (role!=3 && length as u64>risunest_sync_wire::MAX_METADATA_BYTES as u64) {return Err(invalid("Backup dependency spool body exceeds its bound"))}
        let bytes:Vec<u8>=self.db.query_row("SELECT body FROM bodies WHERE hash=?1",[hash],|row|row.get(0))?;
        let domain=if expected_role==1 {"native_backup_spool_control_read"}else{"native_backup_spool_payload_read"};
        if bytes.len() as i64!=length || crate::persistent_store::external_capture::hash_backup_body(&bytes,domain)!=hash {return Err(invalid("Backup dependency spool integrity failed"))}
        if role==3 {crate::persistent_store::external_capture::verified_oversized_control(&bytes)?;}
        Ok(Some(bytes))
    }
    pub(crate) fn visit(&self,visitor:&mut dyn FnMut(&str,&[u8],crate::persistent_store::external_capture::BackupBodyRole)->Result<()>) -> Result<()> {
        if !self.sealed {return Err(invalid("Backup dependency spool is not sealed"))}
        let mut statement = self.db.prepare("SELECT hash,length(body),body,role FROM bodies ORDER BY hash")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let hash:String = row.get(0)?;
            let length:i64 = row.get(1)?;
            let stored_role=row.get::<_,i64>(3)?;
            if length < 0 || (stored_role!=3 && length as u64 > risunest_sync_wire::MAX_METADATA_BYTES as u64) {
                return Err(invalid("Backup dependency spool body exceeds its bound"));
            }
            let bytes:Vec<u8> = row.get(2)?;
            let role = match stored_role {
                1|3 => crate::persistent_store::external_capture::BackupBodyRole::Control,
                2 => crate::persistent_store::external_capture::BackupBodyRole::Payload,
                _ => return Err(invalid("Backup dependency role is invalid")),
            };
            if bytes.len() as i64!=length || crate::persistent_store::external_capture::hash_backup_body(&bytes,"native_backup_spool_read") != hash {return Err(invalid("Backup dependency spool integrity failed"))}
            if stored_role==3 {crate::persistent_store::external_capture::verified_oversized_control(&bytes)?;}
            visitor(&hash,&bytes,role)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DurableCaptureReference {
    pub capture_id: String,
    pub identity: CaptureIdentity,
    /// Relative to `<persistent root>/external-storage`.
    pub catalog_path: String,
    pub catalog_hash: String,
}

#[derive(Debug, Default)]
pub(crate) struct RegisteredCaptureRoots {
    pub assets: crate::asset_repository::migration_gc::AssetRootSet,
    pub catalogs: BTreeSet<PathBuf>,
    pub logical_records: BTreeSet<PathBuf>,
    /// Bodies the content store holds by identity rather than as files.
    pub content_objects: BTreeSet<String>,
}

/// Both paths are selected by the native owner. Previous catalogs must have
/// an authoritative PDS manifest hash; an unregistered receipt is not sufficient.
impl CaptureCatalog {
    pub(crate) fn reopen(
        path: &Path,
        external_root: &Path,
        expected_hash: &[u8; 32],
        expected_identity: &CaptureIdentity,
    ) -> Result<Self> {
        let (db, identity) = verified_catalog(path, expected_hash, expected_identity)?;
        Ok(Self {
            db,
            path: path.into(),
            content: ContentStore::open(external_root)?,
            identity: Some(identity),
            rebuilt: false,
            finalized: true,
            backup_inputs: None,
        })
    }

    /// Where this capture's bodies live. A body staged by the current capture
    /// is readable here before the catalog commits; a second connection to the
    /// same store would not see it yet.
    pub(crate) fn content(&self) -> &ContentStore {
        &self.content
    }

    pub(crate) fn object_reader(&mut self, hash: &str, input: impl std::io::Read, length: u64) -> Result<()> {
        self.require_writing()?;
        self.content.put_reader(hash, input, length)?;
        self.db.execute("INSERT OR IGNORE INTO generated VALUES(?1,?2)", params![hash, length as i64])?;
        Ok(())
    }

    fn require_writing(&self) -> Result<()> {
        if self.identity.is_none() || self.finalized || self.db.is_autocommit() {
            return Err(invalid("Capture catalog is not writable"));
        }
        Ok(())
    }
    pub(crate) fn create(
        directory: &Path,
        external_root: &Path,
        previous: Option<(&Path, &[u8; 32])>,
    ) -> Result<Self> {
        checked_directory(directory)?;
        let content = ContentStore::open(external_root)?;
        let path = directory.join("capture.sqlite");
        // Reserve the destination before copying, never overwrite another job.
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut destination_guard = OwnedCaptureFile::new(path.clone(), directory);
        if let Some((source, expected)) = previous {
            if is_link_like(&fs::symlink_metadata(source)?) {
                return Err(invalid("Capture catalog must not be a link"));
            }
            let mut source = File::open(source)?;
            let length = source.metadata()?.len();
            // Verify the bytes being copied, rather than verifying and reopening.
            let mut digest = sha2::Sha256::new();
            use sha2::Digest;
            use std::io::Read;
            let mut buffer = [0u8; 64 * 1024];
            let mut copied = 0u64;
            loop {
                let n = source.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                destination.write_all(&buffer[..n])?;
                digest.update(&buffer[..n]);
                copied += n as u64;
            }
            let actual: [u8; 32] = digest.finalize().into();
            if copied != length || actual != *expected {
                return Err(invalid("Previous capture catalog differs"));
            }
        }
        destination.sync_all()?;
        drop(destination);
        let db = crate::sqlite_open::open(&path)?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        if previous.is_none() {
            db.execute_batch("CREATE TABLE capture_info(singleton INTEGER PRIMARY KEY CHECK(singleton=1),identity TEXT NOT NULL);
                CREATE TABLE records(key TEXT PRIMARY KEY,hash TEXT NOT NULL,bytes INTEGER NOT NULL);
                CREATE TABLE generated(hash TEXT PRIMARY KEY,bytes INTEGER NOT NULL);
                CREATE TABLE dependencies(record TEXT NOT NULL,hash TEXT NOT NULL,bytes INTEGER NOT NULL,PRIMARY KEY(record,hash));
                CREATE TABLE delta(key TEXT PRIMARY KEY);
                CREATE TABLE original_units(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                CREATE TABLE original_payloads(hash TEXT PRIMARY KEY,bytes INTEGER NOT NULL,record INTEGER NOT NULL);
                CREATE TABLE backup_scope(singleton INTEGER PRIMARY KEY CHECK(singleton=1),unit_count INTEGER NOT NULL);
                CREATE TABLE backup_sections(kind TEXT PRIMARY KEY,body TEXT NOT NULL);")?;
        }
        destination_guard.keep();
        Ok(Self {
            db,
            path,
            content,
            identity: None,
            rebuilt: false,
            finalized: false,
            backup_inputs: None,
        })
    }

    pub(crate) fn install_streamed_backup_inputs(
        &mut self,
        units: OriginalBackupUnits,
        inventory:crate::persistent_store::external_capture::BackupDependencyInventory,
        spool:BackupDependencySpool,
        sections:Vec<super::sections::CapturedSection>,
    ) -> Result<()> {
        let managed_payloads = inventory.payloads.into_iter().filter(|(hash,_)| !inventory.spooled_payloads.contains(hash))
            .map(|(hash,size)| size.map(|size| (hash,size)).ok_or_else(|| invalid("Backup payload size is missing")))
            .collect::<Result<_>>()?;
        let closure = crate::persistent_store::external_capture::BackupDependencyClosure {
            controls:Default::default(),payload_bodies:Default::default(),managed_payloads,record_payloads:inventory.record_payloads,
        };
        if self.identity.is_some() || self.finalized || self.backup_inputs.is_some() {
            return Err(invalid("Backup inputs must precede capture projection"));
        }
        self.backup_inputs = Some(BackupCaptureInputs { units, closure, sections, streamed: Some(spool) });
        Ok(())
    }

    pub(crate) fn backup_sections(&self) -> Result<Vec<super::sections::CapturedSection>> {
        if !self.finalized { return Err(invalid("Backup capture is not durable")); }
        self.original_backup_units()?;
        let mut sections = Vec::new();
        let mut statement = self.db.prepare("SELECT kind,body FROM backup_sections ORDER BY kind")?;
        for row in statement.query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))? {
            let (kind, body) = row?;
            let section: super::sections::CapturedSection = serde_json::from_str(&body)?;
            if section.kind.id() != kind { return Err(invalid("Backup section identity differs")); }
            sections.push(section);
        }
        if sections.len() != 3 { return Err(invalid("Complete backup sections are unavailable")); }
        Ok(sections)
    }

    pub(crate) fn original_backup_units(&self) -> Result<OriginalBackupUnits> {
        if !self.finalized { return Err(invalid("Backup capture is not durable")); }
        let expected: i64 = self.db.query_row("SELECT unit_count FROM backup_scope WHERE singleton=1", [], |row| row.get(0))?;
        let units = OriginalBackupUnits { path: self.path.clone(), count: u64::try_from(expected).map_err(|_| invalid("Original backup count differs"))?, _scratch: None };
        for unit in units.units()? { unit?; }
        Ok(units)
    }

    pub(crate) fn manifest(&self) -> Result<([u8; 32], &Path, &CaptureIdentity)> {
        if !self.finalized {
            return Err(invalid("Capture catalog is not durable"));
        }
        let mut file = File::open(&self.path)?;
        let length = file.metadata()?.len();
        let digest = hash_reader(&mut file, length)
            .map_err(|_| invalid("Capture catalog integrity failed"))?;
        Ok((
            digest,
            &self.path,
            self.identity
                .as_ref()
                .ok_or_else(|| invalid("Capture identity missing"))?,
        ))
    }

    pub(crate) fn durable_reference(
        &self,
        capture_id: &str,
        repository_root: &Path,
    ) -> Result<DurableCaptureReference> {
        if capture_id.is_empty() || capture_id.len() > 1024 || capture_id.contains('\0') {
            return Err(invalid("Capture identity is invalid"));
        }
        let (hash, path, identity) = self.manifest()?;
        let root = repository_root.join("external-storage").canonicalize()?;
        if is_link_like(&fs::symlink_metadata(path)?) {
            return Err(invalid("Capture catalog must not be a link"));
        }
        let path = path.canonicalize()?;
        let relative = path
            .strip_prefix(&root)
            .map_err(|_| invalid("Capture catalog escaped its native directory"))?;
        let relative = relative
            .components()
            .map(|component| match component {
                std::path::Component::Normal(value) => value
                    .to_str()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| invalid("Capture catalog path is invalid")),
                _ => Err(invalid("Capture catalog path is invalid")),
            })
            .collect::<Result<Vec<_>>>()?
            .join("/");
        if relative.is_empty() || relative.len() > 8192 {
            return Err(invalid("Capture catalog path is invalid"));
        }
        Ok(DurableCaptureReference {
            capture_id: capture_id.into(),
            identity: identity.clone(),
            catalog_path: relative,
            catalog_hash: hex::encode(hash),
        })
    }

    pub(crate) fn content_fingerprint(&self, scope: &[u8; 32]) -> Result<[u8; 32]> {
        if !self.finalized {
            return Err(invalid("Capture catalog is not durable"));
        }
        let mut digest = risunest_external_storage_format::format::FingerprintBuilder::new(scope);
        #[cfg(test)]
        crate::persistent_store::hash_work::observe("native_external_capture_fingerprint",b"risunest.external-fingerprint/v1\0".len()+32);
        let mut query = self
            .db
            .prepare("SELECT key,hash FROM records ORDER BY key")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let hash: String = row.get(1)?;
            let hash: [u8; 32] = hex::decode(hash)
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| invalid("Capture record hash is invalid"))?;
            digest
                .push(&key, &hash)
                .map_err(|_| invalid("Capture records are not ordered"))?;
            #[cfg(test)]
            crate::persistent_store::hash_work::update("native_external_capture_fingerprint",40+key.len());
        }
        Ok(digest.finish())
    }
}

impl ContentCaptureSink for CaptureCatalog {
    fn begin(&mut self, identity: &CaptureIdentity, after: Option<i64>) -> Result<()> {
        if self.identity.is_some() {
            return Err(invalid("Capture catalog already started"));
        }
        if let Some(after) = after {
            let encoded: Option<String> = self
                .db
                .query_row(
                    "SELECT identity FROM capture_info WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let old: CaptureIdentity = serde_json::from_str(
                &encoded.ok_or_else(|| invalid("Capture cache requires rebuild"))?,
            )?;
            if old.store_id != identity.store_id
                || old.library_epoch != identity.library_epoch
                || old.generation != identity.generation
                || old.revision != after
            {
                return Err(invalid("Capture cache does not match its PDS cursor"));
            }
        }
        // Nothing staged from an abandoned attempt carries into this one.
        self.content.discard();
        self.db
            .execute_batch("BEGIN IMMEDIATE; DELETE FROM delta;")?;
        self.rebuilt = after.is_none();
        if self.rebuilt {
            self.db.execute_batch(
                "DELETE FROM records; DELETE FROM generated; DELETE FROM dependencies;
                 DELETE FROM original_units; DELETE FROM original_payloads; DELETE FROM backup_scope; DELETE FROM backup_sections;",
            )?;
        }
        self.identity = Some(identity.clone());
        Ok(())
    }
    fn remove_record(&mut self, key: &str) -> Result<()> {
        self.require_writing()?;
        self.db
            .execute("INSERT OR IGNORE INTO delta VALUES(?1)", [key])?;
        self.db.execute("DELETE FROM records WHERE key=?1", [key])?;
        self.db
            .execute("DELETE FROM dependencies WHERE record=?1", [key])?;
        Ok(())
    }
    fn record(&mut self, key: &str, bytes: &[u8]) -> Result<()> {
        self.require_writing()?;
        let hash = hex::encode(hash(bytes));
        self.content.put(&hash, bytes)?;
        self.db.execute(
            "INSERT INTO records VALUES(?1,?2,?3)",
            params![key, hash, bytes.len() as i64],
        )?;
        Ok(())
    }
    fn object(&mut self, hash: &str, bytes: &[u8]) -> Result<()> {
        self.require_writing()?;
        self.content.put(hash, bytes)?;
        self.db.execute(
            "INSERT OR IGNORE INTO generated VALUES(?1,?2)",
            params![hash, bytes.len() as i64],
        )?;
        Ok(())
    }
    fn reference(&mut self, record: &str, hash: &str, size: u64) -> Result<()> {
        self.require_writing()?;
        let size = i64::try_from(size).map_err(|_| invalid("Capture payload length overflow"))?;
        self.db.execute(
            "INSERT OR IGNORE INTO dependencies VALUES(?1,?2,?3)",
            params![record, hash, size],
        )?;
        let stored: i64 = self.db.query_row(
            "SELECT bytes FROM dependencies WHERE record=?1 AND hash=?2",
            params![record, hash],
            |r| r.get(0),
        )?;
        if stored != size {
            return Err(invalid("Capture dependency length differs"));
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.require_writing()?;
        if let Some(inputs) = self.backup_inputs.take() {
            for section in inputs.sections {
                self.db.execute("INSERT INTO backup_sections VALUES(?1,?2)", params![section.kind.id(),serde_json::to_string(&section)?])?;
            }
            self.db.execute("INSERT INTO backup_scope VALUES(1,?1)", [i64::try_from(inputs.units.len()).map_err(|_| invalid("Original backup count overflow"))?])?;
            for unit in inputs.units.units()? {
                let (key,value) = unit?;
                self.db.execute("INSERT INTO original_units VALUES(?1,?2)", params![key.as_str(),serde_json::to_string(&value)?])?;
            }
            if let Some(spool) = inputs.streamed {
                spool.visit(&mut |hash,bytes,role| {
                    self.object(hash,bytes)?;
                    self.reference("backup-original-units",hash,bytes.len() as u64)?;
                    if role == crate::persistent_store::external_capture::BackupBodyRole::Payload {
                        self.db.execute("INSERT INTO original_payloads VALUES(?1,?2,?3)",params![hash,bytes.len() as i64,inputs.closure.record_payloads.contains(hash)])?;
                    }
                    Ok(())
                })?;
            }
            for (hash,body) in inputs.closure.controls {
                self.object(&hash,&body)?;
                self.reference("backup-original-units",&hash,body.len() as u64)?;
            }
            for (hash,body) in inputs.closure.payload_bodies {
                self.object(&hash,&body)?;
                self.reference("backup-original-units",&hash,body.len() as u64)?;
                self.db.execute("INSERT INTO original_payloads VALUES(?1,?2,?3)", params![hash,body.len() as i64,inputs.closure.record_payloads.contains(&hash)])?;
            }
            for (hash,size) in inputs.closure.managed_payloads {
                self.reference("backup-original-units",&hash,size)?;
                self.db.execute("INSERT INTO original_payloads VALUES(?1,?2,?3)", params![hash,i64::try_from(size).map_err(|_| invalid("Original payload size overflow"))?,inputs.closure.record_payloads.contains(&hash)])?;
            }
        }
        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| invalid("Capture not started"))?;
        self.db.execute("INSERT INTO capture_info VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET identity=excluded.identity",[serde_json::to_string(identity)?])?;
        self.db.execute(
            "DELETE FROM generated WHERE hash NOT IN (SELECT hash FROM dependencies)",
            [],
        )?;
        // Bodies before the rows that name them: the catalog only becomes
        // durable once every body it points at already is.
        self.content.commit()?;
        self.db.execute_batch("COMMIT")?;
        // FlushFileBuffers on Windows requires a handle opened for writing.
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)?
            .sync_all()?;
        self.content.sync_objects()?;
        sync_directory(self.path.parent().unwrap())?;
        self.finalized = true;
        Ok(())
    }
}

/// The catalog at `path`, read only, once its bytes and identity match the registration.
fn verified_catalog(
    path: &Path,
    expected_hash: &[u8; 32],
    expected_identity: &CaptureIdentity,
) -> Result<(Connection, CaptureIdentity)> {
    let mut file = crate::trust_boundary::open_regular_source(path)?;
    let size = file.metadata()?.len();
    if hash_reader(&mut file, size).map_err(|_| invalid("Capture catalog integrity failed"))?
        != *expected_hash
    {
        return Err(invalid("Capture catalog integrity failed"));
    }
    let db = crate::sqlite_open::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let encoded: String = db.query_row(
        "SELECT identity FROM capture_info WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    let identity: CaptureIdentity = serde_json::from_str(&encoded)?;
    if identity != *expected_identity {
        return Err(invalid("Capture catalog identity differs"));
    }
    Ok((db, identity))
}

#[cfg(test)]
thread_local! {
    /// Calls to `registered_roots`, so a test can count them.
    pub(crate) static REGISTERED_ROOT_COLLECTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn resolve_registered_catalog(
    repository_root: &Path,
    reference: &DurableCaptureReference,
) -> Result<(PathBuf, PathBuf)> {
    if reference.capture_id.is_empty()
        || reference.capture_id.len() > 1024
        || reference.capture_id.contains('\0')
        || !crate::trust_boundary::is_lower_hex_256(&reference.catalog_hash)
        || reference.catalog_path.is_empty()
        || reference.catalog_path.len() > 8192
    {
        return Err(invalid("Registered capture reference is invalid"));
    }
    let relative = Path::new(&reference.catalog_path);
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(invalid("Registered capture path is invalid"));
    }
    let root = repository_root.join("external-storage").canonicalize()?;
    let path = root.join(relative);
    if is_link_like(&fs::symlink_metadata(&path)?) {
        return Err(invalid("Registered capture is a link"));
    }
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(&root) {
        return Err(invalid("Registered capture escaped its native directory"));
    }
    Ok((canonical, root))
}

/// Each body's length and file as `ContentStore::locate` found it, by identity.
type Located = std::collections::BTreeMap<String, Option<(u64, Option<PathBuf>)>>;

fn capture_roots<'a>(
    references: impl IntoIterator<Item = &'a DurableCaptureReference>,
    repository_root: &Path,
    verify_contents: bool,
    recovery: bool,
    probe: &dyn CancellationProbe,
) -> Result<RegisteredCaptureRoots> {
    let cancelled = || probe.is_cancelled();
    let check = || {
        if cancelled() {
            Err(invalid("Capture validation cancelled"))
        } else {
            Ok(())
        }
    };
    let mut roots = RegisteredCaptureRoots::default();
    let payloads = if recovery {
        Some(crate::asset_repository::PayloadCas::new(repository_root)?)
    } else {
        None
    };
    // Catalogs under one root share its content store, and most of them name
    // the same bodies, so each body is located once per pass.
    let mut stores = std::collections::BTreeMap::<PathBuf, (ContentStore, Located)>::new();
    for reference in references {
        check()?;
        let (path, external_root) = resolve_registered_catalog(repository_root, reference)?;
        let expected: [u8; 32] = hex::decode(&reference.catalog_hash)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| invalid("Registered capture hash is invalid"))?;
        let (db, _) = verified_catalog(&path, &expected, &reference.identity)?;
        let (content, located) = match stores.entry(external_root) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let content = ContentStore::open(entry.key())?;
                entry.insert((content, Default::default()))
            }
        };
        roots.catalogs.insert(path);

        let mut dependencies = db.prepare(
            "SELECT DISTINCT d.hash FROM dependencies d LEFT JOIN generated g ON g.hash=d.hash WHERE g.hash IS NULL",
        )?;
        for hash in dependencies.query_map([], |row| row.get::<_, String>(0))? {
            let hash = hash?;
            if !crate::trust_boundary::is_lower_hex_256(&hash) {
                return Err(invalid("Registered capture payload identity differs"));
            }
            roots.assets.object_hashes.insert(hash);
        }
        if let Some(payloads) = &payloads {
            let mut held = db.prepare(
                "SELECT DISTINCT d.hash,d.bytes FROM dependencies d LEFT JOIN generated g ON g.hash=d.hash WHERE g.hash IS NULL",
            )?;
            for payload in held.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })? {
                let (hash, bytes) = payload?;
                let bytes = u64::try_from(bytes)
                    .map_err(|_| invalid("Registered capture payload length is invalid"))?;
                check()?;
                // Recovery replaces a library with these bytes, so their
                // length is not proof of them.
                if !payloads.holds_exact_object(&hash, bytes, &cancelled)? {
                    return Err(invalid("Recovery capture payload is not held locally"));
                }
            }
        }

        let mut objects = db.prepare(
            "SELECT hash,bytes FROM records UNION SELECT hash,bytes FROM generated ORDER BY hash",
        )?;
        for object in objects.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })? {
            let (hash, bytes) = object?;
            if bytes < 0 || !crate::trust_boundary::is_lower_hex_256(&hash) {
                return Err(invalid("Registered capture object identity differs"));
            }
            check()?;
            let location = match located.get(&hash) {
                Some(location) => location.clone(),
                None => {
                    let location = content.locate(&hash)?;
                    located.insert(hash.clone(), location.clone());
                    location
                }
            };
            match &location {
                Some((size, _)) if *size == bytes as u64 => {}
                Some(_) => return Err(invalid("Capture object length differs")),
                None => return Err(invalid("Capture object is missing")),
            }
            if verify_contents {
                content.validate_checked(&hash, bytes, true, &cancelled)?;
            }
            // Protected in whichever store holds it: a file by its path, a
            // database body by its identity.
            match location.and_then(|(_, path)| path) {
                Some(path) => {
                    roots.logical_records.insert(path);
                }
                None => {
                    roots.content_objects.insert(hash);
                }
            }
        }
    }
    Ok(roots)
}

/// Resolves every physical input owned by durable capture references. Any
/// error is a conservative GC blocker for the caller. Record bodies are not
/// rehashed during inventory; source claims verify them before reading.
pub(crate) fn registered_capture_roots<'a>(
    references: impl IntoIterator<Item = &'a DurableCaptureReference>,
    repository_root: &Path,
) -> Result<RegisteredCaptureRoots> {
    capture_roots(references, repository_root, false, false, &NeverCancelled)
}

#[cfg(test)]
pub(crate) fn validate_capture_sources<'a>(
    references: impl IntoIterator<Item = &'a DurableCaptureReference>,
    repository_root: &Path,
) -> Result<RegisteredCaptureRoots> {
    capture_roots(references, repository_root, true, false, &NeverCancelled)
}

/// What `validate_capture_sources` checks, and that every payload the capture
/// names is held locally at its recorded length and content, so local recovery
/// never has to reach custody. Every body is read in bounded chunks, and
/// `probe` is asked between objects and chunks.
pub(crate) fn validate_recovery_sources<'a>(
    references: impl IntoIterator<Item = &'a DurableCaptureReference>,
    repository_root: &Path,
    probe: &dyn CancellationProbe,
) -> Result<RegisteredCaptureRoots> {
    capture_roots(references, repository_root, true, true, probe)
}

/// GC performs this inventory only when it actually needs roots. Every normal
/// edit/capture can reuse the durable index without scanning all asset aliases.
pub(crate) fn registered_roots(
    db: &Connection,
    repository_root: &Path,
) -> Result<crate::asset_repository::migration_gc::AssetRootSet> {
    #[cfg(test)]
    REGISTERED_ROOT_COLLECTIONS.with(|count| count.set(count.get() + 1));
    let references = registered_references(db, repository_root)?;
    if references.is_empty() {
        return Ok(crate::asset_repository::migration_gc::AssetRootSet::default());
    }
    Ok(registered_capture_roots(&references, repository_root)?.assets)
}

/// Every registered capture catalog, whether or not a job still references it.
pub(crate) fn registered_references(
    db: &Connection,
    repository_root: &Path,
) -> Result<Vec<DurableCaptureReference>> {
    let mut references = Vec::new();
    let mut query=db.prepare("SELECT c.id,c.identity,f.catalog_path,f.file_hash FROM external_storage_capture_files f JOIN external_storage_captures c ON c.id=f.capture_id")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let original = PathBuf::from(row.get::<_, String>(2)?);
        if is_link_like(&fs::symlink_metadata(&original)?) {
            return Err(invalid("Registered capture is a link"));
        }
        let absolute = original.canonicalize()?;
        let root = repository_root.join("external-storage").canonicalize()?;
        let relative = absolute
            .strip_prefix(&root)
            .map_err(|_| invalid("Registered capture escaped its native directory"))?
            .components()
            .map(|component| match component {
                std::path::Component::Normal(value) => value
                    .to_str()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| invalid("Registered capture path is invalid")),
                _ => Err(invalid("Registered capture path is invalid")),
            })
            .collect::<Result<Vec<_>>>()?
            .join("/");
        references.push(DurableCaptureReference {
            capture_id: row.get(0)?,
            identity: serde_json::from_str(&row.get::<_, String>(1)?)?,
            catalog_path: relative,
            catalog_hash: row.get(3)?,
        });
    }
    Ok(references)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> CaptureIdentity {
        CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "library".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision: 1,
        }
    }

    #[test]
    fn failed_catalog_creation_releases_the_reserved_destination_for_retry() {
        let root = tempfile::tempdir().expect("create capture root");
        let source_path = root.path().join("previous.sqlite");
        let source = Connection::open(&source_path).expect("create previous catalog");
        source
            .execute_batch("CREATE TABLE synthetic(value TEXT);")
            .expect("initialize previous catalog");
        drop(source);
        let source_bytes = fs::read(&source_path).expect("read previous catalog");
        let expected = hash(&source_bytes);
        let capture_directory = root.path().join("capture");
        let external = root.path().join("external-storage");

        assert!(CaptureCatalog::create(
            &capture_directory,
            &external,
            Some((&source_path, &[0; 32])),
        )
        .is_err());
        assert!(!capture_directory.join("capture.sqlite").exists());

        let catalog = CaptureCatalog::create(
            &capture_directory,
            &external,
            Some((&source_path, &expected)),
        )
        .expect("retry catalog creation");
        assert!(catalog.path.is_file());
    }

    #[test]
    fn backup_dependency_spool_observes_actual_write_and_read_hashes() {
        use crate::persistent_store::external_capture::BackupBodyRole;
        let directory = tempfile::tempdir().unwrap();
        let mut spool = BackupDependencySpool::new(directory.path()).unwrap();
        let body = b"synthetic observed payload";
        let digest = risunest_sync_wire::hash(body);
        crate::persistent_store::hash_work::reset_hash_work();
        spool.push(&digest,body,BackupBodyRole::Payload).unwrap();
        spool.seal().unwrap();
        spool.visit(&mut |hash,bytes,role| {assert_eq!(hash,digest); assert_eq!(bytes,body); assert_eq!(role,BackupBodyRole::Payload); Ok(())}).unwrap();
        let observed = crate::persistent_store::hash_work::take_hash_work();
        assert!(observed.incomplete.is_empty());
        for domain in ["native_backup_spool_write","native_backup_spool_read"] {
            assert_eq!(observed.domains[domain].calls,1);
            assert_eq!(observed.domains[domain].bytes,body.len() as u64);
        }
    }

    #[test]
    fn backup_dependency_spool_seals_roles_and_rechecks_integrity() {
        use crate::persistent_store::external_capture::BackupBodyRole;
        let directory = tempfile::tempdir().unwrap();
        let mut spool = BackupDependencySpool::new(directory.path()).unwrap();
        let body = b"synthetic streamed control";
        let digest = risunest_sync_wire::hash(body);
        assert!(spool.visit(&mut |_,_,_| Ok(())).is_err());
        assert!(spool.push(&digest,b"different synthetic bytes",BackupBodyRole::Control).is_err());
        spool.push(&digest,body,BackupBodyRole::Control).unwrap();
        assert!(spool.push(&digest,body,BackupBodyRole::Payload).is_err());
        spool.seal().unwrap();
        assert!(spool.push(&digest,body,BackupBodyRole::Control).is_err());
        let mut observed = Vec::new();
        spool.visit(&mut |hash,bytes,role| {observed.push((hash.to_owned(),bytes.to_vec(),role)); Ok(())}).unwrap();
        assert_eq!(observed,vec![(digest.clone(),body.to_vec(),BackupBodyRole::Control)]);
        spool.db.execute("UPDATE bodies SET body=?1 WHERE hash=?2",params![b"synthetic corruption".as_slice(),digest]).unwrap();
        assert!(spool.visit(&mut |_,_,_| Ok(())).is_err());
    }

    #[test]
    fn capture_object_publication_never_replaces_an_existing_identity() {
        let root = tempfile::tempdir().expect("create capture root");
        let capture_directory = root.path().join("capture");
        let mut catalog = CaptureCatalog::create(&capture_directory, root.path(), None)
            .expect("create capture catalog");
        // Above the threshold, so publication still goes through the shared
        // immutable file layout this test is about.
        let payload = vec![7u8; super::super::content_store::SMALL_OBJECT_BYTES + 1];
        let payload = payload.as_slice();
        let expected = hex::encode(hash(payload));
        let published = super::super::content_store::file_location(root.path(), &expected);
        let object_directory = published.parent().unwrap();

        catalog
            .content
            .put(&expected, payload)
            .expect("publish capture object");
        catalog
            .content
            .put(&expected, payload)
            .expect("deduplicate capture object");
        assert_eq!(
            fs::read(&published).expect("read published object"),
            payload
        );
        assert!(fs::read_dir(object_directory)
            .expect("list object directory")
            .all(|entry| !entry
                .expect("read object entry")
                .file_name()
                .to_string_lossy()
            .ends_with(".partial")));
    }

    #[test]
    fn empty_registered_roots_do_not_require_an_external_storage_directory() {
        let root = tempfile::tempdir().unwrap();
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE external_storage_captures(id TEXT,identity TEXT);
             CREATE TABLE external_storage_capture_files(capture_id TEXT,catalog_path TEXT,file_hash TEXT);",
        )
        .unwrap();
        let roots = registered_roots(&db, root.path()).unwrap();
        assert!(roots.object_hashes.is_empty());
        assert!(!root.path().join("external-storage").exists());
    }

    #[test]
    fn durable_capture_roots_keep_catalog_records_and_cas_dependencies_distinct() {
        let root = tempfile::tempdir().unwrap();
        let external = root.path().join("external-storage");
        let capture_directory = external.join("captures/conflict");
        let mut catalog = CaptureCatalog::create(&capture_directory, &external, None)
            .unwrap();
        let identity = identity();
        let record = b"synthetic logical record";
        let record_hash = hex::encode(hash(record));
        let asset_hash = "03".repeat(32);
        // One body of each size, so both protected sets are exercised.
        let generated = vec![9u8; super::super::content_store::SMALL_OBJECT_BYTES + 1];
        let generated_hash = hex::encode(hash(&generated));
        catalog.begin(&identity, None).unwrap();
        catalog.record("root", record).unwrap();
        catalog.object(&generated_hash, &generated).unwrap();
        catalog.reference("root", &generated_hash, generated.len() as u64).unwrap();
        catalog.reference("root", &asset_hash, 9).unwrap();
        catalog.finish().unwrap();
        let reference = catalog
            .durable_reference("capture", root.path())
            .unwrap();

        let roots = registered_capture_roots([&reference], root.path()).unwrap();
        let expected_catalog = capture_directory
            .join("capture.sqlite")
            .canonicalize()
            .unwrap();
        assert!(roots.catalogs.contains(&expected_catalog));
        assert!(roots.content_objects.contains(&record_hash));
        assert!(roots
            .logical_records
            .contains(&super::super::content_store::file_location(&external, &generated_hash).canonicalize().unwrap()));
        assert!(roots.assets.object_hashes.contains(&asset_hash));

        // A body that no longer matches its identity is found by the pass that
        // reads contents, not by the inventory that only counts roots.
        let store = rusqlite::Connection::open(external.join("content.sqlite")).unwrap();
        store
            .execute(
                "UPDATE small_objects SET body=?1 WHERE hash=?2",
                params![b"synthetic damaged record".to_vec(), record_hash],
            )
            .unwrap();
        drop(store);
        // The same length as the original, so only reading it can tell.
        assert!(catalog.content.put(&record_hash, record).is_err());
        assert!(registered_capture_roots([&reference], root.path()).is_ok());
        assert!(validate_capture_sources([&reference], root.path()).is_err());
    }

    /// A finished catalog under `external` holding one record, with what it records
    /// for that record changed by `edit` before it is registered.
    fn registered_record(external: &Path, name: &str, record: &[u8], edit: &str) -> DurableCaptureReference {
        let mut catalog = CaptureCatalog::create(&external.join("captures").join(name), external, None).unwrap();
        catalog.begin(&identity(), None).unwrap();
        catalog.record("root", record).unwrap();
        catalog.finish().unwrap();
        if !edit.is_empty() {
            catalog.db.execute_batch(edit).unwrap();
        }
        catalog.durable_reference(name, external.parent().unwrap()).unwrap()
    }

    fn validation_message(result: Result<RegisteredCaptureRoots>) -> String {
        match result {
            Err(StoreError::Validation { message }) => message,
            other => panic!("expected a validation failure, got {:?}", other.map(|roots| roots.catalogs)),
        }
    }

    #[test]
    fn catalogs_naming_one_body_are_checked_against_it_each() {
        let root = tempfile::tempdir().unwrap();
        let external = root.path().join("external-storage");
        let record = b"synthetic shared record";
        let record_hash = hex::encode(hash(record));
        let first = registered_record(&external, "first", record, "");
        let second = registered_record(&external, "second", record, "");
        let roots = registered_capture_roots([&first, &second], root.path()).unwrap();
        assert_eq!(roots.catalogs.len(), 2);
        assert_eq!(roots.content_objects, BTreeSet::from([record_hash.clone()]));
        assert!(roots.logical_records.is_empty());

        // The body is found once, and a later catalog that records another
        // length for it still fails as it does alone.
        let longer = registered_record(&external, "longer", record, "UPDATE records SET bytes=bytes+1");
        assert_eq!(validation_message(registered_capture_roots([&longer], root.path())), "Capture object length differs");
        assert_eq!(validation_message(registered_capture_roots([&first, &longer], root.path())), "Capture object length differs");

        let other = b"synthetic record only one catalog names";
        let lost = registered_record(&external, "lost", other, "");
        let store = rusqlite::Connection::open(external.join("content.sqlite")).unwrap();
        store.execute("DELETE FROM small_objects WHERE hash=?1", [hex::encode(hash(other))]).unwrap();
        drop(store);
        assert_eq!(validation_message(registered_capture_roots([&first, &lost], root.path())), "Capture object is missing");
        assert!(registered_capture_roots([&first, &second], root.path()).is_ok());
    }

    #[test]
    fn corrupt_catalog_blocks_root_collection() {
        let root = tempfile::tempdir().unwrap();
        let external = root.path().join("external-storage");
        let capture_directory = external.join("captures/conflict");
        let object_directory = external.join("objects");
        let mut catalog = CaptureCatalog::create(&capture_directory, &external, None)
            .unwrap();
        catalog.begin(&identity(), None).unwrap();
        catalog.record("root", b"record").unwrap();
        catalog.finish().unwrap();
        let reference = catalog
            .durable_reference("capture", root.path())
            .unwrap();
        drop(catalog);
        let path = capture_directory.join("capture.sqlite");
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] ^= 1;
        fs::write(path, bytes).unwrap();
        assert!(registered_capture_roots([&reference], root.path()).is_err());
    }
}

#[cfg(test)]
mod original_unit_tests {
    use super::*;
    use risunest_sync_wire::unit::{UnitKey,UnitValue};

    #[test]
    fn original_units_are_ordered_in_bounded_pages_and_outlive_the_source_handle() {
        let directory=tempfile::tempdir().unwrap();
        let source=OriginalBackupUnits::capture(directory.path(),|emit| {
            for index in (0..385).rev() {
                emit(UnitKey::new(&["future-backup",&format!("{index:04}")]).unwrap(),UnitValue::inline(b"true").unwrap())?;
            }
            Ok(())
        }).unwrap();
        assert_eq!(source.len(),385);
        let path=source.path.clone();
        let mut rows=source.units().unwrap();
        drop(source);
        for index in 0..385 {
            assert_eq!(rows.next().unwrap().unwrap().0,UnitKey::new(&["future-backup",&format!("{index:04}")]).unwrap());
            assert!(rows.page.len()<128);
        }
        assert!(rows.next().is_none());
        assert!(path.exists());
        drop(rows);
        assert!(!path.exists());
    }

    #[test]
    fn repeated_original_key_or_failed_producer_leaves_no_partial_source() {
        let directory=tempfile::tempdir().unwrap();
        let key=UnitKey::new(&["future-backup","duplicate"]).unwrap();
        assert!(OriginalBackupUnits::capture(directory.path(),|emit| {
            emit(key.clone(),UnitValue::Deleted)?;
            emit(key,UnitValue::Deleted)
        }).is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(),0);
        assert!(OriginalBackupUnits::capture(directory.path(),|emit| {
            emit(UnitKey::new(&["future-backup","partial"]).unwrap(),UnitValue::Deleted)?;
            Err(invalid("cancelled synthetic producer"))
        }).is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(),0);
    }
}
