use super::*;

#[test]
fn cancelled_waiter_leaves_the_active_hydration_lock_intact() {
    let lock = std::sync::Mutex::new(());
    let active = lock.lock().unwrap();
    let waiting = std::cell::Cell::new(false);
    let result = lock_with_check(&lock, &|| {
        if waiting.replace(true) {
            Err(SyncError::new("cancelled", 409))
        } else {
            Ok(())
        }
    });
    assert!(matches!(result, Err(error) if error.code == "cancelled"));
    assert!(matches!(
        lock.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
    drop(active);
    assert!(lock_with_check(&lock, &|| Ok(())).is_ok());
}

fn config(device: &str) -> StoredConfig {
    serde_json::from_value(serde_json::json!({
        "endpoint":"http://127.0.0.1:8123/", "libraryId":"library", "deviceId":device,
        "credentialId":"00000000-0000-4000-8000-000000000000"
    }))
    .unwrap()
}
fn head() -> RemoteHead {
    RemoteHead {
        head_id: hash(b"head"),
        ..RemoteHead::genesis("library".into(), "epoch".into()).unwrap()
    }
}
fn object(seed: &[u8]) -> RetainedObject {
    RetainedObject {
        hash: hash(b"payload"),
        size: 7.into(),
        retention_id: hash(seed),
    }
}

#[test]
fn custody_survives_reopen_but_stays_device_local() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut store = Residency::open(root.path()).unwrap();
    store
        .confirm(&config("device"), &head(), &[object(b"first")])
        .unwrap();
    drop(store);
    assert!(root
        .path()
        .join("server-sync/asset-residency.sqlite")
        .is_file());
    assert!(!root.path().join("asset-residency.sqlite").exists());
    let store = Residency::open(root.path()).unwrap();
    let context = Residency::context_id(&config("device"), "epoch");
    assert!(store
        .confirms(&hash(b"payload"), Some(7), &context)
        .unwrap());
    assert!(!store
        .confirms(&hash(b"payload"), Some(8), &context)
        .unwrap());
    assert!(!store.confirms(&hash(b"payload"), Some(7), "other").unwrap());
    let other = Residency::open(other.path()).unwrap();
    assert!(other.object(&hash(b"payload"), None).unwrap().is_none());
}

#[test]
fn stale_release_cannot_remove_renewed_custody() {
    let root = tempfile::tempdir().unwrap();
    let mut store = Residency::open(root.path()).unwrap();
    store
        .confirm(&config("device"), &head(), &[object(b"first")])
        .unwrap();
    let old = store.object(&hash(b"payload"), None).unwrap().unwrap();
    assert!(store.begin_release(&old).unwrap());
    assert!(store.object(&old.hash, None).unwrap().is_none());
    store
        .confirm(&config("device"), &head(), &[object(b"second")])
        .unwrap();
    store.finish_release(&old).unwrap();
    assert_eq!(
        store.object(&old.hash, None).unwrap().unwrap().retention_id,
        hash(b"second")
    );
    assert!(!store.begin_release(&old).unwrap());
}

#[test]
fn latest_release_claim_uses_the_newest_id_and_stale_completion_preserves_a_new_retain() {
    let root = tempfile::tempdir().unwrap();
    let mut store = Residency::open(root.path()).unwrap();
    store
        .confirm(&config("device"), &head(), &[object(b"first")])
        .unwrap();
    store
        .confirm(&config("device"), &head(), &[object(b"second")])
        .unwrap();
    let context = Residency::context_id(&config("device"), "epoch");
    let releasing = store
        .begin_latest_release(&hash(b"payload"), &context)
        .unwrap()
        .unwrap();
    assert_eq!(releasing.retention_id, hash(b"second"));

    let mut concurrent = Residency::open(root.path()).unwrap();
    concurrent
        .confirm(&config("device"), &head(), &[object(b"third")])
        .unwrap();
    store.finish_release(&releasing).unwrap();
    assert_eq!(
        store
            .object(&hash(b"payload"), Some(&context))
            .unwrap()
            .unwrap()
            .retention_id,
        hash(b"third")
    );
}

#[test]
fn invalid_confirmation_is_atomic_and_unknown_schema_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let mut store = Residency::open(root.path()).unwrap();
    let mut invalid = object(b"invalid");
    invalid.size = "999999999999999999999999".to_owned().try_into().unwrap();
    assert!(store
        .confirm(&config("device"), &head(), &[object(b"first"), invalid])
        .is_err());
    assert!(store.page("").unwrap().is_empty());
    let mut wrong = head();
    wrong.library_id = "other".into();
    assert!(store
        .confirm(&config("device"), &wrong, &[object(b"first")])
        .is_err());
    store.db.execute_batch("PRAGMA user_version=2").unwrap();
    drop(store);
    assert!(Residency::open(root.path()).is_err());
}
