//! Transactional pruning and pinned capture regressions with synthetic stores.
use super::*;
use crate::persistent_store::{content_capture::ContentCaptureSink, sync_selection, RootMutation};

fn cursor(store: &mut PersistentStore, consumer: &str, revision: i64) {
    let tx = store.connection.transaction().unwrap();
    changes::commit_cursor(&tx, consumer, &active_generation(&tx).unwrap(), revision).unwrap();
    tx.commit().unwrap();
}

fn floor(store: &PersistentStore) -> i64 {
    store.connection.query_row(
        "SELECT revision FROM content_change_floor WHERE singleton=1",
        [],
        |row| row.get(0),
    ).unwrap()
}

fn revisions(store: &PersistentStore) -> Vec<i64> {
    store.connection.prepare("SELECT revision FROM content_changes ORDER BY revision").unwrap()
        .query_map([], |row| row.get(0)).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap()
}

fn consumers(store: &PersistentStore) -> Vec<(String, i64, bool)> {
    store.connection.prepare(
        "SELECT id,revision,rebuild_required FROM content_change_consumers ORDER BY id",
    ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap()
}

fn count(store: &PersistentStore, table: &str) -> i64 {
    store.connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0)).unwrap()
}

fn edit_root(store: &mut PersistentStore, value: i64) {
    store.commit(&WorkingSetCommit {
        root_mutations: Some(vec![RootMutation::Set {
            key: "synthetic".into(),
            value: json!(value),
        }]),
        ..empty_working_set_commit(store.revision().unwrap())
    }).unwrap();
}

fn fill_changes(store: &mut PersistentStore, revision: i64) {
    while store.revision().unwrap() < revision {
        let next = store.revision().unwrap() + 1;
        store.commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: "synthetic-index".into(),
                key: format!("key-{next:04}"),
                value: json!(next),
            }]),
            ..empty_working_set_commit(next - 1)
        }).unwrap();
    }
}

fn prune(store: &mut PersistentStore) -> i64 {
    let tx = store.connection.transaction().unwrap();
    let cutoff = changes::prune(&tx).unwrap();
    tx.commit().unwrap();
    cutoff
}

#[test]
fn repeated_mutations_coalesce_without_pruning_each_write() {
    let (_directory, mut store, _) = open_fixture();
    let generation = active_generation(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    for revision in 2..=10_001 {
        changes::begin_mutation(&tx, &generation, revision, "local").unwrap();
        tx.execute("UPDATE root SET value=?2 WHERE generation=?1", params![generation, json!({"synthetic": revision}).to_string()]).unwrap();
        changes::finish_mutation(&tx).unwrap();
    }
    crate::persistent_store::commit::set_active(&tx, 10_001, &generation).unwrap();
    tx.commit().unwrap();
    assert_eq!(revisions(&store), vec![10_001]);
    assert_eq!(floor(&store), 1);
    assert_eq!(count(&store, "content_change_context"), 0);
}

#[test]
fn pruning_stops_at_the_slowest_valid_consumer() {
    let (_directory, mut store, _) = open_fixture();
    fill_changes(&mut store, 21);
    cursor(&mut store, "slow", 10);
    cursor(&mut store, "fast", 20);
    assert_eq!(prune(&mut store), 10);
    assert_eq!(floor(&store), 10);
    assert_eq!(revisions(&store), (11..=21).collect::<Vec<_>>());
    cursor(&mut store, "slow", 20);
    assert_eq!(prune(&mut store), 20);
    assert_eq!(revisions(&store), vec![21]);
}

#[test]
fn rebuild_consumers_do_not_hold_the_floor_and_new_consumers_rebuild() {
    let (_directory, mut store, _) = open_fixture();
    cursor(&mut store, "slow", 1);
    fill_changes(&mut store, 3);
    cursor(&mut store, "fast", 3);
    let tx = store.connection.transaction().unwrap();
    changes::require_rebuild(&tx, "slow").unwrap();
    assert_eq!(changes::prune(&tx).unwrap(), 3);
    tx.commit().unwrap();
    assert!(revisions(&store).is_empty());
    let lease = store.acquire_revision(3).unwrap().lease;
    let reader = store.revision_leases.get(&lease).unwrap();
    for consumer in ["slow", "new"] {
        assert_eq!(changes::window(reader, consumer).unwrap(), changes::ChangeWindow::Rebuild);
    }
    assert_eq!(changes::window(reader, "fast").unwrap(), changes::ChangeWindow::Incremental { after_revision: 3 });
    assert_eq!(changes::page(reader, 1, None, 128).unwrap_err().to_string(), "Content index rebuild required");
    store.release_revision(&lease).unwrap();
}

#[test]
fn pruning_marks_consumers_behind_the_floor_for_rebuild() {
    let (_directory, mut store, _) = open_fixture();
    fill_changes(&mut store, 3);
    cursor(&mut store, "stale", 1);
    cursor(&mut store, "current", 3);
    store.connection.execute("UPDATE content_change_floor SET revision=2 WHERE singleton=1", []).unwrap();
    assert_eq!(prune(&mut store), 3);
    assert_eq!(floor(&store), 3);
    assert_eq!(consumers(&store), vec![("current".into(), 3, false), ("stale".into(), 1, true)]);
    assert!(revisions(&store).is_empty());
}

#[test]
fn no_consumers_or_only_an_old_generation_allow_pruning_to_current() {
    let (_directory, mut store, database) = open_fixture();
    cursor(&mut store, "old", 1);
    let old_generation = active_generation(&store.connection).unwrap();
    let staging = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&staging, &staged_root(&database)).unwrap();
    store.replace_put_presets(&staging, &[]).unwrap();
    store.replace_commit(&staging, Some(1)).unwrap();
    assert_eq!(consumers(&store), vec![("old".into(), 1, true)]);
    assert_eq!(floor(&store), 2);
    assert_ne!(active_generation(&store.connection).unwrap(), old_generation);
    store.connection.execute("UPDATE content_change_consumers SET rebuild_required=0 WHERE id='old'", []).unwrap();
    fill_changes(&mut store, 3);
    assert_eq!(prune(&mut store), 3);
    assert!(revisions(&store).is_empty());
    let tx = store.connection.transaction().unwrap();
    changes::remove_connection_consumer(&tx, "old").unwrap();
    tx.commit().unwrap();
    fill_changes(&mut store, 4);
    assert_eq!(prune(&mut store), 4);
    assert!(consumers(&store).is_empty());
    assert!(revisions(&store).is_empty());
}

#[test]
fn current_revision_is_the_prune_sentinel_even_with_a_future_consumer() {
    let (_directory, mut store, _) = open_fixture();
    fill_changes(&mut store, 2);
    let tx = store.connection.transaction().unwrap();
    let generation = active_generation(&tx).unwrap();
    assert!(changes::commit_cursor(&tx, "future", &generation, 3).is_err());
    tx.execute("INSERT INTO content_change_consumers VALUES('future',?1,100,0)", [&generation]).unwrap();
    assert_eq!(changes::prune(&tx).unwrap(), 2);
    tx.commit().unwrap();
    assert_eq!(floor(&store), 2);
}

#[test]
fn inconsistent_floor_generation_or_revision_rejects_pruning() {
    for wrong_generation in [true, false] {
        let (_directory, mut store, _) = open_fixture();
        fill_changes(&mut store, 3);
        let before = revisions(&store);
        let tx = store.connection.transaction().unwrap();
        if wrong_generation {
            tx.execute("UPDATE content_change_floor SET generation='synthetic-other'", []).unwrap();
        } else {
            tx.execute("UPDATE content_change_floor SET revision=4", []).unwrap();
        }
        assert_eq!(changes::prune(&tx).unwrap_err().to_string(), "Content index floor is inconsistent");
        tx.rollback().unwrap();
        assert_eq!(floor(&store), 1);
        assert_eq!(revisions(&store), before);
    }
}

#[test]
fn projection_ack_prunes_only_after_success_and_preserves_other_consumers() {
    let (_directory, mut store, _) = open_fixture();
    store.commit_working_set_change_cursor(1).unwrap();
    cursor(&mut store, "backup", 1);
    fill_changes(&mut store, 3);
    let lease = store.acquire_revision(3).unwrap().lease;
    store.working_set_change_window(&lease).unwrap();
    store.working_set_change_page(&lease, 1, None, 1).unwrap();
    store.release_revision(&lease).unwrap();
    // A failed projection releases its reader without acknowledging anything.
    assert_eq!(floor(&store), 1);
    assert_eq!(consumers(&store), vec![("backup".into(), 1, false), (changes::WORKING_SET_CONSUMER.into(), 1, false)]);
    assert_eq!(revisions(&store), vec![2, 3]);
    store.commit_working_set_change_cursor(3).unwrap();
    assert_eq!(floor(&store), 1);
    cursor(&mut store, "backup", 3);
    store.commit_working_set_change_cursor(3).unwrap();
    assert_eq!(floor(&store), 3);
    assert!(revisions(&store).is_empty());
}

#[test]
fn floor_update_failure_rolls_back_deletion_and_the_projection_cursor() {
    let (_directory, mut store, _) = open_fixture();
    store.commit_working_set_change_cursor(1).unwrap();
    fill_changes(&mut store, 3);
    store.connection.execute_batch("CREATE TRIGGER synthetic_floor_failure BEFORE UPDATE ON content_change_floor BEGIN SELECT RAISE(ABORT,'synthetic floor failure'); END").unwrap();
    assert!(store.commit_working_set_change_cursor(3).is_err());
    assert_eq!(floor(&store), 1);
    assert_eq!(revisions(&store), vec![2, 3]);
    assert_eq!(consumers(&store), vec![(changes::WORKING_SET_CONSUMER.into(), 1, false)]);
    store.connection.execute_batch("DROP TRIGGER synthetic_floor_failure").unwrap();
    store.commit_working_set_change_cursor(3).unwrap();
    assert_eq!(floor(&store), 3);
    assert!(revisions(&store).is_empty());
}

#[test]
fn a_pinned_index_and_body_survive_pruning_but_the_old_cursor_is_rejected() {
    let (_directory, mut store, _) = open_fixture();
    store.commit_working_set_change_cursor(1).unwrap();
    edit_root(&mut store, 2);
    let lease = store.acquire_revision(2).unwrap().lease;
    edit_root(&mut store, 3);
    store.commit_working_set_change_cursor(3).unwrap();
    let window = store.working_set_change_window(&lease).unwrap();
    assert_eq!(window.after_revision, Some(1));
    assert_eq!(store.working_set_change_page(&lease, 1, None, 128).unwrap().len(), 1);
    assert_eq!(store.read_root(Some(&lease)).unwrap().value["synthetic"], json!(2));
    assert_eq!(store.commit_working_set_change_cursor(2).unwrap_err().to_string(), "Content index rebuild required");
    assert_eq!(floor(&store), 3);
    assert_eq!(consumers(&store), vec![(changes::WORKING_SET_CONSUMER.into(), 3, false)]);
    store.release_revision(&lease).unwrap();
}

#[test]
fn connection_consumer_removal_is_selective_transactional_and_cannot_remove_the_ui() {
    let (_directory, mut store, _) = open_fixture();
    fill_changes(&mut store, 3);
    cursor(&mut store, "removed", 1);
    cursor(&mut store, "retained", 2);
    cursor(&mut store, changes::WORKING_SET_CONSUMER, 3);
    let tx = store.connection.transaction().unwrap();
    assert!(changes::remove_connection_consumer(&tx, "").is_err());
    assert!(changes::remove_connection_consumer(&tx, changes::WORKING_SET_CONSUMER).is_err());
    changes::remove_connection_consumer(&tx, "removed").unwrap();
    tx.rollback().unwrap();
    assert_eq!(consumers(&store).len(), 3);
    assert_eq!(floor(&store), 1);
    let tx = store.connection.transaction().unwrap();
    changes::remove_connection_consumer(&tx, "removed").unwrap();
    tx.commit().unwrap();
    assert_eq!(consumers(&store), vec![("retained".into(), 2, false), (changes::WORKING_SET_CONSUMER.into(), 3, false)]);
    assert_eq!(floor(&store), 2);
    assert_eq!(revisions(&store), vec![3]);
}

#[test]
fn native_connection_removal_prunes_only_its_consumer() {
    let (_directory, mut store, _) = open_fixture();
    fill_changes(&mut store, 3);
    cursor(&mut store, "removed", 1);
    cursor(&mut store, "retained", 2);
    cursor(&mut store, changes::WORKING_SET_CONSUMER, 3);
    store.external_prepare_connection_removal("removed").unwrap();
    assert_eq!(consumers(&store), vec![("retained".into(), 2, false), (changes::WORKING_SET_CONSUMER.into(), 3, false)]);
    assert_eq!(floor(&store), 2);
    assert_eq!(revisions(&store), vec![3]);
}

#[test]
fn staging_is_invisible_and_keyset_pages_keep_their_order() {
    let (_directory, mut store, database) = open_fixture();
    fill_changes(&mut store, 5);
    cursor(&mut store, "backup", 1);
    let before = revisions(&store);
    let staging = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&staging, &staged_root(&database)).unwrap();
    store.replace_put_presets(&staging, database["botPresets"].as_array().unwrap()).unwrap();
    store.replace_add_characters(&staging, database["characters"].as_array().unwrap()).unwrap();
    assert_eq!(revisions(&store), before);
    assert_eq!(count(&store, "content_change_context"), 0);
    let lease = store.acquire_revision(5).unwrap().lease;
    let reader = store.revision_leases.get(&lease).unwrap();
    let first = changes::page(reader, 1, None, 2).unwrap();
    let second = changes::page(reader, 1, first.last(), 2).unwrap();
    assert_eq!(first.iter().chain(second.iter()).map(|key| key.key2.as_str()).collect::<Vec<_>>(), vec!["key-0002", "key-0003", "key-0004", "key-0005"]);
    assert!(changes::page(reader, 1, second.last(), 2).unwrap().is_empty());
    store.release_revision(&lease).unwrap();
}

struct Never;
impl crate::local_backup::CancellationProbe for Never {
    fn is_cancelled(&self) -> bool { false }
}

fn capture_fixture() -> (tempfile::TempDir, PersistentStore) {
    let (directory, store, _) = open_fixture();
    let authority = json!({"format": "v2", "migrationId": "synthetic-index", "compatibilityHash": "a".repeat(64)});
    store.connection.execute(
        "UPDATE asset_repository_authority SET value=?2 WHERE generation=?1",
        params![active_generation(&store.connection).unwrap(), authority.to_string()],
    ).unwrap();
    (directory, store)
}

fn catalog(directory: &Path, id: &str) -> crate::external_storage::capture::CaptureCatalog {
    crate::external_storage::capture::CaptureCatalog::create(
        &directory.join("external-storage").join(id),
        &directory.join("external-storage/objects"),
        None,
    ).unwrap()
}

#[test]
fn content_capture_rechecks_the_floor_after_projecting_its_pinned_body() {
    use crate::logical_records::{decode_logical_record, encode_logical_record_key, LogicalRecordEnvelope, LogicalRecordLocator};
    let (directory, mut store) = capture_fixture();
    edit_root(&mut store, 2);
    let prepared = store.prepare_content_capture("old", "backup", 2).unwrap();
    let mut old = catalog(directory.path(), "old");
    edit_root(&mut store, 3);
    store.commit_working_set_change_cursor(3).unwrap();
    assert!(prepared.project(&mut old, &Never).unwrap() > 1);
    let root_key = encode_logical_record_key(&LogicalRecordLocator::Root).unwrap();
    let root_hash: String = old.db.query_row("SELECT hash FROM records WHERE key=?1", [root_key], |row| row.get(0)).unwrap();
    let bytes = fs::read(directory.path().join("external-storage/objects").join(root_hash)).unwrap();
    let LogicalRecordEnvelope::Root { value, .. } = decode_logical_record(&bytes).unwrap() else { panic!("root record") };
    assert_eq!(value["synthetic"], json!(2));
    assert_eq!(prepared.register(&mut store, &old, &[1; 32], "logical-v1").unwrap_err().to_string(), "Content index rebuild required");
    assert_eq!(floor(&store), 3);
    assert_eq!(count(&store, "external_storage_captures"), 0);
    assert_eq!(consumers(&store), vec![(changes::WORKING_SET_CONSUMER.into(), 3, false)]);
    assert!(store.active_readers.detached_asset_roots().is_ok());
    let prepared = store.prepare_content_capture("new", "backup", 3).unwrap();
    let mut fresh = catalog(directory.path(), "new");
    assert!(prepared.project(&mut fresh, &Never).unwrap() > 1);
    prepared.register(&mut store, &fresh, &[1; 32], "logical-v1").unwrap();
    assert_eq!(count(&store, "external_storage_captures"), 1);
    assert_eq!(floor(&store), 3);
}

#[test]
fn content_capture_registration_prunes_atomically_without_acknowledging_the_server() {
    let (directory, mut store) = capture_fixture();
    store.connection.execute("INSERT INTO server_sync_state(singleton,config,full_scan) VALUES(1,'{}',0)", []).unwrap();
    edit_root(&mut store, 2);
    let outbox_before: i64 = store.connection.query_row("SELECT revision FROM server_sync_dirty WHERE kind='root'", [], |row| row.get(0)).unwrap();
    let prepared = store.prepare_content_capture("capture", "backup", 2).unwrap();
    let mut capture = catalog(directory.path(), "capture");
    prepared.project(&mut capture, &Never).unwrap();
    assert_eq!(floor(&store), 1);
    assert!(consumers(&store).is_empty());
    store.connection.execute_batch("CREATE TRIGGER synthetic_floor_failure BEFORE UPDATE ON content_change_floor BEGIN SELECT RAISE(ABORT,'synthetic floor failure'); END").unwrap();
    assert!(prepared.register(&mut store, &capture, &[1; 32], "logical-v1").is_err());
    assert_eq!(floor(&store), 1);
    assert_eq!(revisions(&store), vec![2]);
    assert!(consumers(&store).is_empty());
    for table in ["external_storage_captures", "external_storage_capture_files"] {
        assert_eq!(count(&store, table), 0);
    }
    assert!(store.active_readers.detached_asset_roots().is_ok());
    store.connection.execute_batch("DROP TRIGGER synthetic_floor_failure").unwrap();
    let prepared = store.prepare_content_capture("retry", "backup", 2).unwrap();
    prepared.register(&mut store, &capture, &[1; 32], "logical-v1").unwrap();
    assert_eq!(floor(&store), 2);
    assert!(revisions(&store).is_empty());
    let outbox_after: i64 = store.connection.query_row("SELECT revision FROM server_sync_dirty WHERE kind='root'", [], |row| row.get(0)).unwrap();
    assert_eq!(outbox_after, outbox_before);
    assert_eq!(count(&store, "server_sync_dirty"), 1);
}

#[test]
fn content_capture_shared_catalog_ack_prunes_the_new_consumers_window() {
    let (_directory, mut store) = capture_fixture();
    cursor(&mut store, "slow", 1);
    edit_root(&mut store, 2);
    let hydration = store.hydrate_external_capture_dependencies("first", &Never).unwrap();
    let first = store.capture_external_library("first", &hydration, &Never).unwrap();
    assert!(!first.shared);
    assert_eq!(floor(&store), 1);
    let hydration = store.hydrate_external_capture_dependencies("slow", &Never).unwrap();
    let shared = store.capture_external_library("slow", &hydration, &Never).unwrap();
    assert!(shared.shared);
    assert_eq!(shared.id, first.id);
    assert_eq!(shared.projected_records, 0);
    assert_eq!(floor(&store), 2);
    assert!(revisions(&store).is_empty());
}

#[test]
fn content_capture_drop_releases_the_read_guard_without_an_abandon_call() {
    let (directory, mut store) = capture_fixture();
    assert!(store.prepare_content_capture("", "backup", 1).is_err());
    assert!(store.prepare_content_capture("capture", "", 1).is_err());
    let prepared = store.prepare_content_capture("capture", "backup", 1).unwrap();
    assert!(store.active_readers.detached_asset_roots().is_err());
    let mut capture = catalog(directory.path(), "capture");
    prepared.project(&mut capture, &Never).unwrap();
    assert!(capture.remove_record("root").is_err());
    drop(prepared);
    assert!(store.active_readers.detached_asset_roots().is_ok());
    assert!(consumers(&store).is_empty());
    assert_eq!(count(&store, "external_storage_captures"), 0);
    assert_eq!(floor(&store), 1);
}

#[test]
fn restoring_a_copy_keeps_identity_invalidation_without_capture_reservations() {
    let (_directory, mut store, _) = open_fixture();
    cursor(&mut store, "backup", 1);
    let old = sync_selection::identity(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    sync_selection::restored_copy(&tx).unwrap();
    tx.commit().unwrap();
    let current = sync_selection::identity(&store.connection).unwrap();
    assert_ne!(current.store_id, old.store_id);
    assert_ne!(current.library_epoch, old.library_epoch);
    assert_eq!(consumers(&store), vec![("backup".into(), 1, true)]);
    assert_eq!(count(&store, "content_change_context"), 0);
}
