use super::*;

fn object(bytes: &[u8]) -> ObjectIdentity {
    ObjectIdentity {
        hash: risunest_sync_wire::hash(bytes),
        size: Some((bytes.len() as u64).into()),
    }
}

#[test]
fn offline_custody_survives_expired_transfer_leases_and_reopen_until_release() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    let device = Device {
        id: store.add_device().unwrap().device_id,
    };
    let identity = object(b"synthetic retained snapshot body");
    store
        .put_object(&device, &identity.hash, b"synthetic retained snapshot body")
        .unwrap();
    let epoch = store.head().unwrap().epoch;
    let retained = store
        .retain_objects(&device, &epoch, &[identity])
        .unwrap()
        .remove(0);
    store
        .db()
        .unwrap()
        .execute("UPDATE object_leases SET expires=0", [])
        .unwrap();
    store.maintain().unwrap();
    assert!(store.object_size(&retained.hash).unwrap().is_some());
    drop(store);
    let store = Store::open(root.path()).unwrap();
    let listed = store.retained_objects(&device, &epoch, None).unwrap();
    assert_eq!(listed.objects.len(), 1);
    assert_eq!(listed.objects[0].retention_id, retained.retention_id);
    store
        .release_retained_objects(
            &device,
            &epoch,
            &[RetentionRelease {
                device_id: device.id.clone(),
                hash: retained.hash.clone(),
                retention_id: retained.retention_id,
            }],
        )
        .unwrap();
    store.maintain().unwrap();
    assert!(store.object_size(&retained.hash).unwrap().is_none());
}

#[test]
fn stale_or_other_device_release_cannot_remove_new_custody() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    let device = Device {
        id: store.add_device().unwrap().device_id,
    };
    let other = Device {
        id: store.add_device().unwrap().device_id,
    };
    let identity = object(b"synthetic shared payload");
    store
        .put_object(&device, &identity.hash, b"synthetic shared payload")
        .unwrap();
    let epoch = store.head().unwrap().epoch;
    let first = store
        .retain_objects(&device, &epoch, &[object(b"synthetic shared payload")])
        .unwrap()
        .remove(0);
    let second = store
        .retain_objects(&device, &epoch, &[identity])
        .unwrap()
        .remove(0);
    assert_ne!(first.retention_id, second.retention_id);
    store
        .release_retained_objects(
            &device,
            &epoch,
            &[RetentionRelease {
                device_id: device.id.clone(),
                hash: first.hash,
                retention_id: first.retention_id,
            }],
        )
        .unwrap();
    store
        .release_retained_objects(
            &other,
            &epoch,
            &[RetentionRelease {
                device_id: other.id.clone(),
                hash: second.hash.clone(),
                retention_id: second.retention_id.clone(),
            }],
        )
        .unwrap();
    let listed = store.retained_objects(&device, &epoch, None).unwrap();
    assert_eq!(listed.objects.len(), 1);
    assert_eq!(listed.objects[0].retention_id, second.retention_id);
    let release = RetentionRelease {
        device_id: device.id.clone(),
        hash: second.hash,
        retention_id: second.retention_id,
    };
    assert_eq!(
        store
            .release_retained_objects(&other, &epoch, std::slice::from_ref(&release))
            .unwrap_err()
            .code,
        "retention-device-active"
    );
    store.revoke_device(&device.id).unwrap();
    store
        .release_retained_objects(&other, &epoch, &[release])
        .unwrap();
}

#[test]
fn retention_is_atomic_and_requires_matching_size_and_epoch() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    let device = Device {
        id: store.add_device().unwrap().device_id,
    };
    let identity = object(b"synthetic atomic payload");
    store
        .put_object(&device, &identity.hash, b"synthetic atomic payload")
        .unwrap();
    let epoch = store.head().unwrap().epoch;
    assert!(store
        .retain_objects(&device, &epoch, &[identity, object(b"missing fixture")])
        .is_err());
    assert!(store
        .retained_objects(&device, &epoch, None)
        .unwrap()
        .objects
        .is_empty());
    let mut identity = object(b"synthetic atomic payload");
    identity.size = Some(0.into());
    assert!(store.retain_objects(&device, &epoch, &[identity]).is_err());
    assert!(store
        .retain_objects(
            &device,
            "incorrect-epoch",
            &[object(b"synthetic atomic payload")]
        )
        .is_err());
    assert!(store
        .retained_objects(&device, &epoch, None)
        .unwrap()
        .objects
        .is_empty());
}

#[test]
fn revocation_and_restored_epoch_do_not_silently_discard_snapshot_custody() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    let device = Device {
        id: store.add_device().unwrap().device_id,
    };
    let identity = object(b"synthetic historical snapshot");
    let digest = identity.hash.clone();
    store
        .put_object(&device, &digest, b"synthetic historical snapshot")
        .unwrap();
    let epoch = store.head().unwrap().epoch;
    store.retain_objects(&device, &epoch, &[identity]).unwrap();
    store.rotate_restored_epoch().unwrap();
    assert!(store.retained_objects(&device, &epoch, None).is_err());
    let restored = store.head().unwrap().epoch;
    assert_eq!(
        store
            .retained_objects(&device, &restored, None)
            .unwrap()
            .objects
            .len(),
        1
    );
    store.revoke_device(&device.id).unwrap();
    store.maintain().unwrap();
    assert!(store.object_size(&digest).unwrap().is_some());
    assert!(store.retained_objects(&device, &restored, None).is_err());
}
