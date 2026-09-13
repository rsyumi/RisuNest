use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct Cancellation {
    direction: AtomicUsize,
    requests: AtomicUsize,
    cancelled: Arc<AtomicBool>,
}
impl Cancellation {
    fn check(&self) -> crate::server_sync::Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(crate::server_sync::SyncError::new("cancelled", 409))
        } else {
            Ok(())
        }
    }
    fn arm(&self, direction: usize) {
        self.requests.store(0, Ordering::SeqCst);
        self.cancelled.store(false, Ordering::SeqCst);
        self.direction.store(direction, Ordering::SeqCst);
    }
}

fn observed_fixture(cancellation: Arc<Cancellation>) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let server = Arc::new(Store::init(root.path()).unwrap());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let entered = runtime.enter();
    let router = http::router(server.clone()).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let cancellation = cancellation.clone();
            async move {
                let direction = cancellation.direction.load(Ordering::SeqCst);
                if (direction == 1 && request.headers().contains_key("range"))
                    || (direction == 2 && request.uri().path().contains("/chunks/"))
                {
                    cancellation.requests.fetch_add(1, Ordering::SeqCst);
                    cancellation.cancelled.store(true, Ordering::SeqCst);
                }
                next.run(request).await
            }
        },
    ));
    drop(entered);
    let task = runtime.spawn(async move { axum::serve(listener, router).await.unwrap() });
    Fixture {
        _server_root: root,
        server,
        runtime,
        task,
        endpoint,
    }
}

fn remove_roots(store: &mut PersistentStore, alias: &AssetAlias) {
    store
        .delete_asset_alias("asset", &alias.key, store.revision().unwrap())
        .unwrap();
    for snapshot in store.snapshot_list().unwrap() {
        store.snapshot_delete(&snapshot.id).unwrap();
    }
}

fn backup(store: &mut PersistentStore, probe: &dyn crate::local_backup::CancellationProbe) {
    let directory = tempfile::tempdir().unwrap();
    let staging = directory.path().join("staging");
    std::fs::create_dir(&staging).unwrap();
    let revision = store.revision().unwrap();
    crate::portable_backup::create_verified_library_backup(
        store,
        revision,
        &directory.path().join("synthetic.risunest"),
        &staging,
        probe,
    )
    .unwrap();
}

#[test]
fn backup_does_not_resurrect_gc_objects_even_after_unbind() {
    for unbind in [false, true] {
        let fixture = Fixture::new();
        let (_root, mut store) = prepared();
        fixture.bind(&mut store);
        let alias = put(&mut store, "assets/gc.png", b"synthetic GC payload");
        let hash = alias.object_hash.as_ref().unwrap();
        let cas = PayloadCas::new(store.repository_root()).unwrap();
        store
            .asset_residency_set_policy(AssetPolicy::Remote, || Ok(()))
            .unwrap();
        store.asset_residency_evict(|| Ok(())).unwrap();
        store
            .asset_residency_set_policy(AssetPolicy::Full, || Ok(()))
            .unwrap();
        if unbind {
            store.server_unbind().unwrap();
        }
        remove_roots(&mut store, &alias);
        store
            .asset_gc_delete_page(4096, None, i64::MAX / 2, 0)
            .unwrap();
        assert_eq!(
            cas.stat_object(hash).unwrap(),
            None,
            "GC must delete the fixture"
        );
        assert!(store
            .query_asset_object_catalog(4096, None)
            .unwrap()
            .items
            .iter()
            .all(|item| item.object_hash != *hash));
        assert!(Residency::open(store.repository_root())
            .unwrap()
            .object(hash, None)
            .unwrap()
            .is_some());
        backup(&mut store, &crate::local_backup::NeverCancelled);
        assert_eq!(
            cas.stat_object(hash).unwrap(),
            None,
            "backup must respect physical GC"
        );
    }
}

#[test]
fn backup_keeps_registered_remote_objects_without_current_references() {
    let fixture = Fixture::new();
    let (_root, mut store) = prepared();
    fixture.bind(&mut store);
    let bytes = b"synthetic registered orphan";
    let alias = put(&mut store, "assets/orphan.png", bytes);
    let hash = alias.object_hash.as_ref().unwrap();
    store
        .asset_residency_set_policy(AssetPolicy::Remote, || Ok(()))
        .unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    remove_roots(&mut store, &alias);
    let cas = PayloadCas::new(store.repository_root()).unwrap();
    assert_eq!(cas.stat_object(hash).unwrap(), None);
    backup(&mut store, &crate::local_backup::NeverCancelled);
    assert_eq!(cas.read_object(hash).unwrap().unwrap(), bytes);
}

#[test]
fn backup_keeps_remote_snapshot_payloads() {
    let fixture = Fixture::new();
    let (_root, mut store) = prepared();
    fixture.bind(&mut store);
    let bytes = b"synthetic snapshot payload";
    let alias = put(&mut store, "assets/snapshot-backup.png", bytes);
    store.snapshot_create("synthetic-backup-root").unwrap();
    store
        .delete_asset_alias("asset", &alias.key, store.revision().unwrap())
        .unwrap();
    store
        .asset_residency_set_policy(AssetPolicy::Remote, || Ok(()))
        .unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    let cas = PayloadCas::new(store.repository_root()).unwrap();
    let hash = alias.object_hash.as_ref().unwrap();
    assert_eq!(cas.stat_object(hash).unwrap(), None);
    backup(&mut store, &crate::local_backup::NeverCancelled);
    assert_eq!(cas.read_object(hash).unwrap().unwrap(), bytes);
}

#[test]
fn backup_keeps_snapshot_payloads_after_restoring_an_older_catalog() {
    let fixture = Fixture::new();
    let (root, mut store) = prepared();
    fixture.bind(&mut store);
    let before = store.snapshot_create("synthetic-before-asset").unwrap();
    let bytes = b"synthetic later snapshot payload";
    let alias = put(&mut store, "assets/later-snapshot.png", bytes);
    let cas = PayloadCas::new(store.repository_root()).unwrap();
    let owner_bytes = b"synthetic later owner payload";
    let owner = cas.prepare_bytes(owner_bytes).unwrap();
    commit_owner(
        &mut store,
        &[
            crate::asset_repository::owner_manifest_codec::OwnerManifestEntry {
                tuple: [
                    "synthetic".into(),
                    "assets/later-owner.png".into(),
                    "png".into(),
                ],
                payload_hash: Some(
                    hex::decode(&owner.content_hash)
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
            },
        ],
    );
    store.snapshot_create("synthetic-with-asset").unwrap();
    store
        .asset_residency_set_policy(AssetPolicy::Remote, || Ok(()))
        .unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    store.snapshot_restore_request(&before.id).unwrap();
    drop(store);
    let mut store = PersistentStore::open(root.path()).unwrap();
    let hash = alias.object_hash.as_ref().unwrap();
    assert!(store
        .query_asset_object_catalog(4096, None)
        .unwrap()
        .items
        .iter()
        .all(|item| item.object_hash != *hash));
    let cas = PayloadCas::new(store.repository_root()).unwrap();
    assert_eq!(cas.stat_object(hash).unwrap(), None);
    assert_eq!(cas.stat_object(&owner.content_hash).unwrap(), None);
    backup(&mut store, &crate::local_backup::NeverCancelled);
    assert_eq!(
        cas.read_object(hash).unwrap().as_deref(),
        Some(bytes.as_slice())
    );
    assert_eq!(
        cas.read_object(&owner.content_hash).unwrap().as_deref(),
        Some(owner_bytes.as_slice())
    );
}

#[test]
fn cancellation_during_cas_copy_does_not_publish_partial_payload() {
    struct CancelAfterRead<'a> {
        bytes: std::io::Cursor<Vec<u8>>,
        cancellation: &'a Cancellation,
    }
    impl std::io::Read for CancelAfterRead<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let read = std::io::Read::read(&mut self.bytes, buffer)?;
            self.cancellation.cancelled.store(true, Ordering::SeqCst);
            Ok(read)
        }
    }
    let root = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(root.path()).unwrap();
    let bytes = vec![42; 128 * 1024];
    let hash = risunest_sync_wire::hash(&bytes);
    let size = bytes.len() as u64;
    let cancellation = Cancellation::default();
    let mut reader = CancelAfterRead {
        bytes: std::io::Cursor::new(bytes),
        cancellation: &cancellation,
    };
    let result =
        crate::server_sync::transfer::prepare_checked(&cas, &mut reader, &hash, size, &|| {
            cancellation.check()
        });
    assert!(matches!(result, Err(error) if error.code == "cancelled"));
    assert_eq!(cas.stat_object(&hash).unwrap(), None);
    assert_eq!(
        std::fs::read_dir(root.path().join("assets-v2/staging"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn full_policy_cancels_during_last_object_and_can_retry() {
    let cancellation = Arc::new(Cancellation::default());
    let fixture = observed_fixture(cancellation.clone());
    let (_root, mut store) = prepared();
    fixture.bind(&mut store);
    let bytes = vec![73; 17 * 1024 * 1024];
    let alias = put(&mut store, "assets/large.png", &bytes);
    let hash = alias.object_hash.as_ref().unwrap();
    store
        .asset_residency_set_policy(AssetPolicy::Remote, || Ok(()))
        .unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    cancellation.arm(1);
    let result = store.asset_residency_set_policy(AssetPolicy::Full, || cancellation.check());
    assert!(matches!(result, Err(error) if error.code == "cancelled"));
    assert!((1..=2).contains(&cancellation.requests.load(Ordering::SeqCst)));
    let cas = PayloadCas::new(store.repository_root()).unwrap();
    assert_eq!(cas.stat_object(hash).unwrap(), None);
    cancellation.arm(0);
    store
        .asset_residency_set_policy(AssetPolicy::Full, || cancellation.check())
        .unwrap();
    assert_eq!(cas.read_object(hash).unwrap().unwrap(), bytes);
}

#[test]
fn portable_backup_cancels_during_hydration_without_publishing() {
    let cancellation = Arc::new(Cancellation::default());
    let fixture = observed_fixture(cancellation.clone());
    let (_root, mut store) = prepared();
    fixture.bind(&mut store);
    let alias = put(&mut store, "assets/backup.png", &vec![37; 17 * 1024 * 1024]);
    store
        .asset_residency_set_policy(AssetPolicy::Remote, || Ok(()))
        .unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    cancellation.arm(1);
    let directory = tempfile::tempdir().unwrap();
    let staging = directory.path().join("staging");
    std::fs::create_dir(&staging).unwrap();
    let destination = directory.path().join("synthetic.risunest");
    let revision = store.revision().unwrap();
    let result = crate::portable_backup::create_verified_library_backup(
        &mut store,
        revision,
        &destination,
        &staging,
        &crate::local_backup::AtomicCancellation::new(cancellation.cancelled.clone()),
    );
    assert!(matches!(
        result,
        Err(crate::portable_backup::Error::Cancelled)
    ));
    assert!(!destination.exists());
    assert!((1..=2).contains(&cancellation.requests.load(Ordering::SeqCst)));
    assert_eq!(
        PayloadCas::new(store.repository_root())
            .unwrap()
            .stat_object(alias.object_hash.as_ref().unwrap())
            .unwrap(),
        None
    );
    cancellation.arm(0);
    backup(&mut store, &crate::local_backup::NeverCancelled);
}

#[test]
fn offload_cancels_between_chunks_and_preserves_local_payload() {
    let cancellation = Arc::new(Cancellation::default());
    let fixture = observed_fixture(cancellation.clone());
    let (_root, mut store) = prepared();
    fixture.bind(&mut store);
    let bytes = vec![91; 17 * 1024 * 1024];
    let alias = put(&mut store, "assets/offload.png", &bytes);
    store
        .asset_residency_set_policy(AssetPolicy::Remote, || Ok(()))
        .unwrap();
    cancellation.arm(2);
    let result = store.asset_residency_evict(|| cancellation.check());
    assert!(matches!(result, Err(error) if error.code == "cancelled"));
    assert!((1..=2).contains(&cancellation.requests.load(Ordering::SeqCst)));
    let cas = PayloadCas::new(store.repository_root()).unwrap();
    let hash = alias.object_hash.as_ref().unwrap();
    assert_eq!(cas.read_object(hash).unwrap().unwrap(), bytes);
    cancellation.arm(0);
    store
        .asset_residency_evict(|| cancellation.check())
        .unwrap();
    assert_eq!(cas.stat_object(hash).unwrap(), None);
}
