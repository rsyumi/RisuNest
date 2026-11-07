//! Where captured record and generated bodies live. Small ones share a
//! database the external root already keeps, so a capture makes a group of
//! them durable in one transaction instead of publishing and syncing one file
//! per object. Larger ones keep the shared immutable file layout.
use crate::{
    persistent_store::StoreError,
    trust_boundary::{is_link_like, is_lower_hex_256, sync_directory},
};
use risunest_external_storage_format::content_identity::{hash, hash_reader};
use risunest_small_object_store as small_object_store;
pub(crate) use risunest_small_object_store::Body;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

/// Bodies above the database threshold are files here, each in the
/// subdirectory named by the first two hex digits of its identity. A file
/// system without a directory index (Android's ext4) scans a whole directory
/// for every create and every uncached lookup, so one directory holding every
/// body costs time in the square of their count.
const FILE_DIRECTORY: &str = "content-files";
/// Where earlier versions kept every file body directly. Bodies are read from
/// it for as long as it exists, and `move_flat_files` empties and removes it.
/// A new directory rather than subdirectories of this one, because a
/// directory never shrinks and a lookup that misses reads all of it.
const FLAT_FILE_DIRECTORY: &str = "objects";
/// Entries one step of the startup move takes, so app cleanup never waits for
/// more than one step.
const FLAT_MOVE_BATCH: usize = 4_096;

type Result<T> = std::result::Result<T, StoreError>;

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}

/// Bodies at or below this go to the database. The threshold decides where a
/// new body is written; where an existing one lives is decided by asking.
pub(crate) const SMALL_OBJECT_BYTES: usize = 64 * 1024;
const GROUP_ROWS: usize = 256;
const GROUP_BYTES: usize = 8 * 1024 * 1024;
const CANCELLABLE_READ_BYTES: usize = 64 * 1024;

/// Reads at most one bounded chunk at a time and asks `cancelled` before each,
/// so a long body can be stopped between chunks rather than only after it.
pub(crate) struct CancellableRead<'a, R> {
    inner: R,
    cancelled: &'a dyn Fn() -> bool,
    refused: bool,
}

impl<'a, R> CancellableRead<'a, R> {
    pub(crate) fn new(inner: R, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self {
            inner,
            cancelled,
            refused: false,
        }
    }

    /// Whether a read was refused because `cancelled` said so.
    pub(crate) fn refused(&self) -> bool {
        self.refused
    }
}

impl<R: io::Read> io::Read for CancellableRead<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if (self.cancelled)() {
            self.refused = true;
            return Err(io::Error::other("read cancelled"));
        }
        let limit = buffer.len().min(CANCELLABLE_READ_BYTES);
        self.inner.read(&mut buffer[..limit])
    }
}

/// Which store a caller expects to read an object from. Section spool parts are
/// files the caller produced itself and are read straight from disk; capture
/// bodies are resolved by identity, wherever this store put them.
#[derive(Clone, Debug)]
pub(crate) enum ObjectSource {
    File(PathBuf),
    Captured(String),
    /// A body the asset repository answers for under this content hash, held
    /// locally or through its custody. It is registered where it is rather
    /// than read out of a staging file, and only packaging resolves one.
    Library(String),
}

impl ObjectSource {
    /// The file this source names, when it names one at all.
    pub(crate) fn file(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::Captured(_) | Self::Library(_) => None,
        }
    }
}

pub(crate) struct ContentStore {
    files: PathBuf,
    /// The flat directory, when it still existed at open.
    flat: Option<PathBuf>,
    db: rusqlite::Connection,
    /// Bodies a caller has an identity for that are not durable yet. A reader
    /// on this store sees them exactly as it sees written ones.
    staged: BTreeMap<String, Vec<u8>>,
    staged_bytes: usize,
    /// What the checkpoint after the last group left in the log.
    checkpoint: small_object_store::Checkpoint,
    wal_high_water: u64,
}

impl ContentStore {
    /// `root` is the external storage root. The database sits beside the object
    /// directory rather than inside it, so a directory walk over objects never
    /// has to know about it.
    pub(crate) fn open(root: &Path) -> Result<Self> {
        let files = root.join(FILE_DIRECTORY);
        for directory in [root, &files] {
            fs::create_dir_all(directory)?;
            if is_link_like(&fs::symlink_metadata(directory)?) {
                return Err(invalid("Capture directory must not be a link"));
            }
        }
        let flat = flat_directory(root)?;
        let db = crate::sqlite_open::open(root.join("content.sqlite"))?;
        // Incremental, so collection can return freed pages to the filesystem
        // without rewriting the database. It must precede the first table.
        db.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000; PRAGMA journal_size_limit=67108864;",
        )?;
        small_object_store::initialize(&db)
            .map_err(|_| invalid("Capture content store is unavailable"))?;
        Ok(Self {
            files,
            flat,
            db,
            staged: BTreeMap::new(),
            staged_bytes: 0,
            checkpoint: small_object_store::Checkpoint::UNKNOWN,
            wal_high_water: small_object_store::WAL_HIGH_WATER_BYTES,
        })
    }

    /// Attach to a store that already exists, creating nothing. A consumer
    /// that only reads uses this so it never leaves an external directory
    /// behind in a scratch repository.
    pub(crate) fn open_existing(root: &Path) -> Result<Option<Self>> {
        if !root.join("content.sqlite").is_file() {
            return Ok(None);
        }
        Self::open(root).map(Some)
    }

    /// Stage or publish one body under the identity the caller states. An
    /// identity already present keeps the body it already has; different bytes
    /// under it are corruption, never a replacement.
    pub(crate) fn put(&mut self, expected: &str, bytes: &[u8]) -> Result<()> {
        #[cfg(test)]
        crate::persistent_store::hash_work::observe("external_content_put_verify",bytes.len());
        if hex::encode(hash(bytes)) != expected {
            return Err(invalid("Capture object identity differs"));
        }
        if bytes.len() > SMALL_OBJECT_BYTES {
            return self.write_file(expected, bytes);
        }
        if self.stat(expected)?.is_some() {
            // An identity already here keeps the body it already has, and that
            // body is checked rather than assumed, exactly as the file path
            // checks one it finds already published.
            if self.read_all(expected)? != bytes {
                return Err(invalid("Existing capture object differs"));
            }
            return Ok(());
        }
        self.staged_bytes += bytes.len();
        self.staged.insert(expected.to_owned(), bytes.to_vec());
        // A group that reaches the bound is written now. A durable body with no
        // reference to it is collectable; the reverse would be a dangling one.
        if self.staged.len() >= GROUP_ROWS || self.staged_bytes >= GROUP_BYTES {
            self.commit()?;
        }
        Ok(())
    }

    /// Make the staged group durable. Callers do this before recording the
    /// references that name it. A log readers are holding or a volume without
    /// room refuses the group and keeps it staged; groups already written stay
    /// written.
    pub(crate) fn commit(&mut self) -> Result<()> {
        if self.staged.is_empty() {
            return Ok(());
        }
        let available = fs2::available_space(&self.files)?;
        match small_object_store::admit(
            &self.db,
            &mut self.checkpoint,
            self.wal_high_water,
            self.staged_bytes as u64,
            available,
        )
        .map_err(|_| invalid("Capture content store is unavailable"))?
        {
            None => (),
            Some(small_object_store::Pressure::Log) => {
                return Err(invalid("Captured content could not be saved now"))
            }
            Some(small_object_store::Pressure::Space) => {
                return Err(invalid("Not enough free space to save captured content"))
            }
        }
        let group = &self.staged;
        let tx = self.db.transaction()?;
        small_object_store::insert_batch(
            &tx,
            &group
                .iter()
                .map(|(digest, bytes)| (digest.as_str(), bytes.as_slice()))
                .collect::<Vec<_>>(),
        ).map_err(|error| match error {
            small_object_store::StoreError::Database(error) => StoreError::from(error),
            small_object_store::StoreError::Corrupt => invalid("Existing capture object differs"),
            other => invalid(&other.to_string()),
        })?;
        tx.commit()?;
        self.staged.clear();
        self.staged_bytes = 0;
        // Between groups, never inside one. A reader on another connection can
        // leave the log where it is; this group is already durable, and what
        // the checkpoint leaves decides whether the next one is admitted.
        self.checkpoint = small_object_store::checkpoint(&self.db)
            .unwrap_or(small_object_store::Checkpoint::UNKNOWN);
        Ok(())
    }

    /// Drop what a cancelled or restarted capture staged, so nothing records a
    /// reference to a body this store never wrote.
    pub(crate) fn discard(&mut self) {
        self.staged.clear();
        self.staged_bytes = 0;
    }

    pub(crate) fn stat(&self, digest: &str) -> Result<Option<u64>> {
        Ok(self.locate(digest)?.map(|(size, _)| size))
    }

    /// The length of a body and, when a file holds it, that file: what `stat`
    /// and `file_path` answer, from one lookup. Staged bodies are small, so no
    /// file holds them.
    pub(crate) fn locate(&self, digest: &str) -> Result<Option<(u64, Option<PathBuf>)>> {
        if let Some(bytes) = self.staged.get(digest) {
            return Ok(Some((bytes.len() as u64, None)));
        }
        if let Some(size) = small_object_store::size(&self.db, digest)
            .map_err(|_| invalid("Capture object identity is invalid"))?
        {
            return Ok(Some((size, None)));
        }
        Ok(self.locate_file(digest)?.map(|(size, path)| (size, Some(path))))
    }

    fn locate_file(&self, digest: &str) -> Result<Option<(u64, PathBuf)>> {
        match self.find_file(digest, |path| fs::symlink_metadata(path))? {
            None => Ok(None),
            Some((metadata, _)) if is_link_like(&metadata) || !metadata.is_file() => {
                Err(invalid("Capture object must not be a link"))
            }
            Some((metadata, path)) => Ok(Some((metadata.len(), path))),
        }
    }

    /// Asks `look` where this body's file is, at the moment of asking: in its
    /// shard, then in the flat directory while that existed at open, then in
    /// its shard again, because the move out of the flat directory may have
    /// taken it there between the first two looks. Only NotFound moves on.
    fn find_file<T>(
        &self,
        digest: &str,
        mut look: impl FnMut(&Path) -> io::Result<T>,
    ) -> Result<Option<(T, PathBuf)>> {
        let sharded = sharded_path(&self.files, digest)?;
        let flat = self.flat.as_ref().map(|flat| flat.join(digest));
        let again = flat.as_ref().map(|_| &sharded);
        for path in std::iter::once(&sharded).chain(&flat).chain(again) {
            match look(path) {
                Ok(found) => return Ok(Some((found, path.clone()))),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(None)
    }

    pub(crate) fn open_body(&self, digest: &str) -> Result<Body> {
        if let Some(bytes) = self.staged.get(digest) {
            return Ok(Body::bytes(bytes.clone()));
        }
        let stored = small_object_store::read(&self.db, digest, SMALL_OBJECT_BYTES)
            .map_err(|_| invalid("Capture object body differs from its identity"))?;
        if let Some(bytes) = stored {
            return Ok(Body::bytes(bytes));
        }
        match self.find_file(digest, crate::trust_boundary::open_regular_source)? {
            Some((file, _)) => Ok(Body::File(file)),
            None => Err(io::Error::from(io::ErrorKind::NotFound).into()),
        }
    }

    /// The file holding this body, when a file is where it lives. A body the
    /// database holds has no path, and a caller protecting physical inputs
    /// names it by identity instead.
    pub(crate) fn file_path(&self, digest: &str) -> Result<Option<PathBuf>> {
        if small_object_store::size(&self.db, digest)
            .map_err(|_| invalid("Capture object identity is invalid"))?
            .is_some()
        {
            return Ok(None);
        }
        Ok(self.locate_file(digest)?.map(|(_, path)| path))
    }

    /// The whole body, for callers that need it in memory anyway.
    pub(crate) fn read_all(&self, digest: &str) -> Result<Vec<u8>> {
        use std::io::Read as _;

        let mut body = self.open_body(digest)?;
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub(crate) fn open_source(&self, source: &ObjectSource) -> Result<Body> {
        match source {
            ObjectSource::File(path) => Ok(Body::File(
                crate::trust_boundary::open_regular_source(path)?,
            )),
            ObjectSource::Captured(digest) => self.open_body(digest),
            // A library body belongs to the asset repository, which is the only
            // store that may hand one out.
            ObjectSource::Library(_) => Err(invalid("Capture source is not a capture body")),
        }
    }

    /// Confirm one object is present at the stated length, and when asked, that
    /// what is stored still matches the identity it is filed under.
    pub(crate) fn validate(&self, digest: &str, bytes: i64, contents: bool) -> Result<()> {
        self.validate_checked(digest, bytes, contents, &|| false)
    }

    /// `validate`, asking `cancelled` before every read of the body.
    pub(crate) fn validate_checked(
        &self,
        digest: &str,
        bytes: i64,
        contents: bool,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        let expected =
            u64::try_from(bytes).map_err(|_| invalid("Capture object length is invalid"))?;
        match self.stat(digest)? {
            Some(size) if size == expected => (),
            Some(_) => return Err(invalid("Capture object length differs")),
            None => return Err(invalid("Capture object is missing")),
        }
        if !contents {
            return Ok(());
        }
        let mut body = CancellableRead::new(self.open_body(digest)?, cancelled);
        let actual = hash_reader(&mut body, expected).map_err(|_| {
            if body.refused() {
                invalid("Capture object check cancelled")
            } else {
                invalid("Capture object body differs from its identity")
            }
        })?;
        if hex::encode(actual) != digest {
            return Err(invalid("Capture object body differs from its identity"));
        }
        Ok(())
    }

    /// Removes the file bodies `keep` does not name, and the partial files a
    /// writer left, at most `budget` of them, from the flat directory while it
    /// exists and from every shard. The caller has established that nothing is
    /// writing a body. A file that is gone by the time it would be removed,
    /// because the move out of the flat directory took it, is passed over.
    pub(crate) fn remove_unnamed_files(&self, keep: &BTreeSet<String>, budget: usize) -> Result<usize> {
        let mut directories: Vec<PathBuf> = self.flat.iter().cloned().collect();
        match fs::read_dir(&self.files) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    let shard = entry.file_name().to_str().is_some_and(|name| {
                        name.len() == 2 && name.bytes().all(crate::trust_boundary::is_lower_hex_byte)
                    });
                    if shard && entry.file_type()?.is_dir() {
                        directories.push(entry.path());
                    }
                }
            }
        }
        let mut removed = 0;
        for directory in directories {
            if removed == budget {
                break;
            }
            removed += remove_unnamed_in(&directory, keep, budget - removed)?;
        }
        Ok(removed)
    }

    /// Identities and sizes of the database bodies, in identity order. Files
    /// keep their own enumeration through `remove_unnamed_files`.
    pub(crate) fn page(&self, after: &str, limit: usize) -> Result<Vec<(String, u64)>> {
        small_object_store::page(&self.db, after, limit)
            .map_err(|_| invalid("Capture content store is unavailable"))
    }

    /// Remove a group the caller has established is unreferenced.
    pub(crate) fn delete(&mut self, digests: &[&str]) -> Result<usize> {
        let tx = self.db.transaction()?;
        let removed = small_object_store::delete_batch(&tx, digests)
            .map_err(|_| invalid("Capture object identity is invalid"))?;
        tx.commit()?;
        if removed > 0 {
            self.db
                .execute_batch("PRAGMA incremental_vacuum; PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        Ok(removed)
    }

    #[cfg(test)]
    pub(crate) fn set_wal_high_water(&mut self, bytes: u64) {
        self.wal_high_water = bytes;
    }

    /// Publish through the file layout whatever the size, so a measurement can
    /// compare the two placements over the same bodies.
    #[cfg(test)]
    pub(crate) fn put_as_file(&self, expected: &str, bytes: &[u8]) -> Result<()> {
        self.write_file(expected, bytes)
    }

    /// The shared immutable file path, kept for bodies above the threshold.
    fn write_file(&self, expected: &str, bytes: &[u8]) -> Result<()> {
        use std::io::Write as _;
        self.write_file_with(expected, bytes.len() as u64, |file| Ok(file.write_all(bytes)?))
    }

    pub(crate) fn put_reader(&mut self, expected: &str, mut input: impl std::io::Read, length: u64) -> Result<()> {
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        if length <= SMALL_OBJECT_BYTES as u64 {
            let mut bytes = Vec::new(); input.take(length + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 != length { return Err(invalid("Capture object length differs")); }
            return self.put(expected, &bytes);
        }
        self.write_file_with(expected, length, |file| {
            let mut buffer = [0; 64 * 1024]; let mut digest = Sha256::new(); let mut copied = 0u64;
            loop {
                let count = input.read(&mut buffer)?; if count == 0 { break; }
                copied += count as u64;
                if copied > length { return Err(invalid("Capture object length differs")); }
                file.write_all(&buffer[..count])?; digest.update(&buffer[..count]);
            }
            #[cfg(test)] crate::persistent_store::hash_work::observe("external_content_put_verify", copied as usize);
            if copied != length || hex::encode(digest.finalize()) != expected { return Err(invalid("Capture object identity differs")); }
            Ok(())
        })
    }

    fn write_file_with(&self, expected: &str, length: u64, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
        let path = sharded_path(&self.files, expected)?;
        let directory = ensure_shard(&self.files, &path)?;
        // Beside its destination, so one directory sync covers both.
        let staging_path = directory.join(format!(".capture-{}.partial", uuid::Uuid::new_v4()));
        let mut staging_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging_path)?;
        let mut staging = super::capture::OwnedCaptureFile::new(staging_path.clone(), &directory);
        write(&mut staging_file)?;
        staging_file.sync_all()?;
        drop(staging_file);
        let _ = sync_directory(&directory)?;
        #[cfg(target_os = "android")]
        let publication = crate::trust_boundary::rename_without_replace(&staging_path, &path);
        #[cfg(not(target_os = "android"))]
        let publication = fs::hard_link(&staging_path, &path);
        match publication {
            Ok(()) => {
                let _ = sync_directory(&directory)?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if is_link_like(&fs::symlink_metadata(&path)?) {
                    return Err(invalid("Capture object must not be a link"));
                }
                let mut file = File::open(&path)?;
                let actual = hash_reader(&mut file, length)
                    .map_err(|_| invalid("Existing capture object differs"))?;
                if hex::encode(actual) != expected {
                    return Err(invalid("Existing capture object differs"));
                }
            }
            Err(error) => return Err(error.into()),
        }
        let _ = staging.remove_and_sync()?;
        Ok(())
    }

    /// Flush the file directory after a capture publishes its files. Each
    /// publication already synced its shard, and each new shard its parent.
    pub(crate) fn sync_objects(&self) -> Result<()> {
        sync_directory(&self.files)?;
        Ok(())
    }
}

/// The flat directory an earlier version kept file bodies in, while it exists.
fn flat_directory(root: &Path) -> Result<Option<PathBuf>> {
    let path = root.join(FLAT_FILE_DIRECTORY);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(metadata) if is_link_like(&metadata) || !metadata.is_dir() => {
            Err(invalid("Capture directory must not be a link"))
        }
        Ok(_) => Ok(Some(path)),
    }
}

/// Where the store under `root` keeps the file holding `digest`.
#[cfg(test)]
pub(crate) fn file_location(root: &Path, digest: &str) -> PathBuf {
    sharded_path(&root.join(FILE_DIRECTORY), digest).unwrap()
}

/// Writes `bytes` as the file body of `digest` where publication leaves one,
/// for fixtures that fill a store without a capture.
#[cfg(test)]
pub(crate) fn write_file_fixture(root: &Path, digest: &str, bytes: &[u8]) {
    let path = file_location(root, digest);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    if !path.exists() {
        fs::write(path, bytes).unwrap();
    }
}

fn sharded_path(files: &Path, digest: &str) -> Result<PathBuf> {
    if !is_lower_hex_256(digest) {
        return Err(invalid("Capture object identity is invalid"));
    }
    Ok(files.join(&digest[..2]).join(digest))
}

/// The shard directory of `path`, created the first time a body needs it.
fn ensure_shard(files: &Path, path: &Path) -> Result<PathBuf> {
    let directory = path.parent().expect("a sharded path has a shard").to_path_buf();
    let real = |metadata: &fs::Metadata| metadata.is_dir() && !is_link_like(metadata);
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if real(&metadata) => return Ok(directory),
        Ok(_) => return Err(invalid("Capture directory must not be a link")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match fs::create_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    // Whoever created it, its entry is durable before a body goes in it.
    sync_directory(files)?;
    if !real(&fs::symlink_metadata(&directory)?) {
        return Err(invalid("Capture directory must not be a link"));
    }
    Ok(directory)
}

fn remove_unnamed_in(directory: &Path, keep: &BTreeSet<String>, budget: usize) -> Result<usize> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut removed = 0;
    for entry in entries {
        if removed == budget {
            break;
        }
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // A partial file is a body a capture was writing, and none is.
        let collectable = (is_lower_hex_256(&name) && !keep.contains(&name))
            || (name.starts_with(".capture-") && name.ends_with(".partial"));
        if !collectable {
            continue;
        }
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() || is_link_like(&metadata) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if removed > 0 {
        sync_directory(directory)?;
    }
    Ok(removed)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Moves at most `limit` entries out of the flat directory under `root` and
/// removes that directory once nothing in it is left to move. True while
/// entries may remain. A crash leaves each move either undone or done, and a
/// reader finds a body in either place, so a pass can stop anywhere and the
/// next one carries on.
pub(crate) fn move_flat_files(root: &Path, limit: usize) -> Result<bool> {
    let Some(flat) = flat_directory(root)? else {
        return Ok(false);
    };
    let files = root.join(FILE_DIRECTORY);
    // Not `create_dir_all`: a root that cleanup removed is not recreated.
    match fs::create_dir(&files) {
        Ok(()) => {
            sync_directory(root)?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(&files)?;
    if !metadata.is_dir() || is_link_like(&metadata) {
        return Err(invalid("Capture directory must not be a link"));
    }
    let (mut bodies, mut partials) = (Vec::new(), Vec::new());
    let (mut kept, mut more) = (false, false);
    // Listing order: a lookup in a directory without an index starts where
    // the last one ended, so moving in this order does not rescan it.
    for entry in fs::read_dir(&flat)? {
        let name = entry?.file_name();
        match name.to_str() {
            Some(name) if is_lower_hex_256(name) => bodies.push(name.to_owned()),
            Some(name) if name.starts_with(".capture-") && name.ends_with(".partial") => {
                partials.push(name.to_owned())
            }
            _ => kept = true,
        }
        if bodies.len() + partials.len() == limit {
            more = true;
            break;
        }
    }
    let mut touched = BTreeSet::new();
    // Whether this step changed what the next listing shows.
    let mut progress = !partials.is_empty();
    for digest in &bodies {
        let source = flat.join(digest);
        match fs::symlink_metadata(&source) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                progress = true;
                continue;
            }
            Err(error) => return Err(error.into()),
            Ok(metadata) if !metadata.is_file() || is_link_like(&metadata) => {
                kept = true;
                continue;
            }
            Ok(_) => {}
        }
        let target = sharded_path(&files, digest)?;
        let directory = ensure_shard(&files, &target)?;
        if move_body(&source, &target, digest)? {
            touched.insert(directory);
            progress = true;
        } else {
            kept = true;
        }
    }
    // The new layout never writes a partial here, and an older version does
    // not run beside this one, so whatever is here was abandoned.
    for partial in &partials {
        remove_if_present(&flat.join(partial))?;
    }
    for directory in &touched {
        sync_directory(directory)?;
    }
    if !bodies.is_empty() || !partials.is_empty() {
        sync_directory(&flat)?;
    }
    // A step that left every entry where it was would list them again.
    if more && progress {
        return Ok(true);
    }
    if kept || more {
        return Ok(false);
    }
    match fs::remove_dir(&flat) {
        Ok(()) => {
            sync_directory(root)?;
            Ok(false)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Moves one flat body into its shard with the primitive publication uses.
/// False when the shard already holds other bytes under that identity, which
/// leaves the flat file where it is.
fn move_body(source: &Path, target: &Path, digest: &str) -> Result<bool> {
    #[cfg(target_os = "android")]
    let moved = crate::trust_boundary::rename_without_replace(source, target);
    #[cfg(not(target_os = "android"))]
    let moved = fs::hard_link(source, target);
    match moved {
        Ok(()) => {
            #[cfg(not(target_os = "android"))]
            remove_if_present(source)?;
            Ok(true)
        }
        // Collected after it was listed.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        // A writer published it again before the move reached it, or an
        // earlier move stopped between linking and removing.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(target)?;
            if !metadata.is_file() || is_link_like(&metadata) {
                return Ok(false);
            }
            let mut file = crate::trust_boundary::open_regular_source(target)?;
            if !hash_reader(&mut file, metadata.len()).is_ok_and(|actual| hex::encode(actual) == digest) {
                return Ok(false);
            }
            remove_if_present(source)?;
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

/// Moves an earlier version's flat file bodies on a thread of its own, one
/// step at a time, until none is left, app cleanup begins or a step fails.
/// Startup does not wait for it, and readers find a body in either place.
pub(crate) fn move_flat_files_later(app: &tauri::AppHandle, root: PathBuf) {
    use tauri::Manager as _;
    match flat_directory(&root) {
        Ok(Some(_)) => {}
        Ok(None) => return,
        Err(error) => {
            crate::nlog!("warn", "External content files were not moved: {error}");
            return;
        }
    }
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("content-files-move".into())
        .spawn(move || {
            let jobs = app.state::<super::job_store::JobCommandState>();
            loop {
                let Some(_step) = jobs.background_step() else {
                    return;
                };
                match move_flat_files(&root, FLAT_MOVE_BATCH) {
                    Ok(true) => {}
                    Ok(false) => {
                        if matches!(flat_directory(&root), Ok(Some(_))) {
                            crate::nlog!("warn", "Some external content files were not moved");
                        }
                        return;
                    }
                    Err(error) => {
                        crate::nlog!("warn", "External content files were not moved: {error}");
                        return;
                    }
                }
            }
        });
    if let Err(error) = spawned {
        crate::nlog!("warn", "External content files were not moved: {error}");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn failed_content_transactions_retain_the_staged_group_for_retry() {
        for failure in ["begin", "insert", "commit"] {
            let root = tempfile::tempdir().unwrap();
            let mut store = ContentStore::open(root.path()).unwrap();
            let bytes = body(17, 128);
            let hash = digest(&bytes);
            store.put(&hash, &bytes).unwrap();
            match failure {
                "begin" => store.db.execute_batch("BEGIN IMMEDIATE").unwrap(),
                "insert" => store.db.execute_batch(
                    "CREATE TEMP TRIGGER deny_content BEFORE INSERT ON small_objects
                     BEGIN SELECT RAISE(ABORT,'synthetic write failure'); END;",
                ).unwrap(),
                _ => store.db.execute_batch(
                    "PRAGMA foreign_keys=ON;
                     CREATE TABLE parent(id INTEGER PRIMARY KEY);
                     CREATE TABLE child(id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);
                     CREATE TEMP TRIGGER deny_commit AFTER INSERT ON small_objects
                     BEGIN INSERT INTO child VALUES(1); END;",
                ).unwrap(),
            }
            let error = store.commit().expect_err(failure);
            assert!(!error.to_string().contains("Existing capture object differs"));
            assert_eq!(store.staged.get(&hash), Some(&bytes));
            assert_eq!(store.staged_bytes, bytes.len());
            match failure {
                "begin" => store.db.execute_batch("ROLLBACK").unwrap(),
                "insert" => store.db.execute_batch("DROP TRIGGER deny_content").unwrap(),
                _ => store.db.execute_batch("DROP TRIGGER deny_commit").unwrap(),
            }
            store.commit().unwrap();
            assert!(store.staged.is_empty());
            assert_eq!(store.stat(&hash).unwrap(), Some(bytes.len() as u64));
        }
    }

    use super::*;
    use std::io::Read as _;

    fn body(seed: usize, len: usize) -> Vec<u8> {
        let mut bytes = format!("synthetic capture body {seed:08} ").into_bytes();
        bytes.resize(len, (seed % 251) as u8);
        bytes
    }

    fn digest(bytes: &[u8]) -> String {
        hex::encode(hash(bytes))
    }

    #[test]
    fn a_body_goes_to_the_store_its_size_belongs_to_and_reads_back_either_way() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ContentStore::open(root.path()).unwrap();
        let small = body(1, SMALL_OBJECT_BYTES);
        let large = body(2, SMALL_OBJECT_BYTES + 1);
        for bytes in [&small, &large] {
            store.put(&digest(bytes), bytes).unwrap();
        }
        store.commit().unwrap();
        assert!(store.file_path(&digest(&small)).unwrap().is_none());
        assert_eq!(
            store.file_path(&digest(&large)).unwrap(),
            Some(file_location(root.path(), &digest(&large)))
        );
        assert!(!root.path().join(FLAT_FILE_DIRECTORY).exists());
        for bytes in [&small, &large] {
            let hash = digest(bytes);
            assert_eq!(store.stat(&hash).unwrap(), Some(bytes.len() as u64));
            assert_eq!(&store.read_all(&hash).unwrap(), bytes);
            store.validate(&hash, bytes.len() as i64, true).unwrap();
            assert!(store.validate(&hash, bytes.len() as i64 - 1, false).is_err());
        }
        // Offering one identity's bytes under another is never a placement.
        assert!(store.put(&digest(&large), &small).is_err());
        assert!(store.stat(&digest(&body(3, 8))).unwrap().is_none());
    }

    /// A reader holding the log stops a capture before its log passes the
    /// mark by more than one group, keeps the refused group staged, and lets
    /// the same group land once the reader is gone.
    #[test]
    fn a_held_reader_refuses_the_next_group_and_its_release_admits_it() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ContentStore::open(root.path()).unwrap();
        store.set_wal_high_water(512 * 1024);
        let first = body(1, 4096);
        store.put(&digest(&first), &first).unwrap();
        store.commit().unwrap();
        let reader = rusqlite::Connection::open(root.path().join("content.sqlite")).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let _: i64 = reader
            .query_row("SELECT count(*) FROM small_objects", [], |r| r.get(0))
            .unwrap();
        let mut written = vec![first];
        let refused = loop {
            let group = (0..32)
                .map(|index| body(1000 + written.len() * 32 + index, 4096))
                .collect::<Vec<_>>();
            for bytes in &group {
                store.put(&digest(bytes), bytes).unwrap();
            }
            match store.commit() {
                Ok(()) => written.extend(group),
                Err(error) => {
                    assert!(error.to_string().contains("could not be saved now"), "{error}");
                    break group;
                }
            }
            assert!(written.len() < 32 * 64, "the held reader never stopped the capture");
        };
        let log = std::fs::metadata(root.path().join("content.sqlite-wal")).unwrap().len();
        assert!(log < 2 * 1024 * 1024, "the log reached {log} bytes");
        // The refused group is still what the capture holds.
        for bytes in &refused {
            assert_eq!(&store.read_all(&digest(bytes)).unwrap(), bytes);
        }
        reader.execute_batch("COMMIT").unwrap();
        drop(reader);
        store.commit().unwrap();
        drop(store);
        let store = ContentStore::open(root.path()).unwrap();
        for bytes in written.iter().chain(&refused) {
            assert_eq!(&store.read_all(&digest(bytes)).unwrap(), bytes);
        }
    }

    #[test]
    fn a_staged_group_reads_back_before_it_lands_and_an_abandoned_one_leaves_nothing() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ContentStore::open(root.path()).unwrap();
        let kept = body(10, 1024);
        let abandoned = body(11, 1024);
        store.put(&digest(&kept), &kept).unwrap();
        // A caller that was handed an identity can use it before the group
        // becomes durable.
        assert_eq!(store.read_all(&digest(&kept)).unwrap(), kept);
        store.commit().unwrap();
        store.put(&digest(&abandoned), &abandoned).unwrap();
        store.discard();
        assert!(store.stat(&digest(&abandoned)).unwrap().is_none());

        // A group that reaches the bound is written before the caller asks,
        // so an abandoned capture leaves collectable bodies, not lost memory.
        let staged = (0..GROUP_ROWS + 1)
            .map(|seed| {
                let bytes = body(100 + seed, 1024);
                let hash = digest(&bytes);
                store.put(&hash, &bytes).unwrap();
                hash
            })
            .collect::<Vec<_>>();
        store.discard();
        let durable = staged
            .iter()
            .filter(|hash| store.stat(hash).unwrap().is_some())
            .count();
        assert_eq!(durable, GROUP_ROWS);

        let reopened = ContentStore::open(root.path()).unwrap();
        assert_eq!(reopened.read_all(&digest(&kept)).unwrap(), kept);
        assert!(reopened.stat(&digest(&abandoned)).unwrap().is_none());
    }

    #[test]
    fn a_damaged_body_is_reported_and_collection_returns_its_pages() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ContentStore::open(root.path()).unwrap();
        let bytes = body(20, 4096);
        let hash = digest(&bytes);
        store.put(&hash, &bytes).unwrap();
        store.commit().unwrap();
        store
            .db
            .execute(
                "UPDATE small_objects SET body=?1 WHERE hash=?2",
                rusqlite::params![b"damaged".to_vec(), hash],
            )
            .unwrap();
        assert!(store.read_all(&hash).is_err());
        assert!(store.validate(&hash, bytes.len() as i64, true).is_err());

        let filler = (0..64)
            .map(|seed| {
                let bytes = body(200 + seed, SMALL_OBJECT_BYTES / 2);
                let hash = digest(&bytes);
                store.put(&hash, &bytes).unwrap();
                hash
            })
            .collect::<Vec<_>>();
        store.commit().unwrap();
        let before = fs::metadata(root.path().join("content.sqlite")).unwrap().len();
        assert_eq!(
            store
                .delete(&filler.iter().map(String::as_str).collect::<Vec<_>>())
                .unwrap(),
            filler.len()
        );
        assert!(fs::metadata(root.path().join("content.sqlite")).unwrap().len() < before);
        assert_eq!(store.page("", 100).unwrap().len(), 1);
    }

    #[test]
    fn attaching_reads_an_existing_store_and_creates_nothing() {
        let root = tempfile::tempdir().unwrap();
        assert!(ContentStore::open_existing(&root.path().join("absent"))
            .unwrap()
            .is_none());
        assert!(!root.path().join("absent").exists());
        let bytes = body(30, 512);
        let mut store = ContentStore::open(root.path()).unwrap();
        store.put(&digest(&bytes), &bytes).unwrap();
        store.commit().unwrap();
        let attached = ContentStore::open_existing(root.path()).unwrap().unwrap();
        assert_eq!(attached.read_all(&digest(&bytes)).unwrap(), bytes);
        let mut opened = attached.open_source(&ObjectSource::Captured(digest(&bytes))).unwrap();
        let mut read = Vec::new();
        opened.read_to_end(&mut read).unwrap();
        assert_eq!(read, bytes);
    }

    /// Bodies above the threshold where an earlier version kept them, all in
    /// one directory.
    fn flat_store(root: &Path, seeds: std::ops::Range<usize>) -> Vec<Vec<u8>> {
        let flat = root.join(FLAT_FILE_DIRECTORY);
        fs::create_dir_all(&flat).unwrap();
        seeds
            .map(|seed| {
                let bytes = body(seed, SMALL_OBJECT_BYTES + 1 + seed);
                fs::write(flat.join(digest(&bytes)), &bytes).unwrap();
                bytes
            })
            .collect()
    }

    #[test]
    fn a_flat_store_moves_into_shards_a_step_at_a_time_and_reads_throughout() {
        let root = tempfile::tempdir().unwrap();
        let bodies = flat_store(root.path(), 0..6);
        let flat = root.path().join(FLAT_FILE_DIRECTORY);
        fs::write(flat.join(".capture-abandoned.partial"), b"synthetic partial").unwrap();
        let before = ContentStore::open(root.path()).unwrap();
        let mut steps = 0;
        while move_flat_files(root.path(), 2).unwrap() {
            steps += 1;
            let now = ContentStore::open(root.path()).unwrap();
            for bytes in &bodies {
                for store in [&before, &now] {
                    assert_eq!(&store.read_all(&digest(bytes)).unwrap(), bytes);
                }
            }
        }
        assert_eq!(steps, 3);
        assert!(!flat.exists());
        let after = ContentStore::open(root.path()).unwrap();
        for bytes in &bodies {
            let hash = digest(bytes);
            assert_eq!(after.file_path(&hash).unwrap(), Some(file_location(root.path(), &hash)));
            for store in [&before, &after] {
                store.validate(&hash, bytes.len() as i64, true).unwrap();
            }
        }
        assert!(!move_flat_files(root.path(), 2).unwrap());
    }

    #[test]
    fn an_interrupted_move_resumes_from_what_it_left() {
        let root = tempfile::tempdir().unwrap();
        let bodies = flat_store(root.path(), 10..14);
        let hashes = bodies.iter().map(|bytes| digest(bytes)).collect::<Vec<_>>();
        let flat = root.path().join(FLAT_FILE_DIRECTORY);
        let shard_file = |index: usize| {
            let path = file_location(root.path(), &hashes[index]);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            path
        };
        // In its shard and still in the flat directory, as a move that linked
        // it and stopped, or a writer that published it again, leaves it.
        fs::copy(flat.join(&hashes[0]), shard_file(0)).unwrap();
        // Other bytes under the identity in its shard.
        fs::write(shard_file(1), b"synthetic damage").unwrap();
        // A shard created for a body that was never moved into it.
        shard_file(2);
        // An entry nothing writes there.
        fs::write(flat.join("synthetic-notes"), b"synthetic").unwrap();

        assert!(!move_flat_files(root.path(), 100).unwrap());
        let store = ContentStore::open(root.path()).unwrap();
        for index in [0, 2, 3] {
            assert!(!flat.join(&hashes[index]).exists());
            assert_eq!(fs::read(shard_file(index)).unwrap(), bodies[index]);
            store.validate(&hashes[index], bodies[index].len() as i64, true).unwrap();
        }
        // Neither copy of the differing identity is removed, and the flat
        // directory stays for it and for what the move does not know.
        assert_eq!(fs::read(flat.join(&hashes[1])).unwrap(), bodies[1]);
        assert_eq!(fs::read(shard_file(1)).unwrap(), b"synthetic damage");
        assert!(flat.join("synthetic-notes").exists());

        fs::remove_file(shard_file(1)).unwrap();
        fs::remove_file(flat.join("synthetic-notes")).unwrap();
        assert!(!move_flat_files(root.path(), 100).unwrap());
        assert!(!flat.exists());
        assert_eq!(fs::read(shard_file(1)).unwrap(), bodies[1]);
        // An empty flat directory left behind goes on the next pass.
        fs::create_dir(&flat).unwrap();
        assert!(!move_flat_files(root.path(), 100).unwrap());
        assert!(!flat.exists());

        // A step that leaves every entry where it was ends the move instead
        // of listing the same entries again.
        for bytes in flat_store(root.path(), 30..32) {
            let path = file_location(root.path(), &digest(&bytes));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"synthetic damage").unwrap();
        }
        assert!(!move_flat_files(root.path(), 1).unwrap());
        assert!(flat.exists());
    }

    /// The move takes a body to its shard between the look there and the
    /// look in the flat directory, and the third look finds it.
    #[test]
    fn a_body_moved_between_two_looks_is_still_found() {
        let root = tempfile::tempdir().unwrap();
        let bytes = flat_store(root.path(), 20..21).remove(0);
        let hash = digest(&bytes);
        let store = ContentStore::open(root.path()).unwrap();
        let mut looks = 0;
        let found = store
            .find_file(&hash, |path| {
                let read = fs::read(path);
                looks += 1;
                if looks == 1 {
                    assert!(!move_flat_files(root.path(), 100).unwrap());
                }
                read
            })
            .unwrap();
        assert_eq!(found, Some((bytes, file_location(root.path(), &hash))));
        assert_eq!(looks, 3);
    }

    #[test]
    #[ignore = "Explicit synthetic capture durability measurement"]
    fn capture_publication_cost_measurement() {
        const BODIES: usize = 1000;
        let root = tempfile::tempdir().unwrap();
        let bodies = (0..BODIES).map(|seed| body(seed, 2048)).collect::<Vec<_>>();
        for (pass, as_files) in [(0, true), (1, false)] {
            let directory = root.path().join(format!("pass-{pass}"));
            let mut store = ContentStore::open(&directory).unwrap();
            let started = std::time::Instant::now();
            for bytes in &bodies {
                let hash = digest(bytes);
                if as_files {
                    store.put_as_file(&hash, bytes).unwrap();
                } else {
                    store.put(&hash, bytes).unwrap();
                }
            }
            store.commit().unwrap();
            let files = fs::read_dir(directory.join(FILE_DIRECTORY))
                .unwrap()
                .map(|shard| fs::read_dir(shard.unwrap().path()).unwrap().count())
                .sum::<usize>();
            eprintln!(
                "pass={pass} as_files={as_files} bodies={BODIES} object_files={files} elapsed_ms={}",
                started.elapsed().as_millis()
            );
        }
    }
}
