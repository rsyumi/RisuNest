//! Immutable library conflict references. Payload custody stays in Residency;
//! local bytes stay in the repository CAS, never beside the index.
use super::{directory, Side};
use crate::asset_repository::{
    job_pins::{CasJobKind, CasObjectRole, CasReleaseOutcome, DurableCasJob},
    PayloadCas,
};
use crate::server_sync::{
    cache::Cache, client::ServerClient, credentials::StoredConfig,
    residency::Residency, Result, SyncError,
};
use risunest_sync_wire::{canonical, Domain, RecordVersion, RemoteHead, MAX_METADATA_BYTES};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const PAGE: usize = 256;
const FORMAT: &str = "risunest-server-conflict-reference";

fn reference_parent(root: &Path, create: bool) -> Result<Option<PathBuf>> {
    let mut path = root.to_path_buf();
    for name in ["server-sync", "backups"] {
        path.push(name);
        if create {
            match std::fs::create_dir(&path) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(error.into()),
            }
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !crate::trust_boundary::is_link_like(&metadata) => (),
            Ok(_) => return Err(SyncError::new("invalid-backup-path", 409)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Some(path))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Receipt {
    pub format: String,
    pub version: u8,
    pub scope: String,
    pub id: String,
    pub created_at_ms: u64,
    pub head: RemoteHead,
    pub local_revision: i64,
    pub index_hash: String,
    pub index_bytes: u64,
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Local => "local",
        Side::Remote => "remote",
    }
}

#[derive(Debug)]
pub(crate) struct Object {
    pub hash: String,
    pub byte_size: Option<u64>,
    pub metadata: bool,
    pub context_id: Option<String>,
    pub local_required: bool,
}

pub(crate) struct CapturedRecord {
    pub side: Side,
    pub key: String,
    pub version: RecordVersion,
    pub body_hash: Option<String>,
    pub body_bytes: Option<u64>,
}

pub(crate) struct SourceRecord {
    pub key: String,
    pub version: RecordVersion,
    pub payload: Option<crate::persistent_store::server_sync_projection::ServerPayload>,
}

pub(crate) struct SideRequirements {
    pub local_required_bytes: u64,
    pub remote_dependent_bytes: u64,
    pub local_required_available: bool,
}

pub(crate) struct SourceObject {
    pub hash: String,
    pub byte_size: u64,
    pub metadata: bool,
    pub context_id: Option<String>,
    pub local_required: bool,
}

pub(crate) struct Capture {
    pub id: String,
    pub path: PathBuf,
    db: Connection,
    index_file: File,
    pins: DurableCasJob,
    cas: PayloadCas,
    head: RemoteHead,
    revision: i64,
    created_at_ms: u64,
    resuming: bool,
}

impl Capture {
    pub(crate) fn begin_or_resume(root: &Path, revision: i64, generation: &str, head: &RemoteHead) -> Result<Self> {
        if let Some(parent) = reference_parent(root, false)? {
            for entry in std::fs::read_dir(parent)? {
                let entry = entry?;
                let Some(id) = entry.file_name().to_str().map(str::to_owned) else { continue; };
                if entry.path().join("complete.json").try_exists()? { continue; }
                if !entry.path().join("index.sqlite").try_exists()? { continue; }
                if let Ok(capture) = Self::resume(root, &id, revision, generation, head) {
                    return Ok(capture);
                }
                // A mismatched or damaged preparation remains protected,
                // but cannot become a snapshot of a different revision.
            }
        }
        Self::begin(root, revision, generation, head)
    }

    pub(crate) fn begin(root: &Path, revision: i64, generation: &str, head: &RemoteHead) -> Result<Self> {
        head.validate()?;
        if revision < 0 || generation.is_empty() {
            return Err(SyncError::new("invalid-conflict-revision", 409));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let parent = reference_parent(root, true)?
            .ok_or_else(|| SyncError::new("invalid-backup-path", 409))?;
        std::fs::create_dir(parent.join(&id))?;
        let path = directory(root, &id)?;
        let created_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|_| SyncError::new("invalid-conflict-time", 409))?
            .as_millis().try_into().map_err(|_| SyncError::new("invalid-conflict-time", 409))?;
        let pins = DurableCasJob::begin(root, &id,
            CasJobKind::OfficialPublicationOrExportPreparation,
            i64::try_from(created_at_ms).map_err(|_| SyncError::new("invalid-conflict-time", 409))?)?;
        let index_file = std::fs::OpenOptions::new().create_new(true).read(true).write(true)
            .open(path.join("index.sqlite"))?;
        let db = Connection::open(path.join("index.sqlite"))?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
            CREATE TABLE records(
                side TEXT NOT NULL CHECK(side IN ('local','remote')),
                domain TEXT NOT NULL CHECK(domain='library'),
                record_key TEXT NOT NULL, version_json TEXT NOT NULL,
                body_hash TEXT, body_bytes INTEGER CHECK(body_bytes IS NULL OR body_bytes>=0),
                PRIMARY KEY(side,domain,record_key));
            CREATE TABLE objects(
                side TEXT NOT NULL CHECK(side IN ('local','remote')),
                hash TEXT NOT NULL, byte_size INTEGER CHECK(byte_size IS NULL OR byte_size>=0),
                role TEXT NOT NULL CHECK(role IN ('metadata','payload')), context_id TEXT,
                local_required INTEGER NOT NULL CHECK(local_required IN (0,1)),
                PRIMARY KEY(side,hash));
            CREATE INDEX objects_by_hash ON objects(hash);
            CREATE TABLE capture_identity(id TEXT PRIMARY KEY, head_json TEXT NOT NULL,
                local_revision INTEGER NOT NULL, created_at_ms INTEGER NOT NULL, local_generation TEXT NOT NULL,
                local_complete INTEGER NOT NULL CHECK(local_complete IN (0,1)),
                remote_complete INTEGER NOT NULL CHECK(remote_complete IN (0,1)));
            PRAGMA user_version=1;")?;
        db.execute("INSERT INTO capture_identity VALUES(?1,?2,?3,?4,?5,0,0)", params![
            id, String::from_utf8(canonical::encode(head)?).map_err(|_| SyncError::new("conflict-encoding", 409))?,
            revision, created_at_ms as i64, generation])?;
        crate::trust_boundary::sync_directory(&path)?;
        crate::trust_boundary::sync_directory(&parent)?;
        Ok(Self { id, path, db, index_file, pins, cas: PayloadCas::new(root)?, head: head.clone(), revision, created_at_ms, resuming: false })
    }

    pub(crate) fn resume(root: &Path, id: &str, revision: i64, generation: &str, head: &RemoteHead) -> Result<Self> {
        head.validate()?;
        let path = directory(root, id)?;
        match std::fs::symlink_metadata(path.join("complete.json")) {
            Ok(_) => return Err(SyncError::new("conflict-already-complete", 409)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let previous = index(&path)?;
        let (stored_id, stored_head, stored_revision, created_at_ms, stored_generation): (String, String, i64, i64, String) =
            previous.query_row("SELECT id,head_json,local_revision,created_at_ms,local_generation FROM capture_identity", [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        if stored_id != id || stored_head.as_bytes() != canonical::encode(head)? || stored_revision != revision
            || revision < 0 || created_at_ms < 0 || generation.is_empty() || stored_generation != generation {
            return Err(SyncError::new("conflict-repreparation-required", 409));
        }
        drop(previous);
        let original = crate::trust_boundary::open_regular_source(&path.join("index.sqlite"))?;
        let index_file = std::fs::OpenOptions::new().read(true).write(true).open(path.join("index.sqlite"))?;
        if crate::asset_repository::exact_file_identity(&original)? != crate::asset_repository::exact_file_identity(&index_file)? {
            return Err(SyncError::new("invalid-backup-path", 409));
        }
        let db = Connection::open_with_flags(path.join("index.sqlite"), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        let pins = DurableCasJob::open(root, id)?;
        if pins.is_released() { return Err(SyncError::new("conflict-repreparation-required", 409)); }
        Ok(Self { id: id.into(), path, db, index_file, pins, cas: PayloadCas::new(root)?,
            head: head.clone(), revision, created_at_ms: created_at_ms as u64, resuming: true })
    }

    pub(crate) fn side_complete(&self, side: Side) -> Result<bool> {
        Ok(self.db.query_row("SELECT CASE ?1 WHEN 'local' THEN local_complete ELSE remote_complete END
            FROM capture_identity WHERE id=?2", params![side_name(side), self.id], |r| r.get(0))?)
    }

    pub(crate) fn complete_side(&self, side: Side) -> Result<()> {
        self.db.execute("UPDATE capture_identity SET
            local_complete=CASE WHEN ?1='local' THEN 1 ELSE local_complete END,
            remote_complete=CASE WHEN ?1='remote' THEN 1 ELSE remote_complete END WHERE id=?2",
            params![side_name(side), self.id])?;
        Ok(())
    }

    pub(crate) fn has_record(&self, side: Side, key: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM records WHERE side=?1 AND record_key=?2)",
            params![side_name(side), key], |r| r.get(0))?)
    }

    pub(crate) fn visit_records(&self, mut visit: impl FnMut(CapturedRecord) -> Result<()>) -> Result<()> {
        let mut after = (String::new(), String::new());
        loop {
            let mut statement = self.db.prepare("SELECT side,record_key,version_json,body_hash,body_bytes
                FROM records WHERE (side,record_key)>(?1,?2) ORDER BY side,record_key LIMIT 256")?;
            let page = statement.query_map(params![after.0, after.1], |row|
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?, row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if page.is_empty() { return Ok(()); }
            for (side, key, version, body_hash, body_bytes) in page {
                after = (side.clone(), key.clone());
                let side = match side.as_str() {
                    "local" => Side::Local,
                    "remote" => Side::Remote,
                    _ => return Err(SyncError::new("invalid-conflict-side", 409)),
                };
                let version = serde_json::from_str(&version)
                    .map_err(|_| SyncError::new("invalid-conflict-version", 409))?;
                visit(CapturedRecord { side, key, version, body_hash,
                    body_bytes: unsigned_size(body_bytes)? })?;
            }
        }
    }

    pub(crate) fn confirms(&self, residency: &Residency, side: Side, hash: &str, size: Option<u64>) -> Result<bool> {
        use rusqlite::OptionalExtension;
        let row: Option<(Option<String>, i64)> = self.db.query_row("SELECT context_id,byte_size FROM objects
            WHERE side=?1 AND hash=?2", params![side_name(side), hash], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((Some(context), stored_size)) = row else { return Ok(false); };
        let stored_size = unsigned_size(Some(stored_size))?.unwrap();
        if size.is_some_and(|size| size != stored_size) { return Ok(false); }
        residency.confirms(hash, Some(stored_size), &context)
    }

    pub(crate) fn object(&self, side: Side, object: &Object) -> Result<()> {
        risunest_sync_wire::validate_hash(&object.hash)?;
        if let Some(context) = &object.context_id {
            risunest_sync_wire::validate_hash(context)?;
        }
        let size = object.byte_size.map(i64::try_from).transpose()
            .map_err(|_| SyncError::new("invalid-conflict-object-size", 409))?;
        let conflicting: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE hash=?1 AND byte_size IS NOT NULL
                AND ?2 IS NOT NULL AND byte_size!=?2)", params![object.hash, size], |r| r.get(0))?;
        if conflicting {
            return Err(SyncError::new("conflict-object-size-mismatch", 409));
        }
        let conflicting_context: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE side=?1 AND hash=?2 AND context_id IS NOT NULL
                AND ?3 IS NOT NULL AND context_id!=?3)",
            params![side_name(side), object.hash, object.context_id], |r| r.get(0))?;
        if conflicting_context {
            return Err(SyncError::new("conflict-object-context-mismatch", 409));
        }
        self.db.execute("INSERT INTO objects VALUES(?1,?2,?3,?4,?5,?6)
            ON CONFLICT(side,hash) DO UPDATE SET
                byte_size=coalesce(objects.byte_size,excluded.byte_size),
                role=CASE WHEN objects.role='metadata' OR excluded.role='metadata' THEN 'metadata' ELSE 'payload' END,
                context_id=coalesce(objects.context_id,excluded.context_id),
                local_required=max(objects.local_required,excluded.local_required)",
            params![side_name(side), object.hash, size,
                if object.metadata { "metadata" } else { "payload" }, object.context_id,
                object.local_required || object.metadata])?;
        if let Some(size) = size {
            self.db.execute("UPDATE objects SET byte_size=?2 WHERE hash=?1 AND byte_size IS NULL",
                params![object.hash, size])?;
        }
        Ok(())
    }

    pub(crate) fn metadata(&mut self, side: Side, bytes: &[u8]) -> Result<String> {
        let prepared = self.pins.prepare_bytes(&self.cas, bytes, CasObjectRole::DirectObject)?;
        self.object(side, &Object { hash: prepared.content_hash.clone(), byte_size: Some(prepared.byte_size),
            metadata: true, context_id: None, local_required: true })?;
        Ok(prepared.content_hash)
    }

    pub(crate) fn cached_metadata(&mut self, side: Side, cache: &Cache, hash: &str) -> Result<()> {
        let mut file = cache.cas.open_object(hash)?.ok_or_else(|| SyncError::new("conflict-metadata-missing", 409))?;
        let size = file.metadata()?.len();
        self.pins.prepare_reader_expected(&self.cas, &mut file, hash, size, CasObjectRole::DirectObject)?;
        self.object(side, &Object { hash: hash.into(), byte_size: Some(size), metadata: true,
            context_id: None, local_required: true })
    }

    pub(crate) fn local_payload(&mut self, hash: &str, size: u64, check: &impl Fn() -> Result<()>) -> Result<()> {
        check()?;
        self.pins.pin_existing(&self.cas, hash, size, CasObjectRole::DirectObject)?;
        check()?;
        self.object(Side::Local, &Object { hash: hash.into(), byte_size: Some(size), metadata: false,
            context_id: None, local_required: true })
    }

    pub(crate) fn record(&self, side: Side, key: &str, version: &RecordVersion, body: Option<(&str, u64)>) -> Result<()> {
        version.validate()?;
        crate::logical_records::decode_logical_record_key(key)
            .map_err(|_| SyncError::new("invalid-conflict-record-key", 409))?;
        if matches!(version, RecordVersion::Live { .. }) != body.is_some() {
            return Err(SyncError::new("invalid-conflict-record-body", 409));
        }
        if let Some((hash, size)) = body {
            let present: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM objects WHERE side=?1
                AND hash=?2 AND byte_size=?3 AND role='metadata' AND local_required=1)",
                params![side_name(side), hash, i64::try_from(size).map_err(|_| SyncError::new("invalid-conflict-object-size", 409))?], |r| r.get(0))?;
            if !present {
                return Err(SyncError::new("conflict-record-metadata-missing", 409));
            }
        }
        let version_json = String::from_utf8(canonical::encode(version)?)
            .map_err(|_| SyncError::new("conflict-encoding", 409))?;
        if self.resuming && self.has_record(side, key)? {
            let same: bool = self.db.query_row("SELECT domain='library' AND version_json=?3
                AND body_hash IS ?4 AND body_bytes IS ?5 FROM records WHERE side=?1 AND record_key=?2",
                params![side_name(side), key, version_json, body.map(|(hash, _)| hash),
                    body.map(|(_, size)| size as i64)], |r| r.get(0))?;
            if !same { return Err(SyncError::new("conflict-repreparation-required", 409)); }
            return Ok(());
        }
        self.db.execute("INSERT INTO records VALUES(?1,'library',?2,?3,?4,?5)", params![
            side_name(side), key, version_json, body.map(|(hash, _)| hash), body.map(|(_, size)| size as i64)])?;
        Ok(())
    }

    pub(crate) fn retain(&self, client: &ServerClient, config: &StoredConfig, residency: &mut Residency) -> Result<()> {
        let context = Residency::context_id(config, &self.head.epoch);
        let mut after = String::new();
        loop {
            client.ensure_active()?;
            let page = {
                let mut stmt = self.db.prepare("SELECT hash,max(byte_size) FROM objects
                    WHERE context_id=?1 AND hash>?2 GROUP BY hash ORDER BY hash LIMIT 256")?;
                let rows = stmt.query_map(params![context, after], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            };
            if page.is_empty() { break; }
            let page = page.into_iter().map(|(hash, size)| Ok((hash, unsigned_size(size)?)))
                .collect::<Result<Vec<_>>>()?;
            residency.retain(client, config, &self.head, &page)?;
            for (hash, expected) in &page {
                let object = residency.object(hash, Some(&context))?
                    .ok_or_else(|| SyncError::new("conflict-custody-unconfirmed", 409))?;
                if expected.is_some_and(|size| size != object.size) {
                    return Err(SyncError::new("conflict-object-size-mismatch", 409));
                }
                self.db.execute("UPDATE objects SET byte_size=?2 WHERE hash=?1 AND byte_size IS NULL",
                    params![hash, object.size as i64])?;
            }
            after = page.last().unwrap().0.clone();
        }
        Ok(())
    }

    pub(crate) fn finish(mut self, store: &mut crate::persistent_store::PersistentStore,
        check: &impl Fn() -> Result<()>) -> Result<Receipt> {
        check()?;
        if !self.side_complete(Side::Local)? || !self.side_complete(Side::Remote)? {
            return Err(SyncError::new("conflict-preservation-incomplete", 409));
        }
        let incomplete: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM objects WHERE byte_size IS NULL
            OR (local_required=0 AND context_id IS NULL))", [], |r| r.get(0))?;
        if incomplete { return Err(SyncError::new("conflict-preservation-incomplete", 409)); }
        let residency = Residency::open(store.repository_root())?;
        visit_objects(&self.db, |object| {
            check()?;
            if let Some(context) = &object.context_id {
                if !residency.confirms(&object.hash, object.byte_size, context)? {
                    return Err(SyncError::new("conflict-custody-unconfirmed", 409));
                }
            }
            if object.metadata || object.local_required {
                let file = self.cas.open_object(&object.hash)?
                    .ok_or_else(|| SyncError::new("conflict-local-object-missing", 409))?;
                verify_file(file, Some((&object.hash, object.byte_size.unwrap())), check)?;
            }
            Ok(())
        })?;
        self.pins.seal(store, self.created_at_ms as i64)?;
        // The sealed job remains the owner until the complete reference root is
        // durably published. Failure and cancellation must leave its pins active.
        self.db.close().map_err(|(_, error)| error)?;
        let index = self.path.join("index.sqlite");
        // Flush the original writable handle. FlushFileBuffers rejects a
        // read-only Windows handle even after the SQLite connection has closed.
        self.index_file.sync_all().map_err(|_| SyncError::new("conflict-index-sync-failed", 503))?;
        let file = crate::trust_boundary::open_regular_source(&index)?;
        let identity = crate::asset_repository::exact_file_identity(&self.index_file)?;
        if crate::asset_repository::exact_file_identity(&file)? != identity {
            return Err(SyncError::new("invalid-backup-path", 409));
        }
        let (index_hash, index_bytes) = verify_file(file, None, check)?;
        if crate::asset_repository::exact_file_identity(&self.index_file)? != identity {
            return Err(SyncError::new("conflict-index-changed", 409));
        }
        drop(self.index_file);
        let receipt = Receipt { format: FORMAT.into(), version: 1, scope: "library".into(),
            id: self.id, created_at_ms: self.created_at_ms, head: self.head, local_revision: self.revision,
            index_hash, index_bytes };
        let bytes = serde_json::to_vec(&receipt).map_err(|_| SyncError::new("conflict-receipt-encoding", 409))?;
        let mut marker = tempfile::Builder::new().prefix("complete-").tempfile_in(&self.path)?;
        marker.write_all(&bytes)?;
        marker.as_file().sync_all()?;
        check()?;
        // Close the writable handle before publication. Android forbids hard
        // links, and Windows volumes need not support them.
        let marker = marker.into_temp_path();
        let destination = self.path.join("complete.json");
        #[cfg(any(target_os = "android", windows))]
        crate::trust_boundary::rename_without_replace(&marker, &destination)?;
        #[cfg(not(any(target_os = "android", windows)))]
        std::fs::hard_link(&marker, &destination)?;
        drop(marker);
        crate::trust_boundary::sync_directory(&self.path)?;
        // The complete reference root owns these objects now. A failed journal
        // cleanup is a safe leak recovered by normal durable-job maintenance.
        let _ = self.pins.release(CasReleaseOutcome::Committed);
        Ok(receipt)
    }
}

fn verify_file(mut file: File, expected: Option<(&str, u64)>, check: &impl Fn() -> Result<()>) -> Result<(String, u64)> {
    let mut hash = Sha256::new();
    let mut length = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 { break; }
        hash.update(&buffer[..count]);
        length += count as u64;
    }
    let hash = format!("{:x}", hash.finalize());
    if expected.is_some_and(|(digest, bytes)| digest != hash || bytes != length) {
        return Err(SyncError::new("conflict-object-hash-mismatch", 409));
    }
    Ok((hash, length))
}

fn index(path: &Path) -> Result<Connection> {
    crate::trust_boundary::open_regular_source(&path.join("index.sqlite"))?;
    for suffix in ["-wal", "-shm", "-journal"] {
        match std::fs::symlink_metadata(path.join(format!("index.sqlite{suffix}"))) {
            Ok(metadata) if !metadata.is_file() || crate::trust_boundary::is_link_like(&metadata) =>
                return Err(SyncError::new("invalid-backup-path", 409)),
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    let db = Connection::open_with_flags(path.join("index.sqlite"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != 1 { return Err(SyncError::new("invalid-conflict-index", 409)); }
    Ok(db)
}

pub(crate) fn inspect(root: &Path, id: &str) -> Result<Receipt> {
    let path = directory(root, id)?;
    let mut bytes = Vec::new();
    crate::trust_boundary::open_regular_source(&path.join("complete.json"))?.take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 { return Err(SyncError::new("invalid-conflict-receipt", 409)); }
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|_| SyncError::new("invalid-conflict-receipt", 409))?;
    receipt.head.validate()?;
    risunest_sync_wire::validate_hash(&receipt.index_hash)?;
    if receipt.format != FORMAT || receipt.version != 1 || receipt.scope != "library"
        || receipt.id != id || receipt.local_revision < 0 {
        return Err(SyncError::new("invalid-conflict-receipt", 409));
    }
    if crate::trust_boundary::open_regular_source(&path.join("index.sqlite"))?.metadata()?.len() != receipt.index_bytes {
        return Err(SyncError::new("conflict-index-size-mismatch", 409));
    }
    Ok(receipt)
}

pub(crate) fn open(root: &Path, id: &str, check: &impl Fn() -> Result<()>) -> Result<Connection> {
    let receipt = inspect(root, id)?;
    let path = directory(root, id)?;
    verify_file(crate::trust_boundary::open_regular_source(&path.join("index.sqlite"))?,
        Some((&receipt.index_hash, receipt.index_bytes)), check)?;
    let db = index(&path)?;
    let identity: (String, String, i64, i64) = db.query_row("SELECT id,head_json,local_revision,created_at_ms FROM capture_identity", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    if identity.0 != id || identity.1.as_bytes() != canonical::encode(&receipt.head)?
        || identity.2 != receipt.local_revision || u64::try_from(identity.3).ok() != Some(receipt.created_at_ms) {
        return Err(SyncError::new("conflict-index-identity-mismatch", 409));
    }
    Ok(db)
}

pub(crate) fn open_for_inspection(root: &Path, id: &str) -> Result<Connection> {
    inspect(root, id)?;
    index(&directory(root, id)?)
}

fn unsigned_size(size: Option<i64>) -> Result<Option<u64>> {
    size.map(u64::try_from).transpose()
        .map_err(|_| SyncError::new("invalid-conflict-object-size", 409))
}

pub(crate) fn visit_objects(db: &Connection, mut visit: impl FnMut(Object) -> Result<()>) -> Result<()> {
    let mut after = (String::new(), String::new());
    loop {
        let mut stmt = db.prepare("SELECT hash,max(byte_size),max(role='metadata'),context_id,max(local_required)
            FROM objects WHERE (hash,coalesce(context_id,''))>(?1,?2)
            GROUP BY hash,context_id ORDER BY hash,coalesce(context_id,'') LIMIT 256")?;
        let page = stmt.query_map(params![after.0, after.1], |r| Ok((r.get::<_, String>(0)?,
            r.get::<_, Option<i64>>(1)?, r.get::<_, bool>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, bool>(4)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if page.is_empty() { break; }
        for (hash, size, metadata, context_id, local_required) in page {
            risunest_sync_wire::validate_hash(&hash)?;
            if let Some(context) = &context_id { risunest_sync_wire::validate_hash(context)?; }
            after = (hash.clone(), context_id.clone().unwrap_or_default());
            visit(Object { hash, byte_size: unsigned_size(size)?, metadata, context_id, local_required })?;
        }
    }
    Ok(())
}

pub(crate) fn visit_side_objects(
    db: &Connection,
    side: Side,
    mut visit: impl FnMut(Object) -> Result<()>,
) -> Result<()> {
    let mut after = String::new();
    loop {
        let mut statement = db.prepare("SELECT hash,byte_size,role='metadata',context_id,local_required
            FROM objects WHERE side=?1 AND hash>?2 ORDER BY hash LIMIT 256")?;
        let page = statement.query_map(params![side_name(side), after], |r| Ok((
            r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, bool>(2)?,
            r.get::<_, Option<String>>(3)?, r.get::<_, bool>(4)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if page.is_empty() { break; }
        for (hash, size, metadata, context_id, local_required) in page {
            risunest_sync_wire::validate_hash(&hash)?;
            if let Some(context) = &context_id { risunest_sync_wire::validate_hash(context)?; }
            after = hash.clone();
            visit(Object { hash, byte_size: unsigned_size(size)?, metadata, context_id, local_required })?;
        }
    }
    Ok(())
}

pub(crate) fn visit_source_objects(
    root: &Path,
    db: &Connection,
    side: Side,
    check: &impl Fn() -> Result<()>,
    mut visit: impl FnMut(SourceObject) -> Result<()>,
) -> Result<()> {
    let cas = PayloadCas::new(root)?;
    visit_side_objects(db, side, |object| {
        check()?;
        let byte_size = object.byte_size
            .ok_or_else(|| SyncError::new("invalid-conflict-object-size", 409))?;
        if object.metadata || object.local_required {
            let file = cas.open_object(&object.hash)?
                .ok_or_else(|| SyncError::new("conflict-local-object-missing", 409))?;
            verify_file(file, Some((&object.hash, byte_size)), check)?;
        }
        visit(SourceObject {
            hash: object.hash,
            byte_size,
            metadata: object.metadata,
            context_id: object.context_id,
            local_required: object.local_required,
        })
    })
}

pub(crate) fn side_requirements(root: &Path, db: &Connection, side: Side) -> Result<SideRequirements> {
    let cas = PayloadCas::new(root)?;
    let (local, remote, invalid): (i64, i64, bool) = db.query_row(
        "SELECT coalesce(sum(CASE WHEN role='metadata' OR local_required=1 THEN byte_size ELSE 0 END),0),
            coalesce(sum(CASE WHEN role='payload' AND local_required=0 THEN byte_size ELSE 0 END),0),
            EXISTS(SELECT 1 FROM objects WHERE side=?1 AND (byte_size IS NULL
                OR (role='payload' AND local_required=0 AND context_id IS NULL)))
            FROM objects WHERE side=?1", params![side_name(side)], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
    if invalid {
        return Err(SyncError::new("invalid-conflict-object", 409));
    }
    let mut result = SideRequirements {
        local_required_bytes: u64::try_from(local)
            .map_err(|_| SyncError::new("storage-size-overflow", 409))?,
        remote_dependent_bytes: u64::try_from(remote)
            .map_err(|_| SyncError::new("storage-size-overflow", 409))?,
        local_required_available: true,
    };
    let mut after = String::new();
    loop {
        let mut statement = db.prepare("SELECT hash,byte_size FROM objects WHERE side=?1
            AND (role='metadata' OR local_required=1) AND hash>?2 ORDER BY hash LIMIT 256")?;
        let page = statement.query_map(params![side_name(side), after], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?.collect::<std::result::Result<Vec<_>, _>>()?;
        if page.is_empty() { break; }
        for (hash, size) in page {
            risunest_sync_wire::validate_hash(&hash)?;
            let size = u64::try_from(size)
                .map_err(|_| SyncError::new("invalid-conflict-object-size", 409))?;
            after = hash.clone();
            result.local_required_available &= cas.stat_object(&hash).ok().flatten() == Some(size);
        }
    }
    Ok(result)
}

pub(crate) fn visit_source_records(
    root: &Path,
    db: &Connection,
    side: Side,
    check: &impl Fn() -> Result<()>,
    mut visit: impl FnMut(SourceRecord) -> Result<()>,
) -> Result<()> {
    let cas = PayloadCas::new(root)?;
    let mut after = String::new();
    loop {
        check()?;
        let mut statement = db.prepare("SELECT record_key,version_json,body_hash,body_bytes
            FROM records WHERE side=?1 AND record_key>?2 ORDER BY record_key LIMIT 256")?;
        let page = statement.query_map(params![side_name(side), after], |r| Ok((
            r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<i64>>(3)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
        if page.is_empty() { break; }
        for (key, encoded_version, body_hash, body_bytes) in page {
            check()?;
            crate::logical_records::decode_logical_record_key(&key)
                .map_err(|_| SyncError::new("invalid-conflict-record-key", 409))?;
            let version: RecordVersion = serde_json::from_str(&encoded_version)
                .map_err(|_| SyncError::new("invalid-conflict-version", 409))?;
            version.validate()?;
            let body_bytes = unsigned_size(body_bytes)?;
            if matches!(version, RecordVersion::Live { .. }) != body_hash.is_some()
                || body_hash.is_some() != body_bytes.is_some() {
                return Err(SyncError::new("invalid-conflict-record-body", 409));
            }
            let payload = match (body_hash, body_bytes) {
                (Some(hash), Some(size)) => {
                    risunest_sync_wire::validate_hash(&hash)?;
                    let file = cas.open_object(&hash)?
                        .ok_or_else(|| SyncError::new("conflict-record-metadata-missing", 409))?;
                    let bytes = read_verified_file(file, &hash, size, check)?;
                    let payload = serde_json::from_slice(&bytes)
                        .map_err(|_| SyncError::new("invalid-server-payload", 409))?;
                    if serde_json::to_vec(&payload)
                        .map_err(|_| SyncError::new("invalid-server-payload", 409))? != bytes {
                        return Err(SyncError::new("noncanonical-server-payload", 409));
                    }
                    Some(payload)
                }
                (None, None) => None,
                _ => return Err(SyncError::new("invalid-conflict-record-body", 409)),
            };
            after = key.clone();
            visit(SourceRecord { key, version, payload })?;
        }
    }
    Ok(())
}

fn read_verified_file(
    mut file: File,
    expected_hash: &str,
    expected_bytes: u64,
    check: &impl Fn() -> Result<()>,
) -> Result<Vec<u8>> {
    if expected_bytes > MAX_METADATA_BYTES as u64 {
        return Err(SyncError::new("conflict-record-too-large", 409));
    }
    let capacity = usize::try_from(expected_bytes)
        .map_err(|_| SyncError::new("invalid-conflict-object-size", 409))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity)
        .map_err(|_| SyncError::new("conflict-record-too-large", 409))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    while bytes.len() as u64 <= expected_bytes {
        check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 { break; }
        hash.update(&buffer[..count]);
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() as u64 > expected_bytes { break; }
    }
    if bytes.len() as u64 != expected_bytes || format!("{:x}", hash.finalize()) != expected_hash {
        return Err(SyncError::new("conflict-object-hash-mismatch", 409));
    }
    Ok(bytes)
}

pub(crate) fn visit_roots(root: &Path, mut visit: impl FnMut(Object) -> Result<()>) -> Result<()> {
    let Some(parent) = reference_parent(root, false)? else { return Ok(()); };
    let entries = std::fs::read_dir(parent)?;
    for entry in entries {
        let entry = entry?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else { continue; };
        // Other backup kinds have no reference index. Never use receipt presence
        // as a liveness gate: interrupted captures own their known references.
        match std::fs::symlink_metadata(entry.path().join("index.sqlite")) {
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
        let path = directory(root, &id)?;
        visit_objects(&index(&path)?, &mut visit)?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Checkpoint { checkpoint_id: String, head: RemoteHead, domains: Vec<Domain> }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CheckpointPage { checkpoint: Checkpoint, records: Vec<RemoteRecord>, next: Option<Cursor> }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor { domain: Domain, key: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteRecord { pub domain: Domain, pub key: String, pub version: RecordVersion }

pub(crate) struct RemoteRead<'a> {
    client: &'a ServerClient,
    checkpoint: Checkpoint,
}
impl<'a> RemoteRead<'a> {
    pub(crate) fn begin(client: &'a ServerClient, head: &RemoteHead) -> Result<Self> {
        let (_, checkpoint): (_, Checkpoint) = client.json(reqwest::Method::POST, "checkpoints", &[],
            Some(&serde_json::json!({"domains":[Domain::Library]})), &[])?;
        risunest_sync_wire::validate_id(&checkpoint.checkpoint_id)?;
        checkpoint.head.validate()?;
        if checkpoint.head != *head || checkpoint.domains != [Domain::Library] {
            return Err(SyncError::new("conflict-preview-stale", 409));
        }
        Ok(Self { client, checkpoint })
    }
    pub(crate) fn visit(&self, mut visit: impl FnMut(RemoteRecord) -> Result<()>) -> Result<()> {
        let mut after: Option<Cursor> = None;
        loop {
            let mut query = vec![("limit", PAGE.to_string())];
            if let Some(cursor) = &after {
                query.push(("afterDomain", cursor.domain.as_str().into()));
                query.push(("afterKey", cursor.key.clone()));
            }
            let (_, page): (_, CheckpointPage) = self.client.json(reqwest::Method::GET,
                &format!("checkpoints/{}", self.checkpoint.checkpoint_id), &query, None::<&()>, &[])?;
            if page.checkpoint.checkpoint_id != self.checkpoint.checkpoint_id
                || page.checkpoint.head != self.checkpoint.head || page.checkpoint.domains != [Domain::Library]
                || page.records.len() > PAGE {
                return Err(SyncError::new("checkpoint-identity-mismatch", 502));
            }
            let mut previous = after.as_ref().map(|cursor| cursor.key.as_str());
            for record in &page.records {
                record.version.validate()?;
                if record.domain != Domain::Library || previous.is_some_and(|key| key >= record.key.as_str()) {
                    return Err(SyncError::new("unordered-checkpoint", 502));
                }
                previous = Some(&record.key);
            }
            if page.next.as_ref().is_some_and(|next| next.domain != Domain::Library
                || page.records.last().map(|record| record.key.as_str()) != Some(next.key.as_str())) {
                return Err(SyncError::new("invalid-checkpoint-cursor", 502));
            }
            for record in page.records { self.client.ensure_active()?; visit(record)?; }
            after = page.next;
            if after.is_none() { return Ok(()); }
        }
    }
    pub(crate) fn release(self) -> Result<()> {
        let reply = self.client.request(reqwest::Method::DELETE,
            &format!("checkpoints/{}", self.checkpoint.checkpoint_id), &[], None, &[], MAX_METADATA_BYTES)?;
        if reply.status != 204 { return Err(crate::server_sync::client::response_error(reply)); }
        Ok(())
    }
}

#[cfg(test)]
#[path = "backup_references_tests.rs"]
mod tests;
