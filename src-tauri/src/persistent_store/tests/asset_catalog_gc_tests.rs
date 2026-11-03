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
        i64::from(super::schema::SCHEMA_VERSION)
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
        super::schema::SCHEMA_VERSION
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
        super::schema::SCHEMA_VERSION
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
fn direct_asset_object_registration_uses_the_initialized_store_database() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create direct registration directory");
    let store = PersistentStore::open(directory.path()).expect("initialize persistent store");
    let registration = AssetObjectRegistration {
        object_hash: "ab".repeat(32),
        byte_size: 42,
    };

    super::super::register_asset_objects_at_root(directory.path(), &[registration.clone()], 17)
        .expect("register through direct database connection");

    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT byte_size, created_at_ms FROM asset_objects WHERE object_hash = ?1",
                [&registration.object_hash],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .expect("read directly registered object"),
        (42, 17)
    );
}

#[test]
fn direct_asset_object_registration_does_not_create_a_missing_database() {
    let directory = tempfile::tempdir().expect("create missing database directory");
    let database_path = directory.path().join("persistent").join("persistent.sqlite");

    assert!(super::super::register_asset_objects_at_root(directory.path(), &[], 0).is_err());
    assert!(!database_path.exists());
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
        "assets/objects/{}/{}",
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
fn snapshot_restore_preserves_current_asset_catalog_and_objects() {
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
    let stage=store.snapshot_restore_stage(&snapshot.id,"catalog-restore").unwrap();
    let authority=store.lww_binding_authority().unwrap();
    store.snapshot_restore_activate(&stage.staging_id,0,authority).unwrap();
    drop(store);

    let restored = PersistentStore::open(directory.path()).expect("restore catalog snapshot");
    assert!(restored
        .query_asset_object_catalog(16, None)
        .expect("query restored catalog")
        .items
        .iter().any(|item|item.object_hash==prepared.content_hash));
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
    let database_path = directory.path().join("persistent/persistent.sqlite");
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
                    crate::asset_repository::job_pins::CasJobOwner::for_test(),
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
fn asset_gc_delete_page_refuses_unsealed_jobs() {
    use crate::asset_repository::job_pins::{CasJobKind, DurableCasJob};

    let directory = tempfile::tempdir().expect("create blocker directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas.prepare_bytes(b"blocked deletion candidate").unwrap();
    register_gc_candidate(&mut store, &prepared);
    let _job = DurableCasJob::begin(
        directory.path(),
        "gc-blocker-job",
        CasJobKind::DirectAssetOrInlayWrite,
        crate::asset_repository::job_pins::CasJobOwner::for_test(),
        1,
    )
    .unwrap();
    let generation = super::active_generation(&store.connection).expect("read generation");
    store
        .connection
        .execute(
            "INSERT INTO plugin_storage (generation, owner, storage_key, byte_size, ordinal, value)
             VALUES (?1, 'synthetic-plugin', 'gc-blocker-plugin', 2, 0, '{}')",
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
    assert!(!page.report.blockers.contains(&"plugin-storage-opaque".to_owned()));
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
        .join("assets/objects/aa/not-a-catalog-object");
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
        crate::asset_repository::job_pins::CasJobOwner::for_test(),
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
        crate::asset_repository::job_pins::CasJobOwner::for_test(),
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
        "assets/objects/{}/{}",
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
    let connection = Connection::open(directory.path().join("persistent/persistent.sqlite")).unwrap();
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

#[test]
fn resolved_aliases_allow_gc_but_unknown_references_and_plugins_report_blocked() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let portrait = cas.prepare_bytes(b"synthetic portrait").unwrap();
    let orphan = cas.prepare_bytes(b"synthetic orphan").unwrap();
    register_gc_candidate(&mut store, &portrait);
    register_gc_candidate(&mut store, &orphan);
    let generation = super::active_generation(&store.connection).unwrap();
    store.connection.execute(
        "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext,
            inlay_type, width, height, metadata)
         VALUES (?1, 'assets/portrait.bin', ?2, 'asset', ?3, 'application/octet-stream', 'portrait', 'bin',
            NULL, NULL, NULL, '{}')",
        rusqlite::params![generation, portrait.content_hash, portrait.byte_size as i64],
    ).unwrap();
    store.connection.execute("UPDATE root SET value = ?1 WHERE generation = ?2",
        rusqlite::params![r#"{"image":"assets/portrait.bin"}"#, generation]).unwrap();
    let roots = super::snapshot::collect_asset_roots(&store.connection).unwrap();
    assert!(roots.legacy_asset_keys.is_empty());
    assert!(roots.object_hashes.contains(&portrait.content_hash));
    let page = store.asset_gc_delete_page(16, None, 100, 10).unwrap();
    assert_eq!(page.report.deleted_hashes, [orphan.content_hash]);
    assert!(cas.stat_object(&portrait.content_hash).unwrap().is_some());

    for plugin in [false, true] {
        let held = cas.prepare_bytes(if plugin { b"plugin held" } else { b"unknown held" }).unwrap();
        register_gc_candidate(&mut store, &held);
        if plugin {
            store.connection.execute("UPDATE root SET value = '{}'", []).unwrap();
            let unresolved = json!({"image":"assets/plugin-unknown.bin"}).to_string();
            store.connection.execute(
                "INSERT INTO plugin_storage (generation, owner, storage_key, byte_size, ordinal, value)
                 VALUES (?1, 'synthetic-plugin', 'opaque', ?2, 0, ?3)",
                rusqlite::params![generation, unresolved.len() as i64, unresolved]).unwrap();
        } else {
            store.connection.execute("UPDATE root SET value = ?1 WHERE generation = ?2",
                rusqlite::params![r#"{"image":"assets/unknown.bin"}"#, generation]).unwrap();
        }
        let preview = store.prepare_asset_gc_preview(100).unwrap();
        let (page, details) = store.asset_gc_preview_page_detail(&preview, 16, None, 100, 10).unwrap();
        assert!(!page.report.blockers.is_empty());
        assert!(page.report.potential_delete_hashes.is_empty());
        assert_eq!(page.report.potential_delete_bytes, 0);
        assert!(details.iter().all(|candidate| candidate.state == "blocked"));
        let executed = store.asset_gc_delete_page(16, None, 100, 10).unwrap();
        assert!(executed.report.deleted_hashes.is_empty());
        assert!(cas.stat_object(&held.content_hash).unwrap().is_some());
    }
}

#[test]
fn resolved_alias_in_another_generation_does_not_unblock_a_reference() {
    let directory = tempfile::tempdir().unwrap();
    let store = PersistentStore::open(directory.path()).unwrap();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let object = cas.prepare_bytes(b"other generation asset").unwrap();
    let generation = super::active_generation(&store.connection).unwrap();
    store.connection.execute("UPDATE root SET value = ?1 WHERE generation = ?2",
        rusqlite::params![r#"{"image":"assets/other.bin"}"#, generation]).unwrap();
    store.connection.execute(
        "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext,
            inlay_type, width, height, metadata)
         VALUES ('other-generation', 'assets/other.bin', ?1, 'asset', ?2, 'application/octet-stream',
            'other', 'bin', NULL, NULL, NULL, '{}')",
        rusqlite::params![object.content_hash, object.byte_size as i64]).unwrap();
    let roots = super::snapshot::collect_asset_roots(&store.connection).unwrap();
    assert!(roots.legacy_asset_keys.contains("assets/other.bin"));
    assert!(roots.object_hashes.contains(&object.content_hash));
}

#[test]
fn command_marks_recheck_new_roots_between_gc_pages() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let mut objects = [cas.prepare_bytes(b"batch-first").unwrap(), cas.prepare_bytes(b"batch-last").unwrap()];
    objects.sort_by(|left, right| left.content_hash.cmp(&right.content_hash));
    for object in &objects { register_gc_candidate(&mut store, object); }
    let marks = store.prepare_asset_gc_delete_marks().unwrap();
    let first = store.asset_gc_delete_marked_page_with_hook(&marks, 1, None, 100, 10, |_| Ok(())).unwrap();
    assert_eq!(first.report.deleted_hashes, [objects[0].content_hash.clone()]);
    let generation = super::active_generation(&store.connection).unwrap();
    store.connection.execute(
        "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext,
            inlay_type, width, height, metadata)
         VALUES (?1, 'assets/between-pages.bin', ?2, 'asset', ?3, 'application/octet-stream',
            'between-pages', 'bin', NULL, NULL, NULL, '{}')",
        rusqlite::params![generation, objects[1].content_hash, objects[1].byte_size as i64]).unwrap();
    let last = store.asset_gc_delete_marked_page_with_hook(&marks, 1, first.next_cursor.as_deref(),
        100, 10, |_| Ok(())).unwrap();
    assert!(last.report.deleted_hashes.is_empty());
    assert!(cas.stat_object(&objects[1].content_hash).unwrap().is_some());
}

#[test]
fn reused_library_roots_are_collected_again_after_a_change_between_gc_pages() {
    for other_connection in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
        let mut objects = [
            cas.prepare_bytes(b"reuse-first").unwrap(),
            cas.prepare_bytes(b"reuse-second").unwrap(),
            cas.prepare_bytes(b"reuse-third").unwrap(),
        ];
        objects.sort_by(|left, right| left.content_hash.cmp(&right.content_hash));
        for object in &objects { register_gc_candidate(&mut store, object); }
        let marks = store.prepare_asset_gc_delete_marks().unwrap();
        let mut library = None;
        super::super::ASSET_GC_LIBRARY_ROOT_COLLECTIONS.with(|count| count.set(0));
        super::super::ASSET_GC_FINAL_MARKINGS.with(|count| count.set(0));
        let first = store.asset_gc_delete_marked_page_reusing_library(&marks, &mut library, 1, None, 100, 10, |_| Ok(())).unwrap();
        let second = store.asset_gc_delete_marked_page_reusing_library(&marks, &mut library, 1, first.next_cursor.as_deref(), 100, 10, |_| Ok(())).unwrap();
        assert_eq!(first.report.deleted_hashes, [objects[0].content_hash.clone()]);
        assert_eq!(second.report.deleted_hashes, [objects[1].content_hash.clone()]);
        assert_eq!(super::super::ASSET_GC_LIBRARY_ROOT_COLLECTIONS.with(|count| count.get()), 1);
        assert_eq!(super::super::ASSET_GC_FINAL_MARKINGS.with(|count| count.get()), 1);
        let generation = super::active_generation(&store.connection).unwrap();
        let insert = "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height, metadata)
             VALUES (?1, 'assets/between-reused-pages.bin', ?2, 'asset', ?3, 'application/octet-stream',
                'between-reused-pages', 'bin', NULL, NULL, NULL, '{}')";
        let params = rusqlite::params![generation, objects[2].content_hash, objects[2].byte_size as i64];
        if other_connection {
            Connection::open(&store.database_path).unwrap().execute(insert, params).unwrap();
        } else {
            store.connection.execute(insert, params).unwrap();
        }
        let last = store.asset_gc_delete_marked_page_reusing_library(&marks, &mut library, 1, second.next_cursor.as_deref(), 100, 10, |_| Ok(())).unwrap();
        assert!(last.report.deleted_hashes.is_empty(), "other connection: {other_connection}");
        assert_eq!(super::super::ASSET_GC_LIBRARY_ROOT_COLLECTIONS.with(|count| count.get()), 2);
        assert_eq!(super::super::ASSET_GC_FINAL_MARKINGS.with(|count| count.get()), 2);
        assert!(cas.stat_object(&objects[2].content_hash).unwrap().is_some());
    }
}

#[test]
fn plugin_gc_operation_collects_once_and_rejects_own_second_and_reopened_writes() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let objects = [cas.prepare_bytes(b"gc-first-plugin-page").unwrap(), cas.prepare_bytes(b"gc-second-plugin-page").unwrap()];
    for object in &objects { register_gc_candidate(&mut store, object); }
    super::snapshot::PLUGIN_ROOT_SCANS.with(|count|count.set(0));
    let marks = store.prepare_asset_gc_delete_marks().unwrap();
    let first = store.asset_gc_delete_marked_page_with_hook(&marks, 1, None, 100, 10, |_|Ok(())).unwrap();
    store.asset_gc_delete_marked_page_with_hook(&marks, 1, first.next_cursor.as_deref(), 100, 10, |_|Ok(())).unwrap();
    assert_eq!(super::snapshot::PLUGIN_ROOT_SCANS.with(|count|count.get()), 1);
    let generation = super::active_generation(&store.connection).unwrap();
    store.connection.execute("INSERT INTO plugin_storage (generation,owner,storage_key,byte_size,ordinal,value) VALUES(?1,'synthetic','one',2,0,'{}')", [&generation]).unwrap();
    assert!(matches!(store.asset_gc_delete_marked_page_with_hook(&marks, 1, None, 100, 10, |_|Ok(())),Err(StoreError::CommitBusy)));
    let marks = store.prepare_asset_gc_delete_marks().unwrap();
    let writer = Connection::open(&store.database_path).unwrap();
    writer.execute("UPDATE plugin_storage SET value='[]' WHERE owner='synthetic'", []).unwrap();
    assert!(matches!(store.asset_gc_delete_marked_page_with_hook(&marks, 1, None, 100, 10, |_|Ok(())),Err(StoreError::CommitBusy)));
    let marks = store.prepare_asset_gc_delete_marks().unwrap();
    drop(writer);
    drop(store);
    let mut reopened = PersistentStore::open(directory.path()).unwrap();
    assert!(matches!(reopened.asset_gc_delete_marked_page_with_hook(&marks, 1, None, 100, 10, |_|Ok(())),Err(StoreError::CommitBusy)));
}

#[test]
fn plugin_gc_changed_after_tombstone_preserves_body_and_reopen_cancels_pending() {
    use crate::asset_repository::migration_gc::AssetGcDeleteHookPoint;
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let object = cas.prepare_bytes(b"gc-gap-plugin-root").unwrap();
    register_gc_candidate(&mut store, &object);
    let generation = super::active_generation(&store.connection).unwrap();
    let writer = Connection::open(&store.database_path).unwrap();
    let result = store.asset_gc_delete_page_with_hook(1, None, 100, 10, |point| {
        if point == AssetGcDeleteHookPoint::AfterTombstone {
            writer.execute("INSERT INTO plugin_storage (generation,owner,storage_key,byte_size,ordinal,value) VALUES(?1,'synthetic','gap',length(?2),0,?2)", rusqlite::params![generation,serde_json::to_string(&object.content_hash)?])?;
        }
        Ok(())
    });
    assert!(matches!(result, Err(StoreError::CommitBusy)));
    assert_eq!(cas.stat_object(&object.content_hash).unwrap(), Some(object.byte_size));
    assert_eq!(store.connection.query_row("SELECT COUNT(*) FROM asset_object_deletions WHERE state='pending'", [], |row|row.get::<_,i64>(0)).unwrap(),1);
    drop(writer);
    drop(store);
    let reopened = PersistentStore::open(directory.path()).unwrap();
    assert_eq!(reopened.connection.query_row("SELECT COUNT(*) FROM asset_object_deletions", [], |row|row.get::<_,i64>(0)).unwrap(),0);
    assert_eq!(cas.stat_object(&object.content_hash).unwrap(),Some(object.byte_size));
}

#[test]
fn plugin_gc_final_transaction_blocks_library_writer_through_unlink() {
    use crate::asset_repository::migration_gc::AssetGcDeleteHookPoint;
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let object = cas.prepare_bytes(b"gc-exclusive-unlink").unwrap();
    register_gc_candidate(&mut store, &object);
    let writer = Connection::open(&store.database_path).unwrap();
    writer.busy_timeout(std::time::Duration::ZERO).unwrap();
    let mut observed = false;
    store.asset_gc_delete_page_with_hook(1,None,100,10,|point| {
        if point == AssetGcDeleteHookPoint::AfterUnlink {
            observed = true;
            assert!(writer.execute("UPDATE plugin_gc_revision SET revision=revision+1 WHERE singleton=1",[]).is_err());
        }
        Ok(())
    }).unwrap();
    assert!(observed);
    writer.execute("UPDATE plugin_gc_revision SET revision=revision+1 WHERE singleton=1",[]).unwrap();
}

#[test]
fn plugin_gc_preserves_both_disabled_namespaces_and_received_inline_roots_without_body_reads() {
    use risunest_sync_wire::unit::{UnitKey, UnitValue};
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let objects = [cas.prepare_bytes(b"plugin-string-body").unwrap(),cas.prepare_bytes(b"plugin-json-body").unwrap(),cas.prepare_bytes(b"plugin-disabled-received-body").unwrap()];
    for object in &objects { register_gc_candidate(&mut store,object); }
    let device = store.device_store().unwrap();
    for (index,space) in ["string","json"].into_iter().enumerate() {
        let value = serde_json::to_string(&objects[index].content_hash).unwrap();
        device.connection().execute("INSERT INTO plugin_device_storage(owner,space,key,value,byte_size,tombstone,write_clock,writer_id) VALUES('synthetic',?1,'asset',?2,?3,0,'0','')",rusqlite::params![space,value,value.len() as i64]).unwrap();
    }
    let key=UnitKey::new(&["plugin-local","received-only","json","asset"]).unwrap();
    let value=UnitValue::inline(&serde_json::to_vec(&objects[2].content_hash).unwrap()).unwrap();
    device.connection().execute("INSERT INTO lww_units(key,stamp,value,version,identity) VALUES(?1,'synthetic-stamp',?2,'synthetic-version',?3)",rusqlite::params![key.as_str(),serde_json::to_string(&value).unwrap(),value.identity().unwrap()]).unwrap();
    assert!(!device.section_state(crate::persistent_store::device_store::Section::LocalPlugins).unwrap().participating);
    crate::asset_repository::body_io::reset_body_io();
    let marks=store.prepare_asset_gc_delete_marks().unwrap();
    let page=store.asset_gc_delete_marked_page_with_hook(&marks,16,None,100,10,|_|Ok(())).unwrap();
    assert!(page.report.deleted_hashes.is_empty());
    for object in &objects { assert!(page.report.marked_hashes.contains(&object.content_hash)); }
    let io=crate::asset_repository::body_io::take_body_io();
    assert!(io.complete());
    assert_eq!(io.asset_work().opens,0);
    assert_eq!(io.asset_work().read_bytes,0);
    let changed=UnitValue::inline(br#"null"#).unwrap();
    store.device_store().unwrap().connection().execute("UPDATE lww_units SET value=?1 WHERE key=?2",rusqlite::params![serde_json::to_string(&changed).unwrap(),key.as_str()]).unwrap();
    assert!(matches!(store.asset_gc_delete_marked_page_with_hook(&marks,16,None,100,10,|_|Ok(())),Err(StoreError::CommitBusy)));
}

#[test]
fn catalog_pages_seek_past_objects_registered_at_the_same_time() {
    use super::asset_object_catalog::AssetObjectRegistration;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let directory = tempfile::tempdir().expect("create catalog directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let objects = (0..2048u32)
        .map(|index| AssetObjectRegistration {
            object_hash: format!("{index:064x}"),
            byte_size: 1,
        })
        .collect::<Vec<_>>();
    for batch in objects.chunks(512) {
        store
            .asset_object_catalog()
            .register(batch, 5)
            .expect("register objects together");
    }
    let mut cursor = None;
    let mut steps = Vec::new();
    loop {
        let counted = Arc::new(AtomicUsize::new(0));
        let counter = counted.clone();
        store
            .connection
            .progress_handler(
                64,
                Some(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            )
            .expect("count page steps");
        let page = store
            .query_asset_object_catalog(256, cursor.as_deref())
            .expect("query catalog page");
        store
            .connection
            .progress_handler(64, None::<fn() -> bool>)
            .expect("stop counting");
        steps.push(counted.load(Ordering::Relaxed));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(steps.len(), 8);
    // A page that walked from the first object to its cursor would cost several first pages.
    assert!(steps.iter().all(|&step| step <= steps[0] * 2), "{steps:?}");
}

/// The object the `asset` alias for `key` names, when the alias exists.
pub(crate) fn alias_object(store: &PersistentStore, key: &str) -> Option<String> {
    store.read_asset_alias("asset", key, None).unwrap().and_then(|alias| alias.value.object_hash)
}

/// Moves the last write of the alias for `key` back to `physical_ms`, as if an older release had
/// written it then.
pub(crate) fn backdate_alias(store: &PersistentStore, key: &str, physical_ms: u64) {
    let unit = super::super::lww::unit_key(&["asset", key]).unwrap();
    let stamp: String = store.connection
        .query_row("SELECT stamp FROM lww_units WHERE key = ?1", [unit.as_str()], |row| row.get(0))
        .unwrap();
    let mut stamp: risunest_sync_wire::stamp::Stamp = serde_json::from_str(&stamp).unwrap();
    stamp.physical_ms = risunest_sync_wire::stamp::DecimalU64(physical_ms);
    store.connection
        .execute("UPDATE lww_units SET stamp = ?1 WHERE key = ?2",
            rusqlite::params![serde_json::to_string(&stamp).unwrap(), unit.as_str()])
        .unwrap();
}

pub(crate) fn add_synthetic_character(store: &mut PersistentStore, character_id: &str, desc: &str) {
    store.commit(&WorkingSetCommit {
        add_character: Some(json!({
            "chaId": character_id, "type": "character", "name": "Synthetic", "image": "",
            "desc": desc, "chats": [], "lastInteraction": 500,
        })),
        ..empty_working_set_commit(store.revision().unwrap())
    }).unwrap();
}

#[test]
fn unused_file_cleanup_keeps_aliases_referenced_in_any_stored_form_or_held_by_another_root() {
    use crate::server_sync::lww_tests::put_asset;
    use super::super::asset_alias_orphans::OrphanAliasScope;

    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let keys = [
        "assets/shared.png", "assets/html.png", "assets/backslash.png", "assets/한글.png",
        "assets/plugin-bare.png", "assets/setting.png", "assets/archived.png",
        "assets/archived-html.png", "assets/archived-chat.png", "assets/archived-local.png",
        "assets/snapshotted.png", "assets/ledger.png",
    ];
    // Only this alias is old enough for the snapshot to hold it.
    put_asset(&mut store, "assets/snapshotted.png", b"synthetic snapshotted body");
    add_synthetic_character(&mut store, "snapshot-owner", "assets/snapshotted.png");
    let snapshot = store.snapshot_create("synthetic-alias-cleanup").unwrap();
    store.commit(&WorkingSetCommit {
        delete_character_ids: Some(vec!["snapshot-owner".into()]),
        ..empty_working_set_commit(store.revision().unwrap())
    }).unwrap();
    for key in keys.iter().filter(|key| **key != "assets/snapshotted.png") {
        put_asset(&mut store, key, format!("synthetic body of {key}").as_bytes());
    }
    add_synthetic_character(&mut store, "kept",
        r#"<div><img src="assets/html.png" alt=""></div>{{raw::assets/shared.png}} assets\backslash.png assets/한글.png"#);
    data_health_tests::add_character_with_owner_manifest(
        &mut store, "manifest-owner", "assets/manifest.png", "portrait.png");
    // Only the owner manifest still names this key.
    store.connection.execute(
        "UPDATE characters SET detail = json_remove(detail, '$.additionalAssets') WHERE character_id = 'manifest-owner'",
        []).unwrap();
    store.connection.execute("DELETE FROM lww_units WHERE key = ?1",
        [super::super::lww::unit_key(&["character", "manifest-owner", "additionalAssets"]).unwrap().as_str()]).unwrap();
    // A newer body, so the manifest holds a different object than the alias names.
    put_asset(&mut store, "assets/manifest.png", b"synthetic newer manifest body");
    store.commit(&WorkingSetCommit {
        add_character: Some(json!({
            "chaId": "archived", "type": "character", "name": "Synthetic", "image": "",
            "desc": "assets/archived.png", "firstMessage": r#"<p><img src="assets/archived-html.png"></p>"#,
            // A field this device keeps for itself, which only the local payload holds.
            "syntheticLocalNote": r#"<img src="assets/archived-local.png">"#,
            "lastInteraction": 500,
            "chats": [{"id": "archived-chat", "name": "Chat", "message": [
                {"role": "char", "data": r#"앞 글<img src="assets/archived-chat.png">뒤 글"#, "chatId": "archived-first"},
            ]}],
        })),
        ..empty_working_set_commit(store.revision().unwrap())
    }).unwrap();
    store.archive_character("archived", store.revision().unwrap(), 10).unwrap();
    // Only the archived payload still names these keys.
    assert!(store.connection.execute(r#"DELETE FROM lww_units WHERE key LIKE '["character","archived"%'"#, []).unwrap() > 0);
    // Another device that receives the archive holds what its shared payload names.
    let archived = super::super::archive::read_archived_object(
        &store.connection, &super::active_generation(&store.connection).unwrap(), "archived").unwrap().unwrap();
    for key in ["assets/archived-html.png", "assets/archived-chat.png"] {
        assert!(archived.shared_asset_hashes.contains(&alias_object(&store, key).unwrap()), "{key} must be shared");
    }
    store.commit(&WorkingSetCommit {
        plugin_storage: Some(vec![PluginStorageMutation::Set {
            owner: "synthetic-plugin".into(), key: "portrait".into(), value: json!({"id": "plugin-bare.png"}),
        }]),
        ..empty_working_set_commit(store.revision().unwrap())
    }).unwrap();
    let device = store.device_store().unwrap();
    device.write_setting("official-account.pending-assets.v1", &json!([{"key": "assets/setting.png"}])).unwrap();
    device.write_setting("official-account.asset-ledger.v1", &json!({"assets/ledger.png": 1})).unwrap();
    for key in keys.iter().copied().chain(["assets/manifest.png"]) {
        backdate_alias(&store, key, 1_000);
    }
    let now = super::super::device_store::now_ms().unwrap();
    let kept = |store: &PersistentStore, deleted: &[&str]| {
        for key in keys.iter().copied().chain(["assets/manifest.png"]).filter(|key| !deleted.contains(key)) {
            assert!(alias_object(store, key).is_some(), "{key} must stay");
        }
    };

    // An asset key nothing resolves blocks the cleanup, so no alias counts as unused.
    let generation = super::active_generation(&store.connection).unwrap();
    store.connection.execute("UPDATE root SET value = ?1 WHERE generation = ?2",
        rusqlite::params![r#"{"image":"assets/unknown.bin"}"#, generation]).unwrap();
    assert!(store.delete_orphaned_asset_aliases(&OrphanAliasScope::Cleanup { now_ms: now }).unwrap().is_empty());
    kept(&store, &[]);
    store.connection.execute("UPDATE root SET value = '{}' WHERE generation = ?1", [&generation]).unwrap();

    let deleted = store.delete_orphaned_asset_aliases(&OrphanAliasScope::Cleanup { now_ms: now }).unwrap();
    assert_eq!(deleted.iter().map(|alias| alias.key.as_str()).collect::<Vec<_>>(), ["assets/ledger.png"]);
    kept(&store, &["assets/ledger.png"]);

    store.snapshot_delete(&snapshot.id).unwrap();
    let deleted = store.delete_orphaned_asset_aliases(&OrphanAliasScope::Cleanup { now_ms: now }).unwrap();
    assert_eq!(deleted.iter().map(|alias| alias.key.as_str()).collect::<Vec<_>>(), ["assets/snapshotted.png"]);
    kept(&store, &["assets/ledger.png", "assets/snapshotted.png"]);
}

#[test]
fn unused_alias_cleanup_keeps_device_plugin_writers_out_until_it_commits() {
    use crate::server_sync::lww_tests::put_asset;
    use super::super::asset_alias_orphans::OrphanAliasScope;

    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    put_asset(&mut store, "assets/orphan.png", b"synthetic orphan body");
    backdate_alias(&store, "assets/orphan.png", 1_000);
    let now = super::super::device_store::now_ms().unwrap();
    let device_path = store.device_store().unwrap().connection().path().unwrap().to_owned();

    // A plugin writer on another connection could save a reference to the key between the
    // reference check and the commit, so it must find the device store locked by then.
    let writer_locked_out = std::rc::Rc::new(std::cell::Cell::new(false));
    let locked_out = writer_locked_out.clone();
    super::super::lww::BEFORE_HELD_COMMIT.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
        let writer = rusqlite::Connection::open(device_path).unwrap();
        writer.busy_timeout(std::time::Duration::ZERO).unwrap();
        locked_out.set(writer.execute_batch("BEGIN IMMEDIATE").is_err());
    })));
    let deleted = store.delete_orphaned_asset_aliases(&OrphanAliasScope::Cleanup { now_ms: now }).unwrap();
    assert_eq!(deleted.iter().map(|alias| alias.key.as_str()).collect::<Vec<_>>(), ["assets/orphan.png"]);
    assert!(writer_locked_out.get());

    // An app that ends after the commit intent is reserved leaves the delete to recovery at the
    // next start, which applies it only while the plugin values it was checked against stand.
    let interrupt = |store: &mut PersistentStore| {
        super::super::lww::BEFORE_HELD_COMMIT.with(|hook| *hook.borrow_mut() = Some(Box::new(|| panic!("synthetic app end"))));
        let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.delete_orphaned_asset_aliases(&OrphanAliasScope::Cleanup { now_ms: now })
        }));
        assert!(ended.is_err());
    };
    put_asset(&mut store, "assets/recovered.png", b"synthetic recovered body");
    backdate_alias(&store, "assets/recovered.png", 1_000);
    interrupt(&mut store);
    store.lww_recover_intents().unwrap();
    assert!(alias_object(&store, "assets/recovered.png").is_none());

    put_asset(&mut store, "assets/named-later.png", b"synthetic named later body");
    backdate_alias(&store, "assets/named-later.png", 1_000);
    interrupt(&mut store);
    let value = serde_json::to_string("named-later.png").unwrap();
    store.device_store().unwrap().connection().execute(
        "INSERT INTO plugin_device_storage(owner,space,key,value,byte_size,tombstone,write_clock,writer_id) VALUES('synthetic','string','portrait',?1,?2,0,'0','')",
        rusqlite::params![value, value.len() as i64]).unwrap();
    store.lww_recover_intents().unwrap();
    assert!(alias_object(&store, "assets/named-later.png").is_some());
}

#[test]
fn unused_alias_cleanup_reads_received_message_pages_not_applied_yet() {
    use crate::server_sync::lww_tests::put_asset;
    use super::super::asset_alias_orphans::OrphanAliasScope;
    use risunest_external_storage_format::{logical_records::encode_message_page, message_pages::{ManifestPage, MessageManifest, MANIFEST_SCHEMA}};
    use risunest_sync_wire::{descriptor::RecordDescriptor, unit::{UnitKey, UnitValue}};

    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    for key in ["assets/received.png", "assets/unused.png", "assets/late.png"] {
        put_asset(&mut store, key, format!("synthetic body of {key}").as_bytes());
        backdate_alias(&store, key, 1_000);
    }
    let page = encode_message_page(&[json!({"role": "char", "data": "<img src=\"assets/received.png\">"})]).unwrap();
    let manifest = MessageManifest {
        schema: MANIFEST_SCHEMA.into(),
        message_count: 1.into(),
        pages: vec![ManifestPage { hash: page.hash.clone(), message_count: 1, byte_length: page.size.into() }],
    }.encode().unwrap();
    super::super::message_pages::put_object(&store.connection, &manifest.hash, &manifest.bytes).unwrap();
    let value = UnitValue::object(RecordDescriptor::content(manifest.hash.clone())).unwrap();
    let key = UnitKey::new(&["messages", "receiving-owner", "chat"]).unwrap();
    store.connection.execute("INSERT INTO lww_receive_rows VALUES('synthetic-receive',?1,'{}',?2,'staged')",
        rusqlite::params![key.as_str(), serde_json::to_string(&value).unwrap()]).unwrap();
    let now = super::super::device_store::now_ms().unwrap();
    let cleanup = OrphanAliasScope::Cleanup { now_ms: now };

    // Until its page arrives, the received chat may use any of them.
    assert!(store.delete_orphaned_asset_aliases(&cleanup).unwrap().is_empty());

    super::super::message_pages::put_object(&store.connection, &page.hash, &page.bytes).unwrap();
    // A row received through another connection after the search may use the rest.
    let library_path = store.database_path.clone();
    super::super::lww::BEFORE_HELD_COMMIT.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
        rusqlite::Connection::open(library_path).unwrap()
            .execute("INSERT INTO lww_receive_rows VALUES('later-receive','[\"root\"]','{}','{}','held')", []).unwrap();
    })));
    assert!(matches!(store.delete_orphaned_asset_aliases(&cleanup), Err(StoreError::RevisionConflict { .. })));
    assert!(alias_object(&store, "assets/unused.png").is_some());

    store.connection.execute("DELETE FROM lww_receive_rows WHERE request_id = 'later-receive'", []).unwrap();
    let deleted = store.delete_orphaned_asset_aliases(&cleanup).unwrap();
    assert_eq!(deleted.iter().map(|alias| alias.key.as_str()).collect::<Vec<_>>(), ["assets/late.png", "assets/unused.png"]);
    assert!(alias_object(&store, "assets/received.png").is_some());
}

#[test]
fn a_commit_whose_hold_fails_is_not_applied_by_recovery() {
    use crate::server_sync::lww_tests::put_asset;

    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    put_asset(&mut store, "assets/kept.png", b"synthetic kept body");
    let delete = WorkingSetCommit {
        expected_revision: store.revision().unwrap(),
        unit_mutations: Some(vec![super::super::lww::UnitMutation::Delete {
            key: super::super::lww::unit_key(&["asset", "assets/kept.png"]).unwrap(),
        }]),
        ..Default::default()
    };
    assert!(matches!(
        store.commit_holding(&delete, |_| Err::<(), _>(StoreError::CommitBusy)),
        Err(StoreError::CommitBusy)
    ));
    store.lww_recover_intents().unwrap();
    assert!(alias_object(&store, "assets/kept.png").is_some());
}

/// A permanent delete reads its character's details without the conversations, so the keys only
/// a chat names come from the store, by the rules the renderer applies to the details.
#[test]
fn deleted_character_conversations_give_the_owned_keys_they_name() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    for (character_id, data) in [
        ("owner", "<img src=\"assets/chat.png\"> {{raw::assets\\raw.png}}"),
        ("other", "assets/other.png"),
    ] {
        store.commit(&WorkingSetCommit {
            add_character: Some(json!({
                "chaId": character_id, "type": "character", "name": "Synthetic", "image": "", "desc": "",
                "lastInteraction": 500,
                "chats": [{"id": format!("{character_id}-chat"), "name": "Chat", "note": "see assets/note.png", "message": [
                    {"role": "user", "data": data, "chatId": format!("{character_id}-first")},
                    {"role": "char", "data": "sync-conflict-backups/a.risudat assets/", "chatId": format!("{character_id}-second")},
                    {"role": "char", "data": format!("assets/{}.png", "가".repeat(1400)), "chatId": format!("{character_id}-long")},
                ]}],
            })),
            ..empty_working_set_commit(store.revision().unwrap())
        }).unwrap();
    }

    let owner = ["owner".to_owned()];
    assert_eq!(
        store.character_conversation_asset_keys(&owner, 100).unwrap(),
        std::collections::BTreeSet::from(["assets/chat.png", "assets/note.png", "assets/raw.png"].map(str::to_owned)),
    );
    assert_eq!(store.character_conversation_asset_keys(&owner, 1).unwrap().len(), 1);
}

/// An archived character's details and chats live in its archive, which a permanent delete does
/// not read, so its owned keys are the aliases of the objects the archive recorded.
#[test]
fn an_archived_character_gives_the_owned_keys_its_archive_recorded() {
    use crate::server_sync::lww_tests::put_asset;
    use super::super::asset_alias_orphans::OrphanAliasScope;

    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    for key in ["assets/avatar.png", "assets/chat.png", "assets/other.png"] {
        put_asset(&mut store, key, format!("synthetic body of {key}").as_bytes());
    }
    store.commit(&WorkingSetCommit {
        add_character: Some(json!({
            "chaId": "owner", "type": "character", "name": "Synthetic", "image": "assets/avatar.png", "desc": "",
            "lastInteraction": 500,
            "chats": [{"id": "owner-chat", "name": "Chat", "message": [
                {"role": "user", "data": "assets/chat.png", "chatId": "owner-first"},
            ]}],
        })),
        ..empty_working_set_commit(store.revision().unwrap())
    }).unwrap();
    add_synthetic_character(&mut store, "other", "assets/other.png");
    store.archive_character("owner", store.revision().unwrap(), 10).unwrap();

    let owner = ["owner".to_owned()];
    let keys = store.character_conversation_asset_keys(&owner, 100).unwrap();
    assert_eq!(keys, std::collections::BTreeSet::from(["assets/avatar.png", "assets/chat.png"].map(str::to_owned)));
    assert_eq!(store.character_conversation_asset_keys(&owner, 1).unwrap().len(), 1);

    let started_at_ms = super::super::device_store::now_ms().unwrap() + 1;
    store.commit(&WorkingSetCommit {
        unit_mutations: Some(vec![super::super::lww::UnitMutation::Delete {
            key: super::super::lww::unit_key(&["exists", "character", "owner"]).unwrap(),
        }]),
        ..empty_working_set_commit(store.revision().unwrap())
    }).unwrap();
    let generation = super::active_generation(&store.connection).unwrap();
    assert!(super::super::archive::read_archived_object(&store.connection, &generation, "owner").unwrap().is_none());
    let deleted = store.delete_orphaned_asset_aliases(&OrphanAliasScope::DeletedOwner { keys: &keys, started_at_ms }).unwrap();
    assert_eq!(deleted.iter().map(|alias| alias.key.as_str()).collect::<Vec<_>>(), ["assets/avatar.png", "assets/chat.png"]);
    assert!(alias_object(&store, "assets/other.png").is_some());
}
