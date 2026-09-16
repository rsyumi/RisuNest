//! Classify restore payloads without turning preserved source files into live CAS registrations.
use super::*;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreservationReport {
    pub(crate) files: String,
    pub(crate) bytes: String,
    pub(crate) reason: &'static str,
    pub(crate) deletable: bool,
    pub(crate) path: String,
}

pub(crate) struct RestoreInventory {
    pub(crate) db: Connection,
    directory: tempfile::TempDir,
}

impl RestoreInventory {
    pub(crate) fn build(
        archive: &VerifiedArchive,
        owned: &Path,
        probe: &dyn CancellationProbe,
    ) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("restore-inventory-")
            .tempdir_in(owned)?;
        let db = Connection::open(directory.path().join("inventory.sqlite"))?;
        db.execute_batch("PRAGMA cache_size=-16384; PRAGMA temp_store=FILE; CREATE TABLE live_objects (hash TEXT PRIMARY KEY, owner INTEGER NOT NULL); BEGIN IMMEDIATE;")?;
        let mut statement=archive.db.prepare("SELECT DISTINCT lower(hex(object_hash)),kind='owner' FROM files WHERE kind IN ('asset','inlay','owner') AND state='present'")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            let hash: String = row.get(0)?;
            let owner: bool = row.get(1)?;
            db.execute("INSERT INTO live_objects VALUES(?1,?2) ON CONFLICT(hash) DO UPDATE SET owner=max(owner,excluded.owner)",params![hash,owner])?;
        }
        let mut statement=archive.db.prepare("SELECT DISTINCT manifest_hash FROM asset_owner_heads WHERE present=1 ORDER BY manifest_hash")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            let hash: String = row.get(0)?;
            let (mut input, size) = archive.open_object(&hash)?;
            if size > 64 * 1024 * 1024 {
                return Err(Error::Invalid("owner manifest exceeds decoding limit"));
            }
            let mut bytes = Vec::with_capacity(size as usize);
            input.read_to_end(&mut bytes)?;
            let entries =
                crate::asset_repository::owner_manifest_codec::decode_owner_manifest(&bytes)
                    .map_err(|_| Error::Invalid("invalid owner manifest"))?;
            for entry in entries {
                check(probe)?;
                if let Some(hash) = entry.payload_hash {
                    db.execute(
                        "INSERT OR IGNORE INTO live_objects VALUES(?1,0)",
                        [hex::encode(hash)],
                    )?;
                }
            }
        }
        db.execute_batch("COMMIT")?;
        Ok(Self { db, directory })
    }

    /// Publish only non-live source payloads below a dedicated directory that neither BlobStore
    /// nor sync enumerate. The index retains original keys and metadata; file paths are hashes.
    pub(crate) fn preserve(
        &self,
        archive: &VerifiedArchive,
        repository: &Path,
        probe: &dyn CancellationProbe,
    ) -> Result<Option<PreservationReport>> {
        let stage = self.directory.path().join("preserved");
        fs::create_dir(&stage)?;
        fs::create_dir(stage.join("objects"))?;
        let index = Connection::open(stage.join("index.sqlite"))?;
        index.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE source_files (logical_key TEXT PRIMARY KEY,object_hash TEXT NOT NULL,metadata TEXT NOT NULL,byte_length INTEGER NOT NULL,reason TEXT NOT NULL); BEGIN IMMEDIATE;")?;
        let mut statement=archive.db.prepare("SELECT f.logical_key,lower(hex(f.object_hash)),f.metadata,o.byte_length FROM files f JOIN objects o ON o.sha256=f.object_hash WHERE f.kind='preserved' AND f.state='present' ORDER BY f.logical_key")?;
        let mut rows = statement.query([])?;
        let mut count = 0_u64;
        let mut bytes = 0_u64;
        while let Some(row) = rows.next()? {
            check(probe)?;
            let original: String = row.get(0)?;
            let hash: String = row.get(1)?;
            let live: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM live_objects WHERE hash=?1)",
                [&hash],
                |r| r.get(0),
            )?;
            let metadata: String = row.get(2)?;
            let size = sql_u64(row.get(3)?)?;
            // A live canonical CAS path is recreated by installation. Equal bytes alone do
            // not prove that another source path or its metadata can be discarded.
            let canonical = format!("assets-v2/objects/{}/{}", &hash[..2], &hash[2..]);
            if live && original == canonical && metadata == "{\"storage\":\"cas\"}" {
                continue;
            }
            let key = if original.starts_with("source-preservation/") {
                original
            } else {
                format!(
                    "source-preservation/{}/{}",
                    archive.manifest.capture_id, original
                )
            };
            let object = stage.join("objects").join(&hash);
            if !object.try_exists()? {
                let mut output = fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(object)?;
                archive.copy_object(&hash, &mut output, probe)?;
                output.sync_all()?;
                bytes = bytes
                    .checked_add(size)
                    .ok_or(Error::Invalid("preserved byte count overflow"))?;
            }
            index.execute(
                "INSERT INTO source_files VALUES(?1,?2,?3,?4,'not-required-by-library')",
                params![key, hash, metadata, size as i64],
            )?;
            count += 1;
        }
        index.execute_batch("COMMIT")?;
        drop(index);
        if count == 0 {
            return Ok(None);
        }
        let root = repository.join("source-preservation");
        ensure_directory(&root)?;
        let destination = root.join(uuid::Uuid::new_v4().to_string());
        crate::trust_boundary::sync_directory(&stage.join("objects"))?;
        crate::trust_boundary::sync_directory(&stage)?;
        #[cfg(any(windows, target_os = "android"))]
        crate::trust_boundary::rename_without_replace(&stage, &destination)?;
        #[cfg(not(any(windows, target_os = "android")))]
        fs::rename(&stage, &destination)?;
        crate::trust_boundary::sync_directory(&root)?;
        crate::trust_boundary::sync_directory(self.directory.path())?;
        Ok(Some(PreservationReport {
            files: count.to_string(),
            bytes: bytes.to_string(),
            reason: "not-required-by-library",
            deletable: true,
            path: destination.to_string_lossy().into_owned(),
        }))
    }
}

fn ensure_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !crate::trust_boundary::is_link_like(&metadata) => {
            Ok(())
        }
        Ok(_) => Err(Error::Invalid("invalid preservation directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

impl Catalog {
    pub(super) fn capture_preserved_sources(
        &self,
        repository: &Path,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        let root = repository.join("source-preservation");
        match fs::symlink_metadata(&root) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(_) => ensure_directory(&root)?,
        }
        for entry in fs::read_dir(root)? {
            check(probe)?;
            let entry = entry?;
            let directory = entry.path();
            ensure_directory(&directory)?;
            let index_path = directory.join("index.sqlite");
            check_regular(&index_path)?;
            let index = Connection::open_with_flags(index_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            index.execute_batch(
                "PRAGMA trusted_schema=OFF; PRAGMA query_only=ON; PRAGMA cache_size=-16384",
            )?;
            let mut statement=index.prepare("SELECT logical_key,object_hash,metadata,byte_length FROM source_files ORDER BY logical_key")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                check(probe)?;
                let key: String = row.get(0)?;
                let hash: String = row.get(1)?;
                let metadata: String = row.get(2)?;
                let size = sql_u64(row.get(3)?)?;
                if !hash_valid(&hash) || !key.starts_with("source-preservation/") {
                    return Err(Error::Invalid("invalid preserved source descriptor"));
                }
                let existing:Option<(String,String)>=self.db.query_row("SELECT lower(hex(object_hash)),metadata FROM files WHERE kind='preserved' AND logical_key=?1",[&key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                if let Some(existing) = existing {
                    if existing != (hash.clone(), metadata.clone()) {
                        return Err(Error::Invalid("conflicting preserved source records"));
                    }
                    continue;
                }
                let objects = directory.join("objects");
                ensure_directory(&objects)?;
                let path: PathBuf = objects.join(&hash);
                check_regular(&path)?;
                self.add_pinned_file("preserved", &key, &metadata, &path, size, &hash, probe)?;
            }
        }
        Ok(())
    }
}
fn check_regular(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || crate::trust_boundary::is_link_like(&metadata) {
        return Err(Error::Invalid("invalid preserved source file"));
    }
    Ok(())
}
