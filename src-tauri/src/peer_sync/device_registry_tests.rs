use super::delta_completion::{DeltaCompletionContext, DeltaCompletionMode};
use super::device_registry::{
    accept_outgoing_completion_offer, finalize_incoming_completion_delivery,
    incoming_completed_operation_recorded_for_lane,
    incoming_completed_operation_recorded_for_lane_with_bytes, incoming_completion_is_durable,
    incoming_source_summaries, issue_outgoing_unmeasured_completion_offer,
    outgoing_device_summaries, prepare_incoming_completion_delivery,
    record_incoming_completed_operation, record_incoming_completed_operation_best_effort,
    record_incoming_completed_operation_once_for_lane, register_incoming_source,
    register_outgoing_claim, remove_incoming_source, revoke_outgoing_device,
    seal_outgoing_completion_lease, snapshot_incoming_completion_delivery,
    store_delta_completion_activation_intent, CompletionAcceptance,
    CompletionDeliveryFinalizeStatus, CompletionDeliveryPrepareStatus, CompletionLane,
    CompletionSealStatus, DevicePermissions, IncomingSource, IncomingSourceRegistry,
    OutgoingDevice, OutgoingDeviceRegistry, PendingCompletionDelivery,
};
use crate::persistent_store::SyncGenerationIdentity;
use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;

const SOURCE_ID: &str = "b8e9d6d7-6d4c-43d8-b00a-80c8f34478b6";
const TARGET_ID: &str = "5c39d09f-4b6d-4e21-9ad3-a674c4c1c9b0";

fn pending_delivery(
    lane: CompletionLane,
    lease_suffix: u8,
    manifest_byte: char,
    useful_bytes: u64,
) -> PendingCompletionDelivery {
    let completion_lease_id = format!("00000000-0000-4000-8000-{lease_suffix:012}");
    let manifest_id = manifest_byte.to_string().repeat(64);
    let receipt_id = super::device_registry::completion_receipt_id(
        lane.as_str(),
        &completion_lease_id,
        &manifest_id,
    );
    PendingCompletionDelivery {
        source_device_id: SOURCE_ID.to_owned(),
        lane: lane.as_str().to_owned(),
        completion_lease_id,
        manifest_id,
        useful_bytes,
        receipt_id,
    }
}

fn register_test_source(root: &std::path::Path, bearer: char, total_bytes: u64) {
    register_incoming_source(
        root,
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: bearer.to_string().repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            last_seen_ms: 25,
            total_bytes,
        },
    )
    .unwrap();
}

fn delta_completion_context() -> DeltaCompletionContext {
    let operation_id = "00000000-0000-4000-8000-000000000091";
    DeltaCompletionContext {
        operation_id: operation_id.to_owned(),
        source_device_id: SOURCE_ID.to_owned(),
        manifest_id: "b".repeat(64),
        mode: DeltaCompletionMode::CompletionV1,
        durable_job_id: format!("p4-delta-target-{operation_id}"),
        pre_revision: 4,
        pre_common_base: Some(SyncGenerationIdentity {
            generation_id: "base".to_owned(),
            manifest_hash: "a".repeat(64),
            generation_sequence: "1".to_owned(),
        }),
        post_revision: 5,
        post_common_base: SyncGenerationIdentity {
            generation_id: "remote".to_owned(),
            manifest_hash: "b".repeat(64),
            generation_sequence: "2".to_owned(),
        },
        transferred_objects: 1,
        transferred_bytes: 17,
    }
}

#[test]
fn delta_completion_intent_retains_registered_source_and_credential() {
    let root = tempfile::tempdir().unwrap();
    register_test_source(root.path(), 'c', 0);
    store_delta_completion_activation_intent(root.path(), &delta_completion_context()).unwrap();
    let before = fs::read(root.path().join("peer-sync/sources.json")).unwrap();

    assert!(remove_incoming_source(root.path(), SOURCE_ID).is_err());
    assert!(register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Rotated".into(),
            endpoint: "http://192.168.0.9:32145".into(),
            bearer: "d".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 99,
            total_bytes: 0,
        },
    )
    .is_err());
    assert_eq!(
        fs::read(root.path().join("peer-sync/sources.json")).unwrap(),
        before
    );

    register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Same credential".into(),
            endpoint: "http://192.168.0.9:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 99,
            total_bytes: 0,
        },
    )
    .unwrap();
}

#[test]
fn delta_completion_intent_requires_the_registered_source() {
    let root = tempfile::tempdir().unwrap();

    assert!(
        store_delta_completion_activation_intent(root.path(), &delta_completion_context()).is_err()
    );
    assert!(!root
        .path()
        .join("peer-delta/completion-operation.json")
        .exists());
}

#[test]
fn incoming_completion_delivery_is_remote_first_durable_and_retry_safe() {
    let root = tempfile::tempdir().unwrap();
    register_test_source(root.path(), 'c', 5);
    let delivery = pending_delivery(CompletionLane::Delta, 1, 'a', 11);

    assert_eq!(
        prepare_incoming_completion_delivery(root.path(), delivery.clone()).unwrap(),
        CompletionDeliveryPrepareStatus::Pending
    );
    let prepared_bytes = fs::read(root.path().join("peer-sync/sources.json")).unwrap();
    let restarted = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(restarted.sources()[0].total_bytes, 5);
    assert!(!restarted
        .has_completed_operation_for_lane(SOURCE_ID, CompletionLane::Delta, &delivery.receipt_id,)
        .unwrap());
    assert!(!incoming_completion_is_durable(root.path(), &delivery).unwrap());

    let snapshot =
        snapshot_incoming_completion_delivery(root.path(), SOURCE_ID, CompletionLane::Delta)
            .unwrap()
            .unwrap();
    assert_eq!(snapshot.source.device_id, SOURCE_ID);
    assert_eq!(snapshot.source.bearer, "c".repeat(64));
    assert_eq!(snapshot.delivery, delivery);

    assert_eq!(
        prepare_incoming_completion_delivery(root.path(), delivery.clone()).unwrap(),
        CompletionDeliveryPrepareStatus::Pending
    );
    assert_eq!(
        fs::read(root.path().join("peer-sync/sources.json")).unwrap(),
        prepared_bytes
    );

    assert_eq!(
        finalize_incoming_completion_delivery(root.path(), &delivery).unwrap(),
        CompletionDeliveryFinalizeStatus::Finalized
    );
    let finalized = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(finalized.sources()[0].total_bytes, 16);
    assert!(incoming_completion_is_durable(root.path(), &delivery).unwrap());
    assert!(
        snapshot_incoming_completion_delivery(root.path(), SOURCE_ID, CompletionLane::Delta,)
            .unwrap()
            .is_none()
    );

    assert_eq!(
        prepare_incoming_completion_delivery(root.path(), delivery.clone()).unwrap(),
        CompletionDeliveryPrepareStatus::AlreadyDurable
    );
    assert_eq!(
        finalize_incoming_completion_delivery(root.path(), &delivery).unwrap(),
        CompletionDeliveryFinalizeStatus::AlreadyFinalized
    );
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        16
    );
}

#[test]
fn pending_delivery_blocks_later_work_and_a_b_a_replay_does_not_recount_locally() {
    let root = tempfile::tempdir().unwrap();
    register_test_source(root.path(), 'c', 0);
    let operation_a = pending_delivery(CompletionLane::Delta, 1, 'a', 11);
    let operation_b = pending_delivery(CompletionLane::Delta, 2, 'b', 7);
    let operation_c = pending_delivery(CompletionLane::Delta, 3, 'c', 5);

    prepare_incoming_completion_delivery(root.path(), operation_a.clone()).unwrap();
    finalize_incoming_completion_delivery(root.path(), &operation_a).unwrap();
    prepare_incoming_completion_delivery(root.path(), operation_b.clone()).unwrap();
    finalize_incoming_completion_delivery(root.path(), &operation_b).unwrap();
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        18
    );

    assert_eq!(
        prepare_incoming_completion_delivery(root.path(), operation_a.clone()).unwrap(),
        CompletionDeliveryPrepareStatus::Pending
    );
    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        18
    );
    assert!(!incoming_completion_is_durable(root.path(), &operation_a).unwrap());
    assert!(!incoming_completion_is_durable(root.path(), &operation_b).unwrap());
    assert!(prepare_incoming_completion_delivery(root.path(), operation_c).is_err());
    assert_eq!(
        snapshot_incoming_completion_delivery(root.path(), SOURCE_ID, CompletionLane::Delta,)
            .unwrap()
            .unwrap()
            .delivery,
        operation_a
    );
}

#[test]
fn pending_delivery_fails_closed_for_removal_and_credential_rotation() {
    let root = tempfile::tempdir().unwrap();
    register_test_source(root.path(), 'c', 0);
    let delivery = pending_delivery(CompletionLane::Clone, 1, 'a', 9);
    prepare_incoming_completion_delivery(root.path(), delivery.clone()).unwrap();
    let before = fs::read(root.path().join("peer-sync/sources.json")).unwrap();

    assert!(remove_incoming_source(root.path(), SOURCE_ID).is_err());
    assert_eq!(
        fs::read(root.path().join("peer-sync/sources.json")).unwrap(),
        before
    );

    assert!(register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Rotated".into(),
            endpoint: "http://192.168.0.9:32145".into(),
            bearer: "d".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 99,
            total_bytes: 0,
        },
    )
    .is_err());
    assert_eq!(
        fs::read(root.path().join("peer-sync/sources.json")).unwrap(),
        before
    );

    register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Same credential".into(),
            endpoint: "http://192.168.0.9:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 99,
            total_bytes: 0,
        },
    )
    .unwrap();
    let snapshot =
        snapshot_incoming_completion_delivery(root.path(), SOURCE_ID, CompletionLane::Clone)
            .unwrap()
            .unwrap();
    assert_eq!(snapshot.source.endpoint, "http://192.168.0.9:32145");
    assert_eq!(snapshot.source.bearer, "c".repeat(64));
    assert_eq!(snapshot.delivery, delivery);
}

#[test]
fn unsupported_completion_records_once_without_creating_an_outbox() {
    let root = tempfile::tempdir().unwrap();
    register_test_source(root.path(), 'c', 4);
    let receipt_id = "a".repeat(64);

    record_incoming_completed_operation_once_for_lane(
        root.path(),
        SOURCE_ID,
        CompletionLane::Bidirectional,
        &receipt_id,
        9,
    )
    .unwrap();
    record_incoming_completed_operation_once_for_lane(
        root.path(),
        SOURCE_ID,
        CompletionLane::Bidirectional,
        &receipt_id,
        9,
    )
    .unwrap();

    assert_eq!(
        IncomingSourceRegistry::load(root.path()).unwrap().sources()[0].total_bytes,
        13
    );
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("peer-sync/sources.json")).unwrap())
            .unwrap();
    assert!(json.get("pendingCompletionDeliveries").is_none());

    record_incoming_completed_operation_once_for_lane(
        root.path(),
        SOURCE_ID,
        CompletionLane::Bidirectional,
        &receipt_id,
        10,
    )
    .unwrap();
    assert!(incoming_completed_operation_recorded_for_lane_with_bytes(
        root.path(),
        SOURCE_ID,
        CompletionLane::Bidirectional,
        &receipt_id,
        9,
    )
    .unwrap());
    assert!(!incoming_completed_operation_recorded_for_lane_with_bytes(
        root.path(),
        SOURCE_ID,
        CompletionLane::Bidirectional,
        &receipt_id,
        10,
    )
    .unwrap());
}

#[test]
fn pending_completion_json_is_exact_bounded_and_backward_compatible() {
    let root = tempfile::tempdir().unwrap();
    register_test_source(root.path(), 'c', 0);
    let clone = pending_delivery(CompletionLane::Clone, 1, 'a', 9);
    let delta = pending_delivery(CompletionLane::Delta, 2, 'b', 10);
    let bidirectional = pending_delivery(CompletionLane::Bidirectional, 3, 'c', 11);
    for delivery in [&clone, &delta, &bidirectional] {
        prepare_incoming_completion_delivery(root.path(), delivery.clone()).unwrap();
    }

    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("peer-sync/sources.json")).unwrap())
            .unwrap();
    let pending = json["pendingCompletionDeliveries"].as_array().unwrap();
    assert_eq!(pending.len(), 3);
    assert_eq!(
        pending[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "completionLeaseId",
            "lane",
            "manifestId",
            "receiptId",
            "sourceDeviceId",
            "usefulBytes",
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(pending[0]["sourceDeviceId"], SOURCE_ID);
    assert_eq!(pending[0]["lane"], "clone");
    assert_eq!(pending[0]["completionLeaseId"], clone.completion_lease_id);
    assert_eq!(pending[0]["manifestId"], clone.manifest_id);
    assert_eq!(pending[0]["usefulBytes"], 9);
    assert_eq!(pending[0]["receiptId"], clone.receipt_id);

    assert!(prepare_incoming_completion_delivery(
        root.path(),
        pending_delivery(CompletionLane::Clone, 4, 'd', 12),
    )
    .is_err());

    let legacy_root = tempfile::tempdir().unwrap();
    let peer_root = legacy_root.path().join("peer-sync");
    fs::create_dir_all(&peer_root).unwrap();
    fs::write(
        peer_root.join("sources.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": "risunest.peer-source-registry/v1",
            "sources": [{
                "deviceId": SOURCE_ID,
                "name": "Legacy",
                "endpoint": "http://192.168.0.5:32145",
                "bearer": "c".repeat(64),
                "permissions": ["read"],
                "lastSeenMs": 1,
                "totalBytes": 2
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        IncomingSourceRegistry::load(legacy_root.path())
            .unwrap()
            .sources()[0]
            .total_bytes,
        2
    );
}

#[test]
fn malformed_pending_completion_deliveries_are_rejected() {
    let source = serde_json::json!({
        "deviceId": SOURCE_ID,
        "name": "Android",
        "endpoint": "http://192.168.0.5:32145",
        "bearer": "c".repeat(64),
        "permissions": ["read", "bidirectional"],
        "lastSeenMs": 1,
        "totalBytes": 2
    });
    let valid = pending_delivery(CompletionLane::Delta, 1, 'a', 9);
    let valid_json = serde_json::to_value(&valid).unwrap();
    let mut cases = Vec::new();
    for (field, value) in [
        ("sourceDeviceId", serde_json::json!(TARGET_ID)),
        ("lane", serde_json::json!("other")),
        (
            "completionLeaseId",
            serde_json::json!("00000000-0000-1000-8000-000000000001"),
        ),
        ("manifestId", serde_json::json!("A".repeat(64))),
        ("receiptId", serde_json::json!("b".repeat(64))),
    ] {
        let mut invalid = valid_json.clone();
        invalid[field] = value;
        cases.push(vec![invalid]);
    }
    let invalid_variant_lease = "00000000-0000-4000-0000-000000000001";
    let mut invalid_variant = valid_json.clone();
    invalid_variant["completionLeaseId"] = serde_json::json!(invalid_variant_lease);
    invalid_variant["receiptId"] =
        serde_json::json!(super::device_registry::completion_receipt_id(
            CompletionLane::Delta.as_str(),
            invalid_variant_lease,
            &"a".repeat(64),
        ));
    cases.push(vec![invalid_variant]);
    cases.push(vec![valid_json.clone(), valid_json.clone()]);
    let mut unknown = valid_json;
    unknown["unexpected"] = serde_json::json!(true);
    cases.push(vec![unknown]);

    for pending in cases {
        let root = tempfile::tempdir().unwrap();
        let peer_root = root.path().join("peer-sync");
        fs::create_dir_all(&peer_root).unwrap();
        fs::write(
            peer_root.join("sources.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema": "risunest.peer-source-registry/v1",
                "sources": [source.clone()],
                "pendingCompletionDeliveries": pending
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(IncomingSourceRegistry::load(root.path()).is_err());
    }
}

#[test]
fn completion_delivery_prepare_and_finalize_failures_are_atomic() {
    let prepare_root = tempfile::tempdir().unwrap();
    register_test_source(prepare_root.path(), 'c', 0);
    let delivery = pending_delivery(CompletionLane::Clone, 1, 'a', 9);
    let mut registry = IncomingSourceRegistry::load(prepare_root.path()).unwrap();
    let registry_path = prepare_root.path().join("peer-sync/sources.json");
    let preserved_path = prepare_root.path().join("peer-sync/sources.preserved.json");
    let before_prepare = fs::read(&registry_path).unwrap();
    fs::rename(&registry_path, &preserved_path).unwrap();
    fs::create_dir(&registry_path).unwrap();
    assert!(registry
        .prepare_completion_delivery(delivery.clone())
        .is_err());
    assert_eq!(registry.sources()[0].total_bytes, 0);
    fs::remove_dir(&registry_path).unwrap();
    fs::rename(&preserved_path, &registry_path).unwrap();
    assert_eq!(fs::read(&registry_path).unwrap(), before_prepare);
    assert!(snapshot_incoming_completion_delivery(
        prepare_root.path(),
        SOURCE_ID,
        CompletionLane::Clone,
    )
    .unwrap()
    .is_none());

    let overflow_root = tempfile::tempdir().unwrap();
    register_test_source(overflow_root.path(), 'c', u64::MAX);
    prepare_incoming_completion_delivery(overflow_root.path(), delivery.clone()).unwrap();
    let before_overflow = fs::read(overflow_root.path().join("peer-sync/sources.json")).unwrap();
    let different_ack = pending_delivery(CompletionLane::Clone, 2, 'b', 9);
    assert!(finalize_incoming_completion_delivery(overflow_root.path(), &different_ack).is_err());
    assert_eq!(
        fs::read(overflow_root.path().join("peer-sync/sources.json")).unwrap(),
        before_overflow
    );
    assert!(finalize_incoming_completion_delivery(overflow_root.path(), &delivery).is_err());
    assert_eq!(
        fs::read(overflow_root.path().join("peer-sync/sources.json")).unwrap(),
        before_overflow
    );
    assert_eq!(
        snapshot_incoming_completion_delivery(
            overflow_root.path(),
            SOURCE_ID,
            CompletionLane::Clone,
        )
        .unwrap()
        .unwrap()
        .delivery,
        delivery
    );

    let write_root = tempfile::tempdir().unwrap();
    register_test_source(write_root.path(), 'c', 0);
    prepare_incoming_completion_delivery(write_root.path(), delivery.clone()).unwrap();
    let mut registry = IncomingSourceRegistry::load(write_root.path()).unwrap();
    let registry_path = write_root.path().join("peer-sync/sources.json");
    let preserved_path = write_root.path().join("peer-sync/sources.preserved.json");
    let before_finalize = fs::read(&registry_path).unwrap();
    fs::rename(&registry_path, &preserved_path).unwrap();
    fs::create_dir(&registry_path).unwrap();
    assert!(registry
        .finalize_completion_delivery(&delivery, 99)
        .is_err());
    assert_eq!(registry.sources()[0].total_bytes, 0);
    fs::remove_dir(&registry_path).unwrap();
    fs::rename(&preserved_path, &registry_path).unwrap();
    assert_eq!(fs::read(&registry_path).unwrap(), before_finalize);
    assert_eq!(
        snapshot_incoming_completion_delivery(write_root.path(), SOURCE_ID, CompletionLane::Clone,)
            .unwrap()
            .unwrap()
            .delivery,
        delivery
    );
}

#[test]
fn source_issued_completion_lease_blocks_a_b_a_replay_after_reload() {
    let root = tempfile::tempdir().unwrap();
    register_outgoing_claim(
        root.path(),
        OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Windows desktop".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 10,
            last_seen_ms: 0,
            total_bytes: 0,
        },
    )
    .unwrap();
    let manifest_id = "b".repeat(64);
    assert!(issue_outgoing_unmeasured_completion_offer(
        root.path(),
        TARGET_ID,
        CompletionLane::Bidirectional,
        &manifest_id,
        None,
    )
    .is_err());

    let operation_a = issue_outgoing_unmeasured_completion_offer(
        root.path(),
        TARGET_ID,
        CompletionLane::Delta,
        &manifest_id,
        None,
    )
    .unwrap();
    assert_eq!(
        issue_outgoing_unmeasured_completion_offer(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            &manifest_id,
            None,
        )
        .unwrap(),
        operation_a
    );
    assert_eq!(
        seal_outgoing_completion_lease(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            operation_a.as_str(),
            &manifest_id,
            11,
        )
        .unwrap(),
        CompletionSealStatus::Sealed
    );
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            operation_a.as_str(),
            &manifest_id,
            11,
        )
        .unwrap(),
        CompletionAcceptance::Recorded
    );
    register_outgoing_claim(
        root.path(),
        OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Renamed desktop".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 99,
            last_seen_ms: 99,
            total_bytes: 99,
        },
    )
    .unwrap();
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            operation_a.as_str(),
            &manifest_id,
            11,
        )
        .unwrap(),
        CompletionAcceptance::AlreadyRecorded
    );

    let operation_b = issue_outgoing_unmeasured_completion_offer(
        root.path(),
        TARGET_ID,
        CompletionLane::Delta,
        &manifest_id,
        None,
    )
    .unwrap();
    assert_ne!(operation_b, operation_a);
    assert_eq!(
        seal_outgoing_completion_lease(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            operation_b.as_str(),
            &manifest_id,
            7,
        )
        .unwrap(),
        CompletionSealStatus::Sealed
    );
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            operation_b.as_str(),
            &manifest_id,
            7,
        )
        .unwrap(),
        CompletionAcceptance::Recorded
    );

    assert_eq!(
        seal_outgoing_completion_lease(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            operation_a.as_str(),
            &manifest_id,
            11,
        )
        .unwrap(),
        CompletionSealStatus::Conflict
    );
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            operation_a.as_str(),
            &manifest_id,
            11,
        )
        .unwrap(),
        CompletionAcceptance::Rejected
    );
    assert_eq!(
        outgoing_device_summaries(root.path()).unwrap()[0].total_bytes,
        18
    );

    register_outgoing_claim(
        root.path(),
        OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Renamed again".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 100,
            last_seen_ms: 100,
            total_bytes: 100,
        },
    )
    .unwrap();
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            operation_b.as_str(),
            &manifest_id,
            7,
        )
        .unwrap(),
        CompletionAcceptance::AlreadyRecorded
    );
    assert_eq!(
        outgoing_device_summaries(root.path()).unwrap()[0].total_bytes,
        18
    );
}

#[test]
fn unmeasured_completion_lease_supersedes_only_prepared_work_and_resumes_ready_work() {
    let root = tempfile::tempdir().unwrap();
    register_outgoing_claim(
        root.path(),
        OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Windows desktop".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 10,
            last_seen_ms: 0,
            total_bytes: 0,
        },
    )
    .unwrap();
    let manifest_a = "a".repeat(64);
    let manifest_b = "b".repeat(64);
    let lease_a = issue_outgoing_unmeasured_completion_offer(
        root.path(),
        TARGET_ID,
        CompletionLane::Delta,
        &manifest_a,
        None,
    )
    .unwrap();
    let lease_b = issue_outgoing_unmeasured_completion_offer(
        root.path(),
        TARGET_ID,
        CompletionLane::Delta,
        &manifest_b,
        None,
    )
    .unwrap();
    assert_ne!(lease_a, lease_b);
    assert_eq!(
        seal_outgoing_completion_lease(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            lease_a.as_str(),
            &manifest_a,
            8,
        )
        .unwrap(),
        CompletionSealStatus::Conflict
    );
    assert_eq!(
        issue_outgoing_unmeasured_completion_offer(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            &manifest_b,
            None,
        )
        .unwrap(),
        lease_b
    );
    assert_eq!(
        seal_outgoing_completion_lease(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            lease_b.as_str(),
            &manifest_b,
            8,
        )
        .unwrap(),
        CompletionSealStatus::Sealed
    );
    assert!(issue_outgoing_unmeasured_completion_offer(
        root.path(),
        TARGET_ID,
        CompletionLane::Delta,
        &manifest_b,
        None,
    )
    .is_err());
    assert_eq!(
        issue_outgoing_unmeasured_completion_offer(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            &manifest_b,
            Some(lease_b.as_str()),
        )
        .unwrap(),
        lease_b
    );
    assert!(issue_outgoing_unmeasured_completion_offer(
        root.path(),
        TARGET_ID,
        CompletionLane::Delta,
        &manifest_a,
        None,
    )
    .is_err());
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            lease_b.as_str(),
            &manifest_b,
            8,
        )
        .unwrap(),
        CompletionAcceptance::Recorded
    );
    assert_eq!(
        issue_outgoing_unmeasured_completion_offer(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            &manifest_b,
            Some(lease_b.as_str()),
        )
        .unwrap(),
        lease_b
    );
    assert!(issue_outgoing_unmeasured_completion_offer(
        root.path(),
        TARGET_ID,
        CompletionLane::Delta,
        &manifest_b,
        Some(lease_a.as_str()),
    )
    .is_err());
    let next = issue_outgoing_unmeasured_completion_offer(
        root.path(),
        TARGET_ID,
        CompletionLane::Delta,
        &manifest_b,
        None,
    )
    .unwrap();
    assert_ne!(next, lease_b);
}

#[test]
fn completion_lease_issue_and_seal_write_failures_leave_persisted_state_unchanged() {
    let root = tempfile::tempdir().unwrap();
    register_outgoing_claim(
        root.path(),
        OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Windows desktop".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 10,
            last_seen_ms: 0,
            total_bytes: 0,
        },
    )
    .unwrap();
    let registry_path = root.path().join("peer-sync/devices.json");
    let preserved_path = root.path().join("peer-sync/devices.preserved.json");
    let before_issue = fs::read(&registry_path).unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    fs::rename(&registry_path, &preserved_path).unwrap();
    fs::create_dir(&registry_path).unwrap();
    assert!(registry
        .issue_completion_lease(
            TARGET_ID,
            CompletionLane::Delta,
            &"b".repeat(64),
            None,
            None,
        )
        .is_err());
    fs::remove_dir(&registry_path).unwrap();
    fs::rename(&preserved_path, &registry_path).unwrap();
    assert_eq!(fs::read(&registry_path).unwrap(), before_issue);

    let lease = issue_outgoing_unmeasured_completion_offer(
        root.path(),
        TARGET_ID,
        CompletionLane::Delta,
        &"b".repeat(64),
        None,
    )
    .unwrap();
    let before_seal = fs::read(&registry_path).unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    fs::rename(&registry_path, &preserved_path).unwrap();
    fs::create_dir(&registry_path).unwrap();
    assert!(registry
        .seal_completion_lease(
            TARGET_ID,
            CompletionLane::Delta,
            lease.as_str(),
            &"b".repeat(64),
            9,
        )
        .is_err());
    fs::remove_dir(&registry_path).unwrap();
    fs::rename(&preserved_path, &registry_path).unwrap();
    assert_eq!(fs::read(&registry_path).unwrap(), before_seal);
    assert_eq!(
        accept_outgoing_completion_offer(
            root.path(),
            TARGET_ID,
            CompletionLane::Delta,
            lease.as_str(),
            &"b".repeat(64),
            9,
        )
        .unwrap(),
        CompletionAcceptance::Rejected
    );
}

#[test]
fn completion_lease_issue_rejects_impossible_measurement_states() {
    let root = tempfile::tempdir().unwrap();
    register_outgoing_claim(
        root.path(),
        OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Windows desktop".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 10,
            last_seen_ms: 0,
            total_bytes: 0,
        },
    )
    .unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();

    assert!(registry
        .issue_completion_lease(
            TARGET_ID,
            CompletionLane::Clone,
            &"b".repeat(64),
            None,
            None,
        )
        .is_err());
    assert!(registry
        .issue_completion_lease(
            TARGET_ID,
            CompletionLane::Delta,
            &"b".repeat(64),
            Some(1),
            None,
        )
        .is_err());
}

#[test]
fn stable_device_id_migrates_once_and_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let legacy = root.path().join("peer-delta").join("source-device-id");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, format!("{SOURCE_ID}\n")).unwrap();

    let first = super::device_registry::load_or_create_device_id(root.path()).unwrap();
    let second = super::device_registry::load_or_create_device_id(root.path()).unwrap();

    assert_eq!(first, SOURCE_ID);
    assert_eq!(second, first);
    assert_eq!(
        fs::read_to_string(root.path().join("peer-sync/device-id")).unwrap(),
        first
    );
    assert!(!legacy.exists());
}

#[test]
fn outgoing_registry_persists_digest_permissions_last_seen_and_total_bytes() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    registry
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Windows desktop".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            created_at_ms: 10,
            last_seen_ms: 20,
            total_bytes: 30,
        })
        .unwrap();
    registry.record_completed_bytes(TARGET_ID, 12).unwrap();
    registry.save().unwrap();

    let restored = OutgoingDeviceRegistry::load(root.path()).unwrap();
    assert_eq!(restored.devices().len(), 1);
    assert_eq!(restored.devices()[0].last_seen_ms, 20);
    assert_eq!(restored.devices()[0].total_bytes, 42);
    assert!(restored.devices()[0].permissions.allows_bidirectional());
    let contents = fs::read_to_string(root.path().join("peer-sync/devices.json")).unwrap();
    assert!(contents.contains("risunest.peer-device-registry/v1"));
    assert!(!contents.contains("bearer\""));
}

#[test]
fn reregistering_a_claim_preserves_created_at_last_seen_and_total_bytes() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    registry
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Old name".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 10,
            last_seen_ms: 20,
            total_bytes: 30,
        })
        .unwrap();

    registry
        .register_claim(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "New name".into(),
            bearer_digest: "b".repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            created_at_ms: 99,
            last_seen_ms: 99,
            total_bytes: 99,
        })
        .unwrap();

    let registered = &registry.devices()[0];
    assert_eq!(registered.name, "New name");
    assert_eq!(registered.bearer_digest, "b".repeat(64));
    assert!(registered.permissions.allows_bidirectional());
    assert_eq!(registered.created_at_ms, 10);
    assert_eq!(registered.last_seen_ms, 20);
    assert_eq!(registered.total_bytes, 30);
}

#[test]
fn credential_rotation_requires_explicit_revoke_for_any_completion_state() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    registry
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Old name".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 10,
            last_seen_ms: 20,
            total_bytes: 30,
        })
        .unwrap();
    let manifest_id = "b".repeat(64);
    let lease = registry
        .issue_completion_lease(TARGET_ID, CompletionLane::Delta, &manifest_id, None, None)
        .unwrap();
    let rotated = OutgoingDevice {
        device_id: TARGET_ID.into(),
        name: "New name".into(),
        bearer_digest: "c".repeat(64),
        permissions: DevicePermissions::read(),
        created_at_ms: 99,
        last_seen_ms: 99,
        total_bytes: 99,
    };

    assert!(registry.register_claim(rotated.clone()).is_err());
    assert_eq!(registry.devices()[0].bearer_digest, "a".repeat(64));
    assert_eq!(
        registry
            .issue_completion_lease(
                TARGET_ID,
                CompletionLane::Delta,
                &manifest_id,
                None,
                Some(lease.as_str()),
            )
            .unwrap(),
        lease
    );

    assert_eq!(
        registry
            .seal_completion_lease(
                TARGET_ID,
                CompletionLane::Delta,
                lease.as_str(),
                &manifest_id,
                7,
            )
            .unwrap(),
        CompletionSealStatus::Sealed
    );
    assert_eq!(
        registry
            .accept_completion_offer(
                TARGET_ID,
                CompletionLane::Delta,
                lease.as_str(),
                &manifest_id,
                7,
                21,
            )
            .unwrap(),
        CompletionAcceptance::Recorded
    );
    assert!(registry.register_claim(rotated.clone()).is_err());
    assert_eq!(registry.devices()[0].bearer_digest, "a".repeat(64));
    assert_eq!(registry.devices()[0].total_bytes, 37);
    registry.remove(TARGET_ID).unwrap();
    registry.register_claim(rotated).unwrap();
    assert_eq!(registry.devices()[0].bearer_digest, "c".repeat(64));
}

#[test]
fn incoming_registry_stores_bearer_and_revoke_replaces_file_atomically() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = IncomingSourceRegistry::load(root.path()).unwrap();
    registry
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 25,
            total_bytes: 40,
        })
        .unwrap();
    registry.save().unwrap();
    assert!(
        fs::read_to_string(root.path().join("peer-sync/sources.json"))
            .unwrap()
            .contains(&"c".repeat(64))
    );

    registry.remove(SOURCE_ID).unwrap();
    registry.save().unwrap();
    assert!(IncomingSourceRegistry::load(root.path())
        .unwrap()
        .sources()
        .is_empty());
    assert!(!root.path().join("peer-sync/sources.json.tmp").exists());
}

#[test]
fn outgoing_completion_receipt_survives_restart_and_prevents_retry_double_counting() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    registry
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Target".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 10,
            last_seen_ms: 20,
            total_bytes: 30,
        })
        .unwrap();
    registry.save().unwrap();

    registry
        .record_completed_operation(TARGET_ID, "clone", &"b".repeat(64), 12, 40)
        .unwrap();
    let mut restarted = OutgoingDeviceRegistry::load(root.path()).unwrap();
    restarted
        .record_completed_operation(TARGET_ID, "clone", &"b".repeat(64), 12, 41)
        .unwrap();

    let restored = OutgoingDeviceRegistry::load(root.path()).unwrap();
    assert_eq!(restored.devices()[0].total_bytes, 42);
    assert_eq!(restored.devices()[0].last_seen_ms, 41);
}

#[test]
fn outgoing_receipts_are_not_evicted_while_their_devices_remain_registered() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    let device_ids = (1_u128..=257)
        .map(|value| uuid::Uuid::from_u128(value).to_string())
        .collect::<Vec<_>>();
    for device_id in &device_ids {
        registry
            .upsert(OutgoingDevice {
                device_id: device_id.clone(),
                name: "Target".into(),
                bearer_digest: "a".repeat(64),
                permissions: DevicePermissions::read(),
                created_at_ms: 10,
                last_seen_ms: 20,
                total_bytes: 0,
            })
            .unwrap();
    }
    registry.save().unwrap();

    for (index, device_id) in device_ids.iter().enumerate() {
        registry
            .record_completed_operation(device_id, "clone", &format!("{index:064x}"), 1, 40)
            .unwrap();
    }
    let mut restarted = OutgoingDeviceRegistry::load(root.path()).unwrap();
    restarted
        .record_completed_operation(&device_ids[0], "clone", &"0".repeat(64), 1, 41)
        .unwrap();

    let restored = OutgoingDeviceRegistry::load(root.path()).unwrap();
    assert_eq!(restored.devices()[0].total_bytes, 1);
    assert_eq!(restored.devices()[0].last_seen_ms, 41);
}

#[test]
fn reregistering_an_incoming_source_rotates_credentials_without_erasing_history() {
    let root = tempfile::tempdir().unwrap();
    register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Old source".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "a".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 25,
            total_bytes: 40,
        },
    )
    .unwrap();

    register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Renamed source".into(),
            endpoint: "http://192.168.0.6:32145".into(),
            bearer: "b".repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            last_seen_ms: 99,
            total_bytes: 0,
        },
    )
    .unwrap();

    let restored = IncomingSourceRegistry::load(root.path()).unwrap();
    let source = &restored.sources()[0];
    assert_eq!(source.name, "Renamed source");
    assert_eq!(source.endpoint, "http://192.168.0.6:32145");
    assert_eq!(source.bearer, "b".repeat(64));
    assert!(source.permissions.allows_bidirectional());
    assert_eq!(source.last_seen_ms, 25);
    assert_eq!(source.total_bytes, 40);
}

#[test]
fn incoming_completion_accounting_persists_positive_and_zero_byte_successes() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = IncomingSourceRegistry::load(root.path()).unwrap();
    registry
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 25,
            total_bytes: 40,
        })
        .unwrap();
    registry.save().unwrap();

    registry
        .record_completed_operation(SOURCE_ID, 12, 30)
        .unwrap();
    registry
        .record_completed_operation(SOURCE_ID, 0, 31)
        .unwrap();

    let restored = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(restored.sources()[0].total_bytes, 52);
    assert_eq!(restored.sources()[0].last_seen_ms, 31);
}

#[test]
fn incoming_clone_receipt_survives_reregistration_and_counts_a_new_operation() {
    let root = tempfile::tempdir().unwrap();
    register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 25,
            total_bytes: 0,
        },
    )
    .unwrap();
    let first_receipt = "d".repeat(64);
    let second_receipt = "e".repeat(64);
    IncomingSourceRegistry::load(root.path())
        .unwrap()
        .record_completed_operation_once(SOURCE_ID, &first_receipt, 12, 30)
        .unwrap();

    register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Renamed Android".into(),
            endpoint: "http://192.168.0.6:32145".into(),
            bearer: "f".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 99,
            total_bytes: 0,
        },
    )
    .unwrap();
    let mut restarted = IncomingSourceRegistry::load(root.path()).unwrap();
    restarted
        .record_completed_operation_once(SOURCE_ID, &first_receipt, 12, 31)
        .unwrap();
    assert_eq!(restarted.sources()[0].total_bytes, 12);
    restarted
        .record_completed_operation_once(SOURCE_ID, &second_receipt, 12, 32)
        .unwrap();
    assert_eq!(restarted.sources()[0].total_bytes, 24);
    assert_eq!(restarted.sources()[0].last_seen_ms, 32);
}

#[test]
fn incoming_lane_receipt_heads_coexist_retry_after_reload_and_replace_sequentially() {
    let root = tempfile::tempdir().unwrap();
    register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            last_seen_ms: 25,
            total_bytes: 0,
        },
    )
    .unwrap();

    let clone = "1".repeat(64);
    let delta = "2".repeat(64);
    let bidirectional = "3".repeat(64);
    for (lane, receipt, bytes) in [
        (CompletionLane::Clone, clone.as_str(), 11),
        (CompletionLane::Delta, delta.as_str(), 13),
        (CompletionLane::Bidirectional, bidirectional.as_str(), 17),
    ] {
        record_incoming_completed_operation_once_for_lane(
            root.path(),
            SOURCE_ID,
            lane,
            receipt,
            bytes,
        )
        .unwrap();
    }
    let registry = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(registry.sources()[0].total_bytes, 41);

    for (lane, receipt, bytes) in [
        (CompletionLane::Clone, clone.as_str(), 11),
        (CompletionLane::Delta, delta.as_str(), 13),
        (CompletionLane::Bidirectional, bidirectional.as_str(), 17),
    ] {
        assert!(incoming_completed_operation_recorded_for_lane(
            root.path(),
            SOURCE_ID,
            lane,
            receipt,
        )
        .unwrap());
        record_incoming_completed_operation_once_for_lane(
            root.path(),
            SOURCE_ID,
            lane,
            receipt,
            bytes,
        )
        .unwrap();
    }
    let mut restarted = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(restarted.sources()[0].total_bytes, 41);

    let next_delta = "4".repeat(64);
    restarted
        .record_completed_operation_once_for_lane(
            SOURCE_ID,
            CompletionLane::Delta,
            &next_delta,
            19,
            200,
        )
        .unwrap();
    assert_eq!(restarted.sources()[0].total_bytes, 60);
    assert!(!restarted
        .has_completed_operation_for_lane(SOURCE_ID, CompletionLane::Delta, &delta)
        .unwrap());
    assert!(restarted
        .has_completed_operation_for_lane(SOURCE_ID, CompletionLane::Delta, &next_delta)
        .unwrap());

    register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Renamed Android".into(),
            endpoint: "http://192.168.0.6:32145".into(),
            bearer: "d".repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            last_seen_ms: 999,
            total_bytes: 0,
        },
    )
    .unwrap();
    let reregistered = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(reregistered.sources()[0].total_bytes, 60);
    assert!(reregistered
        .has_completed_operation_for_lane(SOURCE_ID, CompletionLane::Clone, &clone)
        .unwrap());
    assert!(reregistered
        .has_completed_operation_for_lane(SOURCE_ID, CompletionLane::Bidirectional, &bidirectional,)
        .unwrap());

    remove_incoming_source(root.path(), SOURCE_ID).unwrap();
    let file: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("peer-sync/sources.json")).unwrap())
            .unwrap();
    assert!(file["completedReceipts"]
        .as_array()
        .is_none_or(Vec::is_empty));
}

#[test]
fn v1_registries_with_optional_or_clone_only_receipts_remain_loadable() {
    let root = tempfile::tempdir().unwrap();
    let peer_root = root.path().join("peer-sync");
    fs::create_dir_all(&peer_root).unwrap();
    fs::write(
        peer_root.join("devices.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": "risunest.peer-device-registry/v1",
            "devices": [{
                "deviceId": TARGET_ID,
                "name": "Legacy target",
                "bearerDigest": "b".repeat(64),
                "permissions": ["read"],
                "createdAtMs": 10,
                "lastSeenMs": 20,
                "totalBytes": 30
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        peer_root.join("sources.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": "risunest.peer-source-registry/v1",
            "sources": [{
                "deviceId": SOURCE_ID,
                "name": "Legacy source",
                "endpoint": "http://192.168.0.5:32145",
                "bearer": "c".repeat(64),
                "permissions": ["read"],
                "lastSeenMs": 25,
                "totalBytes": 40
            }],
            "completedReceipts": [{
                "deviceId": SOURCE_ID,
                "lane": "clone",
                "receiptId": "a".repeat(64)
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let outgoing = OutgoingDeviceRegistry::load(root.path()).unwrap();
    let incoming = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(outgoing.devices()[0].total_bytes, 30);
    assert_eq!(incoming.sources()[0].total_bytes, 40);
    assert!(incoming
        .has_completed_operation(SOURCE_ID, &"a".repeat(64))
        .unwrap());
    assert!(!incoming
        .has_completed_operation_for_lane(SOURCE_ID, CompletionLane::Delta, &"a".repeat(64))
        .unwrap());
}

#[test]
fn incoming_v1_registry_without_receipts_remains_loadable() {
    let root = tempfile::tempdir().unwrap();
    let peer_root = root.path().join("peer-sync");
    fs::create_dir_all(&peer_root).unwrap();
    fs::write(
        peer_root.join("sources.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": "risunest.peer-source-registry/v1",
            "sources": [{
                "deviceId": SOURCE_ID,
                "name": "Legacy source",
                "endpoint": "http://192.168.0.5:32145",
                "bearer": "c".repeat(64),
                "permissions": ["read"],
                "lastSeenMs": 25,
                "totalBytes": 40
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let restored = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(restored.sources()[0].total_bytes, 40);
    assert!(!restored
        .has_completed_operation(SOURCE_ID, &"a".repeat(64))
        .unwrap());
}

#[test]
fn incoming_lane_receipt_overflow_and_write_failure_are_atomic() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = IncomingSourceRegistry::load(root.path()).unwrap();
    registry
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            last_seen_ms: 25,
            total_bytes: u64::MAX,
        })
        .unwrap();
    registry.save().unwrap();
    let registry_path = root.path().join("peer-sync/sources.json");
    let before = fs::read(&registry_path).unwrap();

    assert!(registry
        .record_completed_operation_once_for_lane(
            SOURCE_ID,
            CompletionLane::Delta,
            &"a".repeat(64),
            1,
            30,
        )
        .is_err());
    assert_eq!(registry.sources()[0].total_bytes, u64::MAX);
    assert_eq!(fs::read(&registry_path).unwrap(), before);

    let write_root = tempfile::tempdir().unwrap();
    let mut writable = IncomingSourceRegistry::load(write_root.path()).unwrap();
    writable
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            last_seen_ms: 25,
            total_bytes: 0,
        })
        .unwrap();
    writable.save().unwrap();
    let registry_path = write_root.path().join("peer-sync/sources.json");
    let before = fs::read(&registry_path).unwrap();
    let preserved_path = write_root.path().join("peer-sync/sources.preserved.json");
    fs::rename(&registry_path, &preserved_path).unwrap();
    fs::create_dir(&registry_path).unwrap();
    assert!(writable
        .record_completed_operation_once_for_lane(
            SOURCE_ID,
            CompletionLane::Bidirectional,
            &"b".repeat(64),
            9,
            31,
        )
        .is_err());
    assert_eq!(writable.sources()[0].total_bytes, 0);
    fs::remove_dir(&registry_path).unwrap();
    fs::rename(&preserved_path, &registry_path).unwrap();
    assert_eq!(fs::read(&registry_path).unwrap(), before);
}

#[test]
fn incoming_clone_receipt_write_failure_rolls_back_memory_and_file() {
    let root = tempfile::tempdir().unwrap();
    register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 25,
            total_bytes: 0,
        },
    )
    .unwrap();
    let first_receipt = "d".repeat(64);
    let mut registry = IncomingSourceRegistry::load(root.path()).unwrap();
    registry
        .record_completed_operation_once(SOURCE_ID, &first_receipt, 12, 30)
        .unwrap();
    let registry_path = root.path().join("peer-sync/sources.json");
    let preserved_path = root.path().join("peer-sync/sources.preserved.json");
    let before = fs::read(&registry_path).unwrap();
    fs::rename(&registry_path, &preserved_path).unwrap();
    fs::create_dir(&registry_path).unwrap();

    assert!(registry
        .record_completed_operation_once(SOURCE_ID, &"e".repeat(64), 12, 31)
        .is_err());
    assert_eq!(registry.sources()[0].total_bytes, 12);
    assert_eq!(registry.sources()[0].last_seen_ms, 30);

    fs::remove_dir(&registry_path).unwrap();
    fs::rename(&preserved_path, &registry_path).unwrap();
    assert_eq!(fs::read(&registry_path).unwrap(), before);
    let mut restarted = IncomingSourceRegistry::load(root.path()).unwrap();
    restarted
        .record_completed_operation_once(SOURCE_ID, &first_receipt, 12, 32)
        .unwrap();
    assert_eq!(restarted.sources()[0].total_bytes, 12);
}

#[test]
fn incoming_completion_accounting_rejects_overflow_without_mutating_memory_or_file() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = IncomingSourceRegistry::load(root.path()).unwrap();
    registry
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 25,
            total_bytes: u64::MAX,
        })
        .unwrap();
    registry.save().unwrap();
    let before = fs::read(root.path().join("peer-sync/sources.json")).unwrap();

    assert!(registry
        .record_completed_operation(SOURCE_ID, 1, 30)
        .is_err());
    assert_eq!(registry.sources()[0].total_bytes, u64::MAX);
    assert_eq!(registry.sources()[0].last_seen_ms, 25);
    assert_eq!(
        fs::read(root.path().join("peer-sync/sources.json")).unwrap(),
        before
    );
}

#[test]
fn incoming_completion_accounting_does_not_infer_an_outgoing_completion() {
    let root = tempfile::tempdir().unwrap();
    let mut outgoing = OutgoingDeviceRegistry::load(root.path()).unwrap();
    outgoing
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Target".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 1,
            last_seen_ms: 2,
            total_bytes: 3,
        })
        .unwrap();
    outgoing.save().unwrap();

    record_incoming_completed_operation(root.path(), TARGET_ID, 99).unwrap();

    assert_eq!(
        OutgoingDeviceRegistry::load(root.path()).unwrap().devices()[0].total_bytes,
        3
    );
    assert!(IncomingSourceRegistry::load(root.path())
        .unwrap()
        .sources()
        .is_empty());
}

#[test]
fn registries_reject_malformed_oversized_and_linked_files() {
    let root = tempfile::tempdir().unwrap();
    let peer_root = root.path().join("peer-sync");
    fs::create_dir_all(&peer_root).unwrap();
    fs::write(peer_root.join("devices.json"), b"{").unwrap();
    assert!(OutgoingDeviceRegistry::load(root.path()).is_err());

    fs::write(peer_root.join("devices.json"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    assert!(OutgoingDeviceRegistry::load(root.path()).is_err());

    fs::write(peer_root.join("sources.json"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    assert!(IncomingSourceRegistry::load(root.path()).is_err());

    let outside = root.path().join("outside.json");
    fs::write(&outside, b"{}").unwrap();
    fs::remove_file(peer_root.join("devices.json")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, peer_root.join("devices.json")).unwrap();
    #[cfg(unix)]
    assert!(OutgoingDeviceRegistry::load(root.path()).is_err());

    #[cfg(unix)]
    {
        fs::remove_file(peer_root.join("sources.json")).unwrap();
        std::os::unix::fs::symlink(&outside, peer_root.join("sources.json")).unwrap();
        assert!(IncomingSourceRegistry::load(root.path()).is_err());
    }
}

#[test]
fn outgoing_registry_rejects_impossible_completion_lease_states() {
    let root = tempfile::tempdir().unwrap();
    let peer_root = root.path().join("peer-sync");
    fs::create_dir_all(&peer_root).unwrap();
    let device = serde_json::json!({
        "deviceId": TARGET_ID,
        "name": "Windows desktop",
        "bearerDigest": "a".repeat(64),
        "permissions": ["read"],
        "createdAtMs": 1,
        "lastSeenMs": 0,
        "totalBytes": 0
    });
    for impossible in [
        serde_json::json!({
            "deviceId": TARGET_ID,
            "bearerDigest": "a".repeat(64),
            "lane": "delta",
            "leaseId": "00000000-0000-4000-8000-000000000001",
            "manifestId": "b".repeat(64),
            "ready": true
        }),
        serde_json::json!({
            "deviceId": TARGET_ID,
            "bearerDigest": "a".repeat(64),
            "lane": "delta",
            "leaseId": "00000000-0000-4000-8000-000000000002",
            "manifestId": "b".repeat(64),
            "transferredBytes": 1,
            "ready": false
        }),
        serde_json::json!({
            "deviceId": TARGET_ID,
            "bearerDigest": "a".repeat(64),
            "lane": "clone",
            "leaseId": "00000000-0000-4000-8000-000000000003",
            "manifestId": "b".repeat(64),
            "ready": false
        }),
    ] {
        fs::write(
            peer_root.join("devices.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema": "risunest.peer-device-registry/v1",
                "devices": [device.clone()],
                "completionOffers": [impossible]
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(OutgoingDeviceRegistry::load(root.path()).is_err());
    }
}

#[test]
fn registries_reject_noncanonical_ids_credentials_and_endpoints() {
    let root = tempfile::tempdir().unwrap();
    let mut outgoing = OutgoingDeviceRegistry::load(root.path()).unwrap();
    assert!(outgoing
        .upsert(OutgoingDevice {
            device_id: "B8E9D6D7-6D4C-43D8-B00A-80C8F34478B6".into(),
            name: "Windows desktop".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 0,
            last_seen_ms: 0,
            total_bytes: 0,
        })
        .is_err());
    assert!(outgoing
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Windows desktop".into(),
            bearer_digest: "A".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 0,
            last_seen_ms: 0,
            total_bytes: 0,
        })
        .is_err());

    let mut incoming = IncomingSourceRegistry::load(root.path()).unwrap();
    assert!(incoming
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "https://sync.example.com/not-allowed".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 0,
            total_bytes: 0,
        })
        .is_err());
    assert!(incoming
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "C".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 0,
            total_bytes: 0,
        })
        .is_err());
}

#[test]
fn device_ids_are_bounded_regular_canonical_files() {
    let root = tempfile::tempdir().unwrap();
    let current = root.path().join("peer-sync/device-id");
    fs::create_dir_all(current.parent().unwrap()).unwrap();
    fs::write(&current, "x".repeat(65)).unwrap();
    assert!(super::device_registry::load_or_create_device_id(root.path()).is_err());

    fs::write(&current, "B8E9D6D7-6D4C-43D8-B00A-80C8F34478B6").unwrap();
    assert!(super::device_registry::load_or_create_device_id(root.path()).is_err());

    fs::remove_file(&current).unwrap();
    let legacy = root.path().join("peer-delta/source-device-id");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, "x".repeat(65)).unwrap();
    assert!(super::device_registry::load_or_create_device_id(root.path()).is_err());
}

#[test]
fn stale_registry_temp_does_not_block_a_save() {
    let root = tempfile::tempdir().unwrap();
    let peer_root = root.path().join("peer-sync");
    fs::create_dir_all(&peer_root).unwrap();
    fs::write(peer_root.join("sources.json.tmp"), b"stale").unwrap();

    let registry = IncomingSourceRegistry::load(root.path()).unwrap();
    registry.save().unwrap();
    assert!(peer_root.join("sources.json").exists());
}

#[test]
fn concurrent_registry_saves_do_not_collide_on_a_temp_name() {
    let root = tempfile::tempdir().unwrap();
    let app_root = root.path().to_owned();
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for device_id in [SOURCE_ID, TARGET_ID] {
        let app_root = app_root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let mut registry = OutgoingDeviceRegistry::load(&app_root).unwrap();
            registry
                .upsert(OutgoingDevice {
                    device_id: device_id.into(),
                    name: "Windows desktop".into(),
                    bearer_digest: "a".repeat(64),
                    permissions: DevicePermissions::read(),
                    created_at_ms: 0,
                    last_seen_ms: 0,
                    total_bytes: 0,
                })
                .unwrap();
            barrier.wait();
            registry.save()
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    OutgoingDeviceRegistry::load(root.path()).unwrap();
}

#[test]
fn concurrent_outgoing_claims_preserve_both_devices() {
    let root = tempfile::tempdir().unwrap();
    let app_root = root.path().to_owned();
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for device_id in [SOURCE_ID, TARGET_ID] {
        let app_root = app_root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            register_outgoing_claim(
                &app_root,
                OutgoingDevice {
                    device_id: device_id.to_owned(),
                    name: "Windows desktop".to_owned(),
                    bearer_digest: "a".repeat(64),
                    permissions: DevicePermissions::read(),
                    created_at_ms: 0,
                    last_seen_ms: 0,
                    total_bytes: 0,
                },
            )
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    let registered = OutgoingDeviceRegistry::load(root.path()).unwrap();
    assert_eq!(registered.devices().len(), 2);
    assert!(registered
        .devices()
        .iter()
        .any(|device| device.device_id == SOURCE_ID));
    assert!(registered
        .devices()
        .iter()
        .any(|device| device.device_id == TARGET_ID));
}

#[test]
fn concurrent_incoming_registrations_preserve_both_sources() {
    let root = tempfile::tempdir().unwrap();
    let app_root = root.path().to_owned();
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for device_id in [SOURCE_ID, TARGET_ID] {
        let app_root = app_root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            register_incoming_source(
                &app_root,
                IncomingSource {
                    device_id: device_id.to_owned(),
                    name: "Android".to_owned(),
                    endpoint: "http://192.168.0.5:32145".to_owned(),
                    bearer: "c".repeat(64),
                    permissions: DevicePermissions::read(),
                    last_seen_ms: 0,
                    total_bytes: 0,
                },
            )
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    let registered = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(registered.sources().len(), 2);
    assert!(registered
        .sources()
        .iter()
        .any(|source| source.device_id == SOURCE_ID));
    assert!(registered
        .sources()
        .iter()
        .any(|source| source.device_id == TARGET_ID));
}

#[test]
fn best_effort_incoming_completion_leaves_overflowed_registry_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = IncomingSourceRegistry::load(root.path()).unwrap();
    registry
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 25,
            total_bytes: u64::MAX,
        })
        .unwrap();
    registry.save().unwrap();
    let before = fs::read(root.path().join("peer-sync/sources.json")).unwrap();

    record_incoming_completed_operation_best_effort(root.path(), SOURCE_ID, 1);

    assert_eq!(
        fs::read(root.path().join("peer-sync/sources.json")).unwrap(),
        before
    );
}

#[test]
fn best_effort_incoming_completion_ignores_a_malformed_registry() {
    let root = tempfile::tempdir().unwrap();
    let peer_root = root.path().join("peer-sync");
    fs::create_dir_all(&peer_root).unwrap();
    fs::write(peer_root.join("sources.json"), b"{").unwrap();

    record_incoming_completed_operation_best_effort(root.path(), SOURCE_ID, 1);

    assert_eq!(fs::read(peer_root.join("sources.json")).unwrap(), b"{");
}

#[test]
fn registry_command_views_are_directional_and_never_serialize_credentials() {
    let root = tempfile::tempdir().unwrap();
    let mut outgoing = OutgoingDeviceRegistry::load(root.path()).unwrap();
    outgoing
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Target".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 1,
            last_seen_ms: 2,
            total_bytes: 3,
        })
        .unwrap();
    outgoing.save().unwrap();
    let mut incoming = IncomingSourceRegistry::load(root.path()).unwrap();
    incoming
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Source".into(),
            endpoint: "http://127.0.0.1:32145".into(),
            bearer: "b".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 4,
            total_bytes: 5,
        })
        .unwrap();
    incoming.save().unwrap();

    let outgoing = outgoing_device_summaries(root.path()).unwrap();
    let incoming = incoming_source_summaries(root.path()).unwrap();

    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0].device_id, TARGET_ID);
    assert_eq!(incoming.len(), 1);
    assert_eq!(incoming[0].device_id, SOURCE_ID);
    let outgoing_json = serde_json::to_string(&outgoing).unwrap();
    let incoming_json = serde_json::to_string(&incoming).unwrap();
    assert!(!outgoing_json.contains("bearer"));
    assert!(!incoming_json.contains("bearer"));
    assert!(!incoming_json.contains("127.0.0.1"));

    let mut revoked_live = Vec::new();
    revoke_outgoing_device(root.path(), TARGET_ID, |device_id| {
        revoked_live.push(device_id.to_owned())
    })
    .unwrap();
    remove_incoming_source(root.path(), SOURCE_ID).unwrap();

    assert_eq!(revoked_live, vec![TARGET_ID]);
    assert!(OutgoingDeviceRegistry::load(root.path())
        .unwrap()
        .devices()
        .is_empty());
    assert!(IncomingSourceRegistry::load(root.path())
        .unwrap()
        .sources()
        .is_empty());
    assert!(root.path().join("peer-sync/sources.json").is_file());
}

#[cfg(unix)]
#[test]
fn peer_registry_storage_is_owner_only_on_unix() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let peer_root = root.path().join("peer-sync");

    super::device_registry::load_or_create_device_id(root.path()).unwrap();
    assert_eq!(
        fs::metadata(&peer_root).unwrap().permissions().mode() & 0o777,
        0o700
    );

    fs::set_permissions(&peer_root, fs::Permissions::from_mode(0o755)).unwrap();
    let mut registry = IncomingSourceRegistry::load(root.path()).unwrap();
    registry
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 25,
            total_bytes: 40,
        })
        .unwrap();
    registry.save().unwrap();

    assert_eq!(
        fs::metadata(&peer_root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for name in ["device-id", "sources.json"] {
        assert_eq!(
            fs::metadata(peer_root.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "{name} must be owner-only"
        );
    }
}

#[cfg(unix)]
#[test]
fn device_ids_and_peer_root_reject_symlinks() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = root.path().join("outside-id");
    fs::write(&outside, SOURCE_ID).unwrap();
    let current = root.path().join("peer-sync/device-id");
    fs::create_dir_all(current.parent().unwrap()).unwrap();
    symlink(&outside, &current).unwrap();
    assert!(super::device_registry::load_or_create_device_id(root.path()).is_err());

    fs::remove_file(&current).unwrap();
    fs::remove_dir(root.path().join("peer-sync")).unwrap();
    let outside_root = root.path().join("outside-peer-root");
    fs::create_dir(&outside_root).unwrap();
    symlink(&outside_root, root.path().join("peer-sync")).unwrap();
    assert!(OutgoingDeviceRegistry::load(root.path()).is_err());
}

#[cfg(windows)]
#[test]
fn peer_root_rejects_a_reparse_point_when_symlinks_are_supported() {
    use std::os::windows::fs::symlink_dir;

    let root = tempfile::tempdir().unwrap();
    let outside_root = root.path().join("outside-peer-root");
    fs::create_dir(&outside_root).unwrap();
    let peer_root = root.path().join("peer-sync");
    if let Err(error) = symlink_dir(&outside_root, &peer_root) {
        if error.raw_os_error() == Some(1314) {
            return;
        }
        panic!("create peer root reparse fixture: {error}");
    }
    assert!(OutgoingDeviceRegistry::load(root.path()).is_err());
}
