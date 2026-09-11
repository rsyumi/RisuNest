use super::Store;
use crate::{Error, Result};
use risunest_sync_wire::{delta::MAX_TARGET_BYTES, hash, validate_hash};
use rusqlite::{params, OptionalExtension};
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
};

// The private data directory must be owned by the daemon user. Also reject existing
// symlinks/junctions in every path component; HTTP clients never supply paths.
pub(super) fn check_path(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                #[cfg(windows)]
                let linked = {
                    use std::os::windows::fs::MetadataExt;
                    meta.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let linked = meta.file_type().is_symlink();
                if linked {
                    return Err(Error::new("unsafe-storage-path", 400));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub(super) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
pub(super) fn publish(from: &Path, to: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        // Both paths are private, checked, same-volume server paths.
        if unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)?;
        sync_directory(to.parent().unwrap())?;
    }
    Ok(())
}
impl Store {
    pub fn open_object(&self, digest: &str) -> Result<(File, u64)> {
        let _gate = self
            .objects_gate
            .lock()
            .map_err(|_| Error::new("storage-unavailable", 503))?;
        let size = self
            .object_size(digest)?
            .ok_or(Error::new("object-not-found", 404))?;
        let file = File::open(self.object_path(digest)?)?;
        if file.metadata()?.len() != size {
            return Err(Error::new("corrupt-object", 503));
        }
        Ok((file, size))
    }
    pub(super) fn object_path(&self, digest: &str) -> Result<std::path::PathBuf> {
        validate_hash(digest)?;
        let path = self.root.join("objects").join(&digest[..2]).join(digest);
        check_path(&path)?;
        Ok(path)
    }
    pub fn put_object(&self, device: &super::Device, digest: &str, bytes: &[u8]) -> Result<()> {
        validate_hash(digest)?;
        if bytes.len() > MAX_TARGET_BYTES {
            return Err(Error::new("object-too-large", 413));
        }
        {
            Self::require_device(&*self.db()?, device)?;
        }
        if hash(bytes) != digest {
            return Err(Error::new("hash-mismatch", 400));
        }
        let destination = self.object_path(digest)?;
        fs::create_dir_all(destination.parent().unwrap())?;
        sync_directory(&self.root.join("objects"))?;
        let staging = self.root.join("staging");
        check_path(&staging)?;
        let mut temp = tempfile::NamedTempFile::new_in(staging)?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        // No DB lock during upload, hashing, flush or rename. Concurrent identical
        // publishes replace only with independently verified identical bytes.
        let _gate = self
            .objects_gate
            .lock()
            .map_err(|_| Error::new("storage-unavailable", 503))?;
        publish(temp.path(), &destination)?;
        let mut db = self.db()?;
        Self::require_device(&db, device)?;
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO objects(hash,size) VALUES(?1,?2) ON CONFLICT(hash) DO NOTHING",
            params![digest, bytes.len() as i64],
        )?;
        Self::lease_object(&tx, device, digest)?;
        tx.commit()?;
        Ok(())
    }
    pub fn object_size(&self, digest: &str) -> Result<Option<u64>> {
        validate_hash(digest)?;
        let size: Option<i64> = self
            .reader()?
            .query_row("SELECT size FROM objects WHERE hash=?1", [digest], |r| {
                r.get(0)
            })
            .optional()?;
        size.map(|s| u64::try_from(s).map_err(|_| Error::new("corrupt-metadata", 503)))
            .transpose()
    }
    pub fn get_object(&self, digest: &str) -> Result<Vec<u8>> {
        let size = self
            .object_size(digest)?
            .ok_or(Error::new("object-not-found", 404))?;
        if size > MAX_TARGET_BYTES as u64 {
            return Err(Error::new("corrupt-object", 503));
        }
        let path = self.object_path(digest)?;
        let file = File::open(path)?;
        use std::io::Read;
        let mut bytes = Vec::new();
        file.take(size + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 != size || hash(&bytes) != digest {
            return Err(Error::new("corrupt-object", 503));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_capacity_failure_cannot_publish_partial_object_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::init(directory.path()).unwrap();
        let credential = store.add_device().unwrap();
        let device = super::super::Device {
            id: credential.device_id,
        };
        let head = store.head().unwrap();
        {
            let db = store.db().unwrap();
            let pages: i64 = db.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap();
            db.pragma_update(None, "max_page_count", pages).unwrap();
            // Force the real SQLite SQLITE_FULL path without filling a host
            // volume or changing storage outside this synthetic directory.
            let failure = db
                .execute(
                    "CREATE TABLE synthetic_capacity_probe AS SELECT zeroblob(1048576) AS value",
                    [],
                )
                .unwrap_err();
            assert_eq!(
                failure.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DiskFull)
            );
        }
        let mut failed = None;
        for index in 0..1024 {
            let bytes = format!("synthetic capacity object {index}");
            let digest = hash(bytes.as_bytes());
            match store.put_object(&device, &digest, bytes.as_bytes()) {
                Ok(()) => (),
                Err(error) => {
                    assert_eq!(error.code, "metadata-storage");
                    failed = Some(digest);
                    break;
                }
            }
        }
        let failed = failed.expect("bounded SQLite capacity must be exhausted");
        assert!(store.object_size(&failed).unwrap().is_none());
        let leased: bool = store
            .db()
            .unwrap()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM object_leases WHERE hash=?1)",
                [&failed],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!leased, "object and lease must roll back together");
        assert_eq!(store.head().unwrap(), head);
        drop(store);
        let reopened = Store::open(directory.path()).unwrap();
        assert_eq!(reopened.head().unwrap(), head);
        assert!(reopened.object_size(&failed).unwrap().is_none());
        assert_eq!(
            reopened
                .db()
                .unwrap()
                .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }
}
