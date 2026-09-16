mod backup;
mod commits;
mod connection;
pub(crate) mod management;
pub use backup::BackupManifest;
pub use management::{ManagedDevice, ManagementConnection};
mod jobs;
pub use jobs::CommitSubmission;
mod checkpoints;
mod descriptors;
mod journal;
mod maintenance;
mod media;
pub use media::MediaResponse;
mod retention;
pub use retention::{ObjectIdentity, RetainedObject, RetentionPage, RetentionRelease};
mod objects;
mod schema;
mod scopes;
mod staged;
mod stream_transfers;
pub use stream_transfers::DeltaProgress;
mod transfers;
mod upload_jobs;
mod uploads;
pub use checkpoints::{Checkpoint, CheckpointCursor, CheckpointPage, ReadPin};
pub use journal::{ChangeCursor, ChangePage, JournalChange};
pub use staged::StagedChanges;
pub use transfers::TransferRequest;
pub use uploads::{UploadManifest, UploadProgress, UPLOAD_CHUNK_BYTES};

use crate::{Error, Result};
use risunest_sync_wire::{canonical, hash, validate_id, Domain, RemoteHead, Sequence};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

pub struct Store {
    root: PathBuf,
    db: Mutex<Connection>,
    read_db: Mutex<Connection>,
    objects_gate: Mutex<()>,
    upload_job_gate: Mutex<()>,
    download_job_gate: Mutex<()>,
    connection_gate: Mutex<()>,
    media_signer: risunest_sync_connect::media::MediaSigner,
    heads: tokio::sync::watch::Sender<u64>,
    _owner: File,
}

/// Tokens are emitted once by the administration CLI, never by a sync route.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCredential {
    pub device_id: String,
    pub library_id: String,
    pub token: String,
}

#[derive(Clone, Debug)]
pub struct Device {
    pub id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSession {
    pub head: RemoteHead,
    pub device_id: String,
    pub operation_watermark: Sequence,
    pub operation_pending: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableWork {
    pub commit_jobs: u64,
    pub upload_jobs: u64,
    pub download_jobs: u64,
    pub staged_changes: u64,
    pub uploads: u64,
}

pub(super) fn random_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| Error::new("entropy-unavailable", 503))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub(super) fn json<T: Serialize>(value: &T) -> Result<String> {
    Ok(String::from_utf8(canonical::encode(value)?).unwrap())
}
pub(super) fn parse<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T> {
    canonical::decode(value.as_bytes(), risunest_sync_wire::MAX_METADATA_BYTES)
        .map_err(|_| Error::new("corrupt-metadata", 503))
}
/// Requested sections, deduplicated. An empty request is never an implicit all.
pub(super) fn requested_domains(domains: &[Domain]) -> Result<Vec<Domain>> {
    if domains.is_empty() {
        return Err(Error::new("invalid-domains", 400));
    }
    Ok(domains
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}
/// Literal list for an IN clause. Values come from a closed enum, never input text.
pub(super) fn domain_filter(domains: &[Domain]) -> String {
    domains
        .iter()
        .map(|domain| format!("'{}'", domain.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}

impl Store {
    pub fn durable_work(&self) -> Result<DurableWork> {
        let db = self.reader()?;
        let count = |table: &str| -> Result<u64> {
            let value: i64 = db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })?;
            u64::try_from(value).map_err(|_| Error::new("corrupt-metadata", 503))
        };
        Ok(DurableWork {
            commit_jobs: count("commit_jobs")?,
            upload_jobs: count("upload_jobs")?,
            download_jobs: count("download_deltas")?,
            staged_changes: count("staged_changes")?,
            uploads: count("uploads")?,
        })
    }

    pub fn init(root: &Path) -> Result<Self> {
        Self::open_inner(root, true)
    }
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_inner(root, false)
    }
    fn open_inner(root: &Path, create: bool) -> Result<Self> {
        if !root.is_absolute() {
            return Err(Error::new("absolute-data-dir-required", 400));
        }
        objects::check_path(root)?;
        if create {
            fs::create_dir_all(root)?;
        }
        let root = fs::canonicalize(root)?;
        for name in [
            "owner.lock",
            "metadata.sqlite",
            "metadata.sqlite-wal",
            "metadata.sqlite-shm",
            "objects",
            "staging",
        ] {
            objects::check_path(&root.join(name))?;
        }
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("owner.lock"))?;
        owner
            .try_lock()
            .map_err(|_| Error::new("data-dir-busy", 409))?;
        let db_path = root.join("metadata.sqlite");
        if create && db_path.exists() {
            return Err(Error::new("already-initialized", 409));
        }
        if !create && !db_path.is_file() {
            return Err(Error::new("not-initialized", 404));
        }
        let mut db = Connection::open(db_path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        if create {
            for name in ["objects", "staging"] {
                fs::create_dir_all(root.join(name))?;
            }
            let tx = db.transaction()?;
            tx.execute_batch(schema::SCHEMA)?;
            let head = RemoteHead::genesis(random_id()?, random_id()?)?;
            tx.execute("INSERT INTO library VALUES(1,?1)", [json(&head)?])?;
            tx.execute(
                "INSERT INTO media_secret VALUES(1,?1)",
                [risunest_sync_connect::media::generate_key()?.as_slice()],
            )?;
            tx.commit()?;
            objects::sync_directory(&root)?;
        } else {
            let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if version != schema::VERSION {
                return Err(Error::new("incompatible-store", 409));
            }
        }
        db.execute(
            "UPDATE uploads SET state=CASE WHEN id IN (SELECT upload FROM upload_jobs) THEN 'queued' ELSE 'open' END WHERE state='finalizing'",
            [],
        )?;
        db.execute(
            "UPDATE download_deltas SET state='queued' WHERE state='working'",
            [],
        )?;
        let read_db = Connection::open_with_flags(
            root.join("metadata.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        read_db.busy_timeout(std::time::Duration::from_secs(5))?;
        let media_key: Vec<u8> =
            db.query_row("SELECT key FROM media_secret WHERE singleton=1", [], |r| {
                r.get(0)
            })?;
        let media_signer = risunest_sync_connect::media::MediaSigner::new(&media_key)?;
        let store = Self {
            root,
            db: Mutex::new(db),
            read_db: Mutex::new(read_db),
            objects_gate: Mutex::new(()),
            upload_job_gate: Mutex::new(()),
            download_job_gate: Mutex::new(()),
            connection_gate: Mutex::new(()),
            media_signer,
            heads: tokio::sync::watch::Sender::new(0),
            _owner: owner,
        };
        store
            .head()?
            .validate()
            .map_err(|_| Error::new("corrupt-metadata", 503))?;
        Ok(store)
    }
    pub(super) fn db(&self) -> Result<MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|_| Error::new("writer-unavailable", 503))
    }
    pub(super) fn reader(&self) -> Result<MutexGuard<'_, Connection>> {
        self.read_db
            .lock()
            .map_err(|_| Error::new("reader-unavailable", 503))
    }
    pub fn head(&self) -> Result<RemoteHead> {
        Self::read_head(&*self.reader()?)
    }
    /// Observes announcements that the head may have moved. A reader still
    /// confirms the head itself: an announcement is never the state.
    pub fn head_announcements(&self) -> tokio::sync::watch::Receiver<u64> {
        self.heads.subscribe()
    }
    pub(super) fn announce_head(&self) {
        self.heads.send_modify(|value| *value = value.wrapping_add(1));
    }
    pub fn device_session(&self, device: &Device) -> Result<DeviceSession> {
        let mut connection = self.reader()?;
        let db = connection.transaction()?;
        Self::require_device(&db, device)?;
        let watermark: String = db.query_row(
            "SELECT watermark FROM devices WHERE id=?1",
            [&device.id],
            |r| r.get(0),
        )?;
        let pending = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM commit_jobs WHERE device=?1)",
            [&device.id],
            |r| r.get(0),
        )?;
        Ok(DeviceSession {
            head: Self::read_head(&db)?,
            device_id: device.id.clone(),
            operation_watermark: watermark.try_into()?,
            operation_pending: pending,
        })
    }
    pub fn device_active(&self, actor: &Device, id: &str) -> Result<bool> {
        validate_id(id)?;
        let db = self.reader()?;
        Self::require_device(&db, actor)?;
        Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM devices WHERE id=?1 AND revoked=0)",
            [id],
            |r| r.get(0),
        )?)
    }
    pub(super) fn read_head(db: &Connection) -> Result<RemoteHead> {
        parse(&db.query_row::<String, _, _>(
            "SELECT head FROM library WHERE singleton=1",
            [],
            |r| r.get(0),
        )?)
    }
    pub fn add_device(&self) -> Result<DeviceCredential> {
        self.add_named_device("", None)
    }
    pub(super) fn add_named_device(
        &self,
        name: &str,
        request: Option<&str>,
    ) -> Result<DeviceCredential> {
        let token = random_id()?;
        let device_id = random_id()?;
        let db = self.db()?;
        if let Some(request) = request {
            let exists: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM devices WHERE registration_request=?1)",
                [request],
                |r| r.get(0),
            )?;
            if exists {
                return Err(Error::new("registration-already-issued", 409));
            }
        }
        let library_id = Self::read_head(&db)?.library_id;
        db.execute(
            "INSERT INTO devices(id,verifier,name,registration_request) VALUES(?1,?2,?3,?4)",
            params![device_id, hash(token.as_bytes()), name, request],
        )?;
        Ok(DeviceCredential {
            device_id,
            library_id,
            token,
        })
    }
    pub fn revoke_device(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        if self
            .db()?
            .execute("UPDATE devices SET revoked=1 WHERE id=?1", [id])?
            == 0
        {
            return Err(Error::new("device-not-found", 404));
        }
        Ok(())
    }
    pub fn authenticate(&self, library: &str, token: &str) -> Result<Device> {
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::new("unauthorized", 401));
        }
        let db = self.reader()?;
        if Self::read_head(&db)?.library_id != library {
            return Err(Error::new("unauthorized", 401));
        }
        let id = db
            .query_row(
                "SELECT id FROM devices WHERE verifier=?1 AND revoked=0",
                [hash(token.as_bytes())],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| Device { id })
            .ok_or(Error::new("unauthorized", 401))
    }
    pub(super) fn require_device(db: &Connection, device: &Device) -> Result<()> {
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM devices WHERE id=?1 AND revoked=0)",
            [&device.id],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(Error::new("unauthorized", 401));
        }
        Ok(())
    }
    /// Per-section application points. Sections absent from the request are not
    /// received, which is neither a deletion nor a completed application.
    pub fn acknowledge(
        &self,
        device: &Device,
        epoch: &str,
        sections: &BTreeMap<Domain, Sequence>,
    ) -> Result<()> {
        if sections.is_empty() {
            return Err(Error::new("invalid-ack", 400));
        }
        let mut db = self.db()?;
        Self::require_device(&db, device)?;
        let tx = db.transaction()?;
        let head = Self::read_head(&tx)?;
        if head.epoch != epoch {
            return Err(Error::new("invalid-ack", 409));
        }
        for (domain, seq) in sections {
            let old = Self::read_section_ack(&tx, &device.id, *domain)?;
            if seq > &head.seq || seq < &old {
                return Err(Error::new("invalid-ack", 409));
            }
            tx.execute("INSERT INTO device_section_acks VALUES(?1,?2,?3) ON CONFLICT(device,domain) DO UPDATE SET ack=excluded.ack",params![device.id,domain.as_str(),seq.as_str()])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(super) fn read_section_ack(
        db: &Connection,
        device: &str,
        domain: Domain,
    ) -> Result<Sequence> {
        let value: Option<String> = db
            .query_row(
                "SELECT ack FROM device_section_acks WHERE device=?1 AND domain=?2",
                params![device, domain.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match value {
            Some(value) => value.try_into()?,
            None => 0.into(),
        })
    }
    /// The oldest point every active device has applied for one section.
    pub fn section_ack_floor(&self, domain: Domain) -> Result<Sequence> {
        let db = self.reader()?;
        Self::read_section_ack_floor(&db, domain, &Self::read_head(&db)?.seq)
    }
    pub(super) fn read_section_ack_floor(
        db: &Connection,
        domain: Domain,
        ceiling: &Sequence,
    ) -> Result<Sequence> {
        let mut floor = ceiling.clone();
        let mut statement = db.prepare(
            "SELECT COALESCE((SELECT ack FROM device_section_acks WHERE device=devices.id AND domain=?1),'0') FROM devices WHERE revoked=0",
        )?;
        for value in statement.query_map([domain.as_str()], |r| r.get::<_, String>(0))? {
            floor = floor.min(Sequence::try_from(value?)?);
        }
        Ok(floor)
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn exported_store_format_matches_the_initialized_schema() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::init(&root.path().join("sync")).unwrap();
        let version: i64 = store
            .reader()
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, schema::VERSION);
        assert_eq!(
            crate::STORE_FORMAT_ID,
            format!("risunest-sync-store/v{version}")
        );
    }
}
