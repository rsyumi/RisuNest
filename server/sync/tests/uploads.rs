mod common;
use common::*;
use risunest_sync_server::store::{Store, UploadManifest, UPLOAD_CHUNK_BYTES};
use risunest_sync_wire::hash;

#[test]
fn large_finalization_reservation_survives_restart_and_completes_exactly_once() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let chunk = vec![0x93; UPLOAD_CHUNK_BYTES as usize];
    let chunks = 64 * 1024 * 1024 / UPLOAD_CHUNK_BYTES;
    let mut whole = Sha256::new();
    for _ in 0..chunks {
        whole.update(&chunk);
    }
    let digest = format!("{:x}", whole.finalize());
    let id = store
        .begin_upload(
            &a,
            &UploadManifest {
                hash: digest.clone(),
                size: (chunks * UPLOAD_CHUNK_BYTES).into(),
            },
        )
        .unwrap();
    assert_eq!(
        store.submit_upload(&a, &id).unwrap_err().code,
        "upload-incomplete"
    );
    for index in 0..chunks {
        store
            .put_upload_chunk(&a, &id, index, &hash(&chunk), &chunk)
            .unwrap();
    }
    assert!(store.submit_upload(&a, &id).unwrap().is_none());
    assert!(store.submit_upload(&a, &id).unwrap().is_none());
    assert!(store.upload_progress(&a, &id, None).unwrap().finishing);
    assert_eq!(
        store
            .put_upload_chunk(&a, &id, 0, &hash(&chunk), &chunk)
            .unwrap_err()
            .code,
        "upload-not-open"
    );
    // Model a daemon exiting after claiming the persisted finalization.
    rusqlite::Connection::open(dir.path().join("metadata.sqlite"))
        .unwrap()
        .execute("UPDATE uploads SET state='finalizing' WHERE id=?1", [&id])
        .unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert!(store.run_pending_upload().unwrap());
    assert!(!store.run_pending_upload().unwrap());
    assert!(store.upload_progress(&a, &id, None).unwrap().complete);
    assert_eq!(store.submit_upload(&a, &id).unwrap(), Some(digest.clone()));
    assert_eq!(
        store.object_size(&digest).unwrap(),
        Some(chunks * UPLOAD_CHUNK_BYTES)
    );
    assert_eq!(
        store
            .read_object_range(
                &digest,
                (chunks - 1) * UPLOAD_CHUNK_BYTES,
                UPLOAD_CHUNK_BYTES
            )
            .unwrap(),
        chunk
    );
    store.maintain().unwrap();
    assert!(store.upload_progress(&a, &id, None).unwrap().complete);
    assert_eq!(
        std::fs::read_dir(dir.path().join("staging"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        store.object_size(&digest).unwrap(),
        Some(chunks * UPLOAD_CHUNK_BYTES)
    );
}

#[test]
fn verified_chunks_restart_exact_resume_and_device_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let b = device(&store);
    let bytes = (0..UPLOAD_CHUNK_BYTES as usize + 17)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let digest = hash(&bytes);
    let id = store
        .begin_upload(
            &a,
            &UploadManifest {
                hash: digest.clone(),
                size: (bytes.len() as u64).into(),
            },
        )
        .unwrap();
    let chunk = &bytes[..UPLOAD_CHUNK_BYTES as usize];
    store
        .put_upload_chunk(&a, &id, 0, &hash(chunk), chunk)
        .unwrap();
    assert_eq!(
        store.finish_upload(&a, &id).unwrap_err().code,
        "upload-incomplete"
    );
    assert_eq!(
        store.upload_progress(&b, &id, None).err().unwrap().code,
        "upload-not-found"
    );
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store.upload_progress(&a, &id, None).unwrap().verified,
        vec![0.into()]
    );
    store
        .put_upload_chunk(&a, &id, 0, &hash(chunk), chunk)
        .unwrap();
    let bad = vec![0; chunk.len()];
    assert_eq!(
        store
            .put_upload_chunk(&a, &id, 0, &hash(&bad), &bad)
            .unwrap_err()
            .code,
        "chunk-intent-conflict"
    );
    let tail = &bytes[UPLOAD_CHUNK_BYTES as usize..];
    store
        .put_upload_chunk(&a, &id, 1, &hash(tail), tail)
        .unwrap();
    assert_eq!(store.finish_upload(&a, &id).unwrap(), digest);
    assert_eq!(store.finish_upload(&a, &id).unwrap(), digest);
    assert_eq!(
        store.object_size(&digest).unwrap(),
        Some(bytes.len() as u64)
    );
    assert_eq!(
        store
            .read_object_range(&digest, UPLOAD_CHUNK_BYTES, 17)
            .unwrap(),
        tail
    );
    store.cancel_upload(&a, &id).unwrap();
    assert_eq!(
        store.object_size(&digest).unwrap(),
        Some(bytes.len() as u64)
    );
}

#[test]
fn corrupt_final_hash_expiry_empty_and_cancel_never_publish_partial_content() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let id = store
        .begin_upload(
            &a,
            &UploadManifest {
                hash: hash(b"good"),
                size: 4.into(),
            },
        )
        .unwrap();
    store
        .put_upload_chunk(&a, &id, 0, &hash(b"evil"), b"evil")
        .unwrap();
    assert_eq!(
        store.finish_upload(&a, &id).unwrap_err().code,
        "hash-mismatch"
    );
    assert!(store.object_size(&hash(b"good")).unwrap().is_none());
    store.cancel_upload(&a, &id).unwrap();
    let id = store
        .begin_upload(
            &a,
            &UploadManifest {
                hash: hash(b""),
                size: 0.into(),
            },
        )
        .unwrap();
    store.finish_upload(&a, &id).unwrap();
    assert_eq!(store.get_object(&hash(b"")).unwrap(), b"");
    let id = store
        .begin_upload(
            &a,
            &UploadManifest {
                hash: hash(b"x"),
                size: 1.into(),
            },
        )
        .unwrap();
    let db = rusqlite::Connection::open(dir.path().join("metadata.sqlite")).unwrap();
    db.execute("UPDATE uploads SET expires=0 WHERE id=?1", [&id])
        .unwrap();
    assert_eq!(
        store.upload_progress(&a, &id, None).err().unwrap().code,
        "upload-expired"
    );
    assert!(store
        .put_upload_chunk(&a, &id, 0, &hash(b"x"), b"x")
        .is_err());
}
