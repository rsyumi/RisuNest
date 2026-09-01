use super::device_registry::{
    DevicePermissions, IncomingSource, IncomingSourceRegistry, OutgoingDevice,
    OutgoingDeviceRegistry,
};
use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;

const SOURCE_ID: &str = "b8e9d6d7-6d4c-43d8-b00a-80c8f34478b6";
const TARGET_ID: &str = "5c39d09f-4b6d-4e21-9ad3-a674c4c1c9b0";

#[test]
fn stable_device_id_migrates_once_and_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let legacy = root.path().join("peer-delta").join("source-device-id");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, format!("{SOURCE_ID}\n")).unwrap();

    let first = super::device_registry::load_or_create_device_id(root.path()).unwrap();
    let second = super::device_registry::load_or_create_device_id(root.path()).unwrap();

    assert_eq!(first, SOURCE_ID);
    assert_eq!(second, first);
    assert_eq!(
        fs::read_to_string(root.path().join("peer-sync/device-id")).unwrap(),
        first
    );
    assert!(!legacy.exists());
}

#[test]
fn outgoing_registry_persists_digest_permissions_last_seen_and_total_bytes() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    registry
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Windows desktop".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            created_at_ms: 10,
            last_seen_ms: 20,
            total_bytes: 30,
        })
        .unwrap();
    registry.record_completed_bytes(TARGET_ID, 12).unwrap();
    registry.save().unwrap();

    let restored = OutgoingDeviceRegistry::load(root.path()).unwrap();
    assert_eq!(restored.devices().len(), 1);
    assert_eq!(restored.devices()[0].last_seen_ms, 20);
    assert_eq!(restored.devices()[0].total_bytes, 42);
    assert!(restored.devices()[0].permissions.allows_bidirectional());
    let contents = fs::read_to_string(root.path().join("peer-sync/devices.json")).unwrap();
    assert!(contents.contains("risunest.peer-device-registry/v1"));
    assert!(!contents.contains("bearer\""));
}

#[test]
fn incoming_registry_stores_bearer_and_revoke_replaces_file_atomically() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = IncomingSourceRegistry::load(root.path()).unwrap();
    registry
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 25,
            total_bytes: 40,
        })
        .unwrap();
    registry.save().unwrap();
    assert!(
        fs::read_to_string(root.path().join("peer-sync/sources.json"))
            .unwrap()
            .contains(&"c".repeat(64))
    );

    registry.remove(SOURCE_ID).unwrap();
    registry.save().unwrap();
    assert!(IncomingSourceRegistry::load(root.path())
        .unwrap()
        .sources()
        .is_empty());
    assert!(!root.path().join("peer-sync/sources.json.tmp").exists());
}

#[test]
fn registries_reject_malformed_oversized_and_linked_files() {
    let root = tempfile::tempdir().unwrap();
    let peer_root = root.path().join("peer-sync");
    fs::create_dir_all(&peer_root).unwrap();
    fs::write(peer_root.join("devices.json"), b"{").unwrap();
    assert!(OutgoingDeviceRegistry::load(root.path()).is_err());

    fs::write(peer_root.join("devices.json"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
    assert!(OutgoingDeviceRegistry::load(root.path()).is_err());

    let outside = root.path().join("outside.json");
    fs::write(&outside, b"{}").unwrap();
    fs::remove_file(peer_root.join("devices.json")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, peer_root.join("devices.json")).unwrap();
    #[cfg(unix)]
    assert!(OutgoingDeviceRegistry::load(root.path()).is_err());
}

#[test]
fn registries_reject_noncanonical_ids_credentials_and_endpoints() {
    let root = tempfile::tempdir().unwrap();
    let mut outgoing = OutgoingDeviceRegistry::load(root.path()).unwrap();
    assert!(outgoing
        .upsert(OutgoingDevice {
            device_id: "B8E9D6D7-6D4C-43D8-B00A-80C8F34478B6".into(),
            name: "Windows desktop".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 0,
            last_seen_ms: 0,
            total_bytes: 0,
        })
        .is_err());
    assert!(outgoing
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Windows desktop".into(),
            bearer_digest: "A".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 0,
            last_seen_ms: 0,
            total_bytes: 0,
        })
        .is_err());

    let mut incoming = IncomingSourceRegistry::load(root.path()).unwrap();
    assert!(incoming
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "https://sync.example.com/not-allowed".into(),
            bearer: "c".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 0,
            total_bytes: 0,
        })
        .is_err());
    assert!(incoming
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Android".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "C".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 0,
            total_bytes: 0,
        })
        .is_err());
}

#[test]
fn device_ids_are_bounded_regular_canonical_files() {
    let root = tempfile::tempdir().unwrap();
    let current = root.path().join("peer-sync/device-id");
    fs::create_dir_all(current.parent().unwrap()).unwrap();
    fs::write(&current, "x".repeat(65)).unwrap();
    assert!(super::device_registry::load_or_create_device_id(root.path()).is_err());

    fs::write(&current, "B8E9D6D7-6D4C-43D8-B00A-80C8F34478B6").unwrap();
    assert!(super::device_registry::load_or_create_device_id(root.path()).is_err());

    fs::remove_file(&current).unwrap();
    let legacy = root.path().join("peer-delta/source-device-id");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, "x".repeat(65)).unwrap();
    assert!(super::device_registry::load_or_create_device_id(root.path()).is_err());
}

#[test]
fn stale_registry_temp_does_not_block_a_save() {
    let root = tempfile::tempdir().unwrap();
    let peer_root = root.path().join("peer-sync");
    fs::create_dir_all(&peer_root).unwrap();
    fs::write(peer_root.join("sources.json.tmp"), b"stale").unwrap();

    let registry = IncomingSourceRegistry::load(root.path()).unwrap();
    registry.save().unwrap();
    assert!(peer_root.join("sources.json").exists());
}

#[test]
fn concurrent_registry_saves_do_not_collide_on_a_temp_name() {
    let root = tempfile::tempdir().unwrap();
    let app_root = root.path().to_owned();
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for device_id in [SOURCE_ID, TARGET_ID] {
        let app_root = app_root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let mut registry = OutgoingDeviceRegistry::load(&app_root).unwrap();
            registry
                .upsert(OutgoingDevice {
                    device_id: device_id.into(),
                    name: "Windows desktop".into(),
                    bearer_digest: "a".repeat(64),
                    permissions: DevicePermissions::read(),
                    created_at_ms: 0,
                    last_seen_ms: 0,
                    total_bytes: 0,
                })
                .unwrap();
            barrier.wait();
            registry.save()
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    OutgoingDeviceRegistry::load(root.path()).unwrap();
}

#[cfg(unix)]
#[test]
fn device_ids_and_peer_root_reject_symlinks() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = root.path().join("outside-id");
    fs::write(&outside, SOURCE_ID).unwrap();
    let current = root.path().join("peer-sync/device-id");
    fs::create_dir_all(current.parent().unwrap()).unwrap();
    symlink(&outside, &current).unwrap();
    assert!(super::device_registry::load_or_create_device_id(root.path()).is_err());

    fs::remove_file(&current).unwrap();
    fs::remove_dir(root.path().join("peer-sync")).unwrap();
    let outside_root = root.path().join("outside-peer-root");
    fs::create_dir(&outside_root).unwrap();
    symlink(&outside_root, root.path().join("peer-sync")).unwrap();
    assert!(OutgoingDeviceRegistry::load(root.path()).is_err());
}

#[cfg(windows)]
#[test]
fn peer_root_rejects_a_reparse_point_when_symlinks_are_supported() {
    use std::os::windows::fs::symlink_dir;

    let root = tempfile::tempdir().unwrap();
    let outside_root = root.path().join("outside-peer-root");
    fs::create_dir(&outside_root).unwrap();
    let peer_root = root.path().join("peer-sync");
    if let Err(error) = symlink_dir(&outside_root, &peer_root) {
        if error.raw_os_error() == Some(1314) {
            return;
        }
        panic!("create peer root reparse fixture: {error}");
    }
    assert!(OutgoingDeviceRegistry::load(root.path()).is_err());
}
