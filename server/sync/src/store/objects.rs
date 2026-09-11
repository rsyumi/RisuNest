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
        let db = self.db()?;
        Self::require_device(&db, device)?;
        db.execute(
            "INSERT INTO objects(hash,size) VALUES(?1,?2) ON CONFLICT(hash) DO NOTHING",
            params![digest, bytes.len() as i64],
        )?;
        Self::lease_object(&db, device, digest)?;
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
