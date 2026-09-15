use super::*;
use std::fs;

#[test]
fn dependency_promotion_bounds_wal_writes_and_preserves_durable_objects() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let cache = Cache::open(source.path()).unwrap();
    let mut store = PersistentStore::open(target.path()).unwrap();
    let hashes: Vec<_> = (0..256)
        .map(|i| {
            cache
                .put(format!("synthetic promotion {i:04}").as_bytes())
                .unwrap()
        })
        .collect();
    store
        .connection
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    store
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    store
        .promote_server_dependencies(&cache, &hashes, || Ok(()))
        .unwrap();
    let page_size: i64 = store
        .connection
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .unwrap();
    let bytes = fs::metadata(target.path().join("persistent/persistent.sqlite-wal"))
        .unwrap()
        .len();
    let frames = bytes.saturating_sub(32) / (page_size as u64 + 24);
    eprintln!("256 small objects produced {frames} WAL frames ({bytes} bytes)");
    assert!(
        frames < 128,
        "256 small objects produced {frames} WAL frames ({bytes} bytes)"
    );
    drop(store);
    let store = PersistentStore::open(target.path()).unwrap();
    let pins: i64 = store
        .connection
        .query_row("SELECT count(*) FROM server_sync_objects", [], |r| r.get(0))
        .unwrap();
    let catalog: i64 = store
        .connection
        .query_row("SELECT count(*) FROM asset_objects", [], |r| r.get(0))
        .unwrap();
    assert_eq!((pins, catalog), (256, 256));
    let cas = PayloadCas::new(target.path()).unwrap();
    for (i, hash) in hashes.iter().enumerate() {
        assert_eq!(
            cas.read_object(hash).unwrap().unwrap(),
            format!("synthetic promotion {i:04}").as_bytes()
        );
    }
}

#[test]
fn cancelled_promotion_keeps_durable_roots_and_resumes() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let cache = Cache::open(source.path()).unwrap();
    let mut store = PersistentStore::open(target.path()).unwrap();
    let hashes: Vec<_> = (0..300)
        .map(|i| {
            cache
                .put(format!("synthetic resumable {i}").as_bytes())
                .unwrap()
        })
        .collect();
    let checks = std::cell::Cell::new(0);
    let error = store
        .promote_server_dependencies(&cache, &hashes, || {
            checks.set(checks.get() + 1);
            if checks.get() == 258 {
                Err(SyncError::new("cancelled", 409))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
    assert_eq!(error.code, "cancelled");
    let count: i64 = store
        .connection
        .query_row("SELECT count(*) FROM server_sync_objects", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 256,
        "cancellation between batches leaves committed GC roots"
    );
    drop(store);
    let mut store = PersistentStore::open(target.path()).unwrap();
    store
        .promote_server_dependencies(&cache, &hashes, || Ok(()))
        .unwrap();
    let count: i64 = store
        .connection
        .query_row("SELECT count(*) FROM asset_objects", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 300);
    let cas = PayloadCas::new(target.path()).unwrap();
    for (i, hash) in hashes.iter().enumerate() {
        assert_eq!(
            cas.read_object(hash).unwrap().unwrap(),
            format!("synthetic resumable {i}").as_bytes()
        );
    }
}
