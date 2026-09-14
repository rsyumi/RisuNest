use super::super::snapshot_archive::Archive;
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
    assert_eq!(integer_pragma("user_version"), 2);
}

#[test]
fn snapshots_create_list_and_restore_on_reopen() {
    let (directory, mut store, database) = open_fixture();
    let snapshot = store
        .snapshot_create("contract-test")
        .expect("create snapshot");
    assert!(store
        .snapshot_list()
        .unwrap()
        .iter()
        .any(|s| s.id == snapshot.id));
    assert!(snapshot.bytes > 0);
    assert_eq!(store.snapshot_list().expect("list snapshots").len(), 1);

    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Changed after snapshot" })),
            ..empty_working_set_commit(1)
        })
        .expect("change database after snapshot");
    store
        .snapshot_restore_request(&snapshot.id)
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
    assert!(restored
        .snapshot_list()
        .unwrap()
        .iter()
        .any(|s| s.id == snapshot.id));
    assert_eq!(
        Archive::open(&restored.snapshots_dir)
            .unwrap()
            .pending_restore()
            .unwrap(),
        None
    );
}

#[test]
fn source_preservation_snapshot_keeps_invalid_json_and_retains_objects() {
    let (_directory, store, _) = open_fixture();
    store
        .connection
        .execute("UPDATE root SET value='synthetic invalid JSON'", [])
        .unwrap();
    assert!(store.materialize(None).is_err());
    let snapshot = store.snapshot_create("preserve-invalid-json").unwrap();
    let (_capture, connection) = reconstruct_snapshot(&store, &snapshot.id);
    let value: String = connection
        .query_row("SELECT value FROM root LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "synthetic invalid JSON");
    let metadata = Archive::open(&store.snapshots_dir)
        .unwrap()
        .metadata(&snapshot.id)
        .unwrap();
    assert!(metadata.roots.retain_all_objects);
    assert!(store.materialize(None).is_err());
}

#[test]
fn snapshot_delete_requires_a_listed_id_and_removes_its_roots() {
    let (_directory, store, _) = open_fixture();
    let created = store.snapshot_create("delete-test").unwrap();
    store.snapshot_delete(&created.id).unwrap();
    assert!(store.snapshot_list().unwrap().is_empty());
    assert!(Archive::open(&store.snapshots_dir)
        .unwrap()
        .roots()
        .unwrap()
        .is_empty());
    assert!(store.snapshot_delete("../not-a-snapshot.db").is_err());
    assert!(store
        .snapshot_restore_request("../not-a-snapshot.db")
        .is_err());
    assert!(store.snapshot_delete(&created.id).is_err());
}

#[test]
fn snapshot_delete_rejects_a_pending_restore_target_without_removing_it() {
    let (_directory, store, _) = open_fixture();
    let created = store.snapshot_create("pending-delete").unwrap();
    store.snapshot_restore_request(&created.id).unwrap();
    let error = store.snapshot_delete(&created.id).unwrap_err();
    assert!(error.to_string().contains("pending restore"));
    assert_eq!(store.snapshot_list().unwrap().len(), 1);
    assert_eq!(
        Archive::open(&store.snapshots_dir)
            .unwrap()
            .pending_restore()
            .unwrap()
            .as_deref(),
        Some(created.id.as_str())
    );
}

#[cfg(any(windows, unix))]
#[test]
fn snapshot_archive_rejects_linked_database() {
    let (directory, store, _) = open_fixture();
    let external = directory.path().join("external.sqlite");
    fs::write(&external, b"external synthetic bytes").unwrap();
    let linked = store.snapshots_dir.join("snapshots.sqlite");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&external, &linked).unwrap();
    #[cfg(windows)]
    match std::os::windows::fs::symlink_file(&external, &linked) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("Windows symlink integration unavailable: {error}");
            return;
        }
        Err(error) => panic!("create symlink: {error}"),
    }
    assert!(store.snapshot_list().is_err());
    assert_eq!(fs::read(&external).unwrap(), b"external synthetic bytes");
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
    let metadata = Archive::open(&store.snapshots_dir)
        .unwrap()
        .metadata(&snapshot.id)
        .unwrap();

    assert_eq!(metadata.revision, 1);
    assert_eq!(metadata.roots.manifest_hashes, [manifest_hash].into());
    assert_eq!(
        metadata.roots.object_hashes,
        [object_hash, cold_object_hash].into()
    );
    assert_eq!(
        metadata.roots.legacy_asset_keys,
        [
            "assets/exact.bin".to_owned(),
            "assets/missing.bin".to_owned()
        ]
        .into()
    );
    assert_eq!(metadata.roots.inlay_ids, ["kept-inlay".to_owned()].into());
    assert_eq!(metadata.roots.cold_keys, ["cold-chat".to_owned()].into());
    assert_eq!(
        metadata.roots.blockers,
        [
            "cold-payload-unscanned".to_owned(),
            "plugin-storage-opaque".to_owned()
        ]
        .into()
    );
    assert!(metadata.roots.retain_all_objects);
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
fn renderer_session_release_closes_every_attached_lease_and_truncates_the_wal() {
    let (directory, mut store, _) = open_fixture();
    store
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0")
        .expect("disable automatic checkpoints");
    let first = store.acquire_revision(1).expect("acquire first reader");
    let second = store.acquire_revision(1).expect("acquire second reader");
    store
        .set_app_kv("after-session-readers", &json!(true))
        .expect("append WAL frame after readers");

    store
        .release_all_revision_leases()
        .expect("release renderer session readers");

    assert_eq!(store.active_readers.active_count(), 0);
    assert!(matches!(
        store.read_root(Some(&first.lease)),
        Err(StoreError::SnapshotReleased)
    ));
    assert!(matches!(
        store.read_root(Some(&second.lease)),
        Err(StoreError::SnapshotReleased)
    ));
    let database_path = directory.path().join("persistent/persistent.db");
    let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
    assert_eq!(fs::metadata(wal_path).expect("read final WAL").len(), 0);
}

#[test]
fn attached_export_can_leave_the_store_lock_and_restore_the_same_lease() {
    let (_directory, mut store, _) = open_fixture();
    let lease = store.acquire_revision(1).expect("acquire export reader");

    let prepared = store
        .detach_risu_save_export(&lease.lease)
        .expect("detach export reader");
    assert!(matches!(
        store.read_root(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
    assert_eq!(prepared.reader().unwrap().target.revision, 1);
    let exported = prepared
        .create_attached_export(false)
        .expect("export through detached attached reader");

    store
        .reattach_risu_save_export(prepared)
        .expect("reattach export reader");
    assert_eq!(store.read_root(Some(&lease.lease)).unwrap().revision, 1);
    store
        .cleanup_risu_save_export(Path::new(&exported.path))
        .unwrap();
    store.release_revision(&lease.lease).unwrap();
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
fn renderer_session_release_does_not_close_a_detached_native_job_reader() {
    let (_directory, mut store, _) = open_fixture();
    let mut prepared = store
        .prepare_risu_save_export(1)
        .expect("prepare detached native job reader");
    let attached = store.acquire_revision(1).expect("acquire renderer reader");

    store
        .release_all_revision_leases()
        .expect("release renderer readers");

    assert_eq!(store.active_readers.active_count(), 1);
    assert!(matches!(
        store.read_root(Some(&attached.lease)),
        Err(StoreError::SnapshotReleased)
    ));
    assert_eq!(prepared.reader().unwrap().target.revision, 1);
    prepared.release_reader().unwrap();
    assert_eq!(store.active_readers.active_count(), 0);
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

#[test]
fn ninth_snapshot_removes_the_oldest_and_leaves_eight() {
    let (_directory, store, _) = open_fixture();
    let mut ids = Vec::new();
    for index in 0..9 {
        ids.push(
            store
                .snapshot_create(&format!("rotation-{index}"))
                .unwrap()
                .id,
        );
        thread::sleep(Duration::from_millis(10));
    }
    let listed = store.snapshot_list().unwrap();
    assert_eq!(listed.len(), 8);
    assert!(!listed.iter().any(|s| s.id == ids[0]));
    for id in &ids[1..] {
        let (_capture, db) = reconstruct_snapshot(&store, id);
        assert_eq!(
            db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }
}

#[test]
fn byte_rotation_counts_shared_storage_and_preserves_pending_and_newest() {
    let (_directory, store, _) = open_fixture();
    let target = store.snapshot_create("target").unwrap();
    store.snapshot_restore_request(&target.id).unwrap();
    let middle = store.snapshot_create("middle").unwrap();
    let latest = store.snapshot_create("latest").unwrap();
    let mut archive = Archive::open(&store.snapshots_dir).unwrap();
    archive.rotate(0, &latest.id).unwrap();
    let ids: Vec<_> = archive.list().unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&target.id));
    assert!(ids.contains(&latest.id));
    assert!(!ids.contains(&middle.id));
}

#[test]
fn retention_budget_keeps_the_documented_floor_and_logical_database_multiplier() {
    const MIB: u64 = 1024 * 1024;
    assert_eq!(snapshot::byte_budget(0), 512 * MIB);
    assert_eq!(snapshot::byte_budget(128 * MIB), 512 * MIB);
    assert_eq!(snapshot::byte_budget(140 * MIB), 560 * MIB);
    assert_eq!(snapshot::byte_budget(u64::MAX), u64::MAX);
}

#[test]
#[ignore = "actual-schema synthetic storage measurements"]
fn snapshot_deduplication_actual_schema_measurements() {
    use std::collections::HashMap;
    use std::time::Instant;
    for scenario in [
        "unchanged",
        "append",
        "grow",
        "delete",
        "replace-generation",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let mut database = fixture();
        let mut seed = 614_u64;
        let messages: Vec<_> = (0..4096).map(|index| {
            let data: String = (0..1024).map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                char::from(b'a' + ((seed >> 32) % 26) as u8)
            }).collect();
            json!({"role":"char", "data":data, "time":index, "chatId":format!("synthetic-{index}")})
        }).collect();
        database["characters"][0]["chats"][0]["message"] = Value::Array(messages);
        let import = |store: &mut PersistentStore, value: &Value| {
            let revision = store.revision().unwrap();
            let staging = store.replace_begin().unwrap();
            store
                .replace_put_root(&staging.staging_id, &staged_root(value))
                .unwrap();
            store
                .replace_put_presets(&staging.staging_id, value["botPresets"].as_array().unwrap())
                .unwrap();
            store
                .replace_add_characters(
                    &staging.staging_id,
                    value["characters"].as_array().unwrap(),
                )
                .unwrap();
            store
                .replace_commit(&staging.staging_id, Some(revision))
                .unwrap();
        };
        import(&mut store, &database);
        let mut expected = HashMap::new();
        let mut create_ms = Vec::new();
        let mut restore_ms = Vec::new();
        for iteration in 0..8 {
            if iteration > 0 && scenario != "unchanged" {
                if scenario == "replace-generation" {
                    database["username"] = json!(format!("synthetic revision {iteration}"));
                    import(&mut store, &database);
                } else {
                    let character_id = database["characters"][0]["chaId"].as_str().unwrap();
                    let chat_id = database["characters"][0]["chats"][0]["id"]
                        .as_str()
                        .unwrap();
                    let generation = active_generation(&store.connection).unwrap();
                    match scenario {
                        "append" => {
                            let index: i64 = store.connection.query_row("SELECT coalesce(max(message_index),-1)+1 FROM messages WHERE generation=?1 AND character_id=?2 AND conversation_id=?3", params![generation,character_id,chat_id], |r| r.get(0)).unwrap();
                            store.connection.execute("INSERT INTO messages(generation,character_id,conversation_id,message_index,message_id,value) VALUES(?1,?2,?3,?4,?5,?6)",
                                params![generation,character_id,chat_id,index,format!("append-{iteration}"),json!({"role":"char","data":"synthetic appended message"}).to_string()]).unwrap();
                        }
                        "grow" => {
                            store.connection.execute("UPDATE messages SET value=json_set(value,'$.data',json_extract(value,'$.data')||?4) WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 AND message_index=2000",params![generation,character_id,chat_id,"x".repeat(120)]).unwrap();
                        }
                        "delete" => {
                            store.connection.execute("DELETE FROM messages WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 AND message_index>=?4 AND message_index<?5",params![generation,character_id,chat_id,(iteration-1)*100,iteration*100]).unwrap();
                        }
                        _ => unreachable!(),
                    }
                    store.connection.execute("UPDATE conversations SET message_count=(SELECT count(*) FROM messages m WHERE m.generation=conversations.generation AND m.character_id=conversations.character_id AND m.conversation_id=conversations.conversation_id) WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,character_id,chat_id]).unwrap();
                    store
                        .commit(&WorkingSetCommit {
                            root: Some(json!({"username":format!("synthetic {iteration}")})),
                            ..empty_working_set_commit(store.revision().unwrap())
                        })
                        .unwrap();
                }
            }
            expected.insert(
                store.revision().unwrap(),
                Sha256::digest(serde_json::to_vec(&store.materialize(None).unwrap()).unwrap())
                    .to_vec(),
            );
            let start = Instant::now();
            store.snapshot_create(scenario).unwrap();
            create_ms.push(start.elapsed().as_millis());
        }
        let snapshots = store.snapshot_list().unwrap();
        let logical: u64 = snapshots.iter().map(|s| s.bytes).sum();
        let stored = store.storage_stats().unwrap().snapshot_bytes;
        let unique: i64 = Connection::open(store.snapshots_dir.join("snapshots.sqlite"))
            .unwrap()
            .query_row("SELECT sum(length(data)) FROM chunks", [], |r| r.get(0))
            .unwrap();
        for entry in &snapshots {
            let start = Instant::now();
            let (_capture, connection) = reconstruct_snapshot(&store, &entry.id);
            let actual = super::super::query::materialize(&connection, None).unwrap();
            assert_eq!(
                Sha256::digest(serde_json::to_vec(&actual).unwrap()).to_vec(),
                expected[&current_revision(&connection).unwrap()]
            );
            restore_ms.push(start.elapsed().as_millis());
        }
        assert!(stored < logical, "archive must save space on {scenario}");
        if scenario == "unchanged" {
            assert!(unique as u64 <= snapshots[0].bytes);
        }
        store
            .snapshot_delete(&snapshots.last().unwrap().id)
            .unwrap();
        for entry in store.snapshot_list().unwrap() {
            let (_capture, connection) = reconstruct_snapshot(&store, &entry.id);
            let actual = super::super::query::materialize(&connection, None).unwrap();
            assert_eq!(
                Sha256::digest(serde_json::to_vec(&actual).unwrap()).to_vec(),
                expected[&current_revision(&connection).unwrap()]
            );
        }
        eprintln!(
            "snapshot-dedup-measurement {}",
            json!({"scenario":scenario,"snapshots":snapshots.len(),"logicalBytes":logical,"archiveBytes":stored,"uniquePayloadBytes":unique,"createMs":create_ms,"restoreAndMaterializeMs":restore_ms})
        );
    }
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
        .snapshot_restore_request(&snapshot.id)
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
        .snapshot_restore_request(&target.id)
        .expect("request restore");
    drop(store);

    let restored = PersistentStore::open(directory.path()).expect("apply pending restore");
    assert_eq!(
        restored
            .materialize(None)
            .expect("materialize restored data"),
        database
    );
    assert!(restored
        .snapshot_list()
        .unwrap()
        .iter()
        .any(|s| s.id == target.id));
    assert_eq!(
        Archive::open(&restored.snapshots_dir)
            .unwrap()
            .pending_restore()
            .unwrap(),
        None
    );
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
        .find(|snapshot| snapshot.reason == "pre-restore")
        .expect("pre-restore snapshot remains after rotation");
    let (_capture, connection) = reconstruct_snapshot(&restored, &pre_restore.id);
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
        let (directory, mut store, _) = open_fixture();
        store
            .commit(&WorkingSetCommit {
                root: Some(json!({"username":"Current protected data"})),
                ..empty_working_set_commit(1)
            })
            .unwrap();
        let expected = store.materialize(None).unwrap();
        let id = {
            let mut archive = Archive::open(&store.snapshots_dir).unwrap();
            let scratch = archive.scratch().unwrap();
            if wrong_version {
                let connection = Connection::open(&scratch.path).unwrap();
                connection.execute_batch("PRAGMA user_version=17;").unwrap();
            } else {
                fs::write(&scratch.path, b"not a sqlite database").unwrap();
            }
            let metadata = archive
                .insert(&scratch.path, 1, "invalid", Default::default())
                .unwrap();
            archive.request_restore(&metadata.id).unwrap();
            metadata.id
        };
        drop(store);
        let reopened = PersistentStore::open(directory.path()).unwrap();
        assert_eq!(reopened.materialize(None).unwrap(), expected);
        assert_eq!(
            Archive::open(&reopened.snapshots_dir)
                .unwrap()
                .pending_restore()
                .unwrap(),
            Some(id)
        );
    }
}
