use super::delta_completion::{
    abandon_retained_delta_completion, complete_delta_accounting, recover_delta_completion,
    registered_delta_source_is_active, retained_delta_completion, unlink_canonical_with_sync,
    DeltaCommitWitness, DeltaCompletionContext, DeltaCompletionMode, DeltaCompletionTransport,
    PeerDeltaCompletionJournal, PeerDeltaDurableCompletion, RetainedDeltaWitness,
    DELTA_COMPLETION_AMBIGUOUS,
};
use super::device_registry::{
    completion_receipt_id, incoming_source_by_id, prepare_incoming_completion_delivery,
    record_incoming_completed_operation_once_for_lane, snapshot_incoming_completion_delivery,
    CompletionLane, DevicePermissions, IncomingSource, IncomingSourceRegistry,
    PendingCompletionDelivery,
};
use super::registry_commands::{
    ensure_no_active_registered_source_work, lock_registered_source_lifecycle,
    remove_incoming_source_if_inactive,
};
use crate::{
    asset_repository::{
        job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob},
        PayloadCas,
    },
    persistent_store::{
        establish_logical_common_base, PersistentStore, SyncGenerationIdentity,
        PRODUCT_LOGICAL_LIBRARY_ID,
    },
};
use std::path::{Path, PathBuf};

const OPERATION_ID: &str = "00000000-0000-4000-8000-000000000091";
const OTHER_OPERATION_ID: &str = "00000000-0000-4000-8000-000000000092";
const SOURCE_ID: &str = "00000000-0000-4000-8000-000000000093";

fn identity(generation_id: &str, manifest_byte: char, sequence: &str) -> SyncGenerationIdentity {
    SyncGenerationIdentity {
        generation_id: generation_id.to_owned(),
        manifest_hash: manifest_byte.to_string().repeat(64),
        generation_sequence: sequence.to_owned(),
    }
}

fn context(operation_id: &str) -> DeltaCompletionContext {
    DeltaCompletionContext {
        operation_id: operation_id.to_owned(),
        source_device_id: SOURCE_ID.to_owned(),
        manifest_id: "b".repeat(64),
        mode: DeltaCompletionMode::CompletionV1,
        pre_revision: 4,
        pre_common_base: Some(identity("base", 'a', "1")),
        post_revision: 5,
        post_common_base: identity("remote", 'b', "2"),
        transferred_objects: 1,
        transferred_bytes: 17,
    }
}

fn job_id(operation: &DeltaCompletionContext) -> String {
    format!("p4-delta-target-{}", operation.operation_id)
}

fn register_source(root: &Path, total_bytes: u64) {
    let mut registry = IncomingSourceRegistry::load(root).unwrap();
    registry
        .upsert(IncomingSource {
            device_id: SOURCE_ID.to_owned(),
            name: "source".to_owned(),
            endpoint: "http://192.168.0.9:32145".to_owned(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 1,
            total_bytes,
        })
        .unwrap();
    registry.save().unwrap();
}

struct FixtureTransport {
    root: PathBuf,
    prepared_bytes: u64,
    expected_before_delivery: u64,
    prepare_failures: usize,
    prepares: usize,
    deliveries: usize,
}

struct OfflineTransport;

impl DeltaCompletionTransport for OfflineTransport {
    fn prepare(
        &mut self,
        _source: &IncomingSource,
        _context: &DeltaCompletionContext,
    ) -> Result<u64, super::PeerSyncError> {
        Err(super::PeerSyncError::Protocol(
            "source is offline".to_owned(),
        ))
    }

    fn deliver(
        &mut self,
        _source: &IncomingSource,
        _delivery: &PendingCompletionDelivery,
    ) -> Result<(), super::PeerSyncError> {
        Err(super::PeerSyncError::Protocol(
            "source is offline".to_owned(),
        ))
    }
}

impl DeltaCompletionTransport for FixtureTransport {
    fn prepare(
        &mut self,
        _source: &IncomingSource,
        _context: &DeltaCompletionContext,
    ) -> Result<u64, super::PeerSyncError> {
        self.prepares += 1;
        if self.prepare_failures > 0 {
            self.prepare_failures -= 1;
            return Err(super::PeerSyncError::Protocol(
                "injected source prepare response loss".to_owned(),
            ));
        }
        Ok(self.prepared_bytes)
    }

    fn deliver(
        &mut self,
        _source: &IncomingSource,
        _delivery: &PendingCompletionDelivery,
    ) -> Result<(), super::PeerSyncError> {
        assert_eq!(
            IncomingSourceRegistry::load(&self.root).unwrap().sources()[0].total_bytes,
            self.expected_before_delivery
        );
        self.deliveries += 1;
        Ok(())
    }
}

fn fixture_transport(root: &Path, prepared_bytes: u64, expected: u64) -> FixtureTransport {
    FixtureTransport {
        root: root.to_owned(),
        prepared_bytes,
        expected_before_delivery: expected,
        prepare_failures: 0,
        prepares: 0,
        deliveries: 0,
    }
}

fn pending_delivery(
    operation: &DeltaCompletionContext,
    useful_bytes: u64,
) -> PendingCompletionDelivery {
    PendingCompletionDelivery {
        source_device_id: operation.source_device_id.clone(),
        lane: CompletionLane::Delta.as_str().to_owned(),
        completion_lease_id: operation.operation_id.clone(),
        manifest_id: operation.manifest_id.clone(),
        useful_bytes,
        receipt_id: completion_receipt_id(
            CompletionLane::Delta.as_str(),
            &operation.operation_id,
            &operation.manifest_id,
        ),
    }
}

#[test]
fn delta_completion_journal_is_single_flight_and_activation_intent_is_retryable() {
    let root = tempfile::tempdir().unwrap();
    let journal = PeerDeltaCompletionJournal::new(root.path());
    let first = context(OPERATION_ID);
    let second = context(OTHER_OPERATION_ID);

    journal.store_activation_intent(&first).unwrap();
    assert_eq!(
        journal.load().unwrap(),
        Some(PeerDeltaDurableCompletion::ActivationIntent {
            context: first.clone(),
        })
    );
    journal.store_activation_intent(&first).unwrap();
    assert!(journal.store_activation_intent(&second).is_err());

    journal.remove_exact(&first).unwrap();
    journal.remove_exact(&first).unwrap();
    assert_eq!(journal.load().unwrap(), None);
}

#[test]
fn registered_delta_source_activity_is_scoped_to_the_retained_journal_source() {
    let root = tempfile::tempdir().unwrap();
    let operation = context(OPERATION_ID);

    assert!(!registered_delta_source_is_active(root.path(), Some(SOURCE_ID)).unwrap());
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    assert!(registered_delta_source_is_active(root.path(), Some(SOURCE_ID)).unwrap());
    assert!(!registered_delta_source_is_active(
        root.path(),
        Some("00000000-0000-4000-8000-000000000094"),
    )
    .unwrap());
    assert!(registered_delta_source_is_active(root.path(), None).unwrap());
}

#[test]
fn compatible_registration_allows_rotation_without_a_journal_and_same_bearer_refresh_with_one() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 40);

    super::registry_commands::register_incoming_source_if_compatible(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.to_owned(),
            name: "rotated source".to_owned(),
            endpoint: "http://192.168.0.10:32145".to_owned(),
            bearer: "d".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 99,
            total_bytes: 0,
        },
    )
    .unwrap();
    let receipt_id = "e".repeat(64);
    record_incoming_completed_operation_once_for_lane(
        root.path(),
        SOURCE_ID,
        CompletionLane::Delta,
        &receipt_id,
        12,
    )
    .unwrap();
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&context(OPERATION_ID))
        .unwrap();

    super::registry_commands::register_incoming_source_if_compatible(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.to_owned(),
            name: "moved source".to_owned(),
            endpoint: "http://192.168.0.11:32145".to_owned(),
            bearer: "d".repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            last_seen_ms: 100,
            total_bytes: 0,
        },
    )
    .unwrap();
    record_incoming_completed_operation_once_for_lane(
        root.path(),
        SOURCE_ID,
        CompletionLane::Delta,
        &receipt_id,
        12,
    )
    .unwrap();

    let source = incoming_source_by_id(root.path(), SOURCE_ID)
        .unwrap()
        .unwrap();
    assert_eq!(source.name, "moved source");
    assert_eq!(source.endpoint, "http://192.168.0.11:32145");
    assert_eq!(source.bearer, "d".repeat(64));
    assert!(source.permissions.allows_bidirectional());
    assert_eq!(source.total_bytes, 52);
}

#[test]
fn active_delta_journal_blocks_rotation_and_removal_until_exact_cleanup() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 40);
    let operation = context(OPERATION_ID);
    let journal = PeerDeltaCompletionJournal::new(root.path());
    journal.store_activation_intent(&operation).unwrap();
    let sources_path = root.path().join("peer-sync/sources.json");
    let before = std::fs::read(&sources_path).unwrap();
    let rotated = IncomingSource {
        device_id: SOURCE_ID.to_owned(),
        name: "rotated source".to_owned(),
        endpoint: "http://192.168.0.10:32145".to_owned(),
        bearer: "d".repeat(64),
        permissions: DevicePermissions::read(),
        last_seen_ms: 99,
        total_bytes: 0,
    };

    assert!(
        super::registry_commands::register_incoming_source_if_compatible(root.path(), rotated,)
            .is_err()
    );
    assert_eq!(std::fs::read(&sources_path).unwrap(), before);
    assert!(
        super::registry_commands::remove_incoming_source_if_inactive(root.path(), SOURCE_ID)
            .is_err()
    );
    assert_eq!(std::fs::read(&sources_path).unwrap(), before);

    journal.remove_exact(&operation).unwrap();
    super::registry_commands::remove_incoming_source_if_inactive(root.path(), SOURCE_ID).unwrap();
    assert!(incoming_source_by_id(root.path(), SOURCE_ID)
        .unwrap()
        .is_none());
}

#[test]
fn removing_an_absent_delta_completion_journal_is_retryable() {
    let root = tempfile::tempdir().unwrap();
    let journal = PeerDeltaCompletionJournal::new(root.path());

    journal.remove_exact(&context(OPERATION_ID)).unwrap();
    journal.remove_exact(&context(OPERATION_ID)).unwrap();
}

#[test]
fn failed_unlink_directory_sync_is_completed_by_a_not_found_retry() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("completion-operation.json");
    std::fs::write(&path, b"pending").unwrap();

    assert!(unlink_canonical_with_sync(&path, || {
        Err(super::PeerSyncError::Storage(
            "injected directory sync failure".to_owned(),
        ))
    })
    .is_err());
    assert!(!path.exists());

    let mut synced = false;
    unlink_canonical_with_sync(&path, || {
        synced = true;
        Ok(())
    })
    .unwrap();
    assert!(synced);
}

#[cfg(windows)]
#[test]
fn completed_delta_tombstone_is_bounded_and_cleaned_on_load() {
    let root = tempfile::tempdir().unwrap();
    let peer_delta = root.path().join("peer-delta");
    std::fs::create_dir(&peer_delta).unwrap();
    let tombstone = peer_delta.join(".completion-operation.deleted");
    std::fs::write(&tombstone, b"deleted").unwrap();

    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_none());
    assert!(!tombstone.exists());

    std::fs::write(&tombstone, vec![0; 16 * 1024 + 1]).unwrap();
    assert!(PeerDeltaCompletionJournal::new(root.path()).load().is_err());
}

#[test]
fn v1_completion_retries_after_a_lost_source_prepare_response() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let mut operation = context(OPERATION_ID);
    operation.transferred_bytes = 0;
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = fixture_transport(root.path(), 17, 5);
    transport.prepare_failures = 1;

    assert!(complete_delta_accounting(root.path(), &mut transport).is_err());
    assert_eq!((transport.prepares, transport.deliveries), (1, 0));
    assert!(
        snapshot_incoming_completion_delivery(root.path(), SOURCE_ID, CompletionLane::Delta,)
            .unwrap()
            .is_none()
    );

    assert_eq!(
        complete_delta_accounting(root.path(), &mut transport).unwrap(),
        17
    );
    assert_eq!((transport.prepares, transport.deliveries), (2, 1));
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        22
    );
}

#[test]
fn v1_completion_restart_uses_pending_source_bytes_without_preparing_again() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let mut operation = context(OPERATION_ID);
    operation.transferred_bytes = 0;
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    prepare_incoming_completion_delivery(root.path(), pending_delivery(&operation, 17)).unwrap();
    let mut transport = fixture_transport(root.path(), 99, 5);

    assert_eq!(
        complete_delta_accounting(root.path(), &mut transport).unwrap(),
        17
    );
    assert_eq!((transport.prepares, transport.deliveries), (0, 1));
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        22
    );
}

#[test]
fn v1_completion_rejects_a_mismatched_pending_delivery_before_source_prepare() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let operation = context(OPERATION_ID);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    prepare_incoming_completion_delivery(
        root.path(),
        pending_delivery(&context(OTHER_OPERATION_ID), 23),
    )
    .unwrap();
    let before = std::fs::read(root.path().join("peer-sync/sources.json")).unwrap();
    let mut transport = fixture_transport(root.path(), 17, 5);

    assert!(complete_delta_accounting(root.path(), &mut transport).is_err());
    assert_eq!((transport.prepares, transport.deliveries), (0, 0));
    assert_eq!(
        std::fs::read(root.path().join("peer-sync/sources.json")).unwrap(),
        before
    );
}

#[test]
fn v1_completion_uses_cumulative_source_bytes_after_a_shrunk_retry() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let mut operation = context(OPERATION_ID);
    operation.transferred_bytes = 0;
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = fixture_transport(root.path(), 17, 5);

    assert_eq!(
        complete_delta_accounting(root.path(), &mut transport).unwrap(),
        17
    );
    assert_eq!((transport.prepares, transport.deliveries), (1, 1));
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        22
    );

    assert_eq!(
        complete_delta_accounting(root.path(), &mut transport).unwrap(),
        17
    );
    assert_eq!((transport.prepares, transport.deliveries), (1, 1));
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        22
    );
}

#[test]
fn v1_completion_overflow_retains_activation_intent_and_pending_outbox() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), u64::MAX);
    let operation = context(OPERATION_ID);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = fixture_transport(root.path(), 1, u64::MAX);

    assert!(complete_delta_accounting(root.path(), &mut transport).is_err());
    assert_eq!((transport.prepares, transport.deliveries), (1, 1));
    assert!(matches!(
        PeerDeltaCompletionJournal::new(root.path()).load().unwrap(),
        Some(PeerDeltaDurableCompletion::ActivationIntent { .. })
    ));
    let registry = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(registry.sources()[0].total_bytes, u64::MAX);
    let pending =
        snapshot_incoming_completion_delivery(root.path(), SOURCE_ID, CompletionLane::Delta)
            .unwrap()
            .unwrap();
    assert_eq!(pending.delivery.useful_bytes, 1);

    assert!(complete_delta_accounting(root.path(), &mut transport).is_err());
    assert_eq!((transport.prepares, transport.deliveries), (1, 2));
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        u64::MAX
    );
}

#[test]
fn unsupported_completion_records_target_only_once_including_zero_bytes() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let mut operation = context(OPERATION_ID);
    operation.mode = DeltaCompletionMode::UnsupportedV2;
    operation.transferred_bytes = 0;
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = fixture_transport(root.path(), 99, 5);

    assert_eq!(
        complete_delta_accounting(root.path(), &mut transport).unwrap(),
        0
    );
    assert_eq!(
        complete_delta_accounting(root.path(), &mut transport).unwrap(),
        0
    );
    assert_eq!((transport.prepares, transport.deliveries), (0, 0));
    let registry = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(registry.sources()[0].total_bytes, 5);
    assert!(registry
        .has_completed_operation_for_lane(
            SOURCE_ID,
            CompletionLane::Delta,
            &super::device_registry::completion_receipt_id(
                "delta",
                OPERATION_ID,
                &operation.manifest_id,
            ),
        )
        .unwrap());
}

#[test]
fn unsupported_completion_rejects_a_same_receipt_with_different_bytes() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let mut operation = context(OPERATION_ID);
    operation.mode = DeltaCompletionMode::UnsupportedV2;
    operation.transferred_bytes = 9;
    let receipt_id = completion_receipt_id(
        CompletionLane::Delta.as_str(),
        &operation.operation_id,
        &operation.manifest_id,
    );
    record_incoming_completed_operation_once_for_lane(
        root.path(),
        SOURCE_ID,
        CompletionLane::Delta,
        &receipt_id,
        10,
    )
    .unwrap();
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let before = std::fs::read(root.path().join("peer-sync/sources.json")).unwrap();
    let mut transport = fixture_transport(root.path(), 99, 15);

    assert!(complete_delta_accounting(root.path(), &mut transport).is_err());
    assert_eq!(
        std::fs::read(root.path().join("peer-sync/sources.json")).unwrap(),
        before
    );
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        15
    );
    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_some());
}

#[test]
fn recovery_clears_uncommitted_intent_without_counting() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let mut store = PersistentStore::open(root.path()).unwrap();
    let mut operation = context(OPERATION_ID);
    operation.pre_revision = 0;
    operation.pre_common_base = None;
    operation.post_revision = 1;
    let mut job = DurableCasJob::begin(
        root.path(),
        &job_id(&operation),
        CasJobKind::LogicalDeltaTarget,
        1,
    )
    .unwrap();
    job.seal(&mut store, 1).unwrap();
    drop(job);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = fixture_transport(root.path(), 17, 5);

    assert!(
        recover_delta_completion(&mut store, root.path(), &mut transport)
            .unwrap()
            .is_none()
    );
    assert_eq!((transport.prepares, transport.deliveries), (0, 0));
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        5
    );
    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_none());
    assert!(DurableCasJob::open(root.path(), &job_id(&operation)).is_err());
}

#[test]
fn uncommitted_recovery_tolerates_released_unsealed_and_missing_jobs_without_counting() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let mut store = PersistentStore::open(root.path()).unwrap();
    let mut operation = context(OPERATION_ID);
    operation.pre_revision = 0;
    operation.pre_common_base = None;
    operation.post_revision = 1;
    let mut job = DurableCasJob::begin(
        root.path(),
        &job_id(&operation),
        CasJobKind::LogicalDeltaTarget,
        1,
    )
    .unwrap();
    job.leave_release_record_for_cleanup_retry(CasReleaseOutcome::Aborted)
        .unwrap();
    drop(job);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = fixture_transport(root.path(), 17, 5);

    assert!(
        recover_delta_completion(&mut store, root.path(), &mut transport)
            .unwrap()
            .is_none()
    );
    assert_eq!((transport.prepares, transport.deliveries), (0, 0));
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        5
    );
    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_none());
    assert!(DurableCasJob::open(root.path(), &job_id(&operation)).is_err());

    let missing_root = tempfile::tempdir().unwrap();
    register_source(missing_root.path(), 5);
    let mut missing_store = PersistentStore::open(missing_root.path()).unwrap();
    PeerDeltaCompletionJournal::new(missing_root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut missing_transport = fixture_transport(missing_root.path(), 17, 5);
    assert!(recover_delta_completion(
        &mut missing_store,
        missing_root.path(),
        &mut missing_transport,
    )
    .unwrap()
    .is_none());
    assert_eq!(
        (missing_transport.prepares, missing_transport.deliveries),
        (0, 0)
    );
    assert!(PeerDeltaCompletionJournal::new(missing_root.path())
        .load()
        .unwrap()
        .is_none());
}

#[test]
fn uncommitted_recovery_does_not_touch_an_exact_id_job_owned_by_another_lane() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let mut store = PersistentStore::open(root.path()).unwrap();
    let mut operation = context(OPERATION_ID);
    operation.pre_revision = 0;
    operation.pre_common_base = None;
    operation.post_revision = 1;
    let job =
        DurableCasJob::begin(root.path(), &job_id(&operation), CasJobKind::PeerClone, 1).unwrap();
    let job_path = job.journal_path().to_path_buf();
    drop(job);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let journal_path = root.path().join("peer-delta/completion-operation.json");
    let journal_before = std::fs::read(&journal_path).unwrap();
    let job_before = std::fs::read(&job_path).unwrap();
    let mut transport = fixture_transport(root.path(), 17, 5);

    assert!(recover_delta_completion(&mut store, root.path(), &mut transport).is_err());
    assert_eq!((transport.prepares, transport.deliveries), (0, 0));
    assert_eq!(std::fs::read(&journal_path).unwrap(), journal_before);
    assert_eq!(std::fs::read(&job_path).unwrap(), job_before);
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        5
    );
}

fn committed_context(root: &Path) -> (PersistentStore, PayloadCas, DeltaCompletionContext) {
    let mut store = PersistentStore::open(root).unwrap();
    let cas = PayloadCas::new(root).unwrap();
    let local = store
        .seal_or_initialize_active_logical_generation(&cas)
        .unwrap();
    establish_logical_common_base(
        &mut store,
        &cas,
        SOURCE_ID,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &local.manifest.generation,
        0,
        &local.manifest_bytes,
    )
    .unwrap();
    let common = store
        .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, SOURCE_ID)
        .unwrap()
        .unwrap();
    let mut operation = context(OPERATION_ID);
    operation.mode = DeltaCompletionMode::UnsupportedV2;
    operation.pre_revision = 0;
    operation.pre_common_base = Some(common.clone());
    operation.post_revision = 0;
    operation.post_common_base = common.clone();
    operation.manifest_id = common.manifest_hash;
    (store, cas, operation)
}

#[test]
fn committed_recovery_without_job_counts_and_removes_the_journal_once() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let (mut store, _cas, operation) = committed_context(root.path());
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = fixture_transport(root.path(), 99, 5);

    let completed = recover_delta_completion(&mut store, root.path(), &mut transport)
        .unwrap()
        .unwrap();
    assert_eq!(completed.context, operation);
    assert_eq!(completed.useful_bytes, 17);
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        22
    );
    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_none());
    assert!(
        recover_delta_completion(&mut store, root.path(), &mut transport)
            .unwrap()
            .is_none()
    );
}

#[test]
fn committed_recovery_with_a_released_job_completes_exact_accounting() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let (mut store, _cas, operation) = committed_context(root.path());
    let mut job = DurableCasJob::begin(
        root.path(),
        &job_id(&operation),
        CasJobKind::LogicalDeltaTarget,
        1,
    )
    .unwrap();
    job.seal(&mut store, 1).unwrap();
    job.leave_release_record_for_cleanup_retry(CasReleaseOutcome::Committed)
        .unwrap();
    drop(job);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = fixture_transport(root.path(), 99, 5);

    assert!(
        recover_delta_completion(&mut store, root.path(), &mut transport)
            .unwrap()
            .is_some()
    );
    assert_eq!((transport.prepares, transport.deliveries), (0, 0));
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        22
    );
    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_none());
    let retained = DurableCasJob::open(root.path(), &job_id(&operation)).unwrap();
    assert!(retained.is_sealed());
    assert!(retained.is_released());
}

#[test]
fn committed_recovery_overflow_reuses_pending_bytes_and_retains_recovery_state() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), u64::MAX);
    let (mut store, _cas, mut operation) = committed_context(root.path());
    operation.mode = DeltaCompletionMode::CompletionV1;
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    prepare_incoming_completion_delivery(root.path(), pending_delivery(&operation, 1)).unwrap();
    let mut transport = fixture_transport(root.path(), 99, u64::MAX);

    assert!(recover_delta_completion(&mut store, root.path(), &mut transport).is_err());
    assert_eq!((transport.prepares, transport.deliveries), (0, 1));
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        u64::MAX
    );
    assert!(matches!(
        PeerDeltaCompletionJournal::new(root.path()).load().unwrap(),
        Some(PeerDeltaDurableCompletion::ActivationIntent { .. })
    ));
    let pending =
        snapshot_incoming_completion_delivery(root.path(), SOURCE_ID, CompletionLane::Delta)
            .unwrap()
            .unwrap();
    assert_eq!(pending.delivery.useful_bytes, 1);
}

#[test]
fn committed_recovery_without_job_uses_existing_v1_receipt_while_source_is_offline() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let (mut store, _cas, mut operation) = committed_context(root.path());
    operation.mode = DeltaCompletionMode::CompletionV1;
    operation.transferred_bytes = 0;
    let receipt_id = completion_receipt_id(
        CompletionLane::Delta.as_str(),
        &operation.operation_id,
        &operation.manifest_id,
    );
    record_incoming_completed_operation_once_for_lane(
        root.path(),
        SOURCE_ID,
        CompletionLane::Delta,
        &receipt_id,
        23,
    )
    .unwrap();
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();

    let completed = recover_delta_completion(&mut store, root.path(), &mut OfflineTransport)
        .unwrap()
        .unwrap();
    assert_eq!(completed.useful_bytes, 23);
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        28
    );
    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_none());
}

#[test]
fn ambiguous_recovery_returns_the_stable_retained_code_and_keeps_the_journal() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let mut store = PersistentStore::open(root.path()).unwrap();
    let operation = context(OPERATION_ID);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = fixture_transport(root.path(), 17, 5);

    assert_eq!(
        recover_delta_completion(&mut store, root.path(), &mut transport).unwrap_err(),
        super::PeerSyncError::Validation(DELTA_COMPLETION_AMBIGUOUS.to_owned())
    );
    assert_eq!((transport.prepares, transport.deliveries), (0, 0));
    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_some());
}

#[test]
fn retained_delta_completion_reports_nothing_without_a_journal() {
    let root = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();

    assert_eq!(
        retained_delta_completion(&mut store, root.path()).unwrap(),
        None
    );
}

#[test]
fn retained_delta_completion_names_the_source_and_classifies_every_witness() {
    let ambiguous_root = tempfile::tempdir().unwrap();
    let mut ambiguous_store = PersistentStore::open(ambiguous_root.path()).unwrap();
    let ambiguous = context(OPERATION_ID);
    PeerDeltaCompletionJournal::new(ambiguous_root.path())
        .store_activation_intent(&ambiguous)
        .unwrap();

    let retained = retained_delta_completion(&mut ambiguous_store, ambiguous_root.path())
        .unwrap()
        .unwrap();
    assert_eq!(retained.witness, RetainedDeltaWitness::Ambiguous);
    // An unregistered source has no name to report.
    assert_eq!(retained.source_name, None);
    assert_eq!(retained.operation_id, OPERATION_ID);
    assert_eq!(retained.source_device_id, SOURCE_ID);
    assert_eq!(
        (retained.transferred_objects, retained.transferred_bytes),
        (1, 17)
    );
    // Reading the state never consumes it.
    assert!(PeerDeltaCompletionJournal::new(ambiguous_root.path())
        .load()
        .unwrap()
        .is_some());

    let uncommitted_root = tempfile::tempdir().unwrap();
    register_source(uncommitted_root.path(), 5);
    let mut uncommitted_store = PersistentStore::open(uncommitted_root.path()).unwrap();
    let mut uncommitted = context(OPERATION_ID);
    uncommitted.pre_revision = 0;
    uncommitted.pre_common_base = None;
    uncommitted.post_revision = 1;
    PeerDeltaCompletionJournal::new(uncommitted_root.path())
        .store_activation_intent(&uncommitted)
        .unwrap();

    let retained = retained_delta_completion(&mut uncommitted_store, uncommitted_root.path())
        .unwrap()
        .unwrap();
    assert_eq!(retained.witness, RetainedDeltaWitness::Uncommitted);
    assert_eq!(retained.source_name.as_deref(), Some("source"));

    let committed_root = tempfile::tempdir().unwrap();
    register_source(committed_root.path(), 5);
    let (mut committed_store, _cas, committed) = committed_context(committed_root.path());
    PeerDeltaCompletionJournal::new(committed_root.path())
        .store_activation_intent(&committed)
        .unwrap();

    let retained = retained_delta_completion(&mut committed_store, committed_root.path())
        .unwrap()
        .unwrap();
    assert_eq!(retained.witness, RetainedDeltaWitness::Committed);
    assert_eq!(retained.source_name.as_deref(), Some("source"));
}

#[test]
fn abandoning_an_ambiguous_retention_unblocks_pull_removal_and_registration() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let mut store = PersistentStore::open(root.path()).unwrap();
    let operation = context(OPERATION_ID);
    let mut job = DurableCasJob::begin(
        root.path(),
        &job_id(&operation),
        CasJobKind::LogicalDeltaTarget,
        1,
    )
    .unwrap();
    job.seal(&mut store, 1).unwrap();
    let job_journal = job.journal_path().to_path_buf();
    drop(job);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    prepare_incoming_completion_delivery(root.path(), pending_delivery(&operation, 7)).unwrap();
    assert_eq!(
        retained_delta_completion(&mut store, root.path())
            .unwrap()
            .unwrap()
            .witness,
        RetainedDeltaWitness::Ambiguous
    );
    let revision_before = store.revision().unwrap();
    let common_base_before = store
        .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, SOURCE_ID)
        .unwrap();

    abandon_retained_delta_completion(root.path(), OPERATION_ID).unwrap();

    assert_eq!(
        retained_delta_completion(&mut store, root.path()).unwrap(),
        None
    );
    assert!(!job_journal.exists());
    assert!(DurableCasJob::open(root.path(), &job_id(&operation)).is_err());
    assert!(
        snapshot_incoming_completion_delivery(root.path(), SOURCE_ID, CompletionLane::Delta)
            .unwrap()
            .is_none()
    );
    assert!(!registered_delta_source_is_active(root.path(), Some(SOURCE_ID)).unwrap());
    // The cancellation is local cleanup only: the data this device holds is
    // exactly what it held before.
    assert_eq!(store.revision().unwrap(), revision_before);
    assert_eq!(
        store
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, SOURCE_ID)
            .unwrap(),
        common_base_before
    );

    let lifecycle = lock_registered_source_lifecycle().unwrap();
    ensure_no_active_registered_source_work(&lifecycle, root.path()).unwrap();
    drop(lifecycle);
    remove_incoming_source_if_inactive(root.path(), SOURCE_ID).unwrap();
    assert!(incoming_source_by_id(root.path(), SOURCE_ID)
        .unwrap()
        .is_none());
}

#[test]
fn abandoning_refuses_a_mismatched_operation_and_spares_another_operations_delivery() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let operation = context(OPERATION_ID);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let other = pending_delivery(&context(OTHER_OPERATION_ID), 9);
    prepare_incoming_completion_delivery(root.path(), other.clone()).unwrap();

    assert!(abandon_retained_delta_completion(root.path(), OTHER_OPERATION_ID).is_err());
    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_some());

    abandon_retained_delta_completion(root.path(), OPERATION_ID).unwrap();

    // The lease belongs to another operation, so it is not this one's to drop.
    assert_eq!(
        snapshot_incoming_completion_delivery(root.path(), SOURCE_ID, CompletionLane::Delta)
            .unwrap()
            .unwrap()
            .delivery,
        other
    );
    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_none());
    // Nothing retained is an idempotent success, not a failure.
    abandon_retained_delta_completion(root.path(), OPERATION_ID).unwrap();
}

#[test]
fn delta_completion_witness_prefers_post_and_tolerates_later_local_revisions() {
    let operation = context(OPERATION_ID);

    assert_eq!(
        operation.classify_witness(8, Some(&operation.post_common_base)),
        DeltaCommitWitness::Committed
    );
    assert_eq!(
        operation.classify_witness(7, operation.pre_common_base.as_ref()),
        DeltaCommitWitness::Uncommitted
    );
    assert_eq!(
        operation.classify_witness(8, Some(&identity("other", 'c', "3"))),
        DeltaCommitWitness::Unknown
    );

    let mut already_active = operation;
    already_active.pre_revision = already_active.post_revision;
    already_active.pre_common_base = Some(already_active.post_common_base.clone());
    assert_eq!(
        already_active.classify_witness(
            already_active.post_revision,
            Some(&already_active.post_common_base),
        ),
        DeltaCommitWitness::Committed
    );
}

#[test]
fn delta_completion_journal_rejects_noncanonical_and_oversize_records() {
    let root = tempfile::tempdir().unwrap();
    let journal = PeerDeltaCompletionJournal::new(root.path());
    let mut invalid = context(OPERATION_ID);
    invalid.operation_id = "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA".to_owned();
    assert!(journal.store_activation_intent(&invalid).is_err());

    let peer_delta = root.path().join("peer-delta");
    std::fs::create_dir_all(&peer_delta).unwrap();
    std::fs::write(
        peer_delta.join("completion-operation.json"),
        vec![b'x'; 16 * 1024 + 1],
    )
    .unwrap();
    assert!(journal.load().is_err());
}

#[cfg(unix)]
#[test]
fn delta_completion_journal_rejects_symlink_records() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let peer_delta = root.path().join("peer-delta");
    std::fs::create_dir_all(&peer_delta).unwrap();
    let external = root.path().join("external.json");
    std::fs::write(&external, b"{}").unwrap();
    symlink(external, peer_delta.join("completion-operation.json")).unwrap();

    assert!(PeerDeltaCompletionJournal::new(root.path()).load().is_err());
}

#[cfg(unix)]
#[test]
fn delta_completion_journal_rejects_symlink_root() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    symlink(external.path(), root.path().join("peer-delta")).unwrap();

    assert!(PeerDeltaCompletionJournal::new(root.path()).load().is_err());
}
