use super::super::{
    server_sync_apply::{validate_remote, RemoteRecord},
    server_sync_projection::ServerPayload,
};
use super::*;
use crate::{
    asset_repository::PayloadCas,
    logical_records::{encode_logical_record_key, LogicalRecordEnvelope, LogicalRecordLocator},
    server_sync::client::ServerConfig,
};
use risunest_sync_wire::{Receipt, RecordVersion, RemoteHead, Sequence, TerminalStatus};

fn head(seq: u64) -> RemoteHead {
    RemoteHead {
        library_id: "library".into(),
        epoch: "epoch".into(),
        seq: Sequence::from(seq),
        head_id: risunest_sync_wire::hash(seq.to_string().as_bytes()),
        min_retained_seq: Sequence::from(0),
    }
}
fn bind(store: &mut PersistentStore) {
    store
        .server_bind(&ServerConfig {
            directory: None,
            endpoint: "http://127.0.0.1:4319".into(),
            library_id: "library".into(),
            device_id: "device".into(),
            token: "a".repeat(64),
        })
        .unwrap();
}
#[test]
fn server_sync_credentials_stay_outside_source_backups_and_allow_recovery_after_loss() {
    let (dir, mut store, _) = open_fixture();
    bind(&mut store);
    let credential = store.server_stored_config().unwrap().unwrap();
    let revision = store.revision().unwrap();
    let lease = store.acquire_revision(revision).unwrap().lease;
    let output = dir.path().join("synthetic-preservation.db");
    store
        .capture_preservation_database(&lease, &output, &crate::local_backup::NeverCancelled)
        .unwrap();
    store.release_revision(&lease).unwrap();
    let bytes = std::fs::read(&output).unwrap();
    assert!(!bytes
        .windows(64)
        .any(|part| part == "a".repeat(64).as_bytes()));
    let db = rusqlite::Connection::open(&output).unwrap();
    let config: String = db
        .query_row("SELECT config FROM server_sync_state", [], |r| r.get(0))
        .unwrap();
    assert!(serde_json::from_str::<Value>(&config)
        .unwrap()
        .get("token")
        .is_none());
    credential.remove(dir.path()).unwrap();
    assert!(store.server_config().is_err());
    assert!(store.server_status().unwrap().configured);
    // A native command verifies a new identity and the old revocation first.
    store
        .server_replace_registration(
            &ServerConfig {
                directory: None,
                endpoint: "http://127.0.0.1:4319".into(),
                library_id: "library".into(),
                device_id: "replacement".into(),
                token: "b".repeat(64),
            },
            revision,
        )
        .unwrap();
    assert_eq!(
        store.server_config().unwrap().unwrap().device_id,
        "replacement"
    );
    store.server_unbind().unwrap();
    assert!(!store.server_status().unwrap().configured);
}
fn remote_root(value: Value) -> RemoteRecord {
    let payload = ServerPayload {
        derived_objects: Default::default(),
        record: LogicalRecordEnvelope::Root {
            value,
            owner_heads: Vec::new(),
        },
        messages: None,
    };
    let hash = risunest_sync_wire::hash(&serde_json::to_vec(&payload).unwrap());
    RemoteRecord {
        key: "r1:root".into(),
        version: RecordVersion::Live {
            object_hash: hash.clone(),
            descriptor_hash: None,
        },
        payload: Some(payload),
        local_hash: Some(hash),
    }
}
#[test]
fn remote_apply_cursor_base_and_outbox_are_atomic_and_preserve_local_tail() {
    let (dir, mut store, _) = open_fixture();
    bind(&mut store);
    let cas = PayloadCas::new(dir.path()).unwrap();
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({"statics":{"messages":912},"synthetic":"local"})),
            ..empty_working_set_commit(1)
        })
        .unwrap();
    let dirty =
        super::super::server_sync_outbox::dirty_page(&store.connection, None, 1024).unwrap();
    store.connection.execute_batch("CREATE TRIGGER synthetic_apply_failure BEFORE UPDATE ON server_sync_state WHEN NEW.head IS NOT NULL BEGIN SELECT RAISE(ABORT,'synthetic'); END").unwrap();
    let record = validate_remote(remote_root(json!({"synthetic":"remote"})), &cas).unwrap();
    assert!(store
        .server_apply(2, None, &head(1), vec![record], &dirty, &[])
        .is_err());
    assert_eq!(store.revision().unwrap(), 2);
    assert!(store.server_status().unwrap().head.is_none());
    assert_eq!(store.server_status().unwrap().dirty_records, 1);
    assert_eq!(
        store.server_base("r1:root").unwrap().0,
        RecordVersion::Absent
    );
    store
        .connection
        .execute_batch("DROP TRIGGER synthetic_apply_failure")
        .unwrap();
    let record = validate_remote(remote_root(json!({"synthetic":"remote"})), &cas).unwrap();
    assert_eq!(
        store
            .server_apply(2, None, &head(1), vec![record], &dirty, &[])
            .unwrap(),
        3
    );
    let generation = active_generation(&store.connection).unwrap();
    let value: String = store
        .connection
        .query_row(
            "SELECT value FROM root WHERE generation=?1",
            [generation],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&value).unwrap(),
        json!({"synthetic":"remote","statics":{"messages":912}})
    );
    assert_eq!(store.server_status().unwrap().dirty_records, 0);
    assert_eq!(store.server_status().unwrap().head, Some(head(1)));
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({"synthetic":"newer edit"})),
            ..empty_working_set_commit(3)
        })
        .unwrap();
    let record = validate_remote(remote_root(json!({"synthetic":"older download"})), &cas).unwrap();
    assert!(store
        .server_apply(3, Some(&head(1)), &head(2), vec![record], &dirty, &[])
        .is_err());
    assert_eq!(store.revision().unwrap(), 4);
    assert_eq!(store.server_status().unwrap().head, Some(head(1)));
    assert_eq!(store.server_status().unwrap().dirty_records, 1);
}
#[test]
fn invalid_payload_and_parent_delete_never_activate_or_advance_cursor() {
    let (dir, mut store, _) = open_fixture();
    bind(&mut store);
    let cas = PayloadCas::new(dir.path()).unwrap();
    let mut wrong = remote_root(json!({"characters":[]}));
    assert!(validate_remote(wrong, &cas).is_err());
    wrong = remote_root(json!({}));
    wrong.payload.as_mut().unwrap().messages = Some(vec![]);
    assert!(validate_remote(wrong, &cas).is_err());
    let key = encode_logical_record_key(&LogicalRecordLocator::Character {
        character_id: "char-a".into(),
    })
    .unwrap();
    let deletion = validate_remote(
        RemoteRecord {
            key,
            version: RecordVersion::Tombstone {
                deletion_id: "deletion".into(),
            },
            payload: None,
            local_hash: None,
        },
        &cas,
    )
    .unwrap();
    assert!(store
        .server_apply(1, None, &head(1), vec![deletion], &[], &[])
        .is_err());
    assert_eq!(store.revision().unwrap(), 1);
    assert!(store.server_status().unwrap().head.is_none());
}
#[test]
fn operation_identity_survives_reopen_staging_changes_and_terminal_receipts() {
    let (_dir, mut store, _) = open_fixture();
    bind(&mut store);
    let intent = store
        .server_reserve(&head(0), "b".repeat(64), "stage-a".into(), 1)
        .unwrap();
    assert!(store
        .server_reserve(&head(0), "b".repeat(64), "stage-b".into(), 1)
        .is_err());
    assert!(store.server_unbind().is_err());
    let mut reopened = store.open_native_job_store().unwrap();
    let replacement = reopened.server_restage("stage-b".into()).unwrap();
    assert_eq!(replacement.digest().unwrap(), intent.digest().unwrap());
    assert_eq!(
        replacement.device_operation_seq,
        intent.device_operation_seq
    );
    let mut receipt = Receipt {
        operation_id: risunest_sync_wire::operation_id(
            "library",
            "device",
            &intent.device_operation_seq,
        )
        .unwrap(),
        device_operation_seq: intent.device_operation_seq.clone(),
        intent_digest: intent.digest().unwrap(),
        status: TerminalStatus::Stale,
        head: head(1),
        error: Some("stale-head".into()),
    };
    receipt.intent_digest = "c".repeat(64);
    assert!(reopened.server_observe_receipt(&receipt).is_err());
    assert!(reopened.server_pending().unwrap().is_some());
    receipt.intent_digest = intent.digest().unwrap();
    assert!(!reopened.server_observe_receipt(&receipt).unwrap());
    let next = reopened
        .server_reserve(&head(1), "b".repeat(64), "stage-c".into(), 1)
        .unwrap();
    assert_eq!(next.device_operation_seq, Sequence::from(2));
    super::super::server_sync_outbox::restored_copy(&reopened.connection).unwrap();
    assert!(reopened.server_status().unwrap().registration_required);
    assert!(reopened
        .server_reserve(&head(1), "b".repeat(64), "stage-d".into(), 1)
        .is_err());
}

#[test]
fn recovery_preserves_local_edits_and_bases_while_invalidating_old_operations() {
    let (_dir, mut store, _) = open_fixture();
    bind(&mut store);
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({"synthetic":"unsent"})),
            ..empty_working_set_commit(1)
        })
        .unwrap();
    let revision = store.revision().unwrap();
    let prior = head(4);
    store
        .connection
        .execute(
            "UPDATE server_sync_state SET head=?1",
            [serde_json::to_string(&prior).unwrap()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO server_sync_base VALUES('r1:root',?1,?2)",
            params![
                serde_json::to_string(&RecordVersion::Absent).unwrap(),
                "c".repeat(64)
            ],
        )
        .unwrap();
    store
        .server_reserve(&prior, "b".repeat(64), "stage-old".into(), revision)
        .unwrap();
    let before = store.read_root(None).unwrap().value;
    let dirty = store.server_status().unwrap().dirty_records;
    let mut next = prior.clone();
    next.epoch = "restored-epoch".into();
    assert!(store.server_reconcile_epoch(&prior, revision).is_err());
    assert!(store.server_reconcile_epoch(&next, revision - 1).is_err());
    assert!(store.server_pending().unwrap().is_some());
    store.server_reconcile_epoch(&next, revision).unwrap();
    assert!(store.server_status().unwrap().reconciling);
    assert!(store.server_status().unwrap().full_scan);
    assert!(store.server_status().unwrap().head.is_none());
    assert!(store.server_pending().unwrap().is_none());
    assert_eq!(store.server_status().unwrap().dirty_records, dirty);
    assert_eq!(store.read_root(None).unwrap().value, before);
    assert_eq!(store.revision().unwrap(), revision);
    assert_eq!(
        store.server_base("r1:root").unwrap().1,
        Some("c".repeat(64))
    );
    let mut config = store.server_config().unwrap().unwrap();
    assert!(store
        .server_replace_registration(&config, revision)
        .is_err());
    config.device_id = "new-device".into();
    super::super::server_sync_outbox::restored_copy(&store.connection).unwrap();
    store
        .server_replace_registration(&config, revision)
        .unwrap();
    let status = store.server_status().unwrap();
    assert!(!status.registration_required);
    assert!(status.reconciling);
    assert_eq!(status.device_id.as_deref(), Some("new-device"));
    assert_eq!(store.read_root(None).unwrap().value, before);
    assert_eq!(
        store
            .server_reserve(&next, "b".repeat(64), "stage-new".into(), revision)
            .unwrap()
            .device_operation_seq,
        Sequence::from(1)
    );
}

#[test]
fn server_sync_address_cache_changes_only_endpoint_and_preserves_replica_state() {
    let (_dir, mut store, _) = open_fixture();
    bind(&mut store);
    let config = store.server_config().unwrap().unwrap();
    let pending = store
        .server_reserve(
            &head(0),
            "b".repeat(64),
            "stage-a".into(),
            store.revision().unwrap(),
        )
        .unwrap();
    let pending = serde_json::to_value(&pending).unwrap();
    let before = serde_json::to_value(store.server_status().unwrap()).unwrap();
    let revision = store.revision().unwrap();
    let root = store.read_root(None).unwrap().value;
    let mut verified = config.clone();
    verified.endpoint = "https://new-sync.example".into();
    store.server_cache_endpoint(&config, &verified).unwrap();
    let mut after = serde_json::to_value(store.server_status().unwrap()).unwrap();
    assert_eq!(after["endpoint"], verified.endpoint);
    after["endpoint"] = before["endpoint"].clone();
    assert_eq!(after, before);
    assert_eq!(
        serde_json::to_value(store.server_pending().unwrap().unwrap().intent).unwrap(),
        pending
    );
    assert_eq!(store.revision().unwrap(), revision);
    assert_eq!(store.read_root(None).unwrap().value, root);
    assert_eq!(store.server_config().unwrap().unwrap().token, config.token);
    assert_eq!(
        store
            .server_cache_endpoint(&config, &verified)
            .unwrap_err()
            .code,
        "server-config-changed"
    );
    let mut stranger = verified.clone();
    stranger.device_id = "different".into();
    assert_eq!(
        store
            .server_cache_endpoint(&config, &stranger)
            .unwrap_err()
            .code,
        "device-identity-mismatch"
    );
}

#[test]
fn unconfigured_status_still_reports_pending_operation_for_management_protection() {
    let (_dir, mut store, _) = open_fixture();
    bind(&mut store);
    store
        .server_reserve(&head(0), "b".repeat(64), "stage-a".into(), 1)
        .unwrap();
    // A disconnected/restored state must not hide the independent durable journal.
    store
        .connection
        .execute("DELETE FROM server_sync_state", [])
        .unwrap();
    let status = store.server_status().unwrap();
    assert!(!status.configured);
    assert!(status.operation_pending);
    assert_eq!(
        store.server_unbind().unwrap_err().code,
        "resolve-pending-operation-first"
    );
    assert!(store.server_pending().unwrap().is_some());
}
