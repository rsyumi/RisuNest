use super::super::{
    content_change_index as changes, external_storage_state as external,
    sync_selection as selection,
};
use super::*;

fn count(store: &PersistentStore, table: &str) -> i64 {
    store
        .connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
fn edit(store: &mut PersistentStore, n: i64) {
    store
        .commit(&WorkingSetCommit {
            root_mutations: Some(vec![super::super::RootMutation::Set {
                key: "synthetic".into(),
                value: json!(n),
            }]),
            ..empty_working_set_commit(store.revision().unwrap())
        })
        .unwrap();
}
fn cursor(store: &mut PersistentStore, id: &str, revision: i64) {
    let tx = store.connection.transaction().unwrap();
    changes::commit_cursor(&tx, id, &active_generation(&tx).unwrap(), revision).unwrap();
    tx.commit().unwrap();
}

#[test]
fn external_changes_coalesce_and_rollback_without_server_binding() {
    let (_dir, mut store, _) = open_fixture();
    assert_eq!(count(&store, "content_changes"), 0);
    edit(&mut store, 1);
    edit(&mut store, 2);
    assert_eq!(count(&store, "content_changes"), 1);
    assert_eq!(count(&store, "server_sync_dirty"), 0);
    let revision = store.revision().unwrap();
    store.connection.execute_batch("CREATE TRIGGER synthetic_failure BEFORE UPDATE ON meta WHEN NEW.key='currentRevision' BEGIN SELECT RAISE(ABORT,'synthetic'); END").unwrap();
    assert!(store
        .commit(&WorkingSetCommit {
            root: Some(json!({"synthetic":3})),
            ..empty_working_set_commit(revision)
        })
        .is_err());
    assert_eq!(store.revision().unwrap(), revision);
    let tracked: i64 = store
        .connection
        .query_row("SELECT revision FROM content_changes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tracked, revision);
    assert_eq!(count(&store, "content_change_context"), 0);
}

#[test]
fn external_cursor_and_rows_share_snapshot_and_preserve_r_plus_one() {
    let (_dir, mut store, _) = open_fixture();
    cursor(&mut store, "backup", 1);
    edit(&mut store, 1);
    let lease_id = store.acquire_revision(2).unwrap().lease;
    edit(&mut store, 2);
    let lease = store.revision_leases.get(&lease_id).unwrap();
    assert_eq!(
        changes::window(lease, "backup").unwrap(),
        changes::ChangeWindow::Incremental { after_revision: 1 }
    );
    let page = changes::page(lease, 1, None, 128).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].kind, "root");
    assert_eq!(
        store.read_root(Some(&lease_id)).unwrap().value["synthetic"],
        json!(1)
    );
    cursor(&mut store, "backup", 2);
    store.release_revision(&lease_id).unwrap();
    let lease_id = store.acquire_revision(3).unwrap().lease;
    assert_eq!(
        changes::page(store.revision_leases.get(&lease_id).unwrap(), 2, None, 128)
            .unwrap()
            .len(),
        1
    );
    store.release_revision(&lease_id).unwrap();
}

#[test]
fn external_delete_recreate_projects_final_state_and_messages_dirty_conversation() {
    let (_dir, mut store, _) = open_fixture();
    let generation = active_generation(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    changes::begin_mutation(&tx, &generation, 2, "local").unwrap();
    tx.execute(
        "INSERT INTO plugin_storage VALUES(?1,'synthetic',1,0,'1')",
        [&generation],
    )
    .unwrap();
    tx.execute(
        "DELETE FROM plugin_storage WHERE storage_key='synthetic'",
        [],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO plugin_storage VALUES(?1,'synthetic',1,0,'2')",
        [&generation],
    )
    .unwrap();
    tx.execute(
        "UPDATE messages SET message_id=NULL WHERE generation=?1",
        [&generation],
    )
    .unwrap();
    changes::finish_mutation(&tx).unwrap();
    super::super::commit::set_active(&tx, 2, &generation).unwrap();
    tx.commit().unwrap();
    let lease_id = store.acquire_revision(2).unwrap().lease;
    let lease = store.revision_leases.get(&lease_id).unwrap();
    let page = changes::page(lease, 1, None, 128).unwrap();
    assert!(page.iter().any(|k| k.kind == "conversation"));
    assert_eq!(page.iter().filter(|k| k.kind == "plugin").count(), 1);
    let value: String = lease
        .connection
        .query_row(
            "SELECT value FROM plugin_storage WHERE storage_key='synthetic'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(value, "2");
    store.release_revision(&lease_id).unwrap();
}

#[test]
fn external_floor_respects_each_consumer_and_capture_reservations() {
    let (_dir, mut store, _) = open_fixture();
    cursor(&mut store, "slow", 1);
    edit(&mut store, 1);
    cursor(&mut store, "fast", 2);
    let tx = store.connection.transaction().unwrap();
    changes::reserve(&tx, "capture", &active_generation(&tx).unwrap(), 1).unwrap();
    assert_eq!(changes::prune(&tx).unwrap(), 1);
    changes::require_rebuild(&tx, "slow").unwrap();
    assert_eq!(changes::prune(&tx).unwrap(), 1);
    tx.execute(
        "DELETE FROM content_capture_reservations WHERE id='capture'",
        [],
    )
    .unwrap();
    assert_eq!(changes::prune(&tx).unwrap(), 2);
    tx.commit().unwrap();
    let lease_id = store.acquire_revision(2).unwrap().lease;
    let lease = store.revision_leases.get(&lease_id).unwrap();
    assert_eq!(
        changes::window(lease, "slow").unwrap(),
        changes::ChangeWindow::Rebuild
    );
    assert!(changes::page(lease, 1, None, 128).is_err());
    store.release_revision(&lease_id).unwrap();
}

fn select_external(store: &mut PersistentStore) {
    let epoch = selection::read(&store.connection).unwrap().epoch;
    let tx = store.connection.transaction().unwrap();
    selection::select(
        &tx,
        &epoch,
        &selection::SyncTarget::External("synthetic-connection".into()),
    )
    .unwrap();
    tx.commit().unwrap();
}
fn capture(store: &mut PersistentStore, job: &str) -> selection::CaptureIdentity {
    let identity = selection::identity(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    let id = external::register_capture(
        &tx,
        "synthetic-capture",
        &identity,
        "library",
        "codec-1",
        "",
        &"a".repeat(64),
        "synthetic-consumer",
    )
    .unwrap();
    external::prepare_publication(
        &tx,
        &external::PublishIntent {
            job_id: job,
            connection_id: "synthetic-connection",
            repository_id: "synthetic-repository",
            capture_id: &id,
            identity: &identity,
            strategy: "sequential",
            expected_head: None,
            commit_id: "synthetic-commit",
        },
    )
    .unwrap();
    tx.commit().unwrap();
    identity
}

#[test]
fn external_intent_prevents_blind_retries_and_blocks_replacement_until_settlement() {
    let (_dir, mut store, _) = open_fixture();
    select_external(&mut store);
    let identity = capture(&mut store, "job");
    let tx = store.connection.transaction().unwrap();
    external::begin_publication(&tx, "job").unwrap();
    external::publication_unknown(&tx, "job").unwrap();
    assert!(external::begin_publication(&tx, "job").is_err());
    assert!(selection::require_no_pending_publication(&tx).is_err());
    tx.commit().unwrap();
    edit(&mut store, 1);
    let tx = store.connection.transaction().unwrap();
    assert!(external::confirm_publication(
        &tx,
        "job",
        "wrong-commit",
        "snapshot",
        "observation",
        false
    )
    .is_err());
    external::confirm_publication(
        &tx,
        "job",
        "synthetic-commit",
        "snapshot",
        "observation",
        true,
    )
    .unwrap();
    tx.commit().unwrap();
    let base: String = store
        .connection
        .query_row("SELECT identity FROM external_storage_bases", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<selection::CaptureIdentity>(&base).unwrap(),
        identity
    );
    assert_eq!(store.revision().unwrap(), identity.revision + 1);
    assert_eq!(count(&store, "external_storage_capture_refs"), 1);
    let tx = store.connection.transaction().unwrap();
    assert!(external::begin_publication(&tx, "job").is_err());
    external::finish_history(&tx, "job").unwrap();
    tx.commit().unwrap();
    assert_eq!(count(&store, "external_storage_capture_refs"), 0);
}

#[test]
fn external_base_and_job_phase_commit_atomically() {
    let (dir, mut store, _) = open_fixture();
    select_external(&mut store);
    capture(&mut store, "job");
    let tx = store.connection.transaction().unwrap();
    external::begin_publication(&tx, "job").unwrap();
    tx.commit().unwrap();
    let tx = store.connection.transaction().unwrap();
    external::confirm_publication(
        &tx,
        "job",
        "synthetic-commit",
        "snapshot",
        "observation",
        false,
    )
    .unwrap();
    tx.rollback().unwrap();
    drop(store);
    let mut store = PersistentStore::open(dir.path()).unwrap();
    assert_eq!(count(&store, "external_storage_bases"), 0);
    assert!(selection::require_no_pending_publication(&store.connection).is_err());
    let tx = store.connection.transaction().unwrap();
    external::confirm_publication(
        &tx,
        "job",
        "synthetic-commit",
        "snapshot",
        "observation",
        false,
    )
    .unwrap();
    tx.commit().unwrap();
    drop(store);
    let store = PersistentStore::open(dir.path()).unwrap();
    assert_eq!(count(&store, "external_storage_bases"), 1);
    assert!(selection::require_no_pending_publication(&store.connection).is_ok());
}

#[test]
fn external_replacement_and_physical_copy_invalidate_old_identity() {
    let (_dir, mut store, _) = open_fixture();
    select_external(&mut store);
    let old = capture(&mut store, "job");
    let staging = stage_root(&mut store, "synthetic-replacement");
    store.replace_commit(&staging, Some(old.revision)).unwrap();
    let next = selection::identity(&store.connection).unwrap();
    assert_ne!(old.library_epoch, next.library_epoch);
    assert_ne!(old.generation, next.generation);
    assert!(
        selection::read(&store.connection)
            .unwrap()
            .decision_required
    );
    assert!(selection::require_publish(&store.connection, &old, "synthetic-connection").is_err());
    let tx = store.connection.transaction().unwrap();
    selection::restored_copy(&tx).unwrap();
    tx.commit().unwrap();
    let restored = selection::identity(&store.connection).unwrap();
    assert_ne!(restored.store_id, next.store_id);
    assert_ne!(restored.library_epoch, next.library_epoch);
    assert_ne!(restored.selection_epoch, next.selection_epoch);
}

#[test]
fn external_schema_rejects_missing_trigger_without_migration() {
    let (dir, store, _) = open_fixture();
    store
        .connection
        .execute_batch("DROP TRIGGER content_change_messages_update")
        .unwrap();
    drop(store);
    assert!(PersistentStore::open(dir.path()).is_err());
}

#[test]
fn external_backup_consumers_share_capture_but_cancel_and_device_identity_are_independent() {
    let (dir, mut store, _) = open_fixture();
    let identity = selection::identity(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    let first = external::register_capture(
        &tx,
        "capture-a",
        &identity,
        "library-device",
        "codec-1",
        "device-1",
        &"a".repeat(64),
        "consumer-a",
    )
    .unwrap();
    let shared = external::register_capture(
        &tx,
        "capture-b",
        &identity,
        "library-device",
        "codec-1",
        "device-1",
        &"a".repeat(64),
        "consumer-b",
    )
    .unwrap();
    assert_eq!(first, shared);
    let newer_device = external::register_capture(
        &tx,
        "capture-device-2",
        &identity,
        "library-device",
        "codec-1",
        "device-2",
        &"b".repeat(64),
        "consumer-c",
    )
    .unwrap();
    assert_ne!(first, newer_device);
    external::prepare_backup(
        &tx,
        "job-a",
        "destination-a",
        "repository-a",
        &first,
        "point-a",
    )
    .unwrap();
    external::prepare_backup(
        &tx,
        "job-b",
        "destination-b",
        "repository-b",
        &shared,
        "point-b",
    )
    .unwrap();
    external::cancel_prepared(&tx, "job-a").unwrap();
    assert!(external::capture_has_consumers(&tx, &shared).unwrap());
    tx.commit().unwrap();
    drop(store);
    let mut store = PersistentStore::open(dir.path()).unwrap();
    assert!(external::capture_has_consumers(&store.connection, &shared).unwrap());
    assert_eq!(
        selection::read(&store.connection).unwrap().target,
        selection::SyncTarget::None
    );
    let tx = store.connection.transaction().unwrap();
    external::cancel_prepared(&tx, "job-b").unwrap();
    assert!(!external::capture_has_consumers(&tx, &shared).unwrap());
    tx.commit().unwrap();
}
