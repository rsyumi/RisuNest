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
        format_level: None,
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
        format_level: None,
    };
    let other = Device {
        id: store.add_device().unwrap().device_id,
        format_level: None,
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
        format_level: None,
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
        format_level: None,
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
    assert!(store.retained_objects(&device, &restored, None).is_err());
    assert_eq!(store.managed_devices().unwrap()[0].retained, 1);
    store.revoke_device(&device.id).unwrap();
    store.maintain().unwrap();
    assert!(store.object_size(&digest).unwrap().is_some());
    assert!(store.retained_objects(&device, &restored, None).is_err());
}

#[test]
fn a_revoked_predecessor_stays_listed_until_its_custody_is_released() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    let predecessor = Device {
        id: store.add_device().unwrap().device_id,
        format_level: None,
    };
    let successor = Device {
        id: store.add_device().unwrap().device_id,
        format_level: None,
    };
    let identity = object(b"synthetic resident payload");
    store
        .put_object(&predecessor, &identity.hash, b"synthetic resident payload")
        .unwrap();
    let epoch = store.head().unwrap().epoch;
    let retained = store
        .retain_objects(&predecessor, &epoch, &[identity])
        .unwrap()
        .remove(0);
    store.revoke_device(&predecessor.id).unwrap();
    store.maintain().unwrap();
    let listed = |store: &Store| {
        store
            .managed_devices()
            .unwrap()
            .iter()
            .any(|d| d.id == predecessor.id)
    };
    assert!(listed(&store));
    assert!(store.object_size(&retained.hash).unwrap().is_some());
    let release = RetentionRelease {
        device_id: predecessor.id.clone(),
        hash: retained.hash.clone(),
        retention_id: retained.retention_id,
    };
    store
        .release_retained_objects(&successor, &epoch, std::slice::from_ref(&release))
        .unwrap();
    store.maintain().unwrap();
    assert!(!listed(&store));
    assert!(store.object_size(&retained.hash).unwrap().is_none());
    // A release retried after the predecessor was removed is already satisfied.
    store
        .release_retained_objects(&successor, &epoch, &[release])
        .unwrap();
}

fn file_body(seed: u8) -> Vec<u8> {
    vec![seed; crate::store::objects::SMALL_OBJECT_BYTES + 1]
}

fn link_directory(link: &std::path::Path, target: &std::path::Path) {
    #[cfg(windows)]
    {
        // Directory junctions require no symlink privilege. Arguments refer
        // only to this test's temporary directories.
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

#[test]
fn retaining_file_objects_checks_the_data_directory_once_per_request() {
    use crate::store::objects::frame_metrics::PATH_CHECKS;
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    let device = Device {
        id: store.add_device().unwrap().device_id,
        format_level: None,
    };
    let bodies = (0..24).map(file_body).collect::<Vec<_>>();
    for body in &bodies {
        store
            .put_object(&device, &risunest_sync_wire::hash(body), body)
            .unwrap();
    }
    let epoch = store.head().unwrap().epoch;
    let checks = |bodies: &[Vec<u8>]| {
        let objects = bodies.iter().map(|body| object(body)).collect::<Vec<_>>();
        PATH_CHECKS.with(|count| count.set(0));
        store.retain_objects(&device, &epoch, &objects).unwrap();
        PATH_CHECKS.with(|count| count.get())
    };
    let few = checks(&bodies[..4]);
    let many = checks(&bodies[4..]);
    // Each further object adds its own file and at most its shard, however deep
    // the data directory is.
    assert!(
        many - few <= 2 * 16,
        "{few} checks for 4 objects, {many} for 20"
    );
}

#[test]
fn a_shard_swapped_for_a_link_between_requests_is_refused() {
    use risunest_sync_connect::media::{MediaObject, MediaRequest, MediaSigner};
    let root = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    let device = Device {
        id: store.add_device().unwrap().device_id,
        format_level: None,
    };
    let body = file_body(b'l');
    let identity = object(&body);
    store.put_object(&device, &identity.hash, &body).unwrap();
    let epoch = store.head().unwrap().epoch;
    let media = MediaObject {
        hash: identity.hash.clone(),
        size: (body.len() as u64).into(),
        mime: "image/png".into(),
    };
    let request = MediaRequest {
        refresh_url: MediaSigner::new(&[4; 32])
            .unwrap()
            .refresh_url("http://127.0.0.1:12345", &media)
            .unwrap(),
        object: media,
    };
    store
        .retain_objects(&device, &epoch, std::slice::from_ref(&identity))
        .unwrap();
    assert!(store.object_presence(&identity.hash).unwrap());
    store
        .media_access(&device, &epoch, std::slice::from_ref(&request))
        .unwrap();
    let shard = root.path().join("objects").join(&identity.hash[..2]);
    let moved = external.path().join("shard");
    std::fs::rename(&shard, &moved).unwrap();
    link_directory(&shard, &moved);
    let refusals = [
        store
            .retain_objects(&device, &epoch, std::slice::from_ref(&identity))
            .err(),
        store.object_presence(&identity.hash).err(),
        store
            .media_access(&device, &epoch, std::slice::from_ref(&request))
            .err(),
    ];
    for refusal in refusals {
        assert_eq!(refusal.map(|error| error.code), Some("unsafe-storage-path"));
    }
}
