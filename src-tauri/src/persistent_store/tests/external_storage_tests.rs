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
        "INSERT INTO plugin_storage VALUES(?1,'synthetic-plugin','synthetic',1,0,'1',NULL,NULL,NULL)",
        [&generation],
    )
    .unwrap();
    tx.execute(
        "DELETE FROM plugin_storage WHERE storage_key='synthetic'",
        [],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO plugin_storage VALUES(?1,'synthetic-plugin','synthetic',1,0,'2',NULL,NULL,NULL)",
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
fn external_floor_respects_valid_consumers_without_reservations() {
    let (_dir, mut store, _) = open_fixture();
    cursor(&mut store, "slow", 1);
    edit(&mut store, 1);
    cursor(&mut store, "fast", 2);
    let tx = store.connection.transaction().unwrap();
    assert_eq!(changes::prune(&tx).unwrap(), 1);
    changes::require_rebuild(&tx, "slow").unwrap();
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
    external::prepare_backup(&tx, job, "synthetic-connection", "synthetic-repository", &id, "synthetic-point").unwrap();
    tx.commit().unwrap();
    identity
}



#[test]
fn set_paused_preserves_selection_epoch_and_rejects_stale_selection() {
    let (_dir, mut store, _) = open_fixture();
    let selected = store.external_selection().unwrap();
    let selected = store.external_select(&selected.epoch, &selection::SyncTarget::External("repository".into())).unwrap();
    let paused = store.external_set_paused(&selected.epoch, true).unwrap();
    assert_eq!(paused.epoch, selected.epoch);
    assert!(paused.paused);
    let resumed = store.external_set_paused(&paused.epoch, false).unwrap();
    assert_eq!(resumed.epoch, selected.epoch);
    assert!(!resumed.paused);
    assert!(store.external_set_paused("stale-selection", true).is_err());
    assert!(!store.external_selection().unwrap().paused);
}


#[test]
fn external_replacement_keeps_library_identity_and_invalidates_old_captures() {
    let (_dir, mut store, _) = open_fixture();
    select_external(&mut store);
    let old = capture(&mut store, "job");
    let selected = selection::read(&store.connection).unwrap();
    let clock = store.lww_clock_state().unwrap();
    let staging = stage_root(&mut store, "synthetic-replacement");
    store.replace_commit(&staging, Some(old.revision)).unwrap();
    let next = selection::identity(&store.connection).unwrap();
    assert_eq!(old.library_epoch, next.library_epoch);
    assert_eq!(old.store_id, next.store_id);
    assert_eq!(old.selection_epoch, next.selection_epoch);
    assert_ne!(old.generation, next.generation);
    assert_eq!(old.revision + 1, next.revision);
    let current_selection = selection::read(&store.connection).unwrap();
    assert_eq!(current_selection.target, selected.target);
    assert_eq!(current_selection.epoch, selected.epoch);
    let current_clock = store.lww_clock_state().unwrap();
    assert_eq!(current_clock.writer_id, clock.writer_id);
    assert_eq!(current_clock.binding_authority, clock.binding_authority);
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

fn capture_fixture() -> (tempfile::TempDir, PersistentStore, Value) {
    let (directory, store, database) = open_fixture();
    (directory, store, database)
}

fn capture_library(
    store: &mut PersistentStore,
    consumer: &str,
    probe: &dyn crate::local_backup::CancellationProbe,
) -> super::super::StoreResult<super::super::external_capture::CapturedSnapshot> {
    let hydration = store.hydrate_external_capture_dependencies(consumer, probe)?;
    store.capture_external_library(consumer, &hydration, probe)
}

#[test]
fn external_capture_carries_archived_character_state_and_payload_dependencies() {
    use crate::asset_repository::PayloadCas;
    use crate::logical_records::{
        decode_logical_record, encode_logical_record_key, LogicalRecordEnvelope,
        LogicalRecordLocator,
    };
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    let (directory, mut store, database) = capture_fixture();
    let character_id = database["characters"][0]["chaId"]
        .as_str()
        .expect("fixture character ID");
    let generation = active_generation(&store.connection).unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let asset = cas.prepare_bytes(b"archived character asset").unwrap();
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext, metadata
             ) VALUES (?1, 'archive-asset', ?2, 'asset', ?3, 'image/png', 'archive', 'png', '{}')",
            params![generation, asset.content_hash, asset.byte_size as i64],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE characters SET image='archive-asset', detail=json_set(detail,'$.image','archive-asset')
             WHERE generation=?1 AND character_id=?2",
            params![generation, character_id],
        )
        .unwrap();
    let revision = store.revision().unwrap();
    store
        .archive_character(character_id, revision, 10)
        .expect("archive fixture character");
    let archived = super::super::archive::read_archived_object(
        &store.connection,
        &generation,
        character_id,
    )
    .unwrap()
    .expect("archived metadata");

    let capture = capture_library(&mut store, "archive-export", &Never).unwrap();
    let key = encode_logical_record_key(&LogicalRecordLocator::Character {
        character_id: character_id.to_owned(),
    })
    .unwrap();
    let record_hash: String = capture
        .catalog
        .db
        .query_row("SELECT hash FROM records WHERE key=?1", [&key], |row| row.get(0))
        .unwrap();
    let bytes = crate::external_storage::content_store::ContentStore::open(
        &directory.path().join("external-storage"),
    )
    .unwrap()
    .read_all(&record_hash)
    .unwrap();
    assert!(matches!(
        decode_logical_record(&bytes).unwrap(),
        LogicalRecordEnvelope::ArchivedCharacter { .. }
    ));
    let dependencies = capture
        .catalog
        .db
        .prepare("SELECT hash FROM dependencies WHERE record=?1 ORDER BY hash")
        .unwrap()
        .query_map([&key], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(dependencies.contains(&archived.object_hash));
    assert!(dependencies.contains(&asset.content_hash));
}

#[test]
fn external_capture_manager_shares_consumers_and_reuses_their_durable_cursor() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (_directory, mut store, _) = capture_fixture();
    let first = capture_library(&mut store, "destination-a", &Never).unwrap();
    assert!(first.projected_records > 1);
    assert!(!first.shared);
    store
        .retain_external_capture(&first.id, "destination-a")
        .unwrap();
    let second = capture_library(&mut store, "destination-b", &Never).unwrap();
    assert!(second.shared);
    assert_eq!(second.projected_records, 0);
    assert_eq!(first.id, second.id);
    store
        .retain_external_capture(&second.id, "destination-b")
        .unwrap();
    assert!(!store
        .release_external_capture(&first.id, "destination-a")
        .unwrap());
    assert!(external::capture_has_consumers(&store.connection, &first.id).unwrap());
    edit(&mut store, 2);
    let next = capture_library(&mut store, "destination-b", &Never).unwrap();
    assert_eq!(next.projected_records, 1);
    assert!(!next.catalog.rebuilt);
    assert_ne!(next.id, first.id);
    assert!(store
        .release_external_capture(&first.id, "destination-b")
        .unwrap());
    assert_eq!(count(&store, "content_change_consumers"), 2);
    assert!(store.active_readers.detached_asset_roots().is_ok());
}

#[test]
fn external_capture_rebuilds_an_unreferenced_missing_cache_and_collects_old_metadata() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (_directory, mut store, _) = capture_fixture();
    let first = capture_library(&mut store, "destination", &Never).unwrap();
    let first_id = first.id.clone();
    let (_, first_path, _) = first.catalog.manifest().unwrap();
    let first_path = first_path.to_owned();
    drop(first);
    fs::remove_file(first_path).unwrap();

    let rebuilt = capture_library(&mut store, "destination", &Never).unwrap();
    assert!(!rebuilt.shared);
    assert_ne!(rebuilt.id, first_id);
    assert_eq!(count(&store, "external_storage_captures"), 1);
    assert_eq!(count(&store, "external_storage_capture_files"), 1);
}

#[test]
fn external_capture_rebuilds_an_unreferenced_corrupt_cache() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (_directory, mut store, _) = capture_fixture();
    let first = capture_library(&mut store, "destination", &Never).unwrap();
    let first_id = first.id.clone();
    let (_, path, _) = first.catalog.manifest().unwrap();
    let path = path.to_owned();
    drop(first);
    fs::write(path, b"synthetic-corrupt-capture-cache").unwrap();

    let rebuilt = capture_library(&mut store, "destination", &Never).unwrap();
    assert!(!rebuilt.shared);
    assert_ne!(rebuilt.id, first_id);
    assert_eq!(count(&store, "external_storage_captures"), 1);
}

#[test]
fn external_capture_never_discards_a_missing_cache_owned_by_a_pending_job() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (_directory, mut store, _) = capture_fixture();
    let first = capture_library(&mut store, "destination", &Never).unwrap();
    let first_id = first.id.clone();
    let (_, path, _) = first.catalog.manifest().unwrap();
    let path = path.to_owned();
    let tx = store.connection.transaction().unwrap();
    external::prepare_backup(
        &tx,
        "pending-backup",
        "destination",
        "repository",
        &first_id,
        "history-point",
    )
    .unwrap();
    tx.commit().unwrap();
    drop(first);
    fs::remove_file(path).unwrap();

    let error = match capture_library(&mut store, "other-destination", &Never) {
        Ok(_) => panic!("referenced damaged cache was reused"),
        Err(error) => error.to_string(),
    };
    assert_eq!(error, "Referenced capture cache is unavailable");
    assert_eq!(count(&store, "external_storage_captures"), 1);
    assert_eq!(count(&store, "external_storage_capture_refs"), 1);
    let phase: String = store
        .connection
        .query_row(
            "SELECT phase FROM external_storage_jobs WHERE id='pending-backup'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(phase, "ready");
}

#[test]
fn external_capture_reopens_a_pinned_old_revision_and_gc_keeps_only_needed_cache() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (_directory, mut store, _) = capture_fixture();
    let first = capture_library(&mut store, "destination", &Never).unwrap();
    let first_id = first.id.clone();
    let first_revision = first.identity.revision;
    drop(first);
    store
        .retain_external_capture(&first_id, "pending-owner")
        .unwrap();
    edit(&mut store, 2);
    let second = capture_library(&mut store, "destination", &Never).unwrap();
    assert_ne!(second.id, first_id);
    assert_eq!(count(&store, "external_storage_captures"), 2);
    let reopened = store.reopen_external_capture(&first_id).unwrap();
    assert_eq!(reopened.identity.revision, first_revision);
    drop(reopened);
    drop(second);

    store
        .release_external_capture(&first_id, "pending-owner")
        .unwrap();
    edit(&mut store, 3);
    let third = capture_library(&mut store, "destination", &Never).unwrap();
    assert_eq!(count(&store, "external_storage_captures"), 1);
    assert_eq!(count(&store, "external_storage_capture_files"), 1);
    assert!(store.reopen_external_capture(&third.id).is_err());
}

#[test]
fn external_capture_text_delta_does_not_hydrate_unchanged_remote_only_payloads() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let payload = cas.prepare_bytes(b"unchanged-remote-only-payload").unwrap();
    store
        .commit_asset_alias(
            &AssetAlias {
                key: "assets/remote-only.bin".into(),
                object_hash: Some(payload.content_hash.clone()),
                kind: "asset".into(),
                size: payload.byte_size as i64,
                mime: "application/octet-stream".into(),
                name: "remote-only".into(),
                ext: "bin".into(),
                inlay_type: None,
                width: None,
                height: None,
                metadata: json!({}),
            },
            1,
        )
        .unwrap();
    let first = capture_library(&mut store, "destination", &Never).unwrap();
    drop(first);
    crate::server_sync::residency::Residency::open(directory.path()).unwrap();
    fs::remove_file(
        cas.object_path(&payload.content_hash)
            .unwrap()
            .expect("payload path"),
    )
    .unwrap();
    let revision = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: "synthetic-plugin".to_owned(),
                key: "synthetic-text-setting".into(),
                value: json!("edited"),
            }]),
            ..empty_working_set_commit(revision)
        })
        .unwrap();

    let delta = capture_library(&mut store, "destination", &Never).unwrap();
    assert_eq!(delta.projected_records, 1);
    assert!(!delta.catalog.rebuilt);
    assert!(cas.stat_object(&payload.content_hash).unwrap().is_none());
}

/// An asset alias whose payload body this device no longer holds.
fn released_payload_alias(
    store: &mut PersistentStore,
    root: &std::path::Path,
    body: &[u8],
) -> String {
    let cas = crate::asset_repository::PayloadCas::new(root).unwrap();
    let payload = cas.prepare_bytes(body).unwrap();
    store
        .commit_asset_alias(
            &AssetAlias {
                key: "assets/custody-only.bin".into(),
                object_hash: Some(payload.content_hash.clone()),
                kind: "asset".into(),
                size: payload.byte_size as i64,
                mime: "application/octet-stream".into(),
                name: "custody-only".into(),
                ext: "bin".into(),
                inlay_type: None,
                width: None,
                height: None,
                metadata: json!({}),
            },
            1,
        )
        .unwrap();
    fs::remove_file(
        cas.object_path(&payload.content_hash)
            .unwrap()
            .expect("payload path"),
    )
    .unwrap();
    payload.content_hash
}

#[test]
fn external_capture_sizes_a_custody_only_payload_without_fetching_it() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let body = b"synthetic custody-only payload";
    let hash = released_payload_alias(&mut store, directory.path(), body);
    assert!(capture_library(&mut store, "destination", &Never).is_err());

    crate::server_sync::residency::test_remote::hold(
        directory.path(),
        &[(&hash, body.len() as u64)],
    );
    let capture = capture_library(&mut store, "destination", &Never).unwrap();
    let bytes: i64 = capture
        .catalog
        .db
        .query_row("SELECT bytes FROM dependencies WHERE hash=?1", [&hash], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(bytes, body.len() as i64);
    assert_eq!(
        crate::server_sync::residency::test_remote::fetched(directory.path()),
        0
    );
    assert!(crate::asset_repository::PayloadCas::new(directory.path())
        .unwrap()
        .stat_object(&hash)
        .unwrap()
        .is_none());
}

#[test]
fn a_transfer_capture_serves_local_recovery_only_once_it_is_completed() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let root = directory.path();
    let body = b"synthetic custody-only payload";
    let hash = released_payload_alias(&mut store, root, body);
    crate::server_sync::residency::test_remote::hold(root, &[(&hash, body.len() as u64)]);
    crate::server_sync::residency::test_remote::serve(root, &hash, body.to_vec());
    let capture = capture_library(&mut store, "destination", &Never).unwrap();
    // The publishing job owns it, as it does while a conflict is preserved.
    store.retain_external_capture(&capture.id, "job").unwrap();
    let reference = capture.durable_reference(root).unwrap();

    assert!(crate::external_storage::capture::validate_recovery_sources([&reference], root, &Never)
        .is_err());
    assert!(
        crate::external_storage::snapshot_export::prepare_local_conflict_snapshot(
            root,
            "format-repository",
            &reference,
        )
        .is_err()
    );
    assert_eq!(crate::server_sync::residency::test_remote::fetched(root), 0);

    store.complete_external_capture(&capture.id, &Never).unwrap();
    assert_eq!(crate::server_sync::residency::test_remote::fetched(root), 1);
    crate::external_storage::capture::validate_recovery_sources([&reference], root, &Never).unwrap();
    crate::external_storage::snapshot_export::prepare_local_conflict_snapshot(
        root,
        "format-repository",
        &reference,
    )
    .unwrap();
    // A completed capture holds everything, so completing it again fetches nothing.
    store.complete_external_capture(&capture.id, &Never).unwrap();
    assert_eq!(crate::server_sync::residency::test_remote::fetched(root), 1);
    // The conflict store is the external root's own, not one nested in it.
    let external = root.join("external-storage");
    assert!(external.join("content.sqlite").is_file());
    assert!(!external.join("content-files").join("content.sqlite").exists());
    assert!(!external.join("content-files").join("content-files").exists());
}

/// Flips one byte of a held payload, keeping its length.
fn corrupt_payload_in_place(root: &std::path::Path, hash: &str) -> Vec<u8> {
    let path = crate::asset_repository::PayloadCas::new(root)
        .unwrap()
        .object_path(hash)
        .unwrap()
        .expect("held payload");
    let original = fs::read(&path).unwrap();
    let mut changed = original.clone();
    changed[0] ^= 0xff;
    fs::write(&path, &changed).unwrap();
    original
}

fn recovery_accepted(
    store: &mut PersistentStore,
    root: &std::path::Path,
    capture: &super::super::external_capture::CapturedSnapshot,
    probe: &dyn crate::local_backup::CancellationProbe,
) -> [bool; 3] {
    let reference = capture.durable_reference(root).unwrap();
    [
        store.complete_external_capture(&capture.id, probe).is_ok(),
        crate::external_storage::capture::validate_recovery_sources([&reference], root, probe).is_ok(),
        crate::external_storage::snapshot_export::prepare_local_conflict_snapshot(
            root,
            "format-repository",
            &reference,
        )
        .is_ok(),
    ]
}

#[test]
fn a_completed_recovery_capture_rejects_a_payload_changed_at_its_length() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let root = directory.path();
    let body = b"synthetic custody-only payload";
    let hash = released_payload_alias(&mut store, root, body);
    crate::server_sync::residency::test_remote::hold(root, &[(&hash, body.len() as u64)]);
    crate::server_sync::residency::test_remote::serve(root, &hash, body.to_vec());
    let capture = capture_library(&mut store, "destination", &Never).unwrap();
    store.retain_external_capture(&capture.id, "job").unwrap();
    assert_eq!(recovery_accepted(&mut store, root, &capture, &Never), [true; 3]);

    let original = corrupt_payload_in_place(root, &hash);
    assert_eq!(recovery_accepted(&mut store, root, &capture, &Never), [false; 3]);
    // The differing body is not silently replaced from custody.
    assert_eq!(crate::server_sync::residency::test_remote::fetched(root), 1);

    let path = crate::asset_repository::PayloadCas::new(root)
        .unwrap()
        .object_path(&hash)
        .unwrap()
        .unwrap();
    fs::write(path, original).unwrap();
    assert_eq!(recovery_accepted(&mut store, root, &capture, &Never), [true; 3]);
}

#[test]
fn completing_a_capture_rejects_a_held_payload_that_is_not_its_name() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let root = directory.path();
    let cas = crate::asset_repository::PayloadCas::new(root).unwrap();
    let payload = cas.prepare_bytes(b"synthetic held payload").unwrap();
    store
        .commit_asset_alias(
            &AssetAlias {
                key: "assets/held.bin".into(),
                object_hash: Some(payload.content_hash.clone()),
                kind: "asset".into(),
                size: payload.byte_size as i64,
                mime: "application/octet-stream".into(),
                name: "held".into(),
                ext: "bin".into(),
                inlay_type: None,
                width: None,
                height: None,
                metadata: json!({}),
            },
            1,
        )
        .unwrap();
    let capture = capture_library(&mut store, "destination", &Never).unwrap();
    store.retain_external_capture(&capture.id, "job").unwrap();
    corrupt_payload_in_place(root, &payload.content_hash);

    assert_eq!(recovery_accepted(&mut store, root, &capture, &Never), [false; 3]);
    assert_eq!(crate::server_sync::residency::test_remote::fetched(root), 0);
}

#[test]
fn completing_a_capture_fails_for_a_payload_custody_cannot_serve() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let root = directory.path();
    let body = b"synthetic custody-only payload";
    let hash = released_payload_alias(&mut store, root, body);
    crate::server_sync::residency::test_remote::hold(root, &[(&hash, body.len() as u64)]);
    let capture = capture_library(&mut store, "destination", &Never).unwrap();
    store.retain_external_capture(&capture.id, "job").unwrap();

    assert_eq!(recovery_accepted(&mut store, root, &capture, &Never), [false; 3]);
}

#[test]
fn external_capture_projects_only_changed_records_at_the_pinned_snapshot() {
    use super::super::content_capture::ContentCaptureSink;
    use crate::external_storage::capture::CaptureCatalog;
    use crate::logical_records::{decode_logical_record, LogicalRecordEnvelope};
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let external = directory.path().join("external-storage");
    let mut full = CaptureCatalog::create(
        &directory.path().join("external-storage/full"),
        &external,
        None,
    )
    .unwrap();
    let prepared = store
        .prepare_content_capture("full", "consumer", 1)
        .unwrap();
    assert!(store.active_readers.detached_asset_roots().is_err());
    assert!(prepared.project(&mut full, &Never).unwrap() > 1);
    assert!(full.remove_record("root").is_err());
    prepared
        .register(&mut store, &full, &[1; 32], "logical-v1")
        .unwrap();
    let records: i64 = full.db.query_row("SELECT count(*) FROM records", [], |row| row.get(0)).unwrap();
    assert!(records > 1);
    assert_eq!(count(&store, "external_storage_capture_files"), 1);
    assert!(store.active_readers.detached_asset_roots().is_ok());
    let (digest, path, _) = full.manifest().unwrap();
    let mut delta = CaptureCatalog::create(
        &directory.path().join("external-storage/delta"),
        &external,
        Some((path, &digest)),
    )
    .unwrap();
    edit(&mut store, 2);
    let prepared = store
        .prepare_content_capture("delta", "consumer", 2)
        .unwrap();
    edit(&mut store, 3);
    assert_eq!(prepared.project(&mut delta, &Never).unwrap(), 1);
    assert!(!delta.rebuilt);
    let changed: i64 = delta
        .db
        .query_row("SELECT count(*) FROM delta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(changed, 1);
    let keys: Vec<(String, String)> = delta
        .db
        .prepare("SELECT key,hash FROM records ORDER BY key")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut saw_root = false;
    let content =
        crate::external_storage::content_store::ContentStore::open(&external).unwrap();
    for (_, hash) in keys {
        let mut body = content.open_body(&hash).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut body, &mut bytes).unwrap();
        if let LogicalRecordEnvelope::Root { value, .. } = decode_logical_record(&bytes).unwrap() {
            assert_eq!(value["synthetic"], json!(2));
            saw_root = true;
        }
    }
    assert!(saw_root);
    prepared
        .register(&mut store, &delta, &[1; 32], "logical-v1")
        .unwrap();
    assert!(store.active_readers.detached_asset_roots().is_ok());
    assert_eq!(count(&store, "external_storage_capture_files"), 2);
    let lease = store.acquire_revision(3).unwrap().lease;
    assert_eq!(
        changes::page(store.revision_leases.get(&lease).unwrap(), 2, None, 128)
            .unwrap()
            .len(),
        1
    );
    store.release_revision(&lease).unwrap();
}

#[test]
fn external_capture_rejects_mismatched_cache_and_releases_gc_guard_on_cancel() {
    use crate::external_storage::capture::CaptureCatalog;
    struct Cancel(bool);
    impl crate::local_backup::CancellationProbe for Cancel {
        fn is_cancelled(&self) -> bool {
            self.0
        }
    }
    let (directory, mut store, _) = capture_fixture();
    cursor(&mut store, "consumer", 1);
    let mut empty = CaptureCatalog::create(
        &directory.path().join("empty"),
        &directory.path().join("objects"),
        None,
    )
    .unwrap();
    let prepared = store
        .prepare_content_capture("bad-cache", "consumer", 1)
        .unwrap();
    assert!(prepared
        .project(&mut empty, &Cancel(false))
        .unwrap_err()
        .to_string()
        .contains("Capture cache requires rebuild"));
    assert_eq!(
        prepared
            .project(&mut empty, &Cancel(true))
            .unwrap_err()
            .to_string(),
        "External capture cancelled"
    );
    assert!(empty.manifest().is_err());
    drop(prepared);
    assert!(store.active_readers.detached_asset_roots().is_ok());
    assert_eq!(count(&store, "external_storage_captures"), 0);
    assert_eq!(count(&store, "content_change_consumers"), 1);
}

#[test]
fn external_capture_registration_failure_does_not_advance_capture_or_cursor() {
    use crate::external_storage::capture::CaptureCatalog;
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let mut catalog = CaptureCatalog::create(
        &directory.path().join("external-storage/job"),
        &directory.path().join("external-storage"),
        None,
    )
    .unwrap();
    let prepared = store
        .prepare_content_capture("register-failure", "consumer", 1)
        .unwrap();
    prepared.project(&mut catalog, &Never).unwrap();
    store.connection.execute_batch("CREATE TRIGGER synthetic_capture_failure BEFORE INSERT ON external_storage_capture_files BEGIN SELECT RAISE(ABORT,'synthetic'); END").unwrap();
    assert!(prepared
        .register(&mut store, &catalog, &[1; 32], "logical-v1")
        .is_err());
    for table in [
        "external_storage_captures",
        "content_change_consumers",
        "external_storage_capture_files",
    ] {
        assert_eq!(count(&store, table), 0);
    }
    let floor: i64 = store.connection.query_row("SELECT revision FROM content_change_floor WHERE singleton=1", [], |row| row.get(0)).unwrap();
    assert_eq!(floor, 1);
    assert!(store.active_readers.detached_asset_roots().is_ok());
    store
        .connection
        .execute_batch("DROP TRIGGER synthetic_capture_failure")
        .unwrap();
    drop(store);
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let prepared = store
        .prepare_content_capture("retry", "consumer", 1)
        .unwrap();
    prepared
        .register(&mut store, &catalog, &[1; 32], "logical-v1")
        .unwrap();
    assert_eq!(count(&store, "external_storage_captures"), 1);
    assert_eq!(count(&store, "external_storage_capture_files"), 1);
    assert!(store.active_readers.detached_asset_roots().is_ok());
}

#[test]
fn external_capture_keeps_deleted_source_payload_pinned_after_reopen() {
    use crate::external_storage::capture::{registered_roots, CaptureCatalog};
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let payload = cas
        .prepare_bytes(b"synthetic-external-capture-asset")
        .unwrap();
    let alias = AssetAlias {
        key: "assets/synthetic.bin".into(),
        object_hash: Some(payload.content_hash.clone()),
        kind: "asset".into(),
        size: payload.byte_size as i64,
        mime: "application/octet-stream".into(),
        name: "synthetic".into(),
        ext: "bin".into(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    store.commit_asset_alias(&alias, 1).unwrap();
    let mut catalog = CaptureCatalog::create(
        &directory.path().join("external-storage/job"),
        &directory.path().join("external-storage"),
        None,
    )
    .unwrap();
    let prepared = store
        .prepare_content_capture("asset-capture", "consumer", 2)
        .unwrap();
    prepared.project(&mut catalog, &Never).unwrap();
    prepared
        .register(&mut store, &catalog, &[1; 32], "logical-v1")
        .unwrap();
    store.delete_asset_alias("asset", &alias.key, 2).unwrap();
    drop(store);
    let store = PersistentStore::open(directory.path()).unwrap();
    let roots = registered_roots(&store.connection, directory.path()).unwrap();
    assert!(roots.object_hashes.contains(&payload.content_hash));
    // A damaged registered catalog must stop GC instead of dropping its pins.
    let (_, path, _) = catalog.manifest().unwrap();
    let path = path.to_path_buf();
    drop(catalog);
    fs::write(path, b"synthetic-corrupt-catalog").unwrap();
    assert!(registered_roots(&store.connection, directory.path()).is_err());
}

#[test]
fn external_backup_consumers_share_capture_but_cancel_and_device_identity_are_independent() {
    let (dir, mut store, _) = capture_fixture();
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


/// A publication settled without its capture still has to be settled. The rows
/// are dropped rather than left describing the snapshot before this one.


/// The head moved without its content moving, so the same records describe it
/// and only what they are bound to follows the base.


/// Says "cancelled" from its `at`th question on and counts the questions it
/// answered that way.
struct CancelAt {
    at: usize,
    asked: std::cell::Cell<usize>,
    refused: std::cell::Cell<usize>,
}

impl CancelAt {
    fn new(at: usize) -> Self {
        Self {
            at,
            asked: std::cell::Cell::new(0),
            refused: std::cell::Cell::new(0),
        }
    }
}

impl crate::local_backup::CancellationProbe for CancelAt {
    fn is_cancelled(&self) -> bool {
        self.asked.set(self.asked.get() + 1);
        let cancelled = self.asked.get() >= self.at;
        if cancelled {
            self.refused.set(self.refused.get() + 1);
        }
        cancelled
    }
}

/// Recovery validation stops at whichever question first finds it cancelled,
/// between payloads, between records and inside one long payload, and the
/// capture it was checking stays owned and completes afterwards.
#[test]
fn cancelling_recovery_validation_stops_at_the_next_object_or_chunk() {
    let (directory, mut store, _) = capture_fixture();
    let root = directory.path();
    let cas = crate::asset_repository::PayloadCas::new(root).unwrap();
    let bodies = [vec![3u8; 300 * 1024], b"second payload".to_vec(), b"third payload".to_vec()];
    for (index, body) in bodies.iter().enumerate() {
        let payload = cas.prepare_bytes(body).unwrap();
        let revision = store.revision().unwrap();
        store
            .commit_asset_alias(
                &AssetAlias {
                    key: format!("assets/held-{index}.bin"),
                    object_hash: Some(payload.content_hash.clone()),
                    kind: "asset".into(),
                    size: payload.byte_size as i64,
                    mime: "application/octet-stream".into(),
                    name: format!("held-{index}"),
                    ext: "bin".into(),
                    inlay_type: None,
                    width: None,
                    height: None,
                    metadata: json!({}),
                },
                revision,
            )
            .unwrap();
    }
    let never = CancelAt::new(usize::MAX);
    let capture = capture_library(&mut store, "destination", &never).unwrap();
    store.retain_external_capture(&capture.id, "job").unwrap();
    let reference = capture.durable_reference(root).unwrap();

    let counted = CancelAt::new(usize::MAX);
    crate::external_storage::capture::validate_recovery_sources([&reference], root, &counted)
        .unwrap();
    let questions = counted.asked.get();
    // Three payloads, the long one in five reads, and the capture's records.
    assert!(questions > 3 + 5, "{questions}");
    for at in 1..=questions {
        let probe = CancelAt::new(at);
        assert!(
            crate::external_storage::capture::validate_recovery_sources([&reference], root, &probe)
                .is_err(),
            "{at}"
        );
        assert_eq!((probe.asked.get(), probe.refused.get()), (at, 1), "{at}");
    }

    let probe = CancelAt::new(questions / 2);
    assert!(store.complete_external_capture(&capture.id, &probe).is_err());
    assert_eq!(probe.refused.get(), 1);
    store.complete_external_capture(&capture.id, &never).unwrap();
    crate::external_storage::capture::validate_recovery_sources([&reference], root, &never)
        .unwrap();
}
