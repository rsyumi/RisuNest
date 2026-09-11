//! Cold administration backup. The final manifest is the completion marker.
use super::{
    objects::{check_path, sync_directory},
    Store,
};
use crate::{Error, Result};
use risunest_sync_wire::{canonical, RemoteHead, Sequence};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupManifest {
    pub schema: String,
    pub head: RemoteHead,
    pub metadata_hash: String,
    pub objects: Sequence,
}
fn copy_exact(source: &Path, target: &Path, expected: Option<&str>) -> Result<String> {
    check_path(source)?;
    check_path(target)?;
    fs::create_dir_all(
        target
            .parent()
            .ok_or(Error::new("invalid-backup-path", 400))?,
    )?;
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let mut buffer = [0u8; 64 * 1024];
    let mut digest = Sha256::new();
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n])?;
        digest.update(&buffer[..n]);
    }
    let digest = format!("{:x}", digest.finalize());
    if expected.is_some_and(|hash| hash != digest) {
        return Err(Error::new("corrupt-backup-object", 409));
    }
    output.sync_all()?;
    sync_directory(target.parent().unwrap())?;
    Ok(digest)
}
fn file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn copy_objects(db: &Connection, source: &Path, target: &Path) -> Result<u64> {
    let mut statement = db.prepare("SELECT hash,size FROM objects ORDER BY hash")?;
    let mut rows = statement.query([])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        let hash: String = row.get(0)?;
        risunest_sync_wire::validate_hash(&hash)?;
        let size: i64 = row.get(1)?;
        let from = source.join("objects").join(&hash[..2]).join(&hash);
        let to = target.join("objects").join(&hash[..2]).join(&hash);
        if size < 0 || fs::metadata(&from)?.len() != size as u64 {
            return Err(Error::new("corrupt-backup-object", 409));
        }
        copy_exact(&from, &to, Some(&hash))?;
        count += 1;
    }
    sync_directory(&target.join("objects"))?;
    Ok(count)
}
impl Store {
    pub fn backup(&self, destination: &Path) -> Result<BackupManifest> {
        if !destination.is_absolute() {
            return Err(Error::new("absolute-backup-dir-required", 400));
        }
        check_path(destination)?;
        if destination.exists() {
            return Err(Error::new("backup-destination-exists", 409));
        }
        let _gate = self
            .objects_gate
            .lock()
            .map_err(|_| Error::new("storage-unavailable", 503))?;
        let db = self.db()?;
        fs::create_dir(destination)?;
        fs::create_dir(destination.join("objects"))?;
        let metadata = destination.join("metadata.sqlite");
        db.execute(
            "VACUUM INTO ?1",
            [metadata
                .to_str()
                .ok_or(Error::new("invalid-backup-path", 400))?],
        )?;
        // FlushFileBuffers on Windows requires a writable file handle.
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&metadata)?
            .sync_all()
            .map_err(|_| Error::new("backup-metadata-flush", 503))?;
        let objects = copy_objects(&db, &self.root, destination)?;
        let manifest = BackupManifest {
            schema: "risunest-sync-backup-v1".into(),
            head: Self::read_head(&db)?,
            metadata_hash: file_hash(&metadata)?,
            objects: objects.into(),
        };
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination.join("backup.json"))?;
        marker.write_all(&canonical::encode(&manifest)?)?;
        marker.sync_all()?;
        sync_directory(destination)?;
        if let Some(parent) = destination.parent() {
            sync_directory(parent)?;
        }
        Ok(manifest)
    }
    pub fn restore_backup(source: &Path, destination: &Path) -> Result<Self> {
        if !source.is_absolute() || !destination.is_absolute() {
            return Err(Error::new("absolute-backup-dir-required", 400));
        }
        check_path(source)?;
        check_path(destination)?;
        if destination.exists() {
            return Err(Error::new("restore-destination-exists", 409));
        }
        let marker = source.join("backup.json");
        check_path(&marker)?;
        if fs::metadata(&marker)?.len() > risunest_sync_wire::MAX_METADATA_BYTES as u64 {
            return Err(Error::new("invalid-backup", 409));
        }
        let manifest: BackupManifest =
            canonical::decode(&fs::read(marker)?, risunest_sync_wire::MAX_METADATA_BYTES)?;
        if manifest.schema != "risunest-sync-backup-v1" {
            return Err(Error::new("invalid-backup", 409));
        }
        manifest.head.validate()?;
        risunest_sync_wire::validate_hash(&manifest.metadata_hash)?;
        let source_db = source.join("metadata.sqlite");
        check_path(&source_db)?;
        if file_hash(&source_db)? != manifest.metadata_hash {
            return Err(Error::new("corrupt-backup-metadata", 409));
        }
        let db = Connection::open_with_flags(&source_db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let integrity: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if integrity != "ok" || Self::read_head(&db)? != manifest.head {
            return Err(Error::new("corrupt-backup-metadata", 409));
        }
        fs::create_dir(destination)?;
        fs::create_dir(destination.join("objects"))?;
        let count = copy_objects(&db, source, destination)?;
        if Sequence::from(count) != manifest.objects {
            return Err(Error::new("corrupt-backup-metadata", 409));
        }
        copy_exact(
            &source_db,
            &destination.join("metadata.sqlite"),
            Some(&manifest.metadata_hash),
        )?;
        let store = Self::open(destination)?;
        store.rotate_restored_epoch()?;
        Ok(store)
    }
}
