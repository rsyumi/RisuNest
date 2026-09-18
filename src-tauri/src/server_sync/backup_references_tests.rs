use super::*;
use crate::persistent_store::PersistentStore;
use crate::server_sync::residency::RetainedObject;
use risunest_sync_wire::hash;

fn head() -> RemoteHead {
    RemoteHead::genesis("library".into(), "epoch".into()).unwrap()
}
fn config() -> StoredConfig {
    serde_json::from_value(serde_json::json!({
        "endpoint":"http://127.0.0.1:8123/", "libraryId":"library", "deviceId":"device",
        "credentialId":"00000000-0000-4000-8000-000000000000"
    })).unwrap()
}
fn root_key() -> String {
    crate::logical_records::encode_logical_record_key(&crate::logical_records::LogicalRecordLocator::Root).unwrap()
}
fn live() -> RecordVersion {
    RecordVersion::Live { object_hash: hash(b"record"), descriptor_hash: Some(hash(b"descriptor")) }
}
fn fixture() -> (tempfile::TempDir, PersistentStore, Capture) {
    let root = tempfile::tempdir().unwrap();
    let store = PersistentStore::open(root.path()).unwrap();
    let capture = Capture::begin(root.path(), 0, "generation", &head()).unwrap();
    (root, store, capture)
}
fn scanned(capture: &Capture) {
    capture.complete_side(Side::Local).unwrap();
    capture.complete_side(Side::Remote).unwrap();
}

#[test]
fn object_requirements_merge_without_overwriting_conflicting_sizes() {
    let (_root, _store, capture) = fixture();
    let digest = hash(b"shared");
    let object = |size, metadata, required| Object { hash: digest.clone(), byte_size: size,
        metadata, context_id: None, local_required: required };
    capture.object(Side::Local, &object(None, false, false)).unwrap();
    capture.object(Side::Local, &object(Some(6), true, false)).unwrap();
    capture.object(Side::Local, &object(None, false, false)).unwrap();
    let value: (i64, String, bool) = capture.db.query_row("SELECT byte_size,role,local_required FROM objects", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(value, (6, "metadata".into(), true));
    assert_eq!(capture.object(Side::Remote, &object(Some(7), false, false)).unwrap_err().code,
        "conflict-object-size-mismatch");
    assert_eq!(capture.object(Side::Local, &object(Some(7), false, false)).unwrap_err().code,
        "conflict-object-size-mismatch");
    assert!(capture.object(Side::Remote, &object(Some(u64::MAX), false, false)).is_err());
}

#[test]
fn record_identity_and_tombstone_body_contracts_are_enforced() {
    let (_root, _store, mut capture) = fixture();
    let key = root_key();
    assert!(capture.record(Side::Local, &key, &live(), None).is_err());
    assert!(capture.record(Side::Local, &key, &live(), Some((&hash(b"missing"), 7))).is_err());
    let body = capture.metadata(Side::Local, b"synthetic body").unwrap();
    capture.record(Side::Local, &key, &live(), Some((&body, 14))).unwrap();
    assert!(capture.record(Side::Local, &key, &live(), Some((&body, 14))).is_err());
    let deleted = RecordVersion::Tombstone { deletion_id: "synthetic-deletion".into() };
    assert!(capture.record(Side::Remote, &key, &deleted, Some((&body, 14))).is_err());
    capture.record(Side::Remote, &key, &deleted, None).unwrap();
    assert!(capture.record(Side::Remote, "not-a-logical-key", &deleted, None).is_err());
    let body_is_null: bool = capture.db.query_row("SELECT body_hash IS NULL AND body_bytes IS NULL
        FROM records WHERE side='remote'", [], |r| r.get(0)).unwrap();
    assert!(body_is_null);
    assert!(capture.db.execute("UPDATE records SET domain='hypa'", []).is_err());
}

#[test]
fn completion_closes_index_and_keeps_local_cas_ownership_without_archives() {
    let (root, mut store, mut capture) = fixture();
    let digest = capture.metadata(Side::Local, b"synthetic metadata").unwrap();
    let path = capture.path.clone();
    let id = capture.id.clone();
    assert!(inspect(root.path(), &id).is_err());
    scanned(&capture);
    let receipt = capture.finish(&mut store, &|| Ok(())).unwrap();
    assert_eq!(receipt.format, FORMAT);
    assert_eq!(receipt.scope, "library");
    assert_eq!(receipt.version, 1);
    assert!(!path.join("local.risunest").exists());
    assert!(!path.join("remote.risunest").exists());
    assert!(!path.join("index.sqlite-wal").exists());
    assert!(!path.join("index.sqlite-journal").exists());
    let db = open(root.path(), &id, &|| Ok(())).unwrap();
    assert!(db.execute("DELETE FROM objects", []).is_err());
    assert_eq!(
        DurableCasJob::open(root.path(), &id).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    let mut roots = Vec::new();
    visit_roots(root.path(), |object| { roots.push(object.hash); Ok(()) }).unwrap();
    assert_eq!(roots, vec![digest.clone()]);
    assert!(PayloadCas::new(root.path()).unwrap().stat_object(&digest).unwrap().is_some());
}

#[test]
fn incomplete_capture_keeps_known_local_and_remote_roots_after_reopen() {
    let (root, _store, mut capture) = fixture();
    let local = capture.metadata(Side::Local, b"local-only").unwrap();
    let remote = hash(b"remote-only");
    capture.object(Side::Remote, &Object { hash: remote.clone(), byte_size: Some(11), metadata: false,
        context_id: Some(Residency::context_id(&config(), "epoch")), local_required: false }).unwrap();
    let id = capture.id.clone();
    drop(capture);
    assert!(inspect(root.path(), &id).is_err());
    assert!(!DurableCasJob::open(root.path(), &id).unwrap().is_released());
    let mut roots = Vec::new();
    visit_roots(root.path(), |object| { roots.push((object.hash, object.local_required)); Ok(()) }).unwrap();
    assert!(roots.contains(&(local, true)));
    assert!(roots.contains(&(remote, false)));
}

#[test]
fn cancellation_before_marker_keeps_pins_and_never_exposes_completion() {
    let (root, mut store, mut capture) = fixture();
    let digest = capture.metadata(Side::Local, b"protected").unwrap();
    let id = capture.id.clone();
    let error = capture.finish(&mut store, &|| Err(SyncError::new("cancelled", 409))).unwrap_err();
    assert_eq!(error.code, "cancelled");
    assert!(inspect(root.path(), &id).is_err());
    assert!(!DurableCasJob::open(root.path(), &id).unwrap().is_released());
    let mut seen = false;
    visit_roots(root.path(), |object| { seen |= object.hash == digest; Ok(()) }).unwrap();
    assert!(seen);
}

#[test]
fn marker_publication_never_replaces_an_existing_completion_file() {
    let (root, mut store, mut capture) = fixture();
    capture.metadata(Side::Local, b"protected").unwrap();
    scanned(&capture);
    let path = capture.path.clone();
    let id = capture.id.clone();
    let collided = std::cell::Cell::new(false);
    let check = || {
        let prepared = std::fs::read_dir(&path)?.any(|entry| entry.is_ok_and(|entry|
            entry.file_name().to_string_lossy().starts_with("complete-")));
        if prepared && !collided.replace(true) {
            std::fs::write(path.join("complete.json"), b"existing completion")?;
        }
        Ok(())
    };
    assert!(capture.finish(&mut store, &check).is_err());
    assert!(collided.get());
    assert_eq!(std::fs::read(path.join("complete.json")).unwrap(), b"existing completion");
    let pins = DurableCasJob::open(root.path(), &id).unwrap();
    assert!(pins.is_sealed());
    assert!(!pins.is_released());
    assert!(!std::fs::read_dir(path).unwrap().any(|entry|
        entry.unwrap().file_name().to_string_lossy().starts_with("complete-")));
}

#[test]
fn reference_operations_reject_non_directory_parents() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("server-sync");
    std::fs::write(&parent, b"not a directory").unwrap();
    assert!(Capture::begin(root.path(), 0, "generation", &head()).is_err());
    assert!(Capture::begin_or_resume(root.path(), 0, "generation", &head()).is_err());
    assert!(visit_roots(root.path(), |_| panic!("unsafe roots must not be visited")).is_err());
    assert_eq!(std::fs::read(parent).unwrap(), b"not a directory");
}

#[cfg(unix)]
#[test]
fn reference_operations_do_not_create_or_scan_through_a_linked_parent() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("server-sync")).unwrap();
    assert!(Capture::begin(root.path(), 0, "generation", &head()).is_err());
    assert!(Capture::begin_or_resume(root.path(), 0, "generation", &head()).is_err());
    assert!(visit_roots(root.path(), |_| panic!("linked roots must not be visited")).is_err());
    assert!(!outside.path().join("backups").exists());
}

#[test]
fn remote_payload_requires_exact_durable_custody_but_not_local_bytes() {
    let (root, mut store, capture) = fixture();
    let digest = hash(b"remote-only");
    let context = Residency::context_id(&config(), "epoch");
    capture.object(Side::Remote, &Object { hash: digest.clone(), byte_size: Some(11),
        metadata: false, context_id: Some(context), local_required: false }).unwrap();
    let id = capture.id.clone();
    scanned(&capture);
    assert_eq!(capture.finish(&mut store, &|| Ok(())).unwrap_err().code, "conflict-custody-unconfirmed");
    assert!(inspect(root.path(), &id).is_err());
    let mut residency = Residency::open(root.path()).unwrap();
    residency.confirm(&config(), &head(), &[RetainedObject { hash: digest.clone(), size: 11.into(),
        retention_id: hash(b"custody") }]).unwrap();
    let capture = Capture::begin(root.path(), 0, "generation", &head()).unwrap();
    capture.object(Side::Remote, &Object { hash: digest.clone(), byte_size: Some(11), metadata: false,
        context_id: Some(Residency::context_id(&config(), "epoch")), local_required: false }).unwrap();
    scanned(&capture);
    let receipt = capture.finish(&mut store, &|| Ok(())).unwrap();
    open(root.path(), &receipt.id, &|| Ok(())).unwrap();
    assert_eq!(PayloadCas::new(root.path()).unwrap().stat_object(&digest).unwrap(), None);
}

#[test]
fn source_hash_detects_same_length_index_corruption_while_inspect_only_stats() {
    use std::io::{Seek, SeekFrom};
    let (root, mut store, capture) = fixture();
    scanned(&capture);
    let path = capture.path.clone();
    let receipt = capture.finish(&mut store, &|| Ok(())).unwrap();
    let mut file = std::fs::OpenOptions::new().write(true).open(path.join("index.sqlite")).unwrap();
    file.seek(SeekFrom::Start(100)).unwrap();
    file.write_all(b"broken").unwrap();
    file.sync_all().unwrap();
    assert!(inspect(root.path(), &receipt.id).is_ok());
    assert!(matches!(open(root.path(), &receipt.id, &|| Ok(())),
        Err(error) if error.code == "conflict-object-hash-mismatch"));
}

#[test]
fn receipt_rejects_wrong_version_scope_identity_and_portable_format() {
    let (root, mut store, capture) = fixture();
    scanned(&capture);
    let path = capture.path.clone();
    let receipt = capture.finish(&mut store, &|| Ok(())).unwrap();
    let original = serde_json::to_value(&receipt).unwrap();
    for (key, value) in [("version", serde_json::json!(2)), ("scope", serde_json::json!("device")),
        ("id", serde_json::json!(uuid::Uuid::new_v4().to_string())),
        ("format", serde_json::json!("risunest-portable-backup")),
        ("localRevision", serde_json::json!(-1)), ("extra", serde_json::json!(true))] {
        let mut changed = original.clone();
        changed[key] = value;
        std::fs::write(path.join("complete.json"), serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(inspect(root.path(), &receipt.id).is_err(), "accepted invalid field {key}");
    }
    assert!(inspect(root.path(), "../outside").is_err());
    assert!(inspect(root.path(), &receipt.id.to_uppercase()).is_err());
}

#[test]
fn object_roots_cross_multiple_keyset_pages_and_merge_both_sides() {
    let (root, _store, capture) = fixture();
    for number in 0..(PAGE * 2 + 3) {
        let object = Object { hash: hash(number.to_string().as_bytes()), byte_size: Some(1),
            metadata: false, context_id: Some(hash(b"context")), local_required: false };
        capture.object(Side::Local, &object).unwrap();
        capture.object(Side::Remote, &object).unwrap();
    }
    let mut count = 0;
    visit_roots(root.path(), |_| { count += 1; Ok(()) }).unwrap();
    assert_eq!(count, PAGE * 2 + 3);
}

#[test]
fn corrupted_local_metadata_cannot_publish_a_marker() {
    let (root, mut store, mut capture) = fixture();
    let hash = capture.metadata(Side::Local, b"safe").unwrap();
    let id = capture.id.clone();
    let cas = PayloadCas::new(root.path()).unwrap();
    let path = root.path().join(crate::asset_repository::object_physical_key(&hash));
    assert!(cas.stat_object(&hash).unwrap().is_some());
    std::fs::write(path, b"evil").unwrap();
    scanned(&capture);
    assert_eq!(capture.finish(&mut store, &|| Ok(())).unwrap_err().code, "conflict-object-hash-mismatch");
    assert!(inspect(root.path(), &id).is_err());
}

#[test]
fn unfinished_side_cannot_be_completed_without_finishing_its_scan() {
    let (root, mut store, mut capture) = fixture();
    capture.metadata(Side::Local, b"partial").unwrap();
    capture.complete_side(Side::Local).unwrap();
    let id = capture.id.clone();
    assert_eq!(capture.finish(&mut store, &|| Ok(())).unwrap_err().code, "conflict-preservation-incomplete");
    assert!(inspect(root.path(), &id).is_err());
    let resumed = Capture::begin_or_resume(root.path(), 0, "generation", &head()).unwrap();
    assert_eq!(resumed.id, id);
    assert!(resumed.side_complete(Side::Local).unwrap());
    assert!(!resumed.side_complete(Side::Remote).unwrap());
    resumed.complete_side(Side::Remote).unwrap();
    resumed.finish(&mut store, &|| Ok(())).unwrap();
    open(root.path(), &id, &|| Ok(())).unwrap();
}

#[test]
fn resumption_requires_exact_revision_generation_and_remote_head() {
    let (root, _store, capture) = fixture();
    let id = capture.id.clone();
    drop(capture);
    for (revision, generation, remote) in [(1, "generation", head()), (0, "other-generation", head()),
        (0, "generation", RemoteHead::genesis("library".into(), "other-epoch".into()).unwrap())] {
        assert!(matches!(Capture::resume(root.path(), &id, revision, generation, &remote),
            Err(error) if error.code == "conflict-repreparation-required"));
    }
    let fresh = Capture::begin_or_resume(root.path(), 1, "generation", &head()).unwrap();
    assert_ne!(fresh.id, id);
    assert!(inspect(root.path(), &id).is_err());
    assert!(!DurableCasJob::open(root.path(), &id).unwrap().is_released());
}

#[test]
fn resumed_rows_must_match_the_original_record_exactly() {
    let (root, _store, mut capture) = fixture();
    let body = capture.metadata(Side::Local, b"original").unwrap();
    capture.record(Side::Local, &root_key(), &live(), Some((&body, 8))).unwrap();
    let id = capture.id.clone();
    drop(capture);
    let resumed = Capture::resume(root.path(), &id, 0, "generation", &head()).unwrap();
    resumed.record(Side::Local, &root_key(), &live(), Some((&body, 8))).unwrap();
    assert_eq!(resumed.record(Side::Local, &root_key(), &RecordVersion::Absent, None).unwrap_err().code,
        "conflict-repreparation-required");
}

#[test]
fn every_context_for_a_shared_hash_must_be_confirmed() {
    let (root, mut store, capture) = fixture();
    let mut second = config();
    second.device_id = "other-device".into();
    let digest = hash(b"shared");
    let retained = RetainedObject { hash: digest.clone(), size: 6.into(), retention_id: hash(b"retention") };
    Residency::open(root.path()).unwrap().confirm(&config(), &head(), &[retained]).unwrap();
    for (side, config) in [(Side::Local, config()), (Side::Remote, second)] {
        capture.object(side, &Object { hash: digest.clone(), byte_size: Some(6), metadata: false,
            context_id: Some(Residency::context_id(&config, "epoch")), local_required: false }).unwrap();
    }
    scanned(&capture);
    assert_eq!(capture.finish(&mut store, &|| Ok(())).unwrap_err().code, "conflict-custody-unconfirmed");
}

#[test]
fn cancellation_at_marker_publication_preserves_sealed_pins_and_can_resume() {
    let (root, mut store, mut capture) = fixture();
    let digest = capture.metadata(Side::Local, b"protected").unwrap();
    scanned(&capture);
    let path = capture.path.clone();
    let id = capture.id.clone();
    let check = || {
        if std::fs::read_dir(&path).unwrap().any(|entry|
            entry.unwrap().file_name().to_string_lossy().starts_with("complete-")) {
            Err(SyncError::new("cancelled", 409))
        } else { Ok(()) }
    };
    assert_eq!(capture.finish(&mut store, &check).unwrap_err().code, "cancelled");
    assert!(inspect(root.path(), &id).is_err());
    let pins = DurableCasJob::open(root.path(), &id).unwrap();
    assert!(pins.is_sealed());
    assert!(pins.root_set().unwrap().object_hashes.contains(&digest));
    drop(pins);
    let resumed = Capture::resume(root.path(), &id, 0, "generation", &head()).unwrap();
    resumed.finish(&mut store, &|| Ok(())).unwrap();
    assert!(open(root.path(), &id, &|| Ok(())).is_ok());
    assert!(matches!(Capture::resume(root.path(), &id, 0, "generation", &head()),
        Err(error) if error.code == "conflict-already-complete"));
}
