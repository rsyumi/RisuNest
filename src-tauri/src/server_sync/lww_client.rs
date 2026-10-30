use super::{
    cache::Cache,
    client::{response_code, response_error, RequestAttempt, ServerClient, ServerConfig},
    library_format::{LibraryFormat, LEVEL},
    progress::Step,
    transfer::Transfer,
    Result, SyncError,
};
#[cfg(test)]
use crate::persistent_store::lww::ApplyReceive;
use crate::persistent_store::{
    lww::{
        AckEntry, ApplyResult, Change, Header, MessageLocator, OutboxEntry, Progress, StageReceive,
    },
    PersistentStore,
};
use risunest_sync_wire::{
    canonical,
    library_format::{self, EMPTY_LIBRARY, PRECONDITION_HEADER},
    lww::{
        AckRequest, CancelOperationRequest, ChangesPage, OperationReceipt, PushReceipt,
        PushRequest, StatePage, StatePin, TimeSample, UnitChange,
    },
    stamp::{AdmittedClock, ClockSample, DecimalU64},
    unit::UnitValue,
    MAX_METADATA_BYTES,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Publication {
    pub authority: DecimalU64,
    pub request: PushRequest,
    pub entries: Vec<OutboxEntry>,
    pub config: super::credentials::StoredConfig,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReceivePage {
    #[serde(flatten)]
    pub(crate) page: StageReceive,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) receive_frontier: Option<DecimalU64>,
    #[serde(default = "receive_page_more")]
    pub(crate) more: bool,
}
fn receive_page_more() -> bool { true }

pub(crate) struct OperationLog(pub(crate) Connection);
impl OperationLog {
    pub(crate) fn path(root: &Path) -> std::path::PathBuf {
        root.join("server-sync/lww-operations.sqlite")
    }
    pub(crate) fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root.join("server-sync"))?;
        let db = crate::sqlite_open::open(Self::path(root))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS publications(id TEXT PRIMARY KEY, digest TEXT NOT NULL, body BLOB NOT NULL, intent TEXT NOT NULL, receipt TEXT,acknowledged INTEGER NOT NULL DEFAULT 0); CREATE INDEX IF NOT EXISTS pending_publications ON publications(id) WHERE receipt IS NULL; CREATE INDEX IF NOT EXISTS unacknowledged_publications ON publications(id) WHERE acknowledged=0;")?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS bootstrap(authority TEXT PRIMARY KEY,pin TEXT NOT NULL,after_key TEXT,cursor TEXT NOT NULL); CREATE TABLE IF NOT EXISTS receive_pages(authority TEXT PRIMARY KEY,body TEXT NOT NULL,next_key TEXT,bootstrap INTEGER NOT NULL,done INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS configurations(slot TEXT PRIMARY KEY,config TEXT NOT NULL); CREATE TABLE IF NOT EXISTS bindings(id TEXT PRIMARY KEY,body TEXT NOT NULL); CREATE TABLE IF NOT EXISTS claims(id TEXT PRIMARY KEY,body TEXT NOT NULL);")?;
        Ok(Self(db))
    }
    pub(crate) fn prepare(&self, publication: &Publication) -> Result<Vec<u8>> {
        let bytes = canonical::encode(&publication.request)?;
        let digest = operation_digest(&publication.request)?;
        let old: Option<(String, Vec<u8>)> = self
            .0
            .query_row(
                "SELECT digest,body FROM publications WHERE id=?1",
                [&publication.request.operation_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if old.is_some_and(|old| old != (digest.clone(), bytes.clone())) {
            return Err(SyncError::new("operation-integrity", 409));
        }
        self.0.execute(
            "INSERT OR IGNORE INTO publications(id,digest,body,intent) VALUES(?1,?2,?3,?4)",
            params![
                publication.request.operation_id,
                digest,
                bytes,
                serde_json::to_string(publication)
                    .map_err(|_| SyncError::new("publication-encoding", 409))?
            ],
        )?;
        Ok(bytes)
    }
    pub(crate) fn pending(&self) -> Result<Vec<Publication>> {
        let mut query = self
            .0
            .prepare("SELECT intent FROM publications WHERE receipt IS NULL ORDER BY rowid")?;
        let rows = query.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| {
            serde_json::from_str(&r?).map_err(|_| SyncError::new("publication-integrity", 409))
        })
        .collect()
    }
    fn unacknowledged(&self) -> Result<Vec<(Publication, OperationReceipt)>> {
        let mut query = self.0.prepare(
            "SELECT intent,receipt FROM publications WHERE acknowledged=0 AND receipt IS NOT NULL",
        )?;
        let rows = query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (intent, receipt) = row?;
            Ok((
                serde_json::from_str(&intent)
                    .map_err(|_| SyncError::new("publication-integrity", 409))?,
                serde_json::from_str(&receipt)
                    .map_err(|_| SyncError::new("publication-integrity", 409))?,
            ))
        })
        .collect()
    }
    pub(crate) fn save_config(
        &self,
        slot: &str,
        config: &super::credentials::StoredConfig,
    ) -> Result<()> {
        self.0.execute("INSERT INTO configurations VALUES(?1,?2) ON CONFLICT(slot) DO UPDATE SET config=excluded.config",params![slot,serde_json::to_string(config).map_err(|_|SyncError::new("configuration-integrity",409))?])?;
        Ok(())
    }
    pub(crate) fn config(&self, slot: &str) -> Result<Option<super::credentials::StoredConfig>> {
        let value: Option<String> = self
            .0
            .query_row(
                "SELECT config FROM configurations WHERE slot=?1",
                [slot],
                |r| r.get(0),
            )
            .optional()?;
        value
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|_| SyncError::new("configuration-integrity", 409))
            })
            .transpose()
    }
    pub(crate) fn save_verified<T: Serialize>(
        &self,
        table: &str,
        id: &str,
        value: &T,
    ) -> Result<()> {
        if !matches!(table, "bindings" | "claims") {
            return Err(SyncError::new("operation-integrity", 409));
        }
        let body =
            serde_json::to_string(value).map_err(|_| SyncError::new("operation-integrity", 409))?;
        let old: Option<String> = self
            .0
            .query_row(
                &format!("SELECT body FROM {table} WHERE id=?1"),
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if old.as_ref().is_some_and(|old| old != &body) {
            return Err(SyncError::new("operation-integrity", 409));
        }
        self.0.execute(
            &format!("INSERT OR IGNORE INTO {table} VALUES(?1,?2)"),
            params![id, body],
        )?;
        Ok(())
    }
    pub(crate) fn verified<T: serde::de::DeserializeOwned>(
        &self,
        table: &str,
        id: &str,
    ) -> Result<T> {
        if !matches!(table, "bindings" | "claims") {
            return Err(SyncError::new("operation-integrity", 409));
        }
        let body: String = self.0.query_row(
            &format!("SELECT body FROM {table} WHERE id=?1"),
            [id],
            |r| r.get(0),
        )?;
        serde_json::from_str(&body).map_err(|_| SyncError::new("operation-integrity", 409))
    }
    /// A publication is read only until its acknowledgement is recorded, so a
    /// fence drops the ones acknowledged before it. Detached publications keep
    /// their unknown outcome.
    fn prune(&self) -> Result<()> {
        self.0.execute("DELETE FROM publications WHERE acknowledged=1 AND json_extract(receipt,'$.status') IN ('accepted','rejected')", [])?;
        Ok(())
    }
    fn finish(&self, publication: &Publication, receipt: &OperationReceipt) -> Result<()> {
        if receipt.body_digest() != operation_digest(&publication.request)? {
            return Err(SyncError::new("operation-integrity", 409));
        }
        self.0.execute(
            "UPDATE publications SET receipt=?1 WHERE id=?2 AND digest=?3",
            params![
                serde_json::to_string(receipt)
                    .map_err(|_| SyncError::new("publication-encoding", 409))?,
                publication.request.operation_id,
                receipt.body_digest()
            ],
        )?;
        Ok(())
    }
}
fn now_ms() -> Result<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| SyncError::new("clock-discontinuity", 409))?
            .as_millis(),
    )
    .map_err(|_| SyncError::new("clock-overflow", 409))
}
#[cfg(test)]
fn observe_dependency_purposes(key: &risunest_sync_wire::unit::UnitKey, hashes: &[String]) {
    use crate::asset_repository::body_io::{register_object_purpose, BodyPurpose};
    let purpose = match key.components()[0].as_str() {
        "messages" | "plugin-local" => BodyPurpose::Control,
        "archive" => BodyPurpose::Asset,
        _ => return,
    };
    for hash in hashes {
        register_object_purpose(hash, purpose);
    }
}
#[cfg(test)]
fn observe_descriptor_purposes(
    key: &risunest_sync_wire::unit::UnitKey,
    descriptor_hash: &str,
    descriptor: &risunest_sync_wire::descriptor::RecordDescriptor,
) {
    use crate::asset_repository::body_io::{register_object_purpose, BodyPurpose};
    register_object_purpose(descriptor_hash, BodyPurpose::Control);
    if matches!(key.components()[0].as_str(), "messages" | "archive")
        || crate::persistent_store::lww::lww_known_unit_key(key)
    {
        register_object_purpose(&descriptor.object_hash, BodyPurpose::Control);
    }
    observe_dependency_purposes(key, &descriptor.dependencies);
    for root in [&descriptor.dependency_root, &descriptor.relation_root]
        .into_iter()
        .flatten()
    {
        register_object_purpose(root, BodyPurpose::Control);
    }
}
/// Admitted clock samples by repository, address and library. A sample serves
/// every request until it expires or the server rejects a stamp.
type ClockKey = (std::path::PathBuf, String, String);
static CLOCKS: std::sync::Mutex<std::collections::BTreeMap<ClockKey, (Instant, AdmittedClock)>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());
/// Libraries seen at this client's format level, by address and library,
/// until a reply reports another level or comes from a server without levels.
static CURRENT_FORMAT: std::sync::Mutex<BTreeSet<(String, String)>> = std::sync::Mutex::new(BTreeSet::new());
/// Forgets a library seen at this client's level when a reply reports another
/// level, or succeeds without the level headers, so the next check reads it again.
pub(crate) fn observe_library_format(endpoint: &str, library_id: &str, status: u16, format: Option<LibraryFormat>) {
    let changed = match format {
        Some(format) => format.level != LEVEL,
        None => (200..300).contains(&status),
    };
    if changed {
        if let Ok(mut current) = CURRENT_FORMAT.lock() {
            current.remove(&(endpoint.to_owned(), library_id.to_owned()));
        }
    }
}
pub(crate) struct LwwClient {
    pub client: ServerClient,
    pub log: OperationLog,
    pub access: Option<super::credentials::StoredConfig>,
    root: std::path::PathBuf,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}
#[cfg(test)]
#[derive(serde::Serialize)]
pub(crate) struct NativeReceiveCompletion {
    pub result: ApplyResult,
    pub received_units: usize,
}
impl LwwClient {
    pub(crate) fn new(root: &Path, config: ServerConfig) -> Result<Self> {
        Self::with_cancellation(root, config, None)
    }
    pub(crate) fn with_cancellation(
        root: &Path,
        config: ServerConfig,
        cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Self> {
        let client = ServerClient::with_cancellation(config, cancelled.clone())?;
        #[cfg(test)]
        let client = { let mut client = client; super::client::attach_test_io(root, &mut client); client };
        Ok(Self {
            client,
            log: OperationLog::open(root)?,
            access: None,
            root: root.into(),
            cancelled,
        })
    }
    fn check(&self) -> Result<()> {
        if self
            .cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
        {
            Err(SyncError::new("cancelled", 409))
        } else {
            Ok(())
        }
    }
    fn clock_key(&self) -> ClockKey {
        let config = self.client.config();
        (self.root.clone(), config.endpoint, config.library_id)
    }
    fn forget_clock(&self) {
        if let Ok(mut clocks) = CLOCKS.lock() {
            clocks.remove(&self.clock_key());
        }
    }

    #[cfg(test)]
    pub(super) fn admit_synthetic_clock(&self, now_ms: u64) {
        let key = self.clock_key();
        let clock = ClockSample {
            target_id: key.2.clone(),
            request_wall_ms: now_ms,
            response_wall_ms: now_ms,
            request_monotonic_ms: 0,
            response_monotonic_ms: 0,
            target_ms: now_ms,
            precision_ms: 0,
            successful: true,
            cached_or_aged: false,
        }.admit().unwrap();
        CLOCKS.lock().unwrap().insert(key, (Instant::now(), clock));
    }
    /// The incoming bound from the library's current sample, sampling the
    /// server clock only when there is none or it expired.
    pub(crate) fn admission(&self) -> Result<DecimalU64> {
        let key = self.clock_key();
        let cached = CLOCKS.lock().ok().and_then(|clocks| clocks.get(&key).cloned());
        if let Some((sampled, clock)) = cached {
            let age = u64::try_from(sampled.elapsed().as_millis())
                .map_err(|_| SyncError::new("clock-overflow", 409))?;
            if let Ok(upper) = clock.incoming_upper_ms(&key.2, age) {
                return Ok(upper.into());
            }
        }
        self.fresh_admission()
    }
    /// Samples the server clock and replaces the library's sample with it.
    pub(crate) fn fresh_admission(&self) -> Result<DecimalU64> {
        self.forget_clock();
        let sent_wall = now_ms()?;
        let sent = Instant::now();
        let (_, sample): (_, TimeSample) =
            self.client
                .json(reqwest::Method::GET, "time", &[], None::<&AckRequest>, &[])?;
        let elapsed = u64::try_from(sent.elapsed().as_millis())
            .map_err(|_| SyncError::new("clock-overflow", 409))?;
        let target = self.client.config().library_id;
        let admitted = ClockSample {
            target_id: target.clone(),
            request_wall_ms: sent_wall,
            response_wall_ms: now_ms()?,
            request_monotonic_ms: 0,
            response_monotonic_ms: elapsed,
            target_ms: sample.server_time_ms.0,
            precision_ms: sample.precision_ms.0,
            successful: true,
            cached_or_aged: false,
        }
        .admit()?;
        let upper = admitted.incoming_upper_ms(&target, elapsed)?;
        if let Ok(mut clocks) = CLOCKS.lock() {
            clocks.insert(self.clock_key(), (sent, admitted));
        }
        Ok(upper.into())
    }
    fn format_key(&self) -> (String, String) {
        let config = self.client.config();
        (config.endpoint, config.library_id)
    }
    /// Fails unless the library is at this client's format level. An empty
    /// library is switched to it, and with `switch` any library below it is.
    pub(crate) fn require_library_format(&self, store: &mut PersistentStore, switch: bool) -> Result<()> {
        if !switch && CURRENT_FORMAT.lock().is_ok_and(|current| current.contains(&self.format_key())) {
            return Ok(());
        }
        let (head, format) = self.client.head_with_format()?;
        if let Some(format) = format {
            if format.level < LEVEL && format.max >= LEVEL && (switch || head.seq.as_str() == "0") {
                return self.push_library_format(store, !switch);
            }
        }
        self.accept_library_format(format)
    }
    /// The format check of a bound device, outside a send or receive.
    pub(crate) fn check_library_format(&self, store: &mut PersistentStore, header: &Header, switch: bool) -> Result<()> {
        self.check()?;
        if store.lww_binding_authority()? != header.binding_authority {
            return Err(SyncError::new("binding-authority-changed", 409));
        }
        self.require_library_format(store, switch)
    }
    fn accept_library_format(&self, format: Option<LibraryFormat>) -> Result<()> {
        let code = match format {
            None => "sync-server-update-required",
            Some(format) if format.level == LEVEL => {
                if let Ok(mut current) = CURRENT_FORMAT.lock() {
                    current.insert(self.format_key());
                }
                return Ok(());
            }
            Some(format) if format.level > LEVEL => "app-update-required",
            Some(format) if format.max < LEVEL => "sync-server-update-required",
            Some(_) => "library-format-switch-required",
        };
        Err(SyncError::new(code, 409))
    }
    /// Pushes this client's level alone, with `empty_only` only into a library
    /// that holds nothing else. A push whose changes all lose still succeeds,
    /// so only the level its reply reports confirms the switch.
    fn push_library_format(&self, store: &mut PersistentStore, empty_only: bool) -> Result<()> {
        let stamp = store.lww_reserve_stamp()?;
        let request = PushRequest {
            library_id: self.client.config().library_id,
            writer_id: stamp.writer_id.clone(),
            operation_id: uuid::Uuid::new_v4().to_string(),
            changes: vec![UnitChange { key: library_format::key(), stamp, value: library_format::value(LEVEL)? }],
        };
        let headers = if empty_only { vec![(PRECONDITION_HEADER, EMPTY_LIBRARY.to_owned())] } else { Vec::new() };
        match self.client.request_ambiguous_mutation(
            reqwest::Method::POST,
            "push",
            Some(canonical::encode(&request)?),
            &headers,
            MAX_METADATA_BYTES,
        )? {
            RequestAttempt::Response(reply)
                if (200..300).contains(&reply.status)
                    || (empty_only && response_code(&reply).as_deref() == Some("library-not-empty")) =>
            {
                self.accept_library_format(reply.library_format)
            }
            RequestAttempt::Response(reply) => Err(response_error(reply)),
            RequestAttempt::Failure { error, .. } => Err(error),
        }
    }
    /// The client for the registration that sent a publication. The current
    /// address asks for it when that registration is the one in use.
    fn registration_client(&self, publication: &Publication) -> Result<Option<ServerClient>> {
        let current = self.client.config();
        if publication.config.library_id == current.library_id
            && publication.config.device_id == current.device_id
        {
            return Ok(None);
        }
        #[allow(unused_mut)]
        let mut client = ServerClient::with_cancellation(
            publication.config.resolve(&self.root)?,
            self.cancelled.clone(),
        )?;
        #[cfg(test)]
        {
            client.test_io = self.client.test_io.clone();
        }
        Ok(Some(client))
    }
    /// Whether the server that holds a publication answers for it. An address
    /// that cannot be reached or no longer serves this library does not, and
    /// neither does a registration whose credential this device cannot read.
    pub(crate) fn answers(&self, publication: &Publication) -> Result<bool> {
        let resolved = match self.registration_client(publication) {
            Err(error) if error.code == "device-credential-unavailable" => return Ok(false),
            resolved => resolved?,
        };
        let attempt = resolved.as_ref().unwrap_or(&self.client).request_ambiguous_mutation(
            reqwest::Method::GET,
            &format!("operations/{}", publication.request.operation_id),
            None,
            &[],
            MAX_METADATA_BYTES,
        )?;
        Ok(match attempt {
            super::client::RequestAttempt::Response(reply) => match reply.status {
                404 => super::client::response_code(&reply).as_deref() == Some("operation-not-found"),
                502..=504 => false,
                _ => true,
            },
            super::client::RequestAttempt::Failure { error, .. } => {
                !super::client::is_ambiguous_transient(&error) && error.code != "directory-unreachable"
            }
        })
    }
    pub(crate) fn settle(&self, publication: &Publication) -> Result<OperationReceipt> {
        let resolved = self.registration_client(publication)?;
        let original = resolved.as_ref().unwrap_or(&self.client);
        let path = format!("operations/{}", publication.request.operation_id);
        let reply = original.request(
            reqwest::Method::GET,
            &path,
            &[],
            None,
            &[],
            MAX_METADATA_BYTES,
        )?;
        if reply.status == 404 && super::client::response_code(&reply).as_deref() != Some("operation-not-found") {
            return Err(SyncError::new("server-unreachable", 503));
        }
        let receipt = if reply.status == 404 {
            let (_, receipt) = original.json(
                reqwest::Method::POST,
                &format!("{path}/cancel"),
                &[],
                Some(&CancelOperationRequest {
                    body_digest: operation_digest(&publication.request)?,
                }),
                &[],
            )?;
            receipt
        } else if (200..300).contains(&reply.status) {
            canonical::decode(&reply.body, MAX_METADATA_BYTES)?
        } else {
            return Err(response_error(reply));
        };
        self.log.finish(publication, &receipt)?;
        Ok(receipt)
    }
    fn acknowledge(
        &self,
        store: &mut PersistentStore,
        publication: &Publication,
        receipt: &OperationReceipt,
    ) -> Result<()> {
        self.check()?;
        if let OperationReceipt::Accepted { receipt, .. } = receipt {
            if receipt.operation_id != publication.request.operation_id {
                return Err(SyncError::new("operation-integrity", 409));
            }
            if store.lww_binding_authority()? == publication.authority {
                let entries = publication
                    .entries
                    .iter()
                    .map(|e| {
                        Ok(AckEntry {
                            key: e.key.clone(),
                            version: e.version.clone(),
                            stamp: e.stamp.clone(),
                            value_identity: wire_identity(&e.value)?,
                        })
                    })
                    .collect::<risunest_sync_wire::Result<Vec<_>>>()?;
                store.lww_ack_outbox(
                    &Header {
                        binding_authority: publication.authority,
                        request_id: publication.request.operation_id.clone(),
                    },
                    &entries,
                )?;
            }
        }
        self.log.0.execute(
            "UPDATE publications SET acknowledged=1 WHERE id=?1",
            [&publication.request.operation_id],
        )?;
        Ok(())
    }
    pub(crate) fn fence(&self, store: &mut PersistentStore) -> Result<()> {
        self.log.prune()?;
        for publication in self.log.pending()? {
            let receipt = self.settle(&publication)?;
            self.acknowledge(store, &publication, &receipt)?;
        }
        for (publication, receipt) in self.log.unacknowledged()? {
            self.acknowledge(store, &publication, &receipt)?;
        }
        Ok(())
    }
    pub(crate) fn fence_new_device(&self, store: &mut PersistentStore) -> Result<()> {
        self.log.prune()?;
        for publication in self.log.pending()? {
            if !self.answers(&publication)? {
                continue;
            }
            match self.settle(&publication) {
                Ok(receipt) => self.acknowledge(store, &publication, &receipt)?,
                Err(error) if error.status == 401 => {}
                Err(error) => return Err(error),
            }
        }
        for (publication, receipt) in self.log.unacknowledged()? {
            self.acknowledge(store, &publication, &receipt)?;
        }
        Ok(())
    }
    pub(crate) fn detach_inactive(
        &self,
        former: &super::credentials::StoredConfig,
        authorization_id: &str,
    ) -> Result<()> {
        let tx = self.log.0.unchecked_transaction()?;
        for publication in self.log.pending()? {
            if publication.config.library_id != former.library_id
                || publication.config.device_id != former.device_id
            {
                return Err(SyncError::new("publication-unsettled", 409));
            }
            tx.execute("UPDATE publications SET receipt=?1,acknowledged=1 WHERE id=?2 AND receipt IS NULL",params![serde_json::json!({"kind":"auth-inactive-detached","authorizationId":authorization_id,"bodyDigest":operation_digest(&publication.request)?,"historicalAcceptance":"unknown"}).to_string(),publication.request.operation_id])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn push(
        &self,
        store: &mut PersistentStore,
        header: &Header,
        generating: &[MessageLocator],
    ) -> Result<Option<PushReceipt>> {
        let cache = lane_cache(store.repository_root(), "send");
        let result = self.push_through(store, header, generating, &cache);
        if result.is_ok() {
            discard_cache(&cache);
        }
        result
    }
    fn push_through(
        &self,
        store: &mut PersistentStore,
        header: &Header,
        generating: &[MessageLocator],
        cache_root: &Path,
    ) -> Result<Option<PushReceipt>> {
        self.fence(store)?;
        let lane = self.client.lane();
        lane.step(Step::Preparing);
        let mut entries = store
            .lww_read_outbox_generating(header.binding_authority, 256, generating)?
            .entries;
        if entries.is_empty() {
            return Ok(None);
        }
        self.require_library_format(store, false)?;
        let mut admitted = self.admission()?;
        if entries
            .iter()
            .any(|entry| entry.stamp.physical_ms > admitted)
        {
            self.retry_unpublished(store, header)?;
            entries = store
                .lww_read_outbox_generating(header.binding_authority, 256, generating)?
                .entries;
            admitted = self.fresh_admission()?;
            if entries
                .iter()
                .any(|entry| entry.stamp.physical_ms > admitted)
            {
                return Err(SyncError::new("clock-skew", 409));
            }
        }
        let mut request = PushRequest {
            library_id: self.client.config().library_id,
            writer_id: store.lww_clock_state()?.writer_id,
            operation_id: uuid::Uuid::new_v4().to_string(),
            changes: Vec::new(),
        };
        fit_push_page(&mut request, &mut entries)?;
        lane.plan_items(entries.len());
        let cache = Cache::open(cache_root)?.with_library(store.repository_root())?;
        let prepared = cache.begin_batch()?;
        let mut objects = BTreeSet::new();
        for entry in &entries {
            self.check()?;
            if let UnitValue::Object {
                descriptor_hash,
                descriptor,
            } = &entry.value
            {
                #[cfg(test)]
                observe_descriptor_purposes(&entry.key, descriptor_hash, descriptor);
                let bytes = canonical::encode(descriptor)?;
                if cache.put(&bytes)? != *descriptor_hash {
                    return Err(SyncError::new("descriptor-integrity", 409));
                }
                objects.insert(descriptor_hash.clone());
                objects.insert(descriptor.object_hash.clone());
                objects.extend(descriptor.dependencies.iter().cloned());
                for root in [&descriptor.dependency_root, &descriptor.relation_root]
                    .into_iter()
                    .flatten()
                {
                    let mut pending = vec![root.clone()];
                    while let Some(hash) = pending.pop() {
                        #[cfg(test)]
                        crate::asset_repository::body_io::register_object_purpose(
                            &hash,
                            crate::asset_repository::body_io::BodyPurpose::Control,
                        );
                        if !objects.insert(hash.clone()) {
                            continue;
                        }
                        if let Some(body) = store.lww_object_body(&hash)? {
                            cache.put(&body)?;
                        }
                        let page: risunest_sync_wire::descriptor::ReferencePage =
                            canonical::decode(
                                &cache.read(&hash, MAX_METADATA_BYTES)?,
                                MAX_METADATA_BYTES,
                            )?;
                        page.validate()?;
                        match page {
                            risunest_sync_wire::descriptor::ReferencePage::Branches {
                                children,
                            } => pending.extend(children),
                            risunest_sync_wire::descriptor::ReferencePage::Objects { hashes } => {
                                #[cfg(test)]
                                observe_dependency_purposes(&entry.key, &hashes);
                                objects.extend(hashes)
                            }
                            risunest_sync_wire::descriptor::ReferencePage::Relations { .. } => (),
                        }
                    }
                }
            } else if matches!(entry.key.components()[0].as_str(), "asset" | "inlay") {
                if let UnitValue::Inline { bytes } = &entry.value {
                    use base64::Engine;
                    let value: serde_json::Value = serde_json::from_slice(
                        &base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(bytes)
                            .map_err(|_| SyncError::new("invalid-alias", 409))?,
                    )
                    .map_err(|_| SyncError::new("invalid-alias", 409))?;
                    if let Some(hash) = value.get("objectHash").and_then(|v| v.as_str()) {
                        risunest_sync_wire::validate_hash(hash)?;
                        #[cfg(test)]
                        crate::asset_repository::body_io::register_object_purpose(
                            hash,
                            crate::asset_repository::body_io::BodyPurpose::Asset,
                        );
                        objects.insert(hash.into());
                    }
                }
            }
        }
        let check = || self.check();
        let mut previous = super::previous_storage::PreviousStorage::new(
            store.repository_root(),
            &check,
            self.cancelled.clone(),
        );
        for hash in &objects {
            if cache.stat_object(hash)?.is_none()
                && !store.lww_copy_large_object_to_cas(hash, &cache.cas, super::cache::SMALL_OBJECT_BYTES)?
            {
                if let Some(body) = store.lww_object_body(hash)? {
                    cache.put(&body)?;
                } else {
                    previous.observe(store, hash)?;
                }
            }
        }
        prepared.commit()?;
        let mut hints = BTreeMap::new();
        for entry in &entries {
            if let UnitValue::Object { descriptor, .. } = &entry.value {
                if large_unit(&entry.key) {
                    let target = &descriptor.object_hash;
                    let bases = delta_bases(store, &cache, &entry.key, target, header.binding_authority)?;
                    if !bases.is_empty() {
                        hints.insert(target.clone(), bases);
                    }
                }
            }
        }
        Transfer::new(&self.client, &cache)?
            .with_held(&previous)
            .upload_with_hints(&objects.into_iter().collect::<Vec<_>>(), &[], false, &hints)?;
        let config = self
            .access
            .clone()
            .or(store.server_stored_config()?)
            .ok_or_else(|| SyncError::new("server-unconfigured", 409))?;
        previous.retain(&self.client, &config)?;
        let publication = Publication {
            authority: header.binding_authority,
            request,
            entries,
            config,
        };
        let body = self.log.prepare(&publication)?;
        self.check()?;
        if store.lww_binding_authority()? != header.binding_authority {
            return Err(SyncError::new("binding-authority-changed", 409));
        }
        lane.step(Step::Confirming);
        let reply = self.client.request_ambiguous_mutation(
            reqwest::Method::POST,
            "push",
            Some(body),
            &[],
            MAX_METADATA_BYTES,
        );
        // A cancelled publication fails as its request did, so a busy or
        // unreachable server stays retryable and a refusal does not.
        let failure = match &reply {
            Ok(super::client::RequestAttempt::Response(reply))
                if reply.status == 413 && publication.request.changes.len() == 1 =>
            {
                Some(SyncError::new("unit-too-large", 413))
            }
            Ok(super::client::RequestAttempt::Response(reply))
                if !(200..300).contains(&reply.status) =>
            {
                Some(SyncError::new(
                    super::client::response_code(reply)
                        .unwrap_or_else(|| "server-response-error".into()),
                    reply.status,
                ))
            }
            Ok(super::client::RequestAttempt::Failure { error, .. }) | Err(error) => {
                Some(SyncError::new(error.code.clone(), error.status))
            }
            _ => None,
        };
        let terminal = match reply {
            Ok(super::client::RequestAttempt::Response(reply))
                if (200..300).contains(&reply.status) =>
            {
                OperationReceipt::Accepted {
                    body_digest: operation_digest(&publication.request)?,
                    receipt: canonical::decode(&reply.body, MAX_METADATA_BYTES)?,
                }
            }
            _ => self.settle(&publication)?,
        };
        self.log.finish(&publication, &terminal)?;
        self.acknowledge(store, &publication, &terminal)?;
        match terminal {
            OperationReceipt::Accepted { receipt, .. } => {
                lane.items_done(publication.entries.len());
                Ok(Some(receipt))
            }
            OperationReceipt::Rejected { error, .. } if error == "operation-cancelled" => {
                Err(failure.unwrap_or_else(|| SyncError::new("server-unreachable", 503)))
            }
            OperationReceipt::Rejected { error, .. } => Err(SyncError::new(
                if error == "clock-skew" {
                    self.forget_clock();
                    error
                } else {
                    error
                },
                409,
            )),
        }
    }
    pub(crate) fn state(
        &self,
        store: &mut PersistentStore,
        upper: DecimalU64,
    ) -> Result<(DecimalU64, Vec<Change>)> {
        // Each attempt makes the server copy the whole library under a new pin.
        let mut attempts = 0;
        let lane = self.client.lane();
        loop {
            lane.step(Step::Listing);
            attempts += 1;
            let (_, pin): (_, StatePin) = self.client.json(
                reqwest::Method::POST,
                "state/pins",
                &[],
                None::<&AckRequest>,
                &[],
            )?;
            // The pin reports what it holds, so the listing has its total before the first page.
            lane.plan_listing(pin.unit_count.0);
            let result = (|| {
                let mut after = None;
                let mut changes = Vec::new();
                loop {
                    let mut query = vec![("pin", pin.pin_id.clone()), ("limit", "1024".into())];
                    if let Some(key) = after {
                        query.push(("afterKey", key));
                    }
                    let (_, page): (_, StatePage) = self.client.json(
                        reqwest::Method::GET,
                        "state",
                        &query,
                        None::<&AckRequest>,
                        &[],
                    )?;
                    if page.pin_id != pin.pin_id || page.start_seq != pin.start_seq {
                        return Err(SyncError::new("invalid-state-page", 502));
                    }
                    lane.listed(page.items.len());
                    changes.extend(page.items.into_iter().map(|c| Change {
                        key: c.key,
                        stamp: c.stamp,
                        value: c.value,
                    }));
                    match page.next_key {
                        Some(key) => after = Some(key.as_str().into()),
                        None => break,
                    }
                }
                let mut merged = changes
                    .into_iter()
                    .map(|change| (change.key.clone(), change))
                    .collect::<std::collections::BTreeMap<_, _>>();
                let mut cursor = pin.start_seq;
                loop {
                    let (_, tail): (_, ChangesPage) = self.client.json(
                        reqwest::Method::GET,
                        "changes",
                        &[("after", cursor.0.to_string()), ("limit", "1024".into())],
                        None::<&AckRequest>,
                        &[],
                    )?;
                    if tail.next_after < cursor || tail.next_after > tail.through_seq {
                        return Err(SyncError::new("invalid-changes-page", 502));
                    }
                    lane.listed(tail.items.len());
                    for item in tail.items {
                        let incoming = Change {
                            key: item.key,
                            stamp: item.stamp,
                            value: item.value,
                        };
                        let apply = match merged.get(&incoming.key) {
                            Some(pinned) => tail_replaces(pinned, &incoming)?,
                            None => true,
                        };
                        if apply {
                            merged.insert(incoming.key.clone(), incoming);
                        }
                    }
                    cursor = tail.next_after;
                    if cursor == tail.through_seq {
                        break;
                    }
                }
                let changes = merged.into_values().collect::<Vec<_>>();
                self.prepare_bodies(store, &changes, upper)?;
                Ok((cursor, changes))
            })();
            let release = format!("state/pins/{}", pin.pin_id);
            if result.as_ref().is_err_and(|error| error.code == "cancelled") {
                // A cancelled read still frees its pin, with one attempt.
                if let Ok(client) = self.client.uncancelled() {
                    let _ = client.request_ambiguous_mutation(
                        reqwest::Method::DELETE,
                        &release,
                        None,
                        &[],
                        MAX_METADATA_BYTES,
                    );
                }
            } else {
                let _ = self.client.request(
                    reqwest::Method::DELETE,
                    &release,
                    &[],
                    None,
                    &[],
                    MAX_METADATA_BYTES,
                );
            }
            match result {
                Err(error)
                    if attempts < 3
                        && matches!(error.code.as_str(), "state-pin-expired" | "journal-floor") =>
                {
                    continue
                }
                other => return other,
            }
        }
    }
    pub(crate) fn retry_unpublished(
        &self,
        store: &mut PersistentStore,
        header: &Header,
    ) -> Result<ApplyResult> {
        self.fence(store)?;
        self.fresh_admission()?;
        let corrected = now_ms()?;
        let upper = corrected
            .checked_add(risunest_sync_wire::stamp::MAX_CLOCK_SKEW_MS)
            .ok_or_else(|| SyncError::new("clock-overflow", 409))?;
        let entries = store
            .server_outbox_for_repair(header.binding_authority)?
            .into_iter()
            .filter(|entry| entry.stamp.physical_ms.0 > upper)
            .collect::<Vec<_>>();
        let mut query = self.log.0.prepare(
            "SELECT intent,receipt FROM publications WHERE json_extract(intent,'$.authority')=?1",
        )?;
        let rows = query.query_map([header.binding_authority.0.to_string()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (intent, receipt) = row?;
            let publication: Publication = serde_json::from_str(&intent)
                .map_err(|_| SyncError::new("publication-integrity", 409))?;
            if publication.authority != header.binding_authority {
                continue;
            }
            if !publication
                .entries
                .iter()
                .any(|old| entries.iter().any(|entry| entry == old))
            {
                continue;
            }
            let terminal: OperationReceipt = serde_json::from_str(
                receipt
                    .as_deref()
                    .ok_or_else(|| SyncError::new("publication-unsettled", 409))?,
            )
            .map_err(|_| SyncError::new("publication-unsettled", 409))?;
            if matches!(terminal, OperationReceipt::Accepted { .. }) {
                return Err(SyncError::new("accepted-clock-correction-required", 409));
            }
            if terminal.body_digest() != operation_digest(&publication.request)? {
                return Err(SyncError::new("operation-integrity", 409));
            }
        }
        drop(query);
        let proof = entries
            .iter()
            .map(|entry| {
                Ok(AckEntry {
                    key: entry.key.clone(),
                    version: entry.version.clone(),
                    stamp: entry.stamp.clone(),
                    value_identity: wire_identity(&entry.value)?,
                })
            })
            .collect::<risunest_sync_wire::Result<Vec<_>>>()?;
        let proof_id = uuid::Uuid::new_v4().to_string();
        store.lww_record_unpublished_proof(header, &proof_id, &proof)?;
        Ok(store.lww_retry_unpublished(header, &proof_id, corrected.into())?)
    }
    #[cfg(test)]
    pub(crate) fn receive_page(&self, store: &mut PersistentStore, header: &Header) -> Result<StageReceive> {
        self.receive_page_with_frontier(store, header).map(|page| page.page)
    }

    pub(crate) fn receive_page_with_frontier(
        &self,
        store: &mut PersistentStore,
        header: &Header,
    ) -> Result<ReceivePage> {
        self.check()?;
        if store.lww_binding_authority()? != header.binding_authority {
            return Err(SyncError::new("binding-authority-changed", 409));
        }
        let authority = header.binding_authority.0.to_string();
        let pending: Option<String> = self
            .log
            .0
            .query_row(
                "SELECT body FROM receive_pages WHERE authority=?1",
                [&authority],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(pending) = pending {
            let page: ReceivePage = serde_json::from_str(&pending)
                .map_err(|_| SyncError::new("receive-page-integrity", 409))?;
            // What its archives read is this device's state, which may have changed since the
            // page was stored.
            self.prepare_archive_bodies(store, &page.page.changes)?;
            return Ok(page);
        }
        self.require_library_format(store, false)?;
        self.client.lane().step(Step::Listing);
        let upper = self.admission()?;
        let cursor = store
            .lww_receive_progress(header.binding_authority)?
            .into_iter()
            .find(|p| p.kind == "server")
            .map(|p| p.cursor)
            .unwrap_or(0.into());
        let existing_pin: Option<String> = self
            .log
            .0
            .query_row(
                "SELECT pin FROM bootstrap WHERE authority=?1",
                [&authority],
                |r| r.get(0),
            )
            .optional()?;
        let reply = if existing_pin.is_none() {
            Some(self.client.request(
                reqwest::Method::GET,
                "changes",
                &[("after", cursor.0.to_string()), ("limit", "256".into())],
                None,
                &[],
                MAX_METADATA_BYTES,
            )?)
        } else {
            None
        };
        let boot = existing_pin.is_some()
            || reply.as_ref().is_some_and(|reply| {
                reply.status == 410
                    && super::client::response_code(reply).as_deref() == Some("journal-floor")
            });
        let mut next_key = None;
        let mut done = false;
        let receive_frontier;
        let more;
        let (cursor, changes): (DecimalU64, Vec<Change>) = if boot {
            if existing_pin.is_none() {
                let (_, pin): (_, StatePin) = self.client.json(
                    reqwest::Method::POST,
                    "state/pins",
                    &[],
                    None::<&AckRequest>,
                    &[],
                )?;
                self.log.0.execute(
                    "INSERT INTO bootstrap VALUES(?1,?2,NULL,?3)",
                    params![
                        authority,
                        serde_json::to_string(&pin)
                            .map_err(|_| SyncError::new("state-pin-integrity", 409))?,
                        cursor.0.to_string()
                    ],
                )?;
            }
            let (pin, after, original): (String, Option<String>, String) = self.log.0.query_row(
                "SELECT pin,after_key,cursor FROM bootstrap WHERE authority=?1",
                [&authority],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let pin: StatePin = serde_json::from_str(&pin)
                .map_err(|_| SyncError::new("state-pin-integrity", 409))?;
            let mut query = vec![("pin", pin.pin_id.clone()), ("limit", "256".into())];
            if let Some(after) = after {
                query.push(("afterKey", after));
            }
            let reply = self.client.request(
                reqwest::Method::GET,
                "state",
                &query,
                None,
                &[],
                MAX_METADATA_BYTES,
            )?;
            if reply.status == 410
                && super::client::response_code(&reply).as_deref() == Some("state-pin-expired")
            {
                self.log
                    .0
                    .execute("DELETE FROM bootstrap WHERE authority=?1", [&authority])?;
                // A pin kept from an earlier page is replaced once; one made by this
                // call that already expired is reported instead of pinning again.
                if existing_pin.is_none() {
                    return Err(SyncError::new("state-pin-expired", 410));
                }
                return self.receive_page_with_frontier(store, header);
            }
            if !(200..300).contains(&reply.status) {
                return Err(response_error(reply));
            }
            let page: StatePage = canonical::decode(&reply.body, MAX_METADATA_BYTES)?;
            if page.pin_id != pin.pin_id || page.start_seq != pin.start_seq {
                return Err(SyncError::new("invalid-state-page", 502));
            }
            // Reading the whole state has no known size, so its pages count as they arrive.
            self.client.lane().backlog(page.items.len() as u64, 0);
            next_key = page.next_key.map(|key| key.as_str().to_owned());
            done = next_key.is_none();
            more = !done;
            receive_frontier = done.then_some(pin.start_seq);
            (
                if done {
                    pin.start_seq
                } else {
                    DecimalU64::try_from(original)?
                },
                page.items
                    .into_iter()
                    .map(|c| Change {
                        key: c.key,
                        stamp: c.stamp,
                        value: c.value,
                    })
                    .collect(),
            )
        } else {
            let reply = reply.unwrap();
            if !(200..300).contains(&reply.status) {
                return Err(response_error(reply));
            }
            let page: ChangesPage = canonical::decode(&reply.body, MAX_METADATA_BYTES)?;
            if page.next_after < cursor
                || page.next_after > page.through_seq
                || page.items.windows(2).any(|w| w[0].seq >= w[1].seq)
                || page
                    .items
                    .iter()
                    .any(|i| i.seq <= cursor || i.seq > page.next_after)
            {
                return Err(SyncError::new("invalid-changes-page", 502));
            }
            // The journal keeps one entry per key, so a sequence span bounds the
            // changes in it; it still measures how far through the server this is.
            // A last page holding only this device's own publications reads back a
            // push and is not work to show.
            let left = page.through_seq.0 - page.next_after.0;
            more = left > 0;
            receive_frontier = Some(page.through_seq);
            if more && page.next_after <= cursor { return Err(SyncError::new("invalid-changes-page", 502)); }
            let writer = store.lww_clock_state().ok().map(|state| state.writer_id);
            let read_back = left == 0 && page.items.iter().all(|item| writer.as_ref() == Some(&item.stamp.writer_id));
            self.client.lane().backlog(if read_back { 0 } else { page.next_after.0 - cursor.0 }, left);
            (
                page.next_after,
                page.items
                    .into_iter()
                    .map(|c| Change {
                        key: c.key,
                        stamp: c.stamp,
                        value: c.value,
                    })
                    .collect(),
            )
        };
        let mut unique = std::collections::BTreeMap::<_, Change>::new();
        for change in changes {
            let apply = if let Some(old) = unique.get(&change.key) {
                matches!(
                    wire_compare(&old.stamp, &old.value, &change.stamp, &change.value)?,
                    risunest_sync_wire::unit::LwwDecision::ApplyRemote
                )
            } else {
                true
            };
            if apply {
                unique.insert(change.key.clone(), change);
            }
        }
        let changes = unique.into_values().collect::<Vec<_>>();
        self.client.lane().listed(changes.len());
        let upper = if changes.iter().any(|change| change.stamp.physical_ms > upper) {
            // A stamp beyond the kept sample is checked against a new one before
            // it is rejected.
            self.fresh_admission()?
        } else {
            upper
        };
        self.prepare_bodies(store, &changes, upper)?;
        self.check()?;
        let staged = StageReceive {
            header: Header {
                binding_authority: header.binding_authority,
                request_id: uuid::Uuid::new_v4().to_string(),
            },
            changes,
            progress: Progress {
                kind: "server".into(),
                cursor,
                writer_id: None,
            },
            admitted_time_upper_ms: upper,
        };
        let staged = ReceivePage { page: staged, receive_frontier, more };
        self.log.0.execute(
            "INSERT INTO receive_pages VALUES(?1,?2,?3,?4,?5)",
            params![
                authority,
                serde_json::to_string(&staged)
                    .map_err(|_| SyncError::new("receive-page-integrity", 409))?,
                next_key,
                boot,
                done
            ],
        )?;
        Ok(staged)
    }
    #[cfg(test)]
    pub(crate) fn receive_native(
        &self,
        store: &mut PersistentStore,
        header: &Header,
        generating: &[MessageLocator],
    ) -> Result<ApplyResult> {
        self.receive_native_complete(store, header, generating)
            .map(|completion| completion.result)
    }
    #[cfg(test)]
    pub(crate) fn receive_native_complete(
        &self,
        store: &mut PersistentStore,
        header: &Header,
        generating: &[MessageLocator],
    ) -> Result<NativeReceiveCompletion> {
        let page = self.receive_page(store, header)?;
        let received_units = page.changes.len();
        store.lww_stage_receive(&page)?;
        let result = store.lww_apply_receive(&ApplyReceive {
            header: page.header.clone(),
            generating: generating.to_vec(),
        })?;
        store.lww_finish_receive(&page.header)?;
        self.finish_receive(store, &page.header)?;
        Ok(NativeReceiveCompletion {
            result,
            received_units,
        })
    }
    pub(crate) fn finish_receive(&self, store: &PersistentStore, header: &Header) -> Result<()> {
        self.check()?;
        self.client.lane().step(Step::Confirming);
        if store.lww_binding_authority()? != header.binding_authority {
            return Err(SyncError::new("binding-authority-changed", 409));
        }
        let authority = header.binding_authority.0.to_string();
        let row: Option<(String, Option<String>, bool, bool)> = self
            .log
            .0
            .query_row(
                "SELECT body,next_key,bootstrap,done FROM receive_pages WHERE authority=?1",
                [&authority],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((body, next, boot, done)) = row else {
            return Ok(());
        };
        let page: StageReceive = serde_json::from_str(&body)
            .map_err(|_| SyncError::new("receive-page-integrity", 409))?;
        // A page that keeps the cursor at 0 writes no progress row.
        let durable = store
            .lww_receive_progress(header.binding_authority)?
            .into_iter()
            .find(|p| p.kind == "server")
            .map_or(DecimalU64(0), |p| p.cursor);
        if page.header != *header || durable != page.progress.cursor {
            return Err(SyncError::new("receive-not-durable", 409));
        }
        store.server_assert_receive_finished(&page)?;
        self.acknowledge_cursor(page.progress.cursor)?;
        if boot && done {
            let pin: String = self.log.0.query_row(
                "SELECT pin FROM bootstrap WHERE authority=?1",
                [&authority],
                |r| r.get(0),
            )?;
            let pin: StatePin = serde_json::from_str(&pin)
                .map_err(|_| SyncError::new("state-pin-integrity", 409))?;
            let reply = self.client.request(
                reqwest::Method::DELETE,
                &format!("state/pins/{}", pin.pin_id),
                &[],
                None,
                &[],
                MAX_METADATA_BYTES,
            )?;
            if !(200..300).contains(&reply.status) && reply.status != 410 {
                return Err(response_error(reply));
            }
        }
        let tx = self.log.0.unchecked_transaction()?;
        if boot && done {
            tx.execute("DELETE FROM bootstrap WHERE authority=?1", [&authority])?;
        } else if boot {
            tx.execute(
                "UPDATE bootstrap SET after_key=?1 WHERE authority=?2",
                params![next, authority],
            )?;
        }
        tx.execute("DELETE FROM receive_pages WHERE authority=?1", [&authority])?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn acknowledge_cursor(&self, cursor: DecimalU64) -> Result<()> {
        let reply = self.client.request(
            reqwest::Method::POST,
            "ack",
            &[],
            Some(canonical::encode(&AckRequest { seq: cursor })?),
            &[],
            MAX_METADATA_BYTES,
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(response_error(reply));
        }
        Ok(())
    }
    pub(crate) fn prepare_bodies(
        &self,
        store: &mut PersistentStore,
        changes: &[Change],
        upper: DecimalU64,
    ) -> Result<()> {
        let cache = lane_cache(store.repository_root(), "receive");
        let result = self.prepare_bodies_through(store, changes, upper, &cache);
        if result.is_ok() {
            discard_cache(&cache);
        }
        result
    }
    fn prepare_bodies_through(
        &self,
        store: &mut PersistentStore,
        changes: &[Change],
        upper: DecimalU64,
        cache_root: &Path,
    ) -> Result<()> {
        let cache = Cache::open(cache_root)?.with_library(store.repository_root())?;
        let transfer = Transfer::new(&self.client, &cache)?;
        let mut controls = BTreeSet::new();
        let mut large = BTreeMap::new();
        let mut remote = BTreeSet::new();
        let lane = self.client.lane();
        lane.step(Step::Downloading);
        lane.plan_items(changes.len());
        for change in changes {
            self.check()?;
            lane.items_done(1);
            if change.stamp.physical_ms > upper {
                return Err(SyncError::new("incoming-clock-skew", 409));
            }
            wire_validate(&change.value)?;
            if let UnitValue::Object {
                descriptor_hash,
                descriptor,
            } = &change.value
            {
                #[cfg(test)]
                observe_descriptor_purposes(&change.key, descriptor_hash, descriptor);
                let body = canonical::encode(descriptor)?;
                #[cfg(test)]
                super::hash_metrics::record("c_descriptor_verify", body.len());
                if risunest_sync_wire::hash(&body) != *descriptor_hash {
                    return Err(SyncError::new("descriptor-integrity", 409));
                }
                cache.put(&body)?;
                store.lww_put_object(descriptor_hash, &body)?;
                // Message pages and plugin value parts are applied from the
                // object store, never fetched later as remote assets.
                let stored_dependencies =
                    matches!(change.key.components()[0].as_str(), "messages" | "plugin-local");
                if matches!(change.key.components()[0].as_str(), "messages" | "archive") {
                    controls.insert(descriptor.object_hash.clone());
                } else if large_unit(&change.key) {
                    large.insert(descriptor.object_hash.clone(), change.key.clone());
                } else {
                    remote.insert(descriptor.object_hash.clone());
                }
                if stored_dependencies {
                    controls.extend(descriptor.dependencies.iter().cloned());
                } else {
                    remote.extend(descriptor.dependencies.iter().cloned());
                }
                for root in [&descriptor.dependency_root, &descriptor.relation_root]
                    .into_iter()
                    .flatten()
                {
                    let dependencies =
                        transfer.download_reference_tree(vec![(root.clone(), vec![])])?;
                    #[cfg(test)]
                    observe_dependency_purposes(&change.key, &dependencies);
                    if stored_dependencies {
                        controls.extend(dependencies);
                    } else {
                        remote.extend(dependencies);
                    }
                    let mut pending = vec![root.clone()];
                    let mut seen = BTreeSet::new();
                    while let Some(hash) = pending.pop() {
                        #[cfg(test)]
                        crate::asset_repository::body_io::register_object_purpose(
                            &hash,
                            crate::asset_repository::body_io::BodyPurpose::Control,
                        );
                        if !seen.insert(hash.clone()) {
                            continue;
                        }
                        let body = cache.read(&hash, MAX_METADATA_BYTES)?;
                        store.lww_put_object(&hash, &body)?;
                        if let risunest_sync_wire::descriptor::ReferencePage::Branches {
                            children,
                        } = canonical::decode(&body, MAX_METADATA_BYTES)?
                        {
                            pending.extend(children);
                        }
                    }
                }
            } else if let UnitValue::Inline { bytes } = &change.value {
                if matches!(change.key.components()[0].as_str(), "asset" | "inlay") {
                    use base64::Engine;
                    let value: serde_json::Value = serde_json::from_slice(
                        &base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(bytes)
                            .map_err(|_| SyncError::new("invalid-alias", 409))?,
                    )
                    .map_err(|_| SyncError::new("invalid-alias", 409))?;
                    if let Some(hash) = value.get("objectHash").and_then(|v| v.as_str()) {
                        risunest_sync_wire::validate_hash(hash)?;
                        #[cfg(test)]
                        crate::asset_repository::body_io::register_object_purpose(
                            hash,
                            crate::asset_repository::body_io::BodyPurpose::Asset,
                        );
                        remote.insert(hash.into());
                    }
                }
            }
        }
        let mut required_controls = Vec::new();
        for hash in controls {
            if !store.lww_verified_object_present(&hash)? {
                required_controls.push(hash);
            }
        }
        transfer.download(&required_controls, &[])?;
        for hash in required_controls {
            let body = read_whole(&cache, &hash)?;
            store.lww_put_object(&hash, &body)?;
        }
        let binding = store.lww_binding_authority()?;
        let mut required_large = Vec::new();
        let mut hints = BTreeMap::new();
        for (hash, key) in large {
            if !store.lww_verified_object_present(&hash)? {
                let bases = delta_bases(store, &cache, &key, &hash, binding)?;
                if !bases.is_empty() {
                    hints.insert(hash.clone(), bases);
                }
                required_large.push(hash);
            }
        }
        transfer.download_with_hints(&required_large, &[], &hints)?;
        for hash in required_large {
            let mut body = cache.open_object(&hash)?.ok_or_else(|| SyncError::new("cached-object-missing", 409))?;
            let length = body.len()?;
            store.lww_put_object_reader(&hash, &mut body, length)?;
        }
        if !remote.is_empty() {
            let cas = crate::asset_repository::PayloadCas::new(store.repository_root())?;
            let local_sizes = cas.stat_objects(remote.iter().map(String::as_str))?;
            let missing = remote
                .into_iter()
                .zip(local_sizes)
                .filter_map(|(hash, size)| size.is_none().then_some((hash, None)))
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                let config = self
                    .access
                    .clone()
                    .or(store.server_stored_config()?)
                    .filter(|config| config.library_id == self.client.config().library_id)
                    .ok_or_else(|| SyncError::new("server-unconfigured", 409))?;
                let head = self.client.resolve_identity()?;
                let mut residency = super::residency::Residency::open(store.repository_root())?;
                residency.retain(&self.client, &config, &head, &missing)?;
                let hashes = missing.into_iter().map(|(hash, _)| hash).collect::<Vec<_>>();
                let sizes = residency.active_sizes(&hashes)?;
                let objects = hashes.into_iter().zip(sizes).map(|(object_hash, size)| {
                    let byte_size = size.ok_or_else(|| SyncError::new("remote-object-unavailable", 409))?;
                    Ok(crate::persistent_store::asset_object_catalog::AssetObjectRegistration { object_hash, byte_size })
                }).collect::<Result<Vec<_>>>()?;
                for page in objects.chunks(crate::persistent_store::asset_object_catalog::ASSET_OBJECT_CATALOG_MAX_PAGE as usize) {
                    store
                        .asset_object_catalog()
                        .register(page, now_ms()? as i64)?;
                }
            }
        }
        self.prepare_archive_bodies(store, changes)?;
        #[cfg(test)]
        if let Some(counter) = &self.client.test_io {
            let mut traffic = counter.traffic.lock().unwrap();
            traffic.prepared_units += changes.len() as u64;
        }
        Ok(())
    }
    /// Brings the bodies the archives and restores in `changes` read on this device from the
    /// storage that holds them, so applying the page never waits on a body held elsewhere.
    fn prepare_archive_bodies(&self, store: &PersistentStore, changes: &[Change]) -> Result<()> {
        let missing = store.lww_incoming_archive_bodies(changes)?;
        if missing.is_empty() {
            return Ok(());
        }
        let unavailable =
            super::residency::HydrationSession::new(store.repository_root(), self.cancelled.clone())?
                .hydrate_many(&missing, &|| self.check())?;
        if !unavailable.is_empty() {
            return Err(SyncError {
                cause: Some(format!("{} archive bodies", unavailable.len())),
                ..SyncError::new("required-asset-unavailable", 409)
            });
        }
        Ok(())
    }
}

pub(crate) const LWW_CACHE: &str = "lww-cache";

/// Each lane works in its own copy, so removing one after a finished operation
/// never takes bodies from another lane that is still running.
fn lane_cache(root: &Path, lane: &str) -> std::path::PathBuf {
    root.join("server-sync").join(LWW_CACHE).join(lane)
}

/// The library holds every body a finished operation copied here; a failed
/// one keeps its copies for the retry.
fn discard_cache(path: &Path) {
    // A file the system still holds is removed by the next finished operation
    // or a cache cleanup, so this never fails an operation that completed.
    let _ = std::fs::remove_dir_all(path);
}

/// A downloaded body, whole. Message pages, archive metadata and large unit
/// bodies are published at any size; the read still checks the body against
/// its hash.
#[track_caller]
fn read_whole(cache: &Cache, hash: &str) -> Result<Vec<u8>> {
    cache.read(hash, usize::MAX)
}

/// Units the library keeps as one body, where an edit mostly repeats the body
/// it replaces.
fn large_unit(key: &risunest_sync_wire::unit::UnitKey) -> bool {
    !matches!(key.components()[0].as_str(), "messages" | "archive")
        && crate::persistent_store::lww::lww_known_unit_key(key)
}

/// Earlier bodies of `key` this device can read, copied into `cache` so a
/// transfer can build or apply a delta against them.
fn delta_bases(
    store: &PersistentStore,
    cache: &Cache,
    key: &risunest_sync_wire::unit::UnitKey,
    target: &str,
    binding: DecimalU64,
) -> Result<Vec<String>> {
    let mut bases = Vec::new();
    for base in store.lww_unit_object_bases(key, binding)? {
        if base == target {
            continue;
        }
        if cache.stat_object(&base)?.is_none()
            && !store.lww_copy_large_object_to_cas(&base, &cache.cas, super::cache::SMALL_OBJECT_BYTES)?
        {
            let Some(body) = store.lww_object_body(&base)? else { continue };
            if cache.put(&body)? != base {
                continue;
            }
        }
        bases.push(base);
    }
    Ok(bases)
}

/// Keeps the longest outbox prefix whose canonical push request stays below
/// the metadata bound with room for framing; later entries wait for the next push.
fn fit_push_page(request: &mut PushRequest, entries: &mut Vec<OutboxEntry>) -> Result<()> {
    const PUSH_BUDGET: usize = MAX_METADATA_BYTES - 4096;
    let mut total = canonical::encode(&*request)?.len();
    let mut changes = Vec::new();
    for entry in entries.iter() {
        let change = UnitChange {
            key: entry.key.clone(),
            stamp: entry.stamp.clone(),
            value: entry.value.clone(),
        };
        let bytes = canonical::encode(&change)?.len() + usize::from(!changes.is_empty());
        if total + bytes > PUSH_BUDGET {
            break;
        }
        total += bytes;
        changes.push(change);
    }
    if changes.is_empty() {
        return Err(SyncError::new("unit-too-large", 413));
    }
    entries.truncate(changes.len());
    request.changes = changes;
    Ok(())
}
/// A journal entry after the pin is what the server applied, and the server
/// lets a retirement replace a live existence whatever its stamp.
fn tail_replaces(pinned: &Change, incoming: &Change) -> risunest_sync_wire::Result<bool> {
    let decision = wire_compare(
        &pinned.stamp,
        &pinned.value,
        &incoming.stamp,
        &incoming.value,
    )?;
    Ok(decision == risunest_sync_wire::unit::LwwDecision::ApplyRemote
        || (incoming.key.components()[0] == "exists"
            && incoming.value == UnitValue::Deleted
            && pinned.value != UnitValue::Deleted))
}
fn operation_digest(request: &PushRequest) -> risunest_sync_wire::Result<String> {
    let result = request.digest();
    #[cfg(test)]
    if result.is_ok() {
        super::hash_metrics::operation(request);
    } else {
        super::hash_metrics::incomplete();
    }
    result
}
fn wire_identity(value: &UnitValue) -> risunest_sync_wire::Result<String> {
    let result = value.identity();
    #[cfg(test)]
    if result.is_ok() {
        super::hash_metrics::identity(value);
    } else {
        super::hash_metrics::incomplete();
    }
    result
}
fn wire_validate(value: &UnitValue) -> risunest_sync_wire::Result<()> {
    let result = value.validate();
    #[cfg(test)]
    if result.is_ok() {
        super::hash_metrics::validation(value);
    } else {
        super::hash_metrics::incomplete();
    }
    result
}
fn wire_compare(
    old_stamp: &risunest_sync_wire::stamp::Stamp,
    old: &UnitValue,
    new_stamp: &risunest_sync_wire::stamp::Stamp,
    new: &UnitValue,
) -> risunest_sync_wire::Result<risunest_sync_wire::unit::LwwDecision> {
    let result = risunest_sync_wire::unit::compare_version(old_stamp, old, new_stamp, new);
    #[cfg(test)]
    if result.is_ok() {
        super::hash_metrics::identity(old);
        super::hash_metrics::identity(new);
    } else {
        super::hash_metrics::incomplete();
    }
    result
}
