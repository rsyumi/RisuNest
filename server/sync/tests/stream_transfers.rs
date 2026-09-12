mod common;
use common::*;
use risunest_sync_server::store::{DeltaProgress, Store, TransferRequest, UploadManifest};
use risunest_sync_wire::{delta::Base, hash, stream_delta};
use std::io::Cursor;

#[test]
fn durable_file_delta_jobs_pin_bases_survive_restart_and_isolate_devices() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let b = device(&store);
    let base = vec![0x97; 1024 * 1024];
    let mut target = base.clone();
    target.splice(514311..514311, b"new exact bytes".iter().copied());
    let base_hash = hash(&base);
    let target_hash = hash(&target);
    store.put_object(&a, &base_hash, &base).unwrap();
    let recipe = stream_delta::create(
        &mut [Cursor::new(&base)],
        &[Base {
            hash: base_hash.clone(),
            size: base.len() as u64,
        }],
        &mut Cursor::new(&target),
        Base {
            hash: target_hash.clone(),
            size: target.len() as u64,
        },
        || Ok(()),
    )
    .unwrap();
    let body = stream_delta::encode(&recipe).unwrap();
    let id = store
        .begin_upload(
            &a,
            &UploadManifest {
                hash: target_hash.clone(),
                size: (target.len() as u64).into(),
            },
        )
        .unwrap();
    assert_eq!(
        store
            .attach_upload_delta(&b, &id, &body)
            .unwrap_err()
            .status,
        404
    );
    store.attach_upload_delta(&a, &id, &body).unwrap();
    store.attach_upload_delta(&a, &id, &body).unwrap();
    assert!(store.submit_upload(&a, &id).unwrap().is_none());
    let mut different = recipe.clone();
    different.bases.clear();
    different.ops = vec![risunest_sync_wire::delta::Op::Insert(target.clone())];
    assert_eq!(
        store
            .attach_upload_delta(&a, &id, &stream_delta::encode(&different).unwrap())
            .unwrap_err()
            .code,
        "upload-intent-conflict"
    );
    let db = rusqlite::Connection::open(dir.path().join("metadata.sqlite")).unwrap();
    db.execute("UPDATE object_leases SET expires=0", [])
        .unwrap();
    store.maintain().unwrap();
    assert_eq!(
        store.object_size(&base_hash).unwrap(),
        Some(base.len() as u64)
    );
    db.execute("UPDATE uploads SET state='finalizing' WHERE id=?1", [&id])
        .unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert!(store.run_pending_upload().unwrap());
    assert!(store.upload_progress(&a, &id, None).unwrap().complete);
    assert_eq!(store.get_object(&target_hash).unwrap(), target);
    let request = TransferRequest {
        target: target_hash.clone(),
        bases: vec![base_hash.clone()],
    };
    let job = store.begin_download_delta(&a, &request).unwrap();
    assert_eq!(store.begin_download_delta(&a, &request).unwrap(), job);
    assert_eq!(
        store
            .download_delta_progress(&b, &job)
            .err()
            .unwrap()
            .status,
        404
    );
    db.execute(
        "UPDATE download_deltas SET state='working' WHERE id=?1",
        [&job],
    )
    .unwrap();
    db.execute("UPDATE object_leases SET expires=0", [])
        .unwrap();
    store.maintain().unwrap();
    assert!(store.object_size(&base_hash).unwrap().is_some());
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert!(store.run_pending_download_delta().unwrap());
    let DeltaProgress::Ready(encoded) = store.download_delta_progress(&a, &job).unwrap() else {
        panic!("expected delta")
    };
    assert!(encoded.len() < 8192);
    let mut restored = Vec::new();
    stream_delta::apply(
        &stream_delta::decode(&encoded).unwrap(),
        &mut [Cursor::new(base)],
        &mut restored,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(restored, target);
    store.release_download_delta(&b, &job).unwrap();
    assert!(store.download_delta_progress(&a, &job).is_ok());
    store.release_download_delta(&a, &job).unwrap();
    store.maintain().unwrap();
    assert!(store.object_size(&base_hash).unwrap().is_none());
    let fallback = store
        .begin_download_delta(
            &a,
            &TransferRequest {
                target: target_hash,
                bases: vec![base_hash],
            },
        )
        .unwrap();
    assert!(store.run_pending_download_delta().unwrap());
    assert!(matches!(
        store.download_delta_progress(&a, &fallback).unwrap(),
        DeltaProgress::FullRequired
    ));
}
