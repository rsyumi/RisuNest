use super::*;
use super::super::{lww, message_pages};
use crate::data_health::repair::RepairCandidate;
use crate::persistent_store::active_generation;
use risunest_sync_wire::{
    stamp::Stamp,
    unit::{UnitKey, UnitValue},
};
use serde_json::json;

fn candidate(action: RepairAction) -> RepairCandidate {
    RepairCandidate {
        id: "0:test".to_owned(),
        action,
        finding: 0,
        preferred: true,
        discards: false,
    }
}

/// A library whose root points at an asset it does not register and at one it does, so a repair
/// has something real to remove and something it must leave alone.
fn fixture() -> (tempfile::TempDir, PersistentStore) {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let staging = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(
            &staging,
            &json!({
                "userIcon": "assets/missing.png",
                "customBackground": "assets/kept.png",
                "enabledModules": ["module-a", "module-b"],
            }),
        )
        .unwrap();
    store.replace_commit(&staging, Some(0)).unwrap();
    (directory, store)
}

fn root_of(store: &PersistentStore) -> Value {
    store.read_root(None).unwrap().value
}

fn bind_server_capture(store: &PersistentStore) {
    store.connection.execute_batch(
        "INSERT INTO server_sync_state(singleton,config) VALUES(1,'{}');
         UPDATE library_sync_selection SET target='server',connection_id='synthetic';"
    ).unwrap();
}

#[test]
fn repair_and_undo_keep_sync_identity_and_emit_only_changed_records() {
    check_repair_and_undo_sync_identity(false);
}

#[test]
fn externally_selected_repair_and_undo_keep_identity_and_precise_capture() {
    check_repair_and_undo_sync_identity(true);
}

fn check_repair_and_undo_sync_identity(external: bool) {
    let (_directory, mut store) = fixture();
    bind_server_capture(&store);
    if external {
        let epoch = super::super::sync_selection::read(&store.connection).unwrap().epoch;
        store.external_select(&epoch, &super::super::sync_selection::SyncTarget::External("synthetic-external".into())).unwrap();
    }
    let selected = super::super::sync_selection::read(&store.connection).unwrap();
    let before = super::super::sync_selection::identity(&store.connection).unwrap();
    store.connection.execute_batch("DELETE FROM content_changes; ").unwrap();
    let (_, journal) = store.apply_repair(1, &[candidate(RepairAction::DropReference {
        owner: crate::data_health::Owner { kind: "root".into(), id: "database".into() },
        source_path: "$.userIcon".into(), occurrence: 0, token: None,
    })], 10).unwrap();
    for revision in [2, 3] {
        if revision == 3 { store.undo_repair(&journal, 2).unwrap(); }
        let after = super::super::sync_selection::identity(&store.connection).unwrap();
        assert_eq!(after.library_epoch, before.library_epoch);
        assert_eq!(after.selection_epoch, before.selection_epoch);
        assert_eq!(after.generation, before.generation);
        let after_selection = super::super::sync_selection::read(&store.connection).unwrap();
        assert_eq!(after_selection.target, selected.target);
        for table in ["content_changes"] {
            let entries: Vec<(String, String, String, i64)> = store.connection.prepare(&format!("SELECT kind,key1,key2,revision FROM {table}")).unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).unwrap().collect::<Result<_,_>>().unwrap();
            assert_eq!(entries, vec![("root".into(), String::new(), String::new(), revision)]);
        }
    }
}

#[test]
fn a_reference_removal_rewrites_only_the_field_it_names() {
    let (_directory, mut store) = fixture();
    let (committed, journal) = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.userIcon".to_owned(),
                occurrence: 0,
                token: None,
            })],
            10,
        )
        .unwrap();
    assert_eq!(committed.revision, 2);
    let root = root_of(&store);
    assert!(root.get("userIcon").is_none());
    assert_eq!(
        root.get("customBackground").and_then(Value::as_str),
        Some("assets/kept.png"),
        "a repair changes only what was selected"
    );
    assert_eq!(journal.from_revision, 1);
    assert_eq!(journal.to_revision, 2);
    assert_eq!(journal.records.len(), 1);
    assert_eq!(journal.records[0].table, "root");
}

#[test]
fn root_normalization_removes_only_separately_stored_fields() {
    let (_directory, mut store) = fixture();
    let generation = active_generation(&store.connection).unwrap();
    store
        .connection
        .execute(
            "UPDATE root SET value=json_set(value, '$.pluginStorageMeta', json('{\"owned\":true}')) WHERE generation=?1",
            [&generation],
        )
        .unwrap();
    store
        .apply_repair(
            1,
            &[candidate(RepairAction::NormalizeRecords {
                table: "root".to_owned(),
            })],
            10,
        )
        .unwrap();
    let root = root_of(&store);
    assert!(root.get("pluginStorageMeta").is_none());
    assert_eq!(
        root.get("customBackground").and_then(Value::as_str),
        Some("assets/kept.png")
    );
}

#[test]
fn several_removals_in_one_record_keep_naming_the_same_elements() {
    let (_directory, mut store) = fixture();
    let owner = crate::data_health::Owner {
        kind: "root".to_owned(),
        id: "database".to_owned(),
    };
    store
        .apply_repair(
            1,
            &[
                candidate(RepairAction::DropReference {
                    owner: owner.clone(),
                    source_path: "$.enabledModules[0]".to_owned(),
                    occurrence: 0,
                    token: None,
                }),
                RepairCandidate {
                    id: "1:test".to_owned(),
                    ..candidate(RepairAction::DropReference {
                        owner,
                        source_path: "$.enabledModules[1]".to_owned(),
                        occurrence: 1,
                        token: None,
                    })
                },
            ],
            10,
        )
        .unwrap();
    assert_eq!(
        root_of(&store).get("enabledModules"),
        Some(&json!([])),
        "removing the later element first keeps the earlier index meaning what it meant"
    );
}

#[test]
fn an_undo_restores_the_previous_image_and_raises_the_revision() {
    let (_directory, mut store) = fixture();
    let before = root_of(&store);
    let (_, journal) = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.userIcon".to_owned(),
                occurrence: 0,
                token: None,
            })],
            10,
        )
        .unwrap();
    let (undone, skipped) = store.undo_repair(&journal, 2).unwrap();
    assert_eq!(undone.revision, 3);
    assert!(skipped.is_empty());
    assert_eq!(root_of(&store), before);
}

#[test]
fn an_undo_leaves_a_record_the_reader_changed_after_the_repair() {
    let (_directory, mut store) = fixture();
    bind_server_capture(&store);
    let (_, journal) = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.userIcon".to_owned(),
                occurrence: 0,
                token: None,
            })],
            10,
        )
        .unwrap();
    store
        .commit(&crate::persistent_store::WorkingSetCommit {
            expected_revision: 2,
            root: Some(json!({ "customBackground": "assets/kept.png", "note": "edited" })),
            root_mutations: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_ids: None,
            plugin_storage: None,
            asset_owner_heads: None,
        ..Default::default()})
        .unwrap();

    let (_, skipped) = store.undo_repair(&journal, 3).unwrap();
    assert_eq!(skipped, ["root:"], "the later edit is reported, not discarded");
    for table in ["content_changes"] {
        let revision: i64 = store.connection.query_row(
            &format!("SELECT revision FROM {table} WHERE kind='root' AND key1='' AND key2=''"),
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(revision, 3, "a skipped record must not enter the undo delta in {table}");
        let undo_entries: i64 = store.connection.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE revision>3"), [], |row| row.get(0),
        ).unwrap();
        assert_eq!(undo_entries, 0);
    }
    assert_eq!(
        root_of(&store).get("note").and_then(Value::as_str),
        Some("edited")
    );
}

#[test]
fn a_repair_onto_another_revision_is_refused_before_anything_is_staged() {
    let (_directory, mut store) = fixture();
    let error = store
        .apply_repair(
            0,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.userIcon".to_owned(),
                occurrence: 0,
                token: None,
            })],
            10,
        )
        .unwrap_err();
    assert!(matches!(error, StoreError::RevisionConflict { .. }), "{error:?}");
    assert_eq!(store.revision().unwrap(), 1, "nothing was applied");
    assert_eq!(
        root_of(&store).get("userIcon").and_then(Value::as_str),
        Some("assets/missing.png")
    );
}

#[test]
fn a_repair_that_leaves_the_library_refused_is_not_applied() {
    let (_directory, mut store) = fixture();
    let generation = active_generation(&store.connection).unwrap();
    // An alias whose payload is absent blocks activation, and this repair does not answer it.
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext)
             VALUES (?1, 'assets/absent.png', ?2, 'asset', 4, 'image/png', 'absent.png', 'png')",
            rusqlite::params![generation, "3c".repeat(32)],
        )
        .unwrap();

    let error = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.userIcon".to_owned(),
                occurrence: 0,
                token: None,
            })],
            10,
        )
        .unwrap_err();
    assert!(matches!(error, StoreError::Validation { .. }), "{error:?}");
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(
        active_generation(&store.connection).unwrap(),
        generation,
        "the live generation is untouched"
    );
    assert_eq!(
        root_of(&store).get("userIcon").and_then(Value::as_str),
        Some("assets/missing.png"),
        "the change the reader selected is not half applied"
    );
}

#[test]
fn a_repair_is_applied_while_the_sync_server_holds_a_body_this_device_does_not() {
    let (_directory, mut store) = fixture();
    let root = store.repository_root().to_owned();
    let cas = crate::asset_repository::PayloadCas::new(&root).unwrap();
    let payload = cas.prepare_bytes(b"synthetic server-held body").unwrap();
    let generation = active_generation(&store.connection).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext)
             VALUES (?1, 'assets/kept.png', ?2, 'asset', ?3, 'image/png', 'kept.png', 'png')",
            rusqlite::params![generation, payload.content_hash, payload.byte_size as i64],
        )
        .unwrap();
    std::fs::remove_file(cas.object_path(&payload.content_hash).unwrap().unwrap()).unwrap();
    crate::server_sync::residency::test_remote::hold(&root, &[(&payload.content_hash, payload.byte_size)]);

    let (revision, _) = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.userIcon".to_owned(),
                occurrence: 0,
                token: None,
            })],
            10,
        )
        .expect("the gate accepts a body the server holds");
    assert_eq!(revision.revision, 2);
    assert_eq!(root_of(&store).get("userIcon"), None);
    assert_eq!(root_of(&store).get("customBackground").and_then(Value::as_str), Some("assets/kept.png"));
    assert_eq!(crate::server_sync::residency::test_remote::fetched(&root), 0);
}

#[test]
fn a_removed_alias_releases_its_object_into_the_journal() {
    let (_directory, mut store) = fixture();
    let cas = crate::asset_repository::PayloadCas::new(store.repository_root()).unwrap();
    let stored = cas.prepare_bytes(b"synthetic").unwrap();
    let generation = active_generation(&store.connection).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext)
             VALUES (?1, 'assets/kept.png', ?2, 'asset', 9, 'image/png', 'kept.png', 'png')",
            rusqlite::params![generation, stored.content_hash],
        )
        .unwrap();

    let (_, journal) = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropAlias {
                kind: "asset".to_owned(),
                key: "assets/kept.png".to_owned(),
            })],
            10,
        )
        .unwrap();
    assert!(journal.released_objects.contains(&stored.content_hash));
    assert!(
        crate::data_health::journal::roots(store.repository_root())
            .unwrap()
            .object_hashes
            .is_empty(),
        "the journal only holds what is written to disk, which the command does"
    );
}

#[test]
fn a_path_no_record_uses_changes_nothing() {
    let (_directory, mut store) = fixture();
    let before = root_of(&store);
    let error = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.neverPresent".to_owned(),
                occurrence: 0,
                token: None,
            })],
            10,
        )
        .map(|(committed, _)| committed);
    // The staged copy is identical, so the activation still succeeds and the library is the same.
    assert!(error.is_ok(), "{error:?}");
    assert_eq!(root_of(&store), before);
}

#[test]
fn parse_path_reads_the_shapes_the_reference_graph_emits() {
    assert_eq!(parse_path("$.userIcon"), Some(vec![Step::Key("userIcon".to_owned())]));
    assert_eq!(
        parse_path("$.enabledModules[2]"),
        Some(vec![Step::Key("enabledModules".to_owned()), Step::Index(2)])
    );
    assert_eq!(
        parse_path("$.message[0].data"),
        Some(vec![
            Step::Key("message".to_owned()),
            Step::Index(0),
            Step::Key("data".to_owned()),
        ])
    );
    for key in ["$__name.with.dots", "한글 변수", "a]b", "quote\"and\\slash", "123", ""] {
        let path = format!("{}[0]", crate::lossless_f0::property_path("$.scriptstate", key));
        assert_eq!(
            parse_path(&path),
            Some(vec![
                Step::Key("scriptstate".to_owned()),
                Step::Key(key.to_owned()),
                Step::Index(0),
            ]),
            "{path}"
        );
    }
    assert_eq!(parse_path("userIcon"), None);
    assert_eq!(parse_path("$.a[x]"), None);
    assert_eq!(parse_path(r#"$.a["unterminated]"#), None);
    assert_eq!(parse_path(r#"$.a["b"x]"#), None);
}

#[test]
fn a_failure_during_repair_activation_preserves_live_data_identity_and_capture() {
    let (_directory, mut store) = fixture();
    bind_server_capture(&store);
    let epoch = super::super::sync_selection::read(&store.connection).unwrap().epoch;
    store.external_select(&epoch, &super::super::sync_selection::SyncTarget::External("synthetic-external".into())).unwrap();
    let before = super::super::sync_selection::identity(&store.connection).unwrap();
    let root = root_of(&store);
    store.connection.execute_batch("DELETE FROM content_changes;
        CREATE TRIGGER synthetic_repair_activation_failure BEFORE UPDATE ON meta
        WHEN NEW.key='currentRevision' BEGIN SELECT RAISE(ABORT,'synthetic activation failure'); END;").unwrap();
    assert!(store.apply_repair(1, &[candidate(RepairAction::DropReference {
        owner: crate::data_health::Owner { kind: "root".into(), id: "database".into() },
        source_path: "$.userIcon".into(), occurrence: 0, token: None,
    })], 10).is_err());
    let after = super::super::sync_selection::identity(&store.connection).unwrap();
    assert_eq!(after.library_epoch, before.library_epoch);
    assert_eq!(after.selection_epoch, before.selection_epoch);
    assert_eq!(after.generation, before.generation);
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(root_of(&store), root);
    for table in ["content_changes"] {
        let count: i64 = store.connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "failed activation must not publish {table}");
    }
}

#[test]
#[ignore = "explicit synthetic repair/undo disk and time measurement"]
fn measures_single_reference_repair_and_undo_on_a_large_library() {
    fn file_bytes(path: &std::path::Path) -> u64 {
        match std::fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("read synthetic fixture file size: {error}"),
        }
    }
    fn directory_bytes(path: &std::path::Path) -> u64 {
        std::fs::read_dir(path).unwrap().map(|entry| {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            assert!(!kind.is_symlink());
            if kind.is_dir() { directory_bytes(&entry.path()) } else { file_bytes(&entry.path()) }
        }).sum()
    }

    const MESSAGES: usize = 10_000;
    const MESSAGE_BYTES: usize = 1024;
    let (directory, mut store) = fixture();
    let original = root_of(&store);
    let staging = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&staging, &original).unwrap();
    let messages: Vec<Value> = (0..MESSAGES).map(|index| json!({
        "chatId": format!("synthetic-message-{index}"), "role": "user", "data": "x".repeat(MESSAGE_BYTES),
    })).collect();
    store.replace_add_characters(&staging, &[json!({
        "chaId": "synthetic-large-character", "name": "Synthetic", "type": "character", "chatPage": 0,
        "chats": [{"id": "synthetic-large-chat", "name": "Synthetic", "message": messages}],
    })]).unwrap();
    store.replace_commit(&staging, Some(1)).unwrap();
    bind_server_capture(&store);
    let epoch = super::super::sync_selection::read(&store.connection).unwrap().epoch;
    store.external_select(&epoch, &super::super::sync_selection::SyncTarget::External("synthetic-external".into())).unwrap();
    let selected = super::super::sync_selection::read(&store.connection).unwrap();
    let identity = super::super::sync_selection::identity(&store.connection).unwrap();
    let (actual_messages, message_json_bytes): (i64, i64) = store.connection.query_row(
        "SELECT COUNT(*),SUM(LENGTH(CAST(value AS BLOB))) FROM messages", [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(actual_messages, MESSAGES as i64);
    store.connection.execute_batch("DELETE FROM content_changes;  PRAGMA wal_autocheckpoint=0;").unwrap();
    let database = store.database_path.clone();
    let wal = std::path::PathBuf::from(format!("{}-wal", database.display()));
    let footprint = || {
        let database_bytes = file_bytes(&database);
        let wal_bytes = file_bytes(&wal);
        let total_bytes = directory_bytes(directory.path());
        assert!(total_bytes >= database_bytes + wal_bytes, "synthetic fixture footprint omitted database or WAL bytes");
        (total_bytes, database_bytes, wal_bytes)
    };
    let assert_contract = |store: &PersistentStore, revision: i64| {
        let after = super::super::sync_selection::identity(&store.connection).unwrap();
        assert_eq!(after.library_epoch, identity.library_epoch);
        assert_eq!(after.selection_epoch, identity.selection_epoch);
        assert_eq!(after.generation, identity.generation);
        let selection = super::super::sync_selection::read(&store.connection).unwrap();
        assert_eq!(selection.target, selected.target);
        let retained: (i64, i64) = store.connection.query_row(
            "SELECT COUNT(*),SUM(LENGTH(CAST(value AS BLOB))) FROM messages", [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(retained, (actual_messages, message_json_bytes));
        for table in ["content_changes"] {
            let entries: Vec<(String, String, String, i64)> = store.connection.prepare(&format!("SELECT kind,key1,key2,revision FROM {table}")).unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).unwrap().collect::<Result<_,_>>().unwrap();
            assert_eq!(entries, vec![("root".into(), String::new(), String::new(), revision)]);
        }
    };

    store.checkpoint(crate::persistent_store::CheckpointMode::Truncate).unwrap();
    let repair_before = footprint();
    let started = std::time::Instant::now();
    let (repaired, journal) = store.apply_repair(identity.revision, &[candidate(RepairAction::DropReference {
        owner: crate::data_health::Owner { kind: "root".into(), id: "database".into() },
        source_path: "$.userIcon".into(), occurrence: 0, token: None,
    })], 10).unwrap();
    let repair_ms = started.elapsed().as_millis();
    let repair_after = footprint();
    assert_eq!(journal.records.len(), 1);
    assert_eq!(journal.records[0].table, "root");
    assert!(root_of(&store).get("userIcon").is_none());
    assert_contract(&store, repaired.revision);
    assert!(repair_after.2 > repair_before.2);
    eprintln!("cr004 phase=repair messages={actual_messages} message_json_bytes={message_json_bytes} changed_records={} elapsed_ms={repair_ms} temporary_root_file_bytes_before={} temporary_root_file_bytes_after={} temporary_root_file_growth_bytes={} database_bytes_before={} database_bytes_after={} wal_bytes_before={} wal_bytes_after={} wal_growth_bytes={}",
        journal.records.len(), repair_before.0, repair_after.0, repair_after.0.saturating_sub(repair_before.0), repair_before.1, repair_after.1,
        repair_before.2, repair_after.2, repair_after.2.saturating_sub(repair_before.2));

    store.checkpoint(crate::persistent_store::CheckpointMode::Truncate).unwrap();
    let undo_before = footprint();
    let started = std::time::Instant::now();
    let (undone, skipped) = store.undo_repair(&journal, repaired.revision).unwrap();
    let undo_ms = started.elapsed().as_millis();
    let undo_after = footprint();
    assert!(skipped.is_empty());
    assert_eq!(root_of(&store), original);
    assert_contract(&store, undone.revision);
    assert!(undo_after.2 > undo_before.2);
    eprintln!("cr004 phase=undo messages={actual_messages} message_json_bytes={message_json_bytes} changed_records={} elapsed_ms={undo_ms} temporary_root_file_bytes_before={} temporary_root_file_bytes_after={} temporary_root_file_growth_bytes={} database_bytes_before={} database_bytes_after={} wal_bytes_before={} wal_bytes_after={} wal_growth_bytes={}",
        journal.records.len(), undo_before.0, undo_after.0, undo_after.0.saturating_sub(undo_before.0), undo_before.1, undo_after.1,
        undo_before.2, undo_after.2, undo_after.2.saturating_sub(undo_before.2));
    store.checkpoint(crate::persistent_store::CheckpointMode::Truncate).unwrap();
    let checkpoint = footprint();
    eprintln!("cr004 phase=checkpoint temporary_root_file_bytes={} database_bytes={} wal_bytes={}", checkpoint.0, checkpoint.1, checkpoint.2);
}

fn unit(parts: &[&str]) -> UnitKey {
    lww::unit_key(parts).unwrap()
}

fn chat_message(index: usize) -> Value {
    json!({ "role": "user", "data": format!("message-{index}"), "chatId": format!("m-{index}") })
}

/// A library holding a record of every kind a repair rewrites, written by an activation and an
/// ordinary save, so its shared units already describe it. Each reference in it names a file the
/// library does not hold.
fn library() -> (tempfile::TempDir, PersistentStore) {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let staging = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(
            &staging,
            &json!({
                "customBackground": "assets/missing.png",
                "personas": [{ "id": "persona-a", "name": "P", "icon": "assets/missing.png" }],
                "modules": [{ "id": "module-a", "name": "M", "assets": [["gone", "assets/missing.png", "png"]] }],
                "loadouts": [{ "id": "loadout-a", "name": "Loadout", "characterIds": ["char-gone"] }],
            }),
        )
        .unwrap();
    store
        .replace_put_presets(
            &staging,
            &[
                json!({ "id": "preset-a", "name": "Preset A", "bias": [["assets/missing.png", 1]] }),
                json!({ "id": "preset-b", "name": "Preset B" }),
            ],
        )
        .unwrap();
    let mut long: Vec<Value> = (0..6).map(chat_message).collect();
    long[3]["extra"] = json!("assets/missing.png");
    store
        .replace_add_characters(
            &staging,
            &[
                json!({
                    "chaId": "char-a", "name": "A", "type": "character", "chatPage": 0,
                    "emotionImages": [["happy", "assets/missing.png"]],
                    "chats": [
                        { "id": "conv-a", "name": "Chat A", "note": "assets/missing.png", "message": long },
                        { "id": "conv-b", "name": "Chat B", "message": [chat_message(0), chat_message(1)] },
                    ],
                }),
                json!({
                    "chaId": "char-b", "name": "B", "type": "character", "chatPage": 0,
                    "chats": [{ "id": "conv-c", "name": "Chat C", "message": [chat_message(0)] }],
                }),
            ],
        )
        .unwrap();
    store.replace_commit(&staging, Some(0)).unwrap();
    store
        .commit(&crate::persistent_store::WorkingSetCommit {
            expected_revision: 1,
            plugin_storage: Some(vec![crate::persistent_store::PluginStorageMutation::Set {
                owner: "plugin-a".into(),
                key: "saved".into(),
                value: json!({ "icon": "assets/missing.png", "kept": true }),
            }]),
            ..Default::default()
        })
        .unwrap();
    (directory, store)
}

fn owner(kind: &str, id: &str) -> crate::data_health::Owner {
    crate::data_health::Owner {
        kind: kind.to_owned(),
        id: id.to_owned(),
    }
}

fn drop_reference(kind: &str, id: &str, path: &str) -> RepairCandidate {
    candidate(RepairAction::DropReference {
        owner: owner(kind, id),
        source_path: path.to_owned(),
        occurrence: 0,
        token: None,
    })
}

fn normalize(table: &str) -> RepairCandidate {
    candidate(RepairAction::NormalizeRecords {
        table: table.to_owned(),
    })
}

/// Every candidate in one repair needs its own id, as the planner gives it.
fn numbered(candidates: Vec<RepairCandidate>) -> Vec<RepairCandidate> {
    candidates
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| RepairCandidate {
            id: format!("{index}:test"),
            ..candidate
        })
        .collect()
}

/// What the outbox holds, by unit.
fn queued(store: &PersistentStore) -> BTreeMap<UnitKey, (Stamp, UnitValue)> {
    let authority = store.lww_binding_authority().unwrap();
    store
        .lww_read_outbox(authority, 100_000)
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| (entry.key, (entry.stamp, entry.value)))
        .collect()
}

/// Conversations whose stored pages differ from paging their messages again.
fn stale_pages(store: &mut PersistentStore) -> Vec<String> {
    let generation = active_generation(&store.connection).unwrap();
    let conversations = store
        .connection
        .prepare("SELECT character_id,conversation_id FROM conversations WHERE generation=?1 ORDER BY 1,2")
        .unwrap()
        .query_map([&generation], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let transaction = store.connection.transaction().unwrap();
    let mut stale = Vec::new();
    for (character, conversation) in conversations {
        let current = message_pages::current_manifest(&transaction, &generation, &character, &conversation).unwrap();
        let paged = message_pages::capture_manifest(&transaction, &generation, &character, &conversation, None).unwrap();
        if current != paged {
            stale.push(format!("{character}/{conversation}"));
        }
    }
    transaction.rollback().unwrap();
    stale
}

/// Units the shared state holds differently from the library's rows, with every conversation
/// paged again from its messages: what a replacement with an exact copy would still have to
/// publish. A unit under a retired record is gone for good and is left out.
fn unpublished(store: &mut PersistentStore) -> Vec<UnitKey> {
    let active = active_generation(&store.connection).unwrap();
    let copy = store.replace_begin().unwrap().staging_id;
    copy_generation(&store.connection, &active, &copy).unwrap();
    let [(changes, _), _] = store.replacement_change_sets(&copy, None).unwrap();
    store.replace_abort(&copy).unwrap();
    changes
        .into_iter()
        .filter(|(key, value)| {
            !(matches!(value, UnitValue::Deleted)
                && lww::parent_status(&store.connection, key).unwrap() == "retired")
        })
        .map(|(key, _)| key)
        .collect()
}

/// Checks what an ordinary save guarantees after one repair step: the stored pages are what
/// paging the messages again gives, the shared units are what the rows hold, and exactly the
/// expected units were queued under a stamp newer than anything queued before the step.
fn check_step(
    store: &mut PersistentStore,
    step: &str,
    before: &BTreeMap<UnitKey, (Stamp, UnitValue)>,
    changed: &[UnitKey],
) {
    assert_eq!(stale_pages(store), Vec::<String>::new(), "{step} left stale message pages");
    assert_eq!(unpublished(store), Vec::<UnitKey>::new(), "{step} left units the rows no longer hold");
    let after = queued(store);
    let newest = before.values().map(|(stamp, _)| stamp).max();
    let mut fresh = after
        .iter()
        .filter(|(key, (stamp, _))| before.get(*key).map(|(old, _)| old) != Some(stamp))
        .map(|(key, (stamp, _))| {
            assert!(
                newest.is_none_or(|newest| stamp > newest),
                "{step} queued {key:?} under an older stamp"
            );
            key.clone()
        })
        .collect::<Vec<_>>();
    fresh.sort();
    let mut expected = changed.to_vec();
    expected.sort();
    assert_eq!(fresh, expected, "{step} queued other units than the ones it changed");
}

/// Applies a repair and undoes it, checking the shared state after each step. Returns the
/// messages read and pages written by the two steps themselves.
fn check_repair_and_undo(
    store: &mut PersistentStore,
    candidates: &[RepairCandidate],
    changed: &[UnitKey],
) -> (usize, usize) {
    assert_eq!(unpublished(store), Vec::<UnitKey>::new(), "the fixture starts published");
    let before = queued(store);
    let revision = store.revision().unwrap();
    message_pages::reset_capture_work();
    let (repaired, journal) = store.apply_repair(revision, candidates, 10).unwrap();
    let repair_work = message_pages::take_capture_work().work;
    check_step(store, "repair", &before, changed);
    let before = queued(store);
    message_pages::reset_capture_work();
    let (undone, skipped) = store.undo_repair(&journal, repaired.revision).unwrap();
    let undo_work = message_pages::take_capture_work().work;
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(undone.revision, repaired.revision + 1);
    check_step(store, "undo", &before, changed);
    (
        repair_work.messages_read + undo_work.messages_read,
        repair_work.pages_written + undo_work.pages_written,
    )
}

#[test]
fn a_fresh_library_has_nothing_left_to_publish() {
    let (_directory, mut store) = library();
    assert_eq!(stale_pages(&mut store), Vec::<String>::new());
    assert_eq!(unpublished(&mut store), Vec::<UnitKey>::new());
}

#[test]
fn a_dropped_root_reference_reaches_the_outbox_and_its_undo_too() {
    let (_directory, mut store) = library();
    let (_, old_value) = queued(&store).remove(&unit(&["root", "customBackground"])).unwrap();
    check_repair_and_undo(
        &mut store,
        &[drop_reference("root", "database", "$.customBackground")],
        &[unit(&["root", "customBackground"])],
    );
    assert_eq!(
        queued(&store)[&unit(&["root", "customBackground"])].1,
        old_value,
        "the undo queues the value the repair removed"
    );
}

#[test]
fn a_reference_dropped_from_a_message_repages_its_conversation() {
    let (_directory, mut store) = library();
    check_repair_and_undo(
        &mut store,
        &[drop_reference("message", "char-a/conv-a/3", "$.extra")],
        &[unit(&["messages", "char-a", "conv-a"])],
    );
}

#[test]
fn references_dropped_from_records_reach_the_outbox() {
    for (kind, id, path, changed) in [
        ("preset", "preset-a", "$.bias[0]", unit(&["preset", "preset-a", "bias"])),
        ("character", "char-a", "$.emotionImages[0]", unit(&["character", "char-a", "emotionImages"])),
        ("conversation", "char-a/conv-a", "$.note", unit(&["conversation", "char-a", "conv-a", "note"])),
        ("persona", "persona-a", "$.icon", unit(&["persona", "persona-a", "icon"])),
        ("module", "module-a", "$.assets[0]", unit(&["record", "modules", "module-a"])),
        ("loadout", "Loadout", "$.characterIds[0]", unit(&["record", "loadouts", "loadout-a"])),
    ] {
        let (_directory, mut store) = library();
        check_repair_and_undo(&mut store, &[drop_reference(kind, id, path)], &[changed]);
    }
}

#[test]
fn a_plugin_value_rewritten_with_its_size_reaches_the_outbox() {
    let (_directory, mut store) = library();
    check_repair_and_undo(
        &mut store,
        &numbered(vec![
            drop_reference("plugin-storage", r#"{"owner":"plugin-a","key":"saved"}"#, "$.icon"),
            normalize("plugin_storage"),
        ]),
        &[unit(&["plugin", "plugin-a", "saved"])],
    );
}

fn stored_alias(store: &mut PersistentStore, size: i64) -> String {
    let cas = crate::asset_repository::PayloadCas::new(store.repository_root()).unwrap();
    let stored = cas.prepare_bytes(b"synthetic").unwrap();
    let revision = store.revision().unwrap();
    store
        .commit_asset_alias(
            &crate::persistent_store::AssetAlias {
                key: "assets/kept.png".into(),
                object_hash: Some(stored.content_hash.clone()),
                kind: "asset".into(),
                size,
                mime: "image/png".into(),
                name: "kept.png".into(),
                ext: "png".into(),
                inlay_type: None,
                width: None,
                height: None,
                metadata: json!({}),
            },
            revision,
        )
        .unwrap();
    stored.content_hash
}

#[test]
fn a_removed_alias_and_an_adopted_payload_reach_the_outbox() {
    let (_directory, mut store) = library();
    stored_alias(&mut store, 9);
    check_repair_and_undo(
        &mut store,
        &[candidate(RepairAction::DropAlias {
            kind: "asset".into(),
            key: "assets/kept.png".into(),
        })],
        &[unit(&["asset", "assets/kept.png"])],
    );

    let (_directory, mut store) = library();
    stored_alias(&mut store, 4);
    check_repair_and_undo(
        &mut store,
        &[candidate(RepairAction::AdoptStoredPayload {
            kind: "asset".into(),
            key: "assets/kept.png".into(),
        })],
        &[unit(&["asset", "assets/kept.png"])],
    );
}

#[test]
fn normalizing_derived_columns_queues_nothing_and_keeps_pages() {
    for (table, damage) in [
        ("root", "UPDATE root SET value=json_set(value,'$.pluginStorageMeta',json('{\"owned\":true}')) WHERE generation=?1"),
        ("characters", "UPDATE characters SET conversation_count=9 WHERE generation=?1 AND character_id='char-a'"),
        ("conversations", "UPDATE conversations SET configured_index=0 WHERE generation=?1"),
        ("bot_presets", "UPDATE bot_presets SET configured_index=7 WHERE generation=?1 AND preset_id='preset-b'"),
        ("plugin_storage", "UPDATE plugin_storage SET byte_size=1 WHERE generation=?1"),
    ] {
        let (_directory, mut store) = library();
        let generation = active_generation(&store.connection).unwrap();
        store.connection.execute(damage, [&generation]).unwrap();
        assert_eq!(
            check_repair_and_undo(&mut store, &[normalize(table)], &[]),
            (0, 0),
            "{table}: a record whose messages are untouched keeps its stored pages"
        );
    }
}

/// Moves messages of one conversation to higher indexes without the database noticing, so
/// the conversation's indexes have a gap at `from`.
fn open_gap(store: &PersistentStore, from: i64) {
    let generation = active_generation(&store.connection).unwrap();
    store
        .connection
        .execute_batch(&format!(
            "UPDATE messages SET message_index=message_index+100 WHERE generation='{generation}' AND character_id='char-a' AND conversation_id='conv-a' AND message_index>={from};
             UPDATE messages SET message_index=message_index-99 WHERE generation='{generation}' AND character_id='char-a' AND conversation_id='conv-a' AND message_index>=100;"
        ))
        .unwrap();
}

#[test]
fn renumbering_messages_repages_them_and_an_undo_that_restores_the_gap_is_refused() {
    let (_directory, mut store) = library();
    open_gap(&store, 2);
    let before = queued(&store);
    let revision = store.revision().unwrap();
    let (repaired, journal) = store.apply_repair(revision, &[normalize("messages")], 10).unwrap();
    assert!(journal.records.iter().any(|record| record.table == "messages"));
    check_step(&mut store, "repair", &before, &[]);

    let rows = |store: &PersistentStore| -> Vec<(i64, String)> {
        store
            .connection
            .prepare("SELECT message_index,value FROM messages WHERE character_id='char-a' AND conversation_id='conv-a' AND generation=?1 ORDER BY 1")
            .unwrap()
            .query_map([active_generation(&store.connection).unwrap()], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let repaired_rows = rows(&store);
    let error = store.undo_repair(&journal, repaired.revision).unwrap_err();
    assert!(matches!(error, StoreError::Validation { .. }), "{error:?}");
    assert_eq!(store.revision().unwrap(), repaired.revision);
    assert_eq!(rows(&store), repaired_rows, "a refused undo changes nothing");
    let before = queued(&store);
    check_step(&mut store, "refused undo", &before, &[]);
}

#[test]
fn renumbering_messages_after_a_lost_row_publishes_what_remains() {
    let (_directory, mut store) = library();
    let generation = active_generation(&store.connection).unwrap();
    store
        .connection
        .execute(
            "DELETE FROM messages WHERE generation=?1 AND character_id='char-a' AND conversation_id='conv-a' AND message_index=2",
            [&generation],
        )
        .unwrap();
    let before = queued(&store);
    let revision = store.revision().unwrap();
    store
        .apply_repair(revision, &numbered(vec![normalize("conversations"), normalize("messages")]), 10)
        .unwrap();
    check_step(&mut store, "repair", &before, &[unit(&["messages", "char-a", "conv-a"])]);
    let manifest = queued(&store)[&unit(&["messages", "char-a", "conv-a"])].1.clone();
    let transaction = store.connection.transaction().unwrap();
    let paged = message_pages::capture_manifest(&transaction, &generation, "char-a", "conv-a", None).unwrap();
    transaction.rollback().unwrap();
    assert_eq!(manifest, paged, "the queued manifest pages the five messages that remain");
}

#[test]
fn keeping_the_single_root_changes_and_queues_nothing() {
    let (_directory, mut store) = library();
    let before = queued(&store);
    let revision = store.revision().unwrap();
    let (repaired, journal) = store
        .apply_repair(revision, &[candidate(RepairAction::KeepSingleRecord { table: "root".into() })], 10)
        .unwrap();
    assert!(journal.records.is_empty(), "the live root table cannot hold a second root");
    check_step(&mut store, "repair", &before, &[]);
    store.undo_repair(&journal, repaired.revision).unwrap();
    check_step(&mut store, "undo", &before, &[]);
}

fn diagnosis(store: &mut PersistentStore) -> crate::data_health::ScanResult {
    let revision = store.revision().unwrap();
    let lease = store.acquire_revision(revision).unwrap().lease;
    let reader = store.data_health_reader(&lease).unwrap();
    let findings = reader.scan(2000, &crate::local_backup::NeverCancelled).unwrap();
    drop(reader);
    store.release_revision(&lease).unwrap();
    crate::data_health::ScanResult::new(revision, 1, crate::data_health::ScanDepth::Quick, findings)
}

fn diagnosed_candidates(store: &mut PersistentStore, accept: impl Fn(&RepairAction) -> bool) -> Vec<RepairCandidate> {
    let result = diagnosis(store);
    let selected = crate::data_health::repair::plan(&result).into_iter().filter(|candidate| accept(&candidate.action)).collect::<Vec<_>>();
    assert!(!selected.is_empty(), "diagnosis produced no matching repair: {:?}", result.items);
    selected
}

fn json_record(store: &PersistentStore, table: &str, identity: &[&str]) -> Value {
    let definition = TABLES.iter().find(|item| item.name == table).unwrap();
    let identity = identity.iter().map(|part| (*part).to_owned()).collect::<Vec<_>>();
    let row = stored_row(&store.connection, definition, &active_generation(&store.connection).unwrap(), &identity).unwrap().unwrap();
    let column = match table { "bot_presets" | "plugin_storage" => "value", _ => "detail" };
    let index = definition.columns.iter().position(|(name, _)| *name == column).unwrap();
    serde_json::from_str(row[index].as_deref().unwrap()).unwrap()
}

#[test]
fn diagnosed_orphan_recovery_preserves_payloads_and_survives_restart() {
    for (table, damage) in [
        ("conversations", "DELETE FROM characters WHERE generation=?1 AND character_id='char-b'"),
        ("messages", "DELETE FROM conversations WHERE generation=?1 AND character_id='char-b' AND conversation_id='conv-c'"),
    ] {
        let (directory, mut store) = library();
        let generation = active_generation(&store.connection).unwrap();
        let payload: String = store.connection.query_row("SELECT value FROM messages WHERE generation=?1 AND character_id='char-b' AND conversation_id='conv-c'", [&generation], |row| row.get(0)).unwrap();
        store.connection.execute(damage, [&generation]).unwrap();
        let selected = diagnosed_candidates(&mut store, |action| matches!(action, RepairAction::RecoverOrphans { table: subject } if subject == table));
        let revision = store.revision().unwrap();
        assert!(matches!(store.apply_repair(revision - 1, &selected, 10), Err(StoreError::RevisionConflict { .. })));
        let (repaired, journal) = store.apply_repair(revision, &selected, 10).unwrap();
        assert!(!diagnosis(&mut store).items.iter().any(|item| item.code == crate::data_health::codes::RECORD_ORPHAN));
        assert!(json_record(&store, "characters", &["char-b"]).get("chats").is_none());
        assert!(json_record(&store, "conversations", &["char-b", "conv-c"]).get("message").is_none());
        if table == "conversations" { assert_eq!(json_record(&store, "characters", &["char-b"])["trashTime"], 10); }
        assert_eq!(unpublished(&mut store), Vec::<UnitKey>::new());
        drop(store);
        let mut store = PersistentStore::open(directory.path()).unwrap();
        assert_eq!(store.revision().unwrap(), repaired.revision);
        let retained: String = store.connection.query_row("SELECT value FROM messages WHERE generation=?1 AND character_id='char-b' AND conversation_id='conv-c'", [&generation], |row| row.get(0)).unwrap();
        assert_eq!(retained, payload);
        let (_, skipped) = store.undo_repair(&journal, repaired.revision).unwrap();
        assert!(skipped.is_empty());
        assert!(diagnosis(&mut store).items.iter().any(|item| item.code == crate::data_health::codes::RECORD_ORPHAN));
        assert_eq!(unpublished(&mut store), Vec::<UnitKey>::new());
        let retained: String = store.connection.query_row("SELECT value FROM messages WHERE generation=?1 AND character_id='char-b' AND conversation_id='conv-c'", [&generation], |row| row.get(0)).unwrap();
        assert_eq!(retained, payload);
    }
}

#[test]
fn orphan_recovery_keeps_the_validation_gate_for_invalid_payloads() {
    let (_directory, mut store) = library();
    let generation = active_generation(&store.connection).unwrap();
    store.connection.execute("DELETE FROM conversations WHERE generation=?1 AND character_id='char-b' AND conversation_id='conv-c'", [&generation]).unwrap();
    store.connection.execute("UPDATE messages SET value='[]' WHERE generation=?1 AND character_id='char-b'", [&generation]).unwrap();
    let selected = diagnosed_candidates(&mut store, |action| matches!(action, RepairAction::RecoverOrphans { .. }));
    let revision = store.revision().unwrap();
    let before = queued(&store);
    assert!(matches!(store.apply_repair(revision, &selected, 10), Err(StoreError::Validation { .. })));
    assert_eq!(store.revision().unwrap(), revision);
    assert_eq!(queued(&store), before);
}

/// An undo journal that removes records and restores another, the shape an undo of an orphan
/// recovery has. The removed character's id is retired, the removed conversation's pages are
/// dropped and the restored conversation's changed field is queued.
#[test]
fn an_undo_that_removes_and_restores_records_publishes_both() {
    let (_directory, mut store) = library();
    let generation = active_generation(&store.connection).unwrap();
    let row = |store: &PersistentStore, table: &str, identity: &[&str]| -> Vec<Option<String>> {
        let table = TABLES.iter().find(|candidate| candidate.name == table).unwrap();
        let identity: Vec<String> = identity.iter().map(|part| (*part).to_owned()).collect();
        stored_row(&store.connection, table, &generation, &identity).unwrap().unwrap()
    };
    let character = row(&store, "characters", &["char-b"]);
    let removed_conversation = row(&store, "conversations", &["char-b", "conv-c"]);
    let removed_message = row(&store, "messages", &["char-b", "conv-c", "0"]);
    let mut conversation = row(&store, "conversations", &["char-a", "conv-b"]);
    let mut detail: Value = serde_json::from_str(conversation[6].as_deref().unwrap()).unwrap();
    detail["name"] = json!("Chat B restored");
    conversation[4] = Some("Chat B restored".into());
    conversation[6] = Some(detail.to_string());
    let messages = (0..2)
        .map(|index| row(&store, "messages", &["char-a", "conv-b", &index.to_string()]))
        .collect::<Vec<_>>();
    // The conversation's rows are lost without the shared state hearing of it, so restoring
    // them is the only change the shared state has not seen.
    store
        .connection
        .execute_batch(&format!(
            "DELETE FROM messages WHERE generation='{generation}' AND character_id='char-a' AND conversation_id='conv-b';
             DELETE FROM conversations WHERE generation='{generation}' AND character_id='char-a' AND conversation_id='conv-b';"
        ))
        .unwrap();
    let revision = store.revision().unwrap();
    let mut records = vec![
        RecordChange {
            table: "characters".into(),
            identity: vec!["char-b".into()],
            before: None,
            after: Some(character),
        },
        RecordChange {
            table: "conversations".into(),
            identity: vec!["char-b".into(), "conv-c".into()],
            before: None,
            after: Some(removed_conversation),
        },
        RecordChange {
            table: "messages".into(),
            identity: vec!["char-b".into(), "conv-c".into(), "0".into()],
            before: None,
            after: Some(removed_message),
        },
        RecordChange {
            table: "conversations".into(),
            identity: vec!["char-a".into(), "conv-b".into()],
            before: Some(conversation),
            after: None,
        },
    ];
    for (index, message) in messages.into_iter().enumerate() {
        records.push(RecordChange {
            table: "messages".into(),
            identity: vec!["char-a".into(), "conv-b".into(), index.to_string()],
            before: Some(message),
            after: None,
        });
    }
    let journal = Journal {
        id: "repair-synthetic".into(),
        created_at: 10,
        from_revision: revision - 1,
        to_revision: revision,
        applied: Vec::new(),
        records,
        released_objects: BTreeSet::new(),
    };
    let before = queued(&store);
    let (_, skipped) = store.undo_repair(&journal, revision).unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    let pages: i64 = store
        .connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM message_page_indexes WHERE generation=?1 AND character_id='char-b')
                  + (SELECT COUNT(*) FROM message_page_manifests WHERE generation=?1 AND character_id='char-b')",
            [&generation],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pages, 0, "a removed conversation keeps no stored pages");
    assert_eq!(stale_pages(&mut store), Vec::<String>::new());
    assert_eq!(unpublished(&mut store), Vec::<UnitKey>::new());
    let after = queued(&store);
    let fresh = after
        .iter()
        .filter(|(key, (stamp, _))| before.get(*key).map(|(old, _)| old) != Some(stamp))
        .map(|(key, _)| key.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        fresh,
        BTreeSet::from([
            unit(&["conversation", "char-a", "conv-b", "name"]),
            unit(&["exists", "character", "char-b"]),
        ])
    );
    assert!(matches!(after[&unit(&["exists", "character", "char-b"])].1, UnitValue::Deleted));
}


#[test]
fn diagnosis_repairs_duplicate_preset_names_and_scoped_plugin_keys() {
    for plugin in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let staging = store.replace_begin().unwrap().staging_id;
        store.replace_put_root(&staging, &json!({})).unwrap();
        store.replace_put_presets(&staging, &[
            json!({"id":"preset-a", "name":"Duplicate", "image":"assets/missing.png"}),
            json!({"id":"preset-b", "name":"Duplicate", "image":"assets/missing.png"}),
        ]).unwrap();
        store.replace_commit(&staging, Some(0)).unwrap();
        let key = "saved/with:separators";
        store.commit(&crate::persistent_store::WorkingSetCommit {
            expected_revision: 1,
            plugin_storage: Some(["plugin/a", "plugin/b"].into_iter().map(|owner| crate::persistent_store::PluginStorageMutation::Set {
                owner: owner.into(), key: key.into(), value: json!({"icon":"{{inlay::missing}}", "kept":true}),
            }).collect()), ..Default::default()
        }).unwrap();
        let owner_id = if plugin { r#"{"owner":"plugin/a","key":"saved/with:separators"}"#.to_owned() } else { "preset-a".into() };
        let selected = diagnosed_candidates(&mut store, |action| matches!(action, RepairAction::DropReference { owner, .. } if owner.id == owner_id && owner.kind == if plugin { "plugin-storage" } else { "preset" }));
        assert_eq!(selected.len(), 1);
        let revision = store.revision().unwrap();
        let before = queued(&store);
        assert!(matches!(store.apply_repair(revision - 1, &selected, 10), Err(StoreError::RevisionConflict { .. })));
        assert_eq!(queued(&store), before);
        let (repaired, journal) = store.apply_repair(revision, &selected, 10).unwrap();
        let changed = if plugin { unit(&["plugin", "plugin/a", key]) } else { unit(&["preset", "preset-a", "image"]) };
        check_step(&mut store, "diagnosed repair", &before, &[changed.clone()]);
        // An inlay is cut from the plugin's text, while the preset's image field goes.
        let (table, selected_id, other_id, field, original, repaired_value) = if plugin {
            ("plugin_storage", vec!["plugin/a", key], vec!["plugin/b", key], "icon", json!("{{inlay::missing}}"), Some(json!("")))
        } else { ("bot_presets", vec!["preset-a"], vec!["preset-b"], "image", json!("assets/missing.png"), None) };
        assert_eq!(json_record(&store, table, &selected_id).get(field), repaired_value.as_ref());
        assert_eq!(json_record(&store, table, &other_id).get(field), Some(&original));
        drop(store);
        let mut store = PersistentStore::open(directory.path()).unwrap();
        assert_eq!(json_record(&store, table, &selected_id).get(field), repaired_value.as_ref());
        let before = queued(&store);
        let (_, skipped) = store.undo_repair(&journal, repaired.revision).unwrap();
        assert!(skipped.is_empty());
        assert_eq!(json_record(&store, table, &selected_id).get(field), Some(&original));
        check_step(&mut store, "diagnosed undo", &before, &[changed]);
    }
}

/// A repair names an owner inside the root by its identity, so when it is applied it edits the
/// entry that carries that identity wherever the entry has moved, and it is refused when no single
/// entry carries it any more.
#[test]
fn a_repair_finds_its_entry_in_the_root_again_when_applied() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let a = json!({ "id": "persona-a", "name": "A", "icon": "assets/missing.png" });
    let b = json!({ "id": "persona-b", "name": "B", "icon": "assets/kept.png" });
    let loadout = json!({ "id": "loadout-a", "name": "Loadout", "characterIds": ["char-gone"] });
    let staging = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(&staging, &json!({ "personas": [a, b], "loadouts": [loadout] }))
        .unwrap();
    store.replace_commit(&staging, Some(0)).unwrap();
    stored_alias(&mut store, 9);
    let selected = diagnosed_candidates(&mut store, |action| {
        matches!(action, RepairAction::DropReference { owner, .. } if owner.kind != "root")
    });
    assert_eq!(selected.len(), 2, "{selected:?}");
    let revision = store.revision().unwrap();
    // The lists change under the diagnosis without a save moving the revision.
    let generation = active_generation(&store.connection).unwrap();
    let rewrite = |store: &PersistentStore, personas: Value, loadouts: Value| {
        store
            .connection
            .execute(
                "UPDATE root SET value=json_set(value, '$.personas', json(?2), '$.loadouts', json(?3)) WHERE generation=?1",
                rusqlite::params![generation, personas.to_string(), loadouts.to_string()],
            )
            .unwrap();
    };

    let other = json!({ "id": "loadout-b", "name": "Loadout", "characterIds": ["char-gone"] });
    rewrite(&store, json!([a, b]), json!([loadout, other]));
    let before = root_of(&store);
    let error = store.apply_repair(revision, &selected, 10).unwrap_err();
    assert!(matches!(error, StoreError::Validation { .. }), "{error:?}");
    assert_eq!(store.revision().unwrap(), revision);
    assert_eq!(root_of(&store), before, "neither loadout carrying the name is edited");

    rewrite(&store, json!([b, a]), json!([loadout]));
    store.apply_repair(revision, &selected, 10).unwrap();
    let root = root_of(&store);
    assert_eq!(root["personas"], json!([b, { "id": "persona-a", "name": "A" }]));
    assert_eq!(root["loadouts"], json!([{ "id": "loadout-a", "name": "Loadout", "characterIds": [] }]));
}

/// The manifest an import keeps for a module's assets, which describes exactly these tuples.
fn owner_head(store: &PersistentStore, owner: crate::persistent_store::AssetOwnerLocator, tuples: &[[&str; 3]]) -> crate::persistent_store::AssetOwnerHead {
    use crate::asset_repository::owner_manifest_codec::{encode_owner_manifest, OwnerManifestEntry};
    let entries: Vec<_> = tuples
        .iter()
        .map(|tuple| OwnerManifestEntry { tuple: tuple.map(str::to_owned), payload_hash: None })
        .collect();
    let cas = crate::asset_repository::PayloadCas::new(store.repository_root()).unwrap();
    let manifest = cas.prepare_bytes(&encode_owner_manifest(&entries).unwrap()).unwrap();
    crate::persistent_store::AssetOwnerHead::present(owner, manifest.content_hash, entries.len() as i64)
}

fn owner_heads(store: &PersistentStore) -> Vec<(String, String)> {
    store
        .connection
        .prepare("SELECT owner_kind, owner_locator FROM asset_owner_heads WHERE generation=?1 ORDER BY 1, 2")
        .unwrap()
        .query_map([active_generation(&store.connection).unwrap()], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Selecting everything the diagnosis offers applies as one repair, and each change removes the
/// dangling reference itself: an inlay token leaves the text around it, an asset tuple goes as a
/// whole, and a reference the repair cannot address is not offered. An owner inside the root is
/// edited where one entry carries its identity, and a module or character whose assets change
/// loses the manifest that described them.
#[test]
fn every_offered_reference_repair_applies_and_removes_only_the_reference() {
    use crate::persistent_store::AssetOwnerLocator;
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let staging = store.replace_begin().unwrap().staging_id;
    // A loadout is named by its name, which two loadouts may share.
    let shared = ["loadout-b", "loadout-c"].map(|id| {
        json!({ "id": id, "name": "Shared", "characterIds": ["char-gone"], "presetName": "Preset A", "personaId": "persona-a" })
    });
    store
        .replace_put_root(
            &staging,
            &json!({
                "botPresetsId": 0,
                "selectedPersona": 0,
                "personas": [{
                    "id": "persona-a", "name": "P", "icon": "assets/missing.png",
                    "note": "a {{inlay::gone-g}} b",
                    "embeddedModule": {
                        "id": "embedded-a", "name": "E",
                        "assets": [["gone", "assets/missing.png", "png"], ["kept", "assets/kept.png", "png"]],
                    },
                }],
                "modules": [{
                    "id": "module-a", "name": "M", "icon": "assets/missing.png",
                    "assets": [["kept", "assets/kept.png", "png"], ["gone", "assets/missing.png", "png"]],
                    "lorebook": [{ "content": "x {{inlay::gone-h}} y" }],
                }],
                "loadouts": [
                    {
                        "id": "loadout-a", "name": "Loadout",
                        "characterIds": ["char-gone", "char-a"], "modules": ["module-a", "module-gone"],
                        "presetName": "Preset A", "personaId": "persona-gone", "icons": ["assets/missing.png"],
                    },
                    shared[0],
                    shared[1],
                ],
            }),
        )
        .unwrap();
    store
        .replace_put_presets(&staging, &[json!({ "id": "preset-a", "name": "Preset A" })])
        .unwrap();
    store
        .replace_add_characters(
            &staging,
            &[json!({
                "chaId": "char-a", "name": "A", "type": "character", "chatPage": 0,
                "firstMessage": "before {{inlay::gone-a}} after",
                "emotionImages": [["happy", "assets/missing.png"], ["calm", "assets/kept.png"]],
                "additionalAssets": [["gone", "assets/missing.png", "png"], ["kept", "assets/kept.png", "png"]],
                "chats": [{
                    "id": "conv-a", "name": "Chat A",
                    "scriptstate": {
                        "$__name.with.dots": "{{inlay::gone-b}}",
                        "한글 변수": "x {{inlay::gone-c}} y {{inlayed::gone-d}}",
                    },
                    "message": [{ "role": "char", "data": "hi {{inlay::gone-e}} there", "chatId": "m-0" }],
                }],
            })],
        )
        .unwrap();
    let heads = [
        owner_head(
            &store,
            AssetOwnerLocator::RootModuleAssets { module_id: "module-a".into() },
            &[["kept", "assets/kept.png", "png"], ["gone", "assets/missing.png", "png"]],
        ),
        owner_head(
            &store,
            AssetOwnerLocator::PersonaEmbeddedModuleAssets { persona_id: "persona-a".into(), module_id: "embedded-a".into() },
            &[["gone", "assets/missing.png", "png"], ["kept", "assets/kept.png", "png"]],
        ),
        owner_head(
            &store,
            AssetOwnerLocator::CharacterAdditionalAssets { character_id: "char-a".into() },
            &[["gone", "assets/missing.png", "png"], ["kept", "assets/kept.png", "png"]],
        ),
    ];
    store.replace_put_asset_owner_heads(&staging, &heads).unwrap();
    store.replace_commit(&staging, Some(0)).unwrap();
    store
        .commit(&crate::persistent_store::WorkingSetCommit {
            expected_revision: 1,
            plugin_storage: Some(vec![crate::persistent_store::PluginStorageMutation::Set {
                owner: "plugin-a".into(),
                key: "saved".into(),
                value: json!({ "nested": { "a.b": "{{inlay::gone-f}}" }, "kept": true }),
            }]),
            ..Default::default()
        })
        .unwrap();
    stored_alias(&mut store, 9);

    let result = diagnosis(&mut store);
    let offered = crate::data_health::repair::plan(&result);
    let unoffered = |owner: &str| {
        result
            .items
            .iter()
            .enumerate()
            .filter(|(index, finding)| {
                finding.owner.id == owner && offered.iter().all(|candidate| candidate.finding != *index)
            })
            .count()
    };
    assert_eq!(unoffered("Shared"), 2, "two loadouts carry that name: {:?}", result.items);
    let revision = store.revision().unwrap();
    let (original_root, original_heads) = (root_of(&store), owner_heads(&store));
    assert_eq!(original_heads.len(), 3);
    let (repaired, journal) = store.apply_repair(revision, &offered, 10).unwrap();
    assert_eq!(unpublished(&mut store), Vec::<UnitKey>::new());
    let manifests: BTreeSet<String> = heads.iter().filter_map(|head| head.manifest_hash.clone()).collect();
    assert!(
        journal.released_objects.is_superset(&manifests),
        "the journal holds the manifests an undo restores"
    );

    let character = json_record(&store, "characters", &["char-a"]);
    assert_eq!(character["firstMessage"], "before  after");
    assert_eq!(character["emotionImages"], json!([["calm", "assets/kept.png"]]));
    assert_eq!(character["additionalAssets"], json!([["kept", "assets/kept.png", "png"]]));
    assert_eq!(
        json_record(&store, "conversations", &["char-a", "conv-a"])["scriptstate"],
        json!({ "$__name.with.dots": "", "한글 변수": "x  y " })
    );
    let message: String = store
        .connection
        .query_row(
            "SELECT value FROM messages WHERE generation=?1 AND character_id='char-a' AND conversation_id='conv-a'",
            [active_generation(&store.connection).unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&message).unwrap()["data"], "hi  there");
    assert_eq!(
        json_record(&store, "plugin_storage", &["plugin-a", "saved"]),
        json!({ "nested": { "a.b": "" }, "kept": true })
    );
    let root = root_of(&store);
    assert_eq!(
        root["personas"][0],
        json!({
            "id": "persona-a", "name": "P", "note": "a  b",
            "embeddedModule": { "id": "embedded-a", "name": "E", "assets": [["kept", "assets/kept.png", "png"]] },
        })
    );
    assert_eq!(
        root["modules"][0],
        json!({
            "id": "module-a", "name": "M", "assets": [["kept", "assets/kept.png", "png"]],
            "lorebook": [{ "content": "x  y" }],
        })
    );
    assert_eq!(
        root["loadouts"],
        json!([
            {
                "id": "loadout-a", "name": "Loadout", "characterIds": ["char-a"], "modules": ["module-a"],
                "presetName": "Preset A", "personaId": "persona-gone", "icons": [],
            },
            shared[0],
            shared[1],
        ])
    );
    assert_eq!(owner_heads(&store), Vec::<(String, String)>::new());
    let remaining = diagnosis(&mut store);
    let mut missing: Vec<(&str, &str)> = remaining
        .items
        .iter()
        .filter(|finding| finding.code == crate::data_health::codes::REFERENCE_MISSING)
        .map(|finding| (finding.owner.id.as_str(), finding.locator.as_ref().unwrap().source_path.as_str()))
        .collect();
    missing.sort();
    assert_eq!(
        missing,
        [("Loadout", "$.personaId"), ("Shared", "$.characterIds[0]"), ("Shared", "$.characterIds[0]")],
        "{:?}",
        remaining.items
    );
    let (_, skipped) = store.undo_repair(&journal, repaired.revision).unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(root_of(&store), original_root);
    assert_eq!(owner_heads(&store), original_heads, "an undo restores the manifests too");
    assert_eq!(unpublished(&mut store), Vec::<UnitKey>::new());
}

#[test]
fn diagnosis_normalizes_orders_within_each_parent() {
    for table in ["conversations", "plugin_storage"] {
        let (directory, mut store) = library();
        let revision = store.revision().unwrap();
        store.commit(&crate::persistent_store::WorkingSetCommit {
            expected_revision: revision,
            plugin_storage: Some([("plugin-a","other"), ("plugin-b","saved"), ("plugin-b","other")].into_iter().map(|(owner,key)| crate::persistent_store::PluginStorageMutation::Set {
                owner: owner.into(), key: key.into(), value: json!({"kept":true}),
            }).collect()), ..Default::default()
        }).unwrap();
        let generation = active_generation(&store.connection).unwrap();
        let (damage, query) = if table == "conversations" {
            ("UPDATE conversations SET configured_index=0 WHERE generation=?1", "SELECT character_id,conversation_id,configured_index FROM conversations WHERE generation=?1 ORDER BY 1,3,2")
        } else {
            ("UPDATE plugin_storage SET ordinal=0 WHERE generation=?1", "SELECT owner,storage_key,ordinal FROM plugin_storage WHERE generation=?1 ORDER BY 1,3,2")
        };
        store.connection.execute(damage, [&generation]).unwrap();
        let selected = diagnosed_candidates(&mut store, |action| matches!(action, RepairAction::NormalizeRecords { table: subject } if subject == table));
        let revision = store.revision().unwrap();
        assert!(matches!(store.apply_repair(revision - 1, &selected, 10), Err(StoreError::RevisionConflict { .. })));
        let (repaired, journal) = store.apply_repair(revision, &selected, 10).unwrap();
        let rows = |store: &PersistentStore| store.connection.prepare(query).unwrap().query_map([&generation], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
        let expected = if table == "conversations" { vec![("char-a","conv-a",0),("char-a","conv-b",1),("char-b","conv-c",0)] }
            else { vec![("plugin-a","other",0),("plugin-a","saved",1),("plugin-b","other",0),("plugin-b","saved",1)] };
        let expected = expected.into_iter().map(|(owner,key,index)| (owner.to_owned(),key.to_owned(),index)).collect::<Vec<_>>();
        assert_eq!(rows(&store), expected);
        assert_eq!(unpublished(&mut store), Vec::<UnitKey>::new());
        drop(store);
        let mut store = PersistentStore::open(directory.path()).unwrap();
        assert_eq!(rows(&store), expected);
        store.undo_repair(&journal, repaired.revision).unwrap();
        assert!(rows(&store).iter().all(|(_,_,index)| *index == 0));
        assert_eq!(unpublished(&mut store), Vec::<UnitKey>::new());
    }
}
