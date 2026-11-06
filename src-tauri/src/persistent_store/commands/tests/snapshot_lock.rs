use super::*;
use crate::persistent_store::{active_generation, snapshot_archive::Archive};
use sha2::{Digest, Sha256};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn fixture() -> (tempfile::TempDir, tauri::App<tauri::test::MockRuntime>, String) {
    let directory = tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let stage = store.replace_begin().unwrap();
    store.replace_put_root(&stage.staging_id, &json!({"username": "captured"})).unwrap();
    store.replace_put_presets(&stage.staging_id, &[]).unwrap();
    store.replace_commit(&stage.staging_id, Some(0)).unwrap();
    let bytes = b"synthetic snapshot asset";
    let hash = hex::encode(Sha256::digest(bytes));
    let path = directory.path().join(crate::asset_repository::object_physical_key(&hash));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    let generation = active_generation(&store.connection).unwrap();
    store.connection.execute(
        "INSERT INTO asset_objects(object_hash, byte_size, created_at_ms) VALUES (?1, ?2, 0)",
        rusqlite::params![hash, bytes.len() as i64],
    ).unwrap();
    store.connection.execute(
        "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext)
         VALUES (?1, 'assets/synthetic.bin', ?2, 'asset', ?3, 'application/octet-stream', 'synthetic', 'bin')",
        rusqlite::params![generation, hash, bytes.len() as i64],
    ).unwrap();
    drop(Archive::open(&store.snapshots_dir).unwrap());
    let app = tauri::test::mock_builder()
        .build(tauri::test::mock_context(tauri::test::noop_assets())).unwrap();
    app.manage(PersistentStoreState::with_test_store(store));
    (directory, app, hash)
}

fn changed_root() -> WorkingSetCommit {
    WorkingSetCommit {
        expected_revision: 1,
        root_mutations: None,
        root: Some(json!({"username": "foreground"})),
        replace_presets: None,
        character: None,
        character_details: None,
        replace_character: None,
        add_character: None,
        conversations: None,
        delete_character_ids: None,
        plugin_storage: None,
        asset_owner_heads: None,
    ..Default::default()
    }
}

#[test]
fn foreground_commit_completes_while_snapshot_archive_is_blocked_and_capture_stays_pinned() {
    let (directory, app, hash) = fixture();
    let state = app.state::<PersistentStoreState>();
    let readers = Arc::clone(&state.store.lock().unwrap().as_ref().unwrap().active_readers);
    let archive_path = directory.path().join("persistent/snapshots/snapshots.sqlite");
    let blocker = rusqlite::Connection::open(&archive_path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

    let (snapshot_sent, snapshot_received) = mpsc::channel();
    let handle = app.handle().clone();
    let snapshot_worker = std::thread::spawn(move || {
        snapshot_sent.send(pds_snapshot_create(handle.state(), "periodic".into(), None)).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    while !readers.asset_inventory_deferred() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let capture_pinned = readers.asset_inventory_deferred();
    let (commit_sent, commit_received) = mpsc::channel();
    let handle = app.handle().clone();
    let commit_worker = std::thread::spawn(move || {
        let result = with_store_mut(handle.state(), |store| {
            let gc_refused = matches!(store.prepare_asset_gc_delete_marks(), Err(StoreError::Validation { .. }));
            let result = store.commit(&changed_root())?;
            store.connection.execute("DELETE FROM asset_aliases", [])?;
            Ok((result.revision, gc_refused))
        });
        commit_sent.send(result).unwrap();
    });
    let foreground = commit_received.recv_timeout(Duration::from_secs(2));
    let snapshot_pending = matches!(snapshot_received.try_recv(), Err(mpsc::TryRecvError::Empty));
    let inventory_pinned = readers.asset_inventory_deferred();
    let snapshot_serialized = state.snapshot_operations.try_lock().is_err();
    let (maintenance_sent, maintenance_received) = mpsc::channel();
    let handle = app.handle().clone();
    let maintenance_readers = Arc::clone(&readers);
    let maintenance_worker = std::thread::spawn(move || {
        let state = handle.state::<PersistentStoreState>();
        let result = state.acquire_device_maintenance().map(|maintenance| {
            let capture_released = !maintenance_readers.asset_inventory_deferred();
            let snapshot_finished = state.snapshot_operations.try_lock().is_ok();
            (maintenance, capture_released, snapshot_finished)
        });
        maintenance_sent.send(result).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    while !state.renderer_gate.state.lock().unwrap().maintenance_active && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let maintenance_admission_closed = state.renderer_gate.state.lock().unwrap().maintenance_active;
    let renderer_refused = state.admit_renderer_operation().is_err();
    let maintenance_before_release = maintenance_received.try_recv();
    let maintenance_pending = matches!(&maintenance_before_release, Err(mpsc::TryRecvError::Empty));
    // Always release the real SQLite write block before asserting or joining workers.
    blocker.execute_batch("ROLLBACK").unwrap();
    drop(blocker);
    commit_worker.join().unwrap();
    snapshot_worker.join().unwrap();
    maintenance_worker.join().unwrap();
    let maintenance_result = maintenance_before_release.unwrap_or_else(|_| maintenance_received.recv().unwrap());

    assert!(capture_pinned);
    assert_eq!(foreground.unwrap().unwrap(), (2, true));
    assert!(snapshot_pending);
    assert!(inventory_pinned);
    assert!(snapshot_serialized);
    assert!(maintenance_admission_closed);
    assert!(renderer_refused);
    assert!(maintenance_pending);
    let (maintenance, capture_released, snapshot_finished) = maintenance_result.unwrap();
    assert!(capture_released);
    assert!(snapshot_finished);
    assert!(state.store.lock().unwrap().is_none());
    let snapshot = snapshot_received.recv().unwrap().unwrap();
    assert_eq!(snapshot.revision, 1);
    assert!(!readers.asset_inventory_deferred());
    assert!(state.snapshot_operations.try_lock().is_ok());
    assert!(fs::read_dir(directory.path().join("persistent/snapshots")).unwrap()
        .all(|entry| !entry.unwrap().file_name().to_string_lossy().starts_with("capture-")));
    let archive = Archive::open(&directory.path().join("persistent/snapshots")).unwrap();
    assert_eq!(archive.roots().unwrap()[0].object_hashes, [hash.clone()].into());
    let restored_path = directory.path().join("captured.sqlite");
    fs::File::create(&restored_path).unwrap();
    archive.restore(&snapshot.id, &restored_path).unwrap();
    let restored = rusqlite::Connection::open(&restored_path).unwrap();
    assert_eq!(crate::persistent_store::current_revision(&restored).unwrap(), 1);
    assert_eq!(crate::persistent_store::snapshot::collect_asset_roots(&restored).unwrap().object_hashes, [hash].into());
    let root: String = restored.query_row(
        "SELECT value FROM root WHERE generation = ?1",
        [active_generation(&restored).unwrap()], |row| row.get(0),
    ).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&root).unwrap()["username"], "captured");
    drop(restored);
    drop(archive);
    drop(maintenance);
    open_renderer_persistent_store(&state, directory.path()).unwrap();
    assert_eq!(with_store(app.state(), |store| store.read_root(None)).unwrap().value["username"], "foreground");
}

#[test]
fn failed_snapshot_archive_releases_capture_and_command_guards_without_publishing() {
    let (directory, app, _) = fixture();
    let state = app.state::<PersistentStoreState>();
    let readers = Arc::clone(&state.store.lock().unwrap().as_ref().unwrap().active_readers);
    let archive_path = directory.path().join("persistent/snapshots/snapshots.sqlite");
    let archive = rusqlite::Connection::open(&archive_path).unwrap();
    archive.execute_batch(
        "CREATE TRIGGER reject_capture BEFORE INSERT ON chunks BEGIN SELECT RAISE(ABORT, 'synthetic capture failure'); END;",
    ).unwrap();
    drop(archive);

    assert!(pds_snapshot_create(app.state(), "periodic".into(), None).is_err());
    assert!(!readers.asset_inventory_deferred());
    assert!(state.snapshot_operations.try_lock().is_ok());
    assert!(state.acquire_device_maintenance().is_ok());
    assert!(fs::read_dir(directory.path().join("persistent/snapshots")).unwrap()
        .all(|entry| !entry.unwrap().file_name().to_string_lossy().starts_with("capture-")));
    assert!(Archive::open(&directory.path().join("persistent/snapshots")).unwrap().list().unwrap().is_empty());
}

#[test]
fn the_command_keeps_the_count_it_is_sent_and_refuses_one_out_of_range() {
    let (directory, app, _) = fixture();
    let snapshots = directory.path().join("persistent/snapshots");
    let listed = || -> Vec<String> {
        Archive::open(&snapshots).unwrap().list().unwrap().into_iter().map(|snapshot| snapshot.id).collect()
    };
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(pds_snapshot_create(app.state(), "manual".into(), Some(2)).unwrap().id);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(listed(), vec![ids[2].clone(), ids[1].clone()]);

    for count in [0, 9] {
        assert!(matches!(
            pds_snapshot_create(app.state(), "manual".into(), Some(count)),
            Err(StoreError::Validation { .. })
        ));
    }
    assert_eq!(listed().len(), 2, "a refused count captures nothing");

    pds_snapshot_create(app.state(), "manual".into(), None).unwrap();
    assert_eq!(listed().len(), 3, "a request without a count keeps the full count");
}
