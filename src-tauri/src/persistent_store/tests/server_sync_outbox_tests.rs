use super::super::{server_sync_outbox as outbox, RootMutation};
use super::*;

fn bind(store: &PersistentStore) {
    store
        .connection
        .execute(
            "INSERT INTO server_sync_state(singleton,config,full_scan) VALUES(1,'{}',0)",
            [],
        )
        .unwrap();
}

#[test]
#[ignore = "Explicit local generation-copy, outbox and 10 MiB JSON cost comparison"]
fn server_sync_local_commit_and_large_json_costs() {
    let mut measurements = Vec::new();
    for (enabled, lease_pinned) in [(false, false), (true, false), (false, true), (true, true)] {
        let (directory, mut store, database) = open_fixture();
        let seed = database["characters"][0].clone();
        let initial_count = database["characters"].as_array().unwrap().len();
        let mut characters = database["characters"].as_array().unwrap().clone();
        characters.extend((initial_count..500).map(|i| {
            let mut detail = seed.clone();
            detail["chaId"] = json!(format!("synthetic-character-{i:04}"));
            detail
        }));
        let staging = store.replace_begin().unwrap();
        store
            .replace_put_root(&staging.staging_id, &staged_root(&database))
            .unwrap();
        store
            .replace_put_presets(
                &staging.staging_id,
                database["botPresets"].as_array().unwrap(),
            )
            .unwrap();
        store
            .replace_add_characters(&staging.staging_id, &characters)
            .unwrap();
        store
            .replace_commit(&staging.staging_id, Some(store.revision().unwrap()))
            .unwrap();
        store
            .commit(&WorkingSetCommit {
                plugin_storage: Some(vec![PluginStorageMutation::Set {
                    key: "synthetic-large".into(),
                    value: json!("x".repeat(10 * 1024 * 1024)),
                }]),
                ..empty_working_set_commit(store.revision().unwrap())
            })
            .unwrap();
        if enabled {
            bind(&store);
        }
        let mut samples = Vec::new();
        for n in 0..9 {
            let mut root = store.read_root(None).unwrap().value;
            root["username"] = json!(format!("synthetic-{n}"));
            let lease =
                lease_pinned.then(|| store.acquire_revision(store.revision().unwrap()).unwrap());
            let start = std::time::Instant::now();
            store
                .commit(&WorkingSetCommit {
                    root: Some(root),
                    ..empty_working_set_commit(store.revision().unwrap())
                })
                .unwrap();
            samples.push(start.elapsed().as_micros());
            if let Some(lease) = lease {
                store.release_revision(&lease.lease).unwrap();
            }
            let dirty = outbox::dirty_page(&store.connection, None, 1024).unwrap();
            assert_eq!(dirty.len(), usize::from(enabled));
            if enabled {
                assert_eq!(dirty[0].kind, "root");
            }
        }
        samples.sort();
        let generation = active_generation(&store.connection).unwrap();
        let count: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM characters WHERE generation=?1",
                [&generation],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 500);
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = crate::server_sync::cache::Cache::open(cache_dir.path()).unwrap();
        let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
        let start = std::time::Instant::now();
        let payload = super::super::server_sync_projection::project(
            &store.connection,
            &cas,
            &generation,
            &outbox::ServerDirtyKey {
                kind: "plugin".into(),
                key1: "synthetic-large".into(),
                key2: "".into(),
                revision: store.revision().unwrap(),
            },
        )
        .unwrap()
        .unwrap();
        let read_parse_us = start.elapsed().as_micros();
        let start = std::time::Instant::now();
        let projected = cache
            .project(&payload, &[], &[], vec!["plugin-storage".into()])
            .unwrap();
        let encode_cache_us = start.elapsed().as_micros();
        let start = std::time::Instant::now();
        let restored = cache.restore(&projected.version).unwrap();
        let restore_us = start.elapsed().as_micros();
        assert_eq!(
            serde_json::to_vec(&restored.0).unwrap(),
            serde_json::to_vec(&payload).unwrap()
        );
        measurements.push(json!({"outboxEnabled":enabled,"leasePinned":lease_pinned,"characters":count,"pluginBytes":10*1024*1024,"commitMedianUs":samples[4],"readParseUs":read_parse_us,"encodeAndCacheUs":encode_cache_us,"restoreUs":restore_us}));
    }
    eprintln!(
        "local costs: {}",
        serde_json::to_string(&measurements).unwrap()
    );
}
#[test]
fn incremental_outbox_is_atomic_sparse_across_generation_copy_and_tail_ack() {
    let (_dir, mut store, _) = open_fixture();
    bind(&store);
    let lease = store.acquire_revision(1).unwrap();
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({"username":"synthetic changed"})),
            ..empty_working_set_commit(1)
        })
        .unwrap();
    let sent = outbox::dirty_page(&store.connection, None, 1024).unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].kind, "root");
    assert_eq!(sent[0].revision, 2);
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({"username":"synthetic newer"})),
            ..empty_working_set_commit(2)
        })
        .unwrap();
    let tx = store.connection.transaction().unwrap();
    outbox::acknowledge_keys(&tx, &sent).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        outbox::dirty_page(&store.connection, None, 1024).unwrap()[0].revision,
        3
    );
    store.connection.execute_batch("CREATE TRIGGER synthetic_commit_failure BEFORE UPDATE ON meta WHEN NEW.key='currentRevision' BEGIN SELECT RAISE(ABORT,'synthetic'); END").unwrap();
    assert!(store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "uncommitted".into(),
                value: json!(true)
            }]),
            ..empty_working_set_commit(3)
        })
        .is_err());
    assert_eq!(
        outbox::dirty_page(&store.connection, None, 1024)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>("SELECT count(*) FROM server_sync_context", [], |r| r.get(0))
            .unwrap(),
        0
    );
    store
        .connection
        .execute_batch("DROP TRIGGER synthetic_commit_failure")
        .unwrap();
    store.release_revision(&lease.lease).unwrap();
}

#[test]
fn plugin_clear_keeps_original_membership_and_scope_then_local_set_remains_dirty() {
    let (_dir, mut store, _) = open_fixture();
    bind(&store);
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![
                PluginStorageMutation::Set {
                    key: "first".into(),
                    value: json!(1),
                },
                PluginStorageMutation::Set {
                    key: "second".into(),
                    value: json!(2),
                },
            ]),
            ..empty_working_set_commit(1)
        })
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO server_sync_scope_base VALUES('plugin-storage',?1)",
            ["a".repeat(64)],
        )
        .unwrap();
    let original: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM plugin_storage WHERE generation=?1",
            [active_generation(&store.connection).unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![
                PluginStorageMutation::Clear,
                PluginStorageMutation::Set {
                    key: "later".into(),
                    value: json!(3),
                },
            ]),
            ..empty_working_set_commit(2)
        })
        .unwrap();
    let captured: i64 = store
        .connection
        .query_row("SELECT count(*) FROM server_sync_clear_members", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(captured, original);
    let version: String = store
        .connection
        .query_row("SELECT expected_version FROM server_sync_clears", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(version, "a".repeat(64));
    assert!(outbox::dirty_page(&store.connection, None, 1024)
        .unwrap()
        .iter()
        .any(|k| k.kind == "plugin" && k.key1 == "later"));
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM server_sync_clear_members WHERE key='later'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn server_outbox_tracks_each_public_record_mutation_and_alias_deletion() {
    let (directory, mut store, database) = open_fixture();
    store
        .activate_cold_payload_migration(&ColdPayloadMigrationInput {
            source_revision: store.revision().unwrap(),
            migration_id: "synthetic-cold".into(),
            compatibility_hash: "a".repeat(64),
            cold_aliases: vec![],
        })
        .unwrap();
    bind(&store);
    let mut character = database["characters"][0].clone();
    character.as_object_mut().unwrap().shift_remove("chats");
    character["name"] = json!("synthetic updated character");
    let mut presets = database["botPresets"].as_array().unwrap().clone();
    presets[0]["name"] = json!("synthetic updated preset");
    store
        .commit(&WorkingSetCommit {
            root_mutations: Some(vec![RootMutation::Set {
                key: "username".into(),
                value: json!("synthetic"),
            }]),
            replace_presets: Some(presets),
            character_details: Some(vec![character]),
            conversations: Some(vec![ConversationMutation::ReplaceRange {
                character_id: "char-a".into(),
                conversation_id: "conv-long".into(),
                start: 0,
                delete_count: 0,
                messages: vec![json!({"role":"user","data":"synthetic","chatId":null})],
                conversation: None,
                configured_index: None,
            }]),
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "공용/🦀".into(),
                value: json!({"empty":{},"value":null}),
            }]),
            ..empty_working_set_commit(store.revision().unwrap())
        })
        .unwrap();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let object = cas.prepare_bytes(b"synthetic shared blob").unwrap();
    for kind in ["asset", "inlay"] {
        let alias = AssetAlias {
            key: "synthetic/shared".into(),
            object_hash: Some(object.content_hash.clone()),
            kind: kind.into(),
            size: object.byte_size as i64,
            mime: "application/octet-stream".into(),
            name: "synthetic".into(),
            ext: "bin".into(),
            inlay_type: (kind == "inlay").then(|| "image".into()),
            width: None,
            height: None,
            metadata: json!({}),
        };
        store
            .commit_asset_alias(&alias, store.revision().unwrap())
            .unwrap();
    }
    let cold = ColdAlias {
        key: "synthetic/cold".into(),
        object_hash: Some(object.content_hash),
        size: object.byte_size as i64,
        metadata: json!({}),
    };
    store
        .commit_cold_alias(&cold, store.revision().unwrap())
        .unwrap();
    let dirty = outbox::dirty_page(&store.connection, None, 1024).unwrap();
    let families = dirty
        .iter()
        .map(|key| key.kind.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for family in [
        "root",
        "preset",
        "character",
        "conversation",
        "plugin",
        "asset",
        "inlay",
        "cold",
    ] {
        assert!(
            families.contains(family),
            "missing mutation family {family}"
        );
    }
    let tx = store.connection.transaction().unwrap();
    outbox::acknowledge_keys(&tx, &dirty).unwrap();
    tx.commit().unwrap();
    for kind in ["asset", "inlay"] {
        store
            .delete_asset_alias(kind, "synthetic/shared", store.revision().unwrap())
            .unwrap();
    }
    store
        .delete_cold_alias("synthetic/cold", store.revision().unwrap())
        .unwrap();
    let dirty = outbox::dirty_page(&store.connection, None, 1024).unwrap();
    assert_eq!(
        dirty
            .iter()
            .map(|key| key.kind.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        ["asset", "inlay", "cold"].into_iter().collect()
    );
    assert!(store
        .read_asset_alias("asset", "synthetic/shared", None)
        .unwrap()
        .is_none());
    assert!(store
        .read_asset_alias("inlay", "synthetic/shared", None)
        .unwrap()
        .is_none());
    assert!(store
        .read_cold_alias("synthetic/cold", None)
        .unwrap()
        .is_none());
}

#[test]
fn replacement_restore_preserves_server_replica_reconciliation() {
    let (_dir, mut store, _) = open_fixture();
    bind(&store);
    let staged = stage_root(&mut store, "synthetic replacement");
    store.replace_commit(&staged, Some(1)).unwrap();
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>("SELECT full_scan FROM server_sync_state", [], |r| r.get(0))
            .unwrap(),
        1
    );
    outbox::restored_copy(&store.connection).unwrap();
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT registration_required FROM server_sync_state",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        1
    );
}
