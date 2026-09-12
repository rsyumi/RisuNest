use super::*;

#[test]
fn fresh_schema_adds_only_the_global_empty_asset_object_catalog() {
    let directory = tempfile::tempdir().expect("create asset catalog schema directory");
    let store = PersistentStore::open(directory.path()).expect("open fresh store");
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read current version"),
        2
    );
    assert_eq!(
        table_columns(&store.connection, "asset_objects")
            .into_iter()
            .map(|column| column.0)
            .collect::<Vec<_>>(),
        ["object_hash", "byte_size", "created_at_ms"]
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT COUNT(*) FROM asset_objects", [], |row| row
                .get::<_, i64>(0))
            .expect("count fresh catalog"),
        0
    );
    assert!(super::GENERATION_TABLES
        .iter()
        .all(|(table, _)| *table != "asset_objects"));
    assert_eq!(
        store
            .connection
            .query_row("SELECT COUNT(*) FROM cold_payload_authority", [], |row| row
                .get::<_, i64>(0))
            .expect("count cold authority rows"),
        1
    );
}

#[test]
fn fresh_schema_adds_exact_durable_asset_deletion_tombstones() {
    let directory = tempfile::tempdir().expect("create deletion tombstone schema directory");
    let store = PersistentStore::open(directory.path()).expect("open current store");

    assert_eq!(
        table_columns(&store.connection, "asset_object_deletions")
            .into_iter()
            .map(|column| column.0)
            .collect::<Vec<_>>(),
        [
            "object_hash",
            "byte_size",
            "physical_key",
            "state",
            "created_at_ms"
        ]
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT COUNT(*) FROM asset_object_deletions", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count fresh deletion tombstones"),
        0
    );
    assert!(super::GENERATION_TABLES
        .iter()
        .all(|(table, _)| *table != "asset_object_deletions"));
}

#[test]
fn fresh_schema_adds_generation_scoped_exact_replacement_alias_provenance() {
    let directory = tempfile::tempdir().expect("create replacement provenance schema directory");
    let store = PersistentStore::open(directory.path()).expect("open current store");

    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        table_columns(&store.connection, "asset_alias_replacement_candidates")
            .into_iter()
            .map(|column| column.0)
            .collect::<Vec<_>>(),
        vec![
            "generation".to_owned(),
            "kind".to_owned(),
            "logical_key".to_owned(),
            "object_hash".to_owned(),
            "byte_size".to_owned(),
        ]
    );
    assert!(super::GENERATION_TABLES
        .iter()
        .any(|(table, _)| *table == "asset_alias_replacement_candidates"));
}

#[test]
fn fresh_schema_adds_one_validated_asset_gc_maintenance_cursor_row() {
    let directory = tempfile::tempdir().expect("create asset GC maintenance schema directory");
    let store = PersistentStore::open(directory.path()).expect("open current store");

    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        table_columns(&store.connection, "asset_gc_maintenance_state")
            .into_iter()
            .map(|column| column.0)
            .collect::<Vec<_>>(),
        vec!["singleton".to_owned(), "catalog_cursor".to_owned()]
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT singleton, catalog_cursor FROM asset_gc_maintenance_state",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .unwrap(),
        (1, None)
    );
    assert!(super::GENERATION_TABLES
        .iter()
        .all(|(table, _)| *table != "asset_gc_maintenance_state"));
}

#[test]
fn asset_object_catalog_is_idempotent_conflict_safe_and_stably_paged() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create catalog directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let first = AssetObjectRegistration {
        object_hash: "11".repeat(32),
        byte_size: 11,
    };
    let second = AssetObjectRegistration {
        object_hash: "22".repeat(32),
        byte_size: 22,
    };
    let third = AssetObjectRegistration {
        object_hash: "33".repeat(32),
        byte_size: 33,
    };
    store
        .asset_object_catalog()
        .register(&[first.clone()], 5)
        .expect("register first object");
    store
        .asset_object_catalog()
        .register(&[first.clone()], 99)
        .expect("re-register same object");
    store
        .asset_object_catalog()
        .register(&[second.clone(), third.clone()], 5)
        .expect("register remaining objects");
    assert!(store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: first.object_hash.clone(),
                byte_size: 12,
            }],
            10,
        )
        .is_err());
    assert!(store
        .asset_object_catalog()
        .register(&[first.clone()], -1)
        .is_err());
    assert!(store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: "not-a-hash".to_owned(),
                byte_size: 1,
            }],
            10,
        )
        .is_err());

    let first_page = store
        .query_asset_object_catalog(2, None)
        .expect("query first page");
    assert_eq!(
        first_page
            .items
            .iter()
            .map(|item| (&item.object_hash, item.created_at_ms))
            .collect::<Vec<_>>(),
        [(&first.object_hash, 5), (&second.object_hash, 5)]
    );
    let second_page = store
        .query_asset_object_catalog(2, first_page.next_cursor.as_deref())
        .expect("query second page");
    assert_eq!(second_page.items.len(), 1);
    assert_eq!(second_page.items[0].object_hash, third.object_hash);
    assert!(second_page.next_cursor.is_none());
    assert!(store.query_asset_object_catalog(0, None).is_err());
    assert!(store
        .query_asset_object_catalog(1, Some("not-an-opaque-cursor"))
        .is_err());
}

#[test]
fn asset_object_catalog_revives_a_recreated_deleted_hash_transactionally() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create catalog revival directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let registration = AssetObjectRegistration {
        object_hash: "44".repeat(32),
        byte_size: 44,
    };
    store
        .asset_object_catalog()
        .register(&[registration.clone()], 5)
        .expect("register original object");
    let physical_key = format!(
        "assets-v2/objects/{}/{}",
        &registration.object_hash[..2],
        &registration.object_hash[2..]
    );
    store
        .connection
        .execute(
            "INSERT INTO asset_object_deletions (
                object_hash, byte_size, physical_key, state, created_at_ms
             ) VALUES (?1, ?2, ?3, 'unlinked', 6)",
            rusqlite::params![
                registration.object_hash,
                registration.byte_size as i64,
                physical_key
            ],
        )
        .expect("seed completed deletion tombstone");

    store
        .asset_object_catalog()
        .register(&[registration.clone()], 99)
        .expect("revive recreated object");

    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT byte_size, created_at_ms FROM asset_objects WHERE object_hash = ?1",
                [&registration.object_hash],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap(),
        (44, 99)
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM asset_object_deletions WHERE object_hash = ?1",
                [&registration.object_hash],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
}

#[test]
fn snapshot_restore_without_newer_catalog_rows_leaves_objects_untracked() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create catalog restore directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let snapshot = store
        .snapshot_create("before-catalog-registration")
        .expect("create empty-catalog snapshot");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas
        .prepare_bytes(b"survives without restored inventory")
        .expect("prepare untracked object");
    store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: prepared.content_hash.clone(),
                byte_size: prepared.byte_size,
            }],
            1,
        )
        .expect("register post-snapshot object");
    store
        .snapshot_restore_request(&snapshot.id)
        .expect("request empty-catalog restore");
    drop(store);

    let restored = PersistentStore::open(directory.path()).expect("restore catalog snapshot");
    assert!(restored
        .query_asset_object_catalog(16, None)
        .expect("query restored catalog")
        .items
        .is_empty());
    assert_eq!(
        cas.stat_object(&prepared.content_hash)
            .expect("stat surviving object"),
        Some(prepared.byte_size)
    );
}

#[test]
fn asset_gc_catalog_retains_future_rows_and_aborts_on_size_mismatch() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create conservative catalog directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas
        .prepare_bytes(b"future catalog object")
        .expect("prepare future object");
    store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: prepared.content_hash.clone(),
                byte_size: prepared.byte_size,
            }],
            200,
        )
        .expect("register future catalog object");

    let future = store
        .asset_gc_dry_run(16, None, 100, 10)
        .expect("classify future catalog row");
    assert_eq!(
        future.report.grace_retained_hashes,
        vec![prepared.content_hash.clone()]
    );
    assert!(!future.report.deletion_enabled);

    store
        .connection
        .execute(
            "UPDATE asset_objects SET byte_size = byte_size + 1 WHERE object_hash = ?1",
            [&prepared.content_hash],
        )
        .expect("corrupt catalog size fixture");
    assert!(store.asset_gc_dry_run(16, None, 300, 10).is_err());
}

fn register_gc_candidate(
    store: &mut PersistentStore,
    prepared: &crate::asset_repository::PreparedPayload,
) {
    use super::asset_object_catalog::AssetObjectRegistration;

    store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: prepared.content_hash.clone(),
                byte_size: prepared.byte_size,
            }],
            0,
        )
        .expect("register GC candidate");
}

#[test]
fn asset_gc_delete_page_recollects_a_late_root_under_writer_exclusion() {
    use crate::asset_repository::migration_gc::AssetGcDeleteHookPoint;

    let directory = tempfile::tempdir().expect("create late-root directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas.prepare_bytes(b"late rooted object").unwrap();
    register_gc_candidate(&mut store, &prepared);
    let database_path = directory.path().join("persistent/persistent.db");
    let generation = super::active_generation(&store.connection).unwrap();
    let mut injected = false;

    let page = store
        .asset_gc_delete_page_with_hook(16, None, 100, 10, |point| {
            if point == AssetGcDeleteHookPoint::AfterInitialScan && !injected {
                let connection = Connection::open(&database_path).unwrap();
                connection
                    .execute(
                        "INSERT INTO asset_aliases (
                            generation, logical_key, object_hash, kind, size, mime, name, ext,
                            inlay_type, width, height, metadata
                         ) VALUES (?1, 'assets/late.bin', ?2, 'asset', ?3,
                            'application/octet-stream', 'late', 'bin', NULL, NULL, NULL, '{}')",
                        rusqlite::params![
                            generation,
                            prepared.content_hash,
                            prepared.byte_size as i64
                        ],
                    )
                    .unwrap();
                injected = true;
            }
            Ok(())
        })
        .expect("late root must conservatively retain the object");

    assert!(injected);
    assert!(page.report.deletion_enabled);
    assert!(page.report.marked_hashes.contains(&prepared.content_hash));
    assert!(page.report.deleted_hashes.is_empty());
    assert_eq!(
        cas.stat_object(&prepared.content_hash).unwrap(),
        Some(prepared.byte_size)
    );
}

#[test]
fn asset_gc_delete_page_recollects_a_late_durable_pin_under_writer_exclusion() {
    use crate::asset_repository::{
        job_pins::{CasJobKind, CasObjectRole, DurableCasJob},
        migration_gc::AssetGcDeleteHookPoint,
    };

    let directory = tempfile::tempdir().expect("create late-pin directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas.prepare_bytes(b"late pinned object").unwrap();
    register_gc_candidate(&mut store, &prepared);
    let mut late_job = None;

    let page = store
        .asset_gc_delete_page_with_hook(16, None, 100, 10, |point| {
            if point == AssetGcDeleteHookPoint::AfterInitialScan && late_job.is_none() {
                let mut job = DurableCasJob::begin(
                    directory.path(),
                    "late-gc-pin",
                    CasJobKind::DirectAssetOrInlayWrite,
                    1,
                )
                .unwrap();
                job.pin_existing(
                    &cas,
                    &prepared.content_hash,
                    prepared.byte_size,
                    CasObjectRole::DirectObject,
                )
                .unwrap();
                late_job = Some(job);
            }
            Ok(())
        })
        .expect("late pin must conservatively retain the object");

    assert!(late_job.is_some());
    assert!(page.report.marked_hashes.contains(&prepared.content_hash));
    assert!(page
        .report
        .blockers
        .contains(&"job-pin-unsealed:late-gc-pin".to_owned()));
    assert!(page.report.deleted_hashes.is_empty());
    assert_eq!(
        cas.stat_object(&prepared.content_hash).unwrap(),
        Some(prepared.byte_size)
    );
}

#[test]
fn asset_gc_delete_page_refuses_unsealed_jobs_and_unready_migrations() {
    use crate::asset_repository::{
        job_pins::{CasJobKind, DurableCasJob},
        migration_gc::StagedAssetMigration,
    };

    let directory = tempfile::tempdir().expect("create blocker directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas.prepare_bytes(b"blocked deletion candidate").unwrap();
    register_gc_candidate(&mut store, &prepared);
    let _job = DurableCasJob::begin(
        directory.path(),
        "gc-blocker-job",
        CasJobKind::DirectAssetOrInlayWrite,
        1,
    )
    .unwrap();
    let _migration =
        StagedAssetMigration::begin(directory.path(), "gc-blocker-migration", 0).unwrap();
    let generation = super::active_generation(&store.connection).expect("read generation");
    store
        .connection
        .execute(
            "INSERT INTO plugin_storage (generation, storage_key, byte_size, ordinal, value)
             VALUES (?1, 'gc-blocker-plugin', 2, 0, '{}')",
            [generation],
        )
        .unwrap();

    let page = store
        .asset_gc_delete_page(16, None, 100, 10)
        .expect("documented blockers retain data");

    assert!(page
        .report
        .blockers
        .contains(&"job-pin-unsealed:gc-blocker-job".to_owned()));
    assert!(page
        .report
        .blockers
        .contains(&"staged-migration-unready:gc-blocker-migration".to_owned()));
    assert!(page
        .report
        .blockers
        .contains(&"plugin-storage-opaque".to_owned()));
    assert!(!page.report.deletion_enabled);
    assert!(page.report.deleted_hashes.is_empty());
    assert_eq!(
        cas.stat_object(&prepared.content_hash).unwrap(),
        Some(prepared.byte_size)
    );
}

#[test]
fn asset_gc_delete_page_rejects_same_size_corruption_and_keeps_recovery_evidence() {
    let directory = tempfile::tempdir().expect("create corrupted candidate directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas.prepare_bytes(b"original candidate bytes").unwrap();
    register_gc_candidate(&mut store, &prepared);
    let object_path = directory.path().join(&prepared.physical_key);
    fs::write(&object_path, b"corrupt! candidate bytes").unwrap();
    assert_eq!(
        fs::metadata(&object_path).unwrap().len(),
        prepared.byte_size
    );

    let error = store
        .asset_gc_delete_page(16, None, 100, 10)
        .expect_err("same-size corrupt object must not be deleted");

    assert!(error.to_string().contains("corruption"));
    assert_eq!(fs::read(&object_path).unwrap(), b"corrupt! candidate bytes");
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM asset_objects WHERE object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT state FROM asset_object_deletions WHERE object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "pending"
    );
}

#[test]
fn asset_gc_pending_tombstone_recovery_cancels_a_crash_before_unlink() {
    use crate::asset_repository::migration_gc::AssetGcDeleteHookPoint;

    let directory = tempfile::tempdir().expect("create pre-unlink crash directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas.prepare_bytes(b"survive pre-unlink crash").unwrap();
    register_gc_candidate(&mut store, &prepared);

    let error = store
        .asset_gc_delete_page_with_hook(16, None, 100, 10, |point| {
            if point == AssetGcDeleteHookPoint::AfterTombstone {
                return Err(StoreError::Store {
                    message: "injected crash before unlink".to_owned(),
                });
            }
            Ok(())
        })
        .expect_err("injected pre-unlink crash");
    assert!(error.to_string().contains("injected crash"));
    assert_eq!(
        cas.stat_object(&prepared.content_hash).unwrap(),
        Some(prepared.byte_size)
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT state FROM asset_object_deletions WHERE object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "pending"
    );
    drop(store);

    let reopened = PersistentStore::open(directory.path()).expect("recover pending deletion");
    assert_eq!(
        cas.stat_object(&prepared.content_hash).unwrap(),
        Some(prepared.byte_size)
    );
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT COUNT(*) FROM asset_objects WHERE object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT COUNT(*) FROM asset_object_deletions WHERE object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
}

#[test]
fn asset_gc_pending_tombstone_recovery_finishes_a_crash_after_unlink_idempotently() {
    use crate::asset_repository::migration_gc::AssetGcDeleteHookPoint;

    let directory = tempfile::tempdir().expect("create post-unlink crash directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas.prepare_bytes(b"finish post-unlink crash").unwrap();
    register_gc_candidate(&mut store, &prepared);

    store
        .asset_gc_delete_page_with_hook(16, None, 100, 10, |point| {
            if point == AssetGcDeleteHookPoint::AfterUnlink {
                return Err(StoreError::Store {
                    message: "injected crash after unlink".to_owned(),
                });
            }
            Ok(())
        })
        .expect_err("injected post-unlink crash");
    assert_eq!(cas.stat_object(&prepared.content_hash).unwrap(), None);
    drop(store);

    let first = PersistentStore::open(directory.path()).expect("finish missing pending object");
    assert_eq!(
        first
            .connection
            .query_row(
                "SELECT COUNT(*) FROM asset_objects WHERE object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(
        first
            .connection
            .query_row(
                "SELECT state FROM asset_object_deletions WHERE object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "unlinked"
    );
    drop(first);
    let second = PersistentStore::open(directory.path()).expect("confirm absent unlinked object");
    assert_eq!(
        second
            .connection
            .query_row(
                "SELECT COUNT(*) FROM asset_object_deletions WHERE object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    drop(second);
    PersistentStore::open(directory.path()).expect("recovery remains idempotent");
}

#[test]
fn asset_gc_startup_recovery_processes_only_one_bounded_tombstone_page() {
    use super::asset_object_catalog::ASSET_OBJECT_CATALOG_MAX_PAGE;

    let directory = tempfile::tempdir().expect("create bounded recovery directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let transaction = store.connection.transaction().unwrap();
    for index in 0..=ASSET_OBJECT_CATALOG_MAX_PAGE {
        let object_hash = format!("{index:064x}");
        let physical_key = crate::asset_repository::object_physical_key(&object_hash);
        transaction
            .execute(
                "INSERT INTO asset_object_deletions (
                    object_hash, byte_size, physical_key, state, created_at_ms
                 ) VALUES (?1, 0, ?2, 'unlinked', ?3)",
                rusqlite::params![object_hash, physical_key, index],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(store);

    let first = PersistentStore::open(directory.path()).expect("recover one bounded page");
    assert_eq!(
        first
            .connection
            .query_row("SELECT COUNT(*) FROM asset_object_deletions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        1
    );
    drop(first);

    let second = PersistentStore::open(directory.path()).expect("recover the remaining page");
    assert_eq!(
        second
            .connection
            .query_row("SELECT COUNT(*) FROM asset_object_deletions", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
}

#[test]
fn asset_gc_delete_page_is_bounded_and_never_enumerates_untracked_cas_entries() {
    let directory = tempfile::tempdir().expect("create bounded deletion directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let first = cas.prepare_bytes(b"bounded candidate one").unwrap();
    let second = cas.prepare_bytes(b"bounded candidate two").unwrap();
    register_gc_candidate(&mut store, &first);
    register_gc_candidate(&mut store, &second);
    let sentinel = directory
        .path()
        .join("assets-v2/objects/aa/not-a-catalog-object");
    fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
    fs::write(&sentinel, b"untracked sentinel").unwrap();

    let first_page = store.asset_gc_delete_page(1, None, 100, 10).unwrap();

    assert!(first_page.report.deletion_enabled);
    assert_eq!(first_page.report.deleted_hashes.len(), 1);
    assert_eq!(
        first_page.report.deleted_bytes,
        if first_page.report.deleted_hashes[0] == first.content_hash {
            first.byte_size
        } else {
            second.byte_size
        }
    );
    assert!(first_page.next_cursor.is_some());
    assert_eq!(fs::read(&sentinel).unwrap(), b"untracked sentinel");
    assert_eq!(
        store
            .query_asset_object_catalog(16, None)
            .unwrap()
            .items
            .len(),
        1
    );

    let second_page = store
        .asset_gc_delete_page(1, first_page.next_cursor.as_deref(), 100, 10)
        .unwrap();
    assert!(second_page.report.deletion_enabled);
    assert_eq!(second_page.report.deleted_hashes.len(), 1);
    assert!(store
        .query_asset_object_catalog(16, None)
        .unwrap()
        .items
        .is_empty());
    assert_eq!(fs::read(&sentinel).unwrap(), b"untracked sentinel");
}

#[test]
fn asset_gc_product_maintenance_reports_a_blocker_free_empty_executor() {
    let directory = tempfile::tempdir().expect("create empty maintenance directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");

    let page = store
        .asset_gc_product_maintenance_page(100)
        .expect("run empty product maintenance page");

    assert!(page.report.deletion_enabled);
    assert!(page.report.blockers.is_empty());
    assert!(page.report.deleted_hashes.is_empty());
    assert!(page.next_cursor.is_none());
}

#[test]
fn asset_gc_product_maintenance_is_bounded_and_repeatable() {
    let directory = tempfile::tempdir().expect("create bounded maintenance directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let total = super::ASSET_GC_PRODUCT_PAGE_LIMIT + 1;
    for index in 0..total {
        let prepared = cas
            .prepare_bytes(format!("bounded-maintenance-{index}").as_bytes())
            .expect("prepare maintenance candidate");
        register_gc_candidate(&mut store, &prepared);
    }

    let now_ms = super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS + 1;
    let first = store
        .asset_gc_product_maintenance_page(now_ms)
        .expect("run first product maintenance page");
    assert!(first.report.deletion_enabled);
    assert_eq!(
        first.report.deleted_hashes.len(),
        super::ASSET_GC_PRODUCT_PAGE_LIMIT as usize
    );
    assert!(first.next_cursor.is_some());
    let first_cursor = first.next_cursor.clone();
    drop(store);

    let mut store = PersistentStore::open(directory.path()).expect("reopen bounded maintenance");
    let second = store
        .asset_gc_product_maintenance_page(now_ms)
        .expect("repeat product maintenance page");
    assert!(second.report.deletion_enabled);
    assert_eq!(second.report.deleted_hashes.len(), 1);
    assert!(second.next_cursor.is_none());
    assert!(
        first_cursor.is_some(),
        "deleted cursor anchors remain safe keyset boundaries"
    );

    let third = store
        .asset_gc_product_maintenance_page(now_ms)
        .expect("repeat completed product maintenance");
    assert!(third.report.deletion_enabled);
    assert!(third.report.deleted_hashes.is_empty());
}

#[test]
fn asset_gc_product_maintenance_persists_progress_past_a_rooted_first_page() {
    let directory = tempfile::tempdir().expect("create durable progress directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let total = super::ASSET_GC_PRODUCT_PAGE_LIMIT + 1;
    for index in 0..total {
        let prepared = cas
            .prepare_bytes(format!("durable-progress-{index}").as_bytes())
            .expect("prepare maintenance candidate");
        register_gc_candidate(&mut store, &prepared);
    }
    let first_page_hashes = store
        .query_asset_object_catalog(super::ASSET_GC_PRODUCT_PAGE_LIMIT, None)
        .expect("query first catalog page")
        .items
        .into_iter()
        .map(|candidate| candidate.object_hash)
        .collect::<Vec<_>>();
    let generation = super::active_generation(&store.connection).expect("read active generation");
    let transaction = store
        .connection
        .transaction()
        .expect("begin root transaction");
    for (index, object_hash) in first_page_hashes.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO asset_aliases (
                    generation, logical_key, object_hash, kind, size, mime, name, ext,
                    inlay_type, width, height, metadata
                 ) VALUES (?1, ?2, ?3, 'asset', 1, 'application/octet-stream', ?2, 'bin',
                           NULL, NULL, NULL, '{}')",
                rusqlite::params![generation, format!("rooted-{index}"), object_hash],
            )
            .expect("root first-page object");
    }
    transaction.commit().expect("commit first-page roots");

    let now_ms = super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS + 1;
    let first = store
        .asset_gc_product_maintenance_page(now_ms)
        .expect("run rooted first page");
    assert!(first.report.deletion_enabled);
    assert!(first.report.deleted_hashes.is_empty());
    assert!(first.next_cursor.is_some());
    let persisted_cursor: Option<String> = store
        .connection
        .query_row(
            "SELECT catalog_cursor FROM asset_gc_maintenance_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("read persisted cursor");
    assert_eq!(persisted_cursor, first.next_cursor);
    drop(store);

    let mut reopened = PersistentStore::open(directory.path()).expect("reopen persistent store");
    let second = reopened
        .asset_gc_product_maintenance_page(now_ms)
        .expect("run eligible second page");
    assert!(second.report.deletion_enabled);
    assert_eq!(second.report.deleted_hashes.len(), 1);
    assert!(second.next_cursor.is_none());
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT catalog_cursor FROM asset_gc_maintenance_state WHERE singleton = 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap(),
        None
    );

    let wrapped = reopened
        .asset_gc_product_maintenance_page(now_ms)
        .expect("wrap to rooted first page");
    assert!(wrapped.report.deletion_enabled);
    assert!(wrapped.report.deleted_hashes.is_empty());
}

#[test]
fn asset_gc_product_maintenance_resets_a_malformed_cursor_fail_closed() {
    let directory = tempfile::tempdir().expect("create malformed cursor directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas
        .prepare_bytes(b"malformed-cursor-candidate")
        .expect("prepare maintenance candidate");
    register_gc_candidate(&mut store, &prepared);
    store
        .connection
        .execute(
            "UPDATE asset_gc_maintenance_state SET catalog_cursor = 'not-a-cursor'",
            [],
        )
        .expect("corrupt maintenance cursor");

    let reset = store
        .asset_gc_product_maintenance_page(super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS + 1)
        .expect("reset malformed cursor");
    assert!(!reset.report.deletion_enabled);
    assert_eq!(reset.report.blockers, ["asset-gc-cursor-invalid"]);
    assert!(reset.report.deleted_hashes.is_empty());
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT catalog_cursor FROM asset_gc_maintenance_state WHERE singleton = 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap(),
        None
    );
    assert!(cas.stat_object(&prepared.content_hash).unwrap().is_some());

    let retried = store
        .asset_gc_product_maintenance_page(super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS + 1)
        .expect("retry after cursor reset");
    assert_eq!(retried.report.deleted_hashes, [prepared.content_hash]);
}

#[test]
fn asset_gc_product_maintenance_resets_a_stale_ordering_cursor_fail_closed() {
    let directory = tempfile::tempdir().expect("create stale cursor directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let first = cas.prepare_bytes(b"stale-cursor-first").unwrap();
    let second = cas.prepare_bytes(b"stale-cursor-second").unwrap();
    use super::asset_object_catalog::AssetObjectRegistration;
    store
        .asset_object_catalog()
        .register(
            &[
                AssetObjectRegistration {
                    object_hash: first.content_hash.clone(),
                    byte_size: first.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: second.content_hash.clone(),
                    byte_size: second.byte_size,
                },
            ],
            0,
        )
        .unwrap();
    let catalog_page = store.query_asset_object_catalog(1, None).unwrap();
    let cursor = catalog_page.next_cursor.expect("create ordering cursor");
    let successor = store
        .query_asset_object_catalog(1, Some(&cursor))
        .unwrap()
        .items
        .into_iter()
        .next()
        .expect("read cursor successor");
    store
        .connection
        .execute(
            "DELETE FROM asset_objects WHERE object_hash = ?1",
            [&successor.object_hash],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE asset_gc_maintenance_state SET catalog_cursor = ?1",
            [&cursor],
        )
        .unwrap();

    let reset = store
        .asset_gc_product_maintenance_page(super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS + 1)
        .expect("reset stale cursor");
    assert!(!reset.report.deletion_enabled);
    assert_eq!(reset.report.blockers, ["asset-gc-cursor-stale"]);
    assert!(reset.report.deleted_hashes.is_empty());
    assert!(cas
        .stat_object(&catalog_page.items[0].object_hash)
        .unwrap()
        .is_some());
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT catalog_cursor FROM asset_gc_maintenance_state WHERE singleton = 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap(),
        None
    );
}

#[test]
fn asset_gc_product_maintenance_interruption_recovers_on_next_open() {
    use crate::asset_repository::migration_gc::AssetGcDeleteHookPoint;

    let directory = tempfile::tempdir().expect("create interrupted maintenance directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas
        .prepare_bytes(b"interrupted-product-maintenance")
        .expect("prepare interrupted candidate");
    register_gc_candidate(&mut store, &prepared);
    let now_ms = super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS + 1;

    let error = store
        .asset_gc_product_maintenance_page_with_hook(now_ms, |point| {
            if point == AssetGcDeleteHookPoint::AfterUnlink {
                return Err(StoreError::Store {
                    message: "injected product maintenance interruption".to_owned(),
                });
            }
            Ok(())
        })
        .expect_err("interrupt after product unlink");
    assert!(error
        .to_string()
        .contains("injected product maintenance interruption"));
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT catalog_cursor FROM asset_gc_maintenance_state WHERE singleton = 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap(),
        None,
        "an interrupted page must not commit progress"
    );
    drop(store);

    let mut reopened = PersistentStore::open(directory.path()).expect("recover product tombstone");
    let page = reopened
        .asset_gc_product_maintenance_page(now_ms)
        .expect("repeat recovered product maintenance");
    assert!(page.report.deletion_enabled);
    assert!(page.report.deleted_hashes.is_empty());
    drop(reopened);

    let mut reopened = PersistentStore::open(directory.path()).expect("confirm recovery absence");
    let page = reopened
        .asset_gc_product_maintenance_page(now_ms)
        .expect("repeat idempotent product maintenance");
    assert!(page.report.deletion_enabled);
    assert!(page.report.deleted_hashes.is_empty());
    assert!(cas
        .stat_object(&prepared.content_hash)
        .expect("stat interrupted candidate")
        .is_none());
}

#[test]
fn asset_gc_product_maintenance_keeps_blockers_fail_closed() {
    use crate::asset_repository::job_pins::{CasJobKind, DurableCasJob};

    let directory = tempfile::tempdir().expect("create blocked maintenance directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas
        .prepare_bytes(b"blocked-product-maintenance")
        .expect("prepare blocked candidate");
    register_gc_candidate(&mut store, &prepared);
    let _job = DurableCasJob::begin(
        directory.path(),
        "product-maintenance-blocker",
        CasJobKind::DirectAssetOrInlayWrite,
        0,
    )
    .expect("begin unsealed blocker");

    let page = store
        .asset_gc_product_maintenance_page(100)
        .expect("run blocked product maintenance");

    assert!(!page.report.deletion_enabled);
    assert!(!page.report.blockers.is_empty());
    assert!(page.report.deleted_hashes.is_empty());
    assert!(cas
        .stat_object(&prepared.content_hash)
        .expect("stat blocked candidate")
        .is_some());
}

#[test]
fn asset_gc_product_maintenance_advances_blocked_pages_and_revisits_after_wrap() {
    use crate::asset_repository::job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob};

    let directory = tempfile::tempdir().expect("create blocked cycle directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let total = super::ASSET_GC_PRODUCT_PAGE_LIMIT + 1;
    for index in 0..total {
        let prepared = cas
            .prepare_bytes(format!("blocked-cycle-{index}").as_bytes())
            .expect("prepare blocked candidate");
        register_gc_candidate(&mut store, &prepared);
    }
    let mut job = DurableCasJob::begin(
        directory.path(),
        "blocked-maintenance-cycle",
        CasJobKind::DirectAssetOrInlayWrite,
        0,
    )
    .expect("begin maintenance blocker");

    let first = store
        .asset_gc_product_maintenance_page(super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS + 1)
        .expect("run blocked first page");
    assert!(!first.report.deletion_enabled);
    assert!(!first.report.blockers.is_empty());
    assert!(first.next_cursor.is_some());

    let second = store
        .asset_gc_product_maintenance_page(super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS + 1)
        .expect("run blocked second page");
    assert!(!second.report.deletion_enabled);
    assert!(!second.report.blockers.is_empty());
    assert!(second.report.deleted_hashes.is_empty());
    assert!(second.next_cursor.is_none());
    job.release(CasReleaseOutcome::Aborted)
        .expect("release maintenance blocker");

    let revisited = store
        .asset_gc_product_maintenance_page(super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS + 1)
        .expect("revisit first page after wrap");
    assert!(revisited.report.deletion_enabled);
    assert_eq!(
        revisited.report.deleted_hashes.len(),
        super::ASSET_GC_PRODUCT_PAGE_LIMIT as usize
    );
}

#[test]
fn asset_gc_recovery_rejects_a_noncanonical_tombstone_path_without_data_loss() {
    let directory = tempfile::tempdir().expect("create tombstone mismatch directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas.prepare_bytes(b"retain path mismatch").unwrap();
    register_gc_candidate(&mut store, &prepared);
    let wrong_hash = "55".repeat(32);
    let wrong_key = format!(
        "assets-v2/objects/{}/{}",
        &wrong_hash[..2],
        &wrong_hash[2..]
    );
    store
        .connection
        .execute(
            "INSERT INTO asset_object_deletions (
                object_hash, byte_size, physical_key, state, created_at_ms
             ) VALUES (?1, ?2, ?3, 'pending', 1)",
            rusqlite::params![prepared.content_hash, prepared.byte_size as i64, wrong_key],
        )
        .unwrap();
    drop(store);

    let error = match PersistentStore::open(directory.path()) {
        Ok(_) => panic!("noncanonical recovery target must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("physical key"));
    assert_eq!(
        cas.stat_object(&prepared.content_hash).unwrap(),
        Some(prepared.byte_size)
    );
    let connection = Connection::open(directory.path().join("persistent/persistent.db")).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM asset_objects WHERE object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
}

#[test]
fn asset_gc_recovery_retains_a_reappeared_unlinked_object_until_catalog_revival() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create reappeared object directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas.prepare_bytes(b"reappeared exact object").unwrap();
    register_gc_candidate(&mut store, &prepared);
    store
        .connection
        .execute(
            "DELETE FROM asset_objects WHERE object_hash = ?1",
            [&prepared.content_hash],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO asset_object_deletions (
                object_hash, byte_size, physical_key, state, created_at_ms
             ) VALUES (?1, ?2, ?3, 'unlinked', 1)",
            rusqlite::params![
                prepared.content_hash,
                prepared.byte_size as i64,
                prepared.physical_key
            ],
        )
        .unwrap();
    drop(store);

    let mut reopened = PersistentStore::open(directory.path())
        .expect("retain ambiguous reappeared object during recovery");
    assert_eq!(
        cas.stat_object(&prepared.content_hash).unwrap(),
        Some(prepared.byte_size)
    );
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT state FROM asset_object_deletions WHERE object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "unlinked"
    );
    reopened
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: prepared.content_hash.clone(),
                byte_size: prepared.byte_size,
            }],
            200,
        )
        .expect("revive the recreated object");
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT created_at_ms FROM asset_objects WHERE object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        200
    );
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT COUNT(*) FROM asset_object_deletions WHERE object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
}

#[test]
fn asset_gc_delete_page_fails_closed_for_missing_or_corrupt_manifest_roots() {
    use crate::asset_repository::owner_manifest_codec::encode_owner_manifest;

    for corrupt in [false, true] {
        let directory = tempfile::tempdir().expect("create manifest blocker directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
        let candidate = cas.prepare_bytes(b"manifest-blocked candidate").unwrap();
        register_gc_candidate(&mut store, &candidate);
        let manifest_hash = if corrupt {
            let manifest = cas
                .prepare_bytes(&encode_owner_manifest(&[]).unwrap())
                .unwrap();
            fs::write(directory.path().join(&manifest.physical_key), b"corrupt").unwrap();
            manifest.content_hash
        } else {
            "66".repeat(32)
        };
        let generation = super::active_generation(&store.connection).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
             ) VALUES (?1, 'root-module-assets', '0', 1, ?2, 0)",
                rusqlite::params![generation, manifest_hash],
            )
            .unwrap();

        assert!(store.asset_gc_delete_page(16, None, 100, 10).is_err());
        assert_eq!(
            cas.stat_object(&candidate.content_hash).unwrap(),
            Some(candidate.byte_size)
        );
    }
}
