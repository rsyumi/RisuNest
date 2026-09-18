//! Device-local payload custody. This is physical residency metadata, not a
//! logical record or a synchronized preference. Credentials remain OS-protected.
use super::{credentials::StoredConfig, Result, SyncError};
use risunest_sync_wire::{hash, validate_hash, RemoteHead, Sequence};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AssetPolicy {
    Full,
    Remote,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RetainedObject {
    pub hash: String,
    pub size: Sequence,
    pub retention_id: String,
}

pub(crate) struct RemoteObject {
    pub context: String,
    pub hash: String,
    pub size: u64,
    pub retention_id: String,
    pub device_id: String,
    pub config: StoredConfig,
}

pub(crate) struct Residency {
    db: Connection,
}

impl Residency {
    pub fn path(root: &Path) -> PathBuf {
        root.join("server-sync").join("asset-residency.sqlite")
    }
    pub fn exists(root: &Path) -> bool {
        Self::path(root).exists()
    }
    pub fn open(root: &Path) -> Result<Self> {
        let directory = root.join("server-sync");
        std::fs::create_dir_all(&directory)?;
        let metadata = std::fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || crate::trust_boundary::is_link_like(&metadata) {
            return Err(SyncError::new("unsafe-residency-path", 409));
        }
        let path = std::fs::canonicalize(&directory)?.join("asset-residency.sqlite");
        for suffix in ["", "-wal", "-shm"] {
            let target = PathBuf::from(format!("{}{suffix}", path.display()));
            match std::fs::symlink_metadata(target) {
                Ok(metadata)
                    if !metadata.is_file() || crate::trust_boundary::is_link_like(&metadata) =>
                {
                    return Err(SyncError::new("unsafe-residency-path", 409))
                }
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
        let mut db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            let tx = db.transaction()?;
            let count: i64 =
                tx.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
            if count != 0 {
                return Err(SyncError::new("invalid-residency-store", 409));
            }
            tx.execute_batch("CREATE TABLE contexts(id TEXT PRIMARY KEY,library_id TEXT NOT NULL,device_id TEXT NOT NULL,epoch TEXT NOT NULL,config TEXT NOT NULL);
                CREATE TABLE objects(context TEXT NOT NULL REFERENCES contexts(id),hash TEXT NOT NULL,size INTEGER NOT NULL CHECK(size>=0),retention_id TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('active','releasing','released')),PRIMARY KEY(context,hash));
                CREATE INDEX objects_hash ON objects(hash);
                PRAGMA user_version=1;")?;
            tx.commit()?;
        } else if version != 1 {
            return Err(SyncError::new("incompatible-residency-store", 409));
        }
        Ok(Self { db })
    }
    /// A fresh registration restores access to the same library's historical
    /// custody. Keep its original owner ID for eventual retention release.
    pub fn replace_access_config(&self, config: &StoredConfig) -> Result<()> {
        self.db.execute(
            "UPDATE contexts SET config=?1 WHERE library_id=?2",
            params![
                serde_json::to_string(config)
                    .map_err(|_| SyncError::new("invalid-retention-config", 409))?,
                config.library_id
            ],
        )?;
        Ok(())
    }
    pub fn context_id(config: &StoredConfig, epoch: &str) -> String {
        hash(format!("{}\0{}\0{}", config.library_id, config.device_id, epoch).as_bytes())
    }
    pub fn confirm(
        &mut self,
        config: &StoredConfig,
        head: &RemoteHead,
        objects: &[RetainedObject],
    ) -> Result<()> {
        head.validate()?;
        if config.library_id != head.library_id {
            return Err(SyncError::new("retention-library-mismatch", 409));
        }
        let context = Self::context_id(config, &head.epoch);
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO contexts VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET config=excluded.config",params![context,config.library_id,config.device_id,head.epoch,serde_json::to_string(config).map_err(|_|SyncError::new("invalid-retention-config",409))?])?;
        for object in objects {
            validate_hash(&object.hash)?;
            validate_hash(&object.retention_id)?;
            let size = object
                .size
                .as_str()
                .parse::<i64>()
                .map_err(|_| SyncError::new("invalid-retention-size", 409))?;
            tx.execute("INSERT INTO objects VALUES(?1,?2,?3,?4,'active') ON CONFLICT(context,hash) DO UPDATE SET size=excluded.size,retention_id=excluded.retention_id,state='active'",params![context,object.hash,size,object.retention_id])?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Persist a positively acknowledged custody grant before a dependency can
    /// be activated without bytes or an existing local file can be evicted.
    pub fn retain(
        &mut self,
        client: &super::client::ServerClient,
        config: &StoredConfig,
        head: &RemoteHead,
        objects: &[(String, Option<u64>)],
    ) -> Result<()> {
        for page in objects.chunks(1024) {
            let requests=page.iter().map(|(hash,size)|serde_json::json!({"hash":hash,"size":size.map(|size|size.to_string())})).collect::<Vec<_>>();
            let (_, retained): (_, Vec<RetainedObject>) = client.json(
                reqwest::Method::POST,
                "objects/retention",
                &[],
                Some(&serde_json::json!({"epoch":head.epoch,"objects":requests})),
                &[],
            )?;
            let mut expected = page
                .iter()
                .cloned()
                .collect::<std::collections::BTreeMap<_, _>>();
            if expected.len() != page.len() || retained.len() != page.len() {
                return Err(SyncError::new("invalid-retention-response", 502));
            }
            for object in &retained {
                let Some(size) = expected.remove(&object.hash) else {
                    return Err(SyncError::new("invalid-retention-response", 502));
                };
                if size.is_some_and(|size| object.size != size.into()) {
                    return Err(SyncError::new("invalid-retention-response", 502));
                }
            }
            self.confirm(config, head, &retained)?;
        }
        Ok(())
    }
    pub fn object(&self, digest: &str, context: Option<&str>) -> Result<Option<RemoteObject>> {
        self.lookup(digest, context, true)
    }
    pub fn release_object(&self, digest: &str, context: &str) -> Result<Option<RemoteObject>> {
        self.lookup(digest, Some(context), false)
    }
    fn lookup(
        &self,
        digest: &str,
        context: Option<&str>,
        active: bool,
    ) -> Result<Option<RemoteObject>> {
        validate_hash(digest)?;
        let value:Option<(String,i64,String,String,String)>=self.db.query_row("SELECT o.context,o.size,o.retention_id,c.device_id,c.config FROM objects o JOIN contexts c ON c.id=o.context WHERE o.hash=?1 AND o.state!='released' AND (NOT ?3 OR o.state='active') AND (?2 IS NULL OR o.context=?2) ORDER BY o.rowid DESC LIMIT 1",params![digest,context,active],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        value
            .map(|(context, size, retention_id, device_id, config)| {
                Ok(RemoteObject {
                    context,
                    hash: digest.to_owned(),
                    size: u64::try_from(size)
                        .map_err(|_| SyncError::new("invalid-retention-size", 409))?,
                    retention_id,
                    device_id,
                    config: serde_json::from_str(&config)
                        .map_err(|_| SyncError::new("invalid-retention-config", 409))?,
                })
            })
            .transpose()
    }
    pub fn confirms(&self, digest: &str, size: Option<u64>, context: &str) -> Result<bool> {
        Ok(self
            .object(digest, Some(context))?
            .is_some_and(|object| size.is_none_or(|size| object.size == size)))
    }
    pub fn gc_size(&self, digest: &str) -> std::io::Result<Option<u64>> {
        self.lookup(digest, None, false)
            .map(|object| object.map(|object| object.size))
            .map_err(|error| std::io::Error::other(error.code))
    }
    pub fn page(&self, after: &str) -> Result<Vec<(String, String, u64)>> {
        let mut statement=self.db.prepare("SELECT context||'/'||hash,hash,size FROM objects WHERE state!='released' AND context||'/'||hash>?1 ORDER BY context,hash LIMIT 128")?;
        let rows = statement
            .query_map([after], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(cursor, hash, size)| {
                Ok((
                    cursor,
                    hash,
                    u64::try_from(size)
                        .map_err(|_| SyncError::new("invalid-retention-size", 409))?,
                ))
            })
            .collect()
    }
    pub fn begin_release(&self, object: &RemoteObject) -> Result<bool> {
        Ok(self.db.execute("UPDATE objects SET state='releasing' WHERE context=?1 AND hash=?2 AND retention_id=?3 AND state IN ('active','releasing')",params![object.context,object.hash,object.retention_id])?==1)
    }
    pub fn begin_latest_release(
        &mut self,
        digest: &str,
        context: &str,
    ) -> Result<Option<RemoteObject>> {
        validate_hash(digest)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let value: Option<(i64, String, String, String)> = tx
            .query_row(
                "SELECT o.size,o.retention_id,c.device_id,c.config FROM objects o JOIN contexts c ON c.id=o.context WHERE o.hash=?1 AND o.context=?2 AND o.state!='released'",
                params![digest, context],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((size, retention_id, device_id, config)) = value else {
            tx.commit()?;
            return Ok(None);
        };
        let updated = tx.execute(
            "UPDATE objects SET state='releasing' WHERE context=?1 AND hash=?2 AND retention_id=?3 AND state IN ('active','releasing')",
            params![context, digest, retention_id],
        )?;
        tx.commit()?;
        if updated != 1 {
            return Ok(None);
        }
        Ok(Some(RemoteObject {
            context: context.to_owned(),
            hash: digest.to_owned(),
            size: u64::try_from(size).map_err(|_| SyncError::new("invalid-retention-size", 409))?,
            retention_id,
            device_id,
            config: serde_json::from_str(&config)
                .map_err(|_| SyncError::new("invalid-retention-config", 409))?,
        }))
    }
    pub fn finish_release(&self, object: &RemoteObject) -> Result<()> {
        self.db.execute("UPDATE objects SET state='released' WHERE context=?1 AND hash=?2 AND retention_id=?3 AND state='releasing'",params![object.context,object.hash,object.retention_id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;

/// Byte consumers (exports, AI attachments, plugins) need verified bytes. The
/// temporary transfer CAS is discarded after promotion, including chunk files.
/// Display URLs never call this path.
pub(crate) fn open_or_hydrate(root: &Path, digest: &str) -> Result<Option<std::fs::File>> {
    open_or_hydrate_with_check(root, digest, &|| Ok(()))
}

pub(crate) fn open_or_hydrate_with_check(
    root: &Path,
    digest: &str,
    check: &dyn Fn() -> Result<()>,
) -> Result<Option<std::fs::File>> {
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    static IN_FLIGHT: OnceLock<Mutex<std::collections::HashMap<PathBuf, Weak<Mutex<()>>>>> =
        OnceLock::new();
    check()?;
    validate_hash(digest)?;
    let root = std::fs::canonicalize(root)?;
    let key = root.join(digest);
    let lock = {
        let mut locks = IN_FLIGHT
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| SyncError::new("hydration-unavailable", 503))?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(Mutex::new(()));
            locks.insert(key, Arc::downgrade(&lock));
            lock
        }
    };
    let _hydrating = lock_with_check(&lock, check)?;
    let cas = crate::asset_repository::PayloadCas::new(&root)?;
    {
        let _guard = crate::asset_repository::coordinator::lock_repository_mutation()?;
        if let Some(file) = cas.open_object(digest)? {
            return Ok(Some(file));
        }
    }
    let Some(proof) = Residency::open(&root)?.object(digest, None)? else {
        return Ok(None);
    };
    // Byte consumers share one transfer budget. Each Transfer can already use
    // parallel chunks; unrelated attachments must not multiply those buffers.
    static TRANSFER_BUDGET: Mutex<()> = Mutex::new(());
    let _budget = lock_with_check(&TRANSFER_BUDGET, check)?;
    let client = super::client::ServerClient::new(proof.config.resolve(&root)?)?;
    client.resolve_identity(false)?;
    check()?;
    let directory = tempfile::Builder::new()
        .prefix("asset-hydration-")
        .tempdir_in(&root)?;
    let cache = super::cache::Cache::open(directory.path())?;
    super::transfer::Transfer::new(&client, &cache)?
        .with_check(check)
        .download(&[digest.to_owned()], &[])?;
    let mut file = cache
        .cas
        .open_object(digest)?
        .ok_or_else(|| SyncError::new("hydration-incomplete", 502))?;
    let _guard = crate::asset_repository::coordinator::lock_repository_mutation()?;
    super::transfer::prepare_checked(&cas, &mut file, digest, proof.size, check)?;
    Ok(cas.open_object(digest)?)
}

fn lock_with_check<'a>(
    lock: &'a std::sync::Mutex<()>,
    check: &dyn Fn() -> Result<()>,
) -> Result<std::sync::MutexGuard<'a, ()>> {
    loop {
        check()?;
        match lock.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(SyncError::new("hydration-unavailable", 503));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

/// Explicit byte-consumer adapter. Ordinary PayloadCas::stat_object continues
/// to describe physical storage and never performs a network request.
pub(crate) trait RemotePayloadAccess {
    fn open_available_object(&self, hash: &str) -> std::io::Result<Option<std::fs::File>>;
    fn stat_available_object(&self, hash: &str) -> std::io::Result<Option<u64>> {
        self.open_available_object(hash)?
            .map(|file| file.metadata().map(|metadata| metadata.len()))
            .transpose()
    }
}
impl RemotePayloadAccess for crate::asset_repository::PayloadCas {
    fn open_available_object(&self, hash: &str) -> std::io::Result<Option<std::fs::File>> {
        open_or_hydrate(self.repository_root(), hash)
            .map_err(|error| std::io::Error::other(error.code))
    }
}
