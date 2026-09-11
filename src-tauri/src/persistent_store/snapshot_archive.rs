//! Recovery snapshots share immutable pages in a separate transactional archive.
use super::{SnapshotInfo, StoreError, StoreResult};
use crate::asset_repository::migration_gc::{validate_root_set, AssetRootSet};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const ARCHIVE_FILE: &str = "snapshots.sqlite";
const APPLICATION_ID: i64 = 0x524e5350;
const CHUNK_BYTES: usize = 4096;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Metadata {
    pub id: String,
    pub reason: String,
    pub revision: i64,
    pub bytes: u64,
    pub created_at: u64,
    pub file_hash: Vec<u8>,
    pub roots: AssetRootSet,
}

pub(super) struct Archive {
    connection: Connection,
    directory: PathBuf,
    // Held across capture, publication, restore and collection, including across processes.
    _lock: File,
}

pub(super) struct Scratch {
    pub path: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let path = PathBuf::from(format!("{}{suffix}", self.path.display()));
            if let Err(error) = fs::remove_file(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    crate::nlog!("warn", "snapshot temporary file cleanup failed: {error}");
                }
            }
        }
    }
}

impl Archive {
    pub fn open(directory: &Path) -> StoreResult<Self> {
        fs::create_dir_all(directory)?;
        reject_link(directory)?;
        let lock_path = directory.join("archive.lock");
        reject_link_if_present(&lock_path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock_archive(&lock)?;
        // Earlier RisuNest recovery formats are neither migrated nor removed.
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if entry.path().extension().is_some_and(|ext| ext == "db")
                || name == "pending-restore.json"
            {
                return Err(invalid(
                    "unsupported snapshot format; existing recovery files were preserved",
                ));
            }
            if is_scratch_name(&name) {
                reject_link(&entry.path())?;
                if entry.file_type()?.is_file() {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        let path = directory.join(ARCHIVE_FILE);
        for suffix in ["", "-journal", "-wal", "-shm"] {
            reject_link_if_present(&PathBuf::from(format!("{}{suffix}", path.display())))?;
        }
        let mut connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA cache_size=-2048;",
        )?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let application: i64 = connection.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        if version == 0 && application == 0 {
            let tables: i64 =
                connection.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
            if tables != 0 {
                return Err(invalid("unrecognized snapshot archive"));
            }
            connection.execute_batch("PRAGMA auto_vacuum=FULL; PRAGMA journal_mode=DELETE;")?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch("CREATE TABLE chunks(hash BLOB PRIMARY KEY CHECK(length(hash)=32), data BLOB NOT NULL CHECK(length(data) BETWEEN 1 AND 4096));
                CREATE TABLE snapshots(id TEXT PRIMARY KEY, metadata BLOB NOT NULL, checksum BLOB NOT NULL CHECK(length(checksum)=32));
                CREATE TABLE snapshot_pages(snapshot_id TEXT NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
                    seq INTEGER NOT NULL CHECK(seq>=0), hash BLOB NOT NULL REFERENCES chunks(hash), PRIMARY KEY(snapshot_id,seq));
                CREATE INDEX snapshot_pages_hash ON snapshot_pages(hash);
                CREATE TABLE pending_restore(singleton INTEGER PRIMARY KEY CHECK(singleton=1), id TEXT NOT NULL REFERENCES snapshots(id));
                PRAGMA user_version=1;")?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.commit()?;
        } else if version != 1 || application != APPLICATION_ID {
            return Err(invalid("unsupported snapshot archive version"));
        }
        Ok(Self {
            connection,
            directory: directory.to_owned(),
            _lock: lock,
        })
    }

    pub fn scratch(&self) -> StoreResult<Scratch> {
        let path = self
            .directory
            .join(format!("capture-{}.tmp", Uuid::new_v4()));
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        Ok(Scratch { path })
    }

    pub fn insert(
        &mut self,
        source: &Path,
        revision: i64,
        reason: &str,
        roots: AssetRootSet,
    ) -> StoreResult<Metadata> {
        if revision < 0 {
            return Err(invalid("snapshot revision must be nonnegative"));
        }
        validate_root_set(&roots)?;
        let mut input = File::open(source)?;
        let expected_bytes = input.metadata()?.len();
        let mut metadata = Metadata {
            id: Uuid::new_v4().to_string(),
            reason: reason.chars().take(128).collect(),
            revision,
            bytes: 0,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| invalid("snapshot clock precedes epoch"))?
                .as_millis() as u64,
            file_hash: Vec::new(),
            roots,
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; CHUNK_BYTES];
        let mut seq = 0_i64;
        while metadata.bytes < expected_bytes {
            let length = (expected_bytes - metadata.bytes).min(CHUNK_BYTES as u64) as usize;
            input.read_exact(&mut buffer[..length])?;
            let data = &buffer[..length];
            let hash = Sha256::digest(data).to_vec();
            tx.execute(
                "INSERT OR IGNORE INTO chunks(hash,data) VALUES(?1,?2)",
                params![hash, data],
            )?;
            let existing: Vec<u8> =
                tx.query_row("SELECT data FROM chunks WHERE hash=?1", [&hash], |r| {
                    r.get(0)
                })?;
            if existing != data {
                return Err(invalid("snapshot chunk is corrupt or has a hash collision"));
            }
            tx.execute(
                "INSERT INTO snapshot_pages(snapshot_id,seq,hash) VALUES(?1,?2,?3)",
                params![metadata.id, seq, hash],
            )?;
            hasher.update(data);
            metadata.bytes += length as u64;
            seq += 1;
        }
        if expected_bytes == 0 || input.read(&mut buffer[..1])? != 0 {
            return Err(invalid("snapshot source length changed"));
        }
        metadata.file_hash = hasher.finalize().to_vec();
        let encoded = serde_json::to_vec(&metadata)?;
        let checksum = Sha256::digest(&encoded).to_vec();
        tx.execute(
            "INSERT INTO snapshots(id,metadata,checksum) VALUES(?1,?2,?3)",
            params![metadata.id, encoded, checksum],
        )?;
        tx.commit()?;
        Ok(metadata)
    }

    pub fn metadata(&self, id: &str) -> StoreResult<Metadata> {
        validate_id(id)?;
        let encoded = self
            .connection
            .query_row(
                "SELECT metadata,checksum FROM snapshots WHERE id=?1",
                [id],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
            .ok_or_else(|| invalid("snapshot is not available"))?;
        if Sha256::digest(&encoded.0).as_slice() != encoded.1 {
            return Err(invalid("snapshot metadata checksum mismatch"));
        }
        let metadata: Metadata = serde_json::from_slice(&encoded.0)?;
        if metadata.id != id
            || metadata.bytes == 0
            || metadata.file_hash.len() != 32
            || metadata.revision < 0
        {
            return Err(invalid("invalid snapshot metadata"));
        }
        validate_root_set(&metadata.roots)?;
        Ok(metadata)
    }

    pub fn list(&self) -> StoreResult<Vec<SnapshotInfo>> {
        let mut statement = self.connection.prepare("SELECT id FROM snapshots")?;
        let ids = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let metadata = self.metadata(&id)?;
            let reclaimable_bytes: i64 = self.connection.query_row(
                "SELECT coalesce(sum(length(data)),0) FROM chunks WHERE hash IN (SELECT hash FROM snapshot_pages WHERE snapshot_id=?1)
                 AND hash NOT IN (SELECT hash FROM snapshot_pages WHERE snapshot_id<>?1)", [&id], |r| r.get(0))?;
            result.push(SnapshotInfo {
                id,
                bytes: metadata.bytes,
                modified_at: metadata.created_at,
                reason: metadata.reason,
                reclaimable_bytes: u64::try_from(reclaimable_bytes)
                    .map_err(|_| invalid("invalid snapshot size"))?,
            });
        }
        result.sort_by(|a, b| {
            b.modified_at
                .cmp(&a.modified_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(result)
    }

    pub fn roots(&self) -> StoreResult<Vec<AssetRootSet>> {
        self.list()?
            .iter()
            .map(|s| self.metadata(&s.id).map(|m| m.roots))
            .collect()
    }

    pub fn restore(&self, id: &str, destination: &Path) -> StoreResult<Metadata> {
        let metadata = self.metadata(id)?;
        let mut output = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(destination)?;
        let mut statement = self.connection.prepare(
            "SELECT p.seq,p.hash,c.data FROM snapshot_pages p LEFT JOIN chunks c ON c.hash=p.hash WHERE p.snapshot_id=?1 ORDER BY p.seq")?;
        let mut rows = statement.query([id])?;
        let mut hasher = Sha256::new();
        let mut bytes = 0_u64;
        let mut seq = 0_i64;
        while let Some(row) = rows.next()? {
            let actual_seq: i64 = row.get(0)?;
            let hash: Vec<u8> = row.get(1)?;
            let data: Option<Vec<u8>> = row.get(2)?;
            let data = data.ok_or_else(|| invalid("snapshot chunk is missing"))?;
            let length = metadata.bytes.saturating_sub(bytes).min(CHUNK_BYTES as u64) as usize;
            if actual_seq != seq
                || length == 0
                || data.len() != length
                || Sha256::digest(&data).as_slice() != hash
            {
                return Err(invalid("snapshot chunk length, order or hash mismatch"));
            }
            output.write_all(&data)?;
            hasher.update(&data);
            bytes += data.len() as u64;
            seq += 1;
        }
        if bytes != metadata.bytes || hasher.finalize().as_slice() != metadata.file_hash {
            return Err(invalid("snapshot whole-file length or hash mismatch"));
        }
        output.sync_all()?;
        Ok(metadata)
    }

    pub fn bytes(&self) -> StoreResult<u64> {
        let mut total = 0_u64;
        for suffix in ["", "-journal", "-wal", "-shm"] {
            match fs::metadata(self.directory.join(format!("{ARCHIVE_FILE}{suffix}"))) {
                Ok(meta) => total = total.saturating_add(meta.len()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(total)
    }

    pub fn pending_restore(&self) -> StoreResult<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT id FROM pending_restore WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn request_restore(&self, id: &str) -> StoreResult<()> {
        self.metadata(id)?;
        self.connection.execute("INSERT INTO pending_restore(singleton,id) VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET id=excluded.id", [id])?;
        Ok(())
    }

    pub fn clear_pending_restore(&self, id: &str) -> StoreResult<()> {
        self.connection
            .execute("DELETE FROM pending_restore WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn delete(&mut self, id: &str) -> StoreResult<()> {
        self.metadata(id)?;
        if self.pending_restore()?.as_deref() == Some(id) {
            return Err(invalid("snapshot is pending restore"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM snapshots WHERE id=?1", [id])?;
        tx.execute("DELETE FROM chunks WHERE NOT EXISTS(SELECT 1 FROM snapshot_pages p WHERE p.hash=chunks.hash)", [])?;
        tx.commit()?;
        Ok(())
    }

    pub fn rotate(&mut self, byte_budget: u64, protected: &str) -> StoreResult<()> {
        let pending = self.pending_restore()?;
        loop {
            let snapshots = self.list()?;
            if snapshots.len() <= 8 && self.bytes()? <= byte_budget {
                break;
            }
            let Some(oldest) = snapshots
                .iter()
                .rev()
                .find(|s| s.id != protected && pending.as_deref() != Some(&s.id))
            else {
                break;
            };
            self.delete(&oldest.id)?;
        }
        Ok(())
    }
}

fn is_scratch_name(name: &str) -> bool {
    let name = ["-journal", "-wal", "-shm"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name);
    name.strip_prefix("capture-")
        .and_then(|name| name.strip_suffix(".tmp"))
        .is_some_and(|id| validate_id(id).is_ok())
}

#[cfg(not(target_os = "android"))]
fn lock_archive(file: &File) -> std::io::Result<()> {
    file.lock()
}

#[cfg(target_os = "android")]
fn lock_archive(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // std::fs::File::lock is unsupported on Android. Closing the owned file
    // releases flock, including after process termination.
    loop {
        // SAFETY: file owns a live descriptor for the duration of this call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn validate_id(id: &str) -> StoreResult<()> {
    if Uuid::parse_str(id)
        .ok()
        .is_none_or(|uuid| uuid.to_string() != id)
    {
        return Err(invalid("invalid snapshot ID"));
    }
    Ok(())
}

fn reject_link_if_present(path: &Path) -> StoreResult<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => reject_link(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn reject_link(path: &Path) -> StoreResult<()> {
    if super::snapshot::snapshot_path_is_link_or_reparse(path)? {
        return Err(invalid("snapshot archive path must not be a link"));
    }
    Ok(())
}

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_snapshots_share_payload_and_survive_independent_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(dir.path()).unwrap();
        let source = archive.scratch().unwrap();
        let bytes: Vec<u8> = (0..100_013)
            .map(|i| ((i * 17 + i / 4096) % 251) as u8)
            .collect();
        fs::write(&source.path, &bytes).unwrap();
        let first = archive
            .insert(&source.path, 1, "first", AssetRootSet::default())
            .unwrap();
        let unique = |a: &Archive| {
            a.connection
                .query_row("SELECT sum(length(data)) FROM chunks", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap()
        };
        let before = unique(&archive);
        let second = archive
            .insert(&source.path, 1, "second", AssetRootSet::default())
            .unwrap();
        assert_eq!(before, unique(&archive));
        assert!(archive
            .list()
            .unwrap()
            .iter()
            .all(|s| s.reclaimable_bytes == 0));
        archive.delete(&first.id).unwrap();
        let restored = archive.scratch().unwrap();
        archive.restore(&second.id, &restored.path).unwrap();
        assert_eq!(fs::read(&restored.path).unwrap(), bytes);
        archive.request_restore(&second.id).unwrap();
        assert!(archive.delete(&second.id).is_err());
        archive.clear_pending_restore(&second.id).unwrap();
        archive.delete(&second.id).unwrap();
        assert!(archive.list().unwrap().is_empty());
    }

    #[test]
    fn corrupt_chunks_and_metadata_fail_closed_and_do_not_publish_a_new_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(dir.path()).unwrap();
        let source = archive.scratch().unwrap();
        fs::write(&source.path, vec![23; 9000]).unwrap();
        let first = archive
            .insert(&source.path, 3, "source", AssetRootSet::default())
            .unwrap();
        archive
            .connection
            .execute("UPDATE chunks SET data=zeroblob(length(data))", [])
            .unwrap();
        let output = archive.scratch().unwrap();
        assert!(archive.restore(&first.id, &output.path).is_err());
        assert!(archive
            .insert(&source.path, 3, "rejected", AssetRootSet::default())
            .is_err());
        assert_eq!(archive.list().unwrap().len(), 1);
        archive
            .connection
            .execute("UPDATE snapshots SET metadata=x'7b7d'", [])
            .unwrap();
        assert!(archive.roots().is_err());
        assert!(archive.restore(&first.id, &output.path).is_err());
    }

    #[test]
    fn invalid_asset_roots_reject_capture_and_checked_metadata_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(dir.path()).unwrap();
        let source = archive.scratch().unwrap();
        fs::write(&source.path, vec![31; 4096]).unwrap();
        let mut metadata = archive
            .insert(&source.path, 1, "valid", Default::default())
            .unwrap();
        metadata.roots.object_hashes.insert("invalid-hash".into());
        assert!(archive
            .insert(&source.path, 2, "invalid", metadata.roots.clone())
            .is_err());
        assert_eq!(archive.list().unwrap().len(), 1);
        let encoded = serde_json::to_vec(&metadata).unwrap();
        archive
            .connection
            .execute(
                "UPDATE snapshots SET metadata=?1,checksum=?2 WHERE id=?3",
                params![encoded, Sha256::digest(&encoded).to_vec(), metadata.id],
            )
            .unwrap();
        assert!(archive.roots().is_err());
        assert!(archive.request_restore(&metadata.id).is_err());
    }

    #[test]
    fn old_snapshots_are_reported_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persistent-old.db");
        fs::write(&path, b"synthetic old format").unwrap();
        assert!(Archive::open(dir.path()).is_err());
        assert_eq!(fs::read(path).unwrap(), b"synthetic old format");
    }

    #[test]
    fn missing_reordered_and_truncated_manifests_are_rejected() {
        for mutation in [
            "PRAGMA foreign_keys=OFF; DELETE FROM chunks WHERE hash=(SELECT hash FROM snapshot_pages WHERE seq=1);",
            "UPDATE snapshot_pages SET seq=100 WHERE seq=1;",
            "DELETE FROM snapshot_pages WHERE seq=1;",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut archive = Archive::open(dir.path()).unwrap();
            let source = archive.scratch().unwrap();
            let bytes: Vec<u8> = (0..12000).map(|i| (i / 4096) as u8).collect();
            fs::write(&source.path, bytes).unwrap();
            let metadata = archive.insert(&source.path, 1, "source", Default::default()).unwrap();
            archive.connection.execute_batch(mutation).unwrap();
            let output = archive.scratch().unwrap();
            assert!(archive.restore(&metadata.id, &output.path).is_err(), "{mutation}");
        }
    }

    #[test]
    fn whole_file_hash_and_length_are_checked_even_with_valid_metadata_checksum() {
        for change_length in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut archive = Archive::open(dir.path()).unwrap();
            let source = archive.scratch().unwrap();
            fs::write(&source.path, vec![42; 8192]).unwrap();
            let mut metadata = archive
                .insert(&source.path, 1, "source", Default::default())
                .unwrap();
            if change_length {
                metadata.bytes += 4096;
            } else {
                metadata.file_hash[0] ^= 1;
            }
            let encoded = serde_json::to_vec(&metadata).unwrap();
            archive
                .connection
                .execute(
                    "UPDATE snapshots SET metadata=?1,checksum=?2 WHERE id=?3",
                    params![encoded, Sha256::digest(&encoded).to_vec(), metadata.id],
                )
                .unwrap();
            let output = archive.scratch().unwrap();
            assert!(archive.restore(&metadata.id, &output.path).is_err());
        }
    }

    #[test]
    fn failed_insert_rolls_back_references_and_pages_and_preserves_previous_restore() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(dir.path()).unwrap();
        let source = archive.scratch().unwrap();
        fs::write(&source.path, vec![31; 8192]).unwrap();
        let first = archive
            .insert(&source.path, 1, "first", Default::default())
            .unwrap();
        let count = archive
            .connection
            .query_row("SELECT count(*) FROM chunks", [], |r| r.get::<_, i64>(0))
            .unwrap();
        archive.connection.execute_batch("CREATE TRIGGER interrupt_capture BEFORE INSERT ON snapshot_pages WHEN NEW.seq=1 BEGIN SELECT RAISE(ABORT,'synthetic interrupted capture'); END;").unwrap();
        fs::write(&source.path, vec![73; 12000]).unwrap();
        assert!(archive
            .insert(&source.path, 2, "interrupted", Default::default())
            .is_err());
        assert_eq!(archive.list().unwrap().len(), 1);
        assert_eq!(
            archive
                .connection
                .query_row("SELECT count(*) FROM chunks", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            count
        );
        let output = archive.scratch().unwrap();
        archive.restore(&first.id, &output.path).unwrap();
        assert_eq!(fs::read(&output.path).unwrap(), vec![31; 8192]);
    }

    #[test]
    fn sqlite_full_during_capture_preserves_committed_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(dir.path()).unwrap();
        let source = archive.scratch().unwrap();
        fs::write(&source.path, vec![31; 8192]).unwrap();
        let first = archive
            .insert(&source.path, 1, "first", Default::default())
            .unwrap();
        let pages: i64 = archive
            .connection
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        archive
            .connection
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        let mut data = Vec::new();
        for index in 0..256_u64 {
            data.extend_from_slice(&[index.to_le_bytes(); 512].concat());
        }
        fs::write(&source.path, data).unwrap();
        assert!(archive
            .insert(&source.path, 2, "full", Default::default())
            .is_err());
        assert_eq!(archive.list().unwrap().len(), 1);
        let output = archive.scratch().unwrap();
        archive.restore(&first.id, &output.path).unwrap();
        assert_eq!(fs::read(output.path.clone()).unwrap(), vec![31; 8192]);
    }

    #[test]
    fn deletion_returns_file_space_and_counts_unique_reclaimable_pages() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::open(dir.path()).unwrap();
        let source = archive.scratch().unwrap();
        let mut data = Vec::new();
        for index in 0..256_u64 {
            data.extend_from_slice(&[index.to_le_bytes(); 512].concat());
        }
        fs::write(&source.path, &data).unwrap();
        let first = archive
            .insert(&source.path, 1, "large", Default::default())
            .unwrap();
        let large_bytes = archive.bytes().unwrap();
        assert_eq!(
            archive.list().unwrap()[0].reclaimable_bytes,
            data.len() as u64
        );
        archive.delete(&first.id).unwrap();
        assert!(archive.bytes().unwrap() < large_bytes / 4);
    }

    #[test]
    fn capture_cleanup_is_locked_and_only_removes_owned_names() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::open(dir.path()).unwrap();
        let path = archive.scratch().unwrap().path.clone();
        fs::write(&path, b"synthetic abandoned capture").unwrap();
        let unrelated = dir.path().join("capture-user-notes.txt");
        fs::write(&unrelated, b"preserve").unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let directory = dir.path().to_owned();
        let thread = std::thread::spawn(move || {
            let _second = Archive::open(&directory).unwrap();
            send.send(()).unwrap();
        });
        assert!(receive.recv_timeout(Duration::from_millis(30)).is_err());
        assert!(path.exists());
        drop(archive);
        receive.recv_timeout(Duration::from_secs(5)).unwrap();
        thread.join().unwrap();
        assert!(!path.exists());
        assert_eq!(fs::read(unrelated).unwrap(), b"preserve");
    }
}
