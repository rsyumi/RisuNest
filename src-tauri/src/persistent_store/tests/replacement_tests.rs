use super::*;

#[test]
fn bounded_generation_replacement_rolls_back_deleted_and_partly_moved_batches() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let seed = stage_root(&mut store, "Original");
    store.replace_commit(&seed, Some(0)).unwrap();
    let stage = stage_root(&mut store, "Replacement");
    for (generation, value) in [("revision-1", "1"), (stage.as_str(), "2")] {
        let transaction = store.connection.transaction().unwrap();
        for index in 0..513 {
            transaction.execute(
                "INSERT INTO plugin_storage(generation,owner,storage_key,byte_size,ordinal,value) VALUES(?1,'synthetic-plugin',?2,1,?3,?4)",
                params![generation, format!("synthetic-{index:04}"), index, value],
            ).unwrap();
        }
        transaction.commit().unwrap();
    }
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER reject_later_batch BEFORE UPDATE ON plugin_storage
         WHEN NEW.generation='revision-2' AND OLD.storage_key='synthetic-0300'
         BEGIN SELECT RAISE(ABORT,'synthetic later batch failure'); END;",
        )
        .unwrap();
    assert!(store.replace_commit(&stage, Some(1)).is_err());
    assert_eq!(store.revision().unwrap(), 1);
    for (generation, value) in [("revision-1", "1"), (stage.as_str(), "2")] {
        let count: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM plugin_storage WHERE generation=?1 AND value=?2",
                params![generation, value],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 513);
    }
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM plugin_storage WHERE generation='revision-2'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        0
    );
    store
        .connection
        .execute_batch("DROP TRIGGER reject_later_batch")
        .unwrap();
    store.replace_commit(&stage, Some(1)).unwrap();
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM plugin_storage WHERE generation='revision-2' AND value='2'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        513
    );
}

#[test]
fn replacements_do_not_create_or_require_recovery_snapshots() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let seed = stage_root(&mut store, "Seed");
    store.replace_commit(&seed, Some(0)).unwrap();
    let replacement = stage_root(&mut store, "Replacement");
    store.replace_commit(&replacement, Some(1)).unwrap();
    assert!(store.snapshot_list().unwrap().is_empty());
    store.snapshots_dir = directory.path().join("blocked-snapshots");
    fs::write(&store.snapshots_dir, b"unavailable snapshot directory").unwrap();
    let replacement = stage_root(&mut store, "Final");
    store.replace_commit(&replacement, Some(2)).unwrap();
    assert_eq!(store.revision().unwrap(), 3);
    assert_eq!(store.read_root(None).unwrap().value["username"], "Final");
}

#[test]
fn prepared_replacement_rechecks_revision_before_activation() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let seed = stage_root(&mut store, "Seed");
    store
        .replace_commit(&seed, Some(0))
        .expect("activate revision one");
    let replacement = stage_root(&mut store, "Prepared replacement");

    let prepared = store
        .prepare_replace_commit(&replacement, Some(1))
        .expect("prepare replacement");
    let authorized = prepared;

    let competing = stage_root(&mut store, "Competing revision");
    store
        .replace_commit(&competing, Some(1))
        .expect("activate competing revision");
    let error = store
        .finish_prepared_replace(authorized)
        .expect_err("prepared replacement must recheck revision");

    assert!(matches!(
        error,
        StoreError::RevisionConflict {
            expected: 1,
            actual: 2
        }
    ));
    assert_eq!(store.revision().expect("read current revision"), 2);
    assert_eq!(
        store.read_root(None).expect("read current root").value["username"],
        "Competing revision"
    );
    store
        .replace_abort(&replacement)
        .expect("prepared staging remains abortable");
}

#[test]
fn prepared_replacement_commits_activation_marker_with_new_revision() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let seed = stage_root(&mut store, "Seed");
    store
        .replace_commit(&seed, Some(0))
        .expect("activate revision one");
    let replacement = stage_root(&mut store, "Peer clone");
    let prepared = store
        .prepare_replace_commit(&replacement, Some(1))
        .expect("prepare replacement");
    let authorized = prepared;

    let committed = store
        .finish_prepared_replace_with_app_kv(
            authorized,
            "external-restore-commit:synthetic",
            &json!({ "manifestId": "manifest-1", "revision": 2 }),
        )
        .expect("activate replacement and marker");

    assert_eq!(committed.revision, 2);
    assert_eq!(
        store
            .get_app_kv("external-restore-commit:synthetic")
            .expect("read activation marker"),
        Some(json!({ "manifestId": "manifest-1", "revision": 2 }))
    );
    assert_eq!(
        store.read_root(None).expect("read current root").value["username"],
        "Peer clone"
    );
}

#[test]
fn prepared_replacement_rolls_back_when_activation_marker_write_fails() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let seed = stage_root(&mut store, "Seed");
    store
        .replace_commit(&seed, Some(0))
        .expect("activate revision one");
    let replacement = stage_root(&mut store, "Must not activate");
    let prepared = store
        .prepare_replace_commit(&replacement, Some(1))
        .expect("prepare replacement");
    let authorized = prepared;
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER reject_restore_marker
             BEFORE INSERT ON app_kv
             WHEN NEW.key = 'external-restore-commit:synthetic'
             BEGIN
                 SELECT RAISE(ABORT, 'marker rejected');
             END;",
        )
        .expect("install marker failure trigger");

    store
        .finish_prepared_replace_with_app_kv(
            authorized,
            "external-restore-commit:synthetic",
            &json!({ "manifestId": "manifest-1", "revision": 2 }),
        )
        .expect_err("marker failure must abort the replacement transaction");

    assert_eq!(store.revision().expect("read preserved revision"), 1);
    assert_eq!(
        store.read_root(None).expect("read preserved root").value["username"],
        "Seed"
    );
    assert_eq!(
        store
            .get_app_kv("external-restore-commit:synthetic")
            .expect("read absent activation marker"),
        None
    );
    store
        .replace_abort(&replacement)
        .expect("rolled back staging remains abortable");
}

#[test]
fn checkpoint_truncate_reports_busy_and_truncates_when_unblocked() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    store
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0; INSERT INTO app_kv VALUES ('first', '1');")
        .expect("create initial WAL frames");
    let database_path = directory.path().join("persistent/persistent.sqlite");
    let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
    store
        .checkpoint(CheckpointMode::Passive)
        .expect("passive checkpoint");
    assert!(fs::metadata(&wal_path).expect("read passive WAL").len() > 0);

    let reader = rusqlite::Connection::open(&database_path).expect("open blocking reader");
    reader
        .execute_batch("BEGIN; SELECT value FROM app_kv WHERE key = 'first';")
        .expect("hold read snapshot");
    store
        .connection
        .execute("INSERT INTO app_kv VALUES ('second', '2')", [])
        .expect("write newer WAL frame");
    store
        .connection
        .busy_timeout(Duration::ZERO)
        .expect("disable checkpoint wait");
    assert!(store.checkpoint(CheckpointMode::Truncate).is_err());
    reader
        .execute_batch("ROLLBACK")
        .expect("release read snapshot");

    store
        .checkpoint(CheckpointMode::Truncate)
        .expect("truncate checkpoint");
    assert_eq!(
        fs::metadata(&wal_path).expect("read truncated WAL").len(),
        0
    );
}

#[test]
fn store_errors_serialize_to_the_command_contract() {
    assert_eq!(
        serde_json::to_value(StoreError::RevisionConflict {
            expected: 12,
            actual: 13,
        })
        .expect("serialize revision conflict"),
        json!({ "code": "revision-conflict", "expected": 12, "actual": 13 })
    );
    assert_eq!(
        serde_json::to_value(StoreError::SnapshotReleased).expect("serialize released snapshot"),
        json!({ "code": "snapshot-released" })
    );
    assert_eq!(
        serde_json::to_value(ConversationPage {
            revision: 4,
            items: vec![],
            next_cursor: None,
        })
        .expect("serialize empty page"),
        json!({ "revision": 4, "items": [] })
    );
}
