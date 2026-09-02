use super::{
    device_registry::{
        accept_outgoing_completion_offer, fail_next_outgoing_registry_write_for_test,
        issue_outgoing_unmeasured_completion_offer, CompletionAcceptance, CompletionLane,
        CompletionSealStatus, DevicePermissions, OutgoingDevice, OutgoingDeviceRegistry,
    },
    logical_completion::{
        begin_outgoing_logical_object_issue,
        fail_next_logical_completion_proof_delete_sync_for_test,
        fail_next_logical_completion_proof_write_for_test,
        freeze_outgoing_logical_completion_proof as freeze_coordinated,
        invalidate_outgoing_logical_completion_proofs,
        invalidate_outgoing_logical_completion_state,
        issue_outgoing_logical_completion_lease as issue_coordinated,
        logical_completion_proof_directory_sync_attempts_for_test,
        maximum_logical_completion_proof_size_for_test, outgoing_bidirectional_local_proof_bytes,
        prepare_outgoing_logical_completion_proof, record_outgoing_logical_progress,
        seal_outgoing_bidirectional_logical_completion as seal_bidirectional_coordinated,
        seal_outgoing_delta_logical_completion, LogicalCompletionManifest,
        LogicalCompletionProofProgress, OutgoingLogicalIssuedObjects,
    },
};
use std::{collections::BTreeMap, fs};

#[cfg(windows)]
use super::logical_completion::fail_next_logical_completion_tombstone_cleanup_for_test;

const DEVICE_ID: &str = "00000000-0000-4000-8000-000000000201";
const MANIFEST_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn registered_root(permissions: DevicePermissions) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    registry
        .upsert(OutgoingDevice {
            device_id: DEVICE_ID.to_owned(),
            name: "Target".to_owned(),
            bearer_digest: "b".repeat(64),
            permissions,
            created_at_ms: 1,
            last_seen_ms: 1,
            total_bytes: 0,
        })
        .unwrap();
    registry.save().unwrap();
    root
}

fn objects() -> LogicalCompletionManifest {
    LogicalCompletionManifest::new(BTreeMap::from([("1".repeat(64), 5), ("2".repeat(64), 7)]))
        .unwrap()
}

fn prepare(
    root: &tempfile::TempDir,
    lane: CompletionLane,
    manifest: &LogicalCompletionManifest,
) -> String {
    let lease =
        issue_outgoing_unmeasured_completion_offer(root.path(), DEVICE_ID, lane, MANIFEST_ID, None)
            .unwrap();
    prepare_outgoing_logical_completion_proof(
        root.path(),
        DEVICE_ID,
        lane,
        lease.as_str(),
        MANIFEST_ID,
        manifest,
    )
    .unwrap();
    lease.as_str().to_owned()
}

fn issue(issued: &OutgoingLogicalIssuedObjects, lane: CompletionLane, lease: &str, object: &str) {
    let manifest = objects();
    issued
        .mark_after_full_response(DEVICE_ID, lane, lease, MANIFEST_ID, object, &manifest)
        .unwrap();
}

fn freeze_outgoing_logical_completion_proof(
    app_root: &std::path::Path,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
) -> Result<u64, super::PeerSyncError> {
    freeze_coordinated(
        app_root,
        device_id,
        lane,
        lease_id,
        manifest_id,
        &OutgoingLogicalIssuedObjects::default(),
    )
}

fn issue_outgoing_logical_completion_lease(
    app_root: &std::path::Path,
    device_id: &str,
    lane: CompletionLane,
    manifest_id: &str,
    resume_lease_id: Option<&str>,
    manifest: &LogicalCompletionManifest,
) -> Result<super::device_registry::CompletionLeaseId, super::PeerSyncError> {
    issue_coordinated(
        app_root,
        device_id,
        lane,
        manifest_id,
        resume_lease_id,
        manifest,
        &OutgoingLogicalIssuedObjects::default(),
    )
}

fn seal_outgoing_bidirectional_logical_completion(
    app_root: &std::path::Path,
    device_id: &str,
    lease_id: &str,
    manifest_id: &str,
    remote_apply_bytes: u64,
) -> Result<CompletionSealStatus, super::PeerSyncError> {
    seal_bidirectional_coordinated(
        app_root,
        device_id,
        lease_id,
        manifest_id,
        remote_apply_bytes,
        &OutgoingLogicalIssuedObjects::default(),
    )
}

#[test]
fn proof_counts_each_source_manifest_object_once_and_persists_before_success() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    let issued = OutgoingLogicalIssuedObjects::default();
    let first = "1".repeat(64);
    let second = "2".repeat(64);

    issue(&issued, CompletionLane::Delta, &lease, &first);
    assert_eq!(
        record_outgoing_logical_progress(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
            Some(&first),
            &manifest,
            &issued,
        )
        .unwrap(),
        LogicalCompletionProofProgress::Partial { verified_bytes: 5 }
    );

    let restarted_process = OutgoingLogicalIssuedObjects::default();
    assert_eq!(
        record_outgoing_logical_progress(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
            Some(&first),
            &manifest,
            &restarted_process,
        )
        .unwrap(),
        LogicalCompletionProofProgress::Partial { verified_bytes: 5 }
    );

    issue(&restarted_process, CompletionLane::Delta, &lease, &second);
    assert_eq!(
        record_outgoing_logical_progress(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
            Some(&second),
            &manifest,
            &restarted_process,
        )
        .unwrap(),
        LogicalCompletionProofProgress::Partial { verified_bytes: 12 }
    );
    assert_eq!(
        freeze_outgoing_logical_completion_proof(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
        )
        .unwrap(),
        12
    );
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
            12,
        )
        .unwrap(),
        CompletionAcceptance::Rejected
    );
    seal_outgoing_delta_logical_completion(root.path(), DEVICE_ID, &lease, MANIFEST_ID, 12)
        .unwrap();
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
            12,
        )
        .unwrap(),
        CompletionAcceptance::Recorded
    );
}

#[test]
fn unissued_progress_fails_closed_until_the_target_retries_the_object_get() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    let first = "1".repeat(64);
    let before_restart = OutgoingLogicalIssuedObjects::default();
    issue(&before_restart, CompletionLane::Delta, &lease, &first);

    let after_restart = OutgoingLogicalIssuedObjects::default();
    assert!(record_outgoing_logical_progress(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        Some(&first),
        &manifest,
        &after_restart,
    )
    .is_err());

    issue(&after_restart, CompletionLane::Delta, &lease, &first);
    assert_eq!(
        record_outgoing_logical_progress(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
            Some(&first),
            &manifest,
            &after_restart,
        )
        .unwrap(),
        LogicalCompletionProofProgress::Partial { verified_bytes: 5 }
    );
}

#[test]
fn proof_rejects_wrong_device_lane_lease_manifest_and_object() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    let issued = OutgoingLogicalIssuedObjects::default();
    let object = "1".repeat(64);
    issue(&issued, CompletionLane::Delta, &lease, &object);

    for (device, lane, candidate_lease, candidate_manifest, candidate_object) in [
        (
            "00000000-0000-4000-8000-000000000202",
            CompletionLane::Delta,
            lease.as_str(),
            MANIFEST_ID,
            object.as_str(),
        ),
        (
            DEVICE_ID,
            CompletionLane::Bidirectional,
            lease.as_str(),
            MANIFEST_ID,
            object.as_str(),
        ),
        (
            DEVICE_ID,
            CompletionLane::Delta,
            "00000000-0000-4000-8000-000000000203",
            MANIFEST_ID,
            object.as_str(),
        ),
        (
            DEVICE_ID,
            CompletionLane::Delta,
            lease.as_str(),
            "c".repeat(64).leak(),
            object.as_str(),
        ),
        (
            DEVICE_ID,
            CompletionLane::Delta,
            lease.as_str(),
            MANIFEST_ID,
            "d".repeat(64).leak(),
        ),
    ] {
        assert!(record_outgoing_logical_progress(
            root.path(),
            device,
            lane,
            candidate_lease,
            candidate_manifest,
            Some(candidate_object),
            &manifest,
            &issued,
        )
        .is_err());
    }
}

#[test]
fn proof_rejects_a_version_four_lease_without_the_rfc4122_variant() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let invalid_lease = "00000000-0000-4000-0000-000000000203";

    assert!(prepare_outgoing_logical_completion_proof(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        invalid_lease,
        MANIFEST_ID,
        &manifest,
    )
    .is_err());
    assert!(issue_outgoing_logical_completion_lease(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        MANIFEST_ID,
        Some(invalid_lease),
        &manifest,
    )
    .is_err());
}

#[test]
fn zero_object_delta_manifest_seals_exactly_zero_bytes() {
    let root = registered_root(DevicePermissions::read());
    let manifest = LogicalCompletionManifest::new(BTreeMap::new()).unwrap();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);

    assert_eq!(
        freeze_outgoing_logical_completion_proof(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
        )
        .unwrap(),
        0
    );
    seal_outgoing_delta_logical_completion(root.path(), DEVICE_ID, &lease, MANIFEST_ID, 0).unwrap();
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
            0,
        )
        .unwrap(),
        CompletionAcceptance::Recorded
    );
}

#[test]
fn bidirectional_remote_apply_freezes_local_bytes_and_seals_the_exact_total() {
    let root = registered_root(DevicePermissions::read_and_bidirectional());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Bidirectional, &manifest);
    let issued = OutgoingLogicalIssuedObjects::default();
    for object in ["1".repeat(64), "2".repeat(64)] {
        issue(&issued, CompletionLane::Bidirectional, &lease, &object);
        record_outgoing_logical_progress(
            root.path(),
            DEVICE_ID,
            CompletionLane::Bidirectional,
            &lease,
            MANIFEST_ID,
            Some(&object),
            &manifest,
            &issued,
        )
        .unwrap();
    }
    assert!(outgoing_bidirectional_local_proof_bytes(
        root.path(),
        DEVICE_ID,
        &lease,
        MANIFEST_ID,
        &manifest,
    )
    .is_err());
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            DEVICE_ID,
            CompletionLane::Bidirectional,
            &lease,
            MANIFEST_ID,
            12,
        )
        .unwrap(),
        CompletionAcceptance::Rejected
    );

    seal_outgoing_bidirectional_logical_completion(root.path(), DEVICE_ID, &lease, MANIFEST_ID, 9)
        .unwrap();
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            DEVICE_ID,
            CompletionLane::Bidirectional,
            &lease,
            MANIFEST_ID,
            21,
        )
        .unwrap(),
        CompletionAcceptance::Recorded
    );
    let next = issue_outgoing_logical_completion_lease(
        root.path(),
        DEVICE_ID,
        CompletionLane::Bidirectional,
        MANIFEST_ID,
        None,
        &manifest,
    )
    .unwrap();
    assert_ne!(next.as_str(), lease);
}

#[test]
fn bidirectional_consumers_reject_persisted_bytes_that_do_not_match_the_bitmap() {
    let root = registered_root(DevicePermissions::read_and_bidirectional());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Bidirectional, &manifest);
    let issued = OutgoingLogicalIssuedObjects::default();
    let object = "1".repeat(64);
    issue(&issued, CompletionLane::Bidirectional, &lease, &object);
    record_outgoing_logical_progress(
        root.path(),
        DEVICE_ID,
        CompletionLane::Bidirectional,
        &lease,
        MANIFEST_ID,
        Some(&object),
        &manifest,
        &issued,
    )
    .unwrap();
    freeze_outgoing_logical_completion_proof(
        root.path(),
        DEVICE_ID,
        CompletionLane::Bidirectional,
        &lease,
        MANIFEST_ID,
    )
    .unwrap();
    assert_eq!(
        outgoing_bidirectional_local_proof_bytes(
            root.path(),
            DEVICE_ID,
            &lease,
            MANIFEST_ID,
            &manifest,
        )
        .unwrap(),
        5
    );

    let proof_path = fs::read_dir(root.path().join("peer-sync/logical-completion-proofs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut proof: serde_json::Value =
        serde_json::from_slice(&fs::read(&proof_path).unwrap()).unwrap();
    proof["verifiedBytes"] = serde_json::json!(6);
    fs::write(&proof_path, serde_json::to_vec(&proof).unwrap()).unwrap();

    assert!(outgoing_bidirectional_local_proof_bytes(
        root.path(),
        DEVICE_ID,
        &lease,
        MANIFEST_ID,
        &manifest,
    )
    .is_err());
    assert!(seal_outgoing_bidirectional_logical_completion(
        root.path(),
        DEVICE_ID,
        &lease,
        MANIFEST_ID,
        9,
    )
    .is_err());
}

#[test]
fn stale_manifest_rotation_and_revoke_invalidate_old_proof() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    let issued = OutgoingLogicalIssuedObjects::default();
    let object = "1".repeat(64);
    issue(&issued, CompletionLane::Delta, &lease, &object);

    let next_manifest = "b".repeat(64);
    let next_objects =
        LogicalCompletionManifest::new(BTreeMap::from([("3".repeat(64), 13)])).unwrap();
    let next_lease = issue_outgoing_unmeasured_completion_offer(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &next_manifest,
        None,
    )
    .unwrap();
    prepare_outgoing_logical_completion_proof(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        next_lease.as_str(),
        &next_manifest,
        &next_objects,
    )
    .unwrap();
    assert!(record_outgoing_logical_progress(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        Some(&object),
        &manifest,
        &issued,
    )
    .is_err());

    issued
        .mark_after_full_response(
            DEVICE_ID,
            CompletionLane::Delta,
            next_lease.as_str(),
            &next_manifest,
            &object,
            &manifest,
        )
        .unwrap();

    invalidate_outgoing_logical_completion_proofs(root.path(), DEVICE_ID).unwrap();
    assert!(record_outgoing_logical_progress(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        next_lease.as_str(),
        &next_manifest,
        Some(&object),
        &manifest,
        &issued,
    )
    .is_err());
}

#[test]
fn frozen_proof_rejects_manifest_rotation_before_the_offer_changes() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    freeze_outgoing_logical_completion_proof(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
    )
    .unwrap();
    let next_manifest_id = "b".repeat(64);
    let next_manifest =
        LogicalCompletionManifest::new(BTreeMap::from([("3".repeat(64), 13)])).unwrap();

    assert!(issue_outgoing_logical_completion_lease(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &next_manifest_id,
        None,
        &next_manifest,
    )
    .is_err());
    assert_eq!(
        issue_outgoing_unmeasured_completion_offer(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            MANIFEST_ID,
            Some(&lease),
        )
        .unwrap()
        .as_str(),
        lease
    );
    assert_eq!(
        freeze_outgoing_logical_completion_proof(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
        )
        .unwrap(),
        0
    );
}

#[test]
fn frozen_proof_with_a_mismatched_registry_fails_before_further_mutation() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    freeze_outgoing_logical_completion_proof(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
    )
    .unwrap();
    let next_manifest_id = "b".repeat(64);
    let next_lease = issue_outgoing_unmeasured_completion_offer(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &next_manifest_id,
        None,
    )
    .unwrap();

    assert!(issue_outgoing_logical_completion_lease(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        MANIFEST_ID,
        Some(&lease),
        &manifest,
    )
    .is_err());
    assert_eq!(
        issue_outgoing_unmeasured_completion_offer(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &next_manifest_id,
            Some(next_lease.as_str()),
        )
        .unwrap(),
        next_lease
    );
    prepare_outgoing_logical_completion_proof(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        &manifest,
    )
    .unwrap();
}

#[test]
fn completed_frozen_proof_rotates_to_new_same_or_changed_manifest_work() {
    for next_manifest_id in [MANIFEST_ID.to_owned(), "b".repeat(64)] {
        let root = registered_root(DevicePermissions::read());
        let manifest = LogicalCompletionManifest::new(BTreeMap::new()).unwrap();
        let lease = prepare(&root, CompletionLane::Delta, &manifest);
        freeze_outgoing_logical_completion_proof(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
        )
        .unwrap();
        seal_outgoing_delta_logical_completion(root.path(), DEVICE_ID, &lease, MANIFEST_ID, 0)
            .unwrap();
        assert_eq!(
            accept_outgoing_completion_offer(
                root.path(),
                DEVICE_ID,
                CompletionLane::Delta,
                &lease,
                MANIFEST_ID,
                0,
            )
            .unwrap(),
            CompletionAcceptance::Recorded
        );

        let next = issue_outgoing_logical_completion_lease(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &next_manifest_id,
            None,
            &manifest,
        )
        .unwrap();
        assert_ne!(next.as_str(), lease);
        assert_eq!(
            freeze_outgoing_logical_completion_proof(
                root.path(),
                DEVICE_ID,
                CompletionLane::Delta,
                next.as_str(),
                &next_manifest_id,
            )
            .unwrap(),
            0
        );
        assert_eq!(
            OutgoingDeviceRegistry::load(root.path()).unwrap().devices()[0].total_bytes,
            0
        );
    }
}

#[test]
fn completed_frozen_proof_recovers_when_registry_rotation_precedes_proof_rewrite() {
    let root = registered_root(DevicePermissions::read());
    let manifest = LogicalCompletionManifest::new(BTreeMap::new()).unwrap();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    freeze_outgoing_logical_completion_proof(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
    )
    .unwrap();
    seal_outgoing_delta_logical_completion(root.path(), DEVICE_ID, &lease, MANIFEST_ID, 0).unwrap();
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
            0,
        )
        .unwrap(),
        CompletionAcceptance::Recorded
    );

    let next_manifest_id = "b".repeat(64);
    fail_next_logical_completion_proof_write_for_test(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
    )
    .unwrap();
    assert!(issue_outgoing_logical_completion_lease(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &next_manifest_id,
        None,
        &manifest,
    )
    .is_err());
    let next = issue_outgoing_unmeasured_completion_offer(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &next_manifest_id,
        None,
    )
    .unwrap();
    assert_eq!(
        issue_outgoing_logical_completion_lease(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &next_manifest_id,
            None,
            &manifest,
        )
        .unwrap(),
        next
    );
    assert_eq!(
        freeze_outgoing_logical_completion_proof(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            next.as_str(),
            &next_manifest_id,
        )
        .unwrap(),
        0
    );
}

#[test]
fn completed_frozen_proof_rejects_a_receipt_with_noncanonical_bytes() {
    let root = registered_root(DevicePermissions::read());
    let manifest = LogicalCompletionManifest::new(BTreeMap::new()).unwrap();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    freeze_outgoing_logical_completion_proof(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
    )
    .unwrap();
    seal_outgoing_delta_logical_completion(root.path(), DEVICE_ID, &lease, MANIFEST_ID, 0).unwrap();
    accept_outgoing_completion_offer(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        0,
    )
    .unwrap();

    let registry_path = root.path().join("peer-sync/devices.json");
    let mut registry: serde_json::Value =
        serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    registry["completedReceipts"][0]["transferredBytes"] = serde_json::json!(1);
    fs::write(&registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();

    assert!(issue_outgoing_logical_completion_lease(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &"b".repeat(64),
        None,
        &manifest,
    )
    .is_err());
}

#[test]
fn durable_sealed_bytes_make_registry_seal_failure_retryable_for_both_logical_lanes() {
    for lane in [CompletionLane::Delta, CompletionLane::Bidirectional] {
        let root = registered_root(DevicePermissions::read_and_bidirectional());
        let manifest = LogicalCompletionManifest::new(BTreeMap::new()).unwrap();
        let lease = prepare(&root, lane, &manifest);
        freeze_outgoing_logical_completion_proof(root.path(), DEVICE_ID, lane, &lease, MANIFEST_ID)
            .unwrap();
        fail_next_outgoing_registry_write_for_test(root.path()).unwrap();

        let first = match lane {
            CompletionLane::Delta => seal_outgoing_delta_logical_completion(
                root.path(),
                DEVICE_ID,
                &lease,
                MANIFEST_ID,
                0,
            ),
            CompletionLane::Bidirectional => seal_outgoing_bidirectional_logical_completion(
                root.path(),
                DEVICE_ID,
                &lease,
                MANIFEST_ID,
                9,
            ),
            CompletionLane::Clone => unreachable!(),
        };
        assert!(first.is_err());
        let retried = match lane {
            CompletionLane::Delta => seal_outgoing_delta_logical_completion(
                root.path(),
                DEVICE_ID,
                &lease,
                MANIFEST_ID,
                0,
            ),
            CompletionLane::Bidirectional => seal_outgoing_bidirectional_logical_completion(
                root.path(),
                DEVICE_ID,
                &lease,
                MANIFEST_ID,
                9,
            ),
            CompletionLane::Clone => unreachable!(),
        };
        assert_eq!(retried.unwrap(), CompletionSealStatus::Sealed);
    }
}

#[test]
fn freeze_rejects_active_and_pending_object_responses_for_both_lanes() {
    for lane in [CompletionLane::Delta, CompletionLane::Bidirectional] {
        let root = registered_root(DevicePermissions::read_and_bidirectional());
        let manifest = objects();
        let lease = prepare(&root, lane, &manifest);
        let coordinator = OutgoingLogicalIssuedObjects::default();
        let issue_guard = begin_outgoing_logical_object_issue(
            root.path(),
            DEVICE_ID,
            lane,
            &lease,
            MANIFEST_ID,
            &"1".repeat(64),
            &manifest,
            &coordinator,
        )
        .unwrap();
        assert!(freeze_coordinated(
            root.path(),
            DEVICE_ID,
            lane,
            &lease,
            MANIFEST_ID,
            &coordinator,
        )
        .is_err());
        issue_guard.mark_after_full_response().unwrap();
        assert!(freeze_coordinated(
            root.path(),
            DEVICE_ID,
            lane,
            &lease,
            MANIFEST_ID,
            &coordinator,
        )
        .is_err());

        record_outgoing_logical_progress(
            root.path(),
            DEVICE_ID,
            lane,
            &lease,
            MANIFEST_ID,
            Some(&"1".repeat(64)),
            &manifest,
            &coordinator,
        )
        .unwrap();
        assert_eq!(
            freeze_coordinated(
                root.path(),
                DEVICE_ID,
                lane,
                &lease,
                MANIFEST_ID,
                &coordinator,
            )
            .unwrap(),
            5
        );

        assert!(begin_outgoing_logical_object_issue(
            root.path(),
            DEVICE_ID,
            lane,
            &lease,
            MANIFEST_ID,
            &"1".repeat(64),
            &manifest,
            &coordinator,
        )
        .is_err());
    }
}

#[test]
fn cancelled_object_response_can_retry_the_same_lease_and_progress() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    let coordinator = OutgoingLogicalIssuedObjects::default();
    let object = "1".repeat(64);

    let active = begin_outgoing_logical_object_issue(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        &object,
        &manifest,
        &coordinator,
    )
    .unwrap();
    let next_manifest_id = "b".repeat(64);
    let next_manifest = LogicalCompletionManifest::new(BTreeMap::new()).unwrap();
    assert!(issue_coordinated(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &next_manifest_id,
        None,
        &next_manifest,
        &coordinator,
    )
    .is_err());
    assert!(issue_coordinated(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        MANIFEST_ID,
        Some(&lease),
        &manifest,
        &coordinator,
    )
    .is_err());
    drop(active);
    assert_eq!(
        issue_coordinated(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            MANIFEST_ID,
            Some(&lease),
            &manifest,
            &coordinator,
        )
        .unwrap()
        .as_str(),
        lease
    );
    begin_outgoing_logical_object_issue(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        &object,
        &manifest,
        &coordinator,
    )
    .unwrap()
    .mark_after_full_response()
    .unwrap();
    assert_eq!(
        issue_coordinated(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            MANIFEST_ID,
            None,
            &manifest,
            &coordinator,
        )
        .unwrap()
        .as_str(),
        lease
    );
    assert_eq!(
        issue_coordinated(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            MANIFEST_ID,
            Some(&lease),
            &manifest,
            &coordinator,
        )
        .unwrap()
        .as_str(),
        lease
    );
    begin_outgoing_logical_object_issue(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        &object,
        &manifest,
        &coordinator,
    )
    .unwrap()
    .mark_after_full_response()
    .unwrap();
    record_outgoing_logical_progress(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        Some(&object),
        &manifest,
        &coordinator,
    )
    .unwrap();
    assert_eq!(
        freeze_coordinated(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
            &coordinator,
        )
        .unwrap(),
        5
    );
}

#[test]
fn failed_progress_write_retains_pending_issue_for_retry() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    let coordinator = OutgoingLogicalIssuedObjects::default();
    let object = "1".repeat(64);
    begin_outgoing_logical_object_issue(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        &object,
        &manifest,
        &coordinator,
    )
    .unwrap()
    .mark_after_full_response()
    .unwrap();
    fail_next_logical_completion_proof_write_for_test(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
    )
    .unwrap();
    assert!(record_outgoing_logical_progress(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        Some(&object),
        &manifest,
        &coordinator,
    )
    .is_err());
    assert_eq!(
        record_outgoing_logical_progress(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
            Some(&object),
            &manifest,
            &coordinator,
        )
        .unwrap(),
        LogicalCompletionProofProgress::Partial { verified_bytes: 5 }
    );
}

#[test]
fn superseding_proof_rewrite_retains_pending_on_failure_then_discards_it_on_success() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    let coordinator = OutgoingLogicalIssuedObjects::default();
    let object = "1".repeat(64);
    begin_outgoing_logical_object_issue(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        &object,
        &manifest,
        &coordinator,
    )
    .unwrap()
    .mark_after_full_response()
    .unwrap();

    let next_manifest_id = "b".repeat(64);
    let next_object = "3".repeat(64);
    let next_manifest =
        LogicalCompletionManifest::new(BTreeMap::from([(next_object.clone(), 13)])).unwrap();
    fail_next_logical_completion_proof_write_for_test(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
    )
    .unwrap();
    assert!(issue_coordinated(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &next_manifest_id,
        None,
        &next_manifest,
        &coordinator,
    )
    .is_err());
    assert!(coordinator.has_pending_for_test(
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
    ));
    assert!(record_outgoing_logical_progress(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        Some(&object),
        &manifest,
        &coordinator,
    )
    .is_err());

    let next_lease = issue_coordinated(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &next_manifest_id,
        None,
        &next_manifest,
        &coordinator,
    )
    .unwrap();
    assert!(!coordinator.has_pending_for_test(
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
    ));
    begin_outgoing_logical_object_issue(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        next_lease.as_str(),
        &next_manifest_id,
        &next_object,
        &next_manifest,
        &coordinator,
    )
    .unwrap()
    .mark_after_full_response()
    .unwrap();
    record_outgoing_logical_progress(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        next_lease.as_str(),
        &next_manifest_id,
        Some(&next_object),
        &next_manifest,
        &coordinator,
    )
    .unwrap();
    assert_eq!(
        freeze_coordinated(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            next_lease.as_str(),
            &next_manifest_id,
            &coordinator,
        )
        .unwrap(),
        13
    );
}

#[test]
fn revoke_invalidates_active_and_pending_object_issuance() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    let coordinator = OutgoingLogicalIssuedObjects::default();
    let first = "1".repeat(64);
    let second = "2".repeat(64);
    begin_outgoing_logical_object_issue(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        &first,
        &manifest,
        &coordinator,
    )
    .unwrap()
    .mark_after_full_response()
    .unwrap();
    let active = begin_outgoing_logical_object_issue(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        &second,
        &manifest,
        &coordinator,
    )
    .unwrap();

    invalidate_outgoing_logical_completion_state(root.path(), DEVICE_ID, &coordinator).unwrap();
    assert!(!coordinator.has_pending_for_test(
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
    ));
    assert!(active.mark_after_full_response().is_err());
    assert!(record_outgoing_logical_progress(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        Some(&first),
        &manifest,
        &coordinator,
    )
    .is_err());
}

#[test]
fn proof_delete_directory_sync_failure_retries_even_after_the_file_is_gone() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let lease = prepare(&root, CompletionLane::Delta, &manifest);
    fail_next_logical_completion_proof_delete_sync_for_test(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
    )
    .unwrap();

    assert_eq!(
        logical_completion_proof_directory_sync_attempts_for_test(root.path()),
        0
    );
    assert!(invalidate_outgoing_logical_completion_proofs(root.path(), DEVICE_ID).is_err());
    assert_eq!(
        logical_completion_proof_directory_sync_attempts_for_test(root.path()),
        1
    );
    assert!(!root
        .path()
        .join(format!(
            "peer-sync/logical-completion-proofs/{DEVICE_ID}-delta.json"
        ))
        .exists());
    invalidate_outgoing_logical_completion_proofs(root.path(), DEVICE_ID).unwrap();
    assert_eq!(
        logical_completion_proof_directory_sync_attempts_for_test(root.path()),
        2
    );
    assert!(record_outgoing_logical_progress(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &lease,
        MANIFEST_ID,
        Some(&"1".repeat(64)),
        &manifest,
        &OutgoingLogicalIssuedObjects::default(),
    )
    .is_err());
}

#[cfg(windows)]
#[test]
fn windows_proof_revoke_rename_survives_cleanup_failure_and_retry() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let _lease = prepare(&root, CompletionLane::Delta, &manifest);
    let proof_root = root.path().join("peer-sync/logical-completion-proofs");
    fs::write(
        proof_root.join(format!("{DEVICE_ID}-delta.revoked")),
        b"stale tombstone",
    )
    .unwrap();
    fail_next_logical_completion_tombstone_cleanup_for_test(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
    )
    .unwrap();

    assert!(invalidate_outgoing_logical_completion_proofs(root.path(), DEVICE_ID).is_err());
    assert!(!proof_root.join(format!("{DEVICE_ID}-delta.json")).exists());
    assert!(proof_root
        .join(format!("{DEVICE_ID}-delta.revoked"))
        .exists());

    invalidate_outgoing_logical_completion_proofs(root.path(), DEVICE_ID).unwrap();
    assert!(!proof_root
        .join(format!("{DEVICE_ID}-delta.revoked"))
        .exists());
}

#[cfg(windows)]
#[test]
fn windows_proof_revoke_rejects_a_nonregular_tombstone_before_rename() {
    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let _lease = prepare(&root, CompletionLane::Delta, &manifest);
    let proof_root = root.path().join("peer-sync/logical-completion-proofs");
    fs::create_dir(proof_root.join(format!("{DEVICE_ID}-delta.revoked"))).unwrap();

    assert!(invalidate_outgoing_logical_completion_proofs(root.path(), DEVICE_ID).is_err());
    assert!(proof_root.join(format!("{DEVICE_ID}-delta.json")).exists());
}

#[test]
fn active_stream_for_one_tuple_does_not_block_another_lane() {
    let root = registered_root(DevicePermissions::read_and_bidirectional());
    let delta_manifest = objects();
    let delta_lease = prepare(&root, CompletionLane::Delta, &delta_manifest);
    let bidi_manifest = LogicalCompletionManifest::new(BTreeMap::new()).unwrap();
    let bidi_lease = prepare(&root, CompletionLane::Bidirectional, &bidi_manifest);
    let coordinator = OutgoingLogicalIssuedObjects::default();
    let guard = begin_outgoing_logical_object_issue(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        &delta_lease,
        MANIFEST_ID,
        &"1".repeat(64),
        &delta_manifest,
        &coordinator,
    )
    .unwrap();

    assert_eq!(
        freeze_coordinated(
            root.path(),
            DEVICE_ID,
            CompletionLane::Bidirectional,
            &bidi_lease,
            MANIFEST_ID,
            &coordinator,
        )
        .unwrap(),
        0
    );
    drop(guard);
}

#[test]
fn proof_rejects_overflow_and_malformed_or_oversize_files() {
    assert!(LogicalCompletionManifest::new(BTreeMap::from([
        ("1".repeat(64), u64::MAX),
        ("2".repeat(64), 1),
    ]))
    .is_err());

    let (maximum_serialized_size, proof_size_limit) =
        maximum_logical_completion_proof_size_for_test();
    assert!(maximum_serialized_size <= proof_size_limit);

    for bytes in [b"not-json".to_vec(), vec![b'x'; proof_size_limit + 1]] {
        let root = registered_root(DevicePermissions::read());
        let manifest = objects();
        let lease = prepare(&root, CompletionLane::Delta, &manifest);
        let proof = fs::read_dir(root.path().join("peer-sync/logical-completion-proofs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(proof, bytes).unwrap();
        assert!(prepare_outgoing_logical_completion_proof(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            &lease,
            MANIFEST_ID,
            &manifest,
        )
        .is_err());
    }
}

#[cfg(unix)]
#[test]
fn proof_rejects_symlink_files() {
    use std::os::unix::fs::symlink;

    let root = registered_root(DevicePermissions::read());
    let manifest = objects();
    let _lease = prepare(&root, CompletionLane::Delta, &manifest);
    let proof = fs::read_dir(root.path().join("peer-sync/logical-completion-proofs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(&proof).unwrap();
    let outside = root.path().join("outside");
    fs::write(&outside, b"{}").unwrap();
    symlink(outside, proof).unwrap();

    assert!(prepare_outgoing_logical_completion_proof(
        root.path(),
        DEVICE_ID,
        CompletionLane::Delta,
        "00000000-0000-4000-8000-000000000204",
        MANIFEST_ID,
        &manifest,
    )
    .is_err());
}
