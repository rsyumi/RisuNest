use super::device_registry::{
    DevicePermissions, IncomingSource, IncomingSourceRegistry, OutgoingDevice,
    OutgoingDeviceRegistry,
};
use std::fs;

#[test]
fn stable_device_id_migrates_once_and_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let legacy = root.path().join("peer-delta").join("source-device-id");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, "b8e9d6d7-6d4c-43d8-b00a-80c8f34478b6\n").unwrap();

    let first = super::device_registry::load_or_create_device_id(root.path()).unwrap();
    let second = super::device_registry::load_or_create_device_id(root.path()).unwrap();

    assert_eq!(first, "b8e9d6d7-6d4c-43d8-b00a-80c8f34478b6");
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
            device_id: "target-id".into(),
            name: "Windows desktop".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            created_at_ms: 10,
            last_seen_ms: 20,
            total_bytes: 30,
        })
        .unwrap();
    registry.record_completed_bytes("target-id", 12).unwrap();
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
            device_id: "source-id".into(),
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

    registry.remove("source-id").unwrap();
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
