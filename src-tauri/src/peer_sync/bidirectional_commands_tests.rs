// Extracted from bidirectional_commands.rs so the command module stays
// reviewable; this file is the same `tests` module and keeps every
// `super::` path unchanged.

use super::*;

#[test]
fn android_p5_target_foreground_retains_running_until_durable_terminal_projection() {
    let _registry_guard = super::super::android_foreground::test_registry_guard();
    let state = PeerBidirectionalCommandState::default();
    let foreground = state.reserve_target_foreground().unwrap();
    assert_eq!(
        state.reserve_target_foreground().unwrap_err().to_string(),
        r#"Protocol("Android bidirectional target foreground cleanup is pending")"#,
    );
    assert_eq!(foreground.lane, AndroidForegroundLane::P5Target);
    assert!(registry().attach_exact(&foreground));
    let cancellation = state.mark_target_running_exact(&foreground).unwrap();
    assert!(!cancellation.is_cancelled());
    assert!(state.cancel_target_foreground_exact(&foreground).unwrap());
    assert!(cancellation.is_cancelled());
    assert!(!state.release_target_foreground_exact(&foreground).unwrap());
    assert!(registry().reserve(AndroidForegroundLane::P5Source).is_err());

    let result = PeerBidirectionalSyncResult::ResumeRequired {
        operation_id: "123e4567-e89b-42d3-a456-426614174188".to_owned(),
        phase: "localCommitted",
        committed_revision: 9,
    };
    state
        .publish_target_terminal_exact(&foreground, Ok(result.clone()))
        .unwrap();
    let terminal = state.target_foreground_status().unwrap().unwrap();
    assert_eq!(terminal.phase, AndroidBidirectionalTargetPhase::Terminal);
    assert_eq!(terminal.result, Some(result));
    assert!(terminal.error.is_none());
    assert!(state.release_target_foreground_exact(&foreground).unwrap());
    let source = registry().reserve(AndroidForegroundLane::P5Source).unwrap();
    assert!(registry().abandon_source_exact(&source));
}

#[test]
fn registered_bidirectional_terminal_projection_bounds_transport_and_local_errors() {
    let _registry_guard = super::super::android_foreground::test_registry_guard();
    let state = PeerBidirectionalCommandState::default();
    let foreground = state.reserve_target_foreground().unwrap();
    assert!(registry().attach_exact(&foreground));
    state.mark_target_running_exact(&foreground).unwrap();

    let local = bound_registered_bidirectional_outcome::<PeerBidirectionalSyncResult>(
        Err("C:\\private\\store and bearer secret".to_owned()),
        true,
    );
    assert_eq!(local.as_ref().unwrap_err(), "operationFailed");
    state
        .publish_target_terminal_exact(&foreground, local)
        .unwrap();
    let terminal = state.target_foreground_status().unwrap().unwrap();
    assert_eq!(terminal.error.as_deref(), Some("operationFailed"));
    assert!(state.release_target_foreground_exact(&foreground).unwrap());

    assert_eq!(
        bidirectional_peer_operation_failure(
            "registered bidirectional transport",
            PeerSyncError::Transport("http://192.168.1.7/session/secret".to_owned()),
            true,
        ),
        "transportUnavailable"
    );
}

#[test]
fn android_p5_source_release_consumes_exact_full_stop_after_notification_callback() {
    let _registry_guard = super::super::android_foreground::test_registry_guard();
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let source = LogicalDeltaSourceSession::open_owned(
        store.open_native_job_store().unwrap(),
        directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &active.manifest.generation,
        P5_SOURCE_PIN_PREFIX,
    )
    .unwrap();
    let session_id = "123e4567-e89b-42d3-a456-426614174189";
    let prepared = super::super::lan::PreparedLogicalLanSession::new(
        session_id,
        "123e4567-e89b-42d3-a456-426614174190",
        active.manifest_hash.clone(),
        active.manifest_bytes,
        source.objects().to_vec(),
        Box::new(source),
    )
    .unwrap();
    let state = PeerBidirectionalCommandState::default();
    state
        .install_source(
            LanCloneHost::prepare_logical(prepared),
            session_id,
            &active.manifest_hash,
        )
        .unwrap();
    let foreground = registry().reserve(AndroidForegroundLane::P5Source).unwrap();
    assert!(registry().attach_exact(&foreground));
    state
        .attach_source_foreground(session_id, foreground.clone())
        .unwrap();
    let lease_count = || {
        let connection =
            rusqlite::Connection::open(directory.path().join("persistent").join("persistent.db"))
                .unwrap();
        connection
            .query_row(
                "SELECT COUNT(*) FROM logical_generation_session_pins WHERE session_id LIKE 'logical-session-p5-source-%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };

    assert_eq!(lease_count(), 1);
    assert_eq!(
        state.source_stop_foreground(session_id).unwrap(),
        Some(foreground.clone()),
    );
    assert!(state.source_is_active().unwrap());
    assert_eq!(lease_count(), 1);
    assert!(registry().reserve(AndroidForegroundLane::P4Source).is_err());

    state.pause_source_exact(&foreground);
    assert!(state.source_is_active().unwrap());
    assert_eq!(lease_count(), 1);
    assert!(registry().reserve(AndroidForegroundLane::P4Source).is_err());
    assert_eq!(
        state.source_stop_foreground(session_id).unwrap(),
        Some(foreground.clone()),
    );

    let stale = AndroidForegroundKey {
        generation: foreground.generation + 1,
        ..foreground.clone()
    };
    assert!(!state
        .release_source_android_exact(session_id, &stale)
        .unwrap());
    assert!(state.source_is_active().unwrap());
    assert_eq!(lease_count(), 1);

    assert!(state
        .release_source_android_exact(session_id, &foreground)
        .unwrap());
    assert!(!state.source_is_active().unwrap());
    assert_eq!(lease_count(), 0);
    assert!(state
        .release_source_android_exact(session_id, &foreground)
        .unwrap());
    let fresh = registry().reserve(AndroidForegroundLane::P4Source).unwrap();
    assert!(registry().abandon_source_exact(&fresh));
}
use crate::{
    asset_repository::{
        job_pins::{collect_durable_cas_job_roots, CasObjectRole},
        PayloadCas,
    },
    local_backup::AtomicCancellation,
    peer_sync::logical_delta::{
        build_logical_manifest, encode_logical_manifest, LogicalManifestBuilderInput,
        LogicalRecordEnvelope, LogicalRecordLocator, ProjectedLogicalRecord,
    },
    persistent_store::{
        establish_logical_common_base, logical_delta_source::LogicalDeltaSourceSession, AssetAlias,
        AssetRepositoryAuthorityState, ColdPayloadAuthorityState, PersistentStore,
        VerifiedSyncDeviceRegistration, WorkingSetCommit, PRODUCT_LOGICAL_LIBRARY_ID,
    },
};
use serde_json::json;
use std::{
    collections::{BTreeMap, VecDeque},
    io::Cursor,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
};

static REVERSE_CLEANUP_TEST_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct FakeReverseProcess {
    attempts: Arc<AtomicUsize>,
    fail_through_attempt: usize,
}

struct FakeSourceTunnel {
    lifecycles: VecDeque<RunningTunnelLifecycle>,
    stop_attempts: Arc<AtomicUsize>,
    fail_stop_through_attempt: usize,
}

impl SourceTunnelProcess for FakeSourceTunnel {
    fn stop(&mut self) -> Result<(), PeerSyncError> {
        let attempt = self.stop_attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt <= self.fail_stop_through_attempt {
            Err(PeerSyncError::Transport(
                "fixture source cleanup failed".to_owned(),
            ))
        } else {
            Ok(())
        }
    }

    fn lifecycle(&mut self) -> Result<RunningTunnelLifecycle, PeerSyncError> {
        Ok(self
            .lifecycles
            .pop_front()
            .unwrap_or(RunningTunnelLifecycle::Running))
    }
}

struct BlockingSourceTunnel {
    lifecycle_started: mpsc::Sender<()>,
    lifecycle_release: mpsc::Receiver<()>,
    stop_attempts: Arc<AtomicUsize>,
}

impl SourceTunnelProcess for BlockingSourceTunnel {
    fn stop(&mut self) -> Result<(), PeerSyncError> {
        self.stop_attempts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn lifecycle(&mut self) -> Result<RunningTunnelLifecycle, PeerSyncError> {
        self.lifecycle_started.send(()).unwrap();
        self.lifecycle_release.recv().unwrap();
        Ok(RunningTunnelLifecycle::Running)
    }
}

impl ReverseTunnelProcess for FakeReverseProcess {
    fn stop(&mut self) -> Result<(), PeerSyncError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt <= self.fail_through_attempt {
            Err(PeerSyncError::Transport(
                "fixture reverse cleanup failed".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

fn fake_reverse_owner(
    attempts: &Arc<AtomicUsize>,
    fail_through_attempt: usize,
) -> ReverseTunnelCleanupOwner {
    ReverseTunnelCleanupOwner::Running(Box::new(FakeReverseProcess {
        attempts: Arc::clone(attempts),
        fail_through_attempt,
    }))
}

fn state_with_source_tunnel(
    root: &Path,
    session_id: &str,
    tunnel: Box<dyn SourceTunnelProcess>,
) -> (PeerBidirectionalCommandState, PersistentStore) {
    let cas = PayloadCas::new(root).unwrap();
    let mut store = PersistentStore::open(root).unwrap();
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let source = LogicalDeltaSourceSession::open(
        store.open_native_job_store().unwrap(),
        root,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &active.manifest.generation,
    )
    .unwrap();
    let prepared = super::super::lan::PreparedLogicalLanSession::new(
        session_id,
        "123e4567-e89b-42d3-a456-426614174190",
        active.manifest_hash.clone(),
        active.manifest_bytes,
        source.objects().to_vec(),
        Box::new(source),
    )
    .unwrap();
    let state = PeerBidirectionalCommandState::default();
    state
        .install_source(
            LanCloneHost::prepare_logical(prepared),
            session_id,
            &active.manifest_hash,
        )
        .unwrap();
    {
        let mut runtime = state.lock().unwrap();
        let source = runtime.source.as_mut().unwrap();
        source.host = None;
        source.tunnel = Some(tunnel);
        source.phase = PeerBidirectionalSourcePhase::Running;
        source.pairing_uri = Some("risuailocal://peer-sync/v1".to_owned());
        source.tunnel_metadata = Some(PeerBidirectionalTunnelMetadata::quick());
    }
    (state, store)
}

struct FixtureSource {
    objects: BTreeMap<String, Vec<u8>>,
    reads: usize,
}

struct FailingAfterFixtureSource {
    objects: BTreeMap<String, Vec<u8>>,
    successful_reads: usize,
    opens: usize,
    fail_after: usize,
}

struct BlockingFixtureSource {
    objects: BTreeMap<String, Vec<u8>>,
    opened: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl super::super::LogicalDeltaObjectSource for BlockingFixtureSource {
    fn open_object(
        &mut self,
        object: &super::super::LogicalDeltaObject,
    ) -> Result<Box<dyn Read>, PeerSyncError> {
        self.opened.send(()).unwrap();
        self.release.recv().unwrap();
        let bytes = self.objects.get(&object.hash).ok_or_else(|| {
            PeerSyncError::Transport(format!("fixture object {} is absent", object.hash))
        })?;
        Ok(Box::new(Cursor::new(bytes.clone())))
    }
}

impl super::super::LogicalDeltaObjectSource for FixtureSource {
    fn open_object(
        &mut self,
        object: &super::super::LogicalDeltaObject,
    ) -> Result<Box<dyn Read>, PeerSyncError> {
        self.reads += 1;
        let bytes = self.objects.get(&object.hash).ok_or_else(|| {
            PeerSyncError::Transport(format!("fixture object {} is absent", object.hash))
        })?;
        Ok(Box::new(Cursor::new(bytes.clone())))
    }
}

impl super::super::LogicalDeltaObjectSource for FailingAfterFixtureSource {
    fn open_object(
        &mut self,
        object: &super::super::LogicalDeltaObject,
    ) -> Result<Box<dyn Read>, PeerSyncError> {
        self.opens += 1;
        if self.successful_reads == self.fail_after {
            return Err(PeerSyncError::Transport(format!(
                "fixture stopped before object {}",
                object.hash
            )));
        }
        let bytes = self.objects.get(&object.hash).ok_or_else(|| {
            PeerSyncError::Transport(format!("fixture object {} is absent", object.hash))
        })?;
        self.successful_reads += 1;
        Ok(Box::new(Cursor::new(bytes.clone())))
    }
}

fn generation(id: &str, sequence: &str, hash: char) -> SyncGenerationIdentity {
    SyncGenerationIdentity {
        generation_id: id.to_owned(),
        manifest_hash: hash.to_string().repeat(64),
        generation_sequence: sequence.to_owned(),
    }
}

fn context(operation_id: &str) -> PeerBidirectionalOperationContext {
    let previous = generation("shared-c", "1", 'a');
    PeerBidirectionalOperationContext {
        operation_id: operation_id.to_owned(),
        credential: LanBidirectionalLogicalCredential {
            endpoint: "http://192.168.1.2:32146".to_owned(),
            session_id: "123e4567-e89b-42d3-a456-426614174000".to_owned(),
            manifest_id: "b".repeat(64),
            device_id: "123e4567-e89b-42d3-a456-426614174001".to_owned(),
            source_device_id: "123e4567-e89b-42d3-a456-426614174002".to_owned(),
            bearer: "c".repeat(64),
        },
        library_id: "risunest-product".to_owned(),
        expected_local_revision: 7,
        expected_remote_revision: 7,
        expected_remote_generation: LanBidirectionalGeneration {
            generation_id: "remote-r".to_owned(),
            manifest_hash: "d".repeat(64),
            generation_sequence: "2".to_owned(),
        },
        previous_shared: previous.clone(),
        previous_local: previous,
        durable_job_id: "123e4567-e89b-42d3-a456-426614174003".to_owned(),
        completion_mode: PeerBidirectionalCompletionMode::Legacy,
        completion_delivery: None,
    }
}

const BIDIRECTIONAL_ACCOUNTING_SOURCE_ID: &str = "123e4567-e89b-42d3-a456-426614174002";

fn register_bidirectional_accounting_source(root: &Path, total_bytes: u64, last_seen_ms: u64) {
    let mut registry = super::super::device_registry::IncomingSourceRegistry::load(root).unwrap();
    registry
        .upsert(super::super::device_registry::IncomingSource {
            device_id: BIDIRECTIONAL_ACCOUNTING_SOURCE_ID.to_owned(),
            name: "bidirectional source".to_owned(),
            endpoint: "http://192.168.0.98:32146".to_owned(),
            bearer: "c".repeat(64),
            permissions: super::super::device_registry::DevicePermissions::read_and_bidirectional(),
            last_seen_ms,
            total_bytes,
        })
        .unwrap();
    registry.save().unwrap();
}

fn bidirectional_accounting_source(root: &Path) -> super::super::device_registry::IncomingSource {
    super::super::device_registry::IncomingSourceRegistry::load(root)
        .unwrap()
        .sources()[0]
        .clone()
}

fn backup(side: PeerBidirectionalBackupSide) -> PeerBidirectionalBackupReceipt {
    PeerBidirectionalBackupReceipt {
        package_id: "f".repeat(64),
        side,
        path: "peer-bidirectional/backups/conflict.risulossless".to_owned(),
    }
}

fn retain_source_prepared_fixture(
    root: &Path,
    operation_id: &str,
    source_device_id: &str,
    target_device_id: &str,
    durable_job_id: &str,
) -> (
    PersistentStore,
    PayloadCas,
    SourcePreparedEvidence,
    SyncGenerationIdentity,
) {
    let cas = PayloadCas::new(root).unwrap();
    let mut store = PersistentStore::open(root).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let previous = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    establish_logical_common_base(
        &mut store,
        &cas,
        target_device_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                target_device_id,
                previous.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    DurableCasJob::begin(root, durable_job_id, CasJobKind::LogicalDeltaTarget, 0).unwrap();
    let evidence = SourcePreparedEvidence {
        operation_id: operation_id.to_owned(),
        source_device_id: source_device_id.to_owned(),
        target_device_id: target_device_id.to_owned(),
        expected_source_revision: 0,
        previous_shared: previous.clone(),
        expected_source_generation: previous.clone(),
        shared_generation: LanBidirectionalGeneration {
            generation_id: "prepared-shared".to_owned(),
            manifest_hash: "a".repeat(64),
            generation_sequence: "1".to_owned(),
        },
        incoming_revision: 1,
        transferred_objects: 2,
        transferred_bytes: 19,
        backup_required: false,
        backup: None,
        completion_deferred_v1: false,
    };
    PeerBidirectionalOperationJournal::new(root)
        .store(&evidence.durable_operation(durable_job_id.to_owned()))
        .unwrap();
    (store, cas, evidence, previous)
}

fn retain_source_postcommit_fixture(
    root: &Path,
    operation_id: &str,
    source_device_id: &str,
    target_device_id: &str,
    durable_job_id: &str,
) -> (
    PersistentStore,
    PayloadCas,
    SourcePreparedEvidence,
    LanBidirectionalRemoteApplyReceipt,
) {
    let (mut store, cas, mut evidence, previous) = retain_source_prepared_fixture(
        root,
        operation_id,
        source_device_id,
        target_device_id,
        durable_job_id,
    );
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"committed": "shared"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let shared = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let shared_identity = SyncGenerationIdentity {
        generation_id: shared.manifest.generation.clone(),
        manifest_hash: shared.manifest_hash.clone(),
        generation_sequence: shared.manifest.generation_sequence.clone(),
    };
    store
        .advance_active_sync_device_shared_ack(
            PRODUCT_LOGICAL_LIBRARY_ID,
            target_device_id,
            1,
            &previous,
            &previous,
            &shared_identity,
            &shared.manifest,
            &shared_identity,
        )
        .unwrap();
    evidence.shared_generation = LanBidirectionalGeneration {
        generation_id: shared_identity.generation_id,
        manifest_hash: shared_identity.manifest_hash,
        generation_sequence: shared_identity.generation_sequence,
    };
    PeerBidirectionalOperationJournal::new(root)
        .store(&evidence.durable_operation(durable_job_id.to_owned()))
        .unwrap();
    let mut retained_job = DurableCasJob::open(root, durable_job_id).unwrap();
    retained_job.seal(&mut store, 0).unwrap();
    let receipt = evidence.receipt_at(1).unwrap();
    (store, cas, evidence, receipt)
}

fn issue_deferred_bidirectional_offer(root: &Path, target_device_id: &str) -> String {
    let manifest_id = {
        let cas = PayloadCas::new(root).unwrap();
        let mut store = PersistentStore::open(root).unwrap();
        store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap()
            .manifest_hash
    };
    super::super::device_registry::register_outgoing_claim(
        root,
        super::super::device_registry::OutgoingDevice {
            device_id: target_device_id.to_owned(),
            name: "Deferred target".to_owned(),
            bearer_digest: "a".repeat(64),
            permissions: super::super::device_registry::DevicePermissions::read_and_bidirectional(),
            created_at_ms: 1,
            last_seen_ms: 1,
            total_bytes: 0,
        },
    )
    .unwrap();
    super::super::device_registry::issue_outgoing_unmeasured_completion_offer(
        root,
        target_device_id,
        super::super::device_registry::CompletionLane::Bidirectional,
        &manifest_id,
        None,
    )
    .unwrap()
    .as_str()
    .to_owned()
}

fn awaiting_conflict_operation(
    operation_id: &str,
    durable_job_id: &str,
) -> PeerBidirectionalDurableOperation {
    let mut context = context(operation_id);
    context.durable_job_id = durable_job_id.to_owned();
    PeerBidirectionalDurableOperation::AwaitingConflict {
        schema: OPERATION_SCHEMA.to_owned(),
        context,
        conflicts: vec![PeerBidirectionalConflict {
            key: "root".to_owned(),
            conflict_type: "bothChanged".to_owned(),
        }],
        local_generation: generation("local", "2", 'e'),
        local_manifest_hash: "e".repeat(64),
        remote_manifest_hash: "d".repeat(64),
        backups: vec![backup(PeerBidirectionalBackupSide::Local)],
    }
}

fn lossless_root(username: &str) -> serde_json::Value {
    json!({
        "username": username,
        "botPresetsId": 0,
        "personas": [{ "id": "persona" }],
        "selectedPersona": 0,
        "enabledModules": [],
        "characterOrder": [],
        "modules": [],
        "loadouts": [],
        "plugins": [],
    })
}

fn seed_lossless_backup_fixture(store: &mut PersistentStore, cas: &PayloadCas) {
    let icon = cas.prepare_bytes(b"fixture-icon").unwrap();
    let staging = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(&staging, &lossless_root("Base"))
        .unwrap();
    store
        .replace_put_presets(&staging, &[json!({"name": "preset"})])
        .unwrap();
    store
        .replace_put_asset_aliases(
            &staging,
            &[AssetAlias {
                key: "icon".to_owned(),
                object_hash: Some(icon.content_hash),
                kind: "asset".to_owned(),
                size: icon.byte_size as i64,
                mime: "application/octet-stream".to_owned(),
                name: "icon.bin".to_owned(),
                ext: "bin".to_owned(),
                inlay_type: None,
                width: None,
                height: None,
                metadata: json!({}),
            }],
        )
        .unwrap();
    store
        .replace_put_asset_repository_authority(
            &staging,
            &AssetRepositoryAuthorityState::V2 {
                migration_id: "bidirectional-backup-fixture".to_owned(),
                compatibility_hash: "ab".repeat(32),
            },
        )
        .unwrap();
    store
        .replace_put_cold_payload_authority(
            &staging,
            &ColdPayloadAuthorityState::V2 {
                migration_id: "bidirectional-backup-fixture".to_owned(),
                compatibility_hash: "cd".repeat(32),
            },
        )
        .unwrap();
    store.replace_commit(&staging, Some(0)).unwrap();
}

fn public_conflict_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    PayloadCas,
    PersistentStore,
    crate::peer_sync::logical_delta::BuiltIndexedLogicalManifest,
    LanBidirectionalLogicalCredential,
    String,
) {
    let local_directory = tempfile::tempdir().unwrap();
    let local_cas = PayloadCas::new(local_directory.path()).unwrap();
    let mut local_store = PersistentStore::open(local_directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut local_store, &local_cas);
    let base = local_store
        .seal_or_initialize_active_logical_generation(&local_cas)
        .unwrap();
    drop(local_store);
    let remote_directory = tempfile::tempdir().unwrap();
    copy_tree(local_directory.path(), remote_directory.path());
    let mut local_store = PersistentStore::open(local_directory.path()).unwrap();
    let peer_id = "123e4567-e89b-42d3-a456-426614174195";
    establish_logical_common_base(
        &mut local_store,
        &local_cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        1,
        &base.manifest_bytes,
    )
    .unwrap();
    local_store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                SyncGenerationIdentity {
                    generation_id: base.manifest.generation.clone(),
                    manifest_hash: base.manifest_hash.clone(),
                    generation_sequence: base.manifest.generation_sequence.clone(),
                },
                1,
            )
            .unwrap(),
            1,
        )
        .unwrap();
    local_store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(lossless_root("Local")),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let remote_cas = PayloadCas::new(remote_directory.path()).unwrap();
    let mut remote_store = PersistentStore::open(remote_directory.path()).unwrap();
    remote_store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(lossless_root("Remote")),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let remote = remote_store
        .seal_or_initialize_active_logical_generation(&remote_cas)
        .unwrap();
    let credential = LanBidirectionalLogicalCredential {
        endpoint: "https://initial-public.example".to_owned(),
        session_id: "123e4567-e89b-42d3-a456-426614174196".to_owned(),
        manifest_id: remote.manifest_hash.clone(),
        device_id: "123e4567-e89b-42d3-a456-426614174197".to_owned(),
        source_device_id: peer_id.to_owned(),
        bearer: "a".repeat(64),
    };
    let mut source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    let conflict = match begin_bidirectional_local_merge(
        &mut local_store,
        &local_cas,
        local_directory.path(),
        credential.clone(),
        2,
        &remote.manifest_bytes,
        &mut source,
    )
    .unwrap()
    {
        LocalMergeOutcome::Conflict(conflict) => conflict,
        other => panic!("expected public conflict, got {other:?}"),
    };
    (
        local_directory,
        remote_directory,
        local_cas,
        local_store,
        remote,
        credential,
        conflict.operation_id,
    )
}

#[test]
fn conflict_backup_requires_v2_authority_before_replacement() {
    let authorities = [
        (r#"{"format":"legacy"}"#, r#"{"format":"legacy"}"#),
        (
            r#"{"format":"preparing","migrationId":"asset-preparing","sourceRevision":0}"#,
            r#"{"format":"v2","migrationId":"cold-v2","compatibilityHash":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"}"#,
        ),
        (
            r#"{"format":"v2","migrationId":"asset-v2","compatibilityHash":"abababababababababababababababababababababababababababababababab"}"#,
            r#"{"format":"preparing","migrationId":"cold-preparing","sourceRevision":0}"#,
        ),
    ];

    for (asset_authority, cold_authority) in authorities {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        seed_lossless_backup_fixture(&mut store, &cas);
        let active = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let source = SyncGenerationIdentity {
            generation_id: active.manifest.generation,
            manifest_hash: active.manifest_hash,
            generation_sequence: active.manifest.generation_sequence,
        };
        drop(store);
        let connection =
            rusqlite::Connection::open(directory.path().join("persistent").join("persistent.db"))
                .unwrap();
        connection
            .execute(
                "UPDATE asset_repository_authority SET value = ?1 WHERE generation = 'revision-1'",
                [asset_authority],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE cold_payload_authority SET value = ?1 WHERE generation = 'revision-1'",
                [cold_authority],
            )
            .unwrap();
        drop(connection);
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174099";

        let error = ensure_bidirectional_backup_receipt(
            &mut store,
            &cas,
            directory.path(),
            operation_id,
            1,
            PeerBidirectionalBackupSide::Local,
            &source,
            None,
            &NeverCancelled,
        )
        .unwrap_err();

        assert!(matches!(error, PeerSyncError::Storage(_)));
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.read_root(None).unwrap().value, lossless_root("Base"));
        assert!(!bidirectional_backup_path(
            directory.path(),
            operation_id,
            PeerBidirectionalBackupSide::Local,
        )
        .exists());
    }
}

#[test]
fn bidirectional_backup_recovers_after_a_truncated_operation_temp() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut store, &cas);
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let source = SyncGenerationIdentity {
        generation_id: active.manifest.generation,
        manifest_hash: active.manifest_hash,
        generation_sequence: active.manifest.generation_sequence,
    };
    let operation_id = "123e4567-e89b-42d3-a456-426614174098";
    let staging = directory
        .path()
        .join("peer-bidirectional")
        .join("backup-staging")
        .join(format!("{operation_id}-local"));
    let temporary = staging.join("backup.risulossless");
    fs::create_dir_all(&staging).unwrap();
    fs::write(&temporary, b"truncated backup").unwrap();
    BACKUP_FULL_VERIFICATION_COUNT.with(|count| count.set(0));
    let maintenance_root = directory.path().to_path_buf();
    BACKUP_STAGING_MAINTENANCE_HOOK.with(|slot| {
        slot.replace(Some(Box::new(move |active_staging| {
            assert_eq!(
                super::super::maintenance::cleanup_temp(&maintenance_root)
                    .unwrap()
                    .count,
                0
            );
            assert!(active_staging.exists());
        })));
    });

    let receipt = ensure_bidirectional_backup_receipt(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        1,
        PeerBidirectionalBackupSide::Local,
        &source,
        None,
        &NeverCancelled,
    )
    .unwrap()
    .into_receipt();

    assert!(!temporary.exists());
    assert_eq!(BACKUP_FULL_VERIFICATION_COUNT.with(Cell::get), 1);
    assert_eq!(
        verify_bidirectional_backup_receipt(
            Path::new(&receipt.path),
            operation_id,
            1,
            PeerBidirectionalBackupSide::Local,
            &source,
            Some(&receipt.package_id),
            &NeverCancelled,
        )
        .unwrap(),
        receipt
    );
}

#[test]
fn bidirectional_backup_publishes_a_verified_operation_temp() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut store, &cas);
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let source = SyncGenerationIdentity {
        generation_id: active.manifest.generation,
        manifest_hash: active.manifest_hash,
        generation_sequence: active.manifest.generation_sequence,
    };
    let operation_id = "123e4567-e89b-42d3-a456-426614174097";
    let staging = directory
        .path()
        .join("peer-bidirectional")
        .join("backup-staging")
        .join(format!("{operation_id}-local"));
    let temporary = staging.join("backup.risulossless");
    fs::create_dir_all(&staging).unwrap();
    let prepared = create_and_verify_peer_bidirectional_backup_v1_report(
        &temporary,
        &staging,
        &cas,
        &mut store,
        1,
        &lossless_source_binding(operation_id, PeerBidirectionalBackupSide::Local, &source),
        &NeverCancelled,
    )
    .unwrap();
    drop(store);
    let connection =
        rusqlite::Connection::open(directory.path().join("persistent").join("persistent.db"))
            .unwrap();
    connection
        .execute(
            "UPDATE asset_repository_authority SET value = ?1 WHERE generation = 'revision-1'",
            [r#"{"format":"legacy"}"#],
        )
        .unwrap();
    drop(connection);
    let mut store = PersistentStore::open(directory.path()).unwrap();
    BACKUP_FULL_VERIFICATION_COUNT.with(|count| count.set(0));

    let receipt = ensure_bidirectional_backup_receipt(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        1,
        PeerBidirectionalBackupSide::Local,
        &source,
        None,
        &NeverCancelled,
    )
    .unwrap()
    .into_receipt();

    assert_eq!(receipt.package_id, prepared.archive_sha256);
    assert!(!temporary.exists());
    assert_eq!(BACKUP_FULL_VERIFICATION_COUNT.with(Cell::get), 1);
    assert_eq!(
        Path::new(&receipt.path),
        bidirectional_backup_path(
            directory.path(),
            operation_id,
            PeerBidirectionalBackupSide::Local,
        )
    );
}

#[test]
fn unbound_truncated_final_is_replaced_but_a_bound_final_is_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut store, &cas);
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let source = SyncGenerationIdentity {
        generation_id: active.manifest.generation,
        manifest_hash: active.manifest_hash,
        generation_sequence: active.manifest.generation_sequence,
    };
    let operation_id = "123e4567-e89b-42d3-a456-426614174096";
    let final_path = bidirectional_backup_path(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Local,
    );
    fs::create_dir_all(final_path.parent().unwrap()).unwrap();
    fs::write(&final_path, b"truncated unbound final").unwrap();

    let receipt = ensure_bidirectional_backup_receipt(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        1,
        PeerBidirectionalBackupSide::Local,
        &source,
        None,
        &NeverCancelled,
    )
    .unwrap()
    .into_receipt();
    let verified_bytes = fs::read(&final_path).unwrap();
    assert_ne!(verified_bytes, b"truncated unbound final");

    fs::write(&final_path, b"truncated bound final").unwrap();
    let error = ensure_bidirectional_backup_receipt(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        1,
        PeerBidirectionalBackupSide::Local,
        &source,
        Some(&receipt.package_id),
        &NeverCancelled,
    )
    .unwrap_err();
    assert!(matches!(error, PeerSyncError::Storage(_)));
    assert_eq!(fs::read(&final_path).unwrap(), b"truncated bound final");
}

#[cfg(not(windows))]
#[test]
fn non_windows_existing_final_retries_parent_sync_after_publication_failure() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut store, &cas);
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let source = SyncGenerationIdentity {
        generation_id: active.manifest.generation,
        manifest_hash: active.manifest_hash,
        generation_sequence: active.manifest.generation_sequence,
    };
    let operation_id = "123e4567-e89b-42d3-a456-426614174098";
    let final_path = bidirectional_backup_path(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Local,
    );
    ATOMIC_PARENT_SYNC_FAILPOINT.with(|enabled| enabled.set(true));

    assert!(ensure_bidirectional_backup_receipt(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        1,
        PeerBidirectionalBackupSide::Local,
        &source,
        None,
        &NeverCancelled,
    )
    .is_err());
    assert!(final_path.exists());

    BACKUP_FULL_VERIFICATION_COUNT.with(|count| count.set(0));
    ATOMIC_PARENT_SYNC_FAILPOINT.with(|enabled| enabled.set(true));
    assert!(ensure_bidirectional_backup_receipt(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        1,
        PeerBidirectionalBackupSide::Local,
        &source,
        None,
        &NeverCancelled,
    )
    .is_err());
    assert_eq!(BACKUP_FULL_VERIFICATION_COUNT.with(Cell::get), 1);
    assert!(final_path.exists());

    BACKUP_FULL_VERIFICATION_COUNT.with(|count| count.set(0));
    let recovered = ensure_bidirectional_backup_receipt(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        1,
        PeerBidirectionalBackupSide::Local,
        &source,
        None,
        &NeverCancelled,
    )
    .unwrap();
    assert_eq!(BACKUP_FULL_VERIFICATION_COUNT.with(Cell::get), 1);
    assert!(!recovered.published);
    assert_eq!(Path::new(&recovered.receipt.path), final_path);
}

#[test]
fn source_backup_cancellation_preserves_authority_and_removes_incomplete_output() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut store, &cas);
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let peer_id = "123e4567-e89b-42d3-a456-426614174097";
    let operation_id = "123e4567-e89b-42d3-a456-426614174098";
    let source = SyncGenerationIdentity {
        generation_id: active.manifest.generation.clone(),
        manifest_hash: active.manifest_hash.clone(),
        generation_sequence: active.manifest.generation_sequence.clone(),
    };
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &active.manifest.generation,
        1,
        &active.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                source.clone(),
                0,
            )
            .unwrap(),
            1,
        )
        .unwrap();
    let expected_ack = store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
        .unwrap();
    let expected_common = store
        .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
        .unwrap();
    let cancelled = Arc::new(AtomicBool::new(true));

    let error = ensure_bidirectional_backup_receipt(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        1,
        PeerBidirectionalBackupSide::Remote,
        &source,
        None,
        &AtomicCancellation::new(Arc::clone(&cancelled)),
    )
    .unwrap_err();

    assert_eq!(error, PeerSyncError::Cancelled);
    assert!(cancelled.load(Ordering::SeqCst));
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(
        store
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        expected_ack
    );
    assert_eq!(
        store
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        expected_common
    );
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert!(!bidirectional_backup_path(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    )
    .exists());
}

fn assert_source_post_publish_cancellation(preexisting_backup: bool) {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut store, &cas);
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let peer_id = "123e4567-e89b-42d3-a456-426614174095";
    let source_device_id = "123e4567-e89b-42d3-a456-426614174096";
    let operation_id = "123e4567-e89b-42d3-a456-426614174097";
    let source_identity = SyncGenerationIdentity {
        generation_id: active.manifest.generation.clone(),
        manifest_hash: active.manifest_hash.clone(),
        generation_sequence: active.manifest.generation_sequence.clone(),
    };
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &active.manifest.generation,
        1,
        &active.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                source_identity.clone(),
                0,
            )
            .unwrap(),
            1,
        )
        .unwrap();
    let expected_ack = store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
        .unwrap();
    let expected_common = store
        .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
        .unwrap();
    let backup_path = bidirectional_backup_path(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    let preexisting_bytes = if preexisting_backup {
        let ensured = ensure_bidirectional_backup_receipt(
            &mut store,
            &cas,
            directory.path(),
            operation_id,
            1,
            PeerBidirectionalBackupSide::Remote,
            &source_identity,
            None,
            &NeverCancelled,
        )
        .unwrap();
        assert!(ensured.published);
        Some(fs::read(&backup_path).unwrap())
    } else {
        None
    };
    fs::create_dir_all(directory.path().join("assets-v2").join("job-pins")).unwrap();
    let jobs_before = durable_job_journal_ids(directory.path());
    let shared_generation = LanBidirectionalGeneration {
        generation_id: active.manifest.generation.clone(),
        manifest_hash: active.manifest_hash.clone(),
        generation_sequence: active.manifest.generation_sequence.clone(),
    };
    let mut source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    SOURCE_AFTER_BACKUP_PUBLISH_CANCEL_FAILPOINT.with(|enabled| enabled.set(true));

    let error = apply_bidirectional_remote_shared_inner(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        peer_id,
        1,
        &source_identity.manifest_hash,
        &source_identity,
        shared_generation,
        &active.manifest_bytes,
        &mut source,
        true,
        Some(source_device_id),
        None,
        None,
        false,
        &NeverCancelled,
    )
    .unwrap_err();

    assert_eq!(error, PeerSyncError::Cancelled);
    assert_eq!(source.reads, 0);
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(
        store
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        expected_ack
    );
    assert_eq!(
        store
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        expected_common
    );
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::SourcePrepared {
            ref operation_id,
            ref durable_job_id,
            backup_required: true,
            backup: Some(_),
            ..
        }) if operation_id == "123e4567-e89b-42d3-a456-426614174097"
            && durable_job_id == operation_id
    ));
    assert_eq!(durable_job_journal_ids(directory.path()), jobs_before);
    match preexisting_bytes {
        Some(bytes) => assert_eq!(fs::read(&backup_path).unwrap(), bytes),
        None => assert!(backup_path.is_file()),
    }
    PeerBidirectionalOperationJournal::new(directory.path())
        .abandon(operation_id)
        .unwrap();
    assert!(!backup_path.exists());
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
}

#[test]
fn cancellation_after_new_backup_publication_keeps_it_owned_until_abandon() {
    assert_source_post_publish_cancellation(false);
}

#[test]
fn cancellation_after_existing_backup_verification_keeps_it_owned_until_abandon() {
    assert_source_post_publish_cancellation(true);
}

#[test]
fn source_backup_cancellation_surfaces_cleanup_failure() {
    let directory = tempfile::tempdir().unwrap();
    let owned_directory = directory.path().join("not-a-backup-file");
    fs::create_dir(&owned_directory).unwrap();

    let error =
        remove_newly_published_backup_after_cancellation(Some(&owned_directory)).unwrap_err();

    assert!(
        matches!(error, PeerSyncError::Storage(message) if message.contains("newly published backup"))
    );
    assert!(owned_directory.is_dir());
}

#[test]
fn cancelled_unbound_final_verification_preserves_the_existing_backup() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut store, &cas);
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let source = SyncGenerationIdentity {
        generation_id: active.manifest.generation.clone(),
        manifest_hash: active.manifest_hash.clone(),
        generation_sequence: active.manifest.generation_sequence.clone(),
    };
    let peer_id = "123e4567-e89b-42d3-a456-426614174093";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &active.manifest.generation,
        1,
        &active.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                source.clone(),
                0,
            )
            .unwrap(),
            1,
        )
        .unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174094";
    let backup = ensure_bidirectional_backup_receipt(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        1,
        PeerBidirectionalBackupSide::Remote,
        &source,
        None,
        &NeverCancelled,
    )
    .unwrap();
    assert!(backup.published);
    let backup_path = PathBuf::from(&backup.receipt.path);
    let expected_bytes = fs::read(&backup_path).unwrap();
    let expected_revision = store.revision().unwrap();
    let expected_ack = store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
        .unwrap();
    let expected_common = store
        .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
        .unwrap();
    let expected_operation = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap();
    let expected_jobs = durable_job_journal_ids(directory.path());
    let cancelled = Arc::new(AtomicBool::new(true));

    let error = ensure_bidirectional_backup_receipt(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        1,
        PeerBidirectionalBackupSide::Remote,
        &source,
        None,
        &AtomicCancellation::new(cancelled),
    )
    .unwrap_err();

    assert_eq!(error, PeerSyncError::Cancelled);
    assert_eq!(fs::read(&backup_path).unwrap(), expected_bytes);
    assert_eq!(store.revision().unwrap(), expected_revision);
    assert_eq!(
        store
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        expected_ack
    );
    assert_eq!(
        store
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        expected_common
    );
    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        expected_operation
    );
    assert_eq!(durable_job_journal_ids(directory.path()), expected_jobs);
}

#[test]
fn cancelled_reusable_temp_verification_preserves_the_temp() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut store, &cas);
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let source = SyncGenerationIdentity {
        generation_id: active.manifest.generation,
        manifest_hash: active.manifest_hash,
        generation_sequence: active.manifest.generation_sequence,
    };
    let operation_id = "123e4567-e89b-42d3-a456-426614174095";
    let (staging, temporary) = bidirectional_backup_staging_paths(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    fs::create_dir_all(&staging).unwrap();
    create_and_verify_peer_bidirectional_backup_v1_report(
        &temporary,
        &staging,
        &cas,
        &mut store,
        1,
        &lossless_source_binding(operation_id, PeerBidirectionalBackupSide::Remote, &source),
        &NeverCancelled,
    )
    .unwrap();
    let expected_bytes = fs::read(&temporary).unwrap();
    let expected_revision = store.revision().unwrap();
    let expected_operation = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap();
    let expected_jobs = durable_job_journal_ids(directory.path());
    let final_path = bidirectional_backup_path(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    let cancelled = Arc::new(AtomicBool::new(true));

    let error = ensure_bidirectional_backup_receipt(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        1,
        PeerBidirectionalBackupSide::Remote,
        &source,
        None,
        &AtomicCancellation::new(cancelled),
    )
    .unwrap_err();

    assert_eq!(error, PeerSyncError::Cancelled);
    assert_eq!(fs::read(&temporary).unwrap(), expected_bytes);
    assert!(!final_path.exists());
    assert_eq!(store.revision().unwrap(), expected_revision);
    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        expected_operation
    );
    assert_eq!(durable_job_journal_ids(directory.path()), expected_jobs);
}

#[test]
fn cancelled_create_race_preserves_the_final_and_removes_the_owned_temp() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut store, &cas);
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let source = SyncGenerationIdentity {
        generation_id: active.manifest.generation.clone(),
        manifest_hash: active.manifest_hash.clone(),
        generation_sequence: active.manifest.generation_sequence.clone(),
    };
    let peer_id = "123e4567-e89b-42d3-a456-426614174092";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &active.manifest.generation,
        1,
        &active.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                source.clone(),
                0,
            )
            .unwrap(),
            1,
        )
        .unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174091";
    let expected_revision = store.revision().unwrap();
    let expected_ack = store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
        .unwrap();
    let expected_common = store
        .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
        .unwrap();
    let expected_operation = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap();
    let expected_jobs = durable_job_journal_ids(directory.path());
    let cancelled = Arc::new(AtomicBool::new(false));
    BACKUP_CREATE_RACE_CANCEL_FAILPOINT.with(|slot| slot.replace(Some(Arc::clone(&cancelled))));

    let error = ensure_bidirectional_backup_receipt(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        1,
        PeerBidirectionalBackupSide::Remote,
        &source,
        None,
        &AtomicCancellation::new(Arc::clone(&cancelled)),
    )
    .unwrap_err();

    assert_eq!(error, PeerSyncError::Cancelled);
    assert!(cancelled.load(Ordering::SeqCst));
    let final_path = bidirectional_backup_path(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    assert!(!fs::read(&final_path).unwrap().is_empty());
    verify_bidirectional_backup_receipt(
        &final_path,
        operation_id,
        1,
        PeerBidirectionalBackupSide::Remote,
        &source,
        None,
        &NeverCancelled,
    )
    .unwrap();
    let (_, temporary) = bidirectional_backup_staging_paths(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    assert!(!temporary.exists());
    assert_eq!(store.revision().unwrap(), expected_revision);
    assert_eq!(
        store
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        expected_ack
    );
    assert_eq!(
        store
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        expected_common
    );
    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        expected_operation
    );
    assert_eq!(durable_job_journal_ids(directory.path()), expected_jobs);
}

fn remote_disjoint_manifest(
    base: &crate::peer_sync::logical_delta::LogicalManifest,
) -> crate::peer_sync::logical_delta::BuiltLogicalManifest {
    remote_disjoint_manifest_with_plugin_records(base, 1)
}

fn remote_disjoint_manifest_with_plugin_records(
    base: &crate::peer_sync::logical_delta::LogicalManifest,
    plugin_records: usize,
) -> crate::peer_sync::logical_delta::BuiltLogicalManifest {
    let mut records = vec![ProjectedLogicalRecord::live(
        LogicalRecordLocator::Root,
        LogicalRecordEnvelope::Root {
            value: json!({}),
            owner_heads: vec![],
        },
        vec![],
    )];
    records.extend((0..plugin_records).map(|index| {
        let storage_key = if plugin_records == 1 {
            "remote-key".to_owned()
        } else {
            format!("remote-key-{index:03}")
        };
        ProjectedLogicalRecord::live(
            LogicalRecordLocator::Plugin { storage_key },
            LogicalRecordEnvelope::Plugin {
                ordinal: u64::try_from(index).unwrap(),
                value: if plugin_records == 1 {
                    json!({"side": "remote"})
                } else {
                    json!({"side": "remote", "index": index})
                },
            },
            vec![],
        )
    }));
    build_logical_manifest(LogicalManifestBuilderInput {
        library_id: base.library_id.clone(),
        generation: "remote-disjoint".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: Some(base.generation.clone()),
        source_revision: base.source_revision + 1,
        records,
    })
    .unwrap()
}

fn fixture_source(built: &crate::peer_sync::logical_delta::BuiltLogicalManifest) -> FixtureSource {
    FixtureSource {
        objects: built
            .record_objects
            .iter()
            .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
            .collect(),
        reads: 0,
    }
}

fn disjoint_target_fixture() -> (
    tempfile::TempDir,
    PayloadCas,
    PersistentStore,
    crate::peer_sync::logical_delta::BuiltLogicalManifest,
    LanBidirectionalLogicalCredential,
) {
    disjoint_target_fixture_with_plugin_records(1)
}

fn disjoint_target_fixture_with_plugin_records(
    plugin_records: usize,
) -> (
    tempfile::TempDir,
    PayloadCas,
    PersistentStore,
    crate::peer_sync::logical_delta::BuiltLogicalManifest,
    LanBidirectionalLogicalCredential,
) {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let peer_id = "123e4567-e89b-42d3-a456-426614174012";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    let common = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                common,
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"side": "local"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let remote = remote_disjoint_manifest_with_plugin_records(&base.manifest, plugin_records);
    let credential = LanBidirectionalLogicalCredential {
        endpoint: "http://192.168.1.2:32146".to_owned(),
        session_id: "123e4567-e89b-42d3-a456-426614174010".to_owned(),
        manifest_id: remote.manifest_hash.clone(),
        device_id: "123e4567-e89b-42d3-a456-426614174011".to_owned(),
        source_device_id: peer_id.to_owned(),
        bearer: "d".repeat(64),
    };
    (directory, cas, store, remote, credential)
}

struct CancelOnProbeCall {
    calls: Cell<usize>,
    cancel_on: usize,
}

impl CancellationProbe for CancelOnProbeCall {
    fn is_cancelled(&self) -> bool {
        let call = self.calls.get() + 1;
        self.calls.set(call);
        call >= self.cancel_on
    }
}

#[test]
fn android_p5_cancellation_before_and_after_target_preparation_preserves_durable_boundaries() {
    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut source = fixture_source(&remote);
    let before_journal = CancelOnProbeCall {
        calls: Cell::new(0),
        cancel_on: 1,
    };
    assert_eq!(
        begin_bidirectional_local_merge_with_cancellation(
            &mut store,
            &cas,
            directory.path(),
            credential,
            1,
            &remote.manifest_bytes,
            &mut source,
            &before_journal,
            None,
            PeerBidirectionalCompletionMode::Legacy,
        ),
        Err(PeerSyncError::Cancelled),
    );
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert_eq!(store.revision().unwrap(), 1);

    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut source = fixture_source(&remote);
    let after_prepared = CancelOnProbeCall {
        calls: Cell::new(0),
        cancel_on: 2,
    };
    assert_eq!(
        begin_bidirectional_local_merge_with_cancellation(
            &mut store,
            &cas,
            directory.path(),
            credential,
            1,
            &remote.manifest_bytes,
            &mut source,
            &after_prepared,
            None,
            PeerBidirectionalCompletionMode::Legacy,
        ),
        Err(PeerSyncError::Cancelled),
    );
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::TargetPrepared { .. })
    ));
    assert_eq!(store.revision().unwrap(), 1);
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn durable_job_journal_ids(repository_root: &Path) -> Vec<String> {
    let directory = repository_root.join("assets-v2").join("job-pins");
    let Ok(entries) = fs::read_dir(directory) else {
        return vec![];
    };
    let mut ids = entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter_map(|name| {
            name.strip_prefix("job-")
                .and_then(|name| name.strip_suffix(".journal"))
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

fn assert_no_logical_delta_staging_orphans(repository_root: &Path) {
    let connection =
        rusqlite::Connection::open(repository_root.join("persistent").join("persistent.db"))
            .unwrap();
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let staging_generations: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM root WHERE generation LIKE 'staging-logical-%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let staging_leases: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM snapshot_leases WHERE lease LIKE 'logical-delta-pin-%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let staging_root = repository_root.join("peer-bidirectional").join("staging");
    let staging_directories = if staging_root.exists() {
        fs::read_dir(staging_root).unwrap().count()
    } else {
        0
    };
    assert_eq!(staging_generations, 0, "orphaned PDS staging generation");
    assert_eq!(staging_leases, 0, "orphaned logical delta snapshot lease");
    assert_eq!(staging_directories, 0, "orphaned logical delta directory");
}

#[test]
fn operation_journal_rejects_a_missing_newly_published_backup() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174003";
    let operation = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: context(operation_id),
        committed_revision: 8,
        shared_generation: generation("local-a", "3", 'e'),
        changed: true,
        remote_backup_required: false,
        remote_apply_receipt: None,
        transferred_objects: 2,
        transferred_bytes: 19,
        backups: vec![PeerBidirectionalBackupReceipt {
            package_id: "f".repeat(64),
            side: PeerBidirectionalBackupSide::Local,
            path: bidirectional_backup_path(
                directory.path(),
                operation_id,
                PeerBidirectionalBackupSide::Local,
            )
            .to_string_lossy()
            .into_owned(),
        }],
    };

    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path()).store(&operation),
        Err(PeerSyncError::Storage(message))
            if message == "bidirectional backup disappeared before receipt publication"
    ));
    assert!(!directory
        .path()
        .join("peer-bidirectional/operation.json")
        .exists());
}

#[test]
fn local_committed_operation_survives_reopen_and_atomic_completion() {
    let directory = tempfile::tempdir().unwrap();
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    let operation_id = "123e4567-e89b-42d3-a456-426614174004";
    let local_committed = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: context(operation_id),
        committed_revision: 8,
        shared_generation: generation("local-a", "3", 'e'),
        changed: true,
        remote_backup_required: false,
        remote_apply_receipt: None,
        transferred_objects: 2,
        transferred_bytes: 19,
        backups: vec![backup(PeerBidirectionalBackupSide::Local)],
    };
    journal.store(&local_committed).unwrap();
    assert_eq!(journal.load().unwrap(), Some(local_committed));

    let completed = PeerBidirectionalDurableOperation::Completed {
        schema: OPERATION_SCHEMA.to_owned(),
        remote_apply_receipt: None,
        source_binding: None,
        result: PeerBidirectionalCompletedResult {
            kind: "updated".to_owned(),
            operation_id: operation_id.to_owned(),
            revision: 8,
            remote_revision: 4,
            transferred_objects: 3,
            transferred_bytes: 27,
            backups: vec![backup(PeerBidirectionalBackupSide::Local)],
        },
    };
    journal.store(&completed).unwrap();
    assert_eq!(journal.load().unwrap(), Some(completed));
}

#[test]
fn bidirectional_durable_completion_accounts_once_and_retained_retry_does_not() {
    let directory = tempfile::tempdir().unwrap();
    register_bidirectional_accounting_source(directory.path(), 40, 7);
    let operation_id = "123e4567-e89b-42d3-a456-426614174098";
    let result = PeerBidirectionalCompletedResult {
        kind: "updated".to_owned(),
        operation_id: operation_id.to_owned(),
        revision: 8,
        remote_revision: 4,
        transferred_objects: 3,
        transferred_bytes: 27,
        backups: Vec::new(),
    };
    let completed = PeerBidirectionalDurableOperation::Completed {
        schema: OPERATION_SCHEMA.to_owned(),
        remote_apply_receipt: None,
        source_binding: None,
        result: result.clone(),
    };

    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&completed)
        .unwrap();
    let mut retained_context = context(operation_id);
    retained_context.completion_mode = PeerBidirectionalCompletionMode::Unsupported;
    record_bidirectional_completion(directory.path(), &retained_context, &result).unwrap();
    let after_completed = bidirectional_accounting_source(directory.path());
    assert_eq!(after_completed.total_bytes, 67);
    assert!(after_completed.last_seen_ms > 7);
    let receipt_id = super::super::device_registry::completion_receipt_id(
        "bidirectional",
        operation_id,
        &retained_context.credential.manifest_id,
    );
    assert!(
        super::super::device_registry::incoming_completed_operation_recorded_for_lane(
            directory.path(),
            BIDIRECTIONAL_ACCOUNTING_SOURCE_ID,
            super::super::device_registry::CompletionLane::Bidirectional,
            &receipt_id,
        )
        .unwrap()
    );

    record_bidirectional_completion(directory.path(), &retained_context, &result).unwrap();
    assert_eq!(
        bidirectional_accounting_source(directory.path()).total_bytes,
        after_completed.total_bytes,
        "a retained retry must reuse the deterministic bidirectional receipt"
    );

    assert!(matches!(
        retained_result(&completed).unwrap(),
        PeerBidirectionalSyncResult::Updated { .. }
    ));
    assert!(matches!(
        retained_result(&completed).unwrap(),
        PeerBidirectionalSyncResult::Updated { .. }
    ));
    assert_eq!(
        bidirectional_accounting_source(directory.path()).total_bytes,
        after_completed.total_bytes
    );
}

#[test]
fn migrated_legacy_completion_does_not_create_new_accounting() {
    let directory = tempfile::tempdir().unwrap();
    register_bidirectional_accounting_source(directory.path(), 40, 7);
    let operation_id = "123e4567-e89b-42d3-a456-426614174093";
    let result = PeerBidirectionalCompletedResult {
        kind: "updated".to_owned(),
        operation_id: operation_id.to_owned(),
        revision: 8,
        remote_revision: 4,
        transferred_objects: 1,
        transferred_bytes: 27,
        backups: vec![],
    };

    record_bidirectional_completion(directory.path(), &context(operation_id), &result).unwrap();

    assert_eq!(
        bidirectional_accounting_source(directory.path()).total_bytes,
        40
    );
}

#[test]
fn registered_source_removal_waits_for_the_bidirectional_target_journal() {
    let directory = tempfile::tempdir().unwrap();
    register_bidirectional_accounting_source(directory.path(), 0, 7);
    let operation_id = "123e4567-e89b-42d3-a456-426614174094";
    let mut retained_context = context(operation_id);
    retained_context.completion_mode = PeerBidirectionalCompletionMode::Unsupported;
    retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    retained_context.expected_remote_generation.manifest_hash =
        retained_context.credential.manifest_id.clone();
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal
        .store(&PeerBidirectionalDurableOperation::TargetPrepared {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context,
            local_generation: generation("local", "1", 'e'),
            conflict_policy: TargetPreparedConflictPolicy::Reject,
            changed: false,
            remote_backup_required: false,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        })
        .unwrap();

    assert!(
        super::super::registry_commands::remove_incoming_source_if_inactive(
            directory.path(),
            BIDIRECTIONAL_ACCOUNTING_SOURCE_ID,
        )
        .is_err()
    );
    assert!(super::super::device_registry::incoming_source_by_id(
        directory.path(),
        BIDIRECTIONAL_ACCOUNTING_SOURCE_ID,
    )
    .unwrap()
    .is_some());

    journal
        .store(&PeerBidirectionalDurableOperation::Completed {
            schema: OPERATION_SCHEMA.to_owned(),
            remote_apply_receipt: None,
            source_binding: None,
            result: PeerBidirectionalCompletedResult {
                kind: "noChanges".to_owned(),
                operation_id: operation_id.to_owned(),
                revision: 0,
                remote_revision: 0,
                transferred_objects: 0,
                transferred_bytes: 0,
                backups: vec![],
            },
        })
        .unwrap();
    super::super::registry_commands::remove_incoming_source_if_inactive(
        directory.path(),
        BIDIRECTIONAL_ACCOUNTING_SOURCE_ID,
    )
    .unwrap();
}

#[test]
fn initial_registered_target_publication_revalidates_credential_before_store() {
    for phase in ["targetPrepared", "awaitingConflict"] {
        let directory = tempfile::tempdir().unwrap();
        register_bidirectional_accounting_source(directory.path(), 0, 7);
        let operation_id = "123e4567-e89b-42d3-a456-426614174095";
        let mut retained_context = context(operation_id);
        retained_context.completion_mode = PeerBidirectionalCompletionMode::Unsupported;
        retained_context.credential.bearer = "d".repeat(64);
        let operation = if phase == "targetPrepared" {
            PeerBidirectionalDurableOperation::TargetPrepared {
                schema: OPERATION_SCHEMA.to_owned(),
                context: retained_context,
                local_generation: generation("local", "1", 'e'),
                conflict_policy: TargetPreparedConflictPolicy::Reject,
                changed: false,
                remote_backup_required: false,
                transferred_objects: 0,
                transferred_bytes: 0,
                backups: vec![],
            }
        } else {
            PeerBidirectionalDurableOperation::AwaitingConflict {
                schema: OPERATION_SCHEMA.to_owned(),
                context: retained_context,
                conflicts: vec![PeerBidirectionalConflict {
                    key: "root".to_owned(),
                    conflict_type: "bothChanged".to_owned(),
                }],
                local_generation: generation("local", "1", 'e'),
                local_manifest_hash: "e".repeat(64),
                remote_manifest_hash: "d".repeat(64),
                backups: vec![],
            }
        };
        let journal = PeerBidirectionalOperationJournal::new(directory.path());

        assert!(matches!(
            store_initial_registered_target_operation(directory.path(), &journal, &operation),
            Err(PeerSyncError::Validation(_))
        ));
        assert!(journal.load().unwrap().is_none());
    }
}

#[test]
fn legacy_target_journal_does_not_claim_registered_source_activity() {
    let directory = tempfile::tempdir().unwrap();
    register_bidirectional_accounting_source(directory.path(), 0, 7);
    let operation_id = "123e4567-e89b-42d3-a456-426614174096";
    let mut retained_context = context(operation_id);
    retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    retained_context.expected_remote_generation.manifest_hash =
        retained_context.credential.manifest_id.clone();
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&PeerBidirectionalDurableOperation::TargetPrepared {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context,
            local_generation: generation("local", "1", 'e'),
            conflict_policy: TargetPreparedConflictPolicy::Reject,
            changed: false,
            remote_backup_required: false,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        })
        .unwrap();

    assert!(!registered_bidirectional_source_is_active(
        directory.path(),
        BIDIRECTIONAL_ACCOUNTING_SOURCE_ID,
    )
    .unwrap());
    super::super::registry_commands::remove_incoming_source_if_inactive(
        directory.path(),
        BIDIRECTIONAL_ACCOUNTING_SOURCE_ID,
    )
    .unwrap();
}

#[test]
fn v1_local_commit_persists_the_exact_pending_completion_delivery() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174097";
    let mut retained_context = context(operation_id);
    retained_context.completion_mode = PeerBidirectionalCompletionMode::V1;
    let delivery = super::super::device_registry::PendingCompletionDelivery {
        source_device_id: retained_context.credential.source_device_id.clone(),
        lane: "bidirectional".to_owned(),
        completion_lease_id: operation_id.to_owned(),
        manifest_id: retained_context.credential.manifest_id.clone(),
        useful_bytes: 0,
        receipt_id: super::super::device_registry::completion_receipt_id(
            "bidirectional",
            operation_id,
            &retained_context.credential.manifest_id,
        ),
    };
    retained_context.completion_delivery = Some(delivery.clone());
    let operation = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: retained_context,
        committed_revision: 8,
        shared_generation: generation("shared-zero", "3", 'e'),
        changed: false,
        remote_backup_required: false,
        remote_apply_receipt: Some(LanBidirectionalRemoteApplyReceipt {
            committed_revision: 7,
            committed_generation: LanBidirectionalGeneration {
                generation_id: "shared-zero".to_owned(),
                manifest_hash: "e".repeat(64),
                generation_sequence: "3".to_owned(),
            },
            transferred_objects: 0,
            transferred_bytes: 0,
            backup: None,
        }),
        transferred_objects: 0,
        transferred_bytes: 0,
        backups: vec![],
    };

    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&operation).unwrap();
    let Some(PeerBidirectionalDurableOperation::LocalCommitted { context, .. }) =
        journal.load().unwrap()
    else {
        panic!("expected retained V1 local commit");
    };
    assert_eq!(context.completion_mode, PeerBidirectionalCompletionMode::V1);
    assert_eq!(context.completion_delivery, Some(delivery));
}

#[test]
fn v1_zero_byte_completion_retains_a_target_receipt_without_changing_its_total() {
    let directory = tempfile::tempdir().unwrap();
    register_bidirectional_accounting_source(directory.path(), 0, 7);
    let operation_id = "123e4567-e89b-42d3-a456-426614174095";
    let mut retained_context = context(operation_id);
    retained_context.completion_mode = PeerBidirectionalCompletionMode::V1;
    let delivery = super::super::device_registry::PendingCompletionDelivery {
        source_device_id: retained_context.credential.source_device_id.clone(),
        lane: "bidirectional".to_owned(),
        completion_lease_id: operation_id.to_owned(),
        manifest_id: retained_context.credential.manifest_id.clone(),
        useful_bytes: 0,
        receipt_id: super::super::device_registry::completion_receipt_id(
            "bidirectional",
            operation_id,
            &retained_context.credential.manifest_id,
        ),
    };
    retained_context.completion_delivery = Some(delivery.clone());
    assert_eq!(
        super::super::device_registry::prepare_incoming_completion_delivery(
            directory.path(),
            delivery.clone(),
        )
        .unwrap(),
        super::super::device_registry::CompletionDeliveryPrepareStatus::Pending,
    );
    super::super::device_registry::finalize_incoming_completion_delivery(
        directory.path(),
        &delivery,
    )
    .unwrap();
    let remote = LanBidirectionalRemoteApplyReceipt {
        committed_revision: 7,
        committed_generation: LanBidirectionalGeneration {
            generation_id: "shared-zero".to_owned(),
            manifest_hash: "e".repeat(64),
            generation_sequence: "3".to_owned(),
        },
        transferred_objects: 0,
        transferred_bytes: 0,
        backup: None,
    };
    complete_v1_bidirectional_accounting(
        directory.path(),
        &PeerBidirectionalOperationJournal::new(directory.path()),
        &mut retained_context,
        8,
        &generation("shared-zero", "3", 'e'),
        false,
        false,
        &remote,
        0,
        0,
        &[],
    )
    .unwrap();

    assert_eq!(
        bidirectional_accounting_source(directory.path()).total_bytes,
        0
    );
    assert!(
        super::super::device_registry::incoming_completed_operation_recorded_for_lane(
            directory.path(),
            BIDIRECTIONAL_ACCOUNTING_SOURCE_ID,
            super::super::device_registry::CompletionLane::Bidirectional,
            &delivery.receipt_id,
        )
        .unwrap()
    );
}

#[test]
fn bidirectional_completion_accounting_overflow_preserves_the_terminal_journal() {
    let directory = tempfile::tempdir().unwrap();
    register_bidirectional_accounting_source(directory.path(), u64::MAX, 7);
    let result = PeerBidirectionalCompletedResult {
        kind: "updated".to_owned(),
        operation_id: "123e4567-e89b-42d3-a456-426614174099".to_owned(),
        revision: 8,
        remote_revision: 4,
        transferred_objects: 3,
        transferred_bytes: 1,
        backups: Vec::new(),
    };
    let completed = PeerBidirectionalDurableOperation::Completed {
        schema: OPERATION_SCHEMA.to_owned(),
        remote_apply_receipt: None,
        source_binding: None,
        result: result.clone(),
    };
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&completed)
        .unwrap();
    let before = std::fs::read(directory.path().join("peer-sync/sources.json")).unwrap();

    let mut retained_context = context(&result.operation_id);
    retained_context.completion_mode = PeerBidirectionalCompletionMode::Unsupported;
    assert!(record_bidirectional_completion(directory.path(), &retained_context, &result).is_err());

    assert_eq!(
        retained_result(
            &PeerBidirectionalOperationJournal::new(directory.path())
                .load()
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        PeerBidirectionalSyncResult::Updated {
            operation_id: result.operation_id,
            revision: result.revision,
            remote_revision: result.remote_revision,
            transferred_objects: result.transferred_objects,
            transferred_bytes: result.transferred_bytes,
            backups: result.backups,
        }
    );
    assert_eq!(
        std::fs::read(directory.path().join("peer-sync/sources.json")).unwrap(),
        before
    );
}

#[test]
fn source_prepared_operation_survives_reopen_without_changing_older_journals() {
    let directory = tempfile::tempdir().unwrap();
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    let operation_id = "123e4567-e89b-42d3-a456-426614174104";
    let prepared = PeerBidirectionalDurableOperation::SourcePrepared {
        schema: OPERATION_SCHEMA.to_owned(),
        operation_id: operation_id.to_owned(),
        source_device_id: "123e4567-e89b-42d3-a456-426614174105".to_owned(),
        target_device_id: "123e4567-e89b-42d3-a456-426614174106".to_owned(),
        expected_source_revision: 7,
        previous_shared: generation("shared-b", "2", 'a'),
        expected_source_generation: generation("source-e", "3", 'b'),
        shared_generation: LanBidirectionalGeneration {
            generation_id: "shared-a".to_owned(),
            manifest_hash: "c".repeat(64),
            generation_sequence: "4".to_owned(),
        },
        incoming_revision: 8,
        transferred_objects: 2,
        transferred_bytes: 19,
        backup_required: true,
        backup: Some(LanBidirectionalBackupReceipt {
            package_id: "d".repeat(64),
            path: "peer-bidirectional/backups/source.risulossless".to_owned(),
        }),
        completion_deferred_v1: false,
        durable_job_id: "123e4567-e89b-42d3-a456-426614174107".to_owned(),
    };

    journal.store(&prepared).unwrap();
    assert_eq!(journal.load().unwrap(), Some(prepared.clone()));
    assert!(retained_allows_source_prepare(
        &prepared,
        "123e4567-e89b-42d3-a456-426614174105"
    ));
    assert!(!retained_allows_source_prepare(
        &prepared,
        "123e4567-e89b-42d3-a456-426614174111"
    ));
    let mut store = PersistentStore::open(directory.path()).unwrap();
    assert!(matches!(
        PeerBidirectionalCommandState::default()
            .status(directory.path(), &mut store)
            .unwrap()
            .operation,
        Some(PeerBidirectionalStatusOperation::SourcePrepared { operation_id: retained })
            if retained == operation_id
    ));

    let mut legacy_source = serde_json::to_value(&prepared).unwrap();
    legacy_source
        .as_object_mut()
        .unwrap()
        .remove("backupRequired");
    fs::write(
        journal.root.join(OPERATION_FILE),
        serde_json::to_vec(&legacy_source).unwrap(),
    )
    .unwrap();
    let legacy_source = journal.load().unwrap().unwrap();
    let legacy_evidence = SourcePreparedEvidence::from_operation(&legacy_source).unwrap();
    assert!(legacy_evidence.requires_backup());
    assert!(!legacy_evidence.completion_deferred_v1);

    let mut deferred_source = serde_json::to_value(&prepared).unwrap();
    deferred_source
        .as_object_mut()
        .unwrap()
        .insert("completionDeferredV1".to_owned(), json!(true));
    fs::write(
        journal.root.join(OPERATION_FILE),
        serde_json::to_vec(&deferred_source).unwrap(),
    )
    .unwrap();
    assert!(
        SourcePreparedEvidence::from_operation(&journal.load().unwrap().unwrap())
            .unwrap()
            .completion_deferred_v1
    );

    let old_fixture = serde_json::to_vec(&PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: context(operation_id),
        committed_revision: 8,
        shared_generation: generation("local-a", "3", 'e'),
        changed: true,
        remote_backup_required: true,
        remote_apply_receipt: None,
        transferred_objects: 2,
        transferred_bytes: 19,
        backups: vec![],
    })
    .unwrap();
    fs::write(journal.root.join(OPERATION_FILE), old_fixture).unwrap();
    assert!(matches!(
        journal.load().unwrap(),
        Some(PeerBidirectionalDurableOperation::LocalCommitted {
            remote_apply_receipt: None,
            ..
        })
    ));
}

#[test]
fn source_completed_journal_rejects_binding_result_mismatches() {
    let directory = tempfile::tempdir().unwrap();
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    let binding = SourcePreparedEvidence {
        operation_id: "123e4567-e89b-42d3-a456-426614174112".to_owned(),
        source_device_id: "123e4567-e89b-42d3-a456-426614174113".to_owned(),
        target_device_id: "123e4567-e89b-42d3-a456-426614174114".to_owned(),
        expected_source_revision: 7,
        previous_shared: generation("shared-b", "2", 'a'),
        expected_source_generation: generation("source-e", "3", 'b'),
        shared_generation: LanBidirectionalGeneration {
            generation_id: "shared-a".to_owned(),
            manifest_hash: "c".repeat(64),
            generation_sequence: "4".to_owned(),
        },
        incoming_revision: 9,
        transferred_objects: 2,
        transferred_bytes: 19,
        backup_required: true,
        backup: Some(LanBidirectionalBackupReceipt {
            package_id: "d".repeat(64),
            path: "peer-bidirectional/backups/source.risulossless".to_owned(),
        }),
        completion_deferred_v1: false,
    };
    let receipt = binding.receipt_at(8).unwrap();
    let valid = PeerBidirectionalDurableOperation::Completed {
        schema: OPERATION_SCHEMA.to_owned(),
        remote_apply_receipt: Some(receipt),
        source_binding: Some(binding.clone()),
        result: PeerBidirectionalCompletedResult {
            kind: "updated".to_owned(),
            operation_id: binding.operation_id.clone(),
            revision: 8,
            remote_revision: binding.incoming_revision,
            transferred_objects: binding.transferred_objects,
            transferred_bytes: binding.transferred_bytes,
            backups: vec![PeerBidirectionalBackupReceipt {
                package_id: binding.backup.as_ref().unwrap().package_id.clone(),
                side: PeerBidirectionalBackupSide::Remote,
                path: binding.backup.as_ref().unwrap().path.clone(),
            }],
        },
    };
    journal.store(&valid).unwrap();
    let invalid = [
        {
            let mut operation = valid.clone();
            if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation {
                result.operation_id = "123e4567-e89b-42d3-a456-426614174115".to_owned();
            }
            ("operation", operation)
        },
        {
            let mut operation = valid.clone();
            if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation {
                result.revision = 7;
            }
            ("revision", operation)
        },
        {
            let mut operation = valid.clone();
            if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation {
                result.remote_revision += 1;
            }
            ("remote revision", operation)
        },
        {
            let mut operation = valid.clone();
            if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation {
                result.transferred_objects += 1;
            }
            ("object total", operation)
        },
        {
            let mut operation = valid.clone();
            if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation {
                result.transferred_bytes += 1;
            }
            ("byte total", operation)
        },
        {
            let mut operation = valid.clone();
            if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation {
                result.backups[0].side = PeerBidirectionalBackupSide::Local;
            }
            ("backup", operation)
        },
        {
            let mut operation = valid.clone();
            if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation {
                result.kind = "noChanges".to_owned();
            }
            ("kind", operation)
        },
        {
            let mut operation = valid.clone();
            if let PeerBidirectionalDurableOperation::Completed {
                remote_apply_receipt: Some(receipt),
                ..
            } = &mut operation
            {
                receipt.transferred_objects += 1;
            }
            ("receipt object total", operation)
        },
        {
            let mut operation = valid.clone();
            if let PeerBidirectionalDurableOperation::Completed {
                remote_apply_receipt: Some(receipt),
                ..
            } = &mut operation
            {
                receipt.transferred_bytes += 1;
            }
            ("receipt byte total", operation)
        },
        {
            let mut operation = valid.clone();
            if let PeerBidirectionalDurableOperation::Completed {
                remote_apply_receipt: Some(receipt),
                ..
            } = &mut operation
            {
                receipt.committed_generation.manifest_hash = "e".repeat(64);
            }
            ("receipt committed generation", operation)
        },
        {
            let mut operation = valid.clone();
            if let PeerBidirectionalDurableOperation::Completed {
                remote_apply_receipt: Some(receipt),
                ..
            } = &mut operation
            {
                receipt.backup.as_mut().unwrap().path.push_str(".corrupt");
            }
            ("receipt backup", operation)
        },
    ];

    for (field, operation) in invalid {
        fs::write(
            journal.root.join(OPERATION_FILE),
            serde_json::to_vec(&operation).unwrap(),
        )
        .unwrap();
        assert!(journal.load().is_err(), "accepted mismatched {field}");
    }
}

#[test]
fn source_completed_request_requires_the_original_backup_choice() {
    let operation_id = "123e4567-e89b-42d3-a456-426614174116";
    let source_device_id = "123e4567-e89b-42d3-a456-426614174117";
    let target_device_id = "123e4567-e89b-42d3-a456-426614174118";
    let expected_source_generation = generation("source-e", "3", 'b');
    let mut binding = SourcePreparedEvidence {
        operation_id: operation_id.to_owned(),
        source_device_id: source_device_id.to_owned(),
        target_device_id: target_device_id.to_owned(),
        expected_source_revision: 7,
        previous_shared: generation("shared-b", "2", 'a'),
        expected_source_generation: expected_source_generation.clone(),
        shared_generation: LanBidirectionalGeneration {
            generation_id: "shared-a".to_owned(),
            manifest_hash: "c".repeat(64),
            generation_sequence: "4".to_owned(),
        },
        incoming_revision: 8,
        transferred_objects: 0,
        transferred_bytes: 0,
        backup_required: false,
        backup: None,
        completion_deferred_v1: false,
    };
    let session = LanBidirectionalSession {
        session_id: "123e4567-e89b-42d3-a456-426614174119".to_owned(),
        source_device_id: source_device_id.to_owned(),
        target_device_id: target_device_id.to_owned(),
    };
    let mut request = LanBidirectionalRemoteApplyRequest {
        operation_id: operation_id.to_owned(),
        source_endpoint: "http://127.0.0.1:1".to_owned(),
        source_session_id: "123e4567-e89b-42d3-a456-426614174120".to_owned(),
        source_manifest_id: "manifest".to_owned(),
        source_claim: "claim".to_owned(),
        expected_source_revision: 7,
        expected_source_generation: LanBidirectionalGeneration {
            generation_id: expected_source_generation.generation_id,
            manifest_hash: expected_source_generation.manifest_hash,
            generation_sequence: expected_source_generation.generation_sequence,
        },
        expected_common_base_manifest_hash: binding.previous_shared.manifest_hash.clone(),
        backup_losing_side: false,
        completion_deferred_v1: None,
    };

    assert!(completed_source_request_matches(
        &binding, &session, &request
    ));
    request.backup_losing_side = true;
    assert!(!completed_source_request_matches(
        &binding, &session, &request
    ));
    binding.backup = Some(LanBidirectionalBackupReceipt {
        package_id: "d".repeat(64),
        path: "peer-bidirectional/backups/source.risulossless".to_owned(),
    });
    request.backup_losing_side = false;
    assert!(!completed_source_request_matches(
        &binding, &session, &request
    ));
    request.backup_losing_side = true;
    assert!(completed_source_request_matches(
        &binding, &session, &request
    ));
}

#[test]
fn receipt_before_ack_crash_preserves_a_descendant_edit_on_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let peer_id = "123e4567-e89b-42d3-a456-426614174097";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    let previous = SyncGenerationIdentity {
        generation_id: base.manifest.generation,
        manifest_hash: base.manifest_hash,
        generation_sequence: base.manifest.generation_sequence,
    };
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                previous.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"side": "shared"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let shared = SyncGenerationIdentity {
        generation_id: active.manifest.generation,
        manifest_hash: active.manifest_hash,
        generation_sequence: active.manifest.generation_sequence,
    };
    let operation_id = "123e4567-e89b-42d3-a456-426614174098";
    let mut retained_context = context(operation_id);
    retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    retained_context.credential.source_device_id = peer_id.to_owned();
    retained_context.expected_remote_revision = 4;
    retained_context.previous_shared = previous.clone();
    retained_context.previous_local = previous.clone();
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context.clone(),
            committed_revision: 1,
            shared_generation: shared.clone(),
            changed: true,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        })
        .unwrap();
    let receipt = LanBidirectionalRemoteApplyReceipt {
        committed_revision: 5,
        committed_generation: LanBidirectionalGeneration {
            generation_id: shared.generation_id.clone(),
            manifest_hash: shared.manifest_hash.clone(),
            generation_sequence: shared.generation_sequence.clone(),
        },
        transferred_objects: 3,
        transferred_bytes: 27,
        backup: None,
    };
    COMPLETE_BEFORE_ACK_FAILPOINT.with(|enabled| enabled.set(true));

    assert!(complete_bidirectional_local_after_remote_apply(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        receipt.clone(),
    )
    .is_err());
    assert_eq!(
        store
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap()
            .shared_identity,
        previous
    );
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({"side": "local-after-receipt"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    drop(store);
    let mut reopened = PersistentStore::open(directory.path()).unwrap();

    let resumed = resume_bidirectional_local_committed_with_remote(
        &mut reopened,
        &cas,
        directory.path(),
        operation_id,
        |_context, _revision, _shared, _manifest, _backup_required| {
            panic!("durable remote receipt must complete without a second network request")
        },
    )
    .unwrap();
    assert!(matches!(
        resumed,
        ResumeLocalCommittedOutcome::Completed(PeerBidirectionalCompletedResult {
            revision: 2,
            remote_revision: 5,
            transferred_objects: 3,
            transferred_bytes: 27,
            ..
        })
    ));
    assert_eq!(reopened.revision().unwrap(), 2);
    assert_eq!(
        reopened
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        Some(shared.clone())
    );

    reopened
        .commit(&WorkingSetCommit {
            expected_revision: 2,
            root: Some(json!({"side": "another-local-edit"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    reopened
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context,
            committed_revision: 1,
            shared_generation: shared.clone(),
            changed: true,
            remote_backup_required: false,
            remote_apply_receipt: Some(receipt),
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        })
        .unwrap();
    PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut reopened, operation_id)
        .unwrap();
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert_eq!(reopened.revision().unwrap(), 3);
    assert_eq!(
        reopened
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        Some(shared)
    );
}

#[test]
fn legacy_local_committed_journal_requires_remote_backup_conservatively() {
    let directory = tempfile::tempdir().unwrap();
    let operation_root = directory.path().join("peer-bidirectional");
    fs::create_dir_all(&operation_root).unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174004";
    let legacy_operation = json!({
        "phase": "localCommitted",
        "schema": "risunest.peer-bidirectional-operation/v1",
        "context": {
            "operationId": operation_id,
            "credential": {
                "endpoint": "http://192.168.1.2:32146",
                "sessionId": "123e4567-e89b-42d3-a456-426614174000",
                "manifestId": "b".repeat(64),
                "deviceId": "123e4567-e89b-42d3-a456-426614174001",
                "sourceDeviceId": "123e4567-e89b-42d3-a456-426614174002",
                "bearer": "c".repeat(64),
            },
            "libraryId": "risunest-product",
            "expectedLocalRevision": 7,
            "expectedRemoteRevision": 7,
            "expectedRemoteGeneration": {
                "generationId": "remote-r",
                "manifestHash": "d".repeat(64),
                "generationSequence": "2",
            },
            "previousShared": {
                "generationId": "shared-c",
                "manifestHash": "a".repeat(64),
                "generationSequence": "1",
            },
            "previousLocal": {
                "generationId": "shared-c",
                "manifestHash": "a".repeat(64),
                "generationSequence": "1",
            },
            "durableJobId": "123e4567-e89b-42d3-a456-426614174003",
        },
        "committed_revision": 8,
        "shared_generation": {
            "generationId": "local-a",
            "manifestHash": "e".repeat(64),
            "generationSequence": "3",
        },
        "changed": true,
        "transferred_objects": 2,
        "transferred_bytes": 19,
        "backups": [],
    });
    fs::write(
        operation_root.join(OPERATION_FILE),
        serde_json::to_vec(&legacy_operation).unwrap(),
    )
    .unwrap();

    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .unwrap(),
        PeerBidirectionalDurableOperation::LocalCommitted {
            remote_backup_required: true,
            ..
        }
    ));
}

#[test]
fn zero_transfer_remote_revision_advance_completes_as_updated() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let shared = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    let peer_id = "123e4567-e89b-42d3-a456-426614174025";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                shared.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174026";
    let mut retained_context = context(operation_id);
    retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    retained_context.credential.source_device_id = peer_id.to_owned();
    retained_context.expected_remote_revision = 4;
    retained_context.previous_shared = shared.clone();
    retained_context.previous_local = shared.clone();
    let retained = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: retained_context,
        committed_revision: 0,
        shared_generation: shared.clone(),
        changed: false,
        remote_backup_required: false,
        remote_apply_receipt: None,
        transferred_objects: 0,
        transferred_bytes: 0,
        backups: vec![],
    };
    let journal = PeerBidirectionalOperationJournal::new(directory.path());

    journal.store(&retained).unwrap();
    let unchanged = complete_bidirectional_local_after_remote_apply(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        LanBidirectionalRemoteApplyReceipt {
            committed_revision: 4,
            committed_generation: LanBidirectionalGeneration {
                generation_id: shared.generation_id.clone(),
                manifest_hash: shared.manifest_hash.clone(),
                generation_sequence: shared.generation_sequence.clone(),
            },
            transferred_objects: 0,
            transferred_bytes: 0,
            backup: None,
        },
    )
    .unwrap();
    assert_eq!(unchanged.kind, "noChanges");

    journal.store(&retained).unwrap();
    let record_only = complete_bidirectional_local_after_remote_apply(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        LanBidirectionalRemoteApplyReceipt {
            committed_revision: 5,
            committed_generation: LanBidirectionalGeneration {
                generation_id: shared.generation_id.clone(),
                manifest_hash: shared.manifest_hash.clone(),
                generation_sequence: shared.generation_sequence.clone(),
            },
            transferred_objects: 0,
            transferred_bytes: 0,
            backup: None,
        },
    )
    .unwrap();
    assert_eq!(record_only.kind, "updated");
    assert_eq!(record_only.transferred_objects, 0);
    assert_eq!(record_only.transferred_bytes, 0);

    journal.store(&retained).unwrap();
    assert!(matches!(
        complete_bidirectional_local_after_remote_apply(
            &mut store,
            &cas,
            directory.path(),
            operation_id,
            LanBidirectionalRemoteApplyReceipt {
                committed_revision: 6,
                committed_generation: LanBidirectionalGeneration {
                    generation_id: shared.generation_id,
                    manifest_hash: shared.manifest_hash,
                    generation_sequence: shared.generation_sequence,
                },
                transferred_objects: 0,
                transferred_bytes: 0,
                backup: None,
            },
        ),
        Err(PeerSyncError::Validation(_))
    ));
    assert_eq!(journal.load().unwrap(), Some(retained));
}

#[test]
fn source_exact_common_base_no_op_reopens_with_the_same_revision() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let target_device_id = "123e4567-e89b-42d3-a456-426614174027";
    let source_device_id = "123e4567-e89b-42d3-a456-426614174028";
    let operation_id = "123e4567-e89b-42d3-a456-426614174029";
    let previous = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    establish_logical_common_base(
        &mut store,
        &cas,
        target_device_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                target_device_id,
                previous.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    let shared_generation = LanBidirectionalGeneration {
        generation_id: previous.generation_id.clone(),
        manifest_hash: previous.manifest_hash.clone(),
        generation_sequence: previous.generation_sequence.clone(),
    };
    let mut source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };

    let receipt = apply_bidirectional_remote_shared_inner(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        target_device_id,
        0,
        &previous.manifest_hash,
        &previous,
        shared_generation.clone(),
        &base.manifest_bytes,
        &mut source,
        false,
        Some(source_device_id),
        None,
        None,
        false,
        &NeverCancelled,
    )
    .unwrap()
    .into_receipt();

    assert_eq!(receipt.committed_revision, 0);
    assert_eq!(receipt.transferred_objects, 0);
    assert_eq!(receipt.transferred_bytes, 0);
    assert_eq!(source.reads, 0);
    assert_eq!(store.revision().unwrap(), 0);
    drop(store);
    let start_retry_host = || {
        let source = LogicalDeltaSourceSession::open(
            PersistentStore::open(directory.path()).unwrap(),
            directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
        )
        .unwrap();
        let prepared = super::super::lan::PreparedLogicalLanSession::new(
            &uuid::Uuid::new_v4().to_string(),
            target_device_id,
            base.manifest_hash.clone(),
            base.manifest_bytes.clone(),
            source.objects().to_vec(),
            Box::new(source),
        )
        .unwrap();
        let mut host = LanCloneHost::prepare_logical(prepared);
        let pairing = host.start().unwrap();
        (host, pairing)
    };
    let session = LanBidirectionalSession {
        session_id: "123e4567-e89b-42d3-a456-426614174031".to_owned(),
        source_device_id: source_device_id.to_owned(),
        target_device_id: target_device_id.to_owned(),
    };
    let mut request = LanBidirectionalRemoteApplyRequest {
        operation_id: operation_id.to_owned(),
        source_endpoint: String::new(),
        source_session_id: String::new(),
        source_manifest_id: String::new(),
        source_claim: String::new(),
        expected_source_revision: 0,
        expected_source_generation: LanBidirectionalGeneration {
            generation_id: previous.generation_id.clone(),
            manifest_hash: previous.manifest_hash.clone(),
            generation_sequence: previous.generation_sequence.clone(),
        },
        expected_common_base_manifest_hash: previous.manifest_hash.clone(),
        backup_losing_side: false,
        completion_deferred_v1: None,
    };
    let control = ProductionLanBidirectionalControl::new(
        directory.path().to_path_buf(),
        PersistentStore::open(directory.path()).unwrap(),
        &session.session_id,
        source_device_id,
    );
    let prepared = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let retained_job_id = match prepared {
        PeerBidirectionalDurableOperation::SourcePrepared { durable_job_id, .. } => durable_job_id,
        other => panic!("expected retained source preparation, got {other:?}"),
    };
    assert_eq!(
        DurableCasJob::open(directory.path(), &retained_job_id)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );

    let (mut host, pairing) = start_retry_host();
    request.source_endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    request.source_session_id = pairing.session_id;
    request.source_manifest_id = pairing.manifest_id;
    request.source_claim = pairing.claim;
    SOURCE_AFTER_PREPARED_CALLBACK_FAILPOINT.with(|enabled| enabled.set(true));
    let retry_error = control
        .remote_apply(session.clone(), request.clone(), &NeverCancelled)
        .unwrap_err();
    assert!(
        matches!(&retry_error, PeerSyncError::Storage(message) if message.contains("pre-activation")),
        "{retry_error:?}"
    );
    assert!(!DurableCasJob::open(directory.path(), &retained_job_id)
        .unwrap()
        .is_sealed());
    host.stop().unwrap();
    let mut released = DurableCasJob::open(directory.path(), &retained_job_id).unwrap();
    released
        .leave_release_record_for_cleanup_retry(CasReleaseOutcome::Aborted)
        .unwrap();
    drop(released);
    assert!(DurableCasJob::open(directory.path(), &retained_job_id)
        .unwrap()
        .is_released());

    let (mut host, pairing) = start_retry_host();
    request.source_endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    request.source_session_id = pairing.session_id;
    request.source_manifest_id = pairing.manifest_id;
    request.source_claim = pairing.claim;

    assert_eq!(
        control
            .remote_apply(session.clone(), request.clone(), &NeverCancelled)
            .unwrap(),
        receipt.clone()
    );
    host.stop().unwrap();
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::Completed {
            remote_apply_receipt: Some(replayed),
            result: PeerBidirectionalCompletedResult {
                revision: 0,
                ref kind,
                ..
            },
            ..
        }) if replayed == receipt && kind == "noChanges"
    ));
    assert_eq!(
        control
            .remote_apply(session.clone(), request.clone(), &NeverCancelled)
            .unwrap(),
        receipt
    );
    let mut mismatched_backup = request.clone();
    mismatched_backup.backup_losing_side = true;
    assert!(matches!(
        control.remote_apply(session.clone(), mismatched_backup, &NeverCancelled),
        Err(PeerSyncError::Validation(_))
    ));

    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    let mut mismatched = journal.load().unwrap().unwrap();
    if let PeerBidirectionalDurableOperation::Completed {
        remote_apply_receipt: Some(receipt),
        result,
        ..
    } = &mut mismatched
    {
        receipt.committed_revision = 1;
        result.revision = 1;
        result.kind = "updated".to_owned();
    } else {
        panic!("expected source completion");
    }
    journal.store(&mismatched).unwrap();
    assert!(matches!(
        control.remote_apply(session, request, &NeverCancelled),
        Err(PeerSyncError::Storage(_) | PeerSyncError::Validation(_))
    ));
}

#[test]
fn source_content_identical_no_op_recovers_original_receipt_after_descendant_edit() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let target_device_id = "123e4567-e89b-42d3-a456-426614174033";
    let source_device_id = "123e4567-e89b-42d3-a456-426614174034";
    let operation_id = "123e4567-e89b-42d3-a456-426614174035";
    let previous = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    establish_logical_common_base(
        &mut store,
        &cas,
        target_device_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                target_device_id,
                previous.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    let mut shared_manifest = base.manifest.clone();
    shared_manifest.generation = "123e4567-e89b-42d3-a456-426614174036".to_owned();
    shared_manifest.generation_sequence = "1".to_owned();
    shared_manifest.parent_generation = Some(base.manifest.generation.clone());
    let shared_manifest_bytes = encode_logical_manifest(&shared_manifest).unwrap();
    let shared_generation = LanBidirectionalGeneration {
        generation_id: shared_manifest.generation.clone(),
        manifest_hash: hash_logical_manifest(&shared_manifest).unwrap(),
        generation_sequence: shared_manifest.generation_sequence.clone(),
    };
    let mut source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    let receipt = apply_bidirectional_remote_shared_inner(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        target_device_id,
        0,
        &previous.manifest_hash,
        &previous,
        shared_generation,
        &shared_manifest_bytes,
        &mut source,
        false,
        Some(source_device_id),
        None,
        None,
        false,
        &NeverCancelled,
    )
    .unwrap()
    .into_receipt();
    assert_eq!(receipt.committed_revision, 0);
    assert_eq!(source.reads, 0);
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"preserved": "source-no-op-descendant"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    drop(store);
    let session = LanBidirectionalSession {
        session_id: "123e4567-e89b-42d3-a456-426614174037".to_owned(),
        source_device_id: source_device_id.to_owned(),
        target_device_id: target_device_id.to_owned(),
    };
    let request = LanBidirectionalRemoteApplyRequest {
        operation_id: operation_id.to_owned(),
        source_endpoint: "http://127.0.0.1:1".to_owned(),
        source_session_id: "123e4567-e89b-42d3-a456-426614174038".to_owned(),
        source_manifest_id: "manifest".to_owned(),
        source_claim: "claim".to_owned(),
        expected_source_revision: 0,
        expected_source_generation: LanBidirectionalGeneration {
            generation_id: previous.generation_id,
            manifest_hash: previous.manifest_hash.clone(),
            generation_sequence: previous.generation_sequence,
        },
        expected_common_base_manifest_hash: previous.manifest_hash,
        backup_losing_side: false,
        completion_deferred_v1: None,
    };
    let control = ProductionLanBidirectionalControl::new(
        directory.path().to_path_buf(),
        PersistentStore::open(directory.path()).unwrap(),
        &session.session_id,
        source_device_id,
    );

    assert_eq!(
        control
            .remote_apply(session, request, &NeverCancelled)
            .unwrap(),
        receipt.clone()
    );
    drop(control);
    let inspector = PersistentStore::open(directory.path()).unwrap();
    assert_eq!(inspector.revision().unwrap(), 1);
    assert_eq!(
        inspector.read_root(None).unwrap().value["preserved"],
        "source-no-op-descendant"
    );
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::Completed {
            remote_apply_receipt: Some(replayed),
            result: PeerBidirectionalCompletedResult { revision: 1, .. },
            ..
        }) if replayed == receipt && replayed.committed_revision == 0
    ));
}

fn assert_invalid_retained_sealed_source_job(
    kind: CasJobKind,
    pin_shared_manifest: bool,
    expected_message: &str,
) {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let target_device_id = "123e4567-e89b-42d3-a456-426614174039";
    let source_device_id = "123e4567-e89b-42d3-a456-426614174040";
    let operation_id = "123e4567-e89b-42d3-a456-426614174041";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174042";
    let previous = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    establish_logical_common_base(
        &mut store,
        &cas,
        target_device_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                target_device_id,
                previous.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    let shared_generation = LanBidirectionalGeneration {
        generation_id: previous.generation_id.clone(),
        manifest_hash: previous.manifest_hash.clone(),
        generation_sequence: previous.generation_sequence.clone(),
    };
    let evidence = SourcePreparedEvidence {
        operation_id: operation_id.to_owned(),
        source_device_id: source_device_id.to_owned(),
        target_device_id: target_device_id.to_owned(),
        expected_source_revision: 0,
        previous_shared: previous.clone(),
        expected_source_generation: previous.clone(),
        shared_generation: shared_generation.clone(),
        incoming_revision: 0,
        transferred_objects: 0,
        transferred_bytes: 0,
        backup_required: false,
        backup: None,
        completion_deferred_v1: false,
    };
    let mut job = DurableCasJob::begin(directory.path(), durable_job_id, kind, 0).unwrap();
    if pin_shared_manifest {
        let size = cas
            .stat_object(&base.manifest_hash)
            .unwrap()
            .expect("sealed active manifest must exist in the CAS");
        job.pin_existing(&cas, &base.manifest_hash, size, CasObjectRole::DirectObject)
            .unwrap();
    }
    job.seal(&mut store, 0).unwrap();
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&evidence.durable_operation(durable_job_id.to_owned()))
        .unwrap();
    let mut source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };

    let error = apply_bidirectional_remote_shared_inner(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        target_device_id,
        0,
        &previous.manifest_hash,
        &previous,
        shared_generation,
        &base.manifest_bytes,
        &mut source,
        false,
        Some(source_device_id),
        Some(&evidence),
        Some(durable_job_id),
        false,
        &NeverCancelled,
    )
    .unwrap_err();

    assert!(
        matches!(&error, PeerSyncError::Storage(message) if message == expected_message),
        "{error:?}"
    );
    assert_eq!(store.revision().unwrap(), 0);
    assert!(DurableCasJob::open(directory.path(), durable_job_id)
        .unwrap()
        .is_sealed());
}

#[test]
fn retained_sealed_source_job_rejects_an_unexpected_kind() {
    assert_invalid_retained_sealed_source_job(
        CasJobKind::DirectAssetOrInlayWrite,
        true,
        "retained source job has an unexpected kind",
    );
}

#[test]
fn retained_sealed_source_job_requires_the_shared_manifest_root() {
    assert_invalid_retained_sealed_source_job(
        CasJobKind::LogicalDeltaTarget,
        false,
        "retained source job does not own the shared manifest",
    );
}

#[test]
fn source_unavailable_preserves_local_committed_operation() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let shared = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    let peer_id = "123e4567-e89b-42d3-a456-426614174012";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                shared.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174013";
    let mut retained_context = context(operation_id);
    retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    retained_context.credential.source_device_id = peer_id.to_owned();
    retained_context.previous_shared = shared.clone();
    retained_context.previous_local = shared.clone();
    let retained = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: retained_context,
        committed_revision: 0,
        shared_generation: shared.clone(),
        changed: false,
        remote_backup_required: false,
        remote_apply_receipt: None,
        transferred_objects: 0,
        transferred_bytes: 0,
        backups: vec![],
    };
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&retained).unwrap();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"preserved": "offline-descendant"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    drop(store);
    let mut reopened = PersistentStore::open(directory.path()).unwrap();

    let outcome = resume_bidirectional_local_committed_with_remote(
        &mut reopened,
        &cas,
        directory.path(),
        operation_id,
        |_context, _revision, _shared, _manifest, _backup_required| {
            Err(PeerSyncError::Transport(
                "peer source is offline".to_owned(),
            ))
        },
    )
    .unwrap();

    assert_eq!(
        outcome,
        ResumeLocalCommittedOutcome::SourceUnavailable {
            operation_id: operation_id.to_owned(),
            committed_revision: 0,
        }
    );
    assert_eq!(journal.load().unwrap(), Some(retained.clone()));
    assert_eq!(
        reopened
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        crate::persistent_store::SyncDeviceAckState {
            shared_identity: shared.clone(),
            local_identity: shared,
        }
    );

    let outcome = resume_bidirectional_local_committed_with_remote(
        &mut reopened,
        &cas,
        directory.path(),
        operation_id,
        |_context, _revision, _shared, _manifest, _backup_required| {
            Err(PeerSyncError::ActivationConflict {
                expected: Some("before".to_owned()),
                actual: Some("after".to_owned()),
            })
        },
    )
    .unwrap();
    assert_eq!(
        outcome,
        ResumeLocalCommittedOutcome::Stale {
            operation_id: operation_id.to_owned(),
            reason: PeerBidirectionalStaleReason::RemoteGeneration,
        }
    );
    assert!(journal.load().unwrap().is_none());

    let mut v1_retained = retained;
    let PeerBidirectionalDurableOperation::LocalCommitted { context, .. } = &mut v1_retained else {
        unreachable!();
    };
    context.completion_mode = PeerBidirectionalCompletionMode::V1;
    journal.store(&v1_retained).unwrap();
    let error = resume_bidirectional_local_committed_with_remote(
        &mut reopened,
        &cas,
        directory.path(),
        operation_id,
        |_context, _revision, _shared, _manifest, _backup_required| {
            Err(PeerSyncError::ActivationConflict {
                expected: None,
                actual: None,
            })
        },
    )
    .unwrap_err();
    assert!(matches!(error, PeerSyncError::ActivationConflict { .. }));
    assert_eq!(journal.load().unwrap(), Some(v1_retained));
}

#[test]
fn resume_rejects_required_missing_remote_backup_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let shared = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    let peer_id = "123e4567-e89b-42d3-a456-426614174032";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                shared.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174031";
    let mut retained_context = context(operation_id);
    retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    retained_context.credential.source_device_id = peer_id.to_owned();
    retained_context.expected_local_revision = 0;
    retained_context.expected_remote_revision = 4;
    retained_context.previous_shared = shared.clone();
    retained_context.previous_local = shared.clone();
    let retained = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: retained_context,
        committed_revision: 0,
        shared_generation: shared.clone(),
        changed: true,
        remote_backup_required: true,
        remote_apply_receipt: None,
        transferred_objects: 0,
        transferred_bytes: 0,
        backups: vec![],
    };
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&retained).unwrap();
    let before_root = store.read_root(None).unwrap();
    let before_ack = store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
        .unwrap();
    drop(store);
    let mut reopened = PersistentStore::open(directory.path()).unwrap();

    let error = resume_bidirectional_local_committed_with_remote(
        &mut reopened,
        &cas,
        directory.path(),
        operation_id,
        |_context, _revision, remote_shared, _manifest, backup_required| {
            assert!(backup_required);
            Ok(LanBidirectionalRemoteApplyReceipt {
                committed_revision: 4,
                committed_generation: LanBidirectionalGeneration {
                    generation_id: remote_shared.generation_id.clone(),
                    manifest_hash: remote_shared.manifest_hash.clone(),
                    generation_sequence: remote_shared.generation_sequence.clone(),
                },
                transferred_objects: 0,
                transferred_bytes: 0,
                backup: None,
            })
        },
    )
    .unwrap_err();

    assert!(matches!(error, PeerSyncError::Validation(_)));
    assert_eq!(journal.load().unwrap(), Some(retained));
    assert_eq!(reopened.revision().unwrap(), 0);
    assert_eq!(reopened.read_root(None).unwrap(), before_root);
    assert_eq!(
        reopened
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        before_ack
    );
}

#[test]
fn resume_contacts_the_source_before_abandoning_stale_local_peer_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let shared = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    let peer_id = "123e4567-e89b-42d3-a456-426614174022";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                shared.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    let journal = PeerBidirectionalOperationJournal::new(directory.path());

    let common_base_operation_id = "123e4567-e89b-42d3-a456-426614174023";
    let mut common_base_context = context(common_base_operation_id);
    common_base_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    common_base_context.credential.source_device_id = peer_id.to_owned();
    common_base_context.previous_shared = generation("stale-base", "0", 'a');
    common_base_context.previous_local = shared.clone();
    journal
        .store(&PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: common_base_context,
            committed_revision: 0,
            shared_generation: shared.clone(),
            changed: false,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        })
        .unwrap();
    assert_eq!(
        resume_bidirectional_local_committed_with_remote(
            &mut store,
            &cas,
            directory.path(),
            common_base_operation_id,
            |_context, _revision, _shared, _manifest, _backup_required| {
                Err(PeerSyncError::ActivationConflict {
                    expected: Some("retained-source".to_owned()),
                    actual: Some("different-source".to_owned()),
                })
            },
        )
        .unwrap(),
        ResumeLocalCommittedOutcome::Stale {
            operation_id: common_base_operation_id.to_owned(),
            reason: PeerBidirectionalStaleReason::RemoteGeneration,
        }
    );

    let device_ack_operation_id = "123e4567-e89b-42d3-a456-426614174024";
    assert_eq!(
        store
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        Some(shared.clone())
    );
    store
        .revoke_sync_device(PRODUCT_LOGICAL_LIBRARY_ID, peer_id, &shared)
        .unwrap();
    let mut device_ack_context = context(device_ack_operation_id);
    device_ack_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    device_ack_context.credential.source_device_id = peer_id.to_owned();
    device_ack_context.previous_shared = shared.clone();
    device_ack_context.previous_local = shared.clone();
    journal
        .store(&PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: device_ack_context,
            committed_revision: 0,
            shared_generation: shared.clone(),
            changed: false,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        })
        .unwrap();
    assert_eq!(
        resume_bidirectional_local_committed_with_remote(
            &mut store,
            &cas,
            directory.path(),
            device_ack_operation_id,
            |_context, _revision, _shared, _manifest, _backup_required| {
                Err(PeerSyncError::ActivationConflict {
                    expected: Some("retained-source".to_owned()),
                    actual: Some("different-source".to_owned()),
                })
            },
        )
        .unwrap(),
        ResumeLocalCommittedOutcome::Stale {
            operation_id: device_ack_operation_id.to_owned(),
            reason: PeerBidirectionalStaleReason::RemoteGeneration,
        }
    );
    assert!(journal.load().unwrap().is_none());
}

#[test]
fn remote_generation_rejection_after_local_drift_abandons_the_operation_and_job() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174014";
    let peer_id = "123e4567-e89b-42d3-a456-426614174002";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    let shared = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                shared.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"shared": "local-commit"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let committed = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let committed_shared = SyncGenerationIdentity {
        generation_id: committed.manifest.generation,
        manifest_hash: committed.manifest_hash,
        generation_sequence: committed.manifest.generation_sequence,
    };
    let mut retained_context = context(operation_id);
    retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    retained_context.credential.source_device_id = peer_id.to_owned();
    retained_context.previous_shared = shared.clone();
    retained_context.previous_local = shared.clone();
    let durable_job_id = retained_context.durable_job_id.clone();
    let job = DurableCasJob::begin(
        directory.path(),
        &durable_job_id,
        CasJobKind::LogicalDeltaTarget,
        0,
    )
    .unwrap();
    drop(job);
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context,
            committed_revision: 1,
            shared_generation: committed_shared.clone(),
            changed: false,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        })
        .unwrap();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({"advanced": true})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();

    let outcome = resume_bidirectional_local_committed_with_remote(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        |_context, _revision, _shared, _manifest, _backup_required| {
            Err(PeerSyncError::ActivationConflict {
                expected: Some("retained-source".to_owned()),
                actual: Some("different-source".to_owned()),
            })
        },
    )
    .unwrap();

    assert_eq!(
        outcome,
        ResumeLocalCommittedOutcome::Stale {
            operation_id: operation_id.to_owned(),
            reason: PeerBidirectionalStaleReason::RemoteGeneration,
        }
    );
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert_eq!(
        DurableCasJob::open(directory.path(), &durable_job_id)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn stale_caller_revision_returns_resume_required_when_local_commit_is_intact() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let active = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174021";
    let retained = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: context(operation_id),
        committed_revision: 0,
        shared_generation: SyncGenerationIdentity {
            generation_id: active.manifest.generation,
            manifest_hash: active.manifest_hash,
            generation_sequence: active.manifest.generation_sequence,
        },
        changed: false,
        remote_backup_required: false,
        remote_apply_receipt: None,
        transferred_objects: 0,
        transferred_bytes: 0,
        backups: vec![],
    };
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&retained).unwrap();

    assert_eq!(
        run_retained_remote_completion(&mut store, directory.path(), operation_id, 1, None,)
            .unwrap(),
        PeerBidirectionalSyncResult::ResumeRequired {
            operation_id: operation_id.to_owned(),
            phase: "localCommitted",
            committed_revision: 0,
        }
    );
    assert_eq!(journal.load().unwrap(), Some(retained));
}

#[test]
fn acknowledge_abandons_local_commit_and_allows_a_new_authenticated_target() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let peer_id = "123e4567-e89b-42d3-a456-426614174024";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    let common = SyncGenerationIdentity {
        generation_id: base.manifest.generation,
        manifest_hash: base.manifest_hash,
        generation_sequence: base.manifest.generation_sequence,
    };
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                common.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"committed": true})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174025";
    let mut retained_context = context(operation_id);
    retained_context.credential.source_device_id = peer_id.to_owned();
    retained_context.durable_job_id = "123e4567-e89b-42d3-a456-426614174026".to_owned();
    let durable_job_id = retained_context.durable_job_id.clone();
    drop(
        DurableCasJob::begin(
            directory.path(),
            &durable_job_id,
            CasJobKind::LogicalDeltaTarget,
            1,
        )
        .unwrap(),
    );
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context,
            committed_revision: 1,
            shared_generation: common.clone(),
            changed: true,
            remote_backup_required: true,
            remote_apply_receipt: None,
            transferred_objects: 1,
            transferred_bytes: 1,
            backups: vec![],
        })
        .unwrap();
    let committed_root = store.read_root(None).unwrap();

    let state = PeerBidirectionalCommandState::default();
    state
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();

    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(store.read_root(None).unwrap(), committed_root);
    assert_eq!(
        store
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap(),
        Some(common)
    );
    assert_eq!(
        DurableCasJob::open(directory.path(), &durable_job_id)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    attach_local_device_at_existing_base(&mut store, peer_id, 1).unwrap();
    assert!(state.begin_target().is_ok());
}

#[test]
fn explicit_v1_target_abandon_removes_a_source_unavailable_local_commit() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174096";
    let mut retained_context = context(operation_id);
    retained_context.completion_mode = PeerBidirectionalCompletionMode::V1;
    let retained = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: retained_context,
        committed_revision: 0,
        shared_generation: generation("shared-v1", "1", 'e'),
        changed: false,
        remote_backup_required: false,
        remote_apply_receipt: None,
        transferred_objects: 0,
        transferred_bytes: 0,
        backups: vec![],
    };
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&retained).unwrap();

    PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();
    assert!(journal.load().unwrap().is_none());
}

#[test]
fn v1_remote_receipt_without_a_delivery_retries_completion_instead_of_abandoning() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174096";
    let mut retained_context = context(operation_id);
    retained_context.completion_mode = PeerBidirectionalCompletionMode::V1;
    let retained = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: retained_context,
        committed_revision: 0,
        shared_generation: generation("shared-v1", "1", 'e'),
        changed: false,
        remote_backup_required: false,
        remote_apply_receipt: Some(LanBidirectionalRemoteApplyReceipt {
            committed_revision: 7,
            committed_generation: LanBidirectionalGeneration {
                generation_id: "shared-v1".to_owned(),
                manifest_hash: "e".repeat(64),
                generation_sequence: "1".to_owned(),
            },
            transferred_objects: 1,
            transferred_bytes: 13,
            backup: None,
        }),
        transferred_objects: 0,
        transferred_bytes: 0,
        backups: vec![],
    };
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&retained).unwrap();

    assert!(matches!(
        PeerBidirectionalCommandState::default().acknowledge(
            directory.path(),
            &mut PersistentStore::open(directory.path()).unwrap(),
            operation_id,
        ),
        Err(PeerSyncError::ActivationConflict { .. })
    ));
    assert_eq!(journal.load().unwrap(), Some(retained));
}

#[test]
fn explicit_v1_target_abandon_cleans_only_its_delivery_and_allows_source_removal() {
    let directory = tempfile::tempdir().unwrap();
    register_bidirectional_accounting_source(directory.path(), 11, 7);
    let other_source_id = "123e4567-e89b-42d3-a456-426614174099";
    let mut registry =
        super::super::device_registry::IncomingSourceRegistry::load(directory.path()).unwrap();
    registry
        .upsert(super::super::device_registry::IncomingSource {
            device_id: other_source_id.to_owned(),
            name: "other source".to_owned(),
            endpoint: "http://192.168.0.99:32146".to_owned(),
            bearer: "f".repeat(64),
            permissions: super::super::device_registry::DevicePermissions::read_and_bidirectional(),
            last_seen_ms: 7,
            total_bytes: 23,
        })
        .unwrap();
    registry.save().unwrap();

    let operation_id = "123e4567-e89b-42d3-a456-426614174097";
    let mut retained_context = context(operation_id);
    retained_context.completion_mode = PeerBidirectionalCompletionMode::V1;
    let delivery = super::super::device_registry::PendingCompletionDelivery {
        source_device_id: retained_context.credential.source_device_id.clone(),
        lane: "bidirectional".to_owned(),
        completion_lease_id: operation_id.to_owned(),
        manifest_id: retained_context.credential.manifest_id.clone(),
        useful_bytes: 31,
        receipt_id: super::super::device_registry::completion_receipt_id(
            "bidirectional",
            operation_id,
            &retained_context.credential.manifest_id,
        ),
    };
    retained_context.completion_delivery = Some(delivery.clone());
    let other_operation_id = "123e4567-e89b-42d3-a456-426614174098";
    let other_delivery = super::super::device_registry::PendingCompletionDelivery {
        source_device_id: other_source_id.to_owned(),
        lane: "bidirectional".to_owned(),
        completion_lease_id: other_operation_id.to_owned(),
        manifest_id: "e".repeat(64),
        useful_bytes: 47,
        receipt_id: super::super::device_registry::completion_receipt_id(
            "bidirectional",
            other_operation_id,
            &"e".repeat(64),
        ),
    };
    assert_eq!(
        super::super::device_registry::prepare_incoming_completion_delivery(
            directory.path(),
            delivery.clone(),
        )
        .unwrap(),
        super::super::device_registry::CompletionDeliveryPrepareStatus::Pending,
    );
    assert_eq!(
        super::super::device_registry::prepare_incoming_completion_delivery(
            directory.path(),
            other_delivery.clone(),
        )
        .unwrap(),
        super::super::device_registry::CompletionDeliveryPrepareStatus::Pending,
    );
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal
        .store(&PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context,
            committed_revision: 0,
            shared_generation: generation("shared-v1", "1", 'e'),
            changed: false,
            remote_backup_required: false,
            remote_apply_receipt: Some(LanBidirectionalRemoteApplyReceipt {
                committed_revision: 7,
                committed_generation: LanBidirectionalGeneration {
                    generation_id: "shared-v1".to_owned(),
                    manifest_hash: "e".repeat(64),
                    generation_sequence: "1".to_owned(),
                },
                transferred_objects: 1,
                transferred_bytes: 13,
                backup: None,
            }),
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        })
        .unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();

    PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();

    assert!(journal.load().unwrap().is_none());
    assert!(
        super::super::device_registry::snapshot_incoming_completion_delivery(
            directory.path(),
            BIDIRECTIONAL_ACCOUNTING_SOURCE_ID,
            super::super::device_registry::CompletionLane::Bidirectional,
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(
        super::super::device_registry::snapshot_incoming_completion_delivery(
            directory.path(),
            other_source_id,
            super::super::device_registry::CompletionLane::Bidirectional,
        )
        .unwrap()
        .unwrap()
        .delivery,
        other_delivery
    );
    assert_eq!(
        bidirectional_accounting_source(directory.path()).total_bytes,
        11
    );
    super::super::registry_commands::remove_incoming_source_if_inactive(
        directory.path(),
        BIDIRECTIONAL_ACCOUNTING_SOURCE_ID,
    )
    .unwrap();
    assert!(PeerBidirectionalCommandState::default()
        .begin_target()
        .is_ok());
}

#[test]
fn explicit_v1_target_abandon_releases_precommit_phases_for_source_removal() {
    for phase in ["targetPrepared", "awaitingConflict"] {
        let directory = tempfile::tempdir().unwrap();
        register_bidirectional_accounting_source(directory.path(), 11, 7);
        let operation_id = "123e4567-e89b-42d3-a456-426614174097";
        let mut retained_context = context(operation_id);
        retained_context.completion_mode = PeerBidirectionalCompletionMode::V1;
        retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
        retained_context.expected_remote_generation.manifest_hash =
            retained_context.credential.manifest_id.clone();
        drop(
            DurableCasJob::begin(
                directory.path(),
                &retained_context.durable_job_id,
                CasJobKind::LogicalDeltaTarget,
                0,
            )
            .unwrap(),
        );
        let operation = if phase == "targetPrepared" {
            PeerBidirectionalDurableOperation::TargetPrepared {
                schema: OPERATION_SCHEMA.to_owned(),
                context: retained_context.clone(),
                local_generation: generation("local", "1", 'e'),
                conflict_policy: TargetPreparedConflictPolicy::Reject,
                changed: false,
                remote_backup_required: false,
                transferred_objects: 0,
                transferred_bytes: 0,
                backups: vec![],
            }
        } else {
            PeerBidirectionalDurableOperation::AwaitingConflict {
                schema: OPERATION_SCHEMA.to_owned(),
                context: retained_context.clone(),
                conflicts: vec![PeerBidirectionalConflict {
                    key: "root".to_owned(),
                    conflict_type: "bothChanged".to_owned(),
                }],
                local_generation: generation("local", "1", 'e'),
                local_manifest_hash: "e".repeat(64),
                remote_manifest_hash: "d".repeat(64),
                backups: vec![],
            }
        };
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        journal.store(&operation).unwrap();
        assert!(
            super::super::registry_commands::remove_incoming_source_if_inactive(
                directory.path(),
                BIDIRECTIONAL_ACCOUNTING_SOURCE_ID,
            )
            .is_err()
        );

        PeerBidirectionalCommandState::default()
            .acknowledge(
                directory.path(),
                &mut PersistentStore::open(directory.path()).unwrap(),
                operation_id,
            )
            .unwrap();

        assert!(journal.load().unwrap().is_none());
        assert!(matches!(
            DurableCasJob::open(directory.path(), &retained_context.durable_job_id),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ));
        super::super::registry_commands::remove_incoming_source_if_inactive(
            directory.path(),
            BIDIRECTIONAL_ACCOUNTING_SOURCE_ID,
        )
        .unwrap();
        assert!(PeerBidirectionalCommandState::default()
            .begin_target()
            .is_ok());
    }
}

#[test]
fn deferred_already_active_is_retained_as_a_changed_local_commit() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let shared = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    let peer_id = "123e4567-e89b-42d3-a456-426614174002";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                shared.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174015";
    let mut retained_context = context(operation_id);
    retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    retained_context.expected_remote_revision = 4;
    retained_context.previous_shared = shared.clone();
    retained_context.previous_local = shared.clone();
    let job = RefCell::new(
        DurableCasJob::begin(
            directory.path(),
            &retained_context.durable_job_id,
            CasJobKind::LogicalDeltaTarget,
            0,
        )
        .unwrap(),
    );
    job.borrow_mut().seal(&mut store, 0).unwrap();

    assert_eq!(
        retain_bidirectional_local_activation(
            &mut store,
            &cas,
            directory.path(),
            retained_context,
            &job,
            0,
            Ok(LogicalDeltaActivation::AlreadyActive { revision: 0 }),
            true,
            false,
            0,
            0,
            vec![],
        )
        .unwrap(),
        LocalMergeOutcome::LocalCommitted
    );

    let retained = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&retained).unwrap()["changed"],
        serde_json::Value::Bool(true)
    );
    assert!(matches!(
        retained,
        PeerBidirectionalDurableOperation::LocalCommitted {
            committed_revision: 0,
            transferred_objects: 0,
            transferred_bytes: 0,
            ..
        }
    ));
    assert_eq!(
        DurableCasJob::open(directory.path(), "123e4567-e89b-42d3-a456-426614174003")
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );

    let result = complete_bidirectional_local_after_remote_apply(
        &mut store,
        &cas,
        directory.path(),
        operation_id,
        LanBidirectionalRemoteApplyReceipt {
            committed_revision: 4,
            committed_generation: LanBidirectionalGeneration {
                generation_id: shared.generation_id,
                manifest_hash: shared.manifest_hash,
                generation_sequence: shared.generation_sequence,
            },
            transferred_objects: 0,
            transferred_bytes: 0,
            backup: None,
        },
    )
    .unwrap();
    assert_eq!(result.kind, "updated");
    assert_eq!(result.transferred_objects, 0);
    assert_eq!(result.transferred_bytes, 0);
    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::Completed {
            schema: OPERATION_SCHEMA.to_owned(),
            remote_apply_receipt: None,
            source_binding: None,
            result,
        })
    );
}

#[test]
fn completed_acknowledgement_preserves_lossless_backups() {
    let directory = tempfile::tempdir().unwrap();
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    let operation_id = "123e4567-e89b-42d3-a456-426614174004";
    let backup_root = directory.path().join("peer-bidirectional/backups");
    fs::create_dir_all(&backup_root).unwrap();
    let local_path = backup_root.join(format!("{operation_id}-local.risulossless"));
    let remote_path = backup_root.join(format!("{operation_id}-remote.risulossless"));
    fs::write(&local_path, b"retained local backup").unwrap();
    fs::write(&remote_path, b"retained remote backup").unwrap();
    let backups = vec![
        PeerBidirectionalBackupReceipt {
            package_id: "a".repeat(64),
            side: PeerBidirectionalBackupSide::Local,
            path: local_path.to_string_lossy().into_owned(),
        },
        PeerBidirectionalBackupReceipt {
            package_id: "b".repeat(64),
            side: PeerBidirectionalBackupSide::Remote,
            path: remote_path.to_string_lossy().into_owned(),
        },
    ];
    let local_committed = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: context(operation_id),
        committed_revision: 7,
        shared_generation: generation("shared-a", "2", 'e'),
        changed: true,
        remote_backup_required: false,
        remote_apply_receipt: None,
        transferred_objects: 0,
        transferred_bytes: 0,
        backups: backups.clone(),
    };
    journal.store(&local_committed).unwrap();

    assert!(journal
        .acknowledge_completed("123e4567-e89b-42d3-a456-426614174099")
        .is_err());
    assert!(journal.acknowledge_completed(operation_id).is_err());
    assert_eq!(journal.load().unwrap(), Some(local_committed));

    let completed = PeerBidirectionalDurableOperation::Completed {
        schema: OPERATION_SCHEMA.to_owned(),
        remote_apply_receipt: None,
        source_binding: None,
        result: PeerBidirectionalCompletedResult {
            kind: "updated".to_owned(),
            operation_id: operation_id.to_owned(),
            revision: 7,
            remote_revision: 4,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups,
        },
    };
    journal.store(&completed).unwrap();
    let reopened = PeerBidirectionalOperationJournal::new(directory.path());
    assert_eq!(reopened.load().unwrap(), Some(completed.clone()));
    assert!(reopened
        .acknowledge_completed("123e4567-e89b-42d3-a456-426614174099")
        .is_err());
    assert_eq!(reopened.load().unwrap(), Some(completed));
    assert!(local_path.is_file());
    assert!(remote_path.is_file());

    reopened.acknowledge_completed(operation_id).unwrap();
    assert_eq!(reopened.load().unwrap(), None);
    assert_eq!(fs::read(local_path).unwrap(), b"retained local backup");
    assert_eq!(fs::read(remote_path).unwrap(), b"retained remote backup");
}

#[test]
fn target_prepared_status_and_acknowledgement_preserve_current_local_data() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let local = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174004";
    let mut retained_context = context(operation_id);
    retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    retained_context.expected_local_revision = 0;
    retained_context.credential.manifest_id = retained_context
        .expected_remote_generation
        .manifest_hash
        .clone();
    let mut job = DurableCasJob::begin(
        directory.path(),
        &retained_context.durable_job_id,
        CasJobKind::LogicalDeltaTarget,
        0,
    )
    .unwrap();
    let prepared = PeerBidirectionalDurableOperation::TargetPrepared {
        schema: OPERATION_SCHEMA.to_owned(),
        context: retained_context.clone(),
        local_generation: SyncGenerationIdentity {
            generation_id: local.manifest.generation,
            manifest_hash: local.manifest_hash,
            generation_sequence: local.manifest.generation_sequence,
        },
        conflict_policy: TargetPreparedConflictPolicy::Reject,
        changed: true,
        remote_backup_required: false,
        transferred_objects: 1,
        transferred_bytes: 17,
        backups: vec![],
    };
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&prepared)
        .unwrap();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"preserved": "after target activation"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    job.seal(&mut store, 0).unwrap();
    drop(job);

    let state = PeerBidirectionalCommandState::default();
    let active_target = state.begin_target().unwrap();
    let busy_status = state.status(directory.path(), &mut store).unwrap();
    assert!(matches!(
        busy_status.operation,
        Some(PeerBidirectionalStatusOperation::TargetPrepared {
            operation_id: retained_operation,
        }) if retained_operation == operation_id
    ));
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::TargetPrepared { .. })
    ));
    drop(active_target);

    let status = state.status(directory.path(), &mut store).unwrap();
    assert_eq!(
        serde_json::to_value(status.operation).unwrap(),
        json!({"phase": "targetPrepared", "operationId": operation_id})
    );
    PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();

    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(
        store.read_root(None).unwrap().value,
        json!({"preserved": "after target activation"})
    );
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert!(matches!(
        DurableCasJob::open(directory.path(), &retained_context.durable_job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn target_prepared_acknowledgement_accepts_a_missing_job_but_rejects_a_wrong_kind() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174004";
    let mut retained_context = context(operation_id);
    retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    retained_context.credential.manifest_id = retained_context
        .expected_remote_generation
        .manifest_hash
        .clone();
    let prepared = PeerBidirectionalDurableOperation::TargetPrepared {
        schema: OPERATION_SCHEMA.to_owned(),
        context: retained_context.clone(),
        local_generation: generation("local-l", "2", 'c'),
        conflict_policy: TargetPreparedConflictPolicy::Reject,
        changed: true,
        remote_backup_required: false,
        transferred_objects: 1,
        transferred_bytes: 17,
        backups: vec![],
    };
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&prepared).unwrap();

    PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();
    assert!(journal.load().unwrap().is_none());

    journal.store(&prepared).unwrap();
    let wrong_job = DurableCasJob::begin(
        directory.path(),
        &retained_context.durable_job_id,
        CasJobKind::PeerClone,
        0,
    )
    .unwrap();
    drop(wrong_job);
    assert!(matches!(
        PeerBidirectionalCommandState::default().acknowledge(
            directory.path(),
            &mut store,
            operation_id,
        ),
        Err(PeerSyncError::Storage(message)) if message.contains("unexpected identity")
    ));
    assert_eq!(journal.load().unwrap(), Some(prepared));
    assert_eq!(
        DurableCasJob::open(directory.path(), &retained_context.durable_job_id)
            .unwrap()
            .kind(),
        CasJobKind::PeerClone
    );
}

#[test]
fn context_bearing_durable_operations_reject_malformed_public_https_credentials() {
    let operation_id = "123e4567-e89b-42d3-a456-426614174004";
    let mut retained_context = context(operation_id);
    retained_context.credential.endpoint = "https://sync.example.com/path".to_owned();
    retained_context.credential.manifest_id = retained_context
        .expected_remote_generation
        .manifest_hash
        .clone();
    let operations = [
        PeerBidirectionalDurableOperation::TargetPrepared {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context.clone(),
            local_generation: generation("local-l", "2", 'c'),
            conflict_policy: TargetPreparedConflictPolicy::Reject,
            changed: true,
            remote_backup_required: false,
            transferred_objects: 2,
            transferred_bytes: 19,
            backups: vec![],
        },
        PeerBidirectionalDurableOperation::AwaitingConflict {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context.clone(),
            conflicts: vec![PeerBidirectionalConflict {
                key: "r1:root".to_owned(),
                conflict_type: "sameRecord".to_owned(),
            }],
            local_generation: generation("local-l", "2", 'c'),
            local_manifest_hash: "a".repeat(64),
            remote_manifest_hash: "b".repeat(64),
            backups: vec![],
        },
        PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context,
            committed_revision: 8,
            shared_generation: generation("local-a", "3", 'e'),
            changed: true,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 2,
            transferred_bytes: 19,
            backups: vec![],
        },
    ];

    for operation in operations {
        assert!(matches!(
            operation.validate(),
            Err(PeerSyncError::Storage(message)) if message.contains("desktop endpoint")
        ));
    }
}

#[test]
fn durable_operations_project_the_exact_frontend_contract() {
    let operation_id = "123e4567-e89b-42d3-a456-426614174004";
    let target_prepared = PeerBidirectionalDurableOperation::TargetPrepared {
        schema: OPERATION_SCHEMA.to_owned(),
        context: context(operation_id),
        local_generation: generation("local-l", "2", 'c'),
        conflict_policy: TargetPreparedConflictPolicy::Reject,
        changed: true,
        remote_backup_required: false,
        transferred_objects: 2,
        transferred_bytes: 19,
        backups: vec![],
    };
    assert_eq!(
        serde_json::to_value(target_prepared.status_projection()).unwrap(),
        serde_json::json!({
            "phase": "targetPrepared",
            "operationId": operation_id,
        })
    );

    let conflict = PeerBidirectionalDurableOperation::AwaitingConflict {
        schema: OPERATION_SCHEMA.to_owned(),
        context: context(operation_id),
        conflicts: vec![PeerBidirectionalConflict {
            key: "r1:root".to_owned(),
            conflict_type: "sameRecord".to_owned(),
        }],
        local_generation: generation("local-l", "2", 'c'),
        local_manifest_hash: "a".repeat(64),
        remote_manifest_hash: "b".repeat(64),
        backups: vec![],
    };
    assert_eq!(
        serde_json::to_value(conflict.status_projection()).unwrap(),
        serde_json::json!({
            "phase": "awaitingConflict",
            "result": {
                "kind": "conflict",
                "operationId": operation_id,
                "conflicts": [{"key": "r1:root", "type": "sameRecord"}],
                "localManifestHash": "a".repeat(64),
                "remoteManifestHash": "b".repeat(64),
            }
        })
    );

    let local_committed = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: context(operation_id),
        committed_revision: 8,
        shared_generation: generation("local-a", "3", 'e'),
        changed: true,
        remote_backup_required: false,
        remote_apply_receipt: None,
        transferred_objects: 2,
        transferred_bytes: 19,
        backups: vec![backup(PeerBidirectionalBackupSide::Local)],
    };
    assert_eq!(
        serde_json::to_value(local_committed.status_projection()).unwrap(),
        serde_json::json!({
            "phase": "localCommitted",
            "operationId": operation_id,
            "committedRevision": 8,
        })
    );

    let completed = PeerBidirectionalDurableOperation::Completed {
        schema: OPERATION_SCHEMA.to_owned(),
        remote_apply_receipt: None,
        source_binding: None,
        result: PeerBidirectionalCompletedResult {
            kind: "updated".to_owned(),
            operation_id: operation_id.to_owned(),
            revision: 8,
            remote_revision: 9,
            transferred_objects: 3,
            transferred_bytes: 27,
            backups: vec![backup(PeerBidirectionalBackupSide::Remote)],
        },
    };
    assert_eq!(
        serde_json::to_value(completed.status_projection()).unwrap(),
        serde_json::json!({
            "phase": "completed",
            "result": {
                "kind": "updated",
                "operationId": operation_id,
                "revision": 8,
                "remoteRevision": 9,
                "transferredObjects": 3,
                "transferredBytes": 27,
                "backups": [{
                    "packageId": "f".repeat(64),
                    "side": "remote",
                    "path": "peer-bidirectional/backups/conflict.risulossless",
                }],
            }
        })
    );
}

#[test]
fn reject_conflict_is_durable_and_mutates_neither_side() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut store, &cas);
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    drop(store);
    let remote_directory = tempfile::tempdir().unwrap();
    copy_tree(directory.path(), remote_directory.path());
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let peer_id = "123e4567-e89b-42d3-a456-426614174002";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        1,
        &base.manifest_bytes,
    )
    .unwrap();
    let common = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                common.clone(),
                1,
            )
            .unwrap(),
            1,
        )
        .unwrap();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(lossless_root("Local")),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let remote_cas = PayloadCas::new(remote_directory.path()).unwrap();
    let mut remote_store = PersistentStore::open(remote_directory.path()).unwrap();
    remote_store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(lossless_root("Remote")),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let remote = remote_store
        .seal_or_initialize_active_logical_generation(&remote_cas)
        .unwrap();
    let credential = LanBidirectionalLogicalCredential {
        endpoint: "http://192.168.1.2:32146".to_owned(),
        session_id: "123e4567-e89b-42d3-a456-426614174000".to_owned(),
        manifest_id: remote.manifest_hash.clone(),
        device_id: "123e4567-e89b-42d3-a456-426614174001".to_owned(),
        source_device_id: peer_id.to_owned(),
        bearer: "c".repeat(64),
    };

    let mut source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    TARGET_CONFLICT_STORE_FAILPOINT.with(|enabled| enabled.set(true));
    let store_error = begin_bidirectional_local_merge(
        &mut store,
        &cas,
        directory.path(),
        credential.clone(),
        2,
        &remote.manifest_bytes,
        &mut source,
    )
    .unwrap_err();
    assert!(matches!(
        store_error,
        PeerSyncError::Storage(message) if message.contains("awaiting-conflict")
    ));
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(store.read_root(None).unwrap().value, lossless_root("Local"));
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert!(durable_job_journal_ids(directory.path()).is_empty());
    let conflict = match begin_bidirectional_local_merge(
        &mut store,
        &cas,
        directory.path(),
        credential,
        2,
        &remote.manifest_bytes,
        &mut source,
    )
    .unwrap()
    {
        LocalMergeOutcome::Conflict(conflict) => conflict,
        LocalMergeOutcome::LocalCommitted => panic!("conflict unexpectedly committed"),
    };

    assert_eq!(conflict.operation_id.len(), 36);
    assert_eq!(
        conflict.conflicts,
        vec![PeerBidirectionalConflictStatus {
            key: "r1:root".to_owned(),
            conflict_type: "sameRecord".to_owned(),
        }]
    );
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(store.read_root(None).unwrap().value, lossless_root("Local"));
    assert_eq!(
        store
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap()
            .shared_identity,
        common
    );
    let retained = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let (durable_job_id, local_generation) = match retained {
        PeerBidirectionalDurableOperation::AwaitingConflict {
            context,
            conflicts,
            local_generation,
            ..
        } => {
            assert_eq!(context.operation_id, conflict.operation_id);
            assert_eq!(conflicts.len(), 1);
            (context.durable_job_id, local_generation)
        }
        other => panic!("unexpected retained operation: {other:?}"),
    };
    let job = DurableCasJob::open(directory.path(), &durable_job_id).unwrap();
    assert!(!job.is_sealed());
    assert!(!job.is_released());
    drop(job);

    let backup_path = directory
        .path()
        .join("peer-bidirectional")
        .join("backups")
        .join(format!("{}-local.risulossless", conflict.operation_id));
    fs::create_dir_all(backup_path.parent().unwrap()).unwrap();
    let wrong_source_directory = tempfile::tempdir().unwrap();
    let wrong_source_cas = PayloadCas::new(wrong_source_directory.path()).unwrap();
    let mut wrong_source_store = PersistentStore::open(wrong_source_directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut wrong_source_store, &wrong_source_cas);
    wrong_source_store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(lossless_root("Wrong source at the same revision")),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let wrong_generation = wrong_source_store
        .seal_or_initialize_active_logical_generation(&wrong_source_cas)
        .unwrap();
    let wrong_source_identity = SyncGenerationIdentity {
        generation_id: wrong_generation.manifest.generation,
        manifest_hash: wrong_generation.manifest_hash,
        generation_sequence: wrong_generation.manifest.generation_sequence,
    };
    let wrong_source_staging = wrong_source_directory.path().join("backup-staging");
    fs::create_dir_all(&wrong_source_staging).unwrap();
    let wrong_source_backup = create_and_verify_peer_bidirectional_backup_v1_report(
        &backup_path,
        &wrong_source_staging,
        &wrong_source_cas,
        &mut wrong_source_store,
        2,
        &lossless_source_binding(
            &conflict.operation_id,
            PeerBidirectionalBackupSide::Local,
            &wrong_source_identity,
        ),
        &NeverCancelled,
    )
    .unwrap();
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    let mut retained_with_wrong_receipt = journal.load().unwrap().unwrap();
    match &mut retained_with_wrong_receipt {
        PeerBidirectionalDurableOperation::AwaitingConflict { backups, .. } => {
            backups.push(PeerBidirectionalBackupReceipt {
                package_id: wrong_source_backup.archive_sha256,
                side: PeerBidirectionalBackupSide::Local,
                path: backup_path.to_string_lossy().into_owned(),
            });
        }
        other => panic!("unexpected retained operation: {other:?}"),
    }
    journal.store(&retained_with_wrong_receipt).unwrap();
    let wrong_source_bytes = fs::read(&backup_path).unwrap();
    let mut wrong_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    assert!(resolve_bidirectional_conflict(
        &mut store,
        &cas,
        directory.path(),
        &conflict.operation_id,
        PeerBidirectionalConflictWinner::Remote,
        2,
        &remote.manifest_bytes,
        &mut wrong_source,
    )
    .is_err());
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(store.read_root(None).unwrap().value, lossless_root("Local"));
    assert_eq!(fs::read(&backup_path).unwrap(), wrong_source_bytes);
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .unwrap(),
        PeerBidirectionalDurableOperation::AwaitingConflict { backups, .. }
            if backups.len() == 1
    ));

    fs::remove_file(&backup_path).unwrap();
    fs::write(&backup_path, b"corrupt backup").unwrap();
    let mut corrupt_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    assert!(resolve_bidirectional_conflict(
        &mut store,
        &cas,
        directory.path(),
        &conflict.operation_id,
        PeerBidirectionalConflictWinner::Remote,
        2,
        &remote.manifest_bytes,
        &mut corrupt_source,
    )
    .is_err());
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(store.read_root(None).unwrap().value, lossless_root("Local"));
    assert_eq!(fs::read(&backup_path).unwrap(), b"corrupt backup");
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .unwrap(),
        PeerBidirectionalDurableOperation::AwaitingConflict { backups, .. }
            if backups.len() == 1
    ));

    fs::remove_file(&backup_path).unwrap();
    let complete_staging = directory.path().join("complete-backup-staging");
    fs::create_dir_all(&complete_staging).unwrap();
    let completed_backup = create_and_verify_peer_bidirectional_backup_v1_report(
        &backup_path,
        &complete_staging,
        &cas,
        &mut store,
        2,
        &lossless_source_binding(
            &conflict.operation_id,
            PeerBidirectionalBackupSide::Local,
            &local_generation,
        ),
        &NeverCancelled,
    )
    .unwrap();
    let completed_receipt = PeerBidirectionalBackupReceipt {
        package_id: completed_backup.archive_sha256.clone(),
        side: PeerBidirectionalBackupSide::Local,
        path: backup_path.to_string_lossy().into_owned(),
    };
    let mut retained_with_receipt = journal.load().unwrap().unwrap();
    match &mut retained_with_receipt {
        PeerBidirectionalDurableOperation::AwaitingConflict { backups, .. } => {
            *backups = vec![completed_receipt.clone()];
        }
        other => panic!("unexpected retained operation: {other:?}"),
    }
    journal.store(&retained_with_receipt).unwrap();
    drop(store);
    let mut store = PersistentStore::open(directory.path()).unwrap();

    fs::remove_file(&backup_path).unwrap();
    let mut missing_backup_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    assert!(resolve_bidirectional_conflict(
        &mut store,
        &cas,
        directory.path(),
        &conflict.operation_id,
        PeerBidirectionalConflictWinner::Remote,
        2,
        &remote.manifest_bytes,
        &mut missing_backup_source,
    )
    .is_err());
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(store.read_root(None).unwrap().value, lossless_root("Local"));
    assert_eq!(journal.load().unwrap(), Some(retained_with_receipt.clone()));

    create_and_verify_peer_bidirectional_backup_v1_report(
        &backup_path,
        &complete_staging,
        &cas,
        &mut store,
        2,
        &lossless_source_binding(
            &conflict.operation_id,
            PeerBidirectionalBackupSide::Local,
            &local_generation,
        ),
        &NeverCancelled,
    )
    .unwrap();
    fs::write(&backup_path, b"corrupt persisted backup").unwrap();
    let mut corrupt_persisted_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    assert!(resolve_bidirectional_conflict(
        &mut store,
        &cas,
        directory.path(),
        &conflict.operation_id,
        PeerBidirectionalConflictWinner::Remote,
        2,
        &remote.manifest_bytes,
        &mut corrupt_persisted_source,
    )
    .is_err());
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(fs::read(&backup_path).unwrap(), b"corrupt persisted backup");
    assert_eq!(journal.load().unwrap(), Some(retained_with_receipt.clone()));

    fs::remove_file(&backup_path).unwrap();
    create_and_verify_peer_bidirectional_backup_v1_report(
        &backup_path,
        &complete_staging,
        &cas,
        &mut store,
        2,
        &lossless_source_binding(
            &conflict.operation_id,
            PeerBidirectionalBackupSide::Local,
            &local_generation,
        ),
        &NeverCancelled,
    )
    .unwrap();
    let mut wrong_hash_receipt = retained_with_receipt.clone();
    match &mut wrong_hash_receipt {
        PeerBidirectionalDurableOperation::AwaitingConflict { backups, .. } => {
            backups[0].package_id = "0".repeat(64);
        }
        other => panic!("unexpected retained operation: {other:?}"),
    }
    journal.store(&wrong_hash_receipt).unwrap();
    let mut wrong_hash_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    assert!(resolve_bidirectional_conflict(
        &mut store,
        &cas,
        directory.path(),
        &conflict.operation_id,
        PeerBidirectionalConflictWinner::Remote,
        2,
        &remote.manifest_bytes,
        &mut wrong_hash_source,
    )
    .is_err());
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(journal.load().unwrap(), Some(wrong_hash_receipt));
    journal.store(&retained_with_receipt).unwrap();

    let mut resolution_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    TARGET_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.set(true));
    let prepared_store_error = resolve_bidirectional_conflict(
        &mut store,
        &cas,
        directory.path(),
        &conflict.operation_id,
        PeerBidirectionalConflictWinner::Remote,
        2,
        &remote.manifest_bytes,
        &mut resolution_source,
    )
    .unwrap_err();
    assert!(matches!(
        prepared_store_error,
        PeerSyncError::Storage(message) if message.contains("target-prepared")
    ));
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(journal.load().unwrap(), Some(retained_with_receipt.clone()));
    let mut missing_resolution_source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    assert!(matches!(
        resolve_bidirectional_conflict(
            &mut store,
            &cas,
            directory.path(),
            &conflict.operation_id,
            PeerBidirectionalConflictWinner::Remote,
            2,
            &remote.manifest_bytes,
            &mut missing_resolution_source,
        ),
        Err(PeerSyncError::Transport(_))
    ));
    assert!(missing_resolution_source.reads > 0);
    assert_eq!(store.revision().unwrap(), 2);
    assert!(matches!(
        journal.load().unwrap(),
        Some(PeerBidirectionalDurableOperation::TargetPrepared {
            context,
            conflict_policy: TargetPreparedConflictPolicy::PreferRemote,
            ..
        }) if context.operation_id == conflict.operation_id
    ));

    drop(store);
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let target_prepared = journal.load().unwrap().unwrap();
    let target_job_id = match &target_prepared {
        PeerBidirectionalDurableOperation::TargetPrepared { context, .. } => {
            context.durable_job_id.clone()
        }
        other => panic!("expected retained target preparation, got {other:?}"),
    };
    let complete_backup_bytes = fs::read(&backup_path).unwrap();
    fs::remove_file(&backup_path).unwrap();
    let mut missing_backup_source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    assert!(resume_bidirectional_target_prepared(
        &mut store,
        &cas,
        directory.path(),
        &conflict.operation_id,
        2,
        None,
        &remote.manifest_bytes,
        &mut missing_backup_source,
    )
    .is_err());
    assert_eq!(missing_backup_source.reads, 0);
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(store.read_root(None).unwrap().value, lossless_root("Local"));
    assert_eq!(journal.load().unwrap(), Some(target_prepared.clone()));
    assert!(DurableCasJob::open(directory.path(), &target_job_id).is_ok());

    fs::write(&backup_path, b"corrupt target-prepared backup").unwrap();
    let mut corrupt_backup_source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    assert!(resume_bidirectional_target_prepared(
        &mut store,
        &cas,
        directory.path(),
        &conflict.operation_id,
        2,
        None,
        &remote.manifest_bytes,
        &mut corrupt_backup_source,
    )
    .is_err());
    assert_eq!(corrupt_backup_source.reads, 0);
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(journal.load().unwrap(), Some(target_prepared.clone()));
    assert!(DurableCasJob::open(directory.path(), &target_job_id).is_ok());

    fs::write(&backup_path, &complete_backup_bytes).unwrap();
    let mut wrong_path_prepared = target_prepared.clone();
    let wrong_path = directory
        .path()
        .join("peer-bidirectional")
        .join("backups")
        .join("wrong-local.risulossless");
    match &mut wrong_path_prepared {
        PeerBidirectionalDurableOperation::TargetPrepared { backups, .. } => {
            backups[0].path = wrong_path.to_string_lossy().into_owned();
        }
        other => panic!("expected retained target preparation, got {other:?}"),
    }
    journal.store(&wrong_path_prepared).unwrap();
    let mut wrong_path_source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    assert!(resume_bidirectional_target_prepared(
        &mut store,
        &cas,
        directory.path(),
        &conflict.operation_id,
        2,
        None,
        &remote.manifest_bytes,
        &mut wrong_path_source,
    )
    .is_err());
    assert_eq!(wrong_path_source.reads, 0);
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(journal.load().unwrap(), Some(wrong_path_prepared));
    assert!(DurableCasJob::open(directory.path(), &target_job_id).is_ok());
    journal.store(&target_prepared).unwrap();

    let mut resolution_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    TARGET_AFTER_ACTIVATION_FAILPOINT.with(|enabled| enabled.set(true));
    assert!(matches!(
        resume_bidirectional_target_prepared(
            &mut store,
            &cas,
            directory.path(),
            &conflict.operation_id,
            2,
            None,
            &remote.manifest_bytes,
            &mut resolution_source,
        ),
        Err(PeerSyncError::Storage(message)) if message.contains("after target activation")
    ));

    assert_eq!(store.revision().unwrap(), 3);
    assert_eq!(
        store.read_root(None).unwrap().value,
        lossless_root("Remote")
    );
    assert_eq!(journal.load().unwrap(), Some(target_prepared.clone()));
    let retained_job = DurableCasJob::open(directory.path(), &target_job_id).unwrap();
    assert!(retained_job.is_sealed());
    assert!(!retained_job.is_released());
    drop(retained_job);

    fs::write(&backup_path, b"corrupt post-activation backup").unwrap();
    assert!(PeerBidirectionalCommandState::default()
        .status(directory.path(), &mut store)
        .is_err());
    assert_eq!(journal.load().unwrap(), Some(target_prepared));
    let retained_job = DurableCasJob::open(directory.path(), &target_job_id).unwrap();
    assert!(retained_job.is_sealed());
    assert!(!retained_job.is_released());
    drop(retained_job);

    fs::write(&backup_path, complete_backup_bytes).unwrap();
    BACKUP_FULL_VERIFICATION_COUNT.with(|count| count.set(0));
    let status = PeerBidirectionalCommandState::default()
        .status(directory.path(), &mut store)
        .unwrap();
    assert!(matches!(
        status.operation,
        Some(PeerBidirectionalStatusOperation::LocalCommitted {
            operation_id,
            committed_revision: 3,
        }) if operation_id == conflict.operation_id
    ));
    assert_eq!(BACKUP_FULL_VERIFICATION_COUNT.with(Cell::get), 1);
    let retained = journal.load().unwrap().unwrap();
    match retained {
        PeerBidirectionalDurableOperation::LocalCommitted { backups, .. } => {
            assert_eq!(backups.len(), 1);
            assert_eq!(backups[0].side, PeerBidirectionalBackupSide::Local);
            assert_eq!(backups[0].package_id, completed_backup.archive_sha256);
            assert!(Path::new(&backups[0].path).is_file());
        }
        other => panic!("unexpected retained operation: {other:?}"),
    }
    assert!(matches!(
        DurableCasJob::open(directory.path(), &target_job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn local_winner_backs_up_remote_before_replacement_and_retains_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let local_root = directory.path().join("local");
    fs::create_dir(&local_root).unwrap();
    let local_cas = PayloadCas::new(&local_root).unwrap();
    let mut local_store = PersistentStore::open(&local_root).unwrap();
    seed_lossless_backup_fixture(&mut local_store, &local_cas);
    let base = local_store
        .seal_or_initialize_active_logical_generation(&local_cas)
        .unwrap();
    drop(local_store);
    let remote_root = directory.path().join("remote");
    copy_tree(&local_root, &remote_root);

    let remote_cas = PayloadCas::new(&remote_root).unwrap();
    let mut local_store = PersistentStore::open(&local_root).unwrap();
    let mut remote_store = PersistentStore::open(&remote_root).unwrap();
    let local_device = "123e4567-e89b-42d3-a456-426614174027";
    let remote_device = "123e4567-e89b-42d3-a456-426614174028";
    let common = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    for (store, cas, peer_id) in [
        (&mut local_store, &local_cas, remote_device),
        (&mut remote_store, &remote_cas, local_device),
    ] {
        establish_logical_common_base(
            store,
            cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            1,
            &base.manifest_bytes,
        )
        .unwrap();
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    common.clone(),
                    1,
                )
                .unwrap(),
                1,
            )
            .unwrap();
    }
    local_store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(lossless_root("Local")),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    remote_store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(lossless_root("Remote")),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let remote = remote_store
        .seal_or_initialize_active_logical_generation(&remote_cas)
        .unwrap();
    let credential = LanBidirectionalLogicalCredential {
        endpoint: "http://192.168.1.2:32146".to_owned(),
        session_id: "123e4567-e89b-42d3-a456-426614174029".to_owned(),
        manifest_id: remote.manifest_hash.clone(),
        device_id: local_device.to_owned(),
        source_device_id: remote_device.to_owned(),
        bearer: "f".repeat(64),
    };
    let mut remote_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(&remote_root).unwrap(),
        &remote_root,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    let conflict = match begin_bidirectional_local_merge(
        &mut local_store,
        &local_cas,
        &local_root,
        credential,
        2,
        &remote.manifest_bytes,
        &mut remote_source,
    )
    .unwrap()
    {
        LocalMergeOutcome::Conflict(conflict) => conflict,
        other => panic!("unexpected merge outcome: {other:?}"),
    };
    assert_eq!(local_store.revision().unwrap(), 2);
    assert_eq!(
        local_store.read_root(None).unwrap().value,
        lossless_root("Local")
    );

    let mut wrong_source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    assert!(resolve_bidirectional_conflict(
        &mut local_store,
        &local_cas,
        &local_root,
        "123e4567-e89b-42d3-a456-426614174030",
        PeerBidirectionalConflictWinner::Local,
        2,
        &remote.manifest_bytes,
        &mut wrong_source,
    )
    .is_err());
    assert_eq!(wrong_source.reads, 0);
    assert_eq!(local_store.revision().unwrap(), 2);
    assert_eq!(
        local_store.read_root(None).unwrap().value,
        lossless_root("Local")
    );

    drop(remote_source);
    drop(local_store);
    let mut local_store = PersistentStore::open(&local_root).unwrap();
    let mut resolution_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(&remote_root).unwrap(),
        &remote_root,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    TARGET_AFTER_ACTIVATION_FAILPOINT.with(|enabled| enabled.set(true));
    assert!(matches!(
        resolve_bidirectional_conflict(
            &mut local_store,
            &local_cas,
            &local_root,
            &conflict.operation_id,
            PeerBidirectionalConflictWinner::Local,
            2,
            &remote.manifest_bytes,
            &mut resolution_source,
        ),
        Err(PeerSyncError::Storage(message)) if message.contains("after target activation")
    ));
    assert_eq!(local_store.revision().unwrap(), 2);
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(&local_root)
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::TargetPrepared {
            context,
            conflict_policy: TargetPreparedConflictPolicy::PreferLocal,
            ..
        }) if context.operation_id == conflict.operation_id
    ));

    drop(resolution_source);
    drop(local_store);
    let mut local_store = PersistentStore::open(&local_root).unwrap();
    let mut resolution_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(&remote_root).unwrap(),
        &remote_root,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    assert_eq!(
        resume_bidirectional_target_prepared(
            &mut local_store,
            &local_cas,
            &local_root,
            &conflict.operation_id,
            2,
            None,
            &remote.manifest_bytes,
            &mut resolution_source,
        )
        .unwrap(),
        LocalMergeOutcome::LocalCommitted
    );
    assert_eq!(local_store.revision().unwrap(), 2);
    assert_eq!(
        local_store.read_root(None).unwrap().value,
        lossless_root("Local")
    );
    drop(local_store);
    let mut local_store = PersistentStore::open(&local_root).unwrap();
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(&local_root)
            .load()
            .unwrap()
            .unwrap(),
        PeerBidirectionalDurableOperation::LocalCommitted {
            remote_backup_required: true,
            ..
        }
    ));
    assert_eq!(
        resume_bidirectional_local_committed_with_remote(
            &mut local_store,
            &local_cas,
            &local_root,
            &conflict.operation_id,
            |_context, _revision, _shared, _manifest, remote_backup_required| {
                assert!(remote_backup_required);
                Err(PeerSyncError::Transport(
                    "response lost before remote backup receipt".to_owned(),
                ))
            },
        )
        .unwrap(),
        ResumeLocalCommittedOutcome::SourceUnavailable {
            operation_id: conflict.operation_id.clone(),
            committed_revision: 2,
        }
    );
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(&local_root)
            .load()
            .unwrap()
            .unwrap(),
        PeerBidirectionalDurableOperation::LocalCommitted {
            remote_backup_required: true,
            ..
        }
    ));
    let mut stale_source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    assert!(resolve_bidirectional_conflict(
        &mut local_store,
        &local_cas,
        &local_root,
        &conflict.operation_id,
        PeerBidirectionalConflictWinner::Local,
        2,
        &remote.manifest_bytes,
        &mut stale_source,
    )
    .is_err());
    assert_eq!(stale_source.reads, 0);
    assert_eq!(local_store.revision().unwrap(), 2);

    let shared = local_store
        .seal_or_initialize_active_logical_generation(&local_cas)
        .unwrap();
    let shared_identity = SyncGenerationIdentity {
        generation_id: shared.manifest.generation.clone(),
        manifest_hash: shared.manifest_hash.clone(),
        generation_sequence: shared.manifest.generation_sequence.clone(),
    };
    let remote_identity = SyncGenerationIdentity {
        generation_id: remote.manifest.generation.clone(),
        manifest_hash: remote.manifest_hash.clone(),
        generation_sequence: remote.manifest.generation_sequence.clone(),
    };
    let mut shared_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(&local_root).unwrap(),
        &local_root,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &shared.manifest.generation,
    )
    .unwrap();
    let first_receipt = apply_bidirectional_remote_shared(
        &mut remote_store,
        &remote_cas,
        &remote_root,
        &conflict.operation_id,
        local_device,
        2,
        &common.manifest_hash,
        &remote_identity,
        LanBidirectionalGeneration {
            generation_id: shared_identity.generation_id.clone(),
            manifest_hash: shared_identity.manifest_hash.clone(),
            generation_sequence: shared_identity.generation_sequence.clone(),
        },
        &shared.manifest_bytes,
        &mut shared_source,
        true,
    )
    .unwrap();
    let first_backup = first_receipt.backup.unwrap();
    assert_eq!(first_backup.package_id.len(), 64);
    assert!(Path::new(&first_backup.path).is_file());
    assert_eq!(remote_store.revision().unwrap(), 3);
    assert_eq!(
        remote_store.read_root(None).unwrap().value,
        lossless_root("Local")
    );

    drop(shared_source);
    drop(resolution_source);
    drop(remote_store);
    let mut remote_store = PersistentStore::open(&remote_root).unwrap();
    let mut retry_source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    let recovered_receipt = apply_bidirectional_remote_shared(
        &mut remote_store,
        &remote_cas,
        &remote_root,
        &conflict.operation_id,
        local_device,
        2,
        &common.manifest_hash,
        &remote_identity,
        LanBidirectionalGeneration {
            generation_id: shared_identity.generation_id.clone(),
            manifest_hash: shared_identity.manifest_hash.clone(),
            generation_sequence: shared_identity.generation_sequence.clone(),
        },
        &shared.manifest_bytes,
        &mut retry_source,
        true,
    )
    .unwrap();
    assert_eq!(retry_source.reads, 0);
    assert_eq!(recovered_receipt.transferred_objects, 0);
    assert_eq!(recovered_receipt.transferred_bytes, 0);
    assert_eq!(recovered_receipt.backup, Some(first_backup.clone()));

    let result = complete_bidirectional_local_after_remote_apply(
        &mut local_store,
        &local_cas,
        &local_root,
        &conflict.operation_id,
        recovered_receipt,
    )
    .unwrap();
    assert_eq!(result.kind, "updated");
    assert_eq!(result.backups.len(), 1);
    assert_eq!(result.backups[0].side, PeerBidirectionalBackupSide::Remote);
    assert_eq!(result.backups[0].package_id, first_backup.package_id);

    drop(local_store);
    let retained = PeerBidirectionalOperationJournal::new(&local_root)
        .load()
        .unwrap()
        .unwrap();
    match retained {
        PeerBidirectionalDurableOperation::Completed {
            result: retained, ..
        } => {
            assert_eq!(retained.backups, result.backups);
        }
        other => panic!("unexpected retained operation: {other:?}"),
    }
}

#[test]
fn target_prepared_store_failure_precedes_source_reads_and_local_mutation() {
    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut source = fixture_source(&remote);
    TARGET_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.set(true));

    let error = begin_bidirectional_local_merge(
        &mut store,
        &cas,
        directory.path(),
        credential,
        1,
        &remote.manifest_bytes,
        &mut source,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        PeerSyncError::Storage(message) if message.contains("target-prepared")
    ));
    assert_eq!(source.reads, 0);
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(
        store.read_root(None).unwrap().value,
        json!({"side": "local"})
    );
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert!(durable_job_journal_ids(directory.path()).is_empty());
}

#[test]
fn target_prepared_store_owns_the_deterministic_job_before_job_begin() {
    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut source = fixture_source(&remote);
    TARGET_AFTER_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.set(true));

    let error = begin_bidirectional_local_merge(
        &mut store,
        &cas,
        directory.path(),
        credential,
        1,
        &remote.manifest_bytes,
        &mut source,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        PeerSyncError::Storage(message) if message.contains("after target-prepared")
    ));
    assert_eq!(source.reads, 0);
    assert_eq!(store.revision().unwrap(), 1);
    let prepared = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let (operation_id, job_id) = match &prepared {
        PeerBidirectionalDurableOperation::TargetPrepared { context, .. } => {
            (context.operation_id.clone(), context.durable_job_id.clone())
        }
        other => panic!("expected target-prepared ownership, got {other:?}"),
    };
    assert_eq!(job_id, operation_id);
    assert!(durable_job_journal_ids(directory.path()).is_empty());

    drop(store);
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let mut retry_source = fixture_source(&remote);
    assert_eq!(
        resume_bidirectional_target_prepared(
            &mut store,
            &cas,
            directory.path(),
            &operation_id,
            1,
            None,
            &remote.manifest_bytes,
            &mut retry_source,
        )
        .unwrap(),
        LocalMergeOutcome::LocalCommitted
    );
    assert_eq!(store.revision().unwrap(), 2);
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::LocalCommitted { context, .. })
            if context.operation_id == operation_id && context.durable_job_id == job_id
    ));
    assert!(matches!(
        DurableCasJob::open(directory.path(), &job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn target_transfer_failure_reopens_with_the_same_operation_and_job() {
    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut missing_source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    assert!(matches!(
        begin_bidirectional_local_merge(
            &mut store,
            &cas,
            directory.path(),
            credential,
            1,
            &remote.manifest_bytes,
            &mut missing_source,
        ),
        Err(PeerSyncError::Transport(_))
    ));
    assert_eq!(missing_source.reads, 1);
    assert_eq!(store.revision().unwrap(), 1);
    let prepared = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let (operation_id, job_id, predicted_objects, predicted_bytes) = match &prepared {
        PeerBidirectionalDurableOperation::TargetPrepared {
            context,
            transferred_objects,
            transferred_bytes,
            ..
        } => (
            context.operation_id.clone(),
            context.durable_job_id.clone(),
            *transferred_objects,
            *transferred_bytes,
        ),
        other => panic!("expected target-prepared transfer failure, got {other:?}"),
    };
    assert!(predicted_objects > 0);
    assert!(predicted_bytes > 0);
    let retained_job = DurableCasJob::open(directory.path(), &job_id).unwrap();
    assert!(!retained_job.is_sealed());
    assert!(!retained_job.is_released());
    drop(retained_job);

    let mut retry_source = fixture_source(&remote);
    assert_eq!(
        resume_bidirectional_target_prepared(
            &mut store,
            &cas,
            directory.path(),
            &operation_id,
            1,
            None,
            &remote.manifest_bytes,
            &mut retry_source,
        )
        .unwrap(),
        LocalMergeOutcome::LocalCommitted
    );
    assert_eq!(retry_source.reads, predicted_objects as usize);
    assert_eq!(store.revision().unwrap(), 2);
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::LocalCommitted { context, .. })
            if context.operation_id == operation_id && context.durable_job_id == job_id
    ));
    assert!(matches!(
        DurableCasJob::open(directory.path(), &job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn target_partial_transfer_reopens_the_same_job_and_requests_only_remaining_objects() {
    let (directory, cas, mut store, remote, credential) =
        disjoint_target_fixture_with_plugin_records(100);
    let mut partial_source = FailingAfterFixtureSource {
        objects: fixture_source(&remote).objects,
        successful_reads: 0,
        opens: 0,
        fail_after: 40,
    };
    assert!(matches!(
        begin_bidirectional_local_merge(
            &mut store,
            &cas,
            directory.path(),
            credential,
            1,
            &remote.manifest_bytes,
            &mut partial_source,
        ),
        Err(PeerSyncError::Transport(_))
    ));
    assert_eq!(partial_source.successful_reads, 40);
    assert_eq!(partial_source.opens, 41);
    let prepared = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let (operation_id, job_id, predicted_objects, predicted_bytes) = match &prepared {
        PeerBidirectionalDurableOperation::TargetPrepared {
            context,
            transferred_objects,
            transferred_bytes,
            ..
        } => (
            context.operation_id.clone(),
            context.durable_job_id.clone(),
            *transferred_objects,
            *transferred_bytes,
        ),
        other => panic!("expected target-prepared partial transfer, got {other:?}"),
    };
    assert_eq!(predicted_objects, 100);
    let retained_job = DurableCasJob::open(directory.path(), &job_id).unwrap();
    assert!(!retained_job.is_sealed());
    assert_eq!(retained_job.pin_count(), 41);
    drop(retained_job);
    assert_no_logical_delta_staging_orphans(directory.path());

    drop(store);
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let mut retry_source = fixture_source(&remote);
    assert_eq!(
        resume_bidirectional_target_prepared(
            &mut store,
            &cas,
            directory.path(),
            &operation_id,
            1,
            None,
            &remote.manifest_bytes,
            &mut retry_source,
        )
        .unwrap(),
        LocalMergeOutcome::LocalCommitted
    );
    assert_eq!(retry_source.reads, 60);
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::LocalCommitted {
            context,
            transferred_objects: 100,
            transferred_bytes,
            ..
        }) if context.operation_id == operation_id
            && context.durable_job_id == job_id
            && transferred_bytes == predicted_bytes
    ));
}

#[test]
fn target_changed_activation_crash_recovers_without_a_second_download() {
    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut source = fixture_source(&remote);
    TARGET_AFTER_ACTIVATION_FAILPOINT.with(|enabled| enabled.set(true));
    let error = begin_bidirectional_local_merge(
        &mut store,
        &cas,
        directory.path(),
        credential.clone(),
        1,
        &remote.manifest_bytes,
        &mut source,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        PeerSyncError::Storage(message) if message.contains("after target activation")
    ));
    assert_eq!(store.revision().unwrap(), 2);
    let prepared = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let (operation_id, job_id) = match &prepared {
        PeerBidirectionalDurableOperation::TargetPrepared { context, .. } => {
            (context.operation_id.clone(), context.durable_job_id.clone())
        }
        other => panic!("expected target-prepared activation crash, got {other:?}"),
    };
    assert!(DurableCasJob::open(directory.path(), &job_id)
        .unwrap()
        .is_sealed());

    let mut rebound = credential;
    rebound.endpoint = "http://192.168.1.3:32146".to_owned();
    rebound.session_id = "123e4567-e89b-42d3-a456-426614174019".to_owned();
    rebound.bearer = "f".repeat(64);
    let mut retry_source = fixture_source(&remote);
    assert_eq!(
        resume_bidirectional_target_prepared(
            &mut store,
            &cas,
            directory.path(),
            &operation_id,
            2,
            Some(rebound.clone()),
            &remote.manifest_bytes,
            &mut retry_source,
        )
        .unwrap(),
        LocalMergeOutcome::LocalCommitted
    );
    assert_eq!(retry_source.reads, 0);
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::LocalCommitted {
            context,
            committed_revision: 2,
            ..
        }) if context.credential == rebound
    ));

    let retained = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&prepared)
        .unwrap();
    let altered_manifest = String::from_utf8(remote.manifest_bytes.clone())
        .unwrap()
        .replace("remote-disjoint", "remote-changedx")
        .into_bytes();
    let mut stale_source = fixture_source(&remote);
    assert!(matches!(
        resume_bidirectional_target_prepared(
            &mut store,
            &cas,
            directory.path(),
            &operation_id,
            2,
            None,
            &altered_manifest,
            &mut stale_source,
        ),
        Err(PeerSyncError::StaleManifest { .. })
    ));
    assert_eq!(stale_source.reads, 0);
    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(prepared)
    );
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&retained)
        .unwrap();
}

#[test]
fn target_completion_retries_job_release_before_dropping_its_job_identity() {
    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut source = fixture_source(&remote);
    TARGET_JOB_RELEASE_FAILPOINT.with(|enabled| enabled.set(true));
    let error = begin_bidirectional_local_merge(
        &mut store,
        &cas,
        directory.path(),
        credential,
        1,
        &remote.manifest_bytes,
        &mut source,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        PeerSyncError::Storage(message) if message.contains("job release")
    ));
    let retained = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let (operation_id, job_id, remote_revision, shared) = match &retained {
        PeerBidirectionalDurableOperation::LocalCommitted {
            context,
            shared_generation,
            remote_apply_receipt: None,
            ..
        } => (
            context.operation_id.clone(),
            context.durable_job_id.clone(),
            context.expected_remote_revision,
            shared_generation.clone(),
        ),
        other => panic!("expected retained local completion, got {other:?}"),
    };
    let retained_job = DurableCasJob::open(directory.path(), &job_id).unwrap();
    assert_eq!(retained_job.kind(), CasJobKind::LogicalDeltaTarget);
    assert!(retained_job.is_sealed());
    drop(retained_job);

    let receipt = LanBidirectionalRemoteApplyReceipt {
        committed_revision: remote_revision,
        committed_generation: LanBidirectionalGeneration {
            generation_id: shared.generation_id,
            manifest_hash: shared.manifest_hash,
            generation_sequence: shared.generation_sequence,
        },
        transferred_objects: 0,
        transferred_bytes: 0,
        backup: None,
    };
    TARGET_JOB_RELEASE_FAILPOINT.with(|enabled| enabled.set(true));
    assert!(matches!(
        complete_bidirectional_local_after_remote_apply(
            &mut store,
            &cas,
            directory.path(),
            &operation_id,
            receipt.clone(),
        ),
        Err(PeerSyncError::Storage(message)) if message.contains("job release")
    ));
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::LocalCommitted {
            remote_apply_receipt: Some(retained),
            ..
        }) if retained == receipt
    ));
    let mut released_job = DurableCasJob::open(directory.path(), &job_id).unwrap();
    released_job
        .leave_release_record_for_cleanup_retry(CasReleaseOutcome::Committed)
        .unwrap();
    drop(released_job);

    let result = complete_bidirectional_local_after_remote_apply(
        &mut store,
        &cas,
        directory.path(),
        &operation_id,
        receipt,
    )
    .unwrap();

    assert_eq!(result.operation_id, operation_id);
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::Completed { .. })
    ));
    assert!(matches!(
        DurableCasJob::open(directory.path(), &job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn target_completion_retains_local_committed_when_its_job_has_the_wrong_kind() {
    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut source = fixture_source(&remote);
    TARGET_JOB_RELEASE_FAILPOINT.with(|enabled| enabled.set(true));
    assert!(begin_bidirectional_local_merge(
        &mut store,
        &cas,
        directory.path(),
        credential,
        1,
        &remote.manifest_bytes,
        &mut source,
    )
    .is_err());
    let retained = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let (operation_id, job_id, remote_revision, shared) = match &retained {
        PeerBidirectionalDurableOperation::LocalCommitted {
            context,
            shared_generation,
            ..
        } => (
            context.operation_id.clone(),
            context.durable_job_id.clone(),
            context.expected_remote_revision,
            shared_generation.clone(),
        ),
        other => panic!("expected retained local completion, got {other:?}"),
    };
    DurableCasJob::open(directory.path(), &job_id)
        .unwrap()
        .release(CasReleaseOutcome::Aborted)
        .unwrap();
    let wrong_job =
        DurableCasJob::begin(directory.path(), &job_id, CasJobKind::PeerClone, 0).unwrap();
    drop(wrong_job);
    let receipt = LanBidirectionalRemoteApplyReceipt {
        committed_revision: remote_revision,
        committed_generation: LanBidirectionalGeneration {
            generation_id: shared.generation_id,
            manifest_hash: shared.manifest_hash,
            generation_sequence: shared.generation_sequence,
        },
        transferred_objects: 0,
        transferred_bytes: 0,
        backup: None,
    };

    assert!(matches!(
        complete_bidirectional_local_after_remote_apply(
            &mut store,
            &cas,
            directory.path(),
            &operation_id,
            receipt.clone(),
        ),
        Err(PeerSyncError::Storage(message)) if message.contains("unexpected identity")
    ));
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::LocalCommitted {
            remote_apply_receipt: Some(retained),
            ..
        }) if retained == receipt
    ));
    assert_eq!(
        DurableCasJob::open(directory.path(), &job_id)
            .unwrap()
            .kind(),
        CasJobKind::PeerClone
    );
}

#[test]
fn target_postactivation_status_promotes_only_an_exact_committed_witness() {
    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut source = fixture_source(&remote);
    TARGET_AFTER_ACTIVATION_FAILPOINT.with(|enabled| enabled.set(true));
    assert!(begin_bidirectional_local_merge(
        &mut store,
        &cas,
        directory.path(),
        credential,
        1,
        &remote.manifest_bytes,
        &mut source,
    )
    .is_err());
    let prepared = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let (operation_id, job_id) = match &prepared {
        PeerBidirectionalDurableOperation::TargetPrepared { context, .. } => {
            (context.operation_id.clone(), context.durable_job_id.clone())
        }
        other => panic!("expected target-prepared activation crash, got {other:?}"),
    };

    let state = PeerBidirectionalCommandState::default();
    let active_target = state.begin_target().unwrap();
    let busy_status = state.status(directory.path(), &mut store).unwrap();
    assert!(matches!(
        busy_status.operation,
        Some(PeerBidirectionalStatusOperation::TargetPrepared {
            operation_id: retained_operation,
        }) if retained_operation == operation_id
    ));
    assert!(DurableCasJob::open(directory.path(), &job_id).is_ok());
    drop(active_target);

    let status = state.status(directory.path(), &mut store).unwrap();

    assert!(matches!(
        status.operation,
        Some(PeerBidirectionalStatusOperation::LocalCommitted {
            operation_id: retained_operation,
            committed_revision: 2,
        }) if retained_operation == operation_id
    ));
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::LocalCommitted {
            committed_revision: 2,
            ..
        })
    ));
    assert!(matches!(
        DurableCasJob::open(directory.path(), &job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
    assert_eq!(source.reads, 1);
}

#[test]
fn target_postactivation_status_rejects_malformed_public_https_without_mutation() {
    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut source = fixture_source(&remote);
    TARGET_AFTER_ACTIVATION_FAILPOINT.with(|enabled| enabled.set(true));
    assert!(begin_bidirectional_local_merge(
        &mut store,
        &cas,
        directory.path(),
        credential,
        1,
        &remote.manifest_bytes,
        &mut source,
    )
    .is_err());

    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    let mut prepared = journal.load().unwrap().unwrap();
    let job_id = match &mut prepared {
        PeerBidirectionalDurableOperation::TargetPrepared { context, .. } => {
            context.credential.endpoint = "https://sync.example.com/path".to_owned();
            context.durable_job_id.clone()
        }
        other => panic!("expected target-prepared activation crash, got {other:?}"),
    };
    let retained_bytes = serde_json::to_vec(&prepared).unwrap();
    let operation_path = journal.root.join(OPERATION_FILE);
    fs::write(&operation_path, &retained_bytes).unwrap();
    let revision_before = store.revision().unwrap();
    let root_before = store.read_root(None).unwrap();

    assert!(matches!(
        PeerBidirectionalCommandState::default().status(directory.path(), &mut store),
        Err(PeerSyncError::Storage(message)) if message.contains("desktop endpoint")
    ));
    assert_eq!(store.revision().unwrap(), revision_before);
    assert_eq!(store.read_root(None).unwrap(), root_before);
    assert_eq!(fs::read(operation_path).unwrap(), retained_bytes);
    assert!(DurableCasJob::open(directory.path(), &job_id).is_ok());
}

#[test]
fn target_exact_seal_race_retains_prepared_and_preserves_the_descendant() {
    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut source = fixture_source(&remote);
    TARGET_AFTER_ACTIVATION_FAILPOINT.with(|enabled| enabled.set(true));
    assert!(begin_bidirectional_local_merge(
        &mut store,
        &cas,
        directory.path(),
        credential,
        1,
        &remote.manifest_bytes,
        &mut source,
    )
    .is_err());
    let prepared = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let operation_id = prepared.operation_id().to_owned();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 2,
            root: Some(json!({"preserved": "racing descendant"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();

    let status = PeerBidirectionalCommandState::default()
        .status(directory.path(), &mut store)
        .unwrap();
    assert!(matches!(
        status.operation,
        Some(PeerBidirectionalStatusOperation::TargetPrepared {
            operation_id: retained,
        }) if retained == operation_id
    ));
    let mut retry_source = fixture_source(&remote);
    assert!(matches!(
        resume_bidirectional_target_prepared(
            &mut store,
            &cas,
            directory.path(),
            &operation_id,
            3,
            None,
            &remote.manifest_bytes,
            &mut retry_source,
        ),
        Err(PeerSyncError::ActivationConflict { .. })
    ));
    assert_eq!(retry_source.reads, 0);
    assert_eq!(
        store.read_root(None).unwrap().value,
        json!({"preserved": "racing descendant"})
    );
    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(prepared)
    );
}

#[test]
fn target_no_op_activation_crash_recovers_at_the_same_revision() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let peer_id = "123e4567-e89b-42d3-a456-426614174072";
    let common = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                common,
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    let credential = LanBidirectionalLogicalCredential {
        endpoint: "http://192.168.1.2:32146".to_owned(),
        session_id: "123e4567-e89b-42d3-a456-426614174073".to_owned(),
        manifest_id: base.manifest_hash.clone(),
        device_id: "123e4567-e89b-42d3-a456-426614174074".to_owned(),
        source_device_id: peer_id.to_owned(),
        bearer: "a".repeat(64),
    };
    let mut source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    TARGET_AFTER_ACTIVATION_FAILPOINT.with(|enabled| enabled.set(true));
    assert!(matches!(
        begin_bidirectional_local_merge(
            &mut store,
            &cas,
            directory.path(),
            credential,
            0,
            &base.manifest_bytes,
            &mut source,
        ),
        Err(PeerSyncError::Storage(_))
    ));
    let prepared = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let operation_id = prepared.operation_id().to_owned();
    assert_eq!(store.revision().unwrap(), 0);
    let mut retry_source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    assert_eq!(
        resume_bidirectional_target_prepared(
            &mut store,
            &cas,
            directory.path(),
            &operation_id,
            0,
            None,
            &base.manifest_bytes,
            &mut retry_source,
        )
        .unwrap(),
        LocalMergeOutcome::LocalCommitted
    );
    assert_eq!(retry_source.reads, 0);
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::LocalCommitted {
            committed_revision: 0,
            changed: false,
            transferred_objects: 0,
            transferred_bytes: 0,
            ..
        })
    ));
}

#[test]
fn disjoint_merge_commits_shared_a_and_retains_peer_state_at_c() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let peer_id = "123e4567-e89b-42d3-a456-426614174012";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    let common = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                common.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"side": "local"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let remote = remote_disjoint_manifest(&base.manifest);
    let remote_object_bytes = remote
        .record_objects
        .iter()
        .find(|record| record.key != "r1:root")
        .unwrap()
        .object
        .size;
    let credential = LanBidirectionalLogicalCredential {
        endpoint: "http://192.168.1.2:32146".to_owned(),
        session_id: "123e4567-e89b-42d3-a456-426614174010".to_owned(),
        manifest_id: remote.manifest_hash.clone(),
        device_id: "123e4567-e89b-42d3-a456-426614174011".to_owned(),
        source_device_id: peer_id.to_owned(),
        bearer: "d".repeat(64),
    };
    let mut source = fixture_source(&remote);

    assert_eq!(
        begin_bidirectional_local_merge(
            &mut store,
            &cas,
            directory.path(),
            credential,
            1,
            &remote.manifest_bytes,
            &mut source,
        )
        .unwrap(),
        LocalMergeOutcome::LocalCommitted
    );

    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(
        store.read_root(None).unwrap().value,
        json!({"side": "local"})
    );
    assert_eq!(
        store
            .read_plugin_storage("remote-key", None)
            .unwrap()
            .unwrap()
            .value,
        json!({"side": "remote"})
    );
    assert_eq!(source.reads, 1);
    assert_eq!(
        store
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap()
            .shared_identity,
        common
    );
    let retained = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    match retained {
        PeerBidirectionalDurableOperation::LocalCommitted {
            committed_revision,
            shared_generation,
            transferred_objects,
            transferred_bytes,
            backups,
            ..
        } => {
            assert_eq!(committed_revision, 2);
            assert_eq!(shared_generation.generation_sequence, "2");
            assert_eq!(transferred_objects, 1);
            assert_eq!(transferred_bytes, remote_object_bytes);
            assert!(backups.is_empty());
        }
        other => panic!("unexpected retained operation: {other:?}"),
    }
}

#[test]
fn remote_apply_response_loss_reissues_shared_a_without_redownload() {
    let directory = tempfile::tempdir().unwrap();
    let bootstrap_root = directory.path().join("bootstrap");
    fs::create_dir(&bootstrap_root).unwrap();
    let bootstrap_cas = PayloadCas::new(&bootstrap_root).unwrap();
    let mut bootstrap_store = PersistentStore::open(&bootstrap_root).unwrap();
    let base = bootstrap_store
        .seal_or_initialize_active_logical_generation(&bootstrap_cas)
        .unwrap();
    drop(bootstrap_store);
    let local_root = directory.path().join("local");
    let remote_root = directory.path().join("remote");
    copy_tree(&bootstrap_root, &local_root);
    copy_tree(&bootstrap_root, &remote_root);

    let local_cas = PayloadCas::new(&local_root).unwrap();
    let remote_cas = PayloadCas::new(&remote_root).unwrap();
    let mut local_store = PersistentStore::open(&local_root).unwrap();
    let mut remote_store = PersistentStore::open(&remote_root).unwrap();
    let local_device = "123e4567-e89b-42d3-a456-426614174021";
    let remote_device = "123e4567-e89b-42d3-a456-426614174022";
    let common = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    for (store, cas, peer_id) in [
        (&mut local_store, &local_cas, remote_device),
        (&mut remote_store, &remote_cas, local_device),
    ] {
        establish_logical_common_base(
            store,
            cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    common.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
    }
    local_store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"side": "local"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    remote_store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: Some(vec![crate::persistent_store::PluginStorageMutation::Set {
                key: "remote-key".to_owned(),
                value: json!({"side": "remote"}),
            }]),
            asset_owner_heads: None,
        })
        .unwrap();
    let remote_generation = remote_store
        .seal_or_initialize_active_logical_generation(&remote_cas)
        .unwrap();
    let mut remote_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(&remote_root).unwrap(),
        &remote_root,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote_generation.manifest.generation,
    )
    .unwrap();
    let credential = LanBidirectionalLogicalCredential {
        endpoint: "http://192.168.1.2:32146".to_owned(),
        session_id: "123e4567-e89b-42d3-a456-426614174020".to_owned(),
        manifest_id: remote_generation.manifest_hash.clone(),
        device_id: local_device.to_owned(),
        source_device_id: remote_device.to_owned(),
        bearer: "e".repeat(64),
    };
    assert_eq!(
        begin_bidirectional_local_merge(
            &mut local_store,
            &local_cas,
            &local_root,
            credential,
            1,
            &remote_generation.manifest_bytes,
            &mut remote_source,
        )
        .unwrap(),
        LocalMergeOutcome::LocalCommitted
    );
    let operation_id = match PeerBidirectionalOperationJournal::new(&local_root)
        .load()
        .unwrap()
        .unwrap()
    {
        PeerBidirectionalDurableOperation::LocalCommitted { context, .. } => context.operation_id,
        other => panic!("unexpected retained operation: {other:?}"),
    };
    let shared = local_store
        .seal_or_initialize_active_logical_generation(&local_cas)
        .unwrap();
    let shared_identity = SyncGenerationIdentity {
        generation_id: shared.manifest.generation.clone(),
        manifest_hash: shared.manifest_hash.clone(),
        generation_sequence: shared.manifest.generation_sequence.clone(),
    };
    let remote_identity = SyncGenerationIdentity {
        generation_id: remote_generation.manifest.generation.clone(),
        manifest_hash: remote_generation.manifest_hash.clone(),
        generation_sequence: remote_generation.manifest.generation_sequence.clone(),
    };
    let mut shared_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(&local_root).unwrap(),
        &local_root,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &shared.manifest.generation,
    )
    .unwrap();

    let receipt = apply_bidirectional_remote_shared(
        &mut remote_store,
        &remote_cas,
        &remote_root,
        &operation_id,
        local_device,
        1,
        &common.manifest_hash,
        &remote_identity,
        LanBidirectionalGeneration {
            generation_id: shared_identity.generation_id.clone(),
            manifest_hash: shared_identity.manifest_hash.clone(),
            generation_sequence: shared_identity.generation_sequence.clone(),
        },
        &shared.manifest_bytes,
        &mut shared_source,
        false,
    )
    .unwrap();

    assert_eq!(receipt.committed_revision, 2);
    assert_eq!(
        receipt.committed_generation,
        LanBidirectionalGeneration {
            generation_id: shared_identity.generation_id.clone(),
            manifest_hash: shared_identity.manifest_hash.clone(),
            generation_sequence: shared_identity.generation_sequence.clone(),
        }
    );
    assert_eq!(receipt.transferred_objects, 1);
    assert!(receipt.transferred_bytes > 0);
    assert!(receipt.backup.is_none());
    assert_eq!(
        remote_store.read_root(None).unwrap().value,
        json!({"side": "local"})
    );
    assert_eq!(
        remote_store
            .read_plugin_storage("remote-key", None)
            .unwrap()
            .unwrap()
            .value,
        json!({"side": "remote"})
    );
    let remote_ack = remote_store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, local_device)
        .unwrap();
    assert_eq!(remote_ack.shared_identity, shared_identity);
    assert_ne!(remote_ack.local_identity, remote_ack.shared_identity);

    drop(shared_source);
    drop(remote_source);
    drop(remote_store);
    drop(local_store);
    let mut remote_store = PersistentStore::open(&remote_root).unwrap();
    let mut local_store = PersistentStore::open(&local_root).unwrap();
    let mut retry_source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    let recovered_receipt = apply_bidirectional_remote_shared(
        &mut remote_store,
        &remote_cas,
        &remote_root,
        &operation_id,
        local_device,
        1,
        &common.manifest_hash,
        &remote_identity,
        LanBidirectionalGeneration {
            generation_id: shared_identity.generation_id.clone(),
            manifest_hash: shared_identity.manifest_hash.clone(),
            generation_sequence: shared_identity.generation_sequence.clone(),
        },
        &shared.manifest_bytes,
        &mut retry_source,
        false,
    )
    .unwrap();
    assert_eq!(remote_store.revision().unwrap(), 2);
    assert_eq!(recovered_receipt.committed_revision, 2);
    assert_eq!(
        recovered_receipt.committed_generation,
        LanBidirectionalGeneration {
            generation_id: shared_identity.generation_id.clone(),
            manifest_hash: shared_identity.manifest_hash.clone(),
            generation_sequence: shared_identity.generation_sequence.clone(),
        }
    );
    assert_eq!(recovered_receipt.transferred_objects, 0);
    assert_eq!(recovered_receipt.transferred_bytes, 0);
    assert!(recovered_receipt.backup.is_none());
    assert_eq!(retry_source.reads, 0);

    let result = complete_bidirectional_local_after_remote_apply(
        &mut local_store,
        &local_cas,
        &local_root,
        &operation_id,
        recovered_receipt,
    )
    .unwrap();

    assert_eq!(result.kind, "updated");
    assert_eq!(result.operation_id, operation_id);
    assert_eq!(result.revision, 2);
    assert_eq!(result.remote_revision, 2);
    assert_eq!(result.transferred_objects, 1);
    assert!(result.transferred_bytes > 0);
    assert!(result.backups.is_empty());
    let local_ack = local_store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, remote_device)
        .unwrap();
    assert_eq!(local_ack.shared_identity, shared_identity);
    assert_eq!(local_ack.local_identity, shared_identity);
    assert_eq!(
        PeerBidirectionalOperationJournal::new(&local_root)
            .load()
            .unwrap()
            .unwrap(),
        PeerBidirectionalDurableOperation::Completed {
            schema: OPERATION_SCHEMA.to_owned(),
            remote_apply_receipt: None,
            source_binding: None,
            result: result.clone(),
        }
    );

    remote_store
        .revoke_sync_device(PRODUCT_LOGICAL_LIBRARY_ID, local_device, &shared_identity)
        .unwrap();
    let mut invalid_retry_source = FixtureSource {
        objects: BTreeMap::new(),
        reads: 0,
    };
    assert!(matches!(
        apply_bidirectional_remote_shared(
            &mut remote_store,
            &remote_cas,
            &remote_root,
            &operation_id,
            local_device,
            1,
            &common.manifest_hash,
            &remote_identity,
            LanBidirectionalGeneration {
                generation_id: shared_identity.generation_id.clone(),
                manifest_hash: shared_identity.manifest_hash.clone(),
                generation_sequence: shared_identity.generation_sequence.clone(),
            },
            &shared.manifest_bytes,
            &mut invalid_retry_source,
            false,
        ),
        Err(PeerSyncError::ActivationConflict { .. })
    ));
    assert_eq!(invalid_retry_source.reads, 0);
}

#[test]
fn production_control_attaches_only_an_exact_existing_p4_common_base() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let target_device_id = "123e4567-e89b-42d3-a456-426614174080";
    establish_logical_common_base(
        &mut store,
        &cas,
        target_device_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    let inspector = store.open_native_job_store().unwrap();
    let source_session_id = "123e4567-e89b-42d3-a456-426614174081";
    let source_device_id = "123e4567-e89b-42d3-a456-426614174082";
    let control = ProductionLanBidirectionalControl::new(
        directory.path().to_path_buf(),
        store,
        source_session_id,
        source_device_id,
    );
    let common = LanBidirectionalGeneration {
        generation_id: base.manifest.generation,
        manifest_hash: base.manifest_hash,
        generation_sequence: base.manifest.generation_sequence,
    };
    let session = super::super::lan::LanBidirectionalSession {
        session_id: source_session_id.to_owned(),
        source_device_id: source_device_id.to_owned(),
        target_device_id: target_device_id.to_owned(),
    };

    control
        .register(
            session.clone(),
            super::super::lan::LanBidirectionalRegistrationRequest {
                library_id: PRODUCT_LOGICAL_LIBRARY_ID.to_owned(),
                generation: common.clone(),
                expected_revision: 0,
            },
        )
        .unwrap();
    assert_eq!(
        inspector
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
            .unwrap()
            .shared_identity
            .manifest_hash,
        common.manifest_hash
    );

    let mismatched = LanBidirectionalGeneration {
        generation_id: "mismatched-base".to_owned(),
        manifest_hash: "a".repeat(64),
        generation_sequence: "0".to_owned(),
    };
    let error = control
        .register(
            session.clone(),
            super::super::lan::LanBidirectionalRegistrationRequest {
                library_id: PRODUCT_LOGICAL_LIBRARY_ID.to_owned(),
                generation: mismatched,
                expected_revision: 0,
            },
        )
        .unwrap_err();
    assert!(matches!(error, PeerSyncError::Storage(_)));
    assert_eq!(
        inspector
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
            .unwrap()
            .shared_identity
            .manifest_hash,
        common.manifest_hash
    );

    let unknown_target = "123e4567-e89b-42d3-a456-426614174083";
    let error = control
        .register(
            super::super::lan::LanBidirectionalSession {
                target_device_id: unknown_target.to_owned(),
                ..session
            },
            super::super::lan::LanBidirectionalRegistrationRequest {
                library_id: PRODUCT_LOGICAL_LIBRARY_ID.to_owned(),
                generation: common,
                expected_revision: 0,
            },
        )
        .unwrap_err();
    assert!(matches!(error, PeerSyncError::Storage(_)));
    assert!(inspector
        .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, unknown_target)
        .unwrap()
        .is_none());
}

#[test]
fn source_prepared_precommit_descendant_is_abandoned_without_losing_the_edit() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let source_device_id = "123e4567-e89b-42d3-a456-426614174086";
    let target_device_id = "123e4567-e89b-42d3-a456-426614174087";
    let operation_id = "123e4567-e89b-42d3-a456-426614174088";
    let previous = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    establish_logical_common_base(
        &mut store,
        &cas,
        target_device_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                target_device_id,
                previous.clone(),
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    let evidence = SourcePreparedEvidence {
        operation_id: operation_id.to_owned(),
        source_device_id: source_device_id.to_owned(),
        target_device_id: target_device_id.to_owned(),
        expected_source_revision: 0,
        previous_shared: previous.clone(),
        expected_source_generation: previous.clone(),
        shared_generation: LanBidirectionalGeneration {
            generation_id: "123e4567-e89b-42d3-a456-426614174089".to_owned(),
            manifest_hash: "a".repeat(64),
            generation_sequence: "1".to_owned(),
        },
        incoming_revision: 1,
        transferred_objects: 2,
        transferred_bytes: 19,
        backup_required: false,
        backup: None,
        completion_deferred_v1: false,
    };
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174090";
    let mut job = DurableCasJob::begin(
        directory.path(),
        durable_job_id,
        CasJobKind::LogicalDeltaTarget,
        0,
    )
    .unwrap();
    job.seal(&mut store, 0).unwrap();
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&evidence.durable_operation(durable_job_id.to_owned()))
        .unwrap();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"preserved": "source-local-edit"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let session = LanBidirectionalSession {
        session_id: "123e4567-e89b-42d3-a456-426614174091".to_owned(),
        source_device_id: source_device_id.to_owned(),
        target_device_id: target_device_id.to_owned(),
    };
    let request = LanBidirectionalRemoteApplyRequest {
        operation_id: operation_id.to_owned(),
        source_endpoint: "http://127.0.0.1:1".to_owned(),
        source_session_id: "123e4567-e89b-42d3-a456-426614174092".to_owned(),
        source_manifest_id: "manifest".to_owned(),
        source_claim: "claim".to_owned(),
        expected_source_revision: 0,
        expected_source_generation: LanBidirectionalGeneration {
            generation_id: previous.generation_id.clone(),
            manifest_hash: previous.manifest_hash.clone(),
            generation_sequence: previous.generation_sequence.clone(),
        },
        expected_common_base_manifest_hash: previous.manifest_hash.clone(),
        backup_losing_side: false,
        completion_deferred_v1: None,
    };

    drop(store);
    let control = ProductionLanBidirectionalControl::new(
        directory.path().to_path_buf(),
        PersistentStore::open(directory.path()).unwrap(),
        &session.session_id,
        source_device_id,
    );
    let error = control
        .remote_apply(session, request, &NeverCancelled)
        .unwrap_err();
    drop(control);
    let store = PersistentStore::open(directory.path()).unwrap();

    assert!(matches!(
        error,
        PeerSyncError::StaleManifest { .. } | PeerSyncError::ActivationConflict { .. }
    ));
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(
        store.read_root(None).unwrap().value["preserved"],
        "source-local-edit"
    );
    assert_eq!(
        store
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
            .unwrap(),
        Some(previous.clone())
    );
    assert_eq!(
        store
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
            .unwrap()
            .shared_identity,
        previous
    );
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert_eq!(
        DurableCasJob::open(directory.path(), durable_job_id)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn source_activation_crash_reopens_with_the_exact_prepared_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    seed_lossless_backup_fixture(&mut store, &cas);
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let target_device_id = "123e4567-e89b-42d3-a456-426614174092";
    let source_device_id = "123e4567-e89b-42d3-a456-426614174094";
    establish_logical_common_base(
        &mut store,
        &cas,
        target_device_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        1,
        &base.manifest_bytes,
    )
    .unwrap();
    let common = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                target_device_id,
                common.clone(),
                0,
            )
            .unwrap(),
            1,
        )
        .unwrap();
    establish_logical_common_base(
        &mut store,
        &cas,
        source_device_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        1,
        &base.manifest_bytes,
    )
    .unwrap();
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                source_device_id,
                common.clone(),
                0,
            )
            .unwrap(),
            1,
        )
        .unwrap();
    drop(store);
    let remote_directory = tempfile::tempdir().unwrap();
    copy_tree(directory.path(), remote_directory.path());
    let remote_cas = PayloadCas::new(remote_directory.path()).unwrap();
    let mut remote_store = PersistentStore::open(remote_directory.path()).unwrap();
    remote_store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: Some(vec![crate::persistent_store::PluginStorageMutation::Set {
                key: "remote-key".to_owned(),
                value: json!({"side": "remote"}),
            }]),
            asset_owner_heads: None,
        })
        .unwrap();
    let remote = remote_store
        .seal_or_initialize_active_logical_generation(&remote_cas)
        .unwrap();
    drop(remote_store);
    let inspector = PersistentStore::open(directory.path()).unwrap();
    let source_session_id = "123e4567-e89b-42d3-a456-426614174093";
    let control = ProductionLanBidirectionalControl::new(
        directory.path().to_path_buf(),
        PersistentStore::open(directory.path()).unwrap(),
        source_session_id,
        source_device_id,
    );
    let temporary_session_id = "123e4567-e89b-42d3-a456-426614174095";
    let remote_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &remote.manifest.generation,
    )
    .unwrap();
    let prepared = super::super::lan::PreparedLogicalLanSession::new(
        temporary_session_id,
        target_device_id,
        remote.manifest_hash.clone(),
        remote.manifest_bytes.clone(),
        remote
            .manifest
            .objects
            .iter()
            .map(|object| LogicalDeltaObject {
                hash: object.hash.clone(),
                size: object.size,
            })
            .collect(),
        Box::new(remote_source),
    )
    .unwrap();
    let mut host = LanCloneHost::prepare_logical(prepared);
    let pairing = host.start().unwrap();
    let mut request = LanBidirectionalRemoteApplyRequest {
        operation_id: "123e4567-e89b-42d3-a456-426614174096".to_owned(),
        source_endpoint: format!("http://127.0.0.1:{}", host.address().unwrap().port()),
        source_session_id: pairing.session_id,
        source_manifest_id: pairing.manifest_id,
        source_claim: pairing.claim,
        expected_source_revision: 1,
        expected_source_generation: LanBidirectionalGeneration {
            generation_id: common.generation_id.clone(),
            manifest_hash: common.manifest_hash.clone(),
            generation_sequence: common.generation_sequence.clone(),
        },
        expected_common_base_manifest_hash: common.manifest_hash.clone(),
        backup_losing_side: true,
        completion_deferred_v1: None,
    };
    let session = LanBidirectionalSession {
        session_id: source_session_id.to_owned(),
        source_device_id: source_device_id.to_owned(),
        target_device_id: target_device_id.to_owned(),
    };

    SOURCE_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.set(true));
    let error = control
        .remote_apply(session.clone(), request.clone(), &NeverCancelled)
        .unwrap_err();
    assert!(
        matches!(error, PeerSyncError::Storage(message) if message.contains("source-prepared"))
    );
    assert_eq!(inspector.revision().unwrap(), 1);
    assert_eq!(
        inspector
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
            .unwrap(),
        Some(common.clone())
    );
    assert_eq!(
        inspector
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
            .unwrap()
            .shared_identity,
        common
    );
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert!(durable_job_journal_ids(directory.path()).is_empty());
    assert!(!bidirectional_backup_path(
        directory.path(),
        &request.operation_id,
        PeerBidirectionalBackupSide::Remote,
    )
    .exists());
    host.stop().unwrap();

    let mut retry_remote_store = PersistentStore::open(remote_directory.path()).unwrap();
    retry_remote_store
        .commit(&WorkingSetCommit {
            expected_revision: 2,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: Some(vec![crate::persistent_store::PluginStorageMutation::Set {
                key: "remote-key-2".to_owned(),
                value: json!({"side": "remote-2"}),
            }]),
            asset_owner_heads: None,
        })
        .unwrap();
    let retry_remote = retry_remote_store
        .seal_or_initialize_active_logical_generation(&remote_cas)
        .unwrap();
    drop(retry_remote_store);
    let retry_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &retry_remote.manifest.generation,
    )
    .unwrap();
    let retry_prepared = super::super::lan::PreparedLogicalLanSession::new(
        "123e4567-e89b-42d3-a456-426614174109",
        target_device_id,
        retry_remote.manifest_hash.clone(),
        retry_remote.manifest_bytes.clone(),
        retry_source.objects().to_vec(),
        Box::new(retry_source),
    )
    .unwrap();
    let mut retry_host = LanCloneHost::prepare_logical(retry_prepared);
    let retry_pairing = retry_host.start().unwrap();
    request.source_endpoint = format!("http://127.0.0.1:{}", retry_host.address().unwrap().port());
    request.source_session_id = retry_pairing.session_id;
    request.source_manifest_id = retry_pairing.manifest_id;
    request.source_claim = retry_pairing.claim;

    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    SOURCE_AFTER_INITIAL_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.set(true));
    let initial_prepared_error = control
        .remote_apply(session.clone(), request.clone(), &NeverCancelled)
        .unwrap_err();
    assert!(matches!(
        initial_prepared_error,
        PeerSyncError::Storage(message) if message.contains("initial source-prepared")
    ));
    let initial_prepared = journal.load().unwrap().unwrap();
    assert!(matches!(
        &initial_prepared,
        PeerBidirectionalDurableOperation::SourcePrepared {
            operation_id,
            durable_job_id,
            backup_required: true,
            backup: None,
            ..
        } if operation_id == &request.operation_id && durable_job_id == operation_id
    ));
    let source_backup_path = bidirectional_backup_path(
        directory.path(),
        &request.operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    assert!(!source_backup_path.exists());
    assert!(durable_job_journal_ids(directory.path()).is_empty());

    retry_host.stop().unwrap();
    let retry_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &retry_remote.manifest.generation,
    )
    .unwrap();
    let retry_prepared = super::super::lan::PreparedLogicalLanSession::new(
        &uuid::Uuid::new_v4().to_string(),
        target_device_id,
        retry_remote.manifest_hash.clone(),
        retry_remote.manifest_bytes.clone(),
        retry_source.objects().to_vec(),
        Box::new(retry_source),
    )
    .unwrap();
    retry_host = LanCloneHost::prepare_logical(retry_prepared);
    let retry_pairing = retry_host.start().unwrap();
    request.source_endpoint = format!("http://127.0.0.1:{}", retry_host.address().unwrap().port());
    request.source_session_id = retry_pairing.session_id;
    request.source_manifest_id = retry_pairing.manifest_id;
    request.source_claim = retry_pairing.claim;

    SOURCE_AFTER_BACKUP_BEFORE_RECEIPT_STORE_FAILPOINT.with(|enabled| enabled.set(true));
    let published_backup_error = control
        .remote_apply(session.clone(), request.clone(), &NeverCancelled)
        .unwrap_err();
    assert!(
        matches!(
            &published_backup_error,
            PeerSyncError::Storage(message) if message.contains("backup publication")
        ),
        "{published_backup_error:?}"
    );
    assert!(source_backup_path.is_file());
    assert_eq!(journal.load().unwrap(), Some(initial_prepared));
    assert!(durable_job_journal_ids(directory.path()).is_empty());

    let mut abandon_store = PersistentStore::open(directory.path()).unwrap();
    PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut abandon_store, &request.operation_id)
        .unwrap();
    assert!(journal.load().unwrap().is_none());
    assert!(!source_backup_path.exists());
    assert!(durable_job_journal_ids(directory.path()).is_empty());

    retry_host.stop().unwrap();
    let retry_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &retry_remote.manifest.generation,
    )
    .unwrap();
    let retry_prepared = super::super::lan::PreparedLogicalLanSession::new(
        &uuid::Uuid::new_v4().to_string(),
        target_device_id,
        retry_remote.manifest_hash.clone(),
        retry_remote.manifest_bytes.clone(),
        retry_source.objects().to_vec(),
        Box::new(retry_source),
    )
    .unwrap();
    retry_host = LanCloneHost::prepare_logical(retry_prepared);
    let retry_pairing = retry_host.start().unwrap();
    request.source_endpoint = format!("http://127.0.0.1:{}", retry_host.address().unwrap().port());
    request.source_session_id = retry_pairing.session_id;
    request.source_manifest_id = retry_pairing.manifest_id;
    request.source_claim = retry_pairing.claim;

    SOURCE_AFTER_PREPARED_STORE_PANIC.with(|enabled| enabled.set(true));
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        control.remote_apply(session.clone(), request.clone(), &NeverCancelled)
    }));
    assert!(crashed.is_err());
    drop(control);
    assert_no_logical_delta_staging_orphans(directory.path());
    let crashed_prepared = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let crashed_job_id = match &crashed_prepared {
        PeerBidirectionalDurableOperation::SourcePrepared {
            durable_job_id,
            backup_required: true,
            backup: Some(_),
            ..
        } => durable_job_id.clone(),
        other => panic!("expected source-prepared crash journal, got {other:?}"),
    };
    assert_eq!(crashed_job_id, request.operation_id);
    assert!(matches!(
        DurableCasJob::open(directory.path(), &crashed_job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
    let crashed_roots = collect_durable_cas_job_roots(directory.path());
    assert!(crashed_roots
        .blockers
        .iter()
        .all(|blocker| !blocker.starts_with("job-pin-unsealed:")));
    retry_host.stop().unwrap();
    let handoff_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &retry_remote.manifest.generation,
    )
    .unwrap();
    let handoff_prepared = super::super::lan::PreparedLogicalLanSession::new(
        &uuid::Uuid::new_v4().to_string(),
        target_device_id,
        retry_remote.manifest_hash.clone(),
        retry_remote.manifest_bytes.clone(),
        handoff_source.objects().to_vec(),
        Box::new(handoff_source),
    )
    .unwrap();
    retry_host = LanCloneHost::prepare_logical(handoff_prepared);
    let handoff_pairing = retry_host.start().unwrap();
    request.source_endpoint = format!("http://127.0.0.1:{}", retry_host.address().unwrap().port());
    request.source_session_id = handoff_pairing.session_id;
    request.source_manifest_id = handoff_pairing.manifest_id;
    request.source_claim = handoff_pairing.claim;

    let recovery_control = ProductionLanBidirectionalControl::new(
        directory.path().to_path_buf(),
        PersistentStore::open(directory.path()).unwrap(),
        source_session_id,
        source_device_id,
    );
    SOURCE_AFTER_PREPARED_CALLBACK_FAILPOINT.with(|enabled| enabled.set(true));
    let handoff_error = recovery_control
        .remote_apply(session.clone(), request.clone(), &NeverCancelled)
        .unwrap_err();
    assert!(
        matches!(&handoff_error, PeerSyncError::Storage(message) if message.contains("pre-activation")),
        "{handoff_error:?}"
    );
    let retained_old_job = DurableCasJob::open(directory.path(), &crashed_job_id).unwrap();
    assert!(!retained_old_job.is_sealed());
    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(crashed_prepared)
    );
    assert_no_logical_delta_staging_orphans(directory.path());
    let retained_roots = collect_durable_cas_job_roots(directory.path());
    assert!(!retained_roots.object_hashes.is_empty());
    let retained_object_hash = retained_roots.object_hashes.iter().next().unwrap().clone();
    let retained_object_size = cas.stat_object(&retained_object_hash).unwrap().unwrap();
    assert!(retained_roots.object_hashes.contains(&retained_object_hash));
    assert!(retained_roots
        .blockers
        .contains(&format!("job-pin-unsealed:{crashed_job_id}")));
    drop(retained_old_job);
    let mut sealed_retry_job = DurableCasJob::open(directory.path(), &crashed_job_id).unwrap();
    let mut sealed_retry_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &retry_remote.manifest.generation,
    )
    .unwrap();
    for object in &retry_remote.manifest.objects {
        match cas.stat_object(&object.hash).unwrap() {
            Some(size) => sealed_retry_job
                .pin_existing(&cas, &object.hash, size, CasObjectRole::DirectObject)
                .unwrap(),
            None => {
                let mut reader = sealed_retry_source
                    .open_object(&LogicalDeltaObject {
                        hash: object.hash.clone(),
                        size: object.size,
                    })
                    .unwrap();
                let prepared = sealed_retry_job
                    .prepare_reader(&cas, &mut reader, CasObjectRole::DirectObject)
                    .unwrap();
                assert_eq!(prepared.content_hash, object.hash);
                assert_eq!(prepared.byte_size, object.size);
            }
        }
    }
    let mut seal_store = PersistentStore::open(directory.path()).unwrap();
    sealed_retry_job.seal(&mut seal_store, 0).unwrap();
    drop(seal_store);
    let sealed_roots = sealed_retry_job.root_set().unwrap();
    assert!(sealed_roots
        .object_hashes
        .contains(&retry_remote.manifest_hash));
    assert!(!collect_durable_cas_job_roots(directory.path())
        .blockers
        .contains(&format!("job-pin-unsealed:{crashed_job_id}")));
    retry_host.stop().unwrap();
    let observer_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &retry_remote.manifest.generation,
    )
    .unwrap();
    let observer_prepared = super::super::lan::PreparedLogicalLanSession::new(
        &uuid::Uuid::new_v4().to_string(),
        target_device_id,
        retry_remote.manifest_hash.clone(),
        retry_remote.manifest_bytes.clone(),
        observer_source.objects().to_vec(),
        Box::new(observer_source),
    )
    .unwrap();
    retry_host = LanCloneHost::prepare_logical(observer_prepared);
    let observer_pairing = retry_host.start().unwrap();
    request.source_endpoint = format!("http://127.0.0.1:{}", retry_host.address().unwrap().port());
    request.source_session_id = observer_pairing.session_id;
    request.source_manifest_id = observer_pairing.manifest_id;
    request.source_claim = observer_pairing.claim;

    SOURCE_AFTER_PREPARED_CALLBACK_FAILPOINT.with(|enabled| enabled.set(true));
    let observer_error = recovery_control
        .remote_apply(session.clone(), request.clone(), &NeverCancelled)
        .unwrap_err();
    assert!(
        matches!(&observer_error, PeerSyncError::Storage(message) if message.contains("pre-activation")),
        "{observer_error:?}"
    );
    assert_eq!(
        durable_job_journal_ids(directory.path()),
        vec![crashed_job_id.clone()]
    );
    assert!(DurableCasJob::open(directory.path(), &crashed_job_id)
        .unwrap()
        .is_sealed());
    retry_host.stop().unwrap();
    let completion_source = LogicalDeltaSourceSession::open(
        PersistentStore::open(remote_directory.path()).unwrap(),
        remote_directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &retry_remote.manifest.generation,
    )
    .unwrap();
    let completion_prepared = super::super::lan::PreparedLogicalLanSession::new(
        &uuid::Uuid::new_v4().to_string(),
        target_device_id,
        retry_remote.manifest_hash.clone(),
        retry_remote.manifest_bytes.clone(),
        completion_source.objects().to_vec(),
        Box::new(completion_source),
    )
    .unwrap();
    retry_host = LanCloneHost::prepare_logical(completion_prepared);
    let completion_pairing = retry_host.start().unwrap();
    request.source_endpoint = format!("http://127.0.0.1:{}", retry_host.address().unwrap().port());
    request.source_session_id = completion_pairing.session_id;
    request.source_manifest_id = completion_pairing.manifest_id;
    request.source_claim = completion_pairing.claim;

    BACKUP_FULL_VERIFICATION_COUNT.with(|count| count.set(0));
    SOURCE_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.set(true));
    SOURCE_COMPLETE_STORE_FAILPOINT.with(|enabled| enabled.set(true));
    let error = recovery_control
        .remote_apply(session.clone(), request.clone(), &NeverCancelled)
        .unwrap_err();
    assert!(SOURCE_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.replace(false)));
    assert!(
        matches!(&error, PeerSyncError::Storage(message) if message.contains("completion")),
        "{error:?}"
    );
    assert_eq!(BACKUP_FULL_VERIFICATION_COUNT.with(Cell::get), 1);
    let prepared = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let evidence = SourcePreparedEvidence::from_operation(&prepared).unwrap();
    let committed_job_id = match &prepared {
        PeerBidirectionalDurableOperation::SourcePrepared { durable_job_id, .. } => {
            durable_job_id.clone()
        }
        other => panic!("expected source-prepared completion journal, got {other:?}"),
    };
    assert_eq!(committed_job_id, crashed_job_id);
    assert!(evidence.transferred_objects > 0);
    assert!(evidence.transferred_bytes > 0);
    assert!(evidence.backup.is_some());
    let expected = evidence.receipt().unwrap();
    assert!(matches!(
        DurableCasJob::open(directory.path(), &crashed_job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
    let mut committed_source = PersistentStore::open(directory.path()).unwrap();
    let mut retained_committed_job = DurableCasJob::begin(
        directory.path(),
        &committed_job_id,
        CasJobKind::LogicalDeltaTarget,
        0,
    )
    .unwrap();
    retained_committed_job
        .pin_existing(
            &cas,
            &retained_object_hash,
            retained_object_size,
            CasObjectRole::DirectObject,
        )
        .unwrap();
    retained_committed_job
        .seal(&mut committed_source, 0)
        .unwrap();
    assert!(collect_durable_cas_job_roots(directory.path())
        .object_hashes
        .contains(&retained_object_hash));
    let retained_backup_path = PathBuf::from(&evidence.backup.as_ref().unwrap().path);
    let retained_backup_bytes = fs::read(&retained_backup_path).unwrap();
    fs::write(&retained_backup_path, b"corrupt retained source backup").unwrap();
    assert!(recovery_control
        .remote_apply(session.clone(), request.clone(), &NeverCancelled)
        .is_err());
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::SourcePrepared { .. })
    ));
    let retained_job = DurableCasJob::open(directory.path(), &committed_job_id).unwrap();
    assert!(retained_job.is_sealed());
    assert!(!retained_job.is_released());
    drop(retained_job);
    fs::write(&retained_backup_path, retained_backup_bytes).unwrap();
    PeerBidirectionalCommandState::default()
        .acknowledge(
            directory.path(),
            &mut committed_source,
            &request.operation_id,
        )
        .unwrap();
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::Completed {
            remote_apply_receipt: Some(ref receipt),
            source_binding: Some(_),
            ..
        }) if receipt == &expected
    ));
    assert!(matches!(
        DurableCasJob::open(directory.path(), &committed_job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
    drop(committed_source);
    retry_host.stop().unwrap();
    drop(recovery_control);

    let mut source_store = PersistentStore::open(directory.path()).unwrap();
    source_store
        .commit(&WorkingSetCommit {
            expected_revision: expected.committed_revision,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: Some(vec![crate::persistent_store::PluginStorageMutation::Set {
                key: "source-after-ack".to_owned(),
                value: json!({"preserved": true}),
            }]),
            asset_owner_heads: None,
        })
        .unwrap();
    let source_active = source_store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let refreshed_source_revision = source_store.revision().unwrap();
    let source = LogicalDeltaSourceSession::open(
        PersistentStore::open(directory.path()).unwrap(),
        directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &source_active.manifest.generation,
    )
    .unwrap();
    let (prepared, fresh_session_id, fresh_manifest_id) = prepare_product_source_session(
        directory.path(),
        source_store,
        source,
        source_device_id,
        source_active.manifest_bytes,
    )
    .unwrap();
    let mut source_host = LanCloneHost::prepare_bidirectional_logical(prepared);
    let pairing = source_host.start().unwrap();
    assert_eq!(pairing.session_id, fresh_session_id);
    assert_eq!(pairing.manifest_id, fresh_manifest_id);
    let source_endpoint = format!("http://127.0.0.1:{}", source_host.address().unwrap().port());
    let client = LanBidirectionalLogicalClient::claim(
        &source_endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
        target_device_id,
    )
    .unwrap();
    assert_eq!(client.source_device_id(), source_device_id);

    let target_cas = PayloadCas::new(remote_directory.path()).unwrap();
    let mut target_store = PersistentStore::open(remote_directory.path()).unwrap();
    let target_active = target_store
        .seal_or_initialize_active_logical_generation(&target_cas)
        .unwrap();
    let target_revision = target_store.revision().unwrap();
    target_store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                source_device_id,
                common.clone(),
                0,
            )
            .unwrap(),
            target_revision,
        )
        .unwrap();
    let target_previous = target_store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, source_device_id)
        .unwrap();
    let shared = SyncGenerationIdentity {
        generation_id: target_active.manifest.generation,
        manifest_hash: target_active.manifest_hash,
        generation_sequence: target_active.manifest.generation_sequence,
    };
    let operation_id = request.operation_id.clone();
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174110";
    let mut completed_job = DurableCasJob::begin(
        remote_directory.path(),
        durable_job_id,
        CasJobKind::LogicalDeltaTarget,
        0,
    )
    .unwrap();
    completed_job.release(CasReleaseOutcome::Aborted).unwrap();
    PeerBidirectionalOperationJournal::new(remote_directory.path())
        .store(&PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: PeerBidirectionalOperationContext {
                operation_id: operation_id.clone(),
                credential: client.credential(),
                library_id: PRODUCT_LOGICAL_LIBRARY_ID.to_owned(),
                expected_local_revision: target_revision,
                expected_remote_revision: 1,
                expected_remote_generation: request.expected_source_generation.clone(),
                previous_shared: common,
                previous_local: target_previous.local_identity,
                durable_job_id: durable_job_id.to_owned(),
                completion_mode: PeerBidirectionalCompletionMode::Legacy,
                completion_delivery: None,
            },
            committed_revision: target_revision,
            shared_generation: shared.clone(),
            changed: true,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        })
        .unwrap();
    target_store
        .commit(&WorkingSetCommit {
            expected_revision: target_revision,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: Some(vec![crate::persistent_store::PluginStorageMutation::Set {
                key: "target-after-response-loss".to_owned(),
                value: json!({"preserved": true}),
            }]),
            asset_owner_heads: None,
        })
        .unwrap();
    target_store
        .seal_or_initialize_active_logical_generation(&target_cas)
        .unwrap();
    let refreshed_target_revision = target_store.revision().unwrap();

    DISCOVER_LAN_IPV4_OVERRIDE.with(|address| address.set(Some(Ipv4Addr::LOCALHOST)));
    assert!(matches!(
        run_retained_remote_completion(
            &mut target_store,
            remote_directory.path(),
            &operation_id,
            refreshed_target_revision,
            Some(&client),
        )
        .unwrap(),
        PeerBidirectionalSyncResult::SourceUnavailable { .. }
    ));
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::Completed {
            remote_apply_receipt: Some(ref receipt),
            source_binding: Some(_),
            ..
        }) if receipt == &expected
    ));
    let target_journal = PeerBidirectionalOperationJournal::new(remote_directory.path());
    let mut retained_target = target_journal.load().unwrap().unwrap();
    let PeerBidirectionalDurableOperation::LocalCommitted {
        remote_backup_required,
        ..
    } = &mut retained_target
    else {
        panic!("expected retained target operation after source request mismatch");
    };
    *remote_backup_required = true;
    target_journal.store(&retained_target).unwrap();
    DISCOVER_LAN_IPV4_OVERRIDE.with(|address| address.set(Some(Ipv4Addr::LOCALHOST)));
    let result = run_retained_remote_completion(
        &mut target_store,
        remote_directory.path(),
        &operation_id,
        refreshed_target_revision,
        Some(&client),
    )
    .unwrap();
    let PeerBidirectionalSyncResult::Updated {
        revision,
        transferred_objects,
        transferred_bytes,
        backups,
        ..
    } = result
    else {
        panic!("expected updated retained completion");
    };
    assert_eq!(revision, refreshed_target_revision);
    assert_eq!(transferred_objects, expected.transferred_objects);
    assert_eq!(transferred_bytes, expected.transferred_bytes);
    assert_eq!(backups.len(), 1);
    assert_eq!(inspector.revision().unwrap(), refreshed_source_revision);
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::Completed {
            remote_apply_receipt: Some(receipt),
            source_binding: Some(_),
            result,
            ..
        }) if receipt == expected && result.revision == expected.committed_revision
    ));
    assert!(matches!(
        DurableCasJob::open(directory.path(), &committed_job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
    assert_eq!(
        target_store
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, source_device_id)
            .unwrap(),
        Some(shared)
    );
    source_host.stop().unwrap();
    let source_completed = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    assert!(retained_allows_source_prepare(
        &source_completed,
        source_device_id
    ));
    let completed_control = ProductionLanBidirectionalControl::new(
        directory.path().to_path_buf(),
        PersistentStore::open(directory.path()).unwrap(),
        source_session_id,
        source_device_id,
    );
    let completed_backup = match &source_completed {
        PeerBidirectionalDurableOperation::Completed {
            source_binding:
                Some(SourcePreparedEvidence {
                    backup: Some(backup),
                    ..
                }),
            ..
        } => backup.clone(),
        other => panic!("expected bound completed source backup, got {other:?}"),
    };
    let completed_backup_path = PathBuf::from(&completed_backup.path);
    let completed_backup_bytes = fs::read(&completed_backup_path).unwrap();
    fs::write(&completed_backup_path, b"corrupt completed source backup").unwrap();
    assert!(matches!(
        completed_control.remote_apply(session.clone(), request.clone(), &NeverCancelled),
        Err(PeerSyncError::Storage(_))
    ));
    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(source_completed.clone())
    );
    fs::write(&completed_backup_path, &completed_backup_bytes).unwrap();
    BACKUP_FULL_VERIFICATION_COUNT.with(|count| count.set(0));
    assert_eq!(
        completed_control
            .remote_apply(session.clone(), request.clone(), &NeverCancelled)
            .unwrap(),
        expected
    );
    assert_eq!(BACKUP_FULL_VERIFICATION_COUNT.with(Cell::get), 1);
    fs::remove_file(&completed_backup_path).unwrap();
    assert!(matches!(
        completed_control.remote_apply(session.clone(), request.clone(), &NeverCancelled),
        Err(PeerSyncError::Storage(_))
    ));
    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(source_completed.clone())
    );
    fs::write(&completed_backup_path, &completed_backup_bytes).unwrap();
    assert_eq!(
        completed_control
            .remote_apply(session.clone(), request.clone(), &NeverCancelled)
            .unwrap(),
        expected
    );
    let mut wrong_path_completed = source_completed.clone();
    let wrong_path = directory
        .path()
        .join("wrong-completed-backup.risulossless")
        .to_string_lossy()
        .into_owned();
    let PeerBidirectionalDurableOperation::Completed {
        remote_apply_receipt:
            Some(LanBidirectionalRemoteApplyReceipt {
                backup: Some(receipt_backup),
                ..
            }),
        source_binding:
            Some(SourcePreparedEvidence {
                backup: Some(binding_backup),
                ..
            }),
        result,
        ..
    } = &mut wrong_path_completed
    else {
        panic!("expected bound completed source backup");
    };
    receipt_backup.path = wrong_path.clone();
    binding_backup.path = wrong_path.clone();
    result.backups[0].path = wrong_path;
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&wrong_path_completed)
        .unwrap();
    assert!(matches!(
        completed_control.remote_apply(session.clone(), request.clone(), &NeverCancelled),
        Err(PeerSyncError::Storage(_))
    ));
    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(wrong_path_completed)
    );
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&source_completed)
        .unwrap();
    assert_eq!(
        completed_control
            .remote_apply(session, request, &NeverCancelled)
            .unwrap(),
        expected
    );
    PeerBidirectionalOperationJournal::new(directory.path())
        .acknowledge_completed(&operation_id)
        .unwrap();
    PeerBidirectionalOperationJournal::new(remote_directory.path())
        .acknowledge_completed(&operation_id)
        .unwrap();
}

#[test]
fn command_state_excludes_source_preparation_and_target_work() {
    let state = PeerBidirectionalCommandState::default();

    let source = state.begin_source_prepare().unwrap();
    assert!(matches!(
        state.begin_target(),
        Err(PeerSyncError::Protocol(message)) if message.contains("source")
    ));
    drop(source);

    let target = state.begin_target().unwrap();
    assert!(matches!(
        state.begin_source_prepare(),
        Err(PeerSyncError::Protocol(message)) if message.contains("target")
    ));
    drop(target);

    assert!(state.begin_source_prepare().is_ok());
}

#[test]
fn bidirectional_restart_uses_the_canonical_source_identity_for_durable_checks() {
    let directory = tempfile::tempdir().unwrap();
    let canonical =
        super::super::device_registry::load_or_create_device_id(directory.path()).unwrap();
    let legacy = directory.path().join("peer-delta").join("source-device-id");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, "123e4567-e89b-42d3-a456-426614174199").unwrap();
    fs::remove_file(&legacy).unwrap();
    let source_device_id =
        super::super::delta_commands::canonical_source_device_id(directory.path()).unwrap();
    let operation = PeerBidirectionalDurableOperation::SourcePrepared {
        schema: OPERATION_SCHEMA.to_owned(),
        operation_id: "123e4567-e89b-42d3-a456-426614174200".to_owned(),
        source_device_id: source_device_id.clone(),
        target_device_id: "123e4567-e89b-42d3-a456-426614174201".to_owned(),
        expected_source_revision: 0,
        previous_shared: generation("previous", "0", 'a'),
        expected_source_generation: generation("source", "0", 'b'),
        shared_generation: LanBidirectionalGeneration {
            generation_id: "shared".to_owned(),
            manifest_hash: "c".repeat(64),
            generation_sequence: "1".to_owned(),
        },
        incoming_revision: 1,
        transferred_objects: 0,
        transferred_bytes: 0,
        backup_required: false,
        backup: None,
        completion_deferred_v1: false,
        durable_job_id: "123e4567-e89b-42d3-a456-426614174202".to_owned(),
    };

    let restarted =
        super::super::delta_commands::canonical_source_device_id(directory.path()).unwrap();
    assert_eq!(source_device_id, canonical);
    assert_eq!(restarted, canonical);
    assert!(retained_allows_source_prepare(&operation, &restarted));
    assert!(!legacy.exists());
}

#[test]
fn source_prepared_status_exposes_the_recoverable_operation() {
    let operation_id = "123e4567-e89b-42d3-a456-426614174120";
    let operation = PeerBidirectionalDurableOperation::SourcePrepared {
        schema: OPERATION_SCHEMA.to_owned(),
        operation_id: operation_id.to_owned(),
        source_device_id: "123e4567-e89b-42d3-a456-426614174121".to_owned(),
        target_device_id: "123e4567-e89b-42d3-a456-426614174122".to_owned(),
        expected_source_revision: 0,
        previous_shared: generation("previous", "0", 'a'),
        expected_source_generation: generation("source", "0", 'b'),
        shared_generation: LanBidirectionalGeneration {
            generation_id: "shared".to_owned(),
            manifest_hash: "c".repeat(64),
            generation_sequence: "1".to_owned(),
        },
        incoming_revision: 1,
        transferred_objects: 2,
        transferred_bytes: 19,
        backup_required: false,
        backup: None,
        completion_deferred_v1: false,
        durable_job_id: "123e4567-e89b-42d3-a456-426614174123".to_owned(),
    };

    assert_eq!(
        serde_json::to_value(operation.status_projection()).unwrap(),
        json!({
            "phase": "sourcePrepared",
            "operationId": operation_id,
        })
    );
}

fn invalid_target_prepared_journal_bytes(operation_id: &str) -> Vec<u8> {
    let operation = PeerBidirectionalDurableOperation::TargetPrepared {
        schema: OPERATION_SCHEMA.to_owned(),
        context: context(operation_id),
        local_generation: generation("local", "3", 'e'),
        conflict_policy: TargetPreparedConflictPolicy::Reject,
        changed: true,
        remote_backup_required: false,
        transferred_objects: 2,
        transferred_bytes: 19,
        backups: vec![],
    };
    let mut value = serde_json::to_value(operation).unwrap();
    value["context"]["durableJobId"] = json!("../../untrusted-job");
    serde_json::to_vec(&value).unwrap()
}

fn write_operation_journal(root: &Path, bytes: &[u8]) -> PathBuf {
    let journal = PeerBidirectionalOperationJournal::new(root);
    fs::create_dir_all(&journal.root).unwrap();
    let path = journal.root.join(OPERATION_FILE);
    fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn acknowledge_abandons_only_a_semantically_invalid_retained_journal() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174240";
    let bytes = invalid_target_prepared_journal_bytes(operation_id);
    let operation_path = write_operation_journal(directory.path(), &bytes);
    let sibling = operation_path.parent().unwrap().join("keep.marker");
    fs::write(&sibling, b"unrelated retained state").unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let state = PeerBidirectionalCommandState::default();

    assert!(state.status(directory.path(), &mut store).is_err());
    assert_eq!(fs::read(&operation_path).unwrap(), bytes);

    state
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();

    assert!(!operation_path.exists());
    assert_eq!(fs::read(sibling).unwrap(), b"unrelated retained state");
}

#[test]
fn invalid_retained_journal_abandon_requires_the_exact_operation_id() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174241";
    let bytes = invalid_target_prepared_journal_bytes(operation_id);
    let operation_path = write_operation_journal(directory.path(), &bytes);
    let mut store = PersistentStore::open(directory.path()).unwrap();

    assert!(PeerBidirectionalCommandState::default()
        .acknowledge(
            directory.path(),
            &mut store,
            "123e4567-e89b-42d3-a456-426614174242",
        )
        .is_err());
    assert_eq!(fs::read(operation_path).unwrap(), bytes);
}

#[test]
fn invalid_retained_journal_abandon_rejects_malformed_and_oversized_records() {
    let operation_id = "123e4567-e89b-42d3-a456-426614174243";
    for bytes in [b"{".to_vec(), vec![b' '; MAX_OPERATION_BYTES as usize + 1]] {
        let directory = tempfile::tempdir().unwrap();
        let operation_path = write_operation_journal(directory.path(), &bytes);
        let mut store = PersistentStore::open(directory.path()).unwrap();

        assert!(PeerBidirectionalCommandState::default()
            .acknowledge(directory.path(), &mut store, operation_id)
            .is_err());
        assert_eq!(fs::read(operation_path).unwrap(), bytes);
    }
}

#[test]
fn invalid_retained_journal_abandon_rejects_a_symlink() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174244";
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    fs::create_dir_all(&journal.root).unwrap();
    let target = directory.path().join("outside-operation.json");
    let bytes = invalid_target_prepared_journal_bytes(operation_id);
    fs::write(&target, &bytes).unwrap();
    let operation_path = journal.root.join(OPERATION_FILE);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &operation_path).unwrap();
    #[cfg(windows)]
    if let Err(error) = std::os::windows::fs::symlink_file(&target, &operation_path) {
        if error.raw_os_error() == Some(1314) {
            return;
        }
        panic!("failed to create linked journal fixture: {error}");
    }
    let mut store = PersistentStore::open(directory.path()).unwrap();

    assert!(PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .is_err());
    assert!(fs::symlink_metadata(operation_path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(target).unwrap(), bytes);
}

#[test]
fn invalid_retained_journal_abandon_rejects_an_active_target() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174245";
    let bytes = invalid_target_prepared_journal_bytes(operation_id);
    let operation_path = write_operation_journal(directory.path(), &bytes);
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let state = PeerBidirectionalCommandState::default();
    let _active_target = state.begin_target().unwrap();

    assert!(matches!(
        state.acknowledge(directory.path(), &mut store, operation_id),
        Err(PeerSyncError::Protocol(message)) if message.contains("target operation is active")
    ));
    assert_eq!(fs::read(operation_path).unwrap(), bytes);
}

#[test]
fn tolerant_abandon_review_propagates_a_transient_strict_load_failure() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174246";
    let bytes = invalid_target_prepared_journal_bytes(operation_id);
    let operation_path = write_operation_journal(directory.path(), &bytes);
    let mut store = PersistentStore::open(directory.path()).unwrap();
    OPERATION_READ_FAILPOINT.with(|enabled| enabled.set(true));

    assert!(PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .is_err());
    assert_eq!(fs::read(operation_path).unwrap(), bytes);
}

#[test]
fn tolerant_abandon_review_rejects_a_valid_replacement_race() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174247";
    let invalid = invalid_target_prepared_journal_bytes(operation_id);
    let operation_path = write_operation_journal(directory.path(), &invalid);
    let mut valid_context = context(operation_id);
    valid_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    valid_context.credential.manifest_id = valid_context
        .expected_remote_generation
        .manifest_hash
        .clone();
    let valid = serde_json::to_vec(&PeerBidirectionalDurableOperation::TargetPrepared {
        schema: OPERATION_SCHEMA.to_owned(),
        context: valid_context,
        local_generation: generation("local", "3", 'e'),
        conflict_policy: TargetPreparedConflictPolicy::Reject,
        changed: true,
        remote_backup_required: false,
        transferred_objects: 2,
        transferred_bytes: 19,
        backups: vec![],
    })
    .unwrap();
    ABANDON_BEFORE_CLAIM_REPLACEMENT.with(|replacement| replacement.replace(Some(valid.clone())));
    let mut store = PersistentStore::open(directory.path()).unwrap();

    assert!(PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .is_err());
    assert_eq!(fs::read(operation_path).unwrap(), valid);
}

#[test]
fn tolerant_abandon_review_restores_after_claim_cleanup_failure() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174248";
    let bytes = invalid_target_prepared_journal_bytes(operation_id);
    let operation_path = write_operation_journal(directory.path(), &bytes);
    let mut store = PersistentStore::open(directory.path()).unwrap();
    ABANDON_CLAIM_REMOVE_FAILPOINT.with(|enabled| enabled.set(true));

    assert!(PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .is_err());
    assert_eq!(fs::read(operation_path).unwrap(), bytes);
}

fn abandon_claim_paths(root: &Path) -> Vec<PathBuf> {
    let journal_root = root.join("peer-bidirectional");
    let Ok(entries) = fs::read_dir(journal_root) else {
        return vec![];
    };
    entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name == ".operation-abandon.claim"
                        || name.starts_with(".operation-abandon-") && name.ends_with(".claim")
                })
        })
        .collect()
}

#[test]
fn tolerant_abandon_crash_reopen_restores_a_valid_replacement_claim() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174249";
    let invalid = invalid_target_prepared_journal_bytes(operation_id);
    write_operation_journal(directory.path(), &invalid);
    let mut valid_context = context(operation_id);
    valid_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
    valid_context.credential.manifest_id = valid_context
        .expected_remote_generation
        .manifest_hash
        .clone();
    let valid_operation = PeerBidirectionalDurableOperation::TargetPrepared {
        schema: OPERATION_SCHEMA.to_owned(),
        context: valid_context,
        local_generation: generation("local", "3", 'e'),
        conflict_policy: TargetPreparedConflictPolicy::Reject,
        changed: true,
        remote_backup_required: false,
        transferred_objects: 2,
        transferred_bytes: 19,
        backups: vec![],
    };
    let valid = serde_json::to_vec(&valid_operation).unwrap();
    ABANDON_BEFORE_CLAIM_REPLACEMENT.with(|replacement| replacement.replace(Some(valid.clone())));
    ABANDON_AFTER_CLAIM_FAILPOINT.with(|enabled| enabled.set(true));
    let mut store = PersistentStore::open(directory.path()).unwrap();

    assert!(PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .is_err());
    assert!(!directory
        .path()
        .join("peer-bidirectional")
        .join(OPERATION_FILE)
        .exists());
    assert_eq!(abandon_claim_paths(directory.path()).len(), 1);

    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(valid_operation)
    );
    assert!(abandon_claim_paths(directory.path()).is_empty());
}

#[test]
fn tolerant_abandon_crash_reopen_restores_invalid_claim_for_explicit_retry() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174250";
    let invalid = invalid_target_prepared_journal_bytes(operation_id);
    let operation_path = write_operation_journal(directory.path(), &invalid);
    ABANDON_AFTER_CLAIM_FAILPOINT.with(|enabled| enabled.set(true));
    let mut store = PersistentStore::open(directory.path()).unwrap();

    assert!(PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .is_err());
    assert!(!operation_path.exists());

    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .is_err());
    assert_eq!(fs::read(&operation_path).unwrap(), invalid);
    assert!(abandon_claim_paths(directory.path()).is_empty());

    PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();
    assert!(!operation_path.exists());
}

#[test]
fn tolerant_abandon_claim_recovery_never_overwrites_a_concurrent_journal() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174251";
    let canonical = invalid_target_prepared_journal_bytes(operation_id);
    let operation_path = write_operation_journal(directory.path(), &canonical);
    let claim = operation_path
        .parent()
        .unwrap()
        .join(".operation-abandon.claim");
    let claimed = invalid_target_prepared_journal_bytes("123e4567-e89b-42d3-a456-426614174252");
    fs::write(&claim, &claimed).unwrap();

    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .is_err());
    assert_eq!(fs::read(operation_path).unwrap(), canonical);
    assert_eq!(fs::read(claim).unwrap(), claimed);
}

#[test]
fn tolerant_abandon_claim_recovery_rejects_unbounded_or_linked_claims() {
    let operation_id = "123e4567-e89b-42d3-a456-426614174253";
    let target_bytes = invalid_target_prepared_journal_bytes(operation_id);
    for linked in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        fs::create_dir_all(&journal.root).unwrap();
        let claim = journal.root.join(".operation-abandon.claim");
        if linked {
            let target = directory.path().join("outside-operation.json");
            fs::write(&target, &target_bytes).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &claim).unwrap();
            #[cfg(windows)]
            if let Err(error) = std::os::windows::fs::symlink_file(&target, &claim) {
                if error.raw_os_error() == Some(1314) {
                    continue;
                }
                panic!("failed to create linked claim fixture: {error}");
            }
        } else {
            fs::write(&claim, vec![b' '; MAX_OPERATION_BYTES as usize + 1]).unwrap();
        }

        assert!(journal.load().is_err());
        assert!(fs::symlink_metadata(claim).is_ok());
    }
}

#[test]
fn acknowledge_aborts_exact_source_precommit_and_releases_its_job() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174124";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174125";
    let (mut store, _cas, _evidence, _previous) = retain_source_prepared_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174126",
        "123e4567-e89b-42d3-a456-426614174127",
        durable_job_id,
    );

    PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();

    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert_eq!(
        DurableCasJob::open(directory.path(), durable_job_id)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn acknowledge_removes_only_the_exact_source_backup_staging_tree() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174218";
    let sibling_operation_id = "123e4567-e89b-42d3-a456-426614174219";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174220";
    let (mut store, _cas, mut evidence, _previous) = retain_source_prepared_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174221",
        "123e4567-e89b-42d3-a456-426614174222",
        durable_job_id,
    );
    evidence.backup_required = true;
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&evidence.durable_operation(durable_job_id.to_owned()))
        .unwrap();
    let (owned_staging, owned_temporary) = bidirectional_backup_staging_paths(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    let lossless_staging = owned_staging.join("lossless-v1");
    fs::create_dir_all(&lossless_staging).unwrap();
    fs::write(&owned_temporary, b"partial archive").unwrap();
    fs::write(
        lossless_staging.join("123e4567-e89b-42d3-a456-426614174223.database"),
        b"large database debris",
    )
    .unwrap();
    fs::write(
        lossless_staging.join("123e4567-e89b-42d3-a456-426614174224.owner-empty"),
        b"owner debris",
    )
    .unwrap();
    let (sibling_staging, _) = bidirectional_backup_staging_paths(
        directory.path(),
        sibling_operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    fs::create_dir_all(&sibling_staging).unwrap();
    let sibling_marker = sibling_staging.join("keep.marker");
    fs::write(&sibling_marker, b"sibling operation").unwrap();
    let final_backup = bidirectional_backup_path(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    fs::create_dir_all(final_backup.parent().unwrap()).unwrap();
    fs::write(&final_backup, b"owned final").unwrap();

    PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();

    assert!(!owned_staging.exists());
    assert!(!final_backup.exists());
    assert_eq!(fs::read(sibling_marker).unwrap(), b"sibling operation");
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert!(matches!(
        DurableCasJob::open(directory.path(), durable_job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn source_backup_staging_cleanup_failure_preserves_journal_for_retry() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174225";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174226";
    let (mut store, _cas, mut evidence, _previous) = retain_source_prepared_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174227",
        "123e4567-e89b-42d3-a456-426614174228",
        durable_job_id,
    );
    evidence.backup_required = true;
    let retained = evidence.durable_operation(durable_job_id.to_owned());
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&retained).unwrap();
    let (owned_staging, _) = bidirectional_backup_staging_paths(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    fs::create_dir_all(owned_staging.parent().unwrap()).unwrap();
    fs::write(&owned_staging, b"not an owned directory").unwrap();
    let final_backup = bidirectional_backup_path(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    fs::create_dir_all(final_backup.parent().unwrap()).unwrap();
    fs::write(&final_backup, b"owned final").unwrap();

    assert!(matches!(
        PeerBidirectionalCommandState::default().acknowledge(
            directory.path(),
            &mut store,
            operation_id,
        ),
        Err(PeerSyncError::Storage(_))
    ));
    assert_eq!(journal.load().unwrap(), Some(retained));
    assert!(final_backup.is_file());

    fs::remove_file(&owned_staging).unwrap();
    PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();
    assert!(journal.load().unwrap().is_none());
    assert!(!final_backup.exists());
}

#[test]
fn revoked_target_ack_still_allows_source_precommit_abandon() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174210";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174211";
    let target_device_id = "123e4567-e89b-42d3-a456-426614174212";
    let (mut store, _cas, _evidence, previous) = retain_source_prepared_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174213",
        target_device_id,
        durable_job_id,
    );
    store
        .revoke_sync_device(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id, &previous)
        .unwrap();
    assert!(store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
        .is_err());

    let state = PeerBidirectionalCommandState::default();
    let status = state.status(directory.path(), &mut store).unwrap();
    assert!(matches!(
        status.operation,
        Some(PeerBidirectionalStatusOperation::SourcePrepared {
            ref operation_id
        }) if operation_id == "123e4567-e89b-42d3-a456-426614174210"
    ));
    state
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();

    assert_eq!(store.revision().unwrap(), 0);
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert!(matches!(
        DurableCasJob::open(directory.path(), durable_job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn acknowledge_after_restart_aborts_source_precommit_descendant_without_losing_edit() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174128";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174129";
    let (mut store, cas, _evidence, previous) = retain_source_prepared_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174130",
        "123e4567-e89b-42d3-a456-426614174131",
        durable_job_id,
    );
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"preserved": "local-descendant"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    drop(store);
    let mut reopened = PersistentStore::open(directory.path()).unwrap();

    assert_eq!(
        serde_json::to_value(
            PeerBidirectionalCommandState::default()
                .status(directory.path(), &mut reopened)
                .unwrap()
                .operation,
        )
        .unwrap(),
        json!({
            "phase": "sourcePrepared",
            "operationId": operation_id,
        })
    );

    PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut reopened, operation_id)
        .unwrap();

    assert_eq!(reopened.revision().unwrap(), 1);
    assert_eq!(
        reopened.read_root(None).unwrap().value["preserved"],
        "local-descendant"
    );
    assert_eq!(
        reopened
            .sync_device_ack_state(
                PRODUCT_LOGICAL_LIBRARY_ID,
                "123e4567-e89b-42d3-a456-426614174131",
            )
            .unwrap()
            .shared_identity,
        previous
    );
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert_eq!(
        DurableCasJob::open(directory.path(), durable_job_id)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn acknowledge_after_source_postcommit_promotes_exact_receipt_until_second_ack() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174132";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174133";
    let target_device_id = "123e4567-e89b-42d3-a456-426614174134";
    let (store, _cas, evidence, expected_receipt) = retain_source_postcommit_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174135",
        target_device_id,
        durable_job_id,
    );
    drop(store);
    let mut reopened = PersistentStore::open(directory.path()).unwrap();
    let state = PeerBidirectionalCommandState::default();

    let status = state.status(directory.path(), &mut reopened).unwrap();

    assert!(matches!(
        status.operation,
        Some(PeerBidirectionalStatusOperation::Completed { ref result })
            if result.revision == 1 && result.remote_revision == 1
    ));
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::Completed {
            remote_apply_receipt: Some(receipt),
            source_binding: Some(binding),
            result,
            ..
        }) if receipt == expected_receipt
            && binding == evidence
            && result.revision == 1
            && result.remote_revision == 1
    ));
    assert_eq!(
        DurableCasJob::open(directory.path(), durable_job_id)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    state
        .acknowledge(directory.path(), &mut reopened, operation_id)
        .unwrap();
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
}

#[test]
fn revoked_target_ack_still_promotes_exact_source_postcommit() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174214";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174215";
    let target_device_id = "123e4567-e89b-42d3-a456-426614174216";
    let (mut store, _cas, evidence, expected_receipt) = retain_source_postcommit_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174217",
        target_device_id,
        durable_job_id,
    );
    let shared = SyncGenerationIdentity {
        generation_id: evidence.shared_generation.generation_id.clone(),
        manifest_hash: evidence.shared_generation.manifest_hash.clone(),
        generation_sequence: evidence.shared_generation.generation_sequence.clone(),
    };
    store
        .revoke_sync_device(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id, &shared)
        .unwrap();
    assert!(store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
        .is_err());

    let status = PeerBidirectionalCommandState::default()
        .status(directory.path(), &mut store)
        .unwrap();

    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(store.read_root(None).unwrap().value["committed"], "shared");
    assert!(matches!(
        status.operation,
        Some(PeerBidirectionalStatusOperation::Completed { .. })
    ));
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::Completed {
            remote_apply_receipt: Some(receipt),
            source_binding: Some(binding),
            ..
        }) if receipt == expected_receipt && binding == evidence
    ));
    assert!(matches!(
        DurableCasJob::open(directory.path(), durable_job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn status_promotes_source_postcommit_with_a_later_local_descendant() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174148";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174149";
    let (mut store, cas, evidence, expected_receipt) = retain_source_postcommit_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174150",
        "123e4567-e89b-42d3-a456-426614174151",
        durable_job_id,
    );
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({"preserved": "postcommit-descendant"})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    drop(store);
    let mut reopened = PersistentStore::open(directory.path()).unwrap();

    let status = PeerBidirectionalCommandState::default()
        .status(directory.path(), &mut reopened)
        .unwrap();

    assert_eq!(reopened.revision().unwrap(), 2);
    assert_eq!(
        reopened.read_root(None).unwrap().value["preserved"],
        "postcommit-descendant"
    );
    assert!(matches!(
        status.operation,
        Some(PeerBidirectionalStatusOperation::Completed { ref result })
            if result.revision == 2 && result.remote_revision == 1
    ));
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::Completed {
            remote_apply_receipt: Some(receipt),
            source_binding: Some(binding),
            result,
            ..
        }) if receipt == expected_receipt
            && binding == evidence
            && result.revision == 2
    ));
    assert_eq!(
        DurableCasJob::open(directory.path(), durable_job_id)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn acknowledge_retains_source_prepared_when_durable_witnesses_are_mixed() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174152";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174153";
    let (mut store, _cas, mut evidence, _receipt) = retain_source_postcommit_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174154",
        "123e4567-e89b-42d3-a456-426614174155",
        durable_job_id,
    );
    evidence.shared_generation.manifest_hash = "0".repeat(64);
    let retained = evidence.durable_operation(durable_job_id.to_owned());
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&retained)
        .unwrap();

    assert!(matches!(
        PeerBidirectionalCommandState::default().acknowledge(
            directory.path(),
            &mut store,
            operation_id,
        ),
        Err(PeerSyncError::Validation(message)) if message.contains("mixed durable state")
    ));
    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(retained)
    );
    assert!(DurableCasJob::open(directory.path(), durable_job_id).is_ok());
}

#[test]
fn status_does_not_promote_source_postcommit_with_a_missing_physical_backup() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174156";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174157";
    let (mut store, _cas, mut evidence, _receipt) = retain_source_postcommit_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174158",
        "123e4567-e89b-42d3-a456-426614174159",
        durable_job_id,
    );
    let backup_path = bidirectional_backup_path(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    fs::create_dir_all(backup_path.parent().unwrap()).unwrap();
    fs::write(&backup_path, b"published backup").unwrap();
    evidence.backup = Some(LanBidirectionalBackupReceipt {
        package_id: "f".repeat(64),
        path: backup_path.to_string_lossy().into_owned(),
    });
    let retained = evidence.durable_operation(durable_job_id.to_owned());
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&retained).unwrap();
    fs::remove_file(backup_path).unwrap();

    assert!(PeerBidirectionalCommandState::default()
        .status(directory.path(), &mut store)
        .is_err());
    assert_eq!(journal.load().unwrap(), Some(retained));
    let job = DurableCasJob::open(directory.path(), durable_job_id).unwrap();
    assert!(job.is_sealed());
    assert!(!job.is_released());
}

#[test]
fn acknowledge_does_not_promote_source_postcommit_with_a_corrupt_physical_backup() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174160";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174161";
    let (mut store, _cas, mut evidence, _receipt) = retain_source_postcommit_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174162",
        "123e4567-e89b-42d3-a456-426614174163",
        durable_job_id,
    );
    let backup_path = bidirectional_backup_path(
        directory.path(),
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    fs::create_dir_all(backup_path.parent().unwrap()).unwrap();
    fs::write(&backup_path, b"corrupt physical backup").unwrap();
    evidence.backup = Some(LanBidirectionalBackupReceipt {
        package_id: "f".repeat(64),
        path: backup_path.to_string_lossy().into_owned(),
    });
    let retained = evidence.durable_operation(durable_job_id.to_owned());
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&retained).unwrap();

    assert!(PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .is_err());
    assert_eq!(journal.load().unwrap(), Some(retained));
    let job = DurableCasJob::open(directory.path(), durable_job_id).unwrap();
    assert!(job.is_sealed());
    assert!(!job.is_released());
}

#[test]
fn status_projects_source_prepared_while_source_runtime_is_active() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174164";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174165";
    let (mut store, _cas, evidence, _receipt) = retain_source_postcommit_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174166",
        "123e4567-e89b-42d3-a456-426614174167",
        durable_job_id,
    );
    let active = store
        .seal_or_initialize_active_logical_generation(&PayloadCas::new(directory.path()).unwrap())
        .unwrap();
    let source = LogicalDeltaSourceSession::open(
        PersistentStore::open(directory.path()).unwrap(),
        directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &active.manifest.generation,
    )
    .unwrap();
    let session_id = "123e4567-e89b-42d3-a456-426614174168";
    let prepared = super::super::lan::PreparedLogicalLanSession::new(
        session_id,
        "123e4567-e89b-42d3-a456-426614174169",
        active.manifest_hash.clone(),
        active.manifest_bytes,
        source.objects().to_vec(),
        Box::new(source),
    )
    .unwrap();
    let state = PeerBidirectionalCommandState::default();
    state
        .install_source(
            LanCloneHost::prepare_logical(prepared),
            session_id,
            &active.manifest_hash,
        )
        .unwrap();

    let status = state.status(directory.path(), &mut store).unwrap();

    assert!(matches!(
        status.operation,
        Some(PeerBidirectionalStatusOperation::SourcePrepared { ref operation_id })
            if operation_id == &evidence.operation_id
    ));
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::SourcePrepared { .. })
    ));
    let job = DurableCasJob::open(directory.path(), durable_job_id).unwrap();
    assert!(job.is_sealed());
    assert!(!job.is_released());
}

#[test]
fn status_projects_source_prepared_while_target_guard_is_active_then_promotes_once_idle() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174170";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174171";
    let (mut store, _cas, _evidence, _receipt) = retain_source_postcommit_fixture(
        directory.path(),
        operation_id,
        "123e4567-e89b-42d3-a456-426614174172",
        "123e4567-e89b-42d3-a456-426614174173",
        durable_job_id,
    );
    let state = PeerBidirectionalCommandState::default();
    let active_target = state.begin_target().unwrap();

    let active_status = state.status(directory.path(), &mut store).unwrap();
    assert!(matches!(
        active_status.operation,
        Some(PeerBidirectionalStatusOperation::SourcePrepared { .. })
    ));
    assert!(DurableCasJob::open(directory.path(), durable_job_id).is_ok());
    drop(active_target);

    let idle_status = state.status(directory.path(), &mut store).unwrap();
    assert!(matches!(
        idle_status.operation,
        Some(PeerBidirectionalStatusOperation::Completed { .. })
    ));
    assert!(matches!(
        DurableCasJob::open(directory.path(), durable_job_id),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn fresh_pairing_rebind_changes_only_awaiting_conflict_credential() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174136";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174137";
    let retained = awaiting_conflict_operation(operation_id, durable_job_id);
    let PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } = &retained else {
        unreachable!()
    };
    let mut fresh = context.credential.clone();
    fresh.endpoint = "http://192.168.1.9:32146".to_owned();
    fresh.session_id = "123e4567-e89b-42d3-a456-426614174138".to_owned();
    fresh.manifest_id = "9".repeat(64);
    fresh.bearer = "8".repeat(64);
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&retained)
        .unwrap();
    let expected_generation = context.expected_remote_generation.clone();

    let result = rebind_awaiting_conflict(
        directory.path(),
        retained.clone(),
        fresh.clone(),
        &context.library_id,
        context.expected_remote_revision,
        &expected_generation,
    )
    .unwrap();

    assert!(matches!(
        result,
        PeerBidirectionalSyncResult::Conflict { .. }
    ));
    let mut expected = retained;
    let PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } = &mut expected else {
        unreachable!()
    };
    context.credential = fresh;
    assert_eq!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(expected)
    );
}

#[test]
fn fresh_pairing_rebind_rejects_every_stable_identity_mismatch_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174139";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174140";
    let retained = awaiting_conflict_operation(operation_id, durable_job_id);
    let PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } = &retained else {
        unreachable!()
    };
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&retained).unwrap();
    let good_credential = context.credential.clone();
    let good_generation = context.expected_remote_generation.clone();
    let mut cases = Vec::new();
    let mut wrong_target = good_credential.clone();
    wrong_target.device_id = "123e4567-e89b-42d3-a456-426614174141".to_owned();
    cases.push((
        wrong_target,
        context.library_id.clone(),
        context.expected_remote_revision,
        good_generation.clone(),
    ));
    let mut wrong_source = good_credential.clone();
    wrong_source.source_device_id = "123e4567-e89b-42d3-a456-426614174142".to_owned();
    cases.push((
        wrong_source,
        context.library_id.clone(),
        context.expected_remote_revision,
        good_generation.clone(),
    ));
    cases.push((
        good_credential.clone(),
        "other-library".to_owned(),
        context.expected_remote_revision,
        good_generation.clone(),
    ));
    cases.push((
        good_credential.clone(),
        context.library_id.clone(),
        context.expected_remote_revision + 1,
        good_generation.clone(),
    ));
    for field in ["generation", "hash", "sequence"] {
        let mut generation = good_generation.clone();
        match field {
            "generation" => generation.generation_id = "other-generation".to_owned(),
            "hash" => generation.manifest_hash = "0".repeat(64),
            "sequence" => generation.generation_sequence = "3".to_owned(),
            _ => unreachable!(),
        }
        cases.push((
            good_credential.clone(),
            context.library_id.clone(),
            context.expected_remote_revision,
            generation,
        ));
    }

    for (credential, library_id, revision, generation) in cases {
        assert!(rebind_awaiting_conflict(
            directory.path(),
            retained.clone(),
            credential,
            &library_id,
            revision,
            &generation,
        )
        .is_err());
        assert_eq!(journal.load().unwrap(), Some(retained.clone()));
    }
}

#[test]
fn public_conflict_fresh_link_resolves_each_winner_after_reopen_without_persisting_url() {
    for (winner, expected_root) in [
        (
            PeerBidirectionalConflictWinner::Local,
            lossless_root("Local"),
        ),
        (
            PeerBidirectionalConflictWinner::Remote,
            lossless_root("Remote"),
        ),
    ] {
        let (
            local_directory,
            remote_directory,
            local_cas,
            local_store,
            remote,
            original_credential,
            operation_id,
        ) = public_conflict_fixture();
        drop(local_store);
        let journal = PeerBidirectionalOperationJournal::new(local_directory.path());
        let retained = journal.load().unwrap().unwrap();
        let PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } = &retained else {
            panic!("expected retained conflict");
        };
        assert!(context.credential.endpoint.is_empty());
        assert!(context.credential.session_id.is_empty());
        assert!(context.credential.bearer.is_empty());
        let mut fresh = original_credential;
        fresh.endpoint = "https://fresh-conflict.example".to_owned();
        fresh.session_id = "123e4567-e89b-42d3-a456-426614174198".to_owned();
        fresh.bearer = "b".repeat(64);
        let mut store = PersistentStore::open(local_directory.path()).unwrap();
        let mut source = LogicalDeltaSourceSession::open(
            PersistentStore::open(remote_directory.path()).unwrap(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &remote.manifest.generation,
        )
        .unwrap();

        assert_eq!(
            resolve_awaiting_conflict_with_fresh_source(
                &mut store,
                &local_cas,
                local_directory.path(),
                &retained,
                &operation_id,
                winner,
                2,
                &fresh,
                &remote.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            LocalMergeOutcome::LocalCommitted
        );
        assert_eq!(store.read_root(None).unwrap().value, expected_root);
        assert!(matches!(
            journal.load().unwrap(),
            Some(PeerBidirectionalDurableOperation::LocalCommitted {
                context,
                remote_apply_receipt: None,
                ..
            }) if context.credential.endpoint.is_empty()
        ));
        let serialized = fs::read(
            local_directory
                .path()
                .join("peer-bidirectional")
                .join(OPERATION_FILE),
        )
        .unwrap();
        let serialized = String::from_utf8_lossy(&serialized);
        assert!(!serialized.contains("initial-public"));
        assert!(!serialized.contains("fresh-conflict"));
    }
}

#[test]
fn public_conflict_fresh_link_rejects_identity_mismatch_without_mutation() {
    let (
        local_directory,
        _remote_directory,
        _local_cas,
        local_store,
        remote,
        mut fresh,
        operation_id,
    ) = public_conflict_fixture();
    drop(local_store);
    let journal = PeerBidirectionalOperationJournal::new(local_directory.path());
    let retained = journal.load().unwrap().unwrap();
    fresh.endpoint = "https://fresh-mismatch.example".to_owned();
    fresh.source_device_id = "123e4567-e89b-42d3-a456-426614174189".to_owned();
    let decoded = decode_logical_manifest(&remote.manifest_bytes).unwrap();
    let generation = LanBidirectionalGeneration {
        generation_id: decoded.generation,
        manifest_hash: remote.manifest_hash,
        generation_sequence: decoded.generation_sequence,
    };

    assert!(validate_fresh_awaiting_conflict_source(
        &retained,
        &operation_id,
        &fresh,
        &decoded.library_id,
        i64::try_from(decoded.source_revision).unwrap(),
        &generation,
    )
    .is_err());
    assert_eq!(journal.load().unwrap(), Some(retained));
}

#[test]
fn acknowledge_abandons_only_an_unsealed_awaiting_conflict_job_and_preserves_data() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174143";
    let durable_job_id = "123e4567-e89b-42d3-a456-426614174144";
    let retained = awaiting_conflict_operation(operation_id, durable_job_id);
    let backup_path = directory
        .path()
        .join("peer-bidirectional/backups/conflict.risulossless");
    fs::create_dir_all(backup_path.parent().unwrap()).unwrap();
    fs::write(&backup_path, b"preserved-backup").unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({"preserved": true})),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    DurableCasJob::begin(
        directory.path(),
        durable_job_id,
        CasJobKind::LogicalDeltaTarget,
        0,
    )
    .unwrap();
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&retained)
        .unwrap();

    PeerBidirectionalCommandState::default()
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();

    assert_eq!(fs::read(&backup_path).unwrap(), b"preserved-backup");
    assert_eq!(store.read_root(None).unwrap().value["preserved"], true);
    assert!(PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .is_none());
    assert_eq!(
        DurableCasJob::open(directory.path(), durable_job_id)
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn deferred_v1_source_precommit_descendant_requires_explicit_revoke_to_abandon() {
    for action in ["acknowledge", "reconcile"] {
        let directory = tempfile::tempdir().unwrap();
        let source_device_id = "123e4567-e89b-42d3-a456-426614174210";
        let target_device_id = "123e4567-e89b-42d3-a456-426614174211";
        let operation_id = issue_deferred_bidirectional_offer(directory.path(), target_device_id);
        let durable_job_id = uuid::Uuid::new_v4().to_string();
        let (mut store, cas, mut evidence, previous) = retain_source_prepared_fixture(
            directory.path(),
            &operation_id,
            source_device_id,
            target_device_id,
            &durable_job_id,
        );
        evidence.completion_deferred_v1 = true;
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        journal
            .store(&evidence.durable_operation(durable_job_id))
            .unwrap();
        store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"preserved": "deferred-source-descendant"})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let session = LanBidirectionalSession {
            session_id: "123e4567-e89b-42d3-a456-426614174212".to_owned(),
            source_device_id: source_device_id.to_owned(),
            target_device_id: target_device_id.to_owned(),
        };
        let request = LanBidirectionalRemoteApplyRequest {
            operation_id: operation_id.clone(),
            source_endpoint: "http://127.0.0.1:1".to_owned(),
            source_session_id: "123e4567-e89b-42d3-a456-426614174213".to_owned(),
            source_manifest_id: "manifest".to_owned(),
            source_claim: "claim".to_owned(),
            expected_source_revision: 0,
            expected_source_generation: LanBidirectionalGeneration {
                generation_id: previous.generation_id.clone(),
                manifest_hash: previous.manifest_hash.clone(),
                generation_sequence: previous.generation_sequence.clone(),
            },
            expected_common_base_manifest_hash: previous.manifest_hash,
            backup_losing_side: false,
            completion_deferred_v1: Some(true),
        };

        let error = if action == "acknowledge" {
            PeerBidirectionalCommandState::default()
                .acknowledge(directory.path(), &mut store, &operation_id)
                .unwrap_err()
        } else {
            match reconcile_source_operation(&mut store, &cas, directory.path(), &session, &request)
            {
                Err(error) => error,
                Ok(_) => panic!("deferred source descendant was unexpectedly reconciled"),
            }
        };
        assert!(matches!(error, PeerSyncError::Protocol(_)));
        assert!(journal.load().unwrap().is_some());

        super::super::device_registry::revoke_outgoing_device(
            directory.path(),
            target_device_id,
            |_| {},
        )
        .unwrap();
        if action == "acknowledge" {
            PeerBidirectionalCommandState::default()
                .acknowledge(directory.path(), &mut store, &operation_id)
                .unwrap();
        } else {
            assert!(matches!(
                reconcile_source_operation(&mut store, &cas, directory.path(), &session, &request,),
                Err(PeerSyncError::StaleManifest { .. })
            ));
        }
        assert!(journal.load().unwrap().is_none());
    }
}

#[test]
fn deferred_v1_source_completed_ack_requires_exact_receipt_or_explicit_revoke() {
    for cleanup in ["receipt", "revoke"] {
        let directory = tempfile::tempdir().unwrap();
        let source_device_id = "123e4567-e89b-42d3-a456-426614174214";
        let target_device_id = "123e4567-e89b-42d3-a456-426614174215";
        let operation_id = issue_deferred_bidirectional_offer(directory.path(), target_device_id);
        let durable_job_id = uuid::Uuid::new_v4().to_string();
        let (mut store, _cas, mut evidence, remote_receipt) = retain_source_postcommit_fixture(
            directory.path(),
            &operation_id,
            source_device_id,
            target_device_id,
            &durable_job_id,
        );
        evidence.completion_deferred_v1 = true;
        store_source_completed(
            directory.path(),
            evidence.clone(),
            &remote_receipt,
            remote_receipt.committed_revision,
        )
        .unwrap();
        let combined_bytes = remote_receipt.transferred_bytes.checked_add(23).unwrap();
        super::super::device_registry::seal_outgoing_completion_lease(
            directory.path(),
            target_device_id,
            super::super::device_registry::CompletionLane::Bidirectional,
            &operation_id,
            &evidence.expected_source_generation.manifest_hash,
            combined_bytes,
        )
        .unwrap();
        let state = PeerBidirectionalCommandState::default();

        assert!(matches!(
            state.acknowledge(directory.path(), &mut store, &operation_id),
            Err(PeerSyncError::Protocol(_))
        ));
        assert!(PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .is_some());

        if cleanup == "receipt" {
            super::super::device_registry::accept_outgoing_completion_offer(
                directory.path(),
                target_device_id,
                super::super::device_registry::CompletionLane::Bidirectional,
                &operation_id,
                &evidence.expected_source_generation.manifest_hash,
                combined_bytes,
            )
            .unwrap();
            super::super::device_registry::issue_outgoing_unmeasured_completion_offer(
                directory.path(),
                target_device_id,
                super::super::device_registry::CompletionLane::Bidirectional,
                &"f".repeat(64),
                None,
            )
            .unwrap();
            assert!(
                super::super::device_registry::outgoing_completion_lease_ready_bytes(
                    directory.path(),
                    target_device_id,
                    super::super::device_registry::CompletionLane::Bidirectional,
                    &operation_id,
                    &evidence.expected_source_generation.manifest_hash,
                )
                .unwrap()
                .is_none()
            );
        } else {
            super::super::device_registry::revoke_outgoing_device(
                directory.path(),
                target_device_id,
                |_| {},
            )
            .unwrap();
        }
        state
            .acknowledge(directory.path(), &mut store, &operation_id)
            .unwrap();
        assert!(PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .is_none());
    }
}

#[test]
fn acknowledge_retries_awaiting_conflict_cleanup_with_missing_or_released_job() {
    for case in ["missing", "released"] {
        let directory = tempfile::tempdir().unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174145";
        let durable_job_id = "123e4567-e89b-42d3-a456-426614174146";
        let retained = awaiting_conflict_operation(operation_id, durable_job_id);
        let mut store = PersistentStore::open(directory.path()).unwrap();
        if case == "released" {
            let mut job = DurableCasJob::begin(
                directory.path(),
                durable_job_id,
                CasJobKind::LogicalDeltaTarget,
                0,
            )
            .unwrap();
            job.leave_release_record_for_cleanup_retry(CasReleaseOutcome::Aborted)
                .unwrap();
        }
        PeerBidirectionalOperationJournal::new(directory.path())
            .store(&retained)
            .unwrap();

        PeerBidirectionalCommandState::default()
            .acknowledge(directory.path(), &mut store, operation_id)
            .unwrap();
        assert!(PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .is_none());
        assert!(matches!(
            DurableCasJob::open(directory.path(), durable_job_id),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ));
    }
}

#[test]
fn acknowledge_retains_awaiting_conflict_with_wrong_kind_or_sealed_job() {
    for case in ["wrong-kind", "sealed"] {
        let directory = tempfile::tempdir().unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174145";
        let durable_job_id = "123e4567-e89b-42d3-a456-426614174146";
        let retained = awaiting_conflict_operation(operation_id, durable_job_id);
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let mut job = DurableCasJob::begin(
            directory.path(),
            durable_job_id,
            if case == "wrong-kind" {
                CasJobKind::PeerClone
            } else {
                CasJobKind::LogicalDeltaTarget
            },
            0,
        )
        .unwrap();
        if case == "sealed" {
            job.seal(&mut store, 0).unwrap();
        }
        PeerBidirectionalOperationJournal::new(directory.path())
            .store(&retained)
            .unwrap();

        assert!(PeerBidirectionalCommandState::default()
            .acknowledge(directory.path(), &mut store, operation_id)
            .is_err());
        assert_eq!(
            PeerBidirectionalOperationJournal::new(directory.path())
                .load()
                .unwrap(),
            Some(retained)
        );
    }
}

#[test]
fn acknowledge_rejects_an_active_target_without_mutating_completion() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174147";
    let completed = PeerBidirectionalDurableOperation::Completed {
        schema: OPERATION_SCHEMA.to_owned(),
        remote_apply_receipt: None,
        source_binding: None,
        result: PeerBidirectionalCompletedResult {
            kind: "noChanges".to_owned(),
            operation_id: operation_id.to_owned(),
            revision: 0,
            remote_revision: 0,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        },
    };
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&completed).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let state = PeerBidirectionalCommandState::default();
    let _active_target = state.begin_target().unwrap();

    assert!(matches!(
        state.acknowledge(directory.path(), &mut store, operation_id),
        Err(PeerSyncError::Protocol(message)) if message.contains("target operation is active")
    ));
    assert_eq!(journal.load().unwrap(), Some(completed));
}

#[test]
fn lane_three_capabilities_and_status_dtos_are_exact() {
    assert_eq!(
        serde_json::to_value(peer_bidirectional_capabilities()).unwrap(),
        json!({
            "desktop": true,
            "sourceReady": true,
            "atomicActivationReady": true,
            "authenticatedTransportReady": true,
            "losslessBackupReady": true,
            "durableStateReady": true,
            "productionEnabled": true,
        })
    );
    assert_eq!(
        serde_json::to_value(PeerBidirectionalSyncResult::ResumeRequired {
            operation_id: "123e4567-e89b-42d3-a456-426614174088".to_owned(),
            phase: "localCommitted",
            committed_revision: 9,
        })
        .unwrap(),
        json!({
            "kind": "resumeRequired",
            "operationId": "123e4567-e89b-42d3-a456-426614174088",
            "phase": "localCommitted",
            "committedRevision": 9,
        })
    );
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174084";
    PeerBidirectionalOperationJournal::new(directory.path())
        .store(&PeerBidirectionalDurableOperation::Completed {
            schema: OPERATION_SCHEMA.to_owned(),
            remote_apply_receipt: None,
            source_binding: None,
            result: PeerBidirectionalCompletedResult {
                kind: "updated".to_owned(),
                operation_id: operation_id.to_owned(),
                revision: 8,
                remote_revision: 11,
                transferred_objects: 2,
                transferred_bytes: 19,
                backups: vec![backup(PeerBidirectionalBackupSide::Remote)],
            },
        })
        .unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();

    assert_eq!(
        serde_json::to_value(
            PeerBidirectionalCommandState::default()
                .status(directory.path(), &mut store)
                .unwrap()
        )
        .unwrap(),
        json!({
            "source": { "phase": "idle", "devices": [] },
            "operation": {
                "phase": "completed",
                "result": {
                    "kind": "updated",
                    "operationId": operation_id,
                    "revision": 8,
                    "remoteRevision": 11,
                    "transferredObjects": 2,
                    "transferredBytes": 19,
                    "backups": [{
                        "packageId": "f".repeat(64),
                        "side": "remote",
                        "path": "peer-bidirectional/backups/conflict.risulossless",
                    }],
                },
            },
        })
    );
}

#[test]
fn source_completed_status_projects_its_backup_as_local_without_mutating_the_journal() {
    let directory = tempfile::tempdir().unwrap();
    let binding = SourcePreparedEvidence {
        operation_id: "123e4567-e89b-42d3-a456-426614174148".to_owned(),
        source_device_id: "123e4567-e89b-42d3-a456-426614174149".to_owned(),
        target_device_id: "123e4567-e89b-42d3-a456-426614174150".to_owned(),
        expected_source_revision: 7,
        previous_shared: generation("shared-source-view", "2", 'a'),
        expected_source_generation: generation("source-view", "3", 'b'),
        shared_generation: LanBidirectionalGeneration {
            generation_id: "shared-source-view-next".to_owned(),
            manifest_hash: "c".repeat(64),
            generation_sequence: "4".to_owned(),
        },
        incoming_revision: 9,
        transferred_objects: 2,
        transferred_bytes: 19,
        backup_required: true,
        backup: Some(LanBidirectionalBackupReceipt {
            package_id: "d".repeat(64),
            path: "peer-bidirectional/backups/source-view.risulossless".to_owned(),
        }),
        completion_deferred_v1: false,
    };
    let receipt = binding.receipt_at(8).unwrap();
    let completed = PeerBidirectionalDurableOperation::Completed {
        schema: OPERATION_SCHEMA.to_owned(),
        remote_apply_receipt: Some(receipt),
        source_binding: Some(binding.clone()),
        result: PeerBidirectionalCompletedResult {
            kind: "updated".to_owned(),
            operation_id: binding.operation_id.clone(),
            revision: 8,
            remote_revision: binding.incoming_revision,
            transferred_objects: binding.transferred_objects,
            transferred_bytes: binding.transferred_bytes,
            backups: vec![PeerBidirectionalBackupReceipt {
                package_id: binding.backup.as_ref().unwrap().package_id.clone(),
                side: PeerBidirectionalBackupSide::Remote,
                path: binding.backup.as_ref().unwrap().path.clone(),
            }],
        },
    };
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&completed).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();

    let status = PeerBidirectionalCommandState::default()
        .status(directory.path(), &mut store)
        .unwrap();
    let Some(PeerBidirectionalStatusOperation::Completed { result }) = status.operation else {
        panic!("expected completed source status");
    };
    assert_eq!(result.backups[0].side, PeerBidirectionalBackupSide::Local);
    assert_eq!(journal.load().unwrap(), Some(completed));
}

#[test]
fn completed_acknowledgement_keeps_source_owned_until_explicit_stop() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let built = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let source = LogicalDeltaSourceSession::open(
        PersistentStore::open(directory.path()).unwrap(),
        directory.path(),
        PRODUCT_LOGICAL_LIBRARY_ID,
        &built.manifest.generation,
    )
    .unwrap();
    let session_id = "123e4567-e89b-42d3-a456-426614174085";
    let prepared = super::super::lan::PreparedLogicalLanSession::new(
        session_id,
        "123e4567-e89b-42d3-a456-426614174086",
        built.manifest_hash.clone(),
        built.manifest_bytes,
        source.objects().to_vec(),
        Box::new(source),
    )
    .unwrap();
    let host = LanCloneHost::prepare_logical(prepared);
    let state = PeerBidirectionalCommandState::default();
    state
        .install_source(host, session_id, &built.manifest_hash)
        .unwrap();
    let started = state.start_source(session_id, Ipv4Addr::LOCALHOST).unwrap();
    assert_eq!(started.phase, PeerBidirectionalSourcePhase::Running);
    assert!(matches!(
        state.begin_target(),
        Err(PeerSyncError::Protocol(message)) if message.contains("source")
    ));
    let pairing_uri = url::Url::parse(started.pairing_uri.as_deref().unwrap()).unwrap();
    assert_eq!(pairing_uri.host_str(), Some("peer-sync"));
    let endpoint = pairing_uri
        .query_pairs()
        .find(|(key, _)| key == "endpoint")
        .unwrap()
        .1
        .into_owned();
    let manifest_id = pairing_uri
        .query_pairs()
        .find(|(key, _)| key == "manifest")
        .unwrap()
        .1
        .into_owned();
    let claim = pairing_uri
        .fragment()
        .unwrap()
        .strip_prefix("claim=")
        .unwrap();
    let _client =
        super::super::lan::LanLogicalDeltaClient::claim(&endpoint, session_id, &manifest_id, claim)
            .unwrap();
    let claimed_device_id = state
        .status(directory.path(), &mut store)
        .unwrap()
        .source
        .devices[0]
        .device_id
        .clone();
    state
        .revoke_source_device(session_id, &claimed_device_id)
        .unwrap();
    assert!(
        state
            .status(directory.path(), &mut store)
            .unwrap()
            .source
            .devices[0]
            .revoked
    );
    let operation_id = "123e4567-e89b-42d3-a456-426614174087";
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal
        .store(&PeerBidirectionalDurableOperation::Completed {
            schema: OPERATION_SCHEMA.to_owned(),
            remote_apply_receipt: None,
            source_binding: None,
            result: PeerBidirectionalCompletedResult {
                kind: "noChanges".to_owned(),
                operation_id: operation_id.to_owned(),
                revision: 0,
                remote_revision: 0,
                transferred_objects: 0,
                transferred_bytes: 0,
                backups: vec![],
            },
        })
        .unwrap();

    assert!(matches!(
        state.acknowledge(directory.path(), &mut store, operation_id),
        Err(PeerSyncError::Protocol(message)) if message.contains("source")
    ));
    assert!(journal.load().unwrap().is_some());
    assert_eq!(
        state
            .status(directory.path(), &mut store)
            .unwrap()
            .source
            .phase,
        PeerBidirectionalSourcePhase::Running
    );
    state.stop_source(session_id).unwrap();
    assert_eq!(
        state
            .status(directory.path(), &mut store)
            .unwrap()
            .source
            .phase,
        PeerBidirectionalSourcePhase::Stopped
    );
    assert!(state.begin_target().is_ok());
    state
        .acknowledge(directory.path(), &mut store, operation_id)
        .unwrap();
    assert!(journal.load().unwrap().is_none());
}

#[test]
fn clean_natural_tunnel_exit_finalizes_the_exact_source() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "123e4567-e89b-42d3-a456-426614174191";
    let stop_attempts = Arc::new(AtomicUsize::new(0));
    let (state, mut store) = state_with_source_tunnel(
        directory.path(),
        session_id,
        Box::new(FakeSourceTunnel {
            lifecycles: VecDeque::from([RunningTunnelLifecycle::Stopped]),
            stop_attempts: Arc::clone(&stop_attempts),
            fail_stop_through_attempt: 0,
        }),
    );

    let status = state.status(directory.path(), &mut store).unwrap();

    assert_eq!(status.source.phase, PeerBidirectionalSourcePhase::Stopped);
    assert!(status.source.session_id.is_none());
    assert!(status.source.devices.is_empty());
    assert_eq!(stop_attempts.load(Ordering::SeqCst), 1);
    assert!(!state.source_is_active().unwrap());
}

#[test]
fn cleanup_pending_natural_exit_retains_owner_for_exact_stop_retry() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "123e4567-e89b-42d3-a456-426614174192";
    let stop_attempts = Arc::new(AtomicUsize::new(0));
    let (state, mut store) = state_with_source_tunnel(
        directory.path(),
        session_id,
        Box::new(FakeSourceTunnel {
            lifecycles: VecDeque::from([RunningTunnelLifecycle::CleanupPending]),
            stop_attempts: Arc::clone(&stop_attempts),
            fail_stop_through_attempt: 0,
        }),
    );

    let status = state.status(directory.path(), &mut store).unwrap();
    assert_eq!(status.source.phase, PeerBidirectionalSourcePhase::Stopping);
    assert_eq!(status.source.session_id.as_deref(), Some(session_id));
    assert!(state.source_is_active().unwrap());

    state.stop_source(session_id).unwrap();
    assert_eq!(stop_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        state
            .status(directory.path(), &mut store)
            .unwrap()
            .source
            .phase,
        PeerBidirectionalSourcePhase::Stopped
    );
}

#[test]
fn source_status_probe_serializes_with_exact_stop() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "123e4567-e89b-42d3-a456-426614174193";
    let stop_attempts = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (state, _store) = state_with_source_tunnel(
        directory.path(),
        session_id,
        Box::new(BlockingSourceTunnel {
            lifecycle_started: started_tx,
            lifecycle_release: release_rx,
            stop_attempts: Arc::clone(&stop_attempts),
        }),
    );
    let status_state = state.clone();
    let root = directory.path().to_path_buf();
    let status = thread::spawn(move || {
        let mut store = PersistentStore::open(&root).unwrap();
        status_state.status(&root, &mut store)
    });
    started_rx.recv().unwrap();
    let stop_state = state.clone();
    let session = session_id.to_owned();
    let (stopped_tx, stopped_rx) = mpsc::channel();
    let stop = thread::spawn(move || {
        let result = stop_state.stop_source(&session);
        stopped_tx.send(()).unwrap();
        result
    });

    assert!(stopped_rx.try_recv().is_err());
    assert_eq!(stop_attempts.load(Ordering::SeqCst), 0);
    release_tx.send(()).unwrap();
    status.join().unwrap().unwrap();
    stop.join().unwrap().unwrap();
    stopped_rx.recv().unwrap();
    assert_eq!(stop_attempts.load(Ordering::SeqCst), 1);
    assert!(!state.source_is_active().unwrap());
}

#[test]
fn final_exit_uses_the_same_exact_source_cleanup_owner() {
    let directory = tempfile::tempdir().unwrap();
    let session_id = "123e4567-e89b-42d3-a456-426614174194";
    let stop_attempts = Arc::new(AtomicUsize::new(0));
    let (state, _store) = state_with_source_tunnel(
        directory.path(),
        session_id,
        Box::new(FakeSourceTunnel {
            lifecycles: VecDeque::new(),
            stop_attempts: Arc::clone(&stop_attempts),
            fail_stop_through_attempt: 0,
        }),
    );

    state.shutdown_for_exit();

    assert_eq!(stop_attempts.load(Ordering::SeqCst), 1);
    assert!(!state.source_is_active().unwrap());
}

#[test]
fn offline_status_projects_and_idempotently_revokes_a_durable_registered_device() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let base = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let peer_id = "123e4567-e89b-42d3-a456-426614174099";
    establish_logical_common_base(
        &mut store,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    let common = SyncGenerationIdentity {
        generation_id: base.manifest.generation,
        manifest_hash: base.manifest_hash,
        generation_sequence: base.manifest.generation_sequence,
    };
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                common,
                17,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    let state = PeerBidirectionalCommandState::default();

    let status = state.status(directory.path(), &mut store).unwrap();
    assert_eq!(status.source.phase, PeerBidirectionalSourcePhase::Idle);
    assert_eq!(
        status.source.devices,
        vec![PeerBidirectionalSourceDevice {
            device_id: peer_id.to_owned(),
            transferred_bytes: 0,
            current_object: None,
            last_seen_at: 17,
            revoked: false,
        }]
    );

    state
        .revoke_source_device("stopped-session", peer_id)
        .unwrap();
    revoke_durable_bidirectional_device(&mut store, peer_id).unwrap();
    revoke_durable_bidirectional_device(&mut store, peer_id).unwrap();

    let status = state.status(directory.path(), &mut store).unwrap();
    assert_eq!(status.source.devices.len(), 1);
    assert!(status.source.devices[0].revoked);
}

#[test]
fn bidirectional_network_transfer_does_not_hold_the_managed_store_mutex() {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut setup = PersistentStore::open(directory.path()).unwrap();
    let base = setup
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    let peer_id = "123e4567-e89b-42d3-a456-426614174089";
    establish_logical_common_base(
        &mut setup,
        &cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &base.manifest.generation,
        0,
        &base.manifest_bytes,
    )
    .unwrap();
    let common = SyncGenerationIdentity {
        generation_id: base.manifest.generation.clone(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: base.manifest.generation_sequence.clone(),
    };
    setup
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                PRODUCT_LOGICAL_LIBRARY_ID,
                peer_id,
                common,
                0,
            )
            .unwrap(),
            0,
        )
        .unwrap();
    setup
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({ "side": "local" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    let remote = remote_disjoint_manifest(&base.manifest);
    let managed = Arc::new(Mutex::new(setup));
    let independent = managed.lock().unwrap().open_native_job_store().unwrap();
    let (opened_tx, opened_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let root = directory.path().to_path_buf();
    let remote_manifest = remote.manifest_bytes.clone();
    let remote_objects = remote
        .record_objects
        .iter()
        .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
        .collect();
    let transfer = thread::spawn(move || {
        let mut store = independent;
        let cas = PayloadCas::new(&root).unwrap();
        let mut source = BlockingFixtureSource {
            objects: remote_objects,
            opened: opened_tx,
            release: release_rx,
        };
        begin_bidirectional_local_merge(
            &mut store,
            &cas,
            &root,
            LanBidirectionalLogicalCredential {
                endpoint: "http://192.168.1.2:32146".to_owned(),
                session_id: "123e4567-e89b-42d3-a456-426614174090".to_owned(),
                manifest_id: remote.manifest_hash,
                device_id: "123e4567-e89b-42d3-a456-426614174091".to_owned(),
                source_device_id: peer_id.to_owned(),
                bearer: "e".repeat(64),
            },
            1,
            &remote_manifest,
            &mut source,
        )
    });
    opened_rx.recv().unwrap();

    assert_eq!(
        managed.try_lock().unwrap().revision().unwrap(),
        1,
        "managed PDS must remain available while the independent coordinator reads the network",
    );
    release_tx.send(()).unwrap();
    assert_eq!(
        transfer.join().unwrap().unwrap(),
        LocalMergeOutcome::LocalCommitted
    );
}

#[test]
fn public_tunnel_url_is_not_persisted_and_remote_receipt_stays_local_committed() {
    for completion_mode in [
        PeerBidirectionalCompletionMode::Legacy,
        PeerBidirectionalCompletionMode::Unsupported,
        PeerBidirectionalCompletionMode::V1,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174180";
        let mut context = context(operation_id);
        context.credential.endpoint = "https://quick-id.trycloudflare.com".to_owned();
        context.credential.session_id = "123e4567-e89b-42d3-a456-426614174184".to_owned();
        context.credential.bearer = "7".repeat(64);
        context.completion_mode = completion_mode;
        let stable_device_id = context.credential.device_id.clone();
        let stable_source_device_id = context.credential.source_device_id.clone();
        let stable_manifest_id = context.credential.manifest_id.clone();
        let stable_library_id = context.library_id.clone();
        let stable_local_revision = context.expected_local_revision;
        let stable_remote_revision = context.expected_remote_revision;
        let stable_remote_generation = context.expected_remote_generation.clone();
        let shared_generation = generation("shared-public", "3", 'e');
        let operation = PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context,
            committed_revision: 8,
            shared_generation: shared_generation.clone(),
            changed: true,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 1,
            transferred_bytes: 4,
            backups: vec![],
        };
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        journal.store(&operation).unwrap();
        let bytes = fs::read(
            directory
                .path()
                .join("peer-bidirectional")
                .join(OPERATION_FILE),
        )
        .unwrap();
        let serialized = String::from_utf8_lossy(&bytes);
        assert!(!serialized.contains("trycloudflare"));
        assert!(!serialized.contains("123e4567-e89b-42d3-a456-426614174184"));
        assert!(!serialized.contains(&"7".repeat(64)));
        assert!(!serialized.contains("claim"));
        assert!(!serialized.contains("token"));
        assert!(!serialized.contains("process"));
        let Some(PeerBidirectionalDurableOperation::LocalCommitted { context, .. }) =
            journal.load().unwrap()
        else {
            panic!("expected scrubbed public local commit");
        };
        assert!(context.credential.endpoint.is_empty());
        assert!(context.credential.session_id.is_empty());
        assert!(context.credential.bearer.is_empty());
        assert_eq!(context.credential.device_id, stable_device_id);
        assert_eq!(context.credential.source_device_id, stable_source_device_id);
        assert_eq!(context.credential.manifest_id, stable_manifest_id);
        assert_eq!(context.library_id, stable_library_id);
        assert_eq!(context.expected_local_revision, stable_local_revision);
        assert_eq!(context.expected_remote_revision, stable_remote_revision);
        assert_eq!(context.expected_remote_generation, stable_remote_generation);

        let receipt = LanBidirectionalRemoteApplyReceipt {
            committed_revision: 8,
            committed_generation: LanBidirectionalGeneration {
                generation_id: shared_generation.generation_id,
                manifest_hash: shared_generation.manifest_hash,
                generation_sequence: shared_generation.generation_sequence,
            },
            transferred_objects: 2,
            transferred_bytes: 8,
            backup: None,
        };
        retain_remote_apply_receipt(directory.path(), operation_id, &receipt).unwrap();
        assert!(matches!(
            journal.load().unwrap(),
            Some(PeerBidirectionalDurableOperation::LocalCommitted {
                remote_apply_receipt: Some(retained), ..
            }) if retained == receipt
        ));
    }
}

#[test]
fn private_lan_durable_credential_remains_complete() {
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174185";
    let operation = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: context(operation_id),
        committed_revision: 8,
        shared_generation: generation("shared-private", "3", 'e'),
        changed: true,
        remote_backup_required: false,
        remote_apply_receipt: None,
        transferred_objects: 1,
        transferred_bytes: 4,
        backups: vec![],
    };
    let journal = PeerBidirectionalOperationJournal::new(directory.path());

    journal.store(&operation).unwrap();

    assert_eq!(journal.load().unwrap(), Some(operation));
}

#[test]
fn reverse_cleanup_failure_retains_exact_owner_and_retry_completes_without_remote_apply() {
    let _serial = REVERSE_CLEANUP_TEST_MUTEX.lock().unwrap();
    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut source = fixture_source(&remote);
    TARGET_JOB_RELEASE_FAILPOINT.with(|enabled| enabled.set(true));
    assert!(begin_bidirectional_local_merge(
        &mut store,
        &cas,
        directory.path(),
        credential,
        1,
        &remote.manifest_bytes,
        &mut source,
    )
    .is_err());
    let retained = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let (operation_id, remote_revision, shared) = match retained {
        PeerBidirectionalDurableOperation::LocalCommitted {
            context,
            shared_generation,
            ..
        } => (
            context.operation_id,
            context.expected_remote_revision,
            shared_generation,
        ),
        other => panic!("expected local commit, got {other:?}"),
    };
    let receipt = LanBidirectionalRemoteApplyReceipt {
        committed_revision: remote_revision,
        committed_generation: LanBidirectionalGeneration {
            generation_id: shared.generation_id,
            manifest_hash: shared.manifest_hash,
            generation_sequence: shared.generation_sequence,
        },
        transferred_objects: 0,
        transferred_bytes: 0,
        backup: None,
    };
    retain_remote_apply_receipt(directory.path(), &operation_id, &receipt).unwrap();

    let attempts = Arc::new(AtomicUsize::new(0));
    assert!(
        retain_reverse_cleanup_owner(&operation_id, fake_reverse_owner(&attempts, 1),).is_err()
    );
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::LocalCommitted {
            remote_apply_receipt: Some(retained), ..
        }) if retained == receipt
    ));
    assert!(REVERSE_TUNNEL_CLEANUP
        .lock()
        .unwrap()
        .contains_key(&operation_id));

    let revision = store.revision().unwrap();
    let result =
        run_retained_remote_completion(&mut store, directory.path(), &operation_id, revision, None)
            .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(matches!(
        result,
        PeerBidirectionalSyncResult::NoChanges {
            operation_id: ref completed_operation_id,
            ..
        } | PeerBidirectionalSyncResult::Updated {
            operation_id: ref completed_operation_id,
            ..
        } if completed_operation_id == &operation_id
    ));
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::Completed { .. })
    ));
}

#[test]
fn reverse_remote_error_and_cancellation_preserve_primary_and_local_commit() {
    let _serial = REVERSE_CLEANUP_TEST_MUTEX.lock().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174181";
    let retained = PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: context(operation_id),
        committed_revision: 7,
        shared_generation: generation("shared-cleanup", "4", 'f'),
        changed: true,
        remote_backup_required: false,
        remote_apply_receipt: None,
        transferred_objects: 0,
        transferred_bytes: 0,
        backups: vec![],
    };
    let journal = PeerBidirectionalOperationJournal::new(directory.path());
    journal.store(&retained).unwrap();

    let remote_attempts = Arc::new(AtomicUsize::new(0));
    let cleanup =
        retain_reverse_cleanup_owner(operation_id, fake_reverse_owner(&remote_attempts, 1));
    let primary = PeerSyncError::Protocol("fixture remote rejection".to_owned());
    assert!(matches!(
        resolve_remote_apply_result(Err(primary), cleanup),
        Err(PeerSyncError::Protocol(message)) if message == "fixture remote rejection"
    ));
    assert_eq!(journal.load().unwrap(), Some(retained.clone()));
    retry_reverse_tunnel_cleanup(operation_id).unwrap();

    let cancel_attempts = Arc::new(AtomicUsize::new(0));
    let cleanup =
        retain_reverse_cleanup_owner(operation_id, fake_reverse_owner(&cancel_attempts, 1));
    assert!(matches!(
        resolve_remote_apply_result(Err(PeerSyncError::Cancelled), cleanup),
        Err(PeerSyncError::Cancelled)
    ));
    assert_eq!(cancel_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(journal.load().unwrap(), Some(retained));
    retry_reverse_tunnel_cleanup(operation_id).unwrap();
}

#[test]
fn reverse_cleanup_rejects_cross_operation_retry_and_app_exit_attempts_owner() {
    let _serial = REVERSE_CLEANUP_TEST_MUTEX.lock().unwrap();
    let operation_id = "123e4567-e89b-42d3-a456-426614174182";
    let other_operation_id = "123e4567-e89b-42d3-a456-426614174183";
    let attempts = Arc::new(AtomicUsize::new(0));
    REVERSE_TUNNEL_CLEANUP
        .lock()
        .unwrap()
        .insert(operation_id.to_owned(), fake_reverse_owner(&attempts, 0));

    retry_reverse_tunnel_cleanup(other_operation_id).unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert!(REVERSE_TUNNEL_CLEANUP
        .lock()
        .unwrap()
        .contains_key(operation_id));
    cleanup_reverse_tunnels_for_exit();
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(!REVERSE_TUNNEL_CLEANUP
        .lock()
        .unwrap()
        .contains_key(operation_id));
}

#[test]
fn receipt_store_failure_still_retains_and_retries_exact_reverse_cleanup() {
    let _serial = REVERSE_CLEANUP_TEST_MUTEX.lock().unwrap();
    let (directory, cas, mut store, remote, credential) = disjoint_target_fixture();
    let mut source = fixture_source(&remote);
    TARGET_JOB_RELEASE_FAILPOINT.with(|enabled| enabled.set(true));
    assert!(begin_bidirectional_local_merge(
        &mut store,
        &cas,
        directory.path(),
        credential,
        1,
        &remote.manifest_bytes,
        &mut source,
    )
    .is_err());
    let retained = PeerBidirectionalOperationJournal::new(directory.path())
        .load()
        .unwrap()
        .unwrap();
    let (operation_id, remote_revision, shared) = match retained {
        PeerBidirectionalDurableOperation::LocalCommitted {
            context,
            shared_generation,
            remote_apply_receipt: None,
            ..
        } => (
            context.operation_id,
            context.expected_remote_revision,
            shared_generation,
        ),
        other => panic!("expected local commit without receipt, got {other:?}"),
    };
    let receipt = LanBidirectionalRemoteApplyReceipt {
        committed_revision: remote_revision,
        committed_generation: LanBidirectionalGeneration {
            generation_id: shared.generation_id,
            manifest_hash: shared.manifest_hash,
            generation_sequence: shared.generation_sequence,
        },
        transferred_objects: 0,
        transferred_bytes: 0,
        backup: None,
    };
    let cleanup_attempts = Arc::new(AtomicUsize::new(0));
    REMOTE_RECEIPT_STORE_FAILPOINT.with(|enabled| enabled.set(true));

    let error = finish_remote_apply_request(
        directory.path(),
        &operation_id,
        Ok(receipt.clone()),
        Some(fake_reverse_owner(&cleanup_attempts, 1)),
        None,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        PeerSyncError::Storage(message) if message.contains("remote receipt journal")
    ));
    assert_eq!(cleanup_attempts.load(Ordering::SeqCst), 1);
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::LocalCommitted {
            remote_apply_receipt: None,
            ..
        })
    ));
    retry_reverse_tunnel_cleanup("123e4567-e89b-42d3-a456-426614174199").unwrap();
    assert_eq!(cleanup_attempts.load(Ordering::SeqCst), 1);
    assert!(REVERSE_TUNNEL_CLEANUP
        .lock()
        .unwrap()
        .contains_key(&operation_id));

    cleanup_reverse_tunnels_for_exit();
    assert_eq!(cleanup_attempts.load(Ordering::SeqCst), 2);
    assert!(!REVERSE_TUNNEL_CLEANUP
        .lock()
        .unwrap()
        .contains_key(&operation_id));
    let remote_calls = Cell::new(0);
    let outcome = resume_bidirectional_local_committed_with_remote(
        &mut store,
        &cas,
        directory.path(),
        &operation_id,
        |_context, _revision, _shared, _manifest, _backup| {
            remote_calls.set(remote_calls.get() + 1);
            Ok(receipt.clone())
        },
    )
    .unwrap();
    assert!(matches!(outcome, ResumeLocalCommittedOutcome::Completed(_)));
    assert_eq!(remote_calls.get(), 1);
    assert!(matches!(
        PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap(),
        Some(PeerBidirectionalDurableOperation::Completed { .. })
    ));
}
