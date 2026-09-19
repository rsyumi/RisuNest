//! This adapter lives in PDS's module scope so SQLite state cannot leak through
//! IPC. Network work uses a dedicated job-store connection and a frozen read lease.
#[cfg(test)]
#[path = "engine_promotion_tests.rs"]
mod promotion_tests;

use super::{
    asset_object_catalog::{AssetObjectCatalog, AssetObjectRegistration},
    server_sync_apply::{
        validate_remote_with_residency, RemoteRecord, ReplicaAdvance, ValidatedRecords,
    },
    server_sync_outbox as outbox, server_sync_projection as projection,
    server_sync_sections as sections, PersistentStore,
};
use crate::{
    asset_repository::PayloadCas,
    logical_records::{decode_logical_record_key, encode_logical_record_key, LogicalRecordLocator},
    server_sync::{
        cache::Cache,
        client::{
            is_ambiguous_transient, response_error, Reply, RequestAttempt, RetryBudget,
            ServerClient,
        },
        planner::{self, Decision},
        remote,
        transfer::Transfer,
        Result, SyncError,
    },
};
use reqwest::Method;
use risunest_external_storage_format::section::{InlineOrObject, SectionEntry, SectionValue};
use risunest_sync_wire::{
    canonical, change_digest::ChangeDigest, ChangeSet, CommitIntent, Domain, ReadFence, Receipt,
    RecordChange, RecordVersion, RemoteHead, ScopeFence, Sequence, MAX_METADATA_BYTES,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Resolution {
    KeepLocal,
    KeepRemote,
}

#[derive(Clone, Copy)]
enum ConflictReferenceMode<'a> {
    DirectRestore { source_root: &'a Path },
    PortableExport { source_root: &'a Path },
}

impl<'a> ConflictReferenceMode<'a> {
    fn source_root(self) -> &'a Path {
        match self {
            Self::DirectRestore { source_root } | Self::PortableExport { source_root } => {
                source_root
            }
        }
    }

    fn hydrates_all(self) -> bool {
        matches!(self, Self::PortableExport { .. })
    }
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
                head_id: "b".repeat(64),
                ..RemoteHead::genesis(config.library_id.clone(), "epoch".into()).unwrap()
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

    #[test]
    fn slow_ambiguous_commits_and_receipt_misses_stay_within_retry_budget() {
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
                head_id: "b".repeat(64),
                ..RemoteHead::genesis(config.library_id.clone(), "epoch".into()).unwrap()
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
        let started = std::time::Instant::now();
        let now = std::sync::Arc::new(std::sync::Mutex::new(started));
        let server_now = now.clone();
        let task = std::thread::spawn(move || {
            for attempt in 0..7 {
                let (mut commit, _) = server.accept().unwrap();
                assert!(read_request(&mut commit).starts_with(b"POST /commits HTTP/1.1\r\n"));
                *server_now.lock().unwrap() += Duration::from_secs(30);
                drop(commit);
                if attempt == 6 {
                    break;
                }
                let (mut lookup, _) = server.accept().unwrap();
                assert!(read_request(&mut lookup)
                    .starts_with(format!("GET /operations/{operation} HTTP/1.1\r\n").as_bytes()));
                lookup
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                lookup.flush().unwrap();
            }
        });
        let clock_now = now.clone();
        let sleep_now = now.clone();
        let budget = std::sync::Arc::new(RetryBudget::with_driver(
            None,
            std::sync::Arc::new(move || *clock_now.lock().unwrap()),
            std::sync::Arc::new(move |duration| *sleep_now.lock().unwrap() += duration),
        ));
        let client = ServerClient::with_retry_budget(config, None, budget).unwrap();
        let error = submit_commit(&client, &intent).err().unwrap();
        assert_eq!(error.code, "sync-retry-budget-exhausted");
        assert!(now.lock().unwrap().duration_since(started) <= Duration::from_secs(5 * 60));
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
    bases: Vec<(Domain, String, RecordVersion, Option<String>)>,
    acknowledged: Vec<outbox::ServerDirtyKey>,
    publish_keys: Vec<outbox::ServerDirtyKey>,
    scope_versions: Vec<(String, String)>,
    scope_clears: Vec<(String, String)>,
    scope_fences: Vec<ScopeFence>,
    reads: BTreeMap<String, RecordVersion>,
    committed: bool,
    pub applied: usize,
    plugins_changed: bool,
    device_plugins_changed: bool,
    pub proposals: usize,
    clear_acknowledged: bool,
    section_honored: bool,
    section_participation: Vec<(Domain, String)>,
    section_applied: usize,
    section_proposals: usize,
    activated: Option<i64>,
    cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    verified_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    retryable_failure: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    cycle_items: Option<std::sync::Arc<CycleItemCounter>>,
    retry_budget: std::sync::Arc<RetryBudget>,
}
impl PreparedCycle {
    pub(crate) fn plugin_changes(&self) -> (bool, bool) {
        let device = self.section_honored && self.device_plugins_changed;
        (self.plugins_changed || device, device)
    }
}

/// A published embedding body is bounded by the store's own dimension limit.
const MAX_SECTION_OBJECT_BYTES: usize = 1024 * 1024;

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
        let ambiguous = match client.request_ambiguous_mutation(
            Method::POST,
            "commits",
            Some(canonical::encode(intent)?),
            &[("if-match", intent.expected_head.etag())],
            MAX_METADATA_BYTES,
        )? {
            RequestAttempt::Response(reply) if matches!(reply.status, 502 | 503 | 504) => {
                client.wait_transient_response(
                    reply.retry_after,
                    "server-unreachable",
                    reply.attempt_duration,
                )?;
                true
            }
            RequestAttempt::Response(reply) => return Ok(reply),
            RequestAttempt::Failure {
                error,
                attempt_duration,
            } if is_ambiguous_transient(&error) => {
                client.wait_after_ambiguous(&error, attempt_duration)?;
                true
            }
            RequestAttempt::Failure { error, .. } => return Err(error),
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
        client.charge_retry_resolution(status.attempt_duration)?;
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
fn remote_version(db: &Connection, domain: Domain, key: &str) -> Result<RecordVersion> {
    let value: Option<String> = db
        .query_row(
            "SELECT version FROM server_sync_remote WHERE domain=?1 AND key=?2",
            params![domain.as_str(), key],
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
        LogicalRecordLocator::Plugin { owner, storage_key } => ("plugin", owner, storage_key),
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
    /// Emits one section's proposals. Sections bracket the library in the
    /// change order, so hypa is written before it and local plugins after it.
    fn publish_section_changes(
        &self,
        domain: Domain,
        digest: &mut ChangeDigest,
        builder: &mut PageBuilder<'_>,
        revision: i64,
    ) -> Result<()> {
        let mut after = (domain.as_str().to_owned(), String::new());
        loop {
            let page = self.section_page(&after)?;
            if page.is_empty() {
                return Ok(());
            }
            for (name, key, action, remote, version) in page {
                if name != domain.as_str() {
                    return Ok(());
                }
                after = (name, key.clone());
                if action != "publish" {
                    continue;
                }
                let proposed: RecordVersion = parse(&version)?;
                let change = RecordChange {
                    domain,
                    key: key.clone(),
                    before: parse(&remote)?,
                    after: proposed.clone(),
                };
                digest.change(&change)?;
                builder.change(change)?;
                self.server_record_prepared(
                    domain,
                    &key,
                    &proposed,
                    None,
                    &outbox::ServerDirtyKey {
                        kind: String::new(),
                        key1: String::new(),
                        key2: String::new(),
                        revision,
                    },
                )?;
            }
        }
    }
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
        sections_included: bool,
    ) -> Result<()> {
        if self.server_pending()?.is_some() {
            return Err(SyncError::new("operation-already-pending", 409));
        }
        self.connection.execute_batch("DELETE FROM server_sync_operation_records; DELETE FROM server_sync_operation_pages; DELETE FROM server_sync_operation_scopes;")?;
        let mut digest = ChangeDigest::new();
        let mut builder = PageBuilder::new(&self.connection);
        if sections_included {
            self.publish_section_changes(Domain::Hypa, &mut digest, &mut builder, revision)?;
        }
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
                    domain: Domain::Library,
                    key: item.key.clone(),
                    before,
                    after: after.clone(),
                };
                digest.change(&change)?;
                builder.change(change)?;
                self.server_record_prepared(
                    Domain::Library,
                    &item.key,
                    &after,
                    item.local_hash.as_deref(),
                    &key_parts(&item.key, revision)?,
                )?;
            }
        }
        if sections_included {
            self.publish_section_changes(
                Domain::LocalPlugins,
                &mut digest,
                &mut builder,
                revision,
            )?;
        }
        for (key, version) in reads {
            let fence = ReadFence {
                domain: Domain::Library,
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
        let mut after = (Domain::Hypa, String::new());
        loop {
            let records = {
                let mut stmt=self.connection.prepare("SELECT domain,key,version FROM server_sync_operation_records WHERE (domain,key)>(?1,?2) ORDER BY domain,key LIMIT 1024")?;
                let rows = stmt
                    .query_map(params![after.0.as_str(), after.1], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            };
            if records.is_empty() {
                break;
            }
            for (domain, key, version) in records {
                transfer.client.ensure_active()?;
                let domain = Domain::try_from(domain.as_str())
                    .map_err(|_| SyncError::new("invalid-local-section", 409))?;
                after = (domain, key.clone());
                let version: RecordVersion = parse(&version)?;
                let mut objects = transfer.cache.closure(&version)?;
                let mut base_lease = false;
                let (base, _) = self.server_base(domain, &key)?;
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
                let bases = self.server_base_candidates(domain, &key, transfer.cache, false)?;
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
        let participation = sections::participation(self.device_store()?)?;
        let fetched = Domain::ALL
            .into_iter()
            .filter(|domain| {
                *domain == Domain::Library
                    || participation.iter().any(|(chosen, _)| chosen == domain)
            })
            .collect::<Vec<_>>();
        let through = remote::refresh(&mut self.connection, &client, &observed, &fetched)?;
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
                let (base, _) = self.effective_server_base(Domain::Library, &item.key, committed)?;
                let remote = remote_version(&self.connection, Domain::Library, &item.key)?;
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
                        let (parent_base, _) = self.effective_server_base(Domain::Library, &parent, committed)?;
                        let parent_remote =
                            remote_version(&self.connection, Domain::Library, &parent)?;
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
            self.server_conflict_references(&cache, &transfer, &client, revision, &through)?;
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
        let remote_assets = self.device_store()?.asset_residency_policy()?
            == crate::server_sync::residency::AssetPolicy::Remote;
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
                let (base_version, base_hash) = self.effective_server_base(Domain::Library, &item.key, committed)?;
                let remote_hash = match item.action.as_str() {
                    "apply" => {
                        let (payload, hash) = if matches!(remote, RecordVersion::Live { .. }) {
                            let base_candidates =
                                self.server_base_candidates(Domain::Library, &item.key, &cache, committed)?;
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
                bases.push((Domain::Library, item.key, remote, remote_hash));
                if applying {
                    CycleItemCounter::advance(options.cycle_items.as_deref());
                }
            }
        }
        let (section_applied, section_proposals) =
            self.prepare_server_sections(&client, &transfer, &cache, &participation, committed)?;
        let applied = records.len();
        let plugins_changed = records.affects_plugins();
        let device_plugins_changed = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM server_section_records WHERE domain='local-plugins' AND action='apply')",
            [], |row| row.get(0),
        )?;
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
            plugins_changed,
            device_plugins_changed,
            proposals,
            clear_acknowledged,
            section_honored: false,
            section_participation: participation,
            section_applied,
            section_proposals,
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
        if self.revision()? != ready.revision {
            return Err(SyncError::new("local-revision-changed", 409));
        }
        // A participation choice made after this cycle was planned cancels the
        // section work rather than applying the previous choice.
        let honored = sections::participation(self.device_store()?)? == ready.section_participation;
        let mut bases = ready.bases.clone();
        let mut applied_sections = vec![Domain::Library];
        if honored {
            let cache = self.server_cache()?;
            self.write_prepared_server_sections(|name, key, action, remote, version| {
                let domain = Domain::try_from(name)
                    .map_err(|_| SyncError::new("invalid-local-section", 409))?;
                let write = match action {
                    "apply" => Some(Self::section_apply_write(&cache, domain, &parse::<RecordVersion>(version)?)?),
                    "mark" => Self::section_mark_write(&cache, domain, key, &parse::<RecordVersion>(version)?)?,
                    _ => None,
                };
                bases.push((domain, key.to_owned(), parse::<RecordVersion>(remote)?, None));
                Ok(write)
            })?;
            applied_sections.extend(ready.section_participation.iter().map(|(domain, _)| *domain));
            applied_sections.sort();
        }
        ready.section_honored = honored;
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
                bases,
                finish_operation: ready.committed,
                clear_revision: ready.clear_acknowledged.then_some(ready.revision),
                applied_sections,
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
        let sections_included = ready.section_honored
            && sections::participation(self.device_store()?)? == ready.section_participation;
        let proposals = ready.proposals + if sections_included { ready.section_proposals } else { 0 };
        // Only the sections this device actually applied are acknowledged. With
        // none applied there is nothing to report, so no request is made.
        let sections = self.server_applied_sections(&through.epoch)?;
        if !sections.is_empty() {
            let ack = client.request(
                Method::POST,
                "acks",
                &[],
                Some(canonical::encode(
                    &serde_json::json!({"epoch":through.epoch,"sections":sections}),
                )?),
                &[],
                MAX_METADATA_BYTES,
            )?;
            if ack.status != 204 {
                return Err(response_error(ack));
            }
        }
        let applied_records = ready.applied + if ready.section_honored { ready.section_applied } else { 0 };
        if proposals == 0 && ready.scope_fences.is_empty() {
            return Ok(CycleResult {
                endpoint: client.config().endpoint.clone(),
                phase: "idle".into(),
                local_revision: next_revision,
                head: through.clone(),
                conflict_count: 0,
                conflicts: Vec::new(),
                applied_records,
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
            sections_included,
        )?;
        Ok(CycleResult {
            endpoint: client.config().endpoint.clone(),
            phase: "pending".into(),
            local_revision: next_revision,
            head: through.clone(),
            conflict_count: 0,
            conflicts: Vec::new(),
            applied_records,
            proposed_records: proposals,
        })
    }

    fn server_cache(&self) -> Result<Cache> {
        let config = self
            .server_config()?
            .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
        Cache::open(&self.repository_root.join("server-sync").join(
            risunest_sync_wire::hash(
                format!("{}:{}", config.library_id, config.device_id).as_bytes(),
            ),
        ))
    }
    fn project_section(&self, cache: &Cache, entry: &sections::LocalEntry) -> Result<RecordVersion> {
        let bytes = entry
            .entry
            .encode()
            .map_err(|_| SyncError::new("invalid-local-section-entry", 409))?;
        let mut dependencies = Vec::new();
        if let Some(object) = &entry.object {
            dependencies.push(cache.put(object)?);
        }
        Ok(cache
            .project_bytes(&bytes, &dependencies, &[], Vec::new())?
            .version)
    }
    /// The commit a section publication started now would land in. A removal
    /// this device has not published yet is stamped with it as its entry is
    /// first projected, so the entry encodes the same bytes on every later
    /// cycle and the replica can settle it.
    fn section_publication_generation(&self, domain: Domain) -> Result<Sequence> {
        let applied: Option<String> = self
            .connection
            .query_row(
                "SELECT applied_seq FROM server_sync_remote_sections WHERE domain=?1",
                params![domain.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let applied = match applied {
            Some(value) => Sequence::try_from(value)
                .map_err(|_| SyncError::new("invalid-remote-section-sequence", 502))?,
            None => Sequence::from(0u64),
        };
        applied
            .next()
            .map_err(|_| SyncError::new("invalid-remote-section-sequence", 502))
    }
    /// Decides every section key this cycle touches. A section settles on the
    /// entry's own write clock, so there is always one winner and no conflict
    /// for the caller to resolve.
    fn prepare_server_sections(
        &self,
        client: &ServerClient,
        transfer: &Transfer<'_>,
        cache: &Cache,
        participation: &[(Domain, String)],
        committed: bool,
    ) -> Result<(usize, usize)> {
        self.connection.execute_batch("CREATE TEMP TABLE IF NOT EXISTS server_section_keys(domain TEXT NOT NULL,key TEXT NOT NULL,PRIMARY KEY(domain,key)); DELETE FROM server_section_keys; CREATE TEMP TABLE IF NOT EXISTS server_section_records(domain TEXT NOT NULL,key TEXT NOT NULL,action TEXT NOT NULL,remote TEXT NOT NULL,version TEXT NOT NULL,PRIMARY KEY(domain,key)); DELETE FROM server_section_records;")?;
        if participation.is_empty() {
            return Ok((0, 0));
        }
        let now_ms = u64::try_from(
            crate::persistent_store::device_store::now_ms()
                .map_err(|_| SyncError::new("invalid-device-clock", 500))?,
        )
        .map_err(|_| SyncError::new("invalid-device-clock", 500))?;
        for (domain, _) in participation {
            let mut after = String::new();
            loop {
                let page = sections::pending_page(
                    self.device_store()?,
                    *domain,
                    &after,
                    sections::SECTION_PAGE,
                )?;
                if page.is_empty() {
                    break;
                }
                for key in page {
                    after.clone_from(&key);
                    self.connection.execute(
                        "INSERT OR IGNORE INTO server_section_keys VALUES(?1,?2)",
                        params![domain.as_str(), key],
                    )?;
                }
            }
            self.connection.execute("INSERT OR IGNORE INTO server_section_keys SELECT domain,key FROM server_sync_remote_dirty WHERE domain=?1",[domain.as_str()])?;
        }
        let mut applied = 0usize;
        let mut proposals = 0usize;
        let mut after = (String::new(), String::new());
        loop {
            let page = {
                let mut statement = self.connection.prepare("SELECT domain,key FROM server_section_keys WHERE (domain,key)>(?1,?2) ORDER BY domain,key LIMIT 512")?;
                let rows = statement
                    .query_map(params![after.0, after.1], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            };
            if page.is_empty() {
                break;
            }
            for (name, key) in page {
                client.ensure_active()?;
                after = (name.clone(), key.clone());
                let domain = Domain::try_from(name.as_str())
                    .map_err(|_| SyncError::new("invalid-local-section", 409))?;
                let remote = remote_version(&self.connection, domain, &key)?;
                let (base, _) = self.effective_server_base(domain, &key, committed)?;
                let generation = self.section_publication_generation(domain)?;
                sections::stamp_unpublished_removal(
                    self.device_store()?,
                    domain,
                    &key,
                    &generation,
                    now_ms,
                )?;
                let local = sections::read_local(self.device_store()?, domain, &key)?;
                let (action, version) = if remote == base {
                    match &local {
                        Some(entry) if !entry.published => {
                            // The remote may already carry this exact value from
                            // an earlier operation whose receipt arrived late.
                            let version = self.project_section(cache, entry)?;
                            if version == remote {
                                ("mark", version)
                            } else {
                                ("publish", version)
                            }
                        }
                        _ => continue,
                    }
                } else if matches!(remote, RecordVersion::Live { .. }) {
                    transfer.download_record(&remote, &[], &base)?;
                    let (bytes, _) = cache.restore_bytes(&remote)?;
                    let entry = SectionEntry::decode(&bytes)
                        .map_err(|_| SyncError::new("invalid-remote-section-entry", 502))?;
                    if sections::kind_of(domain) != Some(entry.kind) || entry.key != key {
                        return Err(SyncError::new("invalid-remote-section-entry", 502));
                    }
                    match sections::resolve(local.as_ref(), &entry)
                        .map_err(|_| SyncError::new("section-version-conflict", 409))?
                    {
                        sections::Outcome::Apply => ("apply", remote.clone()),
                        sections::Outcome::Publish => (
                            "publish",
                            self.project_section(
                                cache,
                                local.as_ref().ok_or_else(|| {
                                    SyncError::new("invalid-local-section-entry", 409)
                                })?,
                            )?,
                        ),
                        sections::Outcome::Settled => ("mark", remote.clone()),
                    }
                } else {
                    match &local {
                        Some(entry) => ("publish", self.project_section(cache, entry)?),
                        None => ("mark", remote.clone()),
                    }
                };
                match action {
                    "apply" => applied += 1,
                    "publish" => proposals += 1,
                    _ => (),
                }
                self.connection.execute(
                    "INSERT INTO server_section_records VALUES(?1,?2,?3,?4,?5)",
                    params![name, key, action, json(&remote)?, json(&version)?],
                )?;
            }
        }
        Ok((applied, proposals))
    }
    fn section_page(
        &self,
        after: &(String, String),
    ) -> Result<Vec<(String, String, String, String, String)>> {
        let mut statement = self.connection.prepare("SELECT domain,key,action,remote,version FROM server_section_records WHERE (domain,key)>(?1,?2) ORDER BY domain,key LIMIT 512")?;
        let rows = statement
            .query_map(params![after.0, after.1], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
    /// Reads back the entry this cycle decided to take, with the vector body
    /// when the value was too large to ride inside the entry.
    fn section_apply_write(
        cache: &Cache,
        domain: Domain,
        version: &RecordVersion,
    ) -> Result<sections::SectionWrite> {
        let (bytes, _) = cache.restore_bytes(version)?;
        let entry = SectionEntry::decode(&bytes)
            .map_err(|_| SyncError::new("invalid-remote-section-entry", 502))?;
        let object = match &entry.value {
            SectionValue::Hypa(value) => match &value.vector {
                InlineOrObject::Object(reference) => Some(cache.read(
                    &hex::encode(reference.content_sha256),
                    MAX_SECTION_OBJECT_BYTES,
                )?),
                InlineOrObject::Inline(_) => None,
            },
            _ => None,
        };
        Ok(sections::SectionWrite::Apply {
            domain,
            entry,
            object,
        })
    }
    /// Marks only the section row carried by the cursor this cycle prepared.
    /// A newer local write under the same key must remain unpublished.
    fn section_mark_write(
        cache: &Cache,
        domain: Domain,
        key: &str,
        version: &RecordVersion,
    ) -> Result<Option<sections::SectionWrite>> {
        if !matches!(version, RecordVersion::Live { .. }) {
            return Ok(None);
        }
        let (bytes, _) = cache.restore_bytes(version)?;
        let entry = SectionEntry::decode(&bytes)
            .map_err(|_| SyncError::new("invalid-remote-section-entry", 502))?;
        if sections::kind_of(domain) != Some(entry.kind) || entry.key != key {
            return Err(SyncError::new("invalid-remote-section-entry", 502));
        }
        let version = entry
            .version
            .ok_or_else(|| SyncError::new("invalid-remote-section-entry", 502))?;
        Ok(Some(sections::SectionWrite::MarkVersion {
            domain,
            key: key.to_owned(),
            version,
        }))
    }
    fn effective_server_base(
        &self,
        domain: Domain,
        key: &str,
        committed: bool,
    ) -> Result<(RecordVersion, Option<String>)> {
        if committed {
            let value: Option<(String, Option<String>)> = self
                .connection
                .query_row(
                    "SELECT version,local_hash FROM server_sync_operation_records WHERE domain=?1 AND key=?2",
                    params![domain.as_str(), key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((version, hash)) = value {
                return Ok((parse(&version)?, hash));
            }
        }
        self.server_base(domain, key)
    }
    fn server_base_candidates(
        &self,
        domain: Domain,
        key: &str,
        cache: &Cache,
        committed: bool,
    ) -> Result<Vec<String>> {
        let (base, _) = self.effective_server_base(domain, key, committed)?;
        if domain != Domain::Library {
            // Section entries are small and carry no logical record key, so a
            // delta base inventory buys nothing here.
            return Ok(Vec::new());
        }
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
                "INSERT OR IGNORE INTO server_cycle_keys SELECT key FROM server_sync_base WHERE domain='library'",
                [],
            )?;
        }
        self.connection.execute(
            "INSERT OR IGNORE INTO server_cycle_keys SELECT key FROM server_sync_remote_dirty WHERE domain='library'",
            [],
        )?;
        if committed {
            self.connection.execute("INSERT OR IGNORE INTO server_cycle_keys SELECT key FROM server_sync_operation_records WHERE domain='library'",[])?;
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
                let (base, base_hash) = self.effective_server_base(Domain::Library, &key, committed)?;
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
            ("plugin", "plugin_storage", "storage_key", "owner", "ordinal"),
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
                    key1: if kind == "conversation" || kind == "plugin" {
                        parent.clone()
                    } else {
                        id.clone()
                    },
                    key2: if kind == "conversation" || kind == "plugin" {
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
                        &self.server_base_candidates(Domain::Library, &item.key, cache, false)?,
                        &self.server_base(Domain::Library, &item.key)?.0,
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
                        | Envelope::ArchivedCharacter {
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
    pub(crate) fn server_conflict_references(
        &mut self,
        cache: &Cache,
        transfer: &Transfer<'_>,
        client: &ServerClient,
        revision: i64,
        head: &RemoteHead,
    ) -> Result<crate::server_sync::backups::references::Receipt> {
        use crate::server_sync::backups::{references::{Capture, RemoteRead, PAGE}, Side};
        use crate::server_sync::residency::Residency;
        let config = self.server_stored_config()?
            .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
        let remote = RemoteRead::begin(client, head)?;
        let lease = self.acquire_revision(revision)?;
        let result = (|| {
            let (_, target) = self.read_view(Some(&lease.lease))?;
            let mut capture = Capture::begin_or_resume(&self.repository_root, revision, &target.generation, head)?;
            let mut residency = Residency::open(&self.repository_root)?;
            let context = Residency::context_id(&config, &head.epoch);
            let cas = PayloadCas::new(&self.repository_root)?;
            let (db, target) = self.read_view(Some(&lease.lease))?;
            if !capture.side_complete(Side::Local)? {
                let mut after = None;
                loop {
                    client.ensure_active()?;
                    let page = projection::all_keys_page(
                        db, &target.generation,
                        after.as_ref().map(|key: &outbox::ServerDirtyKey|
                            (key.kind.as_str(), key.key1.as_str(), key.key2.as_str())),
                        PAGE, revision,
                    )?;
                    if page.is_empty() { break; }
                    after = page.last().cloned();
                    for key in page {
                        client.ensure_active()?;
                        let Some(payload) = projection::project(db, &cas, &target.generation, &key)? else { continue; };
                        for bytes in payload.derived_objects.values() { cache.put(bytes)?; }
                        let dependencies = projection::dependencies(&payload, &cas)?;
                        let projected = cache.project(&payload, &dependencies, &relations(&key)?, scopes(&key))?;
                        self.capture_server_reference_record(
                            &mut capture, Side::Local, cache, &residency, &context,
                            &projection::wire_key(&key)?, &projected.version,
                            &payload, &dependencies, &projected.objects, client,
                        )?;
                    }
                }
                let mut after = String::new();
                loop {
                    let page = {
                        let mut statement = db.prepare("SELECT key,version FROM server_sync_base
                            WHERE domain='library' AND key>?1 ORDER BY key LIMIT 256")?;
                        let rows = statement.query_map([&after], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                            .collect::<std::result::Result<Vec<_>, _>>()?;
                        rows
                    };
                    if page.is_empty() { break; }
                    for (key, base) in page {
                        client.ensure_active()?;
                        after = key.clone();
                        if !capture.has_record(Side::Local, &key)? {
                            capture.record(Side::Local, &key, &delete_version(&parse(&base)?), None)?;
                        }
                    }
                }
                capture.complete_side(Side::Local)?;
            }
            if !capture.side_complete(Side::Remote)? {
                remote.visit(|record| {
                    if !matches!(record.version, RecordVersion::Live { .. }) {
                        return capture.record(Side::Remote, &record.key, &record.version, None);
                    }
                    transfer.download_record_metadata(&record.version, &[], &RecordVersion::Absent)?;
                    let (payload, _) = cache.restore(&record.version)?;
                    let key = key_parts(&record.key, revision)?;
                    let dependencies = projection::dependencies(&payload, &cache.cas)?;
                    let projected = cache.project(&payload, &dependencies, &relations(&key)?, scopes(&key))?;
                    if projected.version != record.version {
                        return Err(SyncError::new("server-descriptor-semantics-mismatch", 409));
                    }
                    self.capture_server_reference_record(
                        &mut capture, Side::Remote, cache, &residency, &context,
                        &record.key, &record.version, &payload, &dependencies, &projected.objects, client,
                    )
                })?;
                capture.complete_side(Side::Remote)?;
            }
            capture.retain(client, &config, &mut residency)?;
            capture.visit_records(|record| {
                client.ensure_active()?;
                let payload = if let Some(hash) = &record.body_hash {
                    let bytes = cas.read_object(hash)?
                        .ok_or_else(|| SyncError::new("conflict-metadata-missing", 409))?;
                    if risunest_sync_wire::hash(&bytes) != *hash {
                        return Err(SyncError::new("conflict-object-hash-mismatch", 409));
                    }
                    let payload: projection::ServerPayload = serde_json::from_slice(&bytes)
                        .map_err(|_| SyncError::new("invalid-server-payload", 409))?;
                    let key = key_parts(&record.key, revision)?;
                    let dependencies = projection::dependencies(&payload, &cas)?;
                    if cache.project(&payload, &dependencies, &relations(&key)?, scopes(&key))?.version != record.version {
                        return Err(SyncError::new("server-descriptor-semantics-mismatch", 409));
                    }
                    Some(payload)
                } else { None };
                validate_remote_with_residency(RemoteRecord { key: record.key, version: record.version,
                    payload, local_hash: record.body_hash }, &cas, |hash, size| {
                    capture.confirms(&residency, record.side, hash, size)
                        .map_err(|_| super::StoreError::Validation { message: "Conflict custody unavailable".into() })
                })?;
                Ok(())
            })?;
            client.ensure_active()?;
            if self.revision()? != revision {
                return Err(SyncError::new("local-revision-changed", 409));
            }
            capture.finish(self, &|| client.ensure_active())
        })();
        let released = self.release_revision(&lease.lease);
        match result {
            Ok(receipt) => {
                // An expired/rejected release cannot undo the durable receipt.
                let _ = remote.release();
                released?;
                Ok(receipt)
            }
            Err(error) => Err(error),
        }
    }

    fn capture_server_reference_record(
        &self,
        capture: &mut crate::server_sync::backups::references::Capture,
        side: crate::server_sync::backups::Side,
        cache: &Cache,
        residency: &crate::server_sync::residency::Residency,
        context: &str,
        key: &str,
        version: &RecordVersion,
        payload: &projection::ServerPayload,
        dependencies: &[String],
        objects: &[String],
        client: &ServerClient,
    ) -> Result<()> {
        use crate::server_sync::backups::{references::Object, Side};
        use crate::logical_records::LogicalRecordEnvelope as Envelope;
        let manifests: BTreeSet<&str> = match &payload.record {
            Envelope::Root { owner_heads, .. }
            | Envelope::Character { owner_heads, .. }
            | Envelope::ArchivedCharacter { owner_heads, .. } =>
                owner_heads.iter().filter_map(|head| head.manifest_hash.as_deref()).collect(),
            _ => BTreeSet::new(),
        };
        let payload_hashes: BTreeSet<&str> = dependencies.iter().map(String::as_str)
            .filter(|hash| !manifests.contains(hash)).collect();
        let expected = match &payload.record {
            Envelope::Asset { object_hash: Some(hash), size, .. }
            | Envelope::Inlay { object_hash: Some(hash), size, .. } => Some((hash.as_str(), *size)),
            Envelope::ArchivedCharacter {
                archive_object_hash,
                archive_object_size,
                ..
            } => Some((archive_object_hash.as_str(), *archive_object_size)),
            _ => None,
        };
        let bytes = serde_json::to_vec(payload).map_err(|_| SyncError::new("invalid-local-payload", 409))?;
        let body = capture.metadata(side, &bytes)?;
        for hash in objects {
            client.ensure_active()?;
            if !payload_hashes.contains(hash.as_str()) {
                capture.cached_metadata(side, cache, hash)?;
                continue;
            }
            let size = expected.filter(|(candidate, _)| *candidate == hash).map(|(_, size)| size);
            if matches!(side, Side::Local) {
                if let Some(proof) = residency.object(hash, Some(context))?.or(residency.object(hash, None)?) {
                    if size.is_some_and(|size| size != proof.size) {
                        return Err(SyncError::new("conflict-object-size-mismatch", 409));
                    }
                    capture.object(side, &Object { hash: hash.clone(), byte_size: Some(proof.size),
                        metadata: false, context_id: Some(proof.context), local_required: false })?;
                } else {
                    let actual = PayloadCas::new(&self.repository_root)?.stat_object(hash)?
                        .ok_or_else(|| SyncError::new("conflict-local-payload-unavailable", 409))?;
                    if size.is_some_and(|size| size != actual) {
                        return Err(SyncError::new("conflict-object-size-mismatch", 409));
                    }
                    capture.local_payload(hash, actual, &|| client.ensure_active())?;
                }
            } else {
                capture.object(side, &Object { hash: hash.clone(), byte_size: size,
                    metadata: false, context_id: Some(context.into()), local_required: false })?;
            }
        }
        capture.record(side, key, version, Some((&body, bytes.len() as u64)))
    }

    /// Projects a claimed conflict reference into the normal replacement fence.
    /// The caller must keep its reference-source guard alive until the returned
    /// preparation is either committed or abandoned.
    pub(crate) fn prepare_server_conflict_replacement(
        &mut self,
        source: &crate::server_sync::backups::ReferenceSource,
        expected_revision: i64,
        check: &impl Fn() -> Result<()>,
    ) -> Result<super::PreparedReplaceCommit> {
        let source_root = self.repository_root.clone();
        self.prepare_server_conflict_replacement_mode(
            source,
            expected_revision,
            ConflictReferenceMode::DirectRestore { source_root: &source_root },
            check,
        )
    }

    pub(crate) fn prepare_server_conflict_portable_export(
        &mut self,
        source_root: &Path,
        source: &crate::server_sync::backups::ReferenceSource,
        expected_revision: i64,
        check: &impl Fn() -> Result<()>,
    ) -> Result<super::PreparedReplaceCommit> {
        self.prepare_server_conflict_replacement_mode(
            source,
            expected_revision,
            ConflictReferenceMode::PortableExport { source_root },
            check,
        )
    }

    fn prepare_server_conflict_replacement_mode(
        &mut self,
        source: &crate::server_sync::backups::ReferenceSource,
        expected_revision: i64,
        mode: ConflictReferenceMode<'_>,
        check: &impl Fn() -> Result<()>,
    ) -> Result<super::PreparedReplaceCommit> {
        #[derive(Clone)]
        struct StoredObject {
            byte_size: u64,
            metadata: bool,
            context_id: Option<String>,
            local_required: bool,
        }

        check()?;
        if self.revision()? != expected_revision {
            return Err(SyncError::new("local-revision-changed", 409));
        }
        let source_root = mode.source_root();
        let target_root = self.repository_root.clone();
        let source_native = PayloadCas::new(source_root)?;
        let target_native = PayloadCas::new(&target_root)?;
        let residency = crate::server_sync::residency::Residency::open(source_root)?;
        let mut objects = BTreeMap::new();
        crate::server_sync::backups::visit_reference_objects(
            source_root,
            source,
            check,
            |object| {
                check()?;
                if object.metadata && !object.local_required {
                    return Err(SyncError::new("invalid-conflict-object", 409));
                }
                if objects.contains_key(&object.hash) {
                    return Err(SyncError::new("duplicate-conflict-object", 409));
                }
                let available = if object.metadata || object.local_required {
                    source_native.open_object(&object.hash)?
                } else if mode.hydrates_all() {
                    crate::server_sync::residency::open_or_hydrate_with_check(
                        source_root,
                        &object.hash,
                        check,
                    )?
                } else {
                    source_native.open_object(&object.hash)?
                };
                if let Some(mut file) = available {
                    target_native.prepare_reader_expected(
                        &mut file,
                        &object.hash,
                        object.byte_size,
                    )?;
                } else if object.metadata || object.local_required {
                    return Err(SyncError::new("conflict-local-object-missing", 409));
                } else if mode.hydrates_all() {
                    return Err(SyncError::new("conflict-custody-unavailable", 409));
                } else {
                    let context = object.context_id.as_deref()
                        .ok_or_else(|| SyncError::new("invalid-conflict-object", 409))?;
                    if !residency.confirms(&object.hash, Some(object.byte_size), context)? {
                        return Err(SyncError::new("conflict-custody-unavailable", 409));
                    }
                }
                objects.insert(object.hash, StoredObject {
                    byte_size: object.byte_size,
                    metadata: object.metadata,
                    context_id: object.context_id,
                    local_required: object.local_required,
                });
                Ok(())
            },
        )?;

        let cache = Cache::open(&target_root)?;
        let mut records = ValidatedRecords::new()?;
        crate::server_sync::backups::visit_reference_records(
            source_root,
            source,
            check,
            |record| {
                check()?;
                let dependencies = record.payload.as_ref()
                    .map(|payload| projection::dependencies(payload, &target_native))
                    .transpose()?
                    .unwrap_or_default();
                let mut local = Vec::new();
                let mut remote = BTreeMap::<String, Vec<String>>::new();
                for hash in &dependencies {
                    let object = objects.get(hash)
                        .ok_or_else(|| SyncError::new("conflict-object-missing", 409))?;
                    if target_native.stat_object(hash)? == Some(object.byte_size) {
                        local.push(hash.clone());
                    } else if object.metadata || object.local_required {
                        return Err(SyncError::new("conflict-local-object-missing", 409));
                    } else {
                        let context = object.context_id.as_ref()
                            .ok_or_else(|| SyncError::new("invalid-conflict-object", 409))?;
                        remote.entry(context.clone()).or_default().push(hash.clone());
                    }
                }
                if let Some(payload) = record.payload.as_ref() {
                    let dirty = key_parts(&record.key, source.local_revision())?;
                    if cache.project(
                        payload,
                        &dependencies,
                        &relations(&dirty)?,
                        scopes(&dirty),
                    )?.version != record.version {
                        return Err(SyncError::new(
                            "server-descriptor-semantics-mismatch",
                            409,
                        ));
                    }
                }
                if !local.is_empty() {
                    self.promote_server_dependencies_with_residency(
                        &cache,
                        &local,
                        None,
                        || check(),
                    )?;
                }
                for (context, hashes) in remote {
                    self.promote_server_dependencies_with_residency(
                        &cache,
                        &hashes,
                        Some((&residency, &context)),
                        || check(),
                    )?;
                }
                records.push(validate_remote_with_residency(
                    RemoteRecord {
                        key: record.key,
                        version: record.version,
                        payload: record.payload,
                        local_hash: None,
                    },
                    &target_native,
                    |hash, size| {
                        let Some(object) = objects.get(hash) else { return Ok(false); };
                        if size.is_some_and(|size| size != object.byte_size) {
                            return Ok(false);
                        }
                        if target_native.stat_object(hash)? == Some(object.byte_size) {
                            return Ok(true);
                        }
                        let Some(context) = object.context_id.as_deref() else {
                            return Ok(false);
                        };
                        residency.confirms(hash, Some(object.byte_size), context)
                            .map_err(|_| super::StoreError::Validation {
                                message: "Conflict custody unavailable".into(),
                            })
                    },
                )?)?;
                Ok(())
            },
        )?;

        let stage = self.replace_begin()?;
        let staging_id = stage.staging_id.clone();
        let prepared = (|| -> Result<super::PreparedReplaceCommit> {
            check()?;
            let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let active = super::active_generation(&tx)?;
            let raw_root: Option<serde_json::Value> = tx.query_row(
                "SELECT value FROM root WHERE generation=?1",
                [&active],
                |row| row.get::<_, String>(0),
            ).optional()?.map(|value| serde_json::from_str(&value)).transpose()
                .map_err(|_| SyncError::new("invalid-local-root", 409))?;
            let mut saw_root = false;
            records.visit(false, |item| {
                check().map_err(|_| super::StoreError::Validation {
                    message: "Conflict restore cancelled".into(),
                })?;
                let (record, locator) = item.into_parts();
                let Some(mut payload) = record.payload else { return Ok(()); };
                let raw_character: Option<serde_json::Value> =
                    if let crate::logical_records::LogicalRecordLocator::Character { character_id } = &locator {
                        tx.query_row(
                            "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
                            params![active, character_id],
                            |row| row.get::<_, String>(0),
                        ).optional()?.map(|value| serde_json::from_str(&value)).transpose()?
                    } else {
                        None
                    };
                saw_root |= matches!(&locator, crate::logical_records::LogicalRecordLocator::Root);
                if !mode.hydrates_all() {
                    projection::preserve_local_view(
                        &mut payload,
                        raw_character.as_ref(),
                        raw_root.as_ref(),
                    );
                }
                super::record_apply::apply_materialized_record(
                    &tx,
                    &staging_id,
                    locator,
                    payload.record,
                    payload.messages.as_deref(),
                )?;
                Ok(())
            })?;
            if !saw_root {
                return Err(SyncError::new("conflict-root-missing", 409));
            }
            tx.execute(
                "UPDATE characters SET conversation_count=(SELECT count(*) FROM conversations
                    WHERE conversations.generation=?1 AND conversations.character_id=characters.character_id)
                    WHERE generation=?1",
                [&staging_id],
            )?;
            super::record_apply::validate_configured_index_uniqueness(&tx, &staging_id)?;
            let duplicate_plugins: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM plugin_storage WHERE generation=?1
                    GROUP BY ordinal HAVING count(*)>1)",
                [&staging_id],
                |row| row.get(0),
            )?;
            if duplicate_plugins {
                return Err(SyncError::new("conflict-plugin-order-invalid", 409));
            }
            tx.commit()?;
            check()?;
            self.replace_put_asset_repository_authority(
                &staging_id,
                &super::AssetRepositoryAuthorityState::V2 {
                    migration_id: source.id().to_owned(),
                    compatibility_hash: source.index_hash().to_owned(),
                },
            )?;
            Ok(self.prepare_replace_commit(&staging_id, Some(expected_revision))?)
        })();
        match prepared {
            Ok(prepared) => Ok(prepared),
            Err(error) => {
                self.replace_abort(&staging_id)?;
                Err(error)
            }
        }
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
                    let size = match native.stat_object(hash)? {
                        Some(size) => size,
                        None => match cache.cas.stat_object(hash)? {
                            Some(size) => size,
                            None => remote
                                .and_then(|(residency, context)| {
                                    residency.object(hash, Some(context)).transpose()
                                })
                                .transpose()?
                                .map(|object| object.size)
                                .ok_or_else(|| SyncError::new("missing-downloaded-payload", 409))?,
                        },
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
