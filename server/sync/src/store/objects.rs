use super::Store;
#[cfg(all(test, feature = "source-observer"))]
use crate::source_observer::small_object_store;
#[cfg(all(test, feature = "source-observer"))]
pub use crate::source_observer::Body;
use crate::{Error, Result};
#[cfg(not(all(test, feature = "source-observer")))]
use risunest_small_object_store as small_object_store;
#[cfg(not(all(test, feature = "source-observer")))]
pub use risunest_small_object_store::Body;
use risunest_sync_wire::{delta::MAX_TARGET_BYTES, hash, validate_hash};
use rusqlite::{params, Connection, OptionalExtension};
#[cfg(any(not(all(test, feature = "source-observer")), unix))]
use std::fs::File;
use std::{fs, io::Write, path::Path};

/// Bodies at or below this are written into the metadata database. The
/// threshold decides where a new body goes; where an existing one lives is
/// read from its row, never inferred from its size.
pub(super) const SMALL_OBJECT_BYTES: usize = 64 * 1024;
pub(super) use small_object_store::delete_batch as delete_inline;

// Four workers improved file-backed persistence in release measurements with
// depths 1, 2 and 4. Keep this bounded independently of the request's body count.
const FILE_IO_WORKERS: usize = 4;

fn spawn_file_worker<'scope, 'env, F, T>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    worker: F,
) -> std::io::Result<std::thread::ScopedJoinHandle<'scope, T>>
where
    F: FnOnce() -> T + Send + 'scope,
    T: Send + 'scope,
{
    #[cfg(test)]
    if frame_metrics::SPAWN_FAILURE_MASK.with(|mask| {
        let current = mask.get();
        mask.set(current >> 1);
        current & 1 != 0
    }) {
        return Err(std::io::ErrorKind::WouldBlock.into());
    }
    std::thread::Builder::new().spawn_scoped(scope, worker)
}

enum FileWork<'scope, T> {
    Worker(std::thread::ScopedJoinHandle<'scope, Result<Vec<T>>>),
    Current(Result<Vec<T>>),
}

fn file_batch<T: Sync, U: Send>(
    items: &[T],
    operation: impl Fn(&T) -> Result<U> + Sync,
) -> Result<Vec<U>> {
    #[cfg(not(test))]
    let depth = FILE_IO_WORKERS;
    #[cfg(test)]
    let depth = frame_metrics::FILE_WORKERS.with(std::cell::Cell::get);
    if items.len() <= 1 || depth == 1 {
        return items.iter().map(operation).collect();
    }
    std::thread::scope(|scope| {
        let workers = items
            .chunks(items.len().div_ceil(depth))
            .map(|group| {
                let operation = &operation;
                #[cfg(all(test, feature = "source-observer"))]
                let observed = crate::source_observer::worker();
                #[cfg(test)]
                let samples = frame_metrics::SAMPLES.with(|samples| samples.borrow().clone());
                let worker = spawn_file_worker(scope, move || {
                    #[cfg(all(test, feature = "source-observer"))]
                    let _observed = observed.enter();
                    #[cfg(test)]
                    frame_metrics::SAMPLES.with(|current| *current.borrow_mut() = samples);
                    group.iter().map(operation).collect::<Result<Vec<_>>>()
                });
                match worker {
                    Ok(worker) => FileWork::Worker(worker),
                    // Thread exhaustion must not unwind while holding objects_gate.
                    Err(_) => FileWork::Current(group.iter().map(operation).collect()),
                }
            })
            .collect::<Vec<_>>();
        // Every worker must end before a failure releases staging files or the publication gate.
        let results = workers
            .into_iter()
            .map(|work| match work {
                FileWork::Worker(worker) => worker
                    .join()
                    .unwrap_or_else(|_| Err(Error::new("storage-unavailable", 503))),
                FileWork::Current(result) => result,
            })
            .collect::<Vec<_>>();
        let mut completed = Vec::with_capacity(items.len());
        for result in results {
            completed.extend(result?);
        }
        Ok(completed)
    })
}

pub(super) fn body_error(error: small_object_store::StoreError) -> Error {
    #[cfg(all(test, feature = "source-observer"))]
    crate::source_observer::note_current_violation("inline-body-error");
    match error {
        small_object_store::StoreError::TooLarge => Error::new("object-too-large", 413),
        small_object_store::StoreError::Database(error) => error.into(),
        _ => Error::new("corrupt-object", 503),
    }
}

/// How much of `digest` this store holds and which of its two stores holds it.
/// The column is the authority: an inline row with no body is corruption, not
/// a reason to look for a file.
pub(super) fn placement(db: &Connection, digest: &str) -> Result<Option<(u64, bool)>> {
    validate_hash(digest)?;
    let row: Option<(i64, String)> = db
        .query_row(
            "SELECT size,storage FROM objects WHERE hash=?1",
            [digest],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((size, storage)) = row else {
        return Ok(None);
    };
    let size = u64::try_from(size).map_err(|_| Error::new("corrupt-metadata", 503))?;
    match storage.as_str() {
        "inline" => Ok(Some((size, true))),
        "file" => Ok(Some((size, false))),
        _ => Err(Error::new("corrupt-metadata", 503)),
    }
}

// The private data directory must be owned by the daemon user. Also reject existing
// symlinks/junctions in every path component; HTTP clients never supply paths.
pub(super) fn check_path(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        unlinked_metadata(ancestor)?;
    }
    Ok(())
}
/// The metadata of `path` itself, or `None` when it does not exist. A symlink
/// or junction is refused.
fn unlinked_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    #[cfg(test)]
    frame_metrics::PATH_CHECKS.with(|count| count.set(count.get() + 1));
    match fs::symlink_metadata(path) {
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
            Ok(Some(meta))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
/// Object file paths for one operation over many objects. The data directory,
/// its ancestors and `objects` are checked on first use and each shard once, so
/// each object adds only the check of its own file. It lives for one operation
/// and is never kept, so a link made between operations is still refused.
pub(crate) struct ObjectPaths<'a> {
    root: &'a Path,
    objects: std::sync::OnceLock<std::path::PathBuf>,
    shards: std::sync::Mutex<std::collections::BTreeSet<String>>,
}
impl ObjectPaths<'_> {
    /// The checked path of `digest`'s file.
    pub(crate) fn path(&self, digest: &str) -> Result<std::path::PathBuf> {
        Ok(self.file(digest)?.0)
    }
    /// The metadata of `digest`'s file, or `None` when it does not exist.
    pub(crate) fn metadata(&self, digest: &str) -> Result<Option<fs::Metadata>> {
        Ok(self.file(digest)?.1)
    }
    fn file(&self, digest: &str) -> Result<(std::path::PathBuf, Option<fs::Metadata>)> {
        validate_hash(digest)?;
        let objects = match self.objects.get() {
            Some(objects) => objects,
            None => {
                let objects = self.root.join("objects");
                check_path(&objects)?;
                self.objects.get_or_init(|| objects)
            }
        };
        let shard = objects.join(&digest[..2]);
        let mut shards = self
            .shards
            .lock()
            .map_err(|_| Error::new("storage-unavailable", 503))?;
        if !shards.contains(&digest[..2]) {
            unlinked_metadata(&shard)?;
            shards.insert(digest[..2].to_owned());
        }
        drop(shards);
        let path = shard.join(digest);
        let metadata = unlinked_metadata(&path)?;
        Ok((path, metadata))
    }
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
    pub fn open_object(&self, digest: &str) -> Result<(Body, u64)> {
        #[cfg(all(test, feature = "source-observer"))]
        let _observed = crate::source_observer::operation(&self.root);
        let (size, inline) = {
            let db = self.reader()?;
            placement(&db, digest)?.ok_or(Error::new("object-not-found", 404))?
        };
        if inline {
            // The database already answers from a consistent snapshot, so an
            // inline read does not wait behind a file publication.
            let body = self.inline_body(digest, size)?;
            return Ok((Body::bytes(body), size));
        }
        let _gate = self
            .objects_gate
            .lock()
            .map_err(|_| Error::new("storage-unavailable", 503))?;
        #[cfg(not(all(test, feature = "source-observer")))]
        let file = File::open(self.object_path(digest)?)?;
        #[cfg(all(test, feature = "source-observer"))]
        let file = crate::source_observer::TrackedFile::open(
            &self.root,
            digest,
            &self.object_path(digest)?,
            "file",
        )?;
        if file.metadata()?.len() != size {
            #[cfg(all(test, feature = "source-observer"))]
            crate::source_observer::note_current_violation("corrupt-object");
            return Err(Error::new("corrupt-object", 503));
        }
        Ok((Body::File(file), size))
    }
    /// The body of an object the metadata files as inline. It is read and
    /// checked against its identity here, so an absent or altered row is a
    /// corrupt object rather than a lookup that falls through to the files.
    fn inline_body(&self, digest: &str, size: u64) -> Result<Vec<u8>> {
        let bytes = small_object_store::read(&*self.reader()?, digest, SMALL_OBJECT_BYTES)
            .map_err(body_error)?
            .ok_or(Error::new("corrupt-object", 503))?;
        if bytes.len() as u64 != size {
            return Err(Error::new("corrupt-object", 503));
        }
        Ok(bytes)
    }
    /// Whether the body behind an accounted object is actually present at the
    /// size its metadata states, for the two paths that admit a reference to
    /// it without reading it.
    pub(super) fn check_object_body(
        &self,
        db: &Connection,
        paths: &ObjectPaths,
        digest: &str,
        size: u64,
    ) -> Result<()> {
        let (recorded, inline) =
            placement(db, digest)?.ok_or(Error::new("object-not-found", 404))?;
        if recorded != size {
            return Err(Error::new("corrupt-object", 503));
        }
        let present = if inline {
            small_object_store::size(db, digest).map_err(body_error)? == Some(size)
        } else {
            let metadata = paths
                .metadata(digest)?
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
            metadata.is_file() && metadata.len() == size
        };
        if present {
            Ok(())
        } else {
            Err(Error::new("corrupt-object", 503))
        }
    }
    /// Paths for one operation over many objects; see [`ObjectPaths`].
    pub(crate) fn object_paths(&self) -> ObjectPaths<'_> {
        ObjectPaths {
            root: &self.root,
            objects: std::sync::OnceLock::new(),
            shards: std::sync::Mutex::default(),
        }
    }
    pub(super) fn object_path(&self, digest: &str) -> Result<std::path::PathBuf> {
        self.object_paths().path(digest)
    }
    pub fn put_object(&self, device: &super::Device, digest: &str, bytes: &[u8]) -> Result<()> {
        validate_hash(digest)?;
        if bytes.len() > MAX_TARGET_BYTES {
            return Err(Error::new("object-too-large", 413));
        }
        self.put_objects(device, &[(digest.to_owned(), bytes.to_vec())])
    }
    /// Whether a small body the metadata files as a file has to be published
    /// again because its file is absent or holds other bytes. A healthy file
    /// keeps the object where the row puts it; a damaged one is replaced there,
    /// so an upload never acknowledges a body nothing can read.
    fn filed_body_differs(&self, digest: &str, bytes: &[u8]) -> Result<bool> {
        #[cfg(not(all(test, feature = "source-observer")))]
        let opened = File::open(self.object_path(digest)?);
        #[cfg(all(test, feature = "source-observer"))]
        let opened = crate::source_observer::TrackedFile::open(
            &self.root,
            digest,
            &self.object_path(digest)?,
            "file",
        );
        let file = match opened {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error.into()),
        };
        use std::io::Read;
        let mut stored = Vec::with_capacity(bytes.len());
        file.take(bytes.len() as u64 + 1).read_to_end(&mut stored)?;
        Ok(stored != bytes)
    }
    /// Write a group of small bodies and the metadata naming them in one
    /// transaction. A body already published as a file stays a file; the row
    /// decides where an object lives, so a group never moves one. The caller
    /// has already republished any such file that did not hold its body.
    fn put_inline(
        tx: &rusqlite::Transaction<'_>,
        device: &super::Device,
        objects: &[(&str, &[u8])],
    ) -> Result<()> {
        let mut fresh = Vec::with_capacity(objects.len());
        for (digest, bytes) in objects {
            match placement(tx, digest)? {
                Some((size, _)) if size != bytes.len() as u64 => {
                    return Err(Error::new("corrupt-object", 503))
                }
                Some((_, false)) => (),
                _ => fresh.push((*digest, *bytes)),
            }
        }
        small_object_store::insert_batch(tx, &fresh).map_err(body_error)?;
        for (digest, bytes) in &fresh {
            tx.execute(
                "INSERT INTO objects(hash,size,storage) VALUES(?1,?2,'inline') ON CONFLICT(hash) DO NOTHING",
                params![digest, bytes.len() as i64],
            )?;
        }
        for (digest, _) in objects {
            Self::lease_object(tx, device, digest)?;
        }
        Ok(())
    }
    pub(super) fn put_objects(
        &self,
        device: &super::Device,
        objects: &[(String, Vec<u8>)],
    ) -> Result<()> {
        #[cfg(all(test, feature = "source-observer"))]
        let _observed = crate::source_observer::ingress(&self.root);
        Self::require_device(&*self.db()?, device)?;
        for (digest, bytes) in objects {
            #[cfg(test)]
            let measured = std::time::Instant::now();
            validate_hash(digest)?;
            if bytes.len() > MAX_TARGET_BYTES || {
                let actual = hash(bytes);
                #[cfg(test)]
                frame_metrics::hashed("object", bytes.len());
                #[cfg(all(test, feature = "source-observer"))]
                crate::source_observer::hashed(digest, "object-ingress-sha256", bytes.len());
                actual != *digest
            } {
                return Err(Error::new("hash-mismatch", 400));
            }
            #[cfg(test)]
            frame_metrics::record(1, measured);
        }
        self.put_hashed_objects(device, objects)
    }

    // Only immediate callers that computed identities from these unchanged bodies
    // may enter here. Authorization and durable publication remain shared.
    pub(super) fn put_hashed_objects(
        &self,
        device: &super::Device,
        objects: &[(String, Vec<u8>)],
    ) -> Result<()> {
        Self::require_device(&*self.db()?, device)?;
        let staging = self.root.join("staging");
        check_path(&staging)?;
        let mut inline: Vec<(&str, &[u8])> = Vec::new();
        let mut filed: Vec<(&str, &[u8])> = Vec::new();
        let mut file_hashes = std::collections::BTreeSet::new();
        // Small bodies an earlier upload published as files. Normally none.
        let small_files = {
            let reader = self.reader()?;
            let mut found = std::collections::BTreeSet::new();
            for (digest, bytes) in objects {
                if bytes.len() <= SMALL_OBJECT_BYTES
                    && matches!(placement(&reader, digest)?, Some((_, false)))
                {
                    found.insert(digest.as_str());
                }
            }
            found
        };
        for (digest, bytes) in objects {
            validate_hash(digest)?;
            if bytes.len() > MAX_TARGET_BYTES {
                return Err(Error::new("hash-mismatch", 400));
            }
            if bytes.len() <= SMALL_OBJECT_BYTES
                && !(small_files.contains(digest.as_str())
                    && self.filed_body_differs(digest, bytes)?)
            {
                inline.push((digest.as_str(), bytes.as_slice()));
                continue;
            }
            if file_hashes.insert(digest.as_str()) {
                filed.push((digest.as_str(), bytes.as_slice()));
            }
        }
        let paths = self.object_paths();
        let prepared = file_batch(&filed, |(digest, bytes)| {
            #[cfg(test)]
            let measured = std::time::Instant::now();
            let destination = paths.path(digest)?;
            fs::create_dir_all(destination.parent().unwrap())?;
            let mut temp = self.staging_temp()?;
            temp.write_all(bytes)?;
            #[cfg(test)]
            frame_metrics::record(2, measured);
            #[cfg(test)]
            let measured = std::time::Instant::now();
            temp.as_file().sync_all()?;
            #[cfg(test)]
            frame_metrics::record(3, measured);
            Ok((temp, destination))
        })?;
        // The gate serializes a file publication against the metadata that
        // names it. An inline body is published by the same transaction as its
        // metadata, so a group without files never takes it.
        let gate = if prepared.is_empty() {
            None
        } else {
            #[cfg(test)]
            let measured = std::time::Instant::now();
            let gate = self
                .objects_gate
                .lock()
                .map_err(|_| Error::new("storage-unavailable", 503))?;
            #[cfg(test)]
            frame_metrics::record(4, measured);
            #[cfg(test)]
            let measured = std::time::Instant::now();
            sync_directory(&self.root.join("objects"))?;
            #[cfg(test)]
            frame_metrics::record(6, measured);
            file_batch(&prepared, |(temp, destination)| {
                #[cfg(test)]
                let measured = std::time::Instant::now();
                #[cfg(windows)]
                publish(temp.path(), destination)?;
                #[cfg(not(windows))]
                fs::rename(temp.path(), destination)?;
                #[cfg(test)]
                frame_metrics::record(5, measured);
                Ok(())
            })?;
            let directories = prepared
                .iter()
                .map(|(_, destination)| destination.parent().unwrap())
                .collect::<std::collections::BTreeSet<_>>();
            for directory in directories {
                #[cfg(test)]
                let measured = std::time::Instant::now();
                sync_directory(directory)?;
                #[cfg(test)]
                frame_metrics::record(6, measured);
            }
            Some(gate)
        };
        #[cfg(test)]
        let measured = std::time::Instant::now();
        let mut db = self.db()?;
        #[cfg(test)]
        frame_metrics::record(4, measured);
        #[cfg(test)]
        let measured = std::time::Instant::now();
        Self::require_device(&db, device)?;
        let tx = db.transaction()?;
        for (digest, bytes) in &filed {
            tx.execute(
                "INSERT INTO objects(hash,size,storage) VALUES(?1,?2,'file') ON CONFLICT(hash) DO NOTHING",
                params![digest, bytes.len() as i64],
            )?;
            Self::lease_object(&tx, device, digest)?;
        }
        Self::put_inline(&tx, device, &inline)?;
        tx.commit()?;
        drop(gate);
        #[cfg(test)]
        frame_metrics::record(7, measured);
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
    pub fn object_presence(&self, digest: &str) -> Result<bool> {
        self.object_presence_in(&self.object_paths(), digest)
    }
    /// [`Self::object_presence`] for one of many objects an operation checks.
    pub(crate) fn object_presence_in(&self, paths: &ObjectPaths, digest: &str) -> Result<bool> {
        let db = self.reader()?;
        let Some((size, inline)) = placement(&db, digest)? else {
            return Ok(false);
        };
        if inline {
            return Ok(small_object_store::size(&db, digest).map_err(body_error)? == Some(size));
        }
        Ok(paths
            .metadata(digest)?
            .is_some_and(|meta| meta.is_file() && meta.len() == size))
    }
    pub fn get_object(&self, digest: &str) -> Result<Vec<u8>> {
        #[cfg(all(test, feature = "source-observer"))]
        let _observed = crate::source_observer::operation(&self.root);
        let (size, inline) = {
            let db = self.reader()?;
            placement(&db, digest)?.ok_or(Error::new("object-not-found", 404))?
        };
        if size > MAX_TARGET_BYTES as u64 {
            return Err(Error::new("corrupt-object", 503));
        }
        if inline {
            let bytes = self.inline_body(digest, size)?;
            #[cfg(test)]
            frame_metrics::opened();
            return Ok(bytes);
        }
        let path = self.object_path(digest)?;
        #[cfg(not(all(test, feature = "source-observer")))]
        let file = File::open(path)?;
        #[cfg(all(test, feature = "source-observer"))]
        let file = crate::source_observer::TrackedFile::open(&self.root, digest, &path, "file")?;
        #[cfg(test)]
        frame_metrics::opened();
        use std::io::Read;
        let mut bytes = Vec::new();
        file.take(size + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 != size || {
            let actual = hash(&bytes);
            #[cfg(all(test, feature = "source-observer"))]
            crate::source_observer::hashed(digest, "object-source-sha256", bytes.len());
            actual != digest
        } {
            #[cfg(all(test, feature = "source-observer"))]
            crate::source_observer::note_violation("corrupt-object");
            return Err(Error::new("corrupt-object", 503));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
pub(super) mod frame_metrics {
    type Samples = std::sync::Arc<std::sync::Mutex<[Vec<u128>; 8]>>;

    std::thread_local! {
        pub static SAMPLES: std::cell::RefCell<Option<Samples>> = const { std::cell::RefCell::new(None) };
        pub static FILE_WORKERS: std::cell::Cell<usize> = const { std::cell::Cell::new(super::FILE_IO_WORKERS) };
        pub static SPAWN_FAILURE_MASK: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
        /// Counts the bodies a reply actually opened. A transfer reply is
        /// planned from object metadata, so this must stay at the number of
        /// bodies the reply carries.
        pub static BODY_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// The identities ingress computed, by step and body length. A
        /// materialized frame is hashed once and published under that hash.
        pub static HASHES: std::cell::RefCell<Vec<(&'static str, usize)>> = const { std::cell::RefCell::new(Vec::new()) };
        /// Counts the paths checked for links.
        pub static PATH_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    pub fn record(stage: usize, started: std::time::Instant) {
        SAMPLES.with(|samples| {
            if let Some(samples) = &mut *samples.borrow_mut() {
                samples.lock().unwrap()[stage].push(started.elapsed().as_micros());
            }
        });
    }
    pub fn opened() {
        BODY_OPENS.with(|count| count.set(count.get() + 1));
    }
    pub fn take_opens() -> usize {
        BODY_OPENS.with(|count| count.replace(0))
    }
    pub fn hashed(step: &'static str, bytes: usize) {
        HASHES.with(|hashes| hashes.borrow_mut().push((step, bytes)));
    }
    pub fn take_hashes() -> Vec<(&'static str, usize)> {
        HASHES.with(|hashes| hashes.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_batch_rejects_mismatch_and_oversize_before_publication() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::init(directory.path()).unwrap();
        let registration = store.add_device().unwrap();
        let device = super::super::Device {
            id: registration.device_id,
            format_level: None,
        };
        let good = b"synthetic good".to_vec();
        let digest = hash(&good);
        for (bad_hash, bad_body) in [
            (hash(b"other"), b"wrong bytes".to_vec()),
            ("invalid".into(), vec![]),
            (hash(b"oversized"), vec![0; MAX_TARGET_BYTES + 1]),
        ] {
            assert!(store
                .put_objects(
                    &device,
                    &[(digest.clone(), good.clone()), (bad_hash, bad_body)]
                )
                .is_err());
            assert!(store.object_size(&digest).unwrap().is_none());
        }
    }

    #[test]
    fn frame_handoff_preserves_validation_retry_and_durable_mixed_storage() {
        use risunest_sync_wire::{
            delta,
            transfer::{self, Frame},
        };
        let directory = tempfile::tempdir().unwrap();
        let store = Store::init(directory.path()).unwrap();
        let registration = store.add_device().unwrap();
        let device = super::super::Device {
            id: registration.device_id,
            format_level: None,
        };
        let small = b"synthetic base".to_vec();
        let large = vec![b'z'; SMALL_OBJECT_BYTES + 1];
        let target = b"synthetic base changed".to_vec();
        let recipe = delta::create(&[&small], &target).unwrap();
        let mut bad_recipe = recipe.clone();
        bad_recipe.target_hash = hash(b"wrong target");
        let bad =
            transfer::encode(&[Frame::Full(small.clone()), Frame::Delta(bad_recipe)]).unwrap();
        assert!(store.receive_frames(&device, &bad).is_err());
        assert!(store.object_size(&hash(&small)).unwrap().is_none());
        let frames = [
            Frame::Full(small.clone()),
            Frame::Delta(recipe),
            Frame::Full(large.clone()),
        ];
        let encoded = transfer::encode(&frames).unwrap();
        let mut corrupt = encoded.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert!(store.receive_frames(&device, &corrupt).is_err());
        assert!(store.object_size(&hash(&small)).unwrap().is_none());
        let unknown = super::super::Device {
            id: "missing-device".into(),
            format_level: None,
        };
        assert!(store.receive_frames(&unknown, &encoded).is_err());
        assert!(store.object_size(&hash(&small)).unwrap().is_none());
        store.db().unwrap().execute_batch("CREATE TRIGGER synthetic_frame_failure BEFORE INSERT ON small_objects BEGIN SELECT RAISE(ABORT,'synthetic body failure'); END").unwrap();
        assert!(store.receive_frames(&device, &encoded).is_err());
        for body in [&small, &target, &large] {
            assert!(store.object_size(&hash(body)).unwrap().is_none());
        }
        assert_eq!(
            store
                .db()
                .unwrap()
                .query_row("SELECT count(*) FROM object_leases", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        store
            .db()
            .unwrap()
            .execute_batch("DROP TRIGGER synthetic_frame_failure")
            .unwrap();
        let hashes = [&small, &target, &large].map(|body| hash(body));
        for _ in 0..2 {
            assert_eq!(store.receive_frames(&device, &encoded).unwrap(), hashes);
        }
        drop(store);
        let store = Store::open(directory.path()).unwrap();
        for (body, digest) in [&small, &target, &large].into_iter().zip(hashes) {
            assert_eq!(&store.get_object(&digest).unwrap(), body);
        }
    }

    #[test]
    fn frame_body_write_failure_leaves_no_metadata_or_lease() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::init(directory.path()).unwrap();
        let registration = store.add_device().unwrap();
        let device = super::super::Device {
            id: registration.device_id,
            format_level: None,
        };
        let staging = directory.path().join("staging");
        std::fs::remove_dir(&staging).unwrap();
        std::fs::write(&staging, b"synthetic obstruction").unwrap();
        // Above the inline threshold, so this body is one the staging
        // directory has to carry.
        let bytes = vec![b's'; SMALL_OBJECT_BYTES + 1];
        let digest = hash(&bytes);
        let encoded =
            risunest_sync_wire::transfer::encode(&[risunest_sync_wire::transfer::Frame::Full(
                bytes.clone(),
            )])
            .unwrap();
        assert!(store.receive_frames(&device, &encoded).is_err());
        assert!(store
            .put_objects(&device, &[(digest.clone(), bytes)])
            .is_err());
        assert!(store.object_size(&digest).unwrap().is_none());
        assert_eq!(
            store
                .db()
                .unwrap()
                .query_row("SELECT count(*) FROM object_leases", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn inline_body_write_failure_leaves_no_metadata_lease_or_body() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::init(directory.path()).unwrap();
        let registration = store.add_device().unwrap();
        let device = super::super::Device {
            id: registration.device_id,
            format_level: None,
        };
        let bytes = b"synthetic inline body".to_vec();
        let digest = hash(&bytes);
        store.db().unwrap().execute_batch("CREATE TRIGGER synthetic_inline_failure BEFORE INSERT ON small_objects BEGIN SELECT RAISE(ABORT,'synthetic body failure'); END").unwrap();
        assert!(store
            .put_objects(&device, &[(digest.clone(), bytes.clone())])
            .is_err());
        assert!(store.object_size(&digest).unwrap().is_none());
        {
            let db = store.db().unwrap();
            for table in ["object_leases", "small_objects"] {
                assert_eq!(
                    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                        .unwrap(),
                    0
                );
            }
        }
        store
            .db()
            .unwrap()
            .execute_batch("DROP TRIGGER synthetic_inline_failure")
            .unwrap();
        store
            .put_objects(&device, &[(digest.clone(), bytes.clone())])
            .unwrap();
        assert_eq!(store.get_object(&digest).unwrap(), bytes);
        assert!(std::fs::metadata(store.object_path(&digest).unwrap()).is_err());
    }

    #[test]
    fn a_body_is_filed_by_its_size_and_read_back_from_wherever_it_went() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::init(directory.path()).unwrap();
        let registration = store.add_device().unwrap();
        let device = super::super::Device {
            id: registration.device_id,
            format_level: None,
        };
        assert_eq!(
            store
                .db()
                .unwrap()
                .query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        let small = vec![b'i'; SMALL_OBJECT_BYTES];
        let large = vec![b'f'; SMALL_OBJECT_BYTES + 1];
        let bodies = [small, large];
        let objects = bodies
            .iter()
            .map(|bytes| (hash(bytes), bytes.clone()))
            .collect::<Vec<_>>();
        store.put_objects(&device, &objects).unwrap();
        for (expected, (digest, bytes)) in ["inline", "file"].into_iter().zip(&objects) {
            let storage: String = store
                .db()
                .unwrap()
                .query_row("SELECT storage FROM objects WHERE hash=?1", [digest], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(storage, expected);
            assert_eq!(
                std::fs::metadata(store.object_path(digest).unwrap()).is_ok(),
                expected == "file"
            );
            assert_eq!(store.object_size(digest).unwrap(), Some(bytes.len() as u64));
            assert_eq!(&store.get_object(digest).unwrap(), bytes);
            let (mut body, size) = store.open_object(digest).unwrap();
            assert_eq!(size, bytes.len() as u64);
            let mut read = Vec::new();
            std::io::Read::read_to_end(&mut body, &mut read).unwrap();
            assert_eq!(&read, bytes);
        }
        // An inline row the body no longer backs is corruption, never a reason
        // to go looking in the file store.
        store
            .db()
            .unwrap()
            .execute("DELETE FROM small_objects WHERE hash=?1", [&objects[0].0])
            .unwrap();
        assert_eq!(
            store.get_object(&objects[0].0).unwrap_err().status,
            503,
            "an absent inline body must report corruption"
        );
    }

    #[test]
    fn parallel_file_frames_preserve_mixed_placement_duplicates_and_reopening() {
        use risunest_sync_wire::transfer::{self, Frame};
        let directory = tempfile::tempdir().unwrap();
        let store = Store::init(directory.path()).unwrap();
        let registration = store.add_device().unwrap();
        let device = super::super::Device {
            id: registration.device_id,
            format_level: None,
        };
        let sizes = [64 * 1024, 64 * 1024 + 1, 256 * 1024, 256 * 1024 + 1];
        let bodies = sizes.map(|size| vec![0x5a; size]);
        let frames = bodies
            .iter()
            .cycle()
            .take(16)
            .cloned()
            .map(Frame::Full)
            .collect::<Vec<_>>();
        let encoded = transfer::encode(&frames).unwrap();
        for _ in 0..2 {
            store.receive_frames(&device, &encoded).unwrap();
        }
        for (body, expected) in bodies.iter().zip([true, false, false, false]) {
            let digest = hash(body);
            assert_eq!(
                placement(&store.reader().unwrap(), &digest).unwrap(),
                Some((body.len() as u64, expected))
            );
            assert_eq!(store.object_path(&digest).unwrap().exists(), !expected);
        }
        drop(store);
        let store = Store::open(directory.path()).unwrap();
        for body in &bodies {
            assert_eq!(&store.get_object(&hash(body)).unwrap(), body);
        }
        let copy = tempfile::tempdir().unwrap();
        let backup = copy.path().join("backup");
        store.backup(&backup).unwrap();
        let restored = Store::restore_backup(&backup, &copy.path().join("restored")).unwrap();
        for body in bodies {
            assert_eq!(restored.get_object(&hash(&body)).unwrap(), body);
        }
    }

    #[test]
    fn file_batch_joins_successful_workers_before_returning_an_error() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Barrier,
        };
        for depth in [2, 4, 8] {
            frame_metrics::FILE_WORKERS.with(|workers| workers.set(depth));
            let items = (0..depth).collect::<Vec<_>>();
            let entered = Barrier::new(depth);
            let completed = AtomicUsize::new(0);
            let result = file_batch(&items, |index| {
                entered.wait();
                if *index == 0 {
                    return Err(Error::new("storage-io", 503));
                }
                completed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            assert_eq!(result.unwrap_err().code, "storage-io");
            assert_eq!(completed.load(Ordering::SeqCst), depth - 1);
        }
        frame_metrics::FILE_WORKERS.with(|workers| workers.set(FILE_IO_WORKERS));
    }

    #[test]
    fn thread_creation_failure_falls_back_without_poisoning_publication() {
        use risunest_sync_wire::transfer::{self, Frame};
        frame_metrics::FILE_WORKERS.with(|workers| workers.set(4));
        // Four preparation attempts, followed by four publication attempts.
        for failures in [0x0f, 0xf0, 0xaa, 0xff] {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::init(directory.path()).unwrap();
            let registration = store.add_device().unwrap();
            let device = super::super::Device {
                id: registration.device_id,
                format_level: None,
            };
            let bodies = (0..8)
                .map(|index| vec![index; SMALL_OBJECT_BYTES + 1])
                .collect::<Vec<_>>();
            let frames = bodies.iter().cloned().map(Frame::Full).collect::<Vec<_>>();
            let encoded = transfer::encode(&frames).unwrap();
            frame_metrics::SPAWN_FAILURE_MASK.with(|mask| mask.set(failures));
            let received = store.receive_frames(&device, &encoded).unwrap();
            assert_eq!(
                received,
                bodies.iter().map(|body| hash(body)).collect::<Vec<_>>()
            );
            assert_eq!(
                frame_metrics::SPAWN_FAILURE_MASK.with(std::cell::Cell::get),
                0
            );
            assert!(!store.objects_gate.is_poisoned());
            assert!(store.temporary_paths.lock().unwrap().is_empty());
            assert_eq!(
                std::fs::read_dir(directory.path().join("staging"))
                    .unwrap()
                    .count(),
                0
            );
            store.maintain().unwrap();
            assert_eq!(store.receive_frames(&device, &encoded).unwrap(), received);
            for body in &bodies {
                assert_eq!(&store.get_object(&hash(body)).unwrap(), body);
            }
        }
        frame_metrics::FILE_WORKERS.with(|workers| workers.set(FILE_IO_WORKERS));
    }

    #[test]
    fn mixed_inline_and_spawned_file_work_finishes_before_an_error_returns() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        frame_metrics::FILE_WORKERS.with(|workers| workers.set(4));
        frame_metrics::SPAWN_FAILURE_MASK.with(|mask| mask.set(0b1010));
        let completed = AtomicUsize::new(0);
        let result = file_batch(&(0..8).collect::<Vec<_>>(), |index| {
            if *index == 3 {
                return Err(Error::new("storage-io", 503));
            }
            completed.fetch_add(1, Ordering::SeqCst);
            Ok(*index)
        });
        assert_eq!(result.unwrap_err().code, "storage-io");
        assert_eq!(completed.load(Ordering::SeqCst), 7);
        assert_eq!(
            frame_metrics::SPAWN_FAILURE_MASK.with(std::cell::Cell::get),
            0
        );
        frame_metrics::FILE_WORKERS.with(|workers| workers.set(FILE_IO_WORKERS));
    }

    #[test]
    fn a_parallel_publication_failure_keeps_metadata_unpublished_and_retries_cleanly() {
        use risunest_sync_wire::transfer::{self, Frame};
        let directory = tempfile::tempdir().unwrap();
        let store = Store::init(directory.path()).unwrap();
        let registration = store.add_device().unwrap();
        let device = super::super::Device {
            id: registration.device_id,
            format_level: None,
        };
        let bodies = (0..8)
            .map(|index| vec![index; SMALL_OBJECT_BYTES + 1])
            .collect::<Vec<_>>();
        let hashes = bodies.iter().map(|body| hash(body)).collect::<Vec<_>>();
        let frames = bodies.iter().cloned().map(Frame::Full).collect::<Vec<_>>();
        let encoded = transfer::encode(&frames).unwrap();
        let blocked = store.object_path(&hashes[0]).unwrap();
        std::fs::create_dir_all(&blocked).unwrap();
        assert!(store.receive_frames(&device, &encoded).is_err());
        for digest in &hashes {
            assert!(store.object_size(digest).unwrap().is_none());
        }
        assert_eq!(
            store
                .db()
                .unwrap()
                .query_row("SELECT count(*) FROM object_leases", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(store.temporary_paths.lock().unwrap().is_empty());
        assert_eq!(
            std::fs::read_dir(directory.path().join("staging"))
                .unwrap()
                .count(),
            0
        );
        std::fs::remove_dir(&blocked).unwrap();
        assert_eq!(store.receive_frames(&device, &encoded).unwrap(), hashes);
        for (body, digest) in bodies.iter().zip(&hashes) {
            assert_eq!(&store.get_object(digest).unwrap(), body);
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Upload {
        Frame,
        Chunked,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Held {
        Intact,
        Missing,
        Corrupt,
    }

    fn upload(
        store: &Store,
        device: &super::super::Device,
        through: Upload,
        bytes: &[u8],
    ) -> Result<()> {
        let digest = hash(bytes);
        match through {
            Upload::Frame => store.put_object(device, &digest, bytes),
            Upload::Chunked => {
                let upload = store.begin_upload(
                    device,
                    &super::super::UploadManifest {
                        hash: digest.clone(),
                        size: (bytes.len() as u64).into(),
                    },
                )?;
                store.put_upload_chunk(device, &upload, 0, &digest, bytes)?;
                store.finish_upload(device, &upload).map(|_| ())
            }
        }
    }

    /// Leaves the body the metadata names the way `held` says, without
    /// touching the row that decides where it lives.
    fn damage(store: &Store, bytes: &[u8], held: Held) {
        let digest = hash(bytes);
        let mut altered = bytes.to_vec();
        altered[0] ^= 0x01;
        let (_, inline) = placement(&store.db().unwrap(), &digest).unwrap().unwrap();
        match (held, inline) {
            (Held::Intact, _) => (),
            (Held::Missing, true) => {
                store
                    .db()
                    .unwrap()
                    .execute("DELETE FROM small_objects WHERE hash=?1", [&digest])
                    .unwrap();
            }
            (Held::Corrupt, true) => {
                store
                    .db()
                    .unwrap()
                    .execute(
                        "UPDATE small_objects SET body=?2 WHERE hash=?1",
                        params![digest, altered],
                    )
                    .unwrap();
            }
            (Held::Missing, false) => fs::remove_file(store.object_path(&digest).unwrap()).unwrap(),
            (Held::Corrupt, false) => {
                fs::write(store.object_path(&digest).unwrap(), &altered).unwrap()
            }
        }
    }

    fn assert_readable(store: &Store, bytes: &[u8], case: &str) {
        let digest = hash(bytes);
        assert_eq!(store.get_object(&digest).expect(case), bytes, "{case}");
        assert_eq!(
            store
                .read_object_range(&digest, 1, bytes.len() as u64 - 2)
                .expect(case),
            &bytes[1..bytes.len() - 1],
            "{case}"
        );
        let (mut body, size) = store.open_object(&digest).expect(case);
        let mut read = Vec::new();
        std::io::Read::read_to_end(&mut body, &mut read).unwrap();
        assert_eq!(
            (size, read.as_slice()),
            (bytes.len() as u64, bytes),
            "{case}"
        );
    }

    /// Every success an upload reports is a body the store can serve, whichever
    /// of the two stores held the identity first and whatever became of it.
    /// A file is replaced where it is; an inline body is restored when absent
    /// and refused, never overwritten, when it holds other bytes.
    #[test]
    fn a_reupload_acknowledges_only_a_body_it_can_serve() {
        let bytes = b"synthetic cross-placement upload";
        let digest = hash(bytes);
        for first in [Upload::Frame, Upload::Chunked] {
            let inline = first == Upload::Frame;
            for held in [Held::Intact, Held::Missing, Held::Corrupt] {
                for again in [Upload::Frame, Upload::Chunked] {
                    let case = format!("{first:?} then {held:?} then {again:?}");
                    let directory = tempfile::tempdir().unwrap();
                    let store = Store::init(directory.path()).unwrap();
                    let device = super::super::Device {
                        id: store.add_device().unwrap().device_id,
                        format_level: None,
                    };
                    upload(&store, &device, first, bytes).unwrap();
                    assert_readable(&store, bytes, &case);
                    damage(&store, bytes, held);
                    if let Err(error) = upload(&store, &device, again, bytes) {
                        assert!(inline && held == Held::Corrupt, "{case}: {error:?}");
                        assert_eq!(error.status, 503, "{case}");
                        continue;
                    }
                    assert!(
                        !(inline && held == Held::Corrupt),
                        "{case} accepted bytes the stored body contradicts"
                    );
                    assert_readable(&store, bytes, &case);
                    let (_, placed) = placement(&store.db().unwrap(), &digest).unwrap().unwrap();
                    assert_eq!(placed, inline, "{case} moved the object");
                    assert_eq!(
                        fs::metadata(store.object_path(&digest).unwrap()).is_ok(),
                        !inline,
                        "{case} left a file the metadata does not name"
                    );
                    drop(store);
                    let store = Store::open(directory.path()).unwrap();
                    assert_readable(&store, bytes, &format!("{case} after reopen"));
                }
            }
        }
    }

    #[test]
    fn a_failed_repair_acknowledges_nothing_and_a_retry_completes_it() {
        let bytes = b"synthetic interrupted repair";
        let digest = hash(bytes);
        // An inline body restored by a chunked upload whose completion fails.
        let directory = tempfile::tempdir().unwrap();
        let store = Store::init(directory.path()).unwrap();
        let device = super::super::Device {
            id: store.add_device().unwrap().device_id,
            format_level: None,
        };
        upload(&store, &device, Upload::Frame, bytes).unwrap();
        damage(&store, bytes, Held::Missing);
        let id = store
            .begin_upload(
                &device,
                &super::super::UploadManifest {
                    hash: digest.clone(),
                    size: (bytes.len() as u64).into(),
                },
            )
            .unwrap();
        store
            .put_upload_chunk(&device, &id, 0, &digest, bytes)
            .unwrap();
        store.db().unwrap().execute_batch("CREATE TRIGGER synthetic_completion_failure BEFORE UPDATE OF state ON uploads WHEN NEW.state='complete' BEGIN SELECT RAISE(ABORT,'synthetic completion failure'); END").unwrap();
        assert!(store.finish_upload(&device, &id).is_err());
        assert!(
            store.get_object(&digest).is_err(),
            "a rolled back repair left its body behind"
        );
        store
            .db()
            .unwrap()
            .execute_batch("DROP TRIGGER synthetic_completion_failure")
            .unwrap();
        store.finish_upload(&device, &id).unwrap();
        assert_readable(&store, bytes, "inline repair after a failed completion");
        // A file restored by a frame whose lease cannot be written.
        let directory = tempfile::tempdir().unwrap();
        let store = Store::init(directory.path()).unwrap();
        let device = super::super::Device {
            id: store.add_device().unwrap().device_id,
            format_level: None,
        };
        upload(&store, &device, Upload::Chunked, bytes).unwrap();
        damage(&store, bytes, Held::Corrupt);
        store.db().unwrap().execute_batch("CREATE TRIGGER synthetic_lease_insert BEFORE INSERT ON object_leases BEGIN SELECT RAISE(ABORT,'synthetic lease failure'); END; CREATE TRIGGER synthetic_lease_update BEFORE UPDATE ON object_leases BEGIN SELECT RAISE(ABORT,'synthetic lease failure'); END").unwrap();
        assert!(store.put_object(&device, &digest, bytes).is_err());
        store
            .db()
            .unwrap()
            .execute_batch(
                "DROP TRIGGER synthetic_lease_insert; DROP TRIGGER synthetic_lease_update",
            )
            .unwrap();
        store.put_object(&device, &digest, bytes).unwrap();
        assert_readable(&store, bytes, "file repair after a failed lease");
    }

    #[test]
    fn frame_batch_metadata_failure_is_atomic_and_replay_survives_reopen() {
        use risunest_sync_wire::{
            delta,
            transfer::{self, Frame},
        };
        let directory = tempfile::tempdir().unwrap();
        let store = Store::init(directory.path()).unwrap();
        let credential = store.add_device().unwrap();
        let device = super::super::Device {
            id: credential.device_id,
            format_level: None,
        };
        let base = vec![b'a'; 2048];
        let mut target = base.clone();
        target[1024] = b'b';
        let recipe = delta::create(&[base.as_slice()], &target).unwrap();
        let frames = transfer::encode(&[Frame::Full(base.clone()), Frame::Delta(recipe)]).unwrap();
        store.db().unwrap().execute_batch(&format!("CREATE TRIGGER synthetic_batch_failure BEFORE INSERT ON objects WHEN NEW.hash='{}' BEGIN SELECT RAISE(ABORT,'synthetic metadata failure'); END", hash(&target))).unwrap();
        assert!(store.receive_frames(&device, &frames).is_err());
        assert!(store.object_size(&hash(&base)).unwrap().is_none());
        assert!(store.object_size(&hash(&target)).unwrap().is_none());
        assert_eq!(
            store
                .db()
                .unwrap()
                .query_row("SELECT count(*) FROM object_leases", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        store
            .db()
            .unwrap()
            .execute_batch("DROP TRIGGER synthetic_batch_failure")
            .unwrap();
        drop(store);
        let store = Store::open(directory.path()).unwrap();
        for _ in 0..2 {
            assert_eq!(
                store.receive_frames(&device, &frames).unwrap(),
                vec![hash(&base), hash(&target)]
            );
        }
        assert_eq!(store.get_object(&hash(&base)).unwrap(), base);
        assert_eq!(store.get_object(&hash(&target)).unwrap(), target);
    }

    #[test]
    fn sqlite_capacity_failure_cannot_publish_partial_object_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::init(directory.path()).unwrap();
        let credential = store.add_device().unwrap();
        let device = super::super::Device {
            id: credential.device_id,
            format_level: None,
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
    #[test]
    #[ignore = "Explicit synthetic receive_frames persistence measurement"]
    fn frame_persistence_measurement() {
        for depth in [1, 2, 4, 4, 2, 1] {
            frame_metrics::FILE_WORKERS.with(|workers| workers.set(depth));
            for size in [64 * 1024, 64 * 1024 + 1, 256 * 1024, 1024 * 1024] {
                let root = tempfile::tempdir().unwrap();
                let store = Store::init(root.path()).unwrap();
                let registration = store.add_device().unwrap();
                let device = super::super::Device {
                    id: registration.device_id,
                    format_level: None,
                };
                let count = 64usize;
                frame_metrics::SAMPLES
                    .with(|samples| *samples.borrow_mut() = Some(Default::default()));
                let frames = (0..count)
                    .map(|index| {
                        let mut body = vec![0x5a; size];
                        body[..8].copy_from_slice(&(index as u64).to_le_bytes());
                        body[8..16].copy_from_slice(&(depth as u64).to_le_bytes());
                        risunest_sync_wire::transfer::Frame::Full(body)
                    })
                    .collect::<Vec<_>>();
                let mut elapsed = std::time::Duration::ZERO;
                for group in frames.chunks((4 * 1024 * 1024 - 8) / (size + 45)) {
                    let bytes = risunest_sync_wire::transfer::encode(group).unwrap();
                    let started = std::time::Instant::now();
                    store.receive_frames(&device, &bytes).unwrap();
                    elapsed += started.elapsed();
                }
                eprintln!(
                    "file_workers={depth} object_bytes={size} frame_count={count} wall_us={}",
                    elapsed.as_micros()
                );
                frame_metrics::SAMPLES.with(|samples| {
                    let samples = samples.borrow_mut().take().unwrap();
                    for (stage, mut values) in
                        samples.lock().unwrap().clone().into_iter().enumerate()
                    {
                        if values.is_empty() {
                            continue;
                        }
                        values.sort_unstable();
                        eprintln!(
                            "stage={stage} calls={} sum_us={} p50_us={} p95_us={}",
                            values.len(),
                            values.iter().sum::<u128>(),
                            values[values.len() / 2],
                            values[values.len() * 95 / 100]
                        );
                    }
                });
            }
        }
        frame_metrics::FILE_WORKERS.with(|workers| workers.set(FILE_IO_WORKERS));
    }
}
