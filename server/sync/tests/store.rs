mod common;
use common::*;
use risunest_sync_server::{config::Config, store::Store};
use risunest_sync_wire::{hash, Domain, ReadFence, RecordVersion, TerminalStatus};
use std::sync::Arc;

#[test]
fn owner_lock_and_reinitialization_protect_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    assert_eq!(Store::open(dir.path()).err().unwrap().code, "data-dir-busy");
    let head = store.head().unwrap();
    drop(store);
    assert_eq!(
        Store::init(dir.path()).err().unwrap().code,
        "already-initialized"
    );
    assert_eq!(Store::open(dir.path()).unwrap().head().unwrap(), head);
}
#[test]
fn only_loopback_listener_and_absolute_storage_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    for listen in ["0.0.0.0:4319", "192.168.1.2:4319", "[::]:4319"] {
        assert!(Config {
            data_dir: dir.path().into(),
            listen: listen.parse().unwrap(),
            https_proxy: true
        }
        .validate()
        .is_err());
    }
    assert!(Config {
        data_dir: "relative".into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        https_proxy: false
    }
    .validate()
    .is_err());
}
#[test]
fn tokens_are_scoped_verifiers_and_revocation_is_checked_on_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let c = store.add_device().unwrap();
    assert_eq!(c.token.len(), 64);
    assert!(store.authenticate("other-library", &c.token).is_err());
    assert!(store.authenticate(&c.library_id, &"0".repeat(64)).is_err());
    let a = store.authenticate(&c.library_id, &c.token).unwrap();
    store.revoke_device(&a.id).unwrap();
    assert!(store.authenticate(&c.library_id, &c.token).is_err());
    assert!(store.put_object(&a, &hash(b"x"), b"x").is_err());
    assert!(store.stage_changes(&a, &changes("a", b"x")).is_err());
    drop(store);
    // Synthetic credential only. Never print token bytes.
    let bytes = std::fs::read(dir.path().join("metadata.sqlite")).unwrap();
    assert!(!bytes
        .windows(c.token.len())
        .any(|w| w == c.token.as_bytes()));
}
#[test]
fn objects_publish_before_metadata_and_raw_content_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let head = store.head().unwrap();
    assert!(store.put_object(&a, &hash(b"x"), b"wrong").is_err());
    assert!(store.put_object(&a, "../../metadata.sqlite", b"x").is_err());
    assert_eq!(store.head().unwrap(), head);
    for body in [b"".as_slice(), br#"{ "x":1.0 }"#, &[0xff, 0]] {
        store.put_object(&a, &hash(body), body).unwrap();
        assert_eq!(store.get_object(&hash(body)).unwrap(), body);
    }
    assert_eq!(store.head().unwrap(), head);
}
#[test]
fn commit_retry_precedes_head_and_staging_checks_and_conflicting_identity_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let head = store.head().unwrap();
    store.put_object(&a, &hash(b"x"), b"x").unwrap();
    let intent = stage(&store, &a, &head, 1, &changes("a", b"x"));
    let committed = store.commit(&a, &intent, &head.etag()).unwrap();
    assert_eq!(committed.status, TerminalStatus::Committed);
    assert_eq!(committed.head.seq.as_str(), "1");
    let mut retry = intent.clone();
    retry.staged_changes_id = "another-locator".into();
    assert_eq!(store.commit(&a, &retry, &head.etag()).unwrap(), committed);
    retry.changes_digest = hash(b"different");
    assert_eq!(
        store.commit(&a, &retry, &head.etag()).unwrap_err().code,
        "operation-intent-conflict"
    );
    assert_eq!(store.head().unwrap(), committed.head);
}
#[test]
fn missing_dependencies_and_wrong_before_or_fence_never_advance_head() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let head = store.head().unwrap();
    let intent = stage(&store, &a, &head, 1, &changes("a", b"missing"));
    let failed = store.commit(&a, &intent, &head.etag()).unwrap();
    assert_eq!(failed.error.as_deref(), Some("missing-dependency"));
    store.put_object(&a, &hash(b"missing"), b"missing").unwrap();
    assert_eq!(store.commit(&a, &intent, &head.etag()).unwrap(), failed);
    let mut c = changes("a", b"missing");
    c.changes[0].before = RecordVersion::Tombstone {
        deletion_id: "deleted".into(),
    };
    let intent = stage(&store, &a, &head, 2, &c);
    assert_eq!(
        store
            .commit(&a, &intent, &head.etag())
            .unwrap()
            .error
            .as_deref(),
        Some("before-version-mismatch")
    );
    let mut c = changes("a", b"missing");
    c.read_fences.push(ReadFence {
        domain: Domain::Library,
        key: "owner".into(),
        version: RecordVersion::Tombstone {
            deletion_id: "deleted".into(),
        },
    });
    let intent = stage(&store, &a, &head, 3, &c);
    assert_eq!(
        store
            .commit(&a, &intent, &head.etag())
            .unwrap()
            .error
            .as_deref(),
        Some("read-fence-mismatch")
    );
    assert_eq!(store.head().unwrap(), head);
}
#[test]
fn device_scoping_isolates_staging_receipts_ack_and_revocation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let b = device(&store);
    let head = store.head().unwrap();
    store.put_object(&a, &hash(b"x"), b"x").unwrap();
    let intent = stage(&store, &a, &head, 1, &changes("a", b"x"));
    assert!(store
        .cancel_staged_changes(&b, &intent.staged_changes_id)
        .is_err());
    let failed = store.commit(&b, &intent, &head.etag()).unwrap();
    assert_eq!(failed.error.as_deref(), Some("staging-not-found"));
    let receipt = store.commit(&a, &intent, &head.etag()).unwrap();
    assert!(store.receipt(&b, &receipt.operation_id).is_err());
    assert!(store.acknowledge(&a, "wrong", &acks(&1.into())).is_err());
    assert!(store
        .acknowledge(&a, &head.epoch, &acks(&2.into()))
        .is_err());
    store
        .acknowledge(&a, &head.epoch, &acks(&1.into()))
        .unwrap();
    assert!(store
        .acknowledge(&a, &head.epoch, &acks(&0.into()))
        .is_err());
    store
        .acknowledge(&b, &head.epoch, &acks(&0.into()))
        .unwrap();
    store.revoke_device(&a.id).unwrap();
    store
        .put_object(&b, &hash(b"still-allowed"), b"still-allowed")
        .unwrap();
}
#[test]
fn simultaneous_devices_get_one_commit_one_stale_then_both_independent_changes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::init(dir.path()).unwrap());
    let a = device(&store);
    let b = device(&store);
    let head = store.head().unwrap();
    store.put_object(&a, &hash(b"x"), b"x").unwrap();
    let ia = stage(&store, &a, &head, 1, &changes("a", b"x"));
    let ib = stage(&store, &b, &head, 1, &changes("b", b"x"));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let outcomes = std::thread::scope(|scope| {
        let sa = store.clone();
        let ba = barrier.clone();
        let da = a.clone();
        let ha = head.clone();
        let first = scope.spawn(move || {
            ba.wait();
            sa.commit(&da, &ia, &ha.etag()).unwrap()
        });
        barrier.wait();
        let second = store.commit(&b, &ib, &head.etag()).unwrap();
        (first.join().unwrap(), second)
    });
    assert_eq!(
        [&outcomes.0, &outcomes.1]
            .iter()
            .filter(|r| r.status == TerminalStatus::Committed)
            .count(),
        1
    );
    let (loser, key) = if outcomes.0.status == TerminalStatus::Stale {
        (&a, "a")
    } else {
        (&b, "b")
    };
    let head = store.head().unwrap();
    let retry = stage(&store, loser, &head, 2, &changes(key, b"x"));
    assert_eq!(
        store.commit(loser, &retry, &head.etag()).unwrap().status,
        TerminalStatus::Committed
    );
    assert!(matches!(
        store.record(Domain::Library, "a").unwrap(),
        RecordVersion::Live { .. }
    ));
    assert!(matches!(
        store.record(Domain::Library, "b").unwrap(),
        RecordVersion::Live { .. }
    ));
}
#[test]
fn staging_quota_can_be_released_without_affecting_another_device() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let b = device(&store);
    let mut ids = Vec::new();
    for _ in 0..16 {
        ids.push(
            store
                .stage_changes(&a, &changes("a", b"x"))
                .unwrap()
                .staged_changes_id,
        );
    }
    assert_eq!(
        store
            .stage_changes(&a, &changes("a", b"x"))
            .err()
            .unwrap()
            .code,
        "staging-quota"
    );
    store.stage_changes(&b, &changes("b", b"x")).unwrap();
    store.cancel_staged_changes(&a, &ids[0]).unwrap();
    store.stage_changes(&a, &changes("a", b"x")).unwrap();
}

#[test]
fn linked_data_directory_and_cas_shard_cannot_escape_private_storage() {
    fn link_directory(link: &std::path::Path, target: &std::path::Path) {
        #[cfg(windows)]
        {
            // Directory junctions require no symlink privilege. Arguments refer
            // only to this test's temporary directories; no cleanup shell runs.
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "synthetic junction creation failed"
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }
    let parent = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let linked = parent.path().join("linked-root");
    link_directory(&linked, external.path());
    assert_eq!(
        Store::init(&linked).err().unwrap().code,
        "unsafe-storage-path"
    );
    assert!(!external.path().join("metadata.sqlite").exists());
    let root = parent.path().join("private");
    let store = Store::init(&root).unwrap();
    let a = device(&store);
    let digest = hash(b"escape");
    let shard = root.join("objects").join(&digest[..2]);
    link_directory(&shard, external.path());
    assert_eq!(
        store.put_object(&a, &digest, b"escape").unwrap_err().code,
        "unsafe-storage-path"
    );
    assert!(!external.path().join(&digest).exists());
}
