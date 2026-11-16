//! Device-local payload custody. This is physical residency metadata, not a
//! logical record or a synchronized preference. Credentials remain OS-protected.
use super::{credentials::StoredConfig, Result, SyncError};
use crate::persistent_store::asset_object_catalog::{AssetObjectRegistrar, AssetObjectRegistration};
use risunest_sync_wire::{hash, validate_hash, RemoteHead, Sequence};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

#[cfg(test)]
thread_local! {
    pub(crate) static OPEN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// Closing the last connection tears down the write-ahead log, and opens that
// race the teardown fail with SQLITE_PROTOCOL. While armed, one idle
// connection stays open so per-use connections are never the last one.
struct Anchor {
    root: PathBuf,
    db: Option<Connection>,
}

static ANCHOR: Mutex<Option<Anchor>> = Mutex::new(None);

fn anchor() -> MutexGuard<'static, Option<Anchor>> {
    ANCHOR.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Keeps a connection to `root`'s store open after it next opens. Arming does
/// not create the store.
pub(crate) fn arm_anchor(root: &Path) {
    *anchor() = Some(Anchor {
        root: root.to_path_buf(),
        db: None,
    });
}

#[cfg(any(mobile, test))]
pub(crate) fn release_anchor() {
    anchor().take();
}

fn hold_anchor(root: &Path, path: &Path) {
    let mut anchor = anchor();
    let Some(anchor) = anchor.as_mut().filter(|anchor| anchor.root == root) else {
        return;
    };
    if anchor.db.is_some() {
        return;
    }
    // Only a connection that has read holds the shared lock that keeps the
    // log alive. Failure leaves the next open to retry.
    anchor.db = crate::sqlite_open::open(path)
        .and_then(|db| {
            db.query_row("PRAGMA user_version", [], |_| Ok(()))?;
            Ok(db)
        })
        .ok();
}

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

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    /// The body registry `gc_size` falls back to, opened when first asked.
    remote: std::cell::RefCell<crate::external_storage::lww_residency::RemoteBodies>,
}

impl Residency {
    pub fn path(root: &Path) -> PathBuf {
        root.join("server-sync").join("asset-residency.sqlite")
    }
    pub fn exists(root: &Path) -> bool {
        Self::path(root).exists()
    }
    pub fn open(root: &Path) -> Result<Self> {
        #[cfg(test)]
        OPEN_COUNT.with(|count| count.set(count.get() + 1));
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
        let mut db = crate::sqlite_open::open(&path)?;
        let configure_started = std::time::Instant::now();
        let configure_budget = std::time::Duration::from_secs(5);
        loop {
            db.busy_timeout(configure_budget.saturating_sub(configure_started.elapsed()))?;
            match db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;") {
                Ok(()) => break,
                // A simultaneous WAL-mode change can bypass SQLite's busy timeout.
                Err(error) if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
                    && configure_started.elapsed() < configure_budget => {
                        std::thread::sleep(std::time::Duration::from_millis(10).min(configure_budget.saturating_sub(configure_started.elapsed())));
                    }
                Err(error) => return Err(error.into()),
            }
        }
        db.busy_timeout(configure_budget)?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if version == 0 {
                let count: i64 =
                    tx.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
                if count != 0 {
                    return Err(SyncError::new("invalid-residency-store", 409));
                }
                tx.execute_batch("CREATE TABLE contexts(id TEXT PRIMARY KEY,library_id TEXT NOT NULL,device_id TEXT NOT NULL,epoch TEXT NOT NULL,config TEXT NOT NULL);
                CREATE TABLE objects(context TEXT NOT NULL REFERENCES contexts(id),hash TEXT NOT NULL,size INTEGER NOT NULL CHECK(size>=0),retention_id TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('active','releasing','released')),confirmation_order INTEGER NOT NULL CHECK(confirmation_order>0),PRIMARY KEY(context,hash));
                CREATE INDEX objects_hash ON objects(hash);
                CREATE INDEX objects_confirmation_order ON objects(confirmation_order);
                PRAGMA user_version=1;")?;
            } else if version != 1 {
                return Err(SyncError::new("incompatible-residency-store", 409));
            }
            tx.commit()?;
        } else if version != 1 {
            return Err(SyncError::new("incompatible-residency-store", 409));
        }
        hold_anchor(root, &path);
        Ok(Self { db, remote: std::cell::RefCell::new(crate::external_storage::lww_residency::RemoteBodies::deferred(root)) })
    }
    /// A fresh registration restores access to the same library's historical
    /// custody. Keep its original owner ID for eventual retention release.
    pub fn replace_access_config(&self, config: &StoredConfig) -> Result<()> {
        self.db.execute(
            "UPDATE contexts SET config=?1 WHERE library_id=?2 AND config<>?1",
            params![
                serde_json::to_string(config)
                    .map_err(|_| SyncError::new("invalid-retention-config", 409))?,
                config.library_id
            ],
        )?;
        Ok(())
    }
    pub fn context_id(config: &StoredConfig, epoch: &str) -> String {
        #[cfg(test)]
        super::hash_metrics::record("c_context_identity", format!("{}\0{}\0{}", config.library_id, config.device_id, epoch).len());
        hash(format!("{}\0{}\0{}", config.library_id, config.device_id, epoch).as_bytes())
    }
    #[cfg(test)]
    pub fn confirm(
        &mut self,
        config: &StoredConfig,
        head: &RemoteHead,
        objects: &[RetainedObject],
    ) -> Result<()> {
        Self::check_head(config, head)?;
        self.confirm_under(config, &head.epoch, objects)
    }
    fn check_head(config: &StoredConfig, head: &RemoteHead) -> Result<()> {
        head.validate()?;
        if config.library_id != head.library_id {
            return Err(SyncError::new("retention-library-mismatch", 409));
        }
        Ok(())
    }
    fn confirm_under(&mut self, config: &StoredConfig, epoch: &str, objects: &[RetainedObject]) -> Result<()> {
        let context = Self::context_id(config, epoch);
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO contexts VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET config=excluded.config",params![context,config.library_id,config.device_id,epoch,serde_json::to_string(config).map_err(|_|SyncError::new("invalid-retention-config",409))?])?;
        let order: i64 = tx.query_row("SELECT COALESCE(MAX(confirmation_order),0)+1 FROM objects", [], |row| row.get(0))?;
        for object in objects {
            validate_hash(&object.hash)?;
            validate_hash(&object.retention_id)?;
            let size = object
                .size
                .as_str()
                .parse::<i64>()
                .map_err(|_| SyncError::new("invalid-retention-size", 409))?;
            tx.execute("INSERT INTO objects VALUES(?1,?2,?3,?4,'active',?5) ON CONFLICT(context,hash) DO UPDATE SET size=excluded.size,retention_id=excluded.retention_id,state='active',confirmation_order=excluded.confirmation_order",params![context,object.hash,size,object.retention_id,order])?;
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
        Self::check_head(config, head)?;
        self.retain_under(client, config, &head.epoch, objects)
    }
    /// Retains `objects` under the epoch of the newest custody confirmation, without asking the
    /// server for its head first, when that confirmation was this device's in the configured
    /// library. Otherwise, or when the server has moved to another epoch, it asks and retains
    /// under the current head.
    pub fn retain_current(
        &mut self,
        client: &super::client::ServerClient,
        config: &StoredConfig,
        objects: &[(String, Option<u64>)],
    ) -> Result<()> {
        let confirmed: Option<String> = self.db.query_row(
            "SELECT epoch FROM contexts WHERE id=(SELECT context FROM objects ORDER BY confirmation_order DESC LIMIT 1) AND library_id=?1 AND device_id=?2",
            params![config.library_id, config.device_id],
            |row| row.get(0),
        ).optional()?;
        if let Some(epoch) = confirmed {
            match self.retain_under(client, config, &epoch, objects) {
                Err(error) if error.code == "epoch-mismatch" => {}
                other => return other,
            }
        }
        let head = client.resolve_identity()?;
        self.retain(client, config, &head, objects)
    }
    fn retain_under(
        &mut self,
        client: &super::client::ServerClient,
        config: &StoredConfig,
        epoch: &str,
        objects: &[(String, Option<u64>)],
    ) -> Result<()> {
        for page in objects.chunks(1024) {
            let requests=page.iter().map(|(hash,size)|serde_json::json!({"hash":hash,"size":size.map(|size|size.to_string())})).collect::<Vec<_>>();
            let (_, retained): (_, Vec<RetainedObject>) = client.json(
                reqwest::Method::POST,
                "objects/retention",
                &[],
                Some(&serde_json::json!({"epoch":epoch,"objects":requests})),
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
            self.confirm_under(config, epoch, &retained)?;
        }
        Ok(())
    }
    pub fn object(&self, digest: &str, context: Option<&str>) -> Result<Option<RemoteObject>> {
        self.lookup(digest, context, true)
    }
    /// The digests among `digests` that `object` finds, asked in pages.
    pub(crate) fn active_among(&self, digests: &[String]) -> Result<std::collections::BTreeSet<String>> {
        let mut found = std::collections::BTreeSet::new();
        for page in digests.chunks(512) {
            let marks = (1..=page.len()).map(|index| format!("?{index}")).collect::<Vec<_>>().join(",");
            let mut query = self.db.prepare(&format!("SELECT DISTINCT o.hash FROM objects o JOIN contexts c ON c.id=o.context WHERE o.state='active' AND o.hash IN ({marks})"))?;
            for hash in query.query_map(rusqlite::params_from_iter(page), |row| row.get::<_, String>(0))? {
                found.insert(hash?);
            }
        }
        Ok(found)
    }
    /// The size of what `object(digest, None)` finds for each of `digests`, asked in pages.
    pub(crate) fn active_sizes(&self, digests: &[String]) -> Result<Vec<Option<u64>>> {
        let mut sizes = Vec::with_capacity(digests.len());
        let mut parsed = std::collections::HashSet::new();
        for page in digests.chunks(512) {
            let marks = (1..=page.len()).map(|index| format!("?{index}")).collect::<Vec<_>>().join(",");
            let mut query = self.db.prepare(&format!("SELECT o.hash,o.size,c.config FROM objects o JOIN contexts c ON c.id=o.context WHERE o.state='active' AND o.hash IN ({marks}) ORDER BY o.hash,o.confirmation_order DESC"))?;
            let mut latest = std::collections::HashMap::new();
            for row in query.query_map(rusqlite::params_from_iter(page), |r| Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?, r.get::<_, String>(2)?))))? {
                let (hash, value) = row?;
                latest.entry(hash).or_insert(value);
            }
            for digest in page {
                validate_hash(digest)?;
                let Some((size, config)) = latest.get(digest) else {
                    sizes.push(None);
                    continue;
                };
                let size = u64::try_from(*size).map_err(|_| SyncError::new("invalid-retention-size", 409))?;
                if !parsed.contains(config) {
                    serde_json::from_str::<StoredConfig>(config)
                        .map_err(|_| SyncError::new("invalid-retention-config", 409))?;
                    parsed.insert(config.clone());
                }
                sizes.push(Some(size));
            }
        }
        Ok(sizes)
    }
    pub(crate) fn target_holds(&self, digest: &str, library_id: &str, target_id: &str) -> Result<bool> {
        validate_hash(digest)?;
        validate_hash(target_id)?;
        let mut query = self.db.prepare("SELECT DISTINCT c.epoch FROM objects o JOIN contexts c ON c.id=o.context WHERE o.hash=?1 AND o.state='active' AND c.library_id=?2")?;
        for epoch in query.query_map(params![digest, library_id], |row| row.get::<_, String>(0))? {
            if hash(format!("{library_id}:{}", epoch?).as_bytes()) == target_id { return Ok(true); }
        }
        Ok(false)
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
        let value:Option<(String,i64,String,String,String)>=self.db.query_row("SELECT o.context,o.size,o.retention_id,c.device_id,c.config FROM objects o JOIN contexts c ON c.id=o.context WHERE o.hash=?1 AND o.state!='released' AND (NOT ?3 OR o.state='active') AND (?2 IS NULL OR o.context=?2) ORDER BY o.confirmation_order DESC LIMIT 1",params![digest,context,active],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
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
            .and_then(|size| match size { Some(size) => Ok(Some(size)), None => self.remote.borrow_mut().stat(digest) })
    }
    /// The custody half of `gc_size`.
    pub(crate) fn custody_size(&self, digest: &str) -> Result<Option<u64>> {
        Ok(self.lookup(digest, None, false)?.map(|object| object.size))
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

/// Stands in for the Sync download in tests. A body a test serves for one
/// repository still goes through custody and the checked promotion into its
/// CAS; only the network transfer is skipped.
#[cfg(test)]
pub(crate) mod test_remote {
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
        sync::Mutex,
    };

    static SERVED: Mutex<BTreeMap<PathBuf, (BTreeMap<String, Vec<u8>>, usize)>> =
        Mutex::new(BTreeMap::new());

    pub(crate) fn serve(root: &Path, digest: &str, body: Vec<u8>) {
        let root = std::fs::canonicalize(root).unwrap();
        SERVED.lock().unwrap().entry(root).or_default().0.insert(digest.into(), body);
    }

    /// Records custody for these bodies, as a confirmed remote head does.
    pub(crate) fn hold(root: &Path, bodies: &[(&str, u64)]) {
        let config: super::StoredConfig = serde_json::from_value(serde_json::json!({
            "endpoint":"http://127.0.0.1:9/", "libraryId":"library", "deviceId":"device",
            "credentialId":"00000000-0000-4000-8000-000000000000"
        }))
        .unwrap();
        let head = super::RemoteHead {
            head_id: super::hash(b"head"),
            ..super::RemoteHead::genesis("library".into(), "epoch".into()).unwrap()
        };
        let objects = bodies
            .iter()
            .map(|(digest, size)| super::RetainedObject {
                hash: (*digest).into(),
                size: (*size).into(),
                retention_id: super::hash(digest.as_bytes()),
            })
            .collect::<Vec<_>>();
        super::Residency::open(root)
            .unwrap()
            .confirm(&config, &head, &objects)
            .unwrap();
    }

    /// Leaves a body only on the Sync server: its local file is removed, and the
    /// server holds and serves it.
    pub(crate) fn keep_only_on_server(root: &Path, digest: &str) {
        let cas = crate::asset_repository::PayloadCas::new(root).unwrap();
        let path = cas.object_path(digest).unwrap().unwrap();
        let body = std::fs::read(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        hold(root, &[(digest, body.len() as u64)]);
        serve(root, digest, body);
    }

    /// How many bodies this repository fetched.
    pub(crate) fn fetched(root: &Path) -> usize {
        let root = std::fs::canonicalize(root).unwrap();
        SERVED.lock().unwrap().get(&root).map_or(0, |(_, fetched)| *fetched)
    }

    static FETCH_HOOKS: Mutex<BTreeMap<PathBuf, Box<dyn Fn() + Send>>> =
        Mutex::new(BTreeMap::new());

    /// Runs `hook` each time this repository starts fetching a body.
    pub(crate) fn on_fetch(root: &Path, hook: impl Fn() + Send + 'static) {
        let root = std::fs::canonicalize(root).unwrap();
        FETCH_HOOKS.lock().unwrap().insert(root, Box::new(hook));
    }

    pub(super) fn body(root: &Path, digest: &str) -> Option<Vec<u8>> {
        let body = {
            let mut served = SERVED.lock().unwrap();
            let (bodies, fetched) = served.get_mut(root)?;
            let body = bodies.get(digest)?.clone();
            *fetched += 1;
            body
        };
        if let Some(hook) = FETCH_HOOKS.lock().unwrap().get(root) {
            hook();
        }
        Some(body)
    }
}

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
    HydrationSession::new(root, None)?.open(digest, check)
}

static TRANSFER_BUDGET: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) struct TransientBody {
    body: super::cache::Body,
    _directory: Option<tempfile::TempDir>,
}
impl TransientBody {
    pub(crate) fn len(&self) -> std::io::Result<u64> {
        self.body.len()
    }
}
impl std::io::Read for TransientBody {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.body, buffer)
    }
}

pub(crate) fn open_transient_with_check(
    root: &Path,
    scratch_root: &Path,
    digest: &str,
    check: &dyn Fn() -> Result<()>,
) -> Result<Option<TransientBody>> {
    open_transient_inner(root, scratch_root, digest, check, true, None)
}
pub(crate) fn open_transient_server_with_check(
    root: &Path,
    scratch_root: &Path,
    digest: &str,
    check: &dyn Fn() -> Result<()>,
) -> Result<Option<TransientBody>> {
    open_transient_server_observed(root, scratch_root, digest, check, None)
}
pub(crate) fn open_transient_server_observed(
    root: &Path, scratch_root: &Path, digest: &str, check: &dyn Fn() -> Result<()>,
    observer: Option<std::sync::Arc<dyn Fn(u64) + Send + Sync>>,
) -> Result<Option<TransientBody>> {
    open_transient_inner(root, scratch_root, digest, check, false, observer)
}
fn open_transient_inner(
    root: &Path,
    scratch_root: &Path,
    digest: &str,
    check: &dyn Fn() -> Result<()>,
    allow_external: bool,
    observer: Option<std::sync::Arc<dyn Fn(u64) + Send + Sync>>,
) -> Result<Option<TransientBody>> {
    check()?;
    validate_hash(digest)?;
    let cas = crate::asset_repository::PayloadCas::new(root)?;
    #[cfg(test)]
    let local = cas.open_object_tracked(digest)?.map(super::cache::Body::Tracked);
    #[cfg(not(test))]
    let local = cas.open_object(digest)?.map(super::cache::Body::File);
    if let Some(body) = local {
        return Ok(Some(TransientBody { body, _directory: None }));
    }
    let Some(proof) = Residency::open(root)?.object(digest, None)? else {
        if !allow_external { return Ok(None); }
        return Ok(crate::external_storage::lww_residency::hydrate(root, digest, check)?.map(|file|TransientBody {body:super::cache::Body::File(file),_directory:None}));
    };
    open_transient_server_proof_observed(root, scratch_root, &proof, check, observer)
}
pub(crate) fn open_transient_server_proof_with_check(
    root: &Path,
    scratch_root: &Path,
    proof: &RemoteObject,
    check: &dyn Fn() -> Result<()>,
) -> Result<Option<TransientBody>> {
    open_transient_server_proof_observed(root, scratch_root, proof, check, None)
}
fn open_transient_server_proof_observed(
    root: &Path, scratch_root: &Path, proof: &RemoteObject, check: &dyn Fn() -> Result<()>,
    observer: Option<std::sync::Arc<dyn Fn(u64) + Send + Sync>>,
) -> Result<Option<TransientBody>> {
    check()?;
    let digest = proof.hash.as_str();
    validate_hash(digest)?;
    let _budget = lock_with_check(&TRANSFER_BUDGET, check)?;
    #[cfg(test)]
    if let Some(bytes) = test_remote::body(&std::fs::canonicalize(root)?, digest) {
        if bytes.len() > super::cache::SMALL_OBJECT_BYTES {
            return Err(SyncError::new("transient-fixture-body-too-large", 409));
        }
        if bytes.len() as u64 != proof.size {
            return Err(SyncError::new("hydration-size-mismatch", 409));
        }
        let directory = tempfile::Builder::new().prefix("asset-transient-").tempdir_in(scratch_root)?;
        let cache = super::cache::Cache::open(directory.path())?;
        check()?;
        if cache.put(&bytes)? != digest {
            return Err(SyncError::new("transfer-target-mismatch", 502));
        }
        cache.verify(digest, check)?;
        if cache.stat_object(digest)? != Some(proof.size) {
            return Err(SyncError::new("hydration-size-mismatch", 409));
        }
        let body = cache.open_object(digest)?.ok_or_else(|| SyncError::new("hydration-incomplete", 502))?;
        check()?;
        return Ok(Some(TransientBody { body, _directory: Some(directory) }));
    }
    let mut client = super::client::ServerClient::new(proof.config.resolve(root)?)?;
    #[cfg(test)]
    super::client::attach_test_io(root, &mut client);
    client.resolve_identity()?;
    client.body_download_observer = observer;
    check()?;
    let directory = tempfile::Builder::new().prefix("asset-transient-").tempdir_in(scratch_root)?;
    let cache = super::cache::Cache::open(directory.path())?;
    super::transfer::Transfer::new(&client, &cache)?.with_check(check)
        .download(&[digest.to_owned()], &[])?;
    cache.verify(digest, check)?;
    if cache.stat_object(digest)? != Some(proof.size) {
        return Err(SyncError::new("hydration-size-mismatch", 409));
    }
    let body = cache.open_object(digest)?.ok_or_else(|| SyncError::new("hydration-incomplete", 502))?;
    check()?;
    Ok(Some(TransientBody { body, _directory: Some(directory) }))
}

pub(crate) struct HydrationSession {
    root: PathBuf,
    residency: Option<Residency>,
    clients: std::collections::BTreeMap<String, super::client::ServerClient>,
    cache: Option<super::cache::Cache>,
    directory: Option<tempfile::TempDir>,
    cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    catalog: Option<AssetObjectRegistrar>,
    /// Whether downloads count as the planned work of the current progress lane.
    planned: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HydrationOutcome { AlreadyLocal, Downloaded }
impl HydrationSession {
    pub(crate) fn new(root: &Path, cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) -> Result<Self> {
        Ok(Self { root: std::fs::canonicalize(root)?, residency: None, clients: Default::default(), cache: None, directory: None, cancellation, catalog: None, planned: true })
    }
    pub(crate) fn open(&mut self, digest: &str, check: &dyn Fn() -> Result<()>) -> Result<Option<std::fs::File>> {
        check()?;
        let cas = crate::asset_repository::PayloadCas::new(&self.root)?;
        // A body already here is read as it is, without a catalog write per open.
        if let Some(file) = cas.open_object(digest)? { return Ok(Some(file)); }
        if !self.hydrate_many(&[digest.to_owned()], check)?.is_empty() { return Ok(None); }
        check()?;
        Ok(cas.open_object(digest)?)
    }
    pub(crate) fn hydrate_many(&mut self, digests: &[String], check: &dyn Fn() -> Result<()>) -> Result<Vec<String>> {
        self.hydrate_many_outcomes(digests, check, |_, _| {})
    }
    pub(crate) fn hydrate_many_observed(&mut self, digests: &[String], check: &dyn Fn() -> Result<()>, on_object_done: &dyn Fn()) -> Result<Vec<String>> {
        self.hydrate_many_outcomes(digests, check, |_, _| on_object_done())
    }
    /// Hydrates what the plan is read from. The work found in it is planned only afterwards,
    /// so these downloads are not counted as part of it.
    pub(crate) fn hydrate_many_before_plan(&mut self, digests: &[String], check: &dyn Fn() -> Result<()>, on_object_done: &dyn Fn()) -> Result<Vec<String>> {
        self.planned = false;
        let outcome = self.hydrate_many_observed(digests, check, on_object_done);
        self.planned = true;
        outcome
    }
    pub(crate) fn hydrate_many_outcomes(&mut self, digests: &[String], check: &dyn Fn() -> Result<()>, opened: impl FnMut(&str, HydrationOutcome)) -> Result<Vec<String>> {
        self.hydrate_many_outcomes_prioritized(digests, &Default::default(), check, opened)
    }
    pub(crate) fn hydrate_many_outcomes_prioritized(&mut self, digests: &[String], priority: &std::collections::BTreeSet<String>, check: &dyn Fn() -> Result<()>, mut opened: impl FnMut(&str, HydrationOutcome)) -> Result<Vec<String>> {
        let mut unavailable = Vec::new();
        let mut external = std::collections::BTreeSet::new();
        let mut server_pages = Vec::new();
        let cas = crate::asset_repository::PayloadCas::new(&self.root)?;
        let lane = super::progress::current().filter(|_| self.planned);
        let mut seen = std::collections::BTreeSet::new();
        let digests = digests.iter().filter(|hash| seen.insert((*hash).clone())).cloned().collect::<Vec<_>>();
        let scope = lane.as_ref().map(|lane| lane.asset_plan(Some(digests.len())));
        // Finding what is already here comes before the downloads this scope plans below.
        if let Some(lane) = &lane { lane.step(super::progress::Step::Preparing); }
        let mut completed = std::collections::BTreeSet::new();
        let mut opened = |hash: &str, outcome| {
            if completed.insert(hash.to_owned()) {
                if let (Some(lane), Some(scope)) = (&lane, scope) { lane.asset_done(scope); }
                opened(hash, outcome);
            }
        };
        for page in digests.chunks(64) {
            check()?;
            let hashes = page.iter().collect::<std::collections::BTreeSet<_>>();
            let locks = hashes.iter().map(|hash| hydration_lock(&self.root, hash)).collect::<Result<Vec<_>>>()?;
            let guards = locks.iter().map(|lock| lock_with_check(lock, check)).collect::<Result<Vec<_>>>()?;
            for hash in &hashes {
                check()?;
                validate_hash(hash)?;
            }
            if self.residency.is_none() && Residency::exists(&self.root) { self.residency = Some(Residency::open(&self.root)?); }
            let mut local = Vec::new();
            let mut objects = Vec::new();
            {
                // A row goes in before its body's file and stands on the custody that supplies it
                // until the file arrives. A release drops, under this lock, the rows of bodies it
                // leaves without a file, so custody is read under it too.
                let _guard = crate::asset_repository::coordinator::lock_repository_mutation()?;
                let mut rows = Vec::with_capacity(hashes.len());
                let sizes = cas.stat_objects(page.iter().map(String::as_str))?;
                for (hash, size) in page.iter().zip(sizes) {
                    if let Some(size) = size {
                        rows.push(AssetObjectRegistration { object_hash: hash.clone(), byte_size: size });
                        local.push(hash);
                        continue;
                    }
                    let Some(proof) = self.residency.as_ref().map(|residency| residency.object(hash, None)).transpose()?.flatten() else {
                        external.insert(hash.clone());
                        continue;
                    };
                    rows.push(AssetObjectRegistration { object_hash: hash.clone(), byte_size: proof.size });
                    objects.push(proof);
                }
                register(&mut self.catalog, &self.root, &rows)?;
            }
            for hash in local {
                opened(hash, HydrationOutcome::AlreadyLocal);
            }
            server_pages.push(objects);
            drop(guards);
            std::thread::yield_now();
        }
        let external = digests.iter().filter(|hash| external.remove(hash.as_str())).cloned().collect::<Vec<_>>();
        // Every server body's size is known before the first download, so its byte total is planned
        // once, on the lane the downloading clients report to.
        let planned_bytes = server_pages.iter().flatten().map(|object| object.size).sum::<u64>();
        if let Some(lane) = super::progress::current().filter(|_| planned_bytes > 0) { lane.plan_files(0, planned_bytes); }
        let positions = digests.iter().enumerate().map(|(index, hash)| (hash.as_str(), index)).collect::<std::collections::BTreeMap<_, _>>();
        let rank = |hash: &str| (!priority.contains(hash), positions.get(hash).copied().unwrap_or(usize::MAX));
        let mut server_groups = Vec::new();
        for selected in [true, false] {
            for page in &server_pages {
                let mut group: Vec<&RemoteObject> = Vec::new();
                let mut previous = String::new();
                for object in page.iter().filter(|object| priority.contains(&object.hash) == selected) {
                    let key = format!("{}:{}", object.context, serde_json::to_string(&object.config).map_err(|_| SyncError::new("invalid-server-config", 409))?);
                    if !group.is_empty() && previous != key { server_groups.push(std::mem::take(&mut group)); }
                    previous = key;
                    group.push(object);
                }
                if !group.is_empty() { server_groups.push(group); }
            }
        }
        server_groups.sort_by_key(|group| rank(&group[0].hash));
        let mut server_groups = server_groups.into_iter().peekable();
        let root = self.root.clone();
        let cancellation = self.cancellation.clone();
        if !external.is_empty() {
            unavailable.extend(crate::external_storage::lww_residency::hydrate_registered_many_scheduled(
                &root, &external, priority, cancellation, check, &mut opened,
                &mut |first, opened| {
                    while server_groups.peek().is_some_and(|group| rank(&group[0].hash) < rank(first)) {
                        self.hydrate_server_objects(&server_groups.next().unwrap(), check, opened)?;
                    }
                    Ok(())
                },
            )?);
        }
        for group in server_groups { self.hydrate_server_objects(&group, check, &mut opened)?; }
        check()?;
        if unavailable.is_empty() {
            if let (Some(lane), Some(scope)) = (&lane, scope) { lane.asset_settled(scope); }
        }
        Ok(unavailable)
    }

    fn hydrate_server_objects(&mut self, objects: &[&RemoteObject], check: &dyn Fn() -> Result<()>, opened: &mut dyn FnMut(&str, HydrationOutcome)) -> Result<()> {
        let cas = crate::asset_repository::PayloadCas::new(&self.root)?;
        check()?;
        let hashes = objects.iter().map(|object| &object.hash).collect::<std::collections::BTreeSet<_>>();
        let locks = hashes.iter().map(|hash| hydration_lock(&self.root, hash)).collect::<Result<Vec<_>>>()?;
        let guards = locks.iter().map(|lock| lock_with_check(lock, check)).collect::<Result<Vec<_>>>()?;
        let mut groups = Vec::<(String, Vec<RemoteObject>)>::new();
        for proof in objects {
            let hash = &proof.hash;
            check()?;
            {
                let _guard = crate::asset_repository::coordinator::lock_repository_mutation()?;
                if cas.stat_object(hash)?.is_some() {
                    opened(hash, HydrationOutcome::AlreadyLocal);
                    continue;
                }
            }
            #[cfg(test)]
            if let Some(body) = test_remote::body(&self.root, hash) {
                let staged = super::transfer::stage_checked(&cas, &mut body.as_slice(), hash, proof.size, check)?;
                check()?;
                let _guard = crate::asset_repository::coordinator::lock_repository_mutation()?;
                check()?;
                let outcome = publish_registered(&cas, &mut self.catalog, &self.root, staged);
                check()?;
                outcome?;
                opened(hash, HydrationOutcome::Downloaded);
                continue;
            }
            let key = format!("{}:{}", proof.context, serde_json::to_string(&proof.config)
                .map_err(|_| SyncError::new("invalid-server-config", 409))?);
            if let Some((_, batch)) = groups.last_mut().filter(|(previous, _)| *previous == key) {
                batch.push((*proof).clone());
            } else { groups.push((key, vec![(*proof).clone()])); }
        }
        for (key, objects) in groups {
            let _budget = lock_with_check(&TRANSFER_BUDGET, check)?;
            if !self.clients.contains_key(&key) {
                let client = super::client::ServerClient::with_cancellation(objects[0].config.resolve(&self.root)?, self.cancellation.clone())?;
                #[cfg(test)]
                let client = { let mut client = client; super::client::attach_test_io(&self.root, &mut client); client };
                client.resolve_identity()?;
                self.clients.insert(key.clone(), client);
            }
            if self.cache.is_none() {
                let directory = tempfile::Builder::new().prefix("asset-hydration-").tempdir_in(&self.root)?;
                self.cache = Some(super::cache::Cache::open(directory.path())?);
                self.directory = Some(directory);
            }
            let cache = self.cache.as_ref().unwrap();
            let client = self.clients.get(&key).unwrap();
            super::transfer::Transfer::new(client, cache)?.with_check(check)
                .download(&objects.iter().map(|object| object.hash.clone()).collect::<Vec<_>>(), &[])?;
            let mut remaining = objects.as_slice();
            while !remaining.is_empty() {
                let count = crate::asset_repository::parallel_stage::batch_len(remaining.iter().map(|object| object.size));
                let (batch, rest) = remaining.split_at(count);
                remaining = rest;
                let staged = crate::asset_repository::parallel_stage::stage_batch(batch, check, &|object, stopped| {
                    let mut body = cache.open_derived(&object.hash)?.ok_or_else(|| SyncError::new("hydration-incomplete", 502))?;
                    super::transfer::stage_checked(&cas, &mut body, &object.hash, object.size, &|| {
                        if stopped.load(std::sync::atomic::Ordering::Acquire) { Err(SyncError::new("cancelled", 409)) }
                        else { Ok(()) }
                    })
                });
                check()?;
                for (object, staged) in batch.iter().zip(staged?) {
                    check()?;
                    {
                        let _guard = crate::asset_repository::coordinator::lock_repository_mutation()?;
                        check()?;
                        let outcome = publish_registered(&cas, &mut self.catalog, &self.root, staged);
                        check()?;
                        outcome?;
                        opened(&object.hash, HydrationOutcome::Downloaded);
                    }
                    cache.remove_derived(&object.hash)?;
                }
            }
        }
        drop(guards);
        std::thread::yield_now();
        Ok(())
    }
}

/// Registers the catalog rows not already in place, through the one catalog
/// connection a session keeps.
fn register(catalog: &mut Option<AssetObjectRegistrar>, root: &Path, rows: &[AssetObjectRegistration]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    if catalog.is_none() {
        *catalog = Some(AssetObjectRegistrar::open(root)?);
    }
    let created_at_ms = i64::try_from(crate::external_storage::runtime::now_ms())
        .map_err(|_| SyncError::new("clock-overflow", 409))?;
    match catalog.as_mut() {
        Some(catalog) => Ok(catalog.register_missing(rows, created_at_ms)?),
        None => Ok(()),
    }
}

/// Publishes a staged body and registers it again in the same hold, so a row a
/// release took while the body was fetched is back before anything else runs.
/// Called under the repository mutation lock.
fn publish_registered(
    cas: &crate::asset_repository::PayloadCas,
    catalog: &mut Option<AssetObjectRegistrar>,
    root: &Path,
    staged: crate::asset_repository::StagedPayload,
) -> Result<()> {
    let published = cas.publish_staged(staged)?;
    register(catalog, root, &[AssetObjectRegistration { object_hash: published.content_hash, byte_size: published.byte_size }])
}

pub(crate) fn with_hydration_lock<T>(
    root: &Path,
    digest: &str,
    check: &dyn Fn() -> Result<()>,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let lock = hydration_lock(root, digest)?;
    let _guard = lock_with_check(&lock, check)?;
    operation()
}

fn hydration_lock(root: &Path, digest: &str) -> Result<std::sync::Arc<std::sync::Mutex<()>>> {
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    static IN_FLIGHT: OnceLock<Mutex<std::collections::HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    validate_hash(digest)?;
    let mut locks = IN_FLIGHT.get_or_init(Default::default).lock()
        .map_err(|_| SyncError::new("hydration-unavailable", 503))?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    let key = root.join(digest);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) { return Ok(lock); }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    Ok(lock)
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
