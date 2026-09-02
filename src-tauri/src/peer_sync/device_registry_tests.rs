use super::device_registry::{
    incoming_source_summaries, outgoing_device_summaries, record_incoming_completed_operation,
    record_incoming_completed_operation_best_effort, register_incoming_source,
    register_outgoing_claim, remove_incoming_source, revoke_outgoing_device, DevicePermissions,
    IncomingSource, IncomingSourceRegistry, OutgoingDevice, OutgoingDeviceRegistry,
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
fn reregistering_a_claim_preserves_created_at_last_seen_and_total_bytes() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    registry
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Old name".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 10,
            last_seen_ms: 20,
            total_bytes: 30,
        })
        .unwrap();

    registry
        .register_claim(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "New name".into(),
            bearer_digest: "b".repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            created_at_ms: 99,
            last_seen_ms: 99,
            total_bytes: 99,
        })
        .unwrap();

    let registered = &registry.devices()[0];
    assert_eq!(registered.name, "New name");
    assert_eq!(registered.bearer_digest, "b".repeat(64));
    assert!(registered.permissions.allows_bidirectional());
    assert_eq!(registered.created_at_ms, 10);
    assert_eq!(registered.last_seen_ms, 20);
    assert_eq!(registered.total_bytes, 30);
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
fn outgoing_completion_receipt_survives_restart_and_prevents_retry_double_counting() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    registry
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Target".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 10,
            last_seen_ms: 20,
            total_bytes: 30,
        })
        .unwrap();
    registry.save().unwrap();

    registry
        .record_completed_operation(TARGET_ID, "clone", &"b".repeat(64), 12, 40)
        .unwrap();
    let mut restarted = OutgoingDeviceRegistry::load(root.path()).unwrap();
    restarted
        .record_completed_operation(TARGET_ID, "clone", &"b".repeat(64), 12, 41)
        .unwrap();

    let restored = OutgoingDeviceRegistry::load(root.path()).unwrap();
    assert_eq!(restored.devices()[0].total_bytes, 42);
    assert_eq!(restored.devices()[0].last_seen_ms, 41);
}

#[test]
fn outgoing_receipts_are_not_evicted_while_their_devices_remain_registered() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    let device_ids = (1_u128..=257)
        .map(|value| uuid::Uuid::from_u128(value).to_string())
        .collect::<Vec<_>>();
    for device_id in &device_ids {
        registry
            .upsert(OutgoingDevice {
                device_id: device_id.clone(),
                name: "Target".into(),
                bearer_digest: "a".repeat(64),
                permissions: DevicePermissions::read(),
                created_at_ms: 10,
                last_seen_ms: 20,
                total_bytes: 0,
            })
            .unwrap();
    }
    registry.save().unwrap();

    for (index, device_id) in device_ids.iter().enumerate() {
        registry
            .record_completed_operation(device_id, "clone", &format!("{index:064x}"), 1, 40)
            .unwrap();
    }
    let mut restarted = OutgoingDeviceRegistry::load(root.path()).unwrap();
    restarted
        .record_completed_operation(&device_ids[0], "clone", &"0".repeat(64), 1, 41)
        .unwrap();

    let restored = OutgoingDeviceRegistry::load(root.path()).unwrap();
    assert_eq!(restored.devices()[0].total_bytes, 1);
    assert_eq!(restored.devices()[0].last_seen_ms, 41);
}

#[test]
fn reregistering_an_incoming_source_rotates_credentials_without_erasing_history() {
    let root = tempfile::tempdir().unwrap();
    register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Old source".into(),
            endpoint: "http://192.168.0.5:32145".into(),
            bearer: "a".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 25,
            total_bytes: 40,
        },
    )
    .unwrap();

    register_incoming_source(
        root.path(),
        IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Renamed source".into(),
            endpoint: "http://192.168.0.6:32145".into(),
            bearer: "b".repeat(64),
            permissions: DevicePermissions::read_and_bidirectional(),
            last_seen_ms: 99,
            total_bytes: 0,
        },
    )
    .unwrap();

    let restored = IncomingSourceRegistry::load(root.path()).unwrap();
    let source = &restored.sources()[0];
    assert_eq!(source.name, "Renamed source");
    assert_eq!(source.endpoint, "http://192.168.0.6:32145");
    assert_eq!(source.bearer, "b".repeat(64));
    assert!(source.permissions.allows_bidirectional());
    assert_eq!(source.last_seen_ms, 25);
    assert_eq!(source.total_bytes, 40);
}

#[test]
fn incoming_completion_accounting_persists_positive_and_zero_byte_successes() {
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

    registry
        .record_completed_operation(SOURCE_ID, 12, 30)
        .unwrap();
    registry
        .record_completed_operation(SOURCE_ID, 0, 31)
        .unwrap();

    let restored = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(restored.sources()[0].total_bytes, 52);
    assert_eq!(restored.sources()[0].last_seen_ms, 31);
}

#[test]
fn incoming_completion_accounting_rejects_overflow_without_mutating_memory_or_file() {
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
            total_bytes: u64::MAX,
        })
        .unwrap();
    registry.save().unwrap();
    let before = fs::read(root.path().join("peer-sync/sources.json")).unwrap();

    assert!(registry
        .record_completed_operation(SOURCE_ID, 1, 30)
        .is_err());
    assert_eq!(registry.sources()[0].total_bytes, u64::MAX);
    assert_eq!(registry.sources()[0].last_seen_ms, 25);
    assert_eq!(
        fs::read(root.path().join("peer-sync/sources.json")).unwrap(),
        before
    );
}

#[test]
fn incoming_completion_accounting_does_not_infer_an_outgoing_completion() {
    let root = tempfile::tempdir().unwrap();
    let mut outgoing = OutgoingDeviceRegistry::load(root.path()).unwrap();
    outgoing
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Target".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 1,
            last_seen_ms: 2,
            total_bytes: 3,
        })
        .unwrap();
    outgoing.save().unwrap();

    record_incoming_completed_operation(root.path(), TARGET_ID, 99).unwrap();

    assert_eq!(
        OutgoingDeviceRegistry::load(root.path()).unwrap().devices()[0].total_bytes,
        3
    );
    assert!(IncomingSourceRegistry::load(root.path())
        .unwrap()
        .sources()
        .is_empty());
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

#[test]
fn concurrent_outgoing_claims_preserve_both_devices() {
    let root = tempfile::tempdir().unwrap();
    let app_root = root.path().to_owned();
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for device_id in [SOURCE_ID, TARGET_ID] {
        let app_root = app_root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            register_outgoing_claim(
                &app_root,
                OutgoingDevice {
                    device_id: device_id.to_owned(),
                    name: "Windows desktop".to_owned(),
                    bearer_digest: "a".repeat(64),
                    permissions: DevicePermissions::read(),
                    created_at_ms: 0,
                    last_seen_ms: 0,
                    total_bytes: 0,
                },
            )
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    let registered = OutgoingDeviceRegistry::load(root.path()).unwrap();
    assert_eq!(registered.devices().len(), 2);
    assert!(registered
        .devices()
        .iter()
        .any(|device| device.device_id == SOURCE_ID));
    assert!(registered
        .devices()
        .iter()
        .any(|device| device.device_id == TARGET_ID));
}

#[test]
fn concurrent_incoming_registrations_preserve_both_sources() {
    let root = tempfile::tempdir().unwrap();
    let app_root = root.path().to_owned();
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for device_id in [SOURCE_ID, TARGET_ID] {
        let app_root = app_root.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            register_incoming_source(
                &app_root,
                IncomingSource {
                    device_id: device_id.to_owned(),
                    name: "Android".to_owned(),
                    endpoint: "http://192.168.0.5:32145".to_owned(),
                    bearer: "c".repeat(64),
                    permissions: DevicePermissions::read(),
                    last_seen_ms: 0,
                    total_bytes: 0,
                },
            )
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    let registered = IncomingSourceRegistry::load(root.path()).unwrap();
    assert_eq!(registered.sources().len(), 2);
    assert!(registered
        .sources()
        .iter()
        .any(|source| source.device_id == SOURCE_ID));
    assert!(registered
        .sources()
        .iter()
        .any(|source| source.device_id == TARGET_ID));
}

#[test]
fn best_effort_incoming_completion_leaves_overflowed_registry_unchanged() {
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
            total_bytes: u64::MAX,
        })
        .unwrap();
    registry.save().unwrap();
    let before = fs::read(root.path().join("peer-sync/sources.json")).unwrap();

    record_incoming_completed_operation_best_effort(root.path(), SOURCE_ID, 1);

    assert_eq!(
        fs::read(root.path().join("peer-sync/sources.json")).unwrap(),
        before
    );
}

#[test]
fn best_effort_incoming_completion_ignores_a_malformed_registry() {
    let root = tempfile::tempdir().unwrap();
    let peer_root = root.path().join("peer-sync");
    fs::create_dir_all(&peer_root).unwrap();
    fs::write(peer_root.join("sources.json"), b"{").unwrap();

    record_incoming_completed_operation_best_effort(root.path(), SOURCE_ID, 1);

    assert_eq!(fs::read(peer_root.join("sources.json")).unwrap(), b"{");
}

#[test]
fn registry_command_views_are_directional_and_never_serialize_credentials() {
    let root = tempfile::tempdir().unwrap();
    let mut outgoing = OutgoingDeviceRegistry::load(root.path()).unwrap();
    outgoing
        .upsert(OutgoingDevice {
            device_id: TARGET_ID.into(),
            name: "Target".into(),
            bearer_digest: "a".repeat(64),
            permissions: DevicePermissions::read(),
            created_at_ms: 1,
            last_seen_ms: 2,
            total_bytes: 3,
        })
        .unwrap();
    outgoing.save().unwrap();
    let mut incoming = IncomingSourceRegistry::load(root.path()).unwrap();
    incoming
        .upsert(IncomingSource {
            device_id: SOURCE_ID.into(),
            name: "Source".into(),
            endpoint: "http://127.0.0.1:32145".into(),
            bearer: "b".repeat(64),
            permissions: DevicePermissions::read(),
            last_seen_ms: 4,
            total_bytes: 5,
        })
        .unwrap();
    incoming.save().unwrap();

    let outgoing = outgoing_device_summaries(root.path()).unwrap();
    let incoming = incoming_source_summaries(root.path()).unwrap();

    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0].device_id, TARGET_ID);
    assert_eq!(incoming.len(), 1);
    assert_eq!(incoming[0].device_id, SOURCE_ID);
    let outgoing_json = serde_json::to_string(&outgoing).unwrap();
    let incoming_json = serde_json::to_string(&incoming).unwrap();
    assert!(!outgoing_json.contains("bearer"));
    assert!(!incoming_json.contains("bearer"));
    assert!(!incoming_json.contains("127.0.0.1"));

    let mut revoked_live = Vec::new();
    revoke_outgoing_device(root.path(), TARGET_ID, |device_id| {
        revoked_live.push(device_id.to_owned())
    })
    .unwrap();
    remove_incoming_source(root.path(), SOURCE_ID).unwrap();

    assert_eq!(revoked_live, vec![TARGET_ID]);
    assert!(OutgoingDeviceRegistry::load(root.path())
        .unwrap()
        .devices()
        .is_empty());
    assert!(IncomingSourceRegistry::load(root.path())
        .unwrap()
        .sources()
        .is_empty());
    assert!(root.path().join("peer-sync/sources.json").is_file());
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
