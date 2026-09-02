use super::delta_completion::{
    complete_delta_accounting, recover_delta_completion, unlink_canonical_with_sync,
    DeltaCommitWitness, DeltaCompletionContext, DeltaCompletionMode, DeltaCompletionTransport,
    PeerDeltaCompletionJournal, PeerDeltaDurableCompletion,
};
use super::device_registry::{
    CompletionLane, DevicePermissions, IncomingSource, IncomingSourceRegistry,
    PendingCompletionDelivery,
};
use crate::{
    asset_repository::{
        job_pins::{CasJobKind, DurableCasJob},
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
        durable_job_id: format!("p4-delta-target-{operation_id}"),
        pre_revision: 4,
        pre_common_base: Some(identity("base", 'a', "1")),
        post_revision: 5,
        post_common_base: identity("remote", 'b', "2"),
        transferred_objects: 1,
        transferred_bytes: 17,
    }
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
    prepares: usize,
    deliveries: usize,
}

impl DeltaCompletionTransport for FixtureTransport {
    fn prepare(
        &mut self,
        _source: &IncomingSource,
        _context: &DeltaCompletionContext,
    ) -> Result<u64, super::PeerSyncError> {
        self.prepares += 1;
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

#[test]
fn delta_completion_journal_is_single_flight_and_source_prepare_is_retryable() {
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
    assert!(journal.store_activation_intent(&second).is_err());

    journal.store_source_prepared(&first, 17).unwrap();
    journal.store_source_prepared(&first, 17).unwrap();
    assert!(journal.store_source_prepared(&first, 18).is_err());
    assert_eq!(
        journal.load().unwrap(),
        Some(PeerDeltaDurableCompletion::SourcePrepared {
            context: first.clone(),
            useful_bytes: 17,
        })
    );

    journal.remove_exact(&first).unwrap();
    journal.remove_exact(&first).unwrap();
    assert_eq!(journal.load().unwrap(), None);
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
fn source_prepared_persists_authoritative_bytes_across_a_retried_transfer() {
    let root = tempfile::tempdir().unwrap();
    let journal = PeerDeltaCompletionJournal::new(root.path());
    let mut operation = context(OPERATION_ID);
    operation.transferred_bytes = 0;

    journal.store_activation_intent(&operation).unwrap();
    journal.store_source_prepared(&operation, 17).unwrap();

    assert_eq!(
        journal.load().unwrap(),
        Some(PeerDeltaDurableCompletion::SourcePrepared {
            context: operation,
            useful_bytes: 17,
        })
    );
}

#[test]
fn v1_completion_uses_source_bytes_and_finalizes_remote_first_once() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let mut operation = context(OPERATION_ID);
    operation.transferred_bytes = 0;
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = FixtureTransport {
        root: root.path().to_owned(),
        prepared_bytes: 17,
        expected_before_delivery: 5,
        prepares: 0,
        deliveries: 0,
    };

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
fn v1_completion_overflow_retains_source_proof_and_pending_outbox() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), u64::MAX);
    let operation = context(OPERATION_ID);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = FixtureTransport {
        root: root.path().to_owned(),
        prepared_bytes: 1,
        expected_before_delivery: u64::MAX,
        prepares: 0,
        deliveries: 0,
    };

    assert!(complete_delta_accounting(root.path(), &mut transport).is_err());
    assert_eq!((transport.prepares, transport.deliveries), (1, 1));
    assert!(matches!(
        PeerDeltaCompletionJournal::new(root.path()).load().unwrap(),
        Some(PeerDeltaDurableCompletion::SourcePrepared {
            useful_bytes: 1,
            ..
        })
    ));
    let registry = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(registry.sources()[0].total_bytes, u64::MAX);
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
    let mut transport = FixtureTransport {
        root: root.path().to_owned(),
        prepared_bytes: 99,
        expected_before_delivery: 5,
        prepares: 0,
        deliveries: 0,
    };

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
        &operation.durable_job_id,
        CasJobKind::LogicalDeltaTarget,
        1,
    )
    .unwrap();
    job.seal(&mut store, 1).unwrap();
    drop(job);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = FixtureTransport {
        root: root.path().to_owned(),
        prepared_bytes: 17,
        expected_before_delivery: 5,
        prepares: 0,
        deliveries: 0,
    };

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
    assert!(DurableCasJob::open(root.path(), &operation.durable_job_id).is_err());
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
fn committed_recovery_counts_then_releases_job_and_journal_once() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let (mut store, _cas, operation) = committed_context(root.path());
    let mut job = DurableCasJob::begin(
        root.path(),
        &operation.durable_job_id,
        CasJobKind::LogicalDeltaTarget,
        1,
    )
    .unwrap();
    job.seal(&mut store, 1).unwrap();
    drop(job);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = FixtureTransport {
        root: root.path().to_owned(),
        prepared_bytes: 99,
        expected_before_delivery: 5,
        prepares: 0,
        deliveries: 0,
    };

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
    assert!(DurableCasJob::open(root.path(), &operation.durable_job_id).is_err());
    assert!(
        recover_delta_completion(&mut store, root.path(), &mut transport)
            .unwrap()
            .is_none()
    );
}

#[test]
fn committed_recovery_overflow_retains_job_and_journal_without_success() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), u64::MAX);
    let (mut store, _cas, operation) = committed_context(root.path());
    let mut job = DurableCasJob::begin(
        root.path(),
        &operation.durable_job_id,
        CasJobKind::LogicalDeltaTarget,
        1,
    )
    .unwrap();
    job.seal(&mut store, 1).unwrap();
    drop(job);
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = FixtureTransport {
        root: root.path().to_owned(),
        prepared_bytes: 99,
        expected_before_delivery: u64::MAX,
        prepares: 0,
        deliveries: 0,
    };

    assert!(recover_delta_completion(&mut store, root.path(), &mut transport).is_err());
    assert_eq!((transport.prepares, transport.deliveries), (0, 0));
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        u64::MAX
    );
    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_some());
    let retained = DurableCasJob::open(root.path(), &operation.durable_job_id).unwrap();
    assert!(retained.is_sealed());
    assert!(!retained.is_released());
}

#[test]
fn committed_recovery_requires_job_until_exact_target_receipt_is_durable() {
    let root = tempfile::tempdir().unwrap();
    register_source(root.path(), 5);
    let (mut store, _cas, operation) = committed_context(root.path());
    PeerDeltaCompletionJournal::new(root.path())
        .store_activation_intent(&operation)
        .unwrap();
    let mut transport = FixtureTransport {
        root: root.path().to_owned(),
        prepared_bytes: 99,
        expected_before_delivery: 5,
        prepares: 0,
        deliveries: 0,
    };

    assert!(recover_delta_completion(&mut store, root.path(), &mut transport).is_err());
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        5
    );
    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_some());

    complete_delta_accounting(root.path(), &mut transport).unwrap();
    assert!(
        recover_delta_completion(&mut store, root.path(), &mut transport)
            .unwrap()
            .is_some()
    );
    assert!(PeerDeltaCompletionJournal::new(root.path())
        .load()
        .unwrap()
        .is_none());
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
