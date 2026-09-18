use super::{references::{Capture, Object}, *};
use crate::{
    asset_repository::{job_pins::DurableCasJob, PayloadCas},
    logical_records::{encode_logical_record_key, LogicalRecordEnvelope, LogicalRecordLocator},
    persistent_store::{server_sync_projection::ServerPayload, PersistentStore},
    server_sync::{credentials::StoredConfig, residency::{Residency, RetainedObject}},
};
use risunest_sync_wire::{hash, RecordVersion};

fn head() -> RemoteHead {
    RemoteHead::genesis("library".into(), "epoch".into()).unwrap()
}

fn config() -> StoredConfig {
    serde_json::from_value(serde_json::json!({
        "endpoint":"http://127.0.0.1:8123/", "libraryId":"library", "deviceId":"device",
        "credentialId":"00000000-0000-4000-8000-000000000000"
    })).unwrap()
}

fn finish_empty(root: &Path, store: &mut PersistentStore) -> String {
    let capture = Capture::begin(root, 0, "generation", &head()).unwrap();
    capture.complete_side(Side::Local).unwrap();
    capture.complete_side(Side::Remote).unwrap();
    capture.finish(store, &|| Ok(())).unwrap().id
}

#[test]
fn list_reports_durable_local_requirements_without_claiming_remote_liveness() {
    let root = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();
    let mut capture = Capture::begin(root.path(), 0, "generation", &head()).unwrap();
    let local = capture.metadata(Side::Local, b"local metadata").unwrap();
    let remote = capture.metadata(Side::Remote, b"remote metadata").unwrap();
    let dependency = hash(b"remote dependency");
    let context = Residency::context_id(&config(), "epoch");
    capture.object(Side::Remote, &Object { hash: dependency.clone(), byte_size: Some(17),
        metadata: false, context_id: Some(context), local_required: false }).unwrap();
    Residency::open(root.path()).unwrap().confirm(&config(), &head(), &[RetainedObject {
        hash: dependency, size: 17.into(), retention_id: hash(b"retention"),
    }]).unwrap();
    let version = RecordVersion::Live { object_hash: hash(b"record"), descriptor_hash: None };
    let key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
        owner: "synthetic".into(), storage_key: "value".into(),
    }).unwrap();
    capture.record(Side::Local, &key, &version, Some((&local, 14))).unwrap();
    capture.record(Side::Remote, &key, &version, Some((&remote, 15))).unwrap();
    capture.complete_side(Side::Local).unwrap();
    capture.complete_side(Side::Remote).unwrap();
    let id = capture.finish(&mut store, &|| Ok(())).unwrap().id;

    let backup = inspect(root.path(), &id).unwrap();
    assert_eq!(backup.local.local_required_bytes, 14);
    assert_eq!(backup.local.remote_dependent_bytes, 0);
    assert_eq!(backup.local.availability, Availability::LocalComplete);
    assert_eq!(backup.remote.local_required_bytes, 15);
    assert_eq!(backup.remote.remote_dependent_bytes, 17);
    assert_eq!(backup.remote.availability, Availability::ConnectionRequired);
}

#[test]
fn missing_required_bytes_are_counted_but_make_the_side_unavailable() {
    let root = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();
    let mut capture = Capture::begin(root.path(), 0, "generation", &head()).unwrap();
    let body = capture.metadata(Side::Local, b"required bytes").unwrap();
    capture.complete_side(Side::Local).unwrap();
    capture.complete_side(Side::Remote).unwrap();
    let id = capture.finish(&mut store, &|| Ok(())).unwrap().id;
    let path = root.path().join(crate::asset_repository::object_physical_key(&body));
    std::fs::remove_file(path).unwrap();

    let backup = inspect(root.path(), &id).unwrap();
    assert_eq!(backup.local.local_required_bytes, 14);
    assert_eq!(backup.local.availability, Availability::Unavailable);
}

#[test]
fn completed_reference_releases_capture_pins_after_publishing_its_root() {
    let root = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();
    let mut capture = Capture::begin(root.path(), 0, "generation", &head()).unwrap();
    let metadata = capture.metadata(Side::Local, b"root-owned metadata").unwrap();
    capture.complete_side(Side::Local).unwrap();
    capture.complete_side(Side::Remote).unwrap();
    let id = capture.finish(&mut store, &|| Ok(())).unwrap().id;

    assert_eq!(DurableCasJob::open(root.path(), &id).unwrap_err().kind(),
        std::io::ErrorKind::NotFound);
    let mut roots = Vec::new();
    references::visit_roots(root.path(), |object| {
        roots.push(object.hash);
        Ok(())
    }).unwrap();
    assert_eq!(roots, vec![metadata.clone()]);
    assert!(PayloadCas::new(root.path()).unwrap().stat_object(&metadata).unwrap().is_some());
}

#[test]
fn source_visitors_filter_sides_and_decode_only_verified_record_bodies() {
    let root = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();
    let mut capture = Capture::begin(root.path(), 0, "generation", &head()).unwrap();
    let payload = ServerPayload {
        record: LogicalRecordEnvelope::Plugin {
            owner: "synthetic".into(), ordinal: 7, value: serde_json::json!({"safe": true}),
        },
        messages: None,
        derived_objects: Default::default(),
    };
    let bytes = serde_json::to_vec(&payload).unwrap();
    let body = capture.metadata(Side::Local, &bytes).unwrap();
    let key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
        owner: "synthetic".into(), storage_key: "value".into(),
    }).unwrap();
    let version = RecordVersion::Live { object_hash: hash(b"record"), descriptor_hash: None };
    capture.record(Side::Local, &key, &version, Some((&body, bytes.len() as u64))).unwrap();
    capture.complete_side(Side::Local).unwrap();
    capture.complete_side(Side::Remote).unwrap();
    let id = capture.finish(&mut store, &|| Ok(())).unwrap().id;
    let source = source(root.path(), &id, Side::Local, &|| Ok(())).unwrap();

    let mut records = Vec::new();
    visit_reference_records(root.path(), &source, &|| Ok(()), |record| {
        records.push(record);
        Ok(())
    }).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].key, key);
    assert!(matches!(records[0].payload.as_ref().map(|payload| &payload.record),
        Some(LogicalRecordEnvelope::Plugin { ordinal: 7, .. })));
    let mut objects = Vec::new();
    visit_reference_objects(root.path(), &source, &|| Ok(()), |object| {
        objects.push(object.hash);
        Ok(())
    }).unwrap();
    assert_eq!(objects, vec![body.clone()]);

    let object = PayloadCas::new(root.path()).unwrap().object_path(&body).unwrap().unwrap();
    let mut damaged = std::fs::read(&object).unwrap();
    damaged[0] ^= 1;
    std::fs::write(object, damaged).unwrap();
    assert_eq!(visit_reference_records(root.path(), &source, &|| Ok(()), |_| Ok(())).unwrap_err().code,
        "conflict-object-hash-mismatch");
}

#[test]
fn source_record_body_size_is_bounded_before_allocation() {
    let root = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();
    let mut capture = Capture::begin(root.path(), 0, "generation", &head()).unwrap();
    let body = capture.metadata(Side::Local, b"small body").unwrap();
    let key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
        owner: "synthetic".into(), storage_key: "bounded".into(),
    }).unwrap();
    let version = RecordVersion::Live { object_hash: hash(b"record"), descriptor_hash: None };
    capture.record(Side::Local, &key, &version, Some((&body, 10))).unwrap();
    capture.complete_side(Side::Local).unwrap();
    capture.complete_side(Side::Remote).unwrap();
    let id = capture.finish(&mut store, &|| Ok(())).unwrap().id;
    let db = rusqlite::Connection::open(root.path().join("server-sync/backups")
        .join(id).join("index.sqlite")).unwrap();
    db.execute("UPDATE records SET body_bytes=?1 WHERE side='local'", [
        (risunest_sync_wire::MAX_METADATA_BYTES as i64) + 1,
    ]).unwrap();

    assert_eq!(references::visit_source_records(root.path(), &db, Side::Local,
        &|| Ok(()), |_| Ok(())).unwrap_err().code, "conflict-record-too-large");
}

#[test]
fn backup_list_pages_fifty_without_deleting_older_references() {
    let root = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();
    for _ in 0..51 { finish_empty(root.path(), &mut store); }
    let first = list(root.path(), None).unwrap();
    assert_eq!(first.items.len(), 50);
    let second = list(root.path(), first.next.as_ref()).unwrap();
    assert_eq!(second.items.len(), 1);
    assert!(second.next.is_none());
    assert_eq!(std::fs::read_dir(root.path().join("server-sync/backups")).unwrap().count(), 51);
    assert_eq!(list(root.path(), Some(&BackupCursor { created_at: 0, id: "not-a-uuid".into() }))
        .unwrap_err().code, "invalid-backup-cursor");
}
