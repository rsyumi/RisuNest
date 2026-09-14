//! This adapter lives in PDS's module scope so SQLite state cannot leak through
//! IPC. Network work uses a dedicated job-store connection and a frozen read lease.
#[cfg(test)]
#[path = "engine_promotion_tests.rs"]
mod promotion_tests;

use super::{
    asset_object_catalog::{AssetObjectCatalog, AssetObjectRegistration},
    server_sync_apply::{
        validate_remote, validate_remote_with_residency, RemoteRecord, ReplicaAdvance,
        ValidatedRecords,
    },
    server_sync_outbox as outbox, server_sync_projection as projection, PersistentStore,
};
use crate::{
    asset_repository::PayloadCas,
    logical_records::{decode_logical_record_key, encode_logical_record_key, LogicalRecordLocator},
    server_sync::{
        cache::Cache,
        client::{is_ambiguous_transient, response_error, Reply, RetryBudget, ServerClient},
        planner::{self, Decision},
        remote,
        transfer::Transfer,
        Result, SyncError,
    },
};
use reqwest::Method;
use risunest_sync_wire::{
    canonical, change_digest::ChangeDigest, ChangeSet, CommitIntent, ReadFence, Receipt,
    RecordChange, RecordVersion, RemoteHead, ScopeFence, MAX_METADATA_BYTES,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Resolution {
    KeepLocal,
    KeepRemote,
}

#[cfg(test)]
mod commit_retry_tests {
    use super::*;
    use crate::server_sync::client::ServerConfig;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };

    fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "request ended before its headers were complete");
            bytes.extend_from_slice(&chunk[..read]);
            let Some(headers_end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&bytes[..headers_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let request_length = headers_end + 4 + content_length;
            if bytes.len() >= request_length {
                bytes.truncate(request_length);
                return bytes;
            }
        }
    }

    #[test]
    fn accepted_commit_with_lost_response_checks_same_operation_without_reposting() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let config = ServerConfig {
            directory: None,
            endpoint,
            library_id: "library".into(),
            device_id: "device".into(),
            token: "a".repeat(64),
        };
        let intent = CommitIntent {
            device_operation_seq: 1.into(),
            expected_head: RemoteHead {
                library_id: config.library_id.clone(),
                epoch: "epoch".into(),
                seq: 0.into(),
                head_id: "b".repeat(64),
                min_retained_seq: 0.into(),
            },
            changes_digest: "c".repeat(64),
            staged_changes_id: "d".repeat(64),
        };
        let operation = risunest_sync_wire::operation_id(
            &config.library_id,
            &config.device_id,
            &intent.device_operation_seq,
        )
        .unwrap();
        let expected = operation.clone();
        let task = std::thread::spawn(move || {
            let (mut commit, _) = server.accept().unwrap();
            let request = read_request(&mut commit);
            assert!(request.starts_with(b"POST /commits HTTP/1.1\r\n"));
            drop(commit);

            let (mut lookup, _) = server.accept().unwrap();
            let request = read_request(&mut lookup);
            assert!(
                request.starts_with(format!("GET /operations/{expected} HTTP/1.1\r\n").as_bytes())
            );
            let body = serde_json::json!({"operationId":expected,"status":"pending"}).to_string();
            write!(
                lookup,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            lookup.flush().unwrap();

            server.set_nonblocking(true).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            assert!(matches!(
                server.accept(),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
            ));
        });
        let client = ServerClient::new(config).unwrap();
        let reply = submit_commit(&client, &intent).unwrap();
        assert_eq!(reply.status, 202);
        task.join().unwrap();
    }
}
/// Record counts of the cycle in flight, read by the UI beside the byte counter.
/// `total` covers the records this cycle applies or uploads; `done` advances
/// once per applied record and once per uploaded record.
#[derive(Default)]
pub(crate) struct CycleItemCounter {
    pub total: std::sync::atomic::AtomicU64,
    pub done: std::sync::atomic::AtomicU64,
}
impl CycleItemCounter {
    fn advance(counter: Option<&Self>) {
        if let Some(counter) = counter {
            counter
                .done
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CycleOptions {
    pub resolution: Option<Resolution>,
    pub expected_revision: Option<i64>,
    pub expected_head: Option<RemoteHead>,
    #[serde(skip)]
    pub cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    #[serde(skip)]
    pub verified_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    #[serde(skip)]
    pub retryable_failure: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    #[serde(skip)]
    pub cycle_items: Option<std::sync::Arc<CycleItemCounter>>,
    #[serde(skip)]
    pub(crate) retry_budget: Option<std::sync::Arc<RetryBudget>>,
    #[serde(skip)]
    pub groups: BTreeSet<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CycleResult {
    pub endpoint: String,
    pub phase: String,
    pub local_revision: i64,
    pub head: RemoteHead,
    pub conflict_count: usize,
    pub conflicts: Vec<String>,
    pub applied_records: usize,
    pub proposed_records: usize,
}
pub(crate) enum Preparation {
    Report(CycleResult),
    Ready(PreparedCycle),
}
pub(crate) struct PreparedCycle {
    pub revision: i64,
    previous: Option<RemoteHead>,
    pub through: RemoteHead,
    records: ValidatedRecords,
    bases: Vec<(String, RecordVersion, Option<String>)>,
    acknowledged: Vec<outbox::ServerDirtyKey>,
    publish_keys: Vec<outbox::ServerDirtyKey>,
    scope_versions: Vec<(String, String)>,
    scope_clears: Vec<(String, String)>,
    scope_fences: Vec<ScopeFence>,
    reads: BTreeMap<String, RecordVersion>,
    committed: bool,
    pub applied: usize,
    pub proposals: usize,
    clear_acknowledged: bool,
    activated: Option<i64>,
    cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    verified_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    retryable_failure: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    cycle_items: Option<std::sync::Arc<CycleItemCounter>>,
    retry_budget: std::sync::Arc<RetryBudget>,
}
fn json<T: Serialize>(value: &T) -> Result<String> {
    String::from_utf8(canonical::encode(value)?)
        .map_err(|_| SyncError::new("invalid-metadata", 409))
}
fn parse<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T> {
    Ok(canonical::decode(value.as_bytes(), MAX_METADATA_BYTES)?)
}

fn submit_commit(client: &ServerClient, intent: &CommitIntent) -> Result<Reply> {
    let config = client.config();
    let operation = risunest_sync_wire::operation_id(
        &config.library_id,
        &config.device_id,
        &intent.device_operation_seq,
    )?;
    loop {
        let ambiguous = match client.request(
            Method::POST,
            "commits",
            &[],
            Some(canonical::encode(intent)?),
            &[("if-match", intent.expected_head.etag())],
            MAX_METADATA_BYTES,
        ) {
            Ok(reply) if matches!(reply.status, 502 | 503 | 504) => {
                client.wait_transient_response(reply.retry_after, "server-unreachable")?;
                true
            }
            Ok(reply) => return Ok(reply),
            Err(error) if is_ambiguous_transient(&error) => {
                client.wait_after_ambiguous(&error)?;
                true
            }
            Err(error) => return Err(error),
        };
        debug_assert!(ambiguous);
        let mut status = client.request(
            Method::GET,
            &format!("operations/{operation}"),
            &[],
            None,
            &[],
            MAX_METADATA_BYTES,
        )?;
        client.report_retryable_failure(None);
        match status.status {
            200 if canonical::decode::<Receipt>(&status.body, MAX_METADATA_BYTES).is_ok() => {
                return Ok(status)
            }
            200 => {
                let value: serde_json::Value = canonical::decode(&status.body, MAX_METADATA_BYTES)?;
                if value.get("operationId").and_then(|value| value.as_str())
                    != Some(operation.as_str())
                    || value.get("status").and_then(|value| value.as_str()) != Some("pending")
                {
                    return Err(SyncError::new("invalid-operation-status", 502));
                }
                status.status = 202;
                return Ok(status);
            }
            404 => continue,
            410 => return Ok(status),
            _ => return Err(response_error(status)),
        }
    }
}
fn lookup(db: &Connection, table: &str, key: &str) -> Result<RecordVersion> {
    let value: Option<String> = db
        .query_row(
            &format!("SELECT version FROM {table} WHERE key=?1"),
            [key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(value
        .map(|s| parse(&s))
        .transpose()?
        .unwrap_or(RecordVersion::Absent))
}
fn key_parts(key: &str, revision: i64) -> Result<outbox::ServerDirtyKey> {
    let (kind, key1, key2) = match decode_logical_record_key(key)
        .map_err(|_| SyncError::new("invalid-record-key", 409))?
    {
        LogicalRecordLocator::Root => ("root", String::new(), String::new()),
        LogicalRecordLocator::Preset { preset_id } => ("preset", preset_id, String::new()),
        LogicalRecordLocator::Plugin { storage_key } => ("plugin", storage_key, String::new()),
        LogicalRecordLocator::Character { character_id } => {
            ("character", character_id, String::new())
        }
        LogicalRecordLocator::Conversation {
            character_id,
            conversation_id,
        } => ("conversation", character_id, conversation_id),
        LogicalRecordLocator::Asset { logical_key } => ("asset", logical_key, String::new()),
        LogicalRecordLocator::Inlay { logical_key } => ("inlay", logical_key, String::new()),
        LogicalRecordLocator::Cold { logical_key } => ("cold", logical_key, String::new()),
    };
    Ok(outbox::ServerDirtyKey {
        kind: kind.into(),
        key1,
        key2,
        revision,
    })
}
fn relations(key: &outbox::ServerDirtyKey) -> Result<Vec<String>> {
    if key.kind == "conversation" {
        Ok(vec![encode_logical_record_key(
            &LogicalRecordLocator::Character {
                character_id: key.key1.clone(),
            },
        )
        .map_err(|_| SyncError::new("invalid-parent-key", 409))?])
    } else {
        Ok(Vec::new())
    }
}
fn scopes(key: &outbox::ServerDirtyKey) -> Vec<String> {
    if key.kind == "plugin" {
        vec!["plugin-storage".into()]
    } else {
        Vec::new()
    }
}
fn delete_version(base: &RecordVersion) -> RecordVersion {
    match base {
        RecordVersion::Absent => RecordVersion::Absent,
        RecordVersion::Tombstone { .. } => base.clone(),
        _ => RecordVersion::Tombstone {
            deletion_id: uuid::Uuid::new_v4().to_string(),
        },
    }
}

impl PersistentStore {
    fn publish_server_cycle(
        &mut self,
        client: &ServerClient,
        transfer: &Transfer<'_>,
        cache: &Cache,
        head: &RemoteHead,
        revision: i64,
        reads: &BTreeMap<String, RecordVersion>,
        scopes: &[ScopeFence],
        cycle_items: Option<&CycleItemCounter>,
    ) -> Result<()> {
        if self.server_pending()?.is_some() {
            return Err(SyncError::new("operation-already-pending", 409));
        }
        self.connection.execute_batch("DELETE FROM server_sync_operation_records; DELETE FROM server_sync_operation_pages; DELETE FROM server_sync_operation_scopes;")?;
        let mut digest = ChangeDigest::new();
        let mut builder = PageBuilder::new(&self.connection);
        let mut after = String::new();
        loop {
            let page = cycle_page(&self.connection, &after)?;
            if page.is_empty() {
                break;
            }
            for item in page {
                after = item.key.clone();
                if item.action != "publish" {
                    continue;
                }
                let before: RecordVersion = parse(&item.remote)?;
                let after: RecordVersion = parse(&item.version)?;
                let change = RecordChange {
                    key: item.key.clone(),
                    before,
                    after: after.clone(),
                };
                digest.change(&change)?;
                builder.change(change)?;
                self.server_record_prepared(
                    &item.key,
                    &after,
                    item.local_hash.as_deref(),
                    &key_parts(&item.key, revision)?,
                )?;
            }
        }
        for (key, version) in reads {
            let fence = ReadFence {
                key: key.clone(),
                version: version.clone(),
            };
            digest.read_fence(&fence)?;
            builder.read(fence)?;
        }
        for scope in scopes {
            digest.scope_fence(scope)?;
            builder.scope(scope.clone())?;
            self.connection.execute(
                "INSERT INTO server_sync_operation_scopes VALUES(?1,?2)",
                params![scope.scope, scope.expected_version],
            )?;
        }
        builder.flush()?;
        let changes_digest = digest.finish()?;
        let pages: i64 = self.connection.query_row(
            "SELECT count(*) FROM server_sync_operation_pages",
            [],
            |r| r.get(0),
        )?;
        if pages == 1 {
            self.upload_pending_objects(transfer, cycle_items)?;
            let body: Vec<u8> = self.connection.query_row(
                "SELECT body FROM server_sync_operation_pages WHERE page=0",
                [],
                |r| r.get(0),
            )?;
            let reply = client.request(
                Method::POST,
                "staged-changes",
                &[],
                Some(body),
                &[],
                MAX_METADATA_BYTES,
            )?;
            if reply.status != 201 {
                return Err(response_error(reply));
            }
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Sealed {
                staged_changes_id: String,
                changes_digest: String,
            }
            let sealed: Sealed = canonical::decode(&reply.body, MAX_METADATA_BYTES)?;
            if sealed.changes_digest != changes_digest {
                return Err(SyncError::new("staged-intent-mismatch", 409));
            }
            let intent =
                self.server_reserve(head, changes_digest, sealed.staged_changes_id, revision)?;
            let reply = submit_commit(client, &intent)?;
            if reply.status == 202 {
                return Ok(());
            }
            if reply.status == 410 {
                self.server_abandon_expired_operation()?;
                return Ok(());
            }
            if let Ok(receipt) = canonical::decode::<Receipt>(&reply.body, MAX_METADATA_BYTES) {
                self.server_observe_receipt(&receipt)?;
                return Ok(());
            }
            return Err(response_error(reply));
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Started {
            staged_changes_id: String,
        }
        let (_, stage): (_, Started) =
            client.json(Method::POST, "staged-changes/start", &[], None::<&()>, &[])?;
        self.server_reserve(head, changes_digest, stage.staged_changes_id, revision)?;
        self.resume_server_operation(client, transfer)?;
        let _ = cache;
        Ok(())
    }
    /// Returns true while a durable server job is pending; false after a verified
    /// terminal receipt, allowing the caller to reconcile it with remote history.
    fn resume_server_operation(
        &mut self,
        client: &ServerClient,
        transfer: &Transfer<'_>,
    ) -> Result<bool> {
        let Some(mut pending) = self.server_pending()? else {
            return Ok(false);
        };
        if pending.phase.starts_with('{') {
            return Ok(false);
        }
        let config = self
            .server_config()?
            .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
        let id = risunest_sync_wire::operation_id(
            &config.library_id,
            &config.device_id,
            &pending.intent.device_operation_seq,
        )?;
        let reply = client.request(
            Method::GET,
            &format!("operations/{id}"),
            &[],
            None,
            &[],
            MAX_METADATA_BYTES,
        )?;
        if reply.status == 200 {
            if let Ok(receipt) = canonical::decode::<Receipt>(&reply.body, MAX_METADATA_BYTES) {
                self.server_observe_receipt(&receipt)?;
                return Ok(false);
            }
            let value: serde_json::Value = canonical::decode(&reply.body, MAX_METADATA_BYTES)?;
            if value.get("operationId").and_then(|v| v.as_str()) != Some(&id)
                || value.get("status").and_then(|v| v.as_str()) != Some("pending")
            {
                return Err(SyncError::new("invalid-operation-status", 502));
            }
            return Ok(true);
        }
        if reply.status == 410 {
            self.server_abandon_expired_operation()?;
            return Ok(false);
        }
        if reply.status != 404 {
            return Err(response_error(reply));
        }
        let progress = client.request(
            Method::GET,
            &format!("staged-changes/{}", pending.intent.staged_changes_id),
            &[],
            None,
            &[],
            MAX_METADATA_BYTES,
        )?;
        if [404, 410].contains(&progress.status) {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Started {
                staged_changes_id: String,
            }
            let (_, stage): (_, Started) =
                client.json(Method::POST, "staged-changes/start", &[], None::<&()>, &[])?;
            pending.intent = self.server_restage(stage.staged_changes_id)?;
        } else if progress.status != 200 {
            return Err(response_error(progress));
        }
        self.upload_pending_objects(transfer, None)?;
        // Pages are idempotent by index and exact bytes. Replaying them is safe
        // after process death between a successful PUT and recording its response.
        let mut page_index = 0i64;
        loop {
            let body: Option<Vec<u8>> = self
                .connection
                .query_row(
                    "SELECT body FROM server_sync_operation_pages WHERE page=?1",
                    [page_index],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(body) = body else {
                break;
            };
            let reply = client.request(
                Method::PUT,
                &format!(
                    "staged-changes/{}/pages/{page_index}",
                    pending.intent.staged_changes_id
                ),
                &[],
                Some(body),
                &[],
                MAX_METADATA_BYTES,
            )?;
            if reply.status != 204 {
                return Err(response_error(reply));
            }
            page_index += 1;
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Sealed {
            staged_changes_id: String,
            changes_digest: String,
        }
        let (_, sealed): (_, Sealed) = client.json(
            Method::POST,
            &format!("staged-changes/{}/seal", pending.intent.staged_changes_id),
            &[],
            None::<&()>,
            &[],
        )?;
        if sealed.staged_changes_id != pending.intent.staged_changes_id
            || sealed.changes_digest != pending.intent.changes_digest
        {
            return Err(SyncError::new("staged-intent-mismatch", 409));
        }
        let reply = submit_commit(client, &pending.intent)?;
        if reply.status == 202 {
            return Ok(true);
        }
        if reply.status == 410 {
            self.server_abandon_expired_operation()?;
            return Ok(false);
        }
        if let Ok(receipt) = canonical::decode::<Receipt>(&reply.body, MAX_METADATA_BYTES) {
            self.server_observe_receipt(&receipt)?;
            Ok(false)
        } else {
            Err(response_error(reply))
        }
    }
    fn upload_pending_objects(
        &self,
        transfer: &Transfer<'_>,
        cycle_items: Option<&CycleItemCounter>,
    ) -> Result<()> {
        let mut remote_context = None;
        self.connection.execute_batch("CREATE TEMP TABLE IF NOT EXISTS server_upload_objects(hash TEXT PRIMARY KEY); DELETE FROM server_upload_objects;")?;
        let mut after = String::new();
        loop {
            let records = {
                let mut stmt=self.connection.prepare("SELECT key,version FROM server_sync_operation_records WHERE key>?1 ORDER BY key LIMIT 1024")?;
                let rows = stmt
                    .query_map([&after], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            };
            if records.is_empty() {
                break;
            }
            for (key, version) in records {
                transfer.client.ensure_active()?;
                after = key.clone();
                let version: RecordVersion = parse(&version)?;
                let mut objects = transfer.cache.closure(&version)?;
                let mut base_lease = false;
                let (base, _) = self.server_base(&key)?;
                if let Ok(previous) = transfer.cache.closure(&base) {
                    let roots = base
                        .object_hashes()
                        .into_iter()
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    if !roots.is_empty() {
                        match transfer.pin(&roots) {
                            Ok(()) => {
                                base_lease = true;
                                // The committed descriptor is a transitive GC root.
                                // Its verified closure needs no 100k-hash missing query.
                                let previous = previous.into_iter().collect::<BTreeSet<_>>();
                                objects.retain(|hash| !previous.contains(hash));
                            }
                            Err(error) if error.status == 404 => (),
                            Err(error) => return Err(error),
                        }
                    }
                }
                let mut unsent = Vec::new();
                for hash in objects {
                    if self.connection.execute(
                        "INSERT OR IGNORE INTO server_upload_objects VALUES(?1)",
                        [&hash],
                    )? != 0
                    {
                        unsent.push(hash);
                    }
                }
                let bases = self.server_base_candidates(&key, transfer.cache, false)?;
                let mut local = Vec::new();
                let mut remote = Vec::new();
                for hash in unsent {
                    if transfer.cache.cas.stat_object(&hash)?.is_none() {
                        if remote_context.is_none() {
                            let residency = crate::server_sync::residency::Residency::open(
                                &self.repository_root,
                            )?;
                            let stored = self
                                .server_stored_config()?
                                .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
                            let head = transfer.client.head()?;
                            let context = crate::server_sync::residency::Residency::context_id(
                                &stored,
                                &head.epoch,
                            );
                            remote_context = Some((residency, context));
                        }
                        let (residency, context) = remote_context.as_ref().unwrap();
                        if residency.confirms(&hash, None, context)? {
                            remote.push(hash);
                            continue;
                        }
                    }
                    local.push(hash);
                }
                transfer.pin(&remote)?;
                let hints = transfer.record_reference_hints(&version, &base)?;
                transfer.upload_with_hints(&local, &bases, base_lease, &hints)?;
                CycleItemCounter::advance(cycle_items);
            }
        }
        Ok(())
    }
    pub(crate) fn server_cycle(&mut self, options: &CycleOptions) -> Result<CycleResult> {
        match self.server_prepare_cycle(options)? {
            Preparation::Report(report) => Ok(report),
            Preparation::Ready(mut ready) => {
                self.server_activate_cycle(&mut ready)?;
                self.server_publish_cycle(&ready)
            }
        }
    }
    pub(crate) fn server_prepare_cycle(&mut self, options: &CycleOptions) -> Result<Preparation> {
        super::sync_selection::require_server(&self.connection)?;
        let config = self
            .server_config()?
            .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
        let status = self.server_status()?;
        if status.registration_required {
            return Err(SyncError::new("device-registration-required", 409));
        }
        let mut client = if let Some(retry_budget) = &options.retry_budget {
            ServerClient::with_retry_budget(
                config.clone(),
                options.cancellation.clone(),
                retry_budget.clone(),
            )?
        } else {
            ServerClient::with_cancellation(config.clone(), options.cancellation.clone())?
        };
        client.verified_bytes = options.verified_bytes.clone();
        client.retryable_failure = options.retryable_failure.clone();
        let identity_head = client.resolve_identity(false)?;
        self.server_cache_endpoint(&config, &client.config())?;
        let resume_may_commit = self
            .server_pending()?
            .is_some_and(|p| !p.phase.starts_with('{'));
        let cache = Cache::open(&self.repository_root.join("server-sync").join(
            risunest_sync_wire::hash(
                format!("{}:{}", config.library_id, config.device_id).as_bytes(),
            ),
        ))?;
        let transfer = Transfer::new(&client, &cache)?;
        let retry_budget = client.retry_budget();
        if self.resume_server_operation(&client, &transfer)? {
            return Ok(Preparation::Report(CycleResult {
                endpoint: client.config().endpoint.clone(),
                phase: "pending".into(),
                local_revision: self.revision()?,
                head: client.head()?,
                conflict_count: 0,
                conflicts: Vec::new(),
                applied_records: 0,
                proposed_records: 0,
            }));
        }
        // A pending job may finish after the identity snapshot. Its completed
        // receipt must be followed by a fresh head before remote catch-up.
        let observed = if resume_may_commit {
            client.head()?
        } else {
            identity_head
        };
        if status
            .head
            .as_ref()
            .is_some_and(|h| h.epoch != observed.epoch)
        {
            return Err(SyncError::new("epoch-reconciliation-required", 409));
        }
        let through = remote::refresh(&mut self.connection, &client, &observed)?;
        let pending = self.server_pending()?;
        let committed = pending.as_ref().is_some_and(|p| p.phase.starts_with('{'));
        if let Some(pending) = pending.as_ref().filter(|_| committed) {
            let receipt: Receipt = parse(&pending.phase)?;
            if through.seq < receipt.head.seq {
                return Err(SyncError::new("remote-catch-up-required", 409));
            }
        }
        let revision = self.revision()?;
        let lease = self.acquire_revision(revision)?;
        let prepared = self.prepare_server_cycle(
            &lease.lease,
            &cache,
            committed,
            status.full_scan || !options.groups.is_empty(),
            &client,
        );
        self.release_revision(&lease.lease)?;
        prepared?;
        let mut conflicts = Vec::new();
        let mut conflict_count = 0;
        let mut after = String::new();
        let mut proposals = 0usize;
        let mut reads = BTreeMap::new();
        let mut any_plugin = false;
        let mut local_plugin_change = false;
        let mut required_groups = options.groups.clone();
        loop {
            let page = cycle_page(&self.connection, &after)?;
            if page.is_empty() {
                break;
            }
            for item in page {
                after = item.key.clone();
                let key = key_parts(&item.key, revision)?;
                any_plugin |= key.kind == "plugin";
                let (base, _) = self.effective_server_base(&item.key, committed)?;
                let remote = lookup(&self.connection, "server_sync_remote", &item.key)?;
                let local: RecordVersion = parse(&item.version)?;
                let mut decision = planner::decide(&base, &local, &remote);
                local_plugin_change |= key.kind == "plugin"
                    && matches!(decision, Decision::PublishLocal | Decision::Conflict);
                let forced_group = options.groups.contains(&key.kind)
                    || ((key.kind == "character" || key.kind == "conversation")
                        && options.groups.contains(&format!("family:{}", key.key1)))
                    || (key.kind == "conversation"
                        && options.groups.contains(&format!("order:{}", key.key1)));
                if forced_group && local != remote {
                    decision = Decision::Conflict;
                }
                let independent_library =
                    status.head.is_none() && observed.seq != 0.into() && revision > 0;
                if (status.reconciling || independent_library)
                    && matches!(decision, Decision::AcceptRemote | Decision::PublishLocal)
                {
                    decision = Decision::Conflict;
                }
                if matches!(decision, Decision::PublishLocal | Decision::Conflict) {
                    for parent in relations(&key)? {
                        let (parent_base, _) = self.effective_server_base(&parent, committed)?;
                        let parent_remote =
                            lookup(&self.connection, "server_sync_remote", &parent)?;
                        if planner::read_dependency_conflicts(&parent_base, &parent_remote, true) {
                            decision = Decision::Conflict;
                            required_groups.insert(format!("family:{}", key.key1));
                        }
                        reads.insert(parent, parent_remote);
                    }
                }
                if decision == Decision::Conflict {
                    conflict_count += 1;
                    if conflicts.len() < 100 {
                        conflicts.push(item.key.clone());
                    }
                    if let Some(resolution) = options.resolution {
                        decision = match resolution {
                            Resolution::KeepLocal => Decision::PublishLocal,
                            Resolution::KeepRemote => Decision::AcceptRemote,
                        };
                    }
                }
                let action = match decision {
                    Decision::Conflict => "conflict",
                    Decision::PublishLocal => {
                        proposals += 1;
                        "publish"
                    }
                    Decision::AcceptRemote => "apply",
                    Decision::Identical => "identical",
                    Decision::Unchanged => "unchanged",
                };
                let local = if action == "publish" && local == RecordVersion::Absent {
                    RecordVersion::Tombstone {
                        deletion_id: uuid::Uuid::new_v4().to_string(),
                    }
                } else {
                    local
                };
                self.connection.execute(
                    "UPDATE server_cycle_records SET action=?2,version=?3,remote=?4 WHERE key=?1",
                    params![item.key, action, json(&local)?, json(&remote)?],
                )?;
            }
        }
        let mut scope_versions = Vec::new();
        let mut scope_clears = Vec::new();
        let mut scope_fences = Vec::new();
        let clear_after = pending
            .as_ref()
            .filter(|_| committed)
            .map_or(-1, |p| p.local_revision);
        let clear: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM server_sync_clears WHERE revision>?1 AND revision<=?2)",
            params![clear_after, revision],
            |r| r.get(0),
        )?;
        if any_plugin
            || clear
            || !status
                .head
                .as_ref()
                .is_some_and(|h| h.same_revision(&through))
        {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Scope {
                head: RemoteHead,
                scope: String,
                version: String,
                clear_version: String,
            }
            let (_, scope): (_, Scope) = client.json(
                Method::GET,
                "scopes",
                &[("scope", "plugin-storage".into())],
                None::<&()>,
                &[],
            )?;
            scope.head.validate()?;
            if scope.scope != "plugin-storage" || !scope.head.same_revision(&through) {
                return Err(SyncError::new("server-head-changed", 409));
            }
            risunest_sync_wire::validate_hash(&scope.version)?;
            risunest_sync_wire::validate_hash(&scope.clear_version)?;
            let mut previous_clear: Option<String> = self
                .connection
                .query_row(
                    "SELECT version FROM server_sync_scope_clear_base WHERE scope='plugin-storage'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(operation) = pending.as_ref().filter(|_| committed) {
                let own_clear:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM server_sync_operation_scopes WHERE scope='plugin-storage')",[],|r|r.get(0))?;
                if own_clear {
                    previous_clear = Some(parse::<Receipt>(&operation.phase)?.operation_id);
                }
            }
            if local_plugin_change && previous_clear.is_some_and(|v| v != scope.clear_version) {
                required_groups.insert("plugin".into());
                conflict_count += 1;
                if conflicts.len() < 100 {
                    conflicts.push("plugin-storage:remote-clear".into());
                }
            }
            scope_clears.push((scope.scope.clone(), scope.clear_version));
            if clear {
                let mut stmt = self.connection.prepare(
                    "SELECT expected_version FROM server_sync_clears WHERE revision<=?1 AND revision>?2",
                )?;
                for row in stmt.query_map(params![revision, clear_after], |r| {
                    r.get::<_, Option<String>>(0)
                })? {
                    if planner::clear_conflicts(row?.as_deref(), &scope.version) {
                        required_groups.insert("plugin".into());
                        conflict_count += 1;
                        if conflicts.len() < 100 {
                            conflicts.push("plugin-storage:clear".into());
                        }
                    }
                }
                if !matches!(options.resolution, Some(Resolution::KeepRemote)) {
                    scope_fences.push(ScopeFence {
                        scope: scope.scope.clone(),
                        expected_version: scope.version.clone(),
                        clear: true,
                    });
                }
            }
            scope_versions.push((scope.scope, scope.version));
        }
        required_groups.extend(self.server_order_conflicts(&cache, &transfer, revision)?);
        if required_groups != options.groups {
            return self.server_prepare_cycle(&CycleOptions {
                resolution: options.resolution,
                expected_revision: options.expected_revision,
                expected_head: options.expected_head.clone(),
                cancellation: options.cancellation.clone(),
                verified_bytes: options.verified_bytes.clone(),
                retryable_failure: options.retryable_failure.clone(),
                cycle_items: options.cycle_items.clone(),
                retry_budget: Some(retry_budget),
                groups: required_groups,
            });
        }
        if conflict_count > 0 {
            if options.resolution.is_none() {
                return Ok(Preparation::Report(CycleResult {
                    endpoint: client.config().endpoint.clone(),
                    phase: "conflict".into(),
                    local_revision: revision,
                    head: through,
                    conflict_count,
                    conflicts,
                    applied_records: 0,
                    proposed_records: proposals,
                }));
            }
            if options.expected_revision != Some(revision)
                || !options
                    .expected_head
                    .as_ref()
                    .is_some_and(|h| h.same_revision(&through))
            {
                return Err(SyncError::new("conflict-preview-stale", 409));
            }
            self.server_conflict_backups(&cache, &transfer, &client, revision, &through)?;
        }
        if !scope_fences.is_empty() {
            let mut after = String::new();
            loop {
                let page = cycle_page(&self.connection, &after)?;
                if page.is_empty() {
                    break;
                }
                for item in page {
                    after = item.key.clone();
                    if item.action != "publish" && key_parts(&item.key, revision)?.kind == "plugin"
                    {
                        let remote: RecordVersion = parse(&item.remote)?;
                        if matches!(remote, RecordVersion::Live { .. }) {
                            reads.insert(item.key, remote);
                        }
                    }
                }
            }
        }
        let clear_acknowledged = clear && scope_fences.is_empty();
        let mut residency = crate::server_sync::residency::Residency::open(&self.repository_root)?;
        let remote_assets =
            residency.policy()? == crate::server_sync::residency::AssetPolicy::Remote;
        let stored_config = self
            .server_stored_config()?
            .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
        let custody_context =
            crate::server_sync::residency::Residency::context_id(&stored_config, &through.epoch);
        let mut records = ValidatedRecords::new()?;
        let mut bases = Vec::new();
        let mut acknowledged = Vec::new();
        let mut publish_keys = Vec::new();
        if let Some(counter) = &options.cycle_items {
            let total: i64 = self.connection.query_row(
                "SELECT count(*) FROM server_cycle_records WHERE action IN ('apply','publish')",
                [],
                |r| r.get(0),
            )?;
            counter
                .total
                .store(total.max(0) as u64, std::sync::atomic::Ordering::Relaxed);
            counter.done.store(0, std::sync::atomic::Ordering::Relaxed);
        }
        after.clear();
        loop {
            let page = cycle_page(&self.connection, &after)?;
            if page.is_empty() {
                break;
            }
            for item in page {
                after = item.key.clone();
                let applying = item.action == "apply";
                let remote: RecordVersion = parse(&item.remote)?;
                let dirty = key_parts(&item.key, revision)?;
                let (base_version, base_hash) = self.effective_server_base(&item.key, committed)?;
                let remote_hash = match item.action.as_str() {
                    "apply" => {
                        let (payload, hash) = if matches!(remote, RecordVersion::Live { .. }) {
                            let base_candidates =
                                self.server_base_candidates(&item.key, &cache, committed)?;
                            if remote_assets {
                                transfer.download_record_metadata(
                                    &remote,
                                    &base_candidates,
                                    &base_version,
                                )?;
                            } else {
                                transfer.download_record(
                                    &remote,
                                    &base_candidates,
                                    &base_version,
                                )?;
                            }
                            let (payload, hash) = cache.restore(&remote)?;
                            let dependencies = projection::dependencies(&payload, &cache.cas)?;
                            let expected = cache.project(
                                &payload,
                                &dependencies,
                                &relations(&dirty)?,
                                scopes(&dirty),
                            )?;
                            if expected.version != remote {
                                return Err(SyncError::new(
                                    "server-descriptor-semantics-mismatch",
                                    409,
                                ));
                            }
                            if remote_assets {
                                use crate::logical_records::LogicalRecordEnvelope as Envelope;
                                let expected = match &payload.record {
                                    Envelope::Asset {
                                        object_hash: Some(hash),
                                        size,
                                        ..
                                    }
                                    | Envelope::Inlay {
                                        object_hash: Some(hash),
                                        size,
                                        ..
                                    } => Some((hash, *size)),
                                    _ => None,
                                };
                                let identities = dependencies
                                    .iter()
                                    .map(|hash| {
                                        Ok((
                                            hash.clone(),
                                            expected
                                                .filter(|(candidate, _)| *candidate == hash)
                                                .map(|(_, size)| size)
                                                .or(cache.cas.stat_object(hash)?),
                                        ))
                                    })
                                    .collect::<Result<Vec<_>>>()?;
                                residency.retain(&client, &stored_config, &through, &identities)?;
                            }
                            self.promote_server_dependencies_with_residency(
                                &cache,
                                &dependencies,
                                remote_assets.then_some((&residency, custody_context.as_str())),
                                || client.ensure_active(),
                            )?;
                            (Some(payload), Some(hash))
                        } else {
                            (None, None)
                        };
                        // Semantic validation is done against the original shared
                        // envelope, then the row writer preserves local view fields.
                        records.push(validate_remote_with_residency(
                            RemoteRecord {
                                key: item.key.clone(),
                                version: remote.clone(),
                                payload,
                                local_hash: hash.clone(),
                            },
                            &PayloadCas::new(&self.repository_root)?,
                            |hash, size| {
                                if !remote_assets {
                                    return Ok(false);
                                }
                                residency
                                    .confirms(hash, size, &custody_context)
                                    .map_err(|_| super::StoreError::Validation {
                                        message: "Remote custody unavailable".into(),
                                    })
                            },
                        )?)?;
                        acknowledged.push(dirty);
                        hash
                    }
                    "identical" => {
                        acknowledged.push(dirty);
                        item.local_hash.clone()
                    }
                    "unchanged" => {
                        acknowledged.push(dirty);
                        base_hash
                    }
                    "publish" => {
                        publish_keys.push(dirty);
                        if remote == base_version {
                            base_hash
                        } else {
                            None
                        }
                    }
                    _ => return Err(SyncError::new("unresolved-conflict", 409)),
                };
                bases.push((item.key, remote, remote_hash));
                if applying {
                    CycleItemCounter::advance(options.cycle_items.as_deref());
                }
            }
        }
        let applied = records.len();
        Ok(Preparation::Ready(PreparedCycle {
            revision,
            previous: status.head,
            through,
            records,
            bases,
            acknowledged,
            publish_keys,
            scope_versions,
            scope_clears,
            scope_fences,
            reads,
            committed,
            applied,
            proposals,
            clear_acknowledged,
            activated: None,
            cancellation: options.cancellation.clone(),
            verified_bytes: options.verified_bytes.clone(),
            retryable_failure: options.retryable_failure.clone(),
            cycle_items: options.cycle_items.clone(),
            retry_budget,
        }))
    }
    /// No network work is allowed here. Production holds the JS mutation fence
    /// only for this activation and subsequent DBState/plugin cache refresh.
    pub(crate) fn server_activate_cycle(&mut self, ready: &mut PreparedCycle) -> Result<i64> {
        if let Some(revision) = ready.activated {
            return Ok(revision);
        }
        let revision = self.server_apply_advance(
            ready.revision,
            ready.previous.as_ref(),
            &ready.through,
            &ready.records,
            &ready.acknowledged,
            &ready.scope_versions,
            ReplicaAdvance {
                scope_clears: ready.scope_clears.clone(),
                publish_keys: ready.publish_keys.clone(),
                bases: ready.bases.clone(),
                finish_operation: ready.committed,
                clear_revision: ready.clear_acknowledged.then_some(ready.revision),
                scanned_revision: if ready.proposals == 0 {
                    Some(ready.revision)
                } else {
                    None
                },
            },
        )?;
        ready.activated = Some(revision);
        ready.records.clear()?;
        ready.bases.clear();
        self.connection
            .execute("DELETE FROM server_sync_objects", [])?;
        Ok(revision)
    }
    pub(crate) fn server_publish_cycle(&mut self, ready: &PreparedCycle) -> Result<CycleResult> {
        super::sync_selection::require_server(&self.connection)?;
        let next_revision = ready
            .activated
            .ok_or_else(|| SyncError::new("cycle-not-activated", 409))?;
        let config = self
            .server_config()?
            .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
        let mut client = ServerClient::with_retry_budget(
            config.clone(),
            ready.cancellation.clone(),
            ready.retry_budget.clone(),
        )?;
        client.verified_bytes = ready.verified_bytes.clone();
        client.retryable_failure = ready.retryable_failure.clone();
        let cache = Cache::open(&self.repository_root.join("server-sync").join(
            risunest_sync_wire::hash(
                format!("{}:{}", config.library_id, config.device_id).as_bytes(),
            ),
        ))?;
        let transfer = Transfer::new(&client, &cache)?;
        let through = &ready.through;
        let ack = client.request(
            Method::POST,
            "acks",
            &[],
            Some(canonical::encode(
                &serde_json::json!({"epoch":through.epoch,"seq":through.seq}),
            )?),
            &[],
            MAX_METADATA_BYTES,
        )?;
        if ack.status != 204 {
            return Err(response_error(ack));
        }
        if ready.proposals == 0 && ready.scope_fences.is_empty() {
            return Ok(CycleResult {
                endpoint: client.config().endpoint.clone(),
                phase: "idle".into(),
                local_revision: next_revision,
                head: through.clone(),
                conflict_count: 0,
                conflicts: Vec::new(),
                applied_records: ready.applied,
                proposed_records: 0,
            });
        }
        self.publish_server_cycle(
            &client,
            &transfer,
            &cache,
            through,
            ready.revision,
            &ready.reads,
            &ready.scope_fences,
            ready.cycle_items.as_deref(),
        )?;
        Ok(CycleResult {
            endpoint: client.config().endpoint.clone(),
            phase: "pending".into(),
            local_revision: next_revision,
            head: through.clone(),
            conflict_count: 0,
            conflicts: Vec::new(),
            applied_records: ready.applied,
            proposed_records: ready.proposals,
        })
    }
    fn effective_server_base(
        &self,
        key: &str,
        committed: bool,
    ) -> Result<(RecordVersion, Option<String>)> {
        if committed {
            let value: Option<(String, Option<String>)> = self
                .connection
                .query_row(
                    "SELECT version,local_hash FROM server_sync_operation_records WHERE key=?1",
                    [key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((version, hash)) = value {
                return Ok((parse(&version)?, hash));
            }
        }
        self.server_base(key)
    }
    fn server_base_candidates(
        &self,
        key: &str,
        cache: &Cache,
        committed: bool,
    ) -> Result<Vec<String>> {
        let (base, _) = self.effective_server_base(key, committed)?;
        // Missing local bases use the protocol's explicit full transfer path.
        Ok(cache.base_candidates(key, &base).unwrap_or_default())
    }
    fn prepare_server_cycle(
        &mut self,
        lease: &str,
        cache: &Cache,
        committed: bool,
        full_scan: bool,
        client: &ServerClient,
    ) -> Result<()> {
        self.connection.execute_batch("CREATE TEMP TABLE IF NOT EXISTS server_cycle_keys(key TEXT PRIMARY KEY); DELETE FROM server_cycle_keys; CREATE TEMP TABLE IF NOT EXISTS server_cycle_records(key TEXT PRIMARY KEY,version TEXT NOT NULL,local_hash TEXT,action TEXT NOT NULL DEFAULT '',remote TEXT NOT NULL DEFAULT ''); DELETE FROM server_cycle_records;")?;
        let (db, target) = self.read_view(Some(lease))?;
        let cas = PayloadCas::new(&self.repository_root)?;
        let full_marker: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM server_sync_dirty WHERE kind='full')",
            [],
            |r| r.get(0),
        )?;
        let mut after = None;
        loop {
            let page = outbox::dirty_page(
                db,
                after.as_ref().map(|k: &outbox::ServerDirtyKey| {
                    (k.kind.as_str(), k.key1.as_str(), k.key2.as_str())
                }),
                1024,
            )?;
            if page.is_empty() {
                break;
            }
            after = page.last().cloned();
            for key in page {
                if key.kind != "full" {
                    self.connection.execute(
                        "INSERT OR IGNORE INTO server_cycle_keys VALUES(?1)",
                        [projection::wire_key(&key)?],
                    )?;
                }
            }
        }
        if full_scan || full_marker {
            let mut after = None;
            loop {
                let page = projection::all_keys_page(
                    db,
                    &target.generation,
                    after.as_ref().map(|k: &outbox::ServerDirtyKey| {
                        (k.kind.as_str(), k.key1.as_str(), k.key2.as_str())
                    }),
                    1024,
                    target.revision,
                )?;
                if page.is_empty() {
                    break;
                }
                after = page.last().cloned();
                for key in page {
                    self.connection.execute(
                        "INSERT OR IGNORE INTO server_cycle_keys VALUES(?1)",
                        [projection::wire_key(&key)?],
                    )?;
                }
            }
            self.connection.execute(
                "INSERT OR IGNORE INTO server_cycle_keys SELECT key FROM server_sync_base",
                [],
            )?;
        }
        self.connection.execute(
            "INSERT OR IGNORE INTO server_cycle_keys SELECT key FROM server_sync_remote_dirty",
            [],
        )?;
        if committed {
            self.connection.execute("INSERT OR IGNORE INTO server_cycle_keys SELECT key FROM server_sync_operation_records",[])?;
        }
        let mut after = String::new();
        loop {
            let mut stmt = self.connection.prepare(
                "SELECT key FROM server_cycle_keys WHERE key>?1 ORDER BY key LIMIT 1024",
            )?;
            let keys = stmt
                .query_map([&after], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(stmt);
            if keys.is_empty() {
                break;
            }
            for key in keys {
                client.ensure_active()?;
                after = key.clone();
                let dirty = key_parts(&key, target.revision)?;
                let (base, base_hash) = self.effective_server_base(&key, committed)?;
                let (version, local_hash) = if let Some(payload) =
                    projection::project(db, &cas, &target.generation, &dirty)?
                {
                    let bytes = serde_json::to_vec(&payload)
                        .map_err(|_| SyncError::new("invalid-local-payload", 409))?;
                    let hash = risunest_sync_wire::hash(&bytes);
                    if base_hash.as_ref() == Some(&hash) {
                        (base, Some(hash))
                    } else {
                        let dependencies = projection::dependencies(&payload, &cas)?;
                        for bytes in payload.derived_objects.values() {
                            cache.put(bytes)?;
                        }
                        for hash in &dependencies {
                            if cache.cas.stat_object(hash)?.is_none() {
                                if cas.stat_object(hash)?.is_none() {
                                    let residency = crate::server_sync::residency::Residency::open(
                                        &self.repository_root,
                                    )?;
                                    let config = self
                                        .server_stored_config()?
                                        .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
                                    if let Some(proof) = residency.object(hash, None)? {
                                        if proof.config.library_id == config.library_id
                                            && proof.config.device_id == config.device_id
                                        {
                                            continue;
                                        }
                                    }
                                }
                                let mut source = cas
                                    .open_object(hash)?
                                    .ok_or_else(|| SyncError::new("missing-local-payload", 409))?;
                                let size = source.metadata()?.len();
                                cache.cas.prepare_reader_expected(&mut source, hash, size)?;
                            }
                        }
                        let projected = cache.project(
                            &payload,
                            &dependencies,
                            &relations(&dirty)?,
                            scopes(&dirty),
                        )?;
                        (projected.version, Some(projected.local_hash))
                    }
                } else {
                    (delete_version(&base), None)
                };
                self.connection.execute(
                    "INSERT INTO server_cycle_records(key,version,local_hash) VALUES(?1,?2,?3)",
                    params![key, json(&version)?, local_hash],
                )?;
            }
        }
        Ok(())
    }
    fn server_order_conflicts(
        &self,
        cache: &Cache,
        transfer: &Transfer<'_>,
        revision: i64,
    ) -> Result<BTreeSet<String>> {
        use crate::logical_records::LogicalRecordEnvelope as Envelope;
        let mut groups = BTreeSet::new();
        let incoming_ordered:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM server_cycle_records WHERE action='apply' AND (key GLOB 'r1:plugin:*' OR key GLOB 'r1:preset:*' OR key GLOB 'r1:character:*' OR key GLOB 'r1:conversation:*'))",[],|r|r.get(0))?;
        if !incoming_ordered {
            return Ok(groups);
        }
        let generation = super::active_generation(&self.connection)?;
        let mut characters = {
            let mut stmt = self
                .connection
                .prepare("SELECT character_id FROM characters WHERE generation=?1")?;
            let ids = stmt
                .query_map([&generation], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<BTreeSet<_>, _>>()?;
            ids
        };
        self.connection.execute_batch("CREATE TEMP TABLE IF NOT EXISTS server_cycle_order(key TEXT PRIMARY KEY,scope TEXT NOT NULL,position INTEGER NOT NULL); DELETE FROM server_cycle_order;")?;
        // Only index columns are copied, never plugin values or chat payloads.
        for (kind, table, id, parent, position) in [
            ("plugin", "plugin_storage", "storage_key", "''", "ordinal"),
            (
                "preset",
                "bot_presets",
                "preset_id",
                "''",
                "configured_index",
            ),
            (
                "character",
                "characters",
                "character_id",
                "''",
                "configured_index",
            ),
            (
                "conversation",
                "conversations",
                "conversation_id",
                "character_id",
                "configured_index",
            ),
        ] {
            let mut stmt = self.connection.prepare(&format!(
                "SELECT {id},{parent},{position} FROM {table} WHERE generation=?1"
            ))?;
            let mut rows = stmt.query([&generation])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let parent: String = row.get(1)?;
                let position: i64 = row.get(2)?;
                let key = outbox::ServerDirtyKey {
                    kind: kind.into(),
                    key1: if kind == "conversation" {
                        parent.clone()
                    } else {
                        id.clone()
                    },
                    key2: if kind == "conversation" {
                        id
                    } else {
                        String::new()
                    },
                    revision,
                };
                let scope = if kind == "conversation" {
                    format!("order:{parent}")
                } else {
                    kind.into()
                };
                self.connection.execute(
                    "INSERT INTO server_cycle_order VALUES(?1,?2,?3)",
                    params![projection::wire_key(&key)?, scope, position],
                )?;
            }
        }
        let mut after = String::new();
        loop {
            let page = cycle_page(&self.connection, &after)?;
            if page.is_empty() {
                break;
            }
            for item in page {
                after = item.key.clone();
                if item.action != "apply" {
                    continue;
                }
                let dirty = key_parts(&item.key, revision)?;
                if !["plugin", "preset", "character", "conversation"].contains(&dirty.kind.as_str())
                {
                    continue;
                }
                let remote: RecordVersion = parse(&item.remote)?;
                self.connection
                    .execute("DELETE FROM server_cycle_order WHERE key=?1", [&item.key])?;
                if matches!(remote, RecordVersion::Live { .. }) {
                    transfer.download_record_metadata(
                        &remote,
                        &self.server_base_candidates(&item.key, cache, false)?,
                        &self.server_base(&item.key)?.0,
                    )?;
                    let (payload, _) = cache.restore(&remote)?;
                    let position = match payload.record {
                        Envelope::Plugin { ordinal, .. } => ordinal,
                        Envelope::Preset {
                            configured_index, ..
                        }
                        | Envelope::Character {
                            configured_index, ..
                        }
                        | Envelope::Conversation {
                            configured_index, ..
                        } => configured_index,
                        _ => return Err(SyncError::new("server-record-family-mismatch", 409)),
                    };
                    let scope = if dirty.kind == "conversation" {
                        format!("order:{}", dirty.key1)
                    } else {
                        dirty.kind.clone()
                    };
                    let position = i64::try_from(position)
                        .map_err(|_| SyncError::new("invalid-record-order", 409))?;
                    self.connection.execute(
                        "INSERT INTO server_cycle_order VALUES(?1,?2,?3)",
                        params![item.key, scope, position],
                    )?;
                    if dirty.kind == "character" {
                        characters.insert(dirty.key1.clone());
                    }
                } else if dirty.kind == "character" {
                    characters.remove(&dirty.key1);
                    let children: bool = self.connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM server_cycle_order WHERE scope=?1)",
                        [format!("order:{}", dirty.key1)],
                        |r| r.get(0),
                    )?;
                    // Check after all child deletions have been processed below.
                    if children {
                        groups.insert(format!("family:{}", dirty.key1));
                    }
                }
            }
        }
        groups.retain(|group| {
            self.connection
                .query_row::<bool, _, _>(
                    "SELECT EXISTS(SELECT 1 FROM server_cycle_order WHERE scope=?1)",
                    [format!("order:{}", group.strip_prefix("family:").unwrap())],
                    |r| r.get(0),
                )
                .unwrap_or(true)
        });
        let mut scopes_stmt = self
            .connection
            .prepare("SELECT DISTINCT scope FROM server_cycle_order WHERE scope LIKE 'order:%'")?;
        for scope in scopes_stmt.query_map([], |r| r.get::<_, String>(0))? {
            let scope = scope?;
            let parent = scope.strip_prefix("order:").unwrap();
            if !characters.contains(parent) {
                groups.insert(format!("family:{parent}"));
            }
        }
        let mut stmt=self.connection.prepare("SELECT DISTINCT scope FROM server_cycle_order GROUP BY scope,position HAVING count(*)>1")?;
        for group in stmt.query_map([], |r| r.get::<_, String>(0))? {
            groups.insert(group?);
        }
        Ok(groups)
    }
    fn server_conflict_backups(
        &mut self,
        cache: &Cache,
        transfer: &Transfer<'_>,
        client: &ServerClient,
        revision: i64,
        head: &RemoteHead,
    ) -> Result<()> {
        // Both library-only packages must pass the portable archive verifier before
        // any live activation or server publication. A partial directory is not
        // a completed backup; only the final receipt makes it discoverable.
        struct Cancel<'a>(&'a ServerClient);
        impl crate::local_backup::CancellationProbe for Cancel<'_> {
            fn is_cancelled(&self) -> bool {
                self.0.ensure_active().is_err()
            }
        }
        let root = self
            .repository_root
            .join("server-sync/backups")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&root)?;
        let scratch = tempfile::Builder::new()
            .prefix("working-")
            .tempdir_in(&root)?;
        std::fs::create_dir_all(scratch.path().join("local-staging"))?;
        std::fs::create_dir_all(scratch.path().join("remote-staging"))?;
        let cancel = Cancel(client);
        let local = crate::portable_backup::create_verified_library_backup(
            self,
            revision,
            &root.join("local.risunest"),
            &scratch.path().join("local-staging"),
            &cancel,
        )
        .map_err(|_| SyncError::new("complete-local-backup-required", 409))?;
        let mut remote_store = PersistentStore::open(&scratch.path().join("remote-source"))?;
        remote_store.server_bind(
            &self
                .server_config()?
                .ok_or_else(|| SyncError::new("server-not-bound", 409))?,
        )?;
        let remote_cas = PayloadCas::new(&remote_store.repository_root)?;
        let mut records = ValidatedRecords::new()?;
        let mut after = String::new();
        loop {
            let page = {
                let mut stmt=self.connection.prepare("SELECT key,version FROM server_sync_remote WHERE key>?1 ORDER BY key LIMIT 1024")?;
                let result = stmt
                    .query_map([&after], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                result
            };
            if page.is_empty() {
                break;
            }
            for (key, version) in page {
                client.ensure_active()?;
                after = key.clone();
                let version: RecordVersion = parse(&version)?;
                if !matches!(version, RecordVersion::Live { .. }) {
                    continue;
                }
                transfer.download_record(
                    &version,
                    &self.server_base_candidates(&key, cache, false)?,
                    &self.server_base(&key)?.0,
                )?;
                let (payload, hash) = cache.restore(&version)?;
                let dependencies = projection::dependencies(&payload, &cache.cas)?;
                let dirty = key_parts(&key, revision)?;
                if cache
                    .project(&payload, &dependencies, &relations(&dirty)?, scopes(&dirty))?
                    .version
                    != version
                {
                    return Err(SyncError::new("server-descriptor-semantics-mismatch", 409));
                }
                remote_store
                    .promote_server_dependencies(cache, &dependencies, || client.ensure_active())?;
                records.push(validate_remote(
                    RemoteRecord {
                        key,
                        version,
                        payload: Some(payload),
                        local_hash: Some(hash),
                    },
                    &remote_cas,
                )?)?;
            }
        }
        let initial_revision = remote_store.revision()?;
        let remote_revision = remote_store.server_apply_advance(
            initial_revision,
            None,
            head,
            &records,
            &[],
            &[],
            ReplicaAdvance::default(),
        )?;
        // A complete verified mirror has an authoritative alias inventory, even
        // for references already missing on the server. No legacy files exist
        // in this newly created staging repository.
        let generation = super::active_generation(&remote_store.connection)?;
        let authority = serde_json::to_string(&super::AssetRepositoryAuthorityState::V2 {
            migration_id: head.head_id.clone(),
            compatibility_hash: head.head_id.clone(),
        })
        .map_err(|_| SyncError::new("backup-metadata", 409))?;
        remote_store.connection.execute(
            "UPDATE asset_repository_authority SET value=?2 WHERE generation=?1",
            params![generation, authority],
        )?;
        let authority = serde_json::to_string(&super::ColdPayloadAuthorityState::V2 {
            migration_id: head.head_id.clone(),
            compatibility_hash: head.head_id.clone(),
        })
        .map_err(|_| SyncError::new("backup-metadata", 409))?;
        remote_store.connection.execute("INSERT INTO cold_payload_authority(generation,value) VALUES(?1,?2) ON CONFLICT(generation) DO UPDATE SET value=excluded.value",params![generation,authority])?;
        let remote = crate::portable_backup::create_verified_library_backup(
            &mut remote_store,
            remote_revision,
            &root.join("remote.risunest"),
            &scratch.path().join("remote-staging"),
            &cancel,
        )
        .map_err(|_| SyncError::new("complete-remote-backup-required", 409))?;
        client.ensure_active()?;
        if self.revision()? != revision {
            return Err(SyncError::new("local-revision-changed", 409));
        }
        let receipt=serde_json::to_vec(&serde_json::json!({"format":"risunest-portable-backup","scope":"library","head":head,"localRevision":revision,"localHash":local,"remoteHash":remote})).map_err(|_|SyncError::new("backup-receipt-encoding",409))?;
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("complete.json"))?;
        file.write_all(&receipt)?;
        file.sync_all()?;
        // The verified packages now own every byte. Close database handles
        // before removing the temporary mirror, especially on Windows.
        remote_store.server_unbind()?;
        drop(remote_store);
        drop(records);
        scratch.close()?;
        Ok(())
    }
    fn promote_server_dependencies(
        &mut self,
        cache: &Cache,
        hashes: &[String],
        check_active: impl Fn() -> Result<()>,
    ) -> Result<()> {
        self.promote_server_dependencies_with_residency(cache, hashes, None, check_active)
    }
    fn promote_server_dependencies_with_residency(
        &mut self,
        cache: &Cache,
        hashes: &[String],
        remote: Option<(&crate::server_sync::residency::Residency, &str)>,
        check_active: impl Fn() -> Result<()>,
    ) -> Result<()> {
        let native = PayloadCas::new(&self.repository_root)?;
        for batch in hashes.chunks(256) {
            check_active()?;
            let registrations = batch
                .iter()
                .map(|hash| {
                    let size = match cache.cas.stat_object(hash)? {
                        Some(size) => size,
                        None => remote
                            .and_then(|(residency, context)| {
                                residency.object(hash, Some(context)).transpose()
                            })
                            .transpose()?
                            .map(|object| object.size)
                            .ok_or_else(|| SyncError::new("missing-downloaded-payload", 409))?,
                    };
                    Ok(AssetObjectRegistration {
                        object_hash: hash.clone(),
                        byte_size: size,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let _guard = crate::asset_repository::coordinator::lock_repository_mutation()?;
            // Persist GC roots before copying any bytes. Batch metadata commits,
            // but never hold a SQLite write transaction during object IO.
            let tx = self.connection.transaction()?;
            for object in &registrations {
                tx.execute(
                    "INSERT INTO server_sync_objects VALUES(?1,?2,?3) ON CONFLICT(hash) DO NOTHING",
                    params![
                        object.object_hash,
                        object.byte_size as i64,
                        crate::asset_repository::object_physical_key(&object.object_hash)
                    ],
                )?;
            }
            tx.commit()?;
            for object in &registrations {
                check_active()?;
                if native.stat_object(&object.object_hash)? != Some(object.byte_size) {
                    if cache.cas.stat_object(&object.object_hash)?.is_none()
                        && remote.is_some_and(|(residency, context)| {
                            residency
                                .confirms(&object.object_hash, Some(object.byte_size), context)
                                .unwrap_or(false)
                        })
                    {
                        continue;
                    }
                    let mut source = cache
                        .cas
                        .open_object(&object.object_hash)?
                        .ok_or_else(|| SyncError::new("missing-downloaded-payload", 409))?;
                    native.prepare_reader_expected(
                        &mut source,
                        &object.object_hash,
                        object.byte_size,
                    )?;
                }
            }
            AssetObjectCatalog::new(&mut self.connection).register(&registrations, 0)?;
        }
        Ok(())
    }
}
struct CycleRecord {
    key: String,
    version: String,
    local_hash: Option<String>,
    action: String,
    remote: String,
}
struct PageBuilder<'a> {
    db: &'a Connection,
    page: i64,
    used: usize,
    body: ChangeSet,
}
impl<'a> PageBuilder<'a> {
    fn new(db: &'a Connection) -> Self {
        Self {
            db,
            page: 0,
            used: 256,
            body: ChangeSet {
                changes: Vec::new(),
                read_fences: Vec::new(),
                scope_fences: Vec::new(),
            },
        }
    }
    fn reserve(&mut self, value: &impl Serialize, count: usize) -> Result<()> {
        let size = canonical::encode(value)?.len() + 1;
        if self.used + size > MAX_METADATA_BYTES || count >= 1024 {
            self.flush()?;
        }
        self.used += size;
        Ok(())
    }
    fn change(&mut self, value: RecordChange) -> Result<()> {
        self.reserve(&value, self.body.changes.len())?;
        self.body.changes.push(value);
        Ok(())
    }
    fn read(&mut self, value: ReadFence) -> Result<()> {
        self.reserve(&value, self.body.read_fences.len())?;
        self.body.read_fences.push(value);
        Ok(())
    }
    fn scope(&mut self, value: ScopeFence) -> Result<()> {
        self.reserve(&value, self.body.scope_fences.len())?;
        self.body.scope_fences.push(value);
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        if self.body.changes.is_empty()
            && self.body.read_fences.is_empty()
            && self.body.scope_fences.is_empty()
        {
            return Ok(());
        }
        self.body.validate_page()?;
        let bytes = canonical::encode(&self.body)?;
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(SyncError::new("metadata-too-large", 413));
        }
        self.db.execute(
            "INSERT INTO server_sync_operation_pages VALUES(?1,?2)",
            params![self.page, bytes],
        )?;
        self.page += 1;
        self.body.changes.clear();
        self.body.read_fences.clear();
        self.body.scope_fences.clear();
        self.used = 256;
        Ok(())
    }
}
fn cycle_page(db: &Connection, after: &str) -> Result<Vec<CycleRecord>> {
    let mut stmt=db.prepare("SELECT key,version,local_hash,action,remote FROM server_cycle_records WHERE key>?1 ORDER BY key LIMIT 1024")?;
    let rows = stmt
        .query_map([after], |r| {
            Ok(CycleRecord {
                key: r.get(0)?,
                version: r.get(1)?,
                local_hash: r.get(2)?,
                action: r.get(3)?,
                remote: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}
