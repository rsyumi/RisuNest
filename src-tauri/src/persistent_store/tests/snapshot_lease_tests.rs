use super::*;

#[test]
fn schema_configures_the_documented_sqlite_profile() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");

    let integer_pragma = |name: &str| {
        store
            .connection
            .query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0))
            .expect("read integer pragma")
    };
    let journal_mode: String = store
        .connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read journal mode");

    assert_eq!(journal_mode, "wal");
    assert_eq!(integer_pragma("synchronous"), 1);
    assert_eq!(integer_pragma("busy_timeout"), 5_000);
    assert_eq!(integer_pragma("cache_size"), -16_000);
    assert_eq!(integer_pragma("temp_store"), 2);
    assert_eq!(integer_pragma("journal_size_limit"), 67_108_864);
    assert_eq!(integer_pragma("foreign_keys"), 0);
    assert_eq!(integer_pragma("user_version"), 1);
}

#[test]
fn snapshots_create_list_and_restore_on_reopen() {
    let (directory, mut store, database) = open_fixture();
    let snapshot = store
        .snapshot_create("contract-test")
        .expect("create snapshot");
    assert!(std::path::Path::new(&snapshot.path).is_file());
    assert!(snapshot.bytes > 0);
    assert_eq!(store.snapshot_list().expect("list snapshots").len(), 1);

    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Changed after snapshot" })),
            ..empty_working_set_commit(1)
        })
        .expect("change database after snapshot");
    store
        .snapshot_restore_request(std::path::Path::new(&snapshot.path))
        .expect("request snapshot restore");
    drop(store);

    let restored = PersistentStore::open(directory.path()).expect("restore snapshot on reopen");
    assert_eq!(restored.revision().expect("read restored revision"), 1);
    assert_eq!(
        restored
            .materialize(None)
            .expect("materialize restored data"),
        database
    );
    assert!(std::path::Path::new(&snapshot.path).is_file());
    assert!(!directory
        .path()
        .join("persistent/snapshots/pending-restore.json")
        .exists());
}

#[test]
fn snapshot_delete_requires_a_listed_top_level_snapshot_and_removes_its_sidecar() {
    let (directory, store, _) = open_fixture();
    let created = store
        .snapshot_create("delete-test")
        .expect("create snapshot");
    let snapshot = std::path::PathBuf::from(&created.path);
    let sidecar =
        crate::asset_repository::migration_gc::snapshot_asset_root_sidecar_path(&snapshot);
    assert!(sidecar.is_file());

    store
        .snapshot_delete(&snapshot)
        .expect("delete listed snapshot");
    assert!(!snapshot.exists());
    assert!(!sidecar.exists());

    let nested = directory
        .path()
        .join("persistent/snapshots/nested/not-a-snapshot.db");
    std::fs::create_dir_all(nested.parent().expect("nested parent")).expect("create nested parent");
    std::fs::write(&nested, b"not a snapshot").expect("write nested file");
    assert!(store.snapshot_delete(&nested).is_err());
}

#[test]
fn snapshot_delete_rejects_a_pending_restore_target_without_removing_it() {
    let (directory, store, _) = open_fixture();
    let created = store
        .snapshot_create("pending-delete")
        .expect("create snapshot");
    let snapshot = std::path::PathBuf::from(&created.path);
    store
        .snapshot_restore_request(&snapshot)
        .expect("request pending restore");

    let error = store
        .snapshot_delete(&snapshot)
        .expect_err("pending restore target must not be deleted");

    assert!(error.to_string().contains("pending restore"));
    assert!(snapshot.is_file());
    assert!(directory
        .path()
        .join("persistent/snapshots/pending-restore.json")
        .is_file());
}

#[cfg(unix)]
#[test]
fn snapshot_list_and_restore_reject_linked_candidates() {
    let (directory, store, _) = open_fixture();
    let external = directory.path().join("external-snapshot.db");
    std::fs::write(&external, b"external snapshot").expect("write external candidate");
    let linked = directory
        .path()
        .join("persistent/snapshots/linked-snapshot.db");
    std::os::unix::fs::symlink(&external, &linked).expect("create snapshot symlink");

    assert!(store
        .snapshot_list()
        .expect("list snapshots")
        .iter()
        .all(|snapshot| snapshot.path != linked.to_string_lossy()));
    assert!(store.snapshot_restore_request(&linked).is_err());
    assert_eq!(
        std::fs::read(&external).expect("read external candidate"),
        b"external snapshot"
    );
}

#[cfg(windows)]
#[test]
fn snapshot_list_and_restore_reject_file_symlink_candidates() {
    let (directory, store, _) = open_fixture();
    let external = directory.path().join("external-snapshot.db");
    std::fs::write(&external, b"external snapshot").expect("write external candidate");
    let linked = directory
        .path()
        .join("persistent/snapshots/linked-snapshot.db");
    match std::os::windows::fs::symlink_file(&external, &linked) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!(
                "Windows denied file symlink creation, so linked snapshot integration coverage is unavailable: {error}"
            );
            assert!(!snapshot::snapshot_path_is_link_or_reparse(&external)
                .expect("inspect regular snapshot candidate"));
            return;
        }
        Err(error) => panic!("create snapshot file symlink: {error}"),
    }

    assert!(snapshot::snapshot_path_is_link_or_reparse(&linked)
        .expect("inspect linked snapshot candidate"));

    assert!(store
        .snapshot_list()
        .expect("list snapshots")
        .iter()
        .all(|snapshot| snapshot.path != linked.to_string_lossy()));
    assert!(store.snapshot_restore_request(&linked).is_err());
    assert_eq!(
        std::fs::read(&external).expect("read external candidate"),
        b"external snapshot"
    );
}

#[test]
fn snapshot_creation_persists_asset_roots_before_returning() {
    let (directory, store, _) = open_fixture();
    let generation = super::active_generation(&store.connection).expect("read active generation");
    let manifest_hash = "a".repeat(64);
    let object_hash = "b".repeat(64);
    let cold_object_hash = "c".repeat(64);
    store
        .connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params![
                generation,
                serde_json::to_string(&json!({
                    "asset": "assets/exact.bin",
                    "inlay": "{{inlay::kept-inlay}}",
                    "coldStoragedChats": ["cold-chat"]
                }))
                .unwrap()
            ],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height
             ) VALUES (?1, 'assets/missing.bin', NULL, 'asset', 0,
                'application/octet-stream', 'missing', 'bin', NULL, NULL, NULL)",
            [&generation],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height
             ) VALUES (?1, 'assets/exact.bin', ?2, 'asset', 1,
                'application/octet-stream', 'exact', 'bin', NULL, NULL, NULL)",
            rusqlite::params![generation, object_hash],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'cold-chat', ?2, 1, '{}')",
            rusqlite::params![generation, cold_object_hash],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
             ) VALUES (?1, 'root-module-assets', 'module-1', 1, ?2, 1)",
            rusqlite::params![generation, manifest_hash],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO plugin_storage (generation, storage_key, byte_size, ordinal, value)
             VALUES (?1, 'opaque', 2, 0, '{}')",
            [generation],
        )
        .unwrap();

    let snapshot = store.snapshot_create("asset-roots").unwrap();
    let sidecar = crate::asset_repository::migration_gc::read_snapshot_asset_root_sidecar(
        Path::new(&snapshot.path),
    )
    .expect("read snapshot asset-root sidecar");

    assert_eq!(sidecar.revision, 1);
    assert_eq!(sidecar.roots.manifest_hashes, [manifest_hash].into());
    assert_eq!(
        sidecar.roots.object_hashes,
        [object_hash, cold_object_hash].into()
    );
    assert_eq!(
        sidecar.roots.legacy_asset_keys,
        [
            "assets/exact.bin".to_owned(),
            "assets/missing.bin".to_owned()
        ]
        .into()
    );
    assert_eq!(sidecar.roots.inlay_ids, ["kept-inlay".to_owned()].into());
    assert_eq!(sidecar.roots.cold_keys, ["cold-chat".to_owned()].into());
    assert_eq!(
        sidecar.roots.blockers,
        [
            "cold-payload-unscanned".to_owned(),
            "plugin-storage-opaque".to_owned()
        ]
        .into()
    );
    assert!(sidecar.roots.retain_all_objects);
    assert!(
        crate::asset_repository::migration_gc::snapshot_asset_root_sidecar_path(Path::new(
            &snapshot.path
        ))
        .is_file()
    );
    drop(directory);
}

#[test]
fn asset_gc_dry_run_keeps_leased_generation_roots_until_release() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let original_payload = cas.prepare_bytes(b"original").unwrap();
    let replacement_payload = cas.prepare_bytes(b"replacement").unwrap();
    let collectable_payload = cas.prepare_bytes(b"collectable").unwrap();
    let original = AssetAlias {
        key: "assets/leased.bin".to_owned(),
        object_hash: Some(original_payload.content_hash.clone()),
        kind: "asset".to_owned(),
        size: original_payload.byte_size as i64,
        mime: "application/octet-stream".to_owned(),
        name: "Leased".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let first = store.commit_asset_alias(&original, 0).unwrap();
    let lease = store.acquire_revision(first.revision).unwrap();
    let replacement = AssetAlias {
        object_hash: Some(replacement_payload.content_hash.clone()),
        size: replacement_payload.byte_size as i64,
        ..original
    };
    store
        .commit_asset_alias(&replacement, first.revision)
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[
                AssetObjectRegistration {
                    object_hash: original_payload.content_hash.clone(),
                    byte_size: original_payload.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: replacement_payload.content_hash.clone(),
                    byte_size: replacement_payload.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: collectable_payload.content_hash.clone(),
                    byte_size: collectable_payload.byte_size,
                },
            ],
            0,
        )
        .expect("register GC candidates");

    let leased = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert!(leased
        .marked_hashes
        .contains(&original_payload.content_hash));
    assert!(leased
        .marked_hashes
        .contains(&replacement_payload.content_hash));
    assert_eq!(
        leased.potential_delete_hashes,
        vec![collectable_payload.content_hash.clone()]
    );

    store.release_revision(&lease.lease).unwrap();
    let released = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert_eq!(
        released.marked_hashes,
        vec![replacement_payload.content_hash]
    );
    assert_eq!(
        released
            .potential_delete_hashes
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        [
            collectable_payload.content_hash,
            original_payload.content_hash
        ]
        .into_iter()
        .collect()
    );
    assert!(!released.deletion_enabled);
}

#[test]
fn asset_gc_dry_run_retains_every_catalog_object_for_opaque_plugin_storage() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let first = cas.prepare_bytes(b"plugin-private-a").unwrap();
    let second = cas.prepare_bytes(b"plugin-private-b").unwrap();
    let generation = super::active_generation(&store.connection).expect("read generation");
    store
        .connection
        .execute(
            "INSERT INTO plugin_storage (generation, storage_key, byte_size, ordinal, value)
             VALUES (?1, 'opaque-plugin', 22, 0, ?2)",
            rusqlite::params![
                generation,
                serde_json::to_string(&json!({
                    "privateEncoding": "cGx1Z2luLWRlZmluZWQtcmVmZXJlbmNl"
                }))
                .unwrap()
            ],
        )
        .unwrap();
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
        .expect("register plugin liveness candidates");

    let first_page = store.asset_gc_dry_run(1, None, 100, 10).unwrap();
    let second_page = store
        .asset_gc_dry_run(1, first_page.next_cursor.as_deref(), 100, 10)
        .unwrap();
    let mut marked = first_page.report.marked_hashes.clone();
    marked.extend(second_page.report.marked_hashes.clone());
    marked.sort();
    marked.dedup();

    assert_eq!(
        marked,
        [first.content_hash, second.content_hash]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    assert!(first_page.report.potential_delete_hashes.is_empty());
    assert!(second_page.report.potential_delete_hashes.is_empty());
    assert!(first_page.next_cursor.is_some());
    assert!(second_page.next_cursor.is_none());
    for report in [first_page.report, second_page.report] {
        assert!(report
            .blockers
            .contains(&"plugin-storage-opaque".to_owned()));
        assert!(!report.deletion_enabled);
    }
}

#[test]
fn asset_gc_dry_run_marks_native_cas_references_nested_in_cold_payloads() {
    use super::asset_object_catalog::AssetObjectRegistration;
    use flate2::{
        write::{DeflateEncoder, GzEncoder, ZlibEncoder},
        Compression,
    };
    use std::io::Write;

    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let nested = cas.prepare_bytes(b"nested-cold-resource").unwrap();
    let nested_url = cas.prepare_bytes(b"nested-cold-url-resource").unwrap();
    let collectable = cas.prepare_bytes(b"not-referenced-by-cold").unwrap();
    let nested_physical_key = format!(
        "assets-v2/objects/{}/{}",
        &nested.content_hash[..2],
        &nested.content_hash[2..]
    );
    let nested_url_physical_key = format!(
        "assets-v2/objects/{}/{}",
        &nested_url.content_hash[..2],
        &nested_url.content_hash[2..]
    );
    let nested_render_url = format!(
        "HTTP://user@RISUASSET.LOCALHOST:8080/{}",
        hex::encode(nested_url_physical_key)
    );
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(
            serde_json::to_string(&json!({
                "character": {
                    "name": "Cold fixture",
                    "roadmap14Unknown": {
                        "nested": [nested_physical_key.clone(), nested_render_url]
                    },
                    "chats": [{
                        "message": [{
                            "data": "\u{ef01}COLDSTORAGE\u{ef01}cold-zlib"
                        }]
                    }]
                }
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    let mut cold_bytes = encoder.finish().unwrap();
    let cold = cas.prepare_bytes(&cold_bytes).unwrap();
    let oversized = super::snapshot::decode_cold_payload_with_limit(
        &cas,
        &cold.content_hash,
        cold.byte_size,
        32,
    )
    .expect_err("decoded output over the configured limit must fail closed");
    assert!(oversized.to_string().contains("exceeds the decoded limit"));
    let mut zlib_encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    zlib_encoder
        .write_all(
            serde_json::to_string(&json!({
                "message": { "data": "\u{ef01}COLDSTORAGE\u{ef01}cold-raw" }
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    let cold_zlib = cas.prepare_bytes(&zlib_encoder.finish().unwrap()).unwrap();
    let mut deflate_encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    deflate_encoder
        .write_all(
            serde_json::to_string(&json!([nested_physical_key]))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    let cold_raw = cas
        .prepare_bytes(&deflate_encoder.finish().unwrap())
        .unwrap();
    let generation = super::active_generation(&store.connection).expect("read generation");
    store
        .connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params![
                generation,
                serde_json::to_string(&json!({ "coldstorage": "cold-root" })).unwrap()
            ],
        )
        .unwrap();
    for (key, alias) in [("cold-zlib", &cold_zlib), ("cold-raw", &cold_raw)] {
        store
            .connection
            .execute(
                "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
                 VALUES (?1, ?2, ?3, ?4, '{}')",
                rusqlite::params![generation, key, alias.content_hash, alias.byte_size as i64],
            )
            .unwrap();
    }
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'cold-root', ?2, ?3, '{}')",
            rusqlite::params![generation, cold.content_hash, cold.byte_size as i64],
        )
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[
                AssetObjectRegistration {
                    object_hash: cold.content_hash.clone(),
                    byte_size: cold.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: cold_zlib.content_hash.clone(),
                    byte_size: cold_zlib.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: cold_raw.content_hash.clone(),
                    byte_size: cold_raw.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: nested.content_hash.clone(),
                    byte_size: nested.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: nested_url.content_hash.clone(),
                    byte_size: nested_url.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: collectable.content_hash.clone(),
                    byte_size: collectable.byte_size,
                },
            ],
            0,
        )
        .expect("register cold liveness candidates");

    let report = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;

    assert!(report.marked_hashes.contains(&cold.content_hash));
    assert!(report.marked_hashes.contains(&cold_zlib.content_hash));
    assert!(report.marked_hashes.contains(&cold_raw.content_hash));
    assert!(report.marked_hashes.contains(&nested.content_hash));
    assert!(report.marked_hashes.contains(&nested_url.content_hash));
    assert_eq!(
        report.potential_delete_hashes,
        vec![collectable.content_hash.clone()]
    );
    assert!(!report
        .blockers
        .contains(&"cold-payload-unscanned".to_owned()));
    assert!(!report
        .blockers
        .contains(&"native-asset-url-unresolved".to_owned()));
    assert!(!report.deletion_enabled);

    store
        .connection
        .execute(
            "DELETE FROM cold_aliases WHERE generation = ?1 AND key = 'cold-raw'",
            [&generation],
        )
        .unwrap();
    let missing_nested = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert!(missing_nested.potential_delete_hashes.is_empty());
    assert!(missing_nested
        .blockers
        .contains(&"cold-payload-unscanned".to_owned()));
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'cold-raw', ?2, ?3, '{}')",
            rusqlite::params![generation, cold_raw.content_hash, cold_raw.byte_size as i64],
        )
        .unwrap();

    let mut unknown_encoder = GzEncoder::new(Vec::new(), Compression::default());
    unknown_encoder
        .write_all(b"{\"roadmap14Unknown\":true}")
        .unwrap();
    let unknown = cas
        .prepare_bytes(&unknown_encoder.finish().unwrap())
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'cold-unknown', ?2, ?3, '{}')",
            rusqlite::params![generation, unknown.content_hash, unknown.byte_size as i64],
        )
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: unknown.content_hash.clone(),
                byte_size: unknown.byte_size,
            }],
            0,
        )
        .unwrap();
    let unknown_report = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert!(unknown_report
        .marked_hashes
        .contains(&collectable.content_hash));
    assert!(unknown_report.potential_delete_hashes.is_empty());
    assert!(unknown_report
        .blockers
        .contains(&"cold-payload-unscanned".to_owned()));
    store
        .connection
        .execute(
            "DELETE FROM cold_aliases WHERE generation = ?1 AND key = 'cold-unknown'",
            [&generation],
        )
        .unwrap();

    let mut unknown_url_encoder = GzEncoder::new(Vec::new(), Compression::default());
    unknown_url_encoder
        .write_all(b"{\"message\":{\"data\":\"HTTP://RISUASSET.LOCALHOST/not-hex\"}}")
        .unwrap();
    let unknown_url = cas
        .prepare_bytes(&unknown_url_encoder.finish().unwrap())
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'cold-unknown-url', ?2, ?3, '{}')",
            rusqlite::params![
                generation,
                unknown_url.content_hash,
                unknown_url.byte_size as i64
            ],
        )
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: unknown_url.content_hash.clone(),
                byte_size: unknown_url.byte_size,
            }],
            0,
        )
        .unwrap();
    let unknown_url_report = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert!(unknown_url_report.potential_delete_hashes.is_empty());
    assert!(unknown_url_report
        .blockers
        .contains(&"native-asset-url-unresolved".to_owned()));
    store
        .connection
        .execute(
            "DELETE FROM cold_aliases WHERE generation = ?1 AND key = 'cold-unknown-url'",
            [generation],
        )
        .unwrap();

    cold_bytes[0] ^= 0xff;
    fs::write(directory.path().join(&cold.physical_key), cold_bytes).unwrap();
    let blocked = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert!(blocked.marked_hashes.contains(&cold.content_hash));
    assert!(blocked.marked_hashes.contains(&nested.content_hash));
    assert!(blocked.marked_hashes.contains(&nested_url.content_hash));
    assert!(blocked.marked_hashes.contains(&collectable.content_hash));
    assert!(blocked.potential_delete_hashes.is_empty());
    assert!(blocked
        .blockers
        .contains(&"cold-payload-unscanned".to_owned()));
    assert!(!blocked.deletion_enabled);
}

#[test]
fn asset_gc_dry_run_retains_ambiguous_cross_generation_cold_keys() {
    use super::asset_object_catalog::AssetObjectRegistration;
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;

    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(b"{\"message\":[]}")
        .expect("encode cold fixture");
    let cold = cas.prepare_bytes(&encoder.finish().unwrap()).unwrap();
    let collectable = cas.prepare_bytes(b"cross-generation-candidate").unwrap();
    let generation = super::active_generation(&store.connection).expect("read generation");
    store
        .connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params![
                generation,
                serde_json::to_string(&json!({ "coldstorage": "shared-cold-key" })).unwrap()
            ],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO root (generation, value) VALUES ('retained-shadow', ?1)",
            [serde_json::to_string(&json!({ "coldstorage": "shared-cold-key" })).unwrap()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'shared-cold-key', ?2, ?3, '{}')",
            rusqlite::params![generation, cold.content_hash, cold.byte_size as i64],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES ('retained-shadow', 'shared-cold-key', NULL, 0, '{}')",
            [],
        )
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[
                AssetObjectRegistration {
                    object_hash: cold.content_hash.clone(),
                    byte_size: cold.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: collectable.content_hash.clone(),
                    byte_size: collectable.byte_size,
                },
            ],
            0,
        )
        .unwrap();

    let report = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;

    assert!(report.marked_hashes.contains(&cold.content_hash));
    assert!(report.marked_hashes.contains(&collectable.content_hash));
    assert!(report.potential_delete_hashes.is_empty());
    assert!(report
        .blockers
        .contains(&"cold-payload-unscanned".to_owned()));
    assert!(!report.deletion_enabled);
}

#[test]
fn asset_gc_dry_run_keeps_detached_export_roots_until_reader_release() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let original_payload = cas.prepare_bytes(b"detached-original").unwrap();
    let replacement_payload = cas.prepare_bytes(b"detached-replacement").unwrap();
    let original = AssetAlias {
        key: "assets/detached.bin".to_owned(),
        object_hash: Some(original_payload.content_hash.clone()),
        kind: "asset".to_owned(),
        size: original_payload.byte_size as i64,
        mime: "application/octet-stream".to_owned(),
        name: "Detached".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let first = store.commit_asset_alias(&original, 0).unwrap();
    let mut prepared = store.prepare_risu_save_export(first.revision).unwrap();
    let replacement = AssetAlias {
        object_hash: Some(replacement_payload.content_hash.clone()),
        size: replacement_payload.byte_size as i64,
        ..original
    };
    store
        .commit_asset_alias(&replacement, first.revision)
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[
                AssetObjectRegistration {
                    object_hash: original_payload.content_hash.clone(),
                    byte_size: original_payload.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: replacement_payload.content_hash.clone(),
                    byte_size: replacement_payload.byte_size,
                },
            ],
            0,
        )
        .expect("register GC candidates");

    let detached = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert!(detached
        .marked_hashes
        .contains(&original_payload.content_hash));
    assert!(detached
        .marked_hashes
        .contains(&replacement_payload.content_hash));
    assert!(detached.potential_delete_hashes.is_empty());

    let reader = prepared.take_reader().expect("take detached reader");
    prepared.release(reader).expect("release detached reader");
    let released = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert_eq!(
        released.marked_hashes,
        vec![replacement_payload.content_hash]
    );
    assert_eq!(
        released.potential_delete_hashes,
        vec![original_payload.content_hash]
    );
}

#[test]
fn detached_export_registry_keeps_exact_v8_roots_until_reader_release() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let generation = super::active_generation(&store.connection).expect("read generation");
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height, metadata
             ) VALUES (?1, 'assets/detached.bin', ?2, 'asset', 1,
                'application/octet-stream', 'Detached', 'bin', NULL, NULL, NULL, '{}')",
            rusqlite::params![generation, "ab".repeat(32)],
        )
        .expect("insert detached asset alias root");
    store
        .connection
        .execute(
            "INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
             ) VALUES (?1, 'root-module-assets', '0', 1, ?2, 1)",
            rusqlite::params![generation, "cd".repeat(32)],
        )
        .expect("insert detached owner head root");
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'cold/detached', ?2, 1, '{}')",
            rusqlite::params![generation, "ef".repeat(32)],
        )
        .expect("insert detached cold alias root");

    let mut prepared = store
        .prepare_risu_save_export(0)
        .expect("prepare detached reader with v8 roots");
    let roots = store
        .active_readers
        .detached_asset_roots()
        .expect("read detached roots");
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].manifest_hashes, ["cd".repeat(32)].into());
    assert_eq!(
        roots[0].object_hashes,
        ["ab".repeat(32), "ef".repeat(32)].into()
    );

    let reader = prepared.take_reader().expect("take detached reader");
    prepared.release(reader).expect("release detached reader");
    assert!(store
        .active_readers
        .detached_asset_roots()
        .expect("read roots after detached release")
        .is_empty());
}

#[test]
fn android_revision_reader_starts_writable_before_query_only_snapshot_pinning() {
    let flags = super::snapshot::revision_reader_open_flags_for_target(true);

    assert!(flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE));
    assert!(!flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY));
}

#[test]
fn checkpoints_accept_both_documented_modes() {
    let (_directory, store, _) = open_fixture();

    store
        .checkpoint(CheckpointMode::Passive)
        .expect("passive checkpoint");
    store
        .checkpoint(CheckpointMode::Truncate)
        .expect("truncate checkpoint");
}

#[test]
fn active_lease_rejects_truncate_and_final_release_truncates_the_wal() {
    let (directory, mut store, _) = open_fixture();
    store
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0")
        .expect("disable automatic checkpoints");
    let lease = store.acquire_revision(1).expect("acquire WAL reader");
    store
        .set_app_kv("after-lease", &json!(true))
        .expect("append WAL frame after lease");
    store
        .checkpoint(CheckpointMode::Passive)
        .expect("passive checkpoint with active lease");

    let started = std::time::Instant::now();
    let error = store
        .checkpoint(CheckpointMode::Truncate)
        .expect_err("truncate must reject an active lease");
    // Far above scheduler/disk jitter, comfortably below the 5s SQLite busy timeout:
    // proves the call rejected promptly instead of waiting out the busy handler.
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(matches!(
        error,
        StoreError::Store { message } if message.contains("active read lease")
    ));

    store
        .release_revision(&lease.lease)
        .expect("release final lease and truncate WAL");
    let database_path = directory.path().join("persistent/persistent.db");
    let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
    assert_eq!(fs::metadata(wal_path).expect("read final WAL").len(), 0);
}

#[test]
fn detached_export_reader_rejects_truncate_until_it_is_released() {
    let (directory, mut store, _) = open_fixture();
    store
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0")
        .expect("disable automatic checkpoints");
    let mut prepared = store
        .prepare_risu_save_export(1)
        .expect("prepare detached export reader");
    store
        .set_app_kv("after-detached-export", &json!(true))
        .expect("append WAL frame after detached reader");

    let started = std::time::Instant::now();
    let error = store
        .checkpoint(CheckpointMode::Truncate)
        .expect_err("truncate must reject a detached export reader");
    // Far above scheduler/disk jitter, comfortably below the 5s SQLite busy timeout.
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(matches!(
        error,
        StoreError::Store { message } if message.contains("active read lease")
    ));

    let reader = prepared.take_reader().expect("take detached export reader");
    prepared
        .release(reader)
        .expect("release detached export reader");
    let database_path = directory.path().join("persistent/persistent.db");
    let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
    assert_eq!(fs::metadata(wal_path).expect("read final WAL").len(), 0);
}

#[test]
fn detached_export_release_stays_prompt_while_an_attached_reader_remains() {
    let (directory, mut store, _) = open_fixture();
    store
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0")
        .expect("disable automatic checkpoints");
    let attached = store.acquire_revision(1).expect("acquire attached reader");
    let mut prepared = store
        .prepare_risu_save_export(1)
        .expect("prepare detached export reader");
    store
        .set_app_kv("after-two-readers", &json!(true))
        .expect("append WAL frame after both readers");

    let reader = prepared.take_reader().expect("take detached export reader");
    let started = std::time::Instant::now();
    prepared
        .release(reader)
        .expect("release detached reader with attached reader remaining");
    // Far above scheduler/disk jitter, comfortably below the 5s SQLite busy timeout.
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        store
            .read_root(Some(&attached.lease))
            .expect("attached reader remains pinned")
            .revision,
        1
    );
    let database_path = directory.path().join("persistent/persistent.db");
    let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
    assert!(fs::metadata(&wal_path).expect("read pinned WAL").len() > 0);

    store
        .release_revision(&attached.lease)
        .expect("release final attached reader");
    assert_eq!(fs::metadata(wal_path).expect("read final WAL").len(), 0);
}

#[test]
fn dropping_store_with_active_lease_reopens_latest_state_and_truncates_recovered_wal() {
    let (directory, mut store, _) = open_fixture();
    store
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0")
        .expect("disable automatic checkpoints");
    let lease = store.acquire_revision(1).expect("acquire WAL reader");
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Writer survives lease drop" })),
            ..empty_working_set_commit(1)
        })
        .expect("append writer state while lease is active");
    let database_path = directory.path().join("persistent/persistent.db");
    let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
    assert!(fs::metadata(&wal_path).expect("read pinned WAL").len() > 0);
    drop(store);

    let reopened = PersistentStore::open(directory.path()).expect("reopen after active lease drop");
    assert_eq!(reopened.revision().expect("read reopened revision"), 2);
    assert_eq!(
        reopened
            .read_root(None)
            .expect("read reopened writer state")
            .value["username"],
        "Writer survives lease drop"
    );
    assert!(matches!(
        reopened.read_root(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
    assert_eq!(
        fs::metadata(&wal_path).expect("read recovered WAL").len(),
        0
    );
}

fn sparse_snapshot(path: &Path, bytes: u64) {
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .expect("create sparse snapshot");
    file.set_len(bytes).expect("size sparse snapshot");
    thread::sleep(Duration::from_millis(10));
}

#[test]
fn ninth_snapshot_removes_the_oldest_and_leaves_eight() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    let mut created = Vec::new();

    for index in 0..9 {
        created.push(
            store
                .snapshot_create(&format!("rotation-{index}"))
                .expect("create rotating snapshot")
                .path,
        );
        thread::sleep(Duration::from_millis(10));
    }

    let listed = store.snapshot_list().expect("list rotated snapshots");
    assert_eq!(listed.len(), 8);
    assert!(!Path::new(&created[0]).exists());
    assert!(
        !crate::asset_repository::migration_gc::snapshot_asset_root_sidecar_path(Path::new(
            &created[0]
        ))
        .exists()
    );
    assert!(created[1..].iter().all(|path| Path::new(path).is_file()));
    assert!(created[1..].iter().all(|path| {
        crate::asset_repository::migration_gc::snapshot_asset_root_sidecar_path(Path::new(path))
            .is_file()
    }));
}

#[test]
fn byte_rotation_uses_the_512_mib_floor_and_removes_oldest_first() {
    const MIB: u64 = 1024 * 1024;
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    let snapshots = snapshots_dir(&directory);
    let mut sparse = Vec::new();
    for index in 0..6 {
        let path = snapshots.join(format!("persistent-sparse-{index}.db"));
        sparse_snapshot(&path, 100 * MIB);
        sparse.push(path);
    }

    let created = store
        .snapshot_create("floor-rotation")
        .expect("create snapshot and rotate");
    let listed = store.snapshot_list().expect("list floor-rotated snapshots");
    let total: u64 = listed.iter().map(|snapshot| snapshot.bytes).sum();

    assert!(total <= 512 * MIB);
    assert!(!sparse[0].exists());
    assert!(sparse[1..].iter().all(|path| path.is_file()));
    assert!(Path::new(&created.path).is_file());
}

#[test]
fn byte_rotation_uses_four_times_current_logical_database_size() {
    const MIB: u64 = 1024 * 1024;
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    store
        .connection
        .execute_batch(
            "CREATE TABLE rotation_payload (value BLOB);\n             INSERT INTO rotation_payload VALUES (zeroblob(140 * 1024 * 1024));",
        )
        .expect("grow logical database above the rotation floor branch");
    let page_count: i64 = store
        .connection
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .expect("read page count");
    let page_size: i64 = store
        .connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .expect("read page size");
    let logical_bytes = (page_count * page_size) as u64;
    assert!(logical_bytes * 4 > 512 * MIB);

    let snapshots = snapshots_dir(&directory);
    let sparse_bytes = logical_bytes * 2 / 3;
    let mut sparse = Vec::new();
    for index in 0..5 {
        let path = snapshots.join(format!("persistent-large-sparse-{index}.db"));
        sparse_snapshot(&path, sparse_bytes);
        sparse.push(path);
    }

    let created = store
        .snapshot_create("large-rotation")
        .expect("create snapshot and rotate using logical size");
    let listed = store.snapshot_list().expect("list size-rotated snapshots");
    let total: u64 = listed.iter().map(|snapshot| snapshot.bytes).sum();

    assert!(total <= logical_bytes * 4);
    assert!(total > 512 * MIB);
    assert!(!sparse[0].exists());
    assert!(sparse[1..].iter().all(|path| path.is_file()));
    assert!(Path::new(&created.path).is_file());
}

#[test]
fn pending_restore_reopens_cleanly_after_the_store_drops_an_active_lease() {
    let (directory, mut store, database) = open_fixture();
    let lease = store
        .acquire_revision(1)
        .expect("acquire pre-restore lease");
    let snapshot = store
        .snapshot_create("lease-restore")
        .expect("create snapshot while lease is active");
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Writer after restore snapshot" })),
            ..empty_working_set_commit(1)
        })
        .expect("commit after restore snapshot");
    store
        .snapshot_restore_request(Path::new(&snapshot.path))
        .expect("prepare restore while lease is active");
    drop(store);

    let restored = PersistentStore::open(directory.path()).expect("apply pending restore");
    assert_eq!(restored.revision().expect("read restored revision"), 1);
    assert_eq!(
        restored
            .materialize(None)
            .expect("materialize restored data"),
        database
    );
    assert!(matches!(
        restored.read_root(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn pending_restore_target_and_pre_restore_snapshot_survive_rotation() {
    let (directory, mut store, database) = open_fixture();
    let target = store
        .snapshot_create("restore-target")
        .expect("create restore target");
    for index in 0..7 {
        store
            .snapshot_create(&format!("fill-{index}"))
            .expect("fill snapshot rotation");
        thread::sleep(Duration::from_millis(10));
    }
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Current before restore" })),
            ..empty_working_set_commit(1)
        })
        .expect("change current data");
    store
        .snapshot_restore_request(Path::new(&target.path))
        .expect("request restore");
    drop(store);

    let restored = PersistentStore::open(directory.path()).expect("apply pending restore");
    assert_eq!(
        restored
            .materialize(None)
            .expect("materialize restored data"),
        database
    );
    assert!(Path::new(&target.path).is_file());
    assert!(!snapshots_dir(&directory)
        .join("pending-restore.json")
        .exists());
    assert!(
        restored
            .snapshot_list()
            .expect("list rotated restore snapshots")
            .len()
            <= 8
    );

    let pre_restore = restored
        .snapshot_list()
        .expect("list restore snapshots")
        .into_iter()
        .find(|snapshot| snapshot.path.contains("pre-restore"))
        .expect("pre-restore snapshot remains after rotation");
    let connection =
        rusqlite::Connection::open(pre_restore.path).expect("open pre-restore snapshot");
    let value: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-1'",
            [],
            |row| row.get(0),
        )
        .expect("read pre-restore root");
    assert_eq!(
        serde_json::from_str::<Value>(&value).expect("parse root")["username"],
        "Current before restore"
    );
}

#[test]
fn invalid_restore_candidates_preserve_current_data_and_marker() {
    for wrong_version in [false, true] {
        let (directory, mut store, database) = open_fixture();
        store
            .commit(&WorkingSetCommit {
                root: Some(json!({ "username": "Current protected data" })),
                ..empty_working_set_commit(1)
            })
            .expect("change current data");
        let expected = store.materialize(None).expect("materialize current data");
        assert_ne!(expected, database);
        let candidate = snapshots_dir(&directory).join(if wrong_version {
            "persistent-wrong-version.db"
        } else {
            "persistent-corrupt.db"
        });
        if wrong_version {
            let connection =
                rusqlite::Connection::open(&candidate).expect("create wrong-version database");
            connection
                .execute_batch("PRAGMA user_version = 17;")
                .expect("set wrong schema version");
        } else {
            fs::write(&candidate, b"not a sqlite database").expect("write corrupt database");
        }
        store
            .snapshot_restore_request(&candidate)
            .expect("write pending marker");
        drop(store);

        let reopened =
            PersistentStore::open(directory.path()).expect("reopen after rejected restore");
        assert_eq!(
            reopened
                .materialize(None)
                .expect("read preserved current data"),
            expected
        );
        assert!(snapshots_dir(&directory)
            .join("pending-restore.json")
            .exists());
    }
}
