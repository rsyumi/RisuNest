//! Local canonical capture spool. The SQLite catalog contains only hashes and
//! lengths; unchanged record bodies are immutable shared files, never copied as
//! part of another small-edit capture. This module performs no remote requests.
use crate::{
    persistent_store::{
        content_capture::ContentCaptureSink, sync_selection::CaptureIdentity, StoreError,
    },
    trust_boundary::{is_link_like, sync_directory},
};
use risunest_external_storage_format::content_identity::{hash, hash_reader};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
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

pub(crate) struct CaptureCatalog {
    pub(crate) db: Connection,
    path: PathBuf,
    object_directory: PathBuf,
    identity: Option<CaptureIdentity>,
    pub(crate) rebuilt: bool,
    finalized: bool,
}

/// Both paths are selected by the native owner. Previous catalogs must have
/// an authoritative PDS manifest hash; an unregistered receipt is not sufficient.
impl CaptureCatalog {
    fn require_writing(&self) -> Result<()> {
        if self.identity.is_none() || self.finalized || self.db.is_autocommit() {
            return Err(invalid("Capture catalog is not writable"));
        }
        Ok(())
    }
    pub(crate) fn create(
        directory: &Path,
        object_directory: &Path,
        previous: Option<(&Path, &[u8; 32])>,
    ) -> Result<Self> {
        checked_directory(directory)?;
        checked_directory(object_directory)?;
        let path = directory.join("capture.sqlite");
        // Reserve the destination before copying, never overwrite another job.
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
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
        let db = Connection::open(&path)?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        if previous.is_none() {
            db.execute_batch("CREATE TABLE capture_info(singleton INTEGER PRIMARY KEY CHECK(singleton=1),identity TEXT NOT NULL);
                CREATE TABLE records(key TEXT PRIMARY KEY,hash TEXT NOT NULL,bytes INTEGER NOT NULL);
                CREATE TABLE generated(hash TEXT PRIMARY KEY,bytes INTEGER NOT NULL);
                CREATE TABLE dependencies(record TEXT NOT NULL,hash TEXT NOT NULL,bytes INTEGER NOT NULL,PRIMARY KEY(record,hash));
                CREATE TABLE delta(key TEXT PRIMARY KEY);")?;
        }
        Ok(Self {
            db,
            path,
            object_directory: object_directory.into(),
            identity: None,
            rebuilt: false,
            finalized: false,
        })
    }

    fn write_object(&self, expected: &str, bytes: &[u8]) -> Result<()> {
        if hex::encode(hash(bytes)) != expected {
            return Err(invalid("Capture object identity differs"));
        }
        let path = self.object_directory.join(expected);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(bytes)?;
                file.sync_all()?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if is_link_like(&fs::symlink_metadata(&path)?) {
                    return Err(invalid("Capture object must not be a link"));
                }
                let mut file = File::open(path)?;
                let actual = hash_reader(&mut file, bytes.len() as u64)
                    .map_err(|_| invalid("Existing capture object differs"))?;
                if hex::encode(actual) != expected {
                    return Err(invalid("Existing capture object differs"));
                }
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
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

    pub(crate) fn content_fingerprint(&self, scope: &[u8; 32]) -> Result<[u8; 32]> {
        if !self.finalized {
            return Err(invalid("Capture catalog is not durable"));
        }
        let mut digest = risunest_external_storage_format::format::FingerprintBuilder::new(scope);
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
        self.db
            .execute_batch("BEGIN IMMEDIATE; DELETE FROM delta;")?;
        self.rebuilt = after.is_none();
        if self.rebuilt {
            self.db.execute_batch(
                "DELETE FROM records; DELETE FROM generated; DELETE FROM dependencies;",
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
        self.write_object(&hash, bytes)?;
        self.db.execute(
            "INSERT INTO records VALUES(?1,?2,?3)",
            params![key, hash, bytes.len() as i64],
        )?;
        Ok(())
    }
    fn object(&mut self, hash: &str, bytes: &[u8]) -> Result<()> {
        self.require_writing()?;
        self.write_object(hash, bytes)?;
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
        let identity = self
            .identity
            .as_ref()
            .ok_or_else(|| invalid("Capture not started"))?;
        self.db.execute("INSERT INTO capture_info VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET identity=excluded.identity",[serde_json::to_string(identity)?])?;
        self.db.execute(
            "DELETE FROM generated WHERE hash NOT IN (SELECT hash FROM dependencies)",
            [],
        )?;
        self.db.execute_batch("COMMIT")?;
        // FlushFileBuffers on Windows requires a handle opened for writing.
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)?
            .sync_all()?;
        sync_directory(&self.object_directory)?;
        sync_directory(self.path.parent().unwrap())?;
        self.finalized = true;
        Ok(())
    }
}

/// GC performs this inventory only when it actually needs roots. Every normal
/// edit/capture can reuse the durable index without scanning all asset aliases.
pub(crate) fn registered_roots(
    db: &Connection,
    repository_root: &Path,
) -> Result<crate::asset_repository::migration_gc::AssetRootSet> {
    let mut roots = crate::asset_repository::migration_gc::AssetRootSet::default();
    let mut query=db.prepare("SELECT f.catalog_path,f.file_hash FROM external_storage_capture_files f JOIN external_storage_captures c ON c.id=f.capture_id")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        let expected: String = row.get(1)?;
        let path = PathBuf::from(raw);
        if is_link_like(&fs::symlink_metadata(&path)?) {
            return Err(invalid("Registered capture is a link"));
        }
        let root = repository_root.join("external-storage").canonicalize()?;
        if !path.canonicalize()?.starts_with(root) {
            return Err(invalid("Registered capture escaped its spool"));
        }
        let mut file = File::open(&path)?;
        let length = file.metadata()?.len();
        let digest = hash_reader(&mut file, length)
            .map_err(|_| invalid("Registered capture integrity failed"))?;
        if hex::encode(digest) != expected {
            return Err(invalid("Registered capture integrity failed"));
        }
        let catalog =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut dependencies=catalog.prepare("SELECT DISTINCT d.hash FROM dependencies d LEFT JOIN generated g ON g.hash=d.hash WHERE g.hash IS NULL")?;
        for hash in dependencies.query_map([], |r| r.get::<_, String>(0))? {
            let hash = hash?;
            if !crate::trust_boundary::is_lower_hex_256(&hash) {
                return Err(invalid("Registered capture payload identity differs"));
            }
            roots.object_hashes.insert(hash);
        }
    }
    Ok(roots)
}
