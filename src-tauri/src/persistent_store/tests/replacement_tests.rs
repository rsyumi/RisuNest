use super::*;

#[test]
fn activation_leaves_the_previous_library_rows_in_place_for_the_purge() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let seed = stage_root(&mut store, "Original");
    store.replace_commit(&seed, Some(0)).unwrap();
    let stage = stage_root(&mut store, "Replacement");
    for (generation, value) in [(seed.as_str(), "1"), (stage.as_str(), "2")] {
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
        .execute_batch(&format!(
            "CREATE TRIGGER reject_previous_update BEFORE UPDATE ON plugin_storage WHEN OLD.generation='{seed}'
             BEGIN SELECT RAISE(ABORT,'synthetic previous library update'); END;
             CREATE TRIGGER reject_previous_delete BEFORE DELETE ON plugin_storage WHEN OLD.generation='{seed}'
             BEGIN SELECT RAISE(ABORT,'synthetic previous library delete'); END;"
        ))
        .unwrap();
    store.replace_commit(&stage, Some(1)).unwrap();
    assert_eq!(store.revision().unwrap(), 2);
    for (generation, value) in [(seed.as_str(), "1"), (stage.as_str(), "2")] {
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
    store
        .connection
        .execute_batch("DROP TRIGGER reject_previous_update; DROP TRIGGER reject_previous_delete;")
        .unwrap();
    while store.purge_retired_batch(100).unwrap() {}
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM plugin_storage WHERE generation=?1",
                [&seed],
                |row| row.get(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM plugin_storage WHERE generation=?1 AND value='2'",
                [&stage],
                |row| row.get(0),
            )
            .unwrap(),
        513
    );
}

#[test]
fn ordinary_replacement_retry_returns_its_receipt_without_replacing_later_edits() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let seed = stage_root(&mut store, "Original");
    store.replace_commit(&seed, Some(0)).unwrap();
    let stage = stage_root(&mut store, "Replacement");
    assert_eq!(store.replace_commit(&stage, Some(1)).unwrap().revision, 2);
    store.commit(&WorkingSetCommit {
        root: Some(json!({"username":"Later edit"})),
        ..empty_working_set_commit(2)
    }).unwrap();
    let before = serde_json::to_value(store.lww_clock_state().unwrap()).unwrap();
    let pending = serde_json::to_value(store.lww_read_outbox(store.lww_binding_authority().unwrap(), 100).unwrap()).unwrap();
    assert_eq!(store.replace_commit(&stage, Some(1)).unwrap().revision, 2);
    assert_eq!(store.revision().unwrap(), 3);
    assert_eq!(store.read_root(None).unwrap().value["username"], "Later edit");
    assert_eq!(serde_json::to_value(store.lww_clock_state().unwrap()).unwrap(), before);
    assert_eq!(serde_json::to_value(store.lww_read_outbox(store.lww_binding_authority().unwrap(), 100).unwrap()).unwrap(), pending);
    assert!(store.replace_commit(&stage, Some(2)).is_err());
    assert!(store.replace_put_root(&stage, &json!({"username":"Reused stage"})).is_err());
    assert_eq!(store.replace_commit(&stage, Some(1)).unwrap().revision, 2);
    store.device_store_mut().unwrap().connection().execute(
        "UPDATE lww_clock SET binding_authority='1' WHERE singleton=1", [],
    ).unwrap();
    assert!(store.replace_commit(&stage, Some(1)).is_err());
    assert_eq!(store.read_root(None).unwrap().value["username"], "Later edit");
}

#[test]
fn unfinished_ordinary_replacement_rejects_changed_stage_before_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let seed = stage_root(&mut store, "Original");
    store.replace_commit(&seed, Some(0)).unwrap();
    let stage = stage_root(&mut store, "Replacement");
    store.connection.execute_batch(
        "CREATE TRIGGER reject_replacement BEFORE UPDATE ON generations
         BEGIN SELECT RAISE(ABORT,'synthetic replacement failure'); END;",
    ).unwrap();
    assert!(store.replace_commit(&stage, Some(1)).is_err());
    let before = serde_json::to_value(store.lww_clock_state().unwrap()).unwrap();
    store.replace_put_root(&stage, &json!({"username":"Changed staged input"})).unwrap();
    store.connection.execute_batch("DROP TRIGGER reject_replacement").unwrap();
    assert!(store.replace_commit(&stage, Some(1)).is_err());
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(store.read_root(None).unwrap().value["username"], "Original");
    assert_eq!(serde_json::to_value(store.lww_clock_state().unwrap()).unwrap(), before);
    drop(store);
    let mut store = PersistentStore::open(directory.path()).unwrap();
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(store.read_root(None).unwrap().value["username"], "Original");
    assert_eq!(store.device_store().unwrap().connection().query_row(
        "SELECT failures FROM lww_intent_failures WHERE request_id=?1", [&stage], |row| row.get::<_, i64>(0),
    ).unwrap(), 1);
    assert!(store.replace_commit(&stage, Some(1)).is_err());
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(store.read_root(None).unwrap().value["username"], "Original");
}

#[test]
fn ordinary_replacement_receipt_rejects_another_source_kind_even_with_matching_digests() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let stage = stage_root(&mut store, "Original");
    store.replace_commit(&stage, Some(0)).unwrap();
    let body: String = store.device_store().unwrap().connection().query_row(
        "SELECT body FROM lww_intents WHERE request_id=?1", [&stage], |row| row.get(0),
    ).unwrap();
    let mut body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["source_units"], Value::Null);
    body["source_units"] = json!({ "rows": 0, "digest": "0".repeat(64) });
    let body = serde_json::to_string(&body).unwrap();
    let digest = risunest_sync_wire::hash(body.as_bytes());
    store.device_store().unwrap().connection().execute(
        "UPDATE lww_intents SET body=?2,digest=?3 WHERE request_id=?1",
        params![stage, body, digest],
    ).unwrap();
    store.connection.execute(
        "UPDATE lww_requests SET digest=?2 WHERE request_id=?1", params![stage, digest],
    ).unwrap();
    assert!(store.replace_commit(&stage, Some(0)).is_err());
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(store.read_root(None).unwrap().value["username"], "Original");
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

fn generation_rows(store: &PersistentStore, generation: &str, table: &str, order: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let mut statement = store
        .connection
        .prepare(&format!("SELECT * FROM {table} WHERE generation = ?1 ORDER BY {order}"))
        .unwrap();
    let columns = statement.column_count();
    statement
        .query_map([generation], |row| {
            (0..columns)
                .map(|index| {
                    // The generation names the staging itself, never staged content.
                    row.get::<_, rusqlite::types::Value>(index).map(|value| match value {
                        rusqlite::types::Value::Text(text) if text == generation => {
                            rusqlite::types::Value::Text("<generation>".into())
                        }
                        value => value,
                    })
                })
                .collect()
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn assert_same_staged_tables(store: &PersistentStore, left: &str, right: &str, tables: &[(&str, &str)]) {
    for (table, order) in tables {
        let rows = generation_rows(store, left, table, order);
        assert!(!rows.is_empty(), "{table} has staged rows");
        assert_eq!(rows, generation_rows(store, right, table, order), "{table}");
    }
}

// Characters whose chats take their recency from lastDate, from the last
// message, and from neither.
fn paged_characters() -> Vec<Value> {
    let mut characters = fixture()["characters"].as_array().unwrap().clone();
    let chats = characters[1]["chats"].as_array_mut().unwrap();
    chats[0].as_object_mut().unwrap().remove("lastDate");
    chats[1].as_object_mut().unwrap().remove("lastDate");
    let last = chats[1]["message"].as_array_mut().unwrap().last_mut().unwrap();
    last.as_object_mut().unwrap().remove("time");
    characters
}

fn stage_character_in_pieces(store: &mut PersistentStore, staging: &str, character: &Value, page: usize) {
    let mut detail = character.as_object().unwrap().clone();
    let chats = detail.shift_remove("chats").and_then(|chats| chats.as_array().cloned()).unwrap_or_default();
    store
        .replace_put_character_detail(staging, &Value::Object(detail), chats.len() as i64)
        .unwrap();
    let character_id = character["chaId"].as_str().unwrap();
    for (index, chat) in chats.iter().enumerate() {
        let mut conversation = chat.as_object().unwrap().clone();
        let messages = conversation
            .shift_remove("message")
            .and_then(|messages| messages.as_array().cloned())
            .unwrap_or_default();
        store
            .replace_put_conversation(
                staging,
                character_id,
                index as i64,
                &Value::Object(conversation),
                messages.len() as i64,
                messages.last().and_then(|message| message.get("time")),
            )
            .unwrap();
        for (number, messages) in messages.chunks(page).enumerate() {
            store
                .replace_add_conversation_messages(
                    staging,
                    character_id,
                    chat["id"].as_str().unwrap(),
                    (number * page) as i64,
                    messages,
                )
                .unwrap();
        }
    }
}

#[test]
fn characters_staged_in_pieces_match_characters_staged_whole() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let characters = paged_characters();
    let whole = store.replace_begin().unwrap().staging_id;
    store.replace_add_characters(&whole, &characters).unwrap();
    let pieces = store.replace_begin().unwrap().staging_id;
    store.replace_add_characters(&pieces, &characters[..1]).unwrap();
    for character in &characters[1..] {
        stage_character_in_pieces(&mut store, &pieces, character, 7);
    }

    assert_same_staged_tables(&store, &whole, &pieces, &[
        ("characters", "character_id"),
        ("conversations", "character_id, conversation_id"),
        ("messages", "character_id, conversation_id, message_index"),
    ]);
    assert_eq!(
        super::super::query::materialize_staging(&store.connection, &pieces).unwrap(),
        super::super::query::materialize_staging(&store.connection, &whole).unwrap()
    );
    let recency: Vec<(String, i64)> = store
        .connection
        .prepare("SELECT conversation_id, recent_at FROM conversations WHERE generation = ?1 AND character_id = 'char-a' ORDER BY configured_index")
        .unwrap()
        .query_map([&pieces], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(recency, [("conv-long".to_owned(), 1_700_000_000_129), ("conv-short".to_owned(), 0)]);

    // A whole character stores the exact text of its object without `chats`,
    // wherever `chats` sits and whether or not it is present.
    let ordered = store.replace_begin().unwrap().staging_id;
    let extra = [
        json!({ "type": "character", "chaId": "chats-last", "name": "Last", "desc": "", "chats": [] }),
        json!({ "type": "character", "chaId": "no-chats", "name": "None", "desc": "" }),
    ];
    store.replace_add_characters(&ordered, &extra).unwrap();
    let staged = characters.iter().map(|character| (&whole, character));
    for (generation, character) in staged.chain(extra.iter().map(|character| (&ordered, character))) {
        let mut detail = character.as_object().unwrap().clone();
        detail.shift_remove("chats");
        let stored: String = store
            .connection
            .query_row(
                "SELECT detail FROM characters WHERE generation = ?1 AND character_id = ?2",
                params![generation, character["chaId"].as_str().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, serde_json::to_string(&detail).unwrap(), "{}", character["chaId"]);
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    let many = store.replace_begin().unwrap().staging_id;
    let mut steps = Vec::new();
    for index in 0..256 {
        let counted = Arc::new(AtomicUsize::new(0));
        let counter = counted.clone();
        store.connection.progress_handler(1, Some(move || { counter.fetch_add(1, Ordering::Relaxed); false })).unwrap();
        store
            .replace_put_character_detail(&many, &json!({ "chaId": format!("many-{index}"), "name": "Many", "type": "character" }), 0)
            .unwrap();
        store.connection.progress_handler(1, None::<fn() -> bool>).unwrap();
        steps.push(counted.load(Ordering::Relaxed));
    }
    assert!(steps[255] <= steps[0] * 2, "{steps:?}");
}

#[test]
fn conversations_staged_in_pieces_reject_the_chat_ids_whole_characters_reject() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    for ids in [["same", "same"], ["", "other"]] {
        let mut character = paged_characters().remove(1);
        let chats = character["chats"].as_array_mut().unwrap();
        for (chat, id) in chats.iter_mut().zip(ids) {
            chat["id"] = json!(id);
        }
        let whole = store.replace_begin().unwrap().staging_id;
        assert!(store.replace_add_characters(&whole, &[character.clone()]).is_err());

        let pieces = store.replace_begin().unwrap().staging_id;
        let mut detail = character.as_object().unwrap().clone();
        let chats = detail.shift_remove("chats").unwrap();
        store.replace_put_character_detail(&pieces, &Value::Object(detail), 2).unwrap();
        let staged = chats.as_array().unwrap().iter().enumerate().map(|(index, chat)| {
            let mut conversation = chat.as_object().unwrap().clone();
            conversation.shift_remove("message");
            store.replace_put_conversation(&pieces, "char-a", index as i64, &Value::Object(conversation), 0, None)
        });
        assert!(staged.collect::<Result<Vec<_>, _>>().is_err());
    }
}

#[test]
fn appended_preset_batches_stage_like_one_preset_list() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let mut presets = fixture()["botPresets"].as_array().unwrap().clone();
    presets.push(json!({ "id": "preset-gamma", "name": "Gamma" }));
    let whole = store.replace_begin().unwrap().staging_id;
    store.replace_put_presets(&whole, &presets).unwrap();
    let pieces = store.replace_begin().unwrap().staging_id;
    store.replace_put_presets(&pieces, &presets[..1]).unwrap();
    store.replace_add_presets(&pieces, &presets[1..]).unwrap();

    assert_same_staged_tables(&store, &whole, &pieces, &[("bot_presets", "preset_id")]);
    assert!(store.replace_add_presets(&pieces, &presets[..1]).is_err());
    let twice = [json!({ "id": "preset-delta" }), json!({ "id": "preset-delta" })];
    assert!(store.replace_add_presets(&pieces, &twice).is_err());
    assert_same_staged_tables(&store, &whole, &pieces, &[("bot_presets", "preset_id")]);
}

#[test]
fn appended_plugin_storage_batches_stage_like_the_whole_root() {
    use crate::persistent_store::{plugin_owner::UNOWNED_OWNER, PluginStorageValue};
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let root = json!({ "username": "synthetic" });

    let values = [
        ("synthetic-plugin", "a", json!(1)),
        (UNOWNED_OWNER, "b", json!("two")),
        ("other-plugin", "a", json!({ "x": 3 })),
    ]
    .map(|(owner, key, value)| PluginStorageValue { owner: owner.into(), key: key.into(), value });
    let whole = store.replace_begin().unwrap().staging_id;
    store.replace_put_root_with_plugin_storage(&whole, &root, Some(&values)).unwrap();
    let pieces = store.replace_begin().unwrap().staging_id;
    store.replace_put_root_with_plugin_storage(&pieces, &root, Some(&[])).unwrap();
    store.replace_add_plugin_storage_values(&pieces, &values[..1]).unwrap();
    store.replace_add_plugin_storage_values(&pieces, &values[1..]).unwrap();
    assert_same_staged_tables(&store, &whole, &pieces, &[
        ("root", "generation"),
        ("plugin_storage", "ordinal"),
    ]);

    let mut embedded = root.clone();
    embedded["pluginCustomStorage"] = json!({ "a": 1, "b": "two", "c": { "x": 3 } });
    embedded["pluginStorageMeta"] = json!({
        "a": { "plugin": "synthetic-plugin" },
        "c": { "plugin": "other-plugin" },
    });
    let whole = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&whole, &embedded).unwrap();
    let pieces = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&pieces, &root).unwrap();
    let batch = |value: Value| value.as_object().unwrap().clone();
    store
        .replace_add_plugin_storage(&pieces, &batch(json!({ "a": 1 })), Some(&batch(json!({ "a": { "plugin": "synthetic-plugin" } }))))
        .unwrap();
    store
        .replace_add_plugin_storage(&pieces, &batch(json!({ "b": "two", "c": { "x": 3 } })), Some(&batch(json!({ "c": { "plugin": "other-plugin" } }))))
        .unwrap();
    assert_same_staged_tables(&store, &whole, &pieces, &[
        ("root", "generation"),
        ("plugin_storage", "ordinal"),
    ]);
}

#[test]
fn database_only_replacement_drops_forwarded_aliases_that_no_staged_value_names() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let aliases = [
        ("asset", "assets/root.bin"), ("asset", "assets/preset.bin"), ("asset", "assets/character.bin"),
        ("asset", "assets/conversation.bin"), ("inlay", "message-inlay"), ("asset", "assets/key-only.bin"),
        ("asset", "assets/unnamed.bin"),
    ]
    .map(|(kind, key)| AssetAlias {
        key: key.to_owned(),
        object_hash: Some("ab".repeat(32)),
        kind: kind.to_owned(),
        size: 1,
        mime: "application/octet-stream".to_owned(),
        name: key.to_owned(),
        ext: "bin".to_owned(),
        inlay_type: (kind == "inlay").then(|| "image".to_owned()),
        width: None,
        height: None,
        metadata: json!({}),
    });
    let initial = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&initial, &json!({})).unwrap();
    store.replace_put_asset_aliases(&initial, &aliases).unwrap();
    store.replace_commit(&initial, Some(0)).unwrap();
    let character = json!({
        "type": "character", "chaId": "named", "name": "Synthetic",
        "emotionImages": [["happy", "assets/character.bin"]],
        "notes": { "assets/key-only.bin": true },
        "chats": [{
            "id": "chat", "name": "Chat", "extra": { "background": "assets/conversation.bin" },
            "message": [{ "role": "user", "data": "see {{inlay::message-inlay}}", "chatId": "m1" }]
        }]
    });
    let all = aliases.iter().map(|alias| alias.key.as_str()).collect::<Vec<_>>();
    // A character whose additionalAssets is not an array, or a module or an
    // embedded module whose assets is not, leaves the scan incomplete.
    let listed = json!({ "modules": [{ "id": "module", "assets": [] }], "personas": [{ "id": "persona", "embeddedModule": { "id": "embedded" } }] });
    let module = json!({ "modules": [{ "id": "module", "assets": {} }] });
    let persona = json!({ "personas": [{ "id": "persona", "embeddedModule": { "id": "embedded", "assets": {} } }] });
    let no_module = json!({ "personas": [{ "id": "persona", "embeddedModule": null }] });
    for (additional_assets, root, kept) in [
        (json!({}), listed.clone(), all.clone()),
        (json!([]), module, all.clone()),
        (json!([]), persona, all.clone()),
        (json!([]), no_module, all.clone()),
        (json!([]), listed, all[..5].to_vec()),
    ] {
        let revision = store.revision().unwrap();
        let stage = store.replace_begin().unwrap().staging_id;
        let mut root = root;
        root["activeAsset"] = json!("assets/root.bin");
        store.replace_put_root(&stage, &root).unwrap();
        store.replace_put_presets(&stage, &[json!({ "id": "preset", "name": "Preset", "mainPrompt": "assets/preset.bin" })]).unwrap();
        let mut character = character.clone();
        character["additionalAssets"] = additional_assets;
        store.replace_add_characters(&stage, &[character]).unwrap();
        store.replace_preserve_repositories(&stage, Some(revision)).unwrap();
        store.replace_commit(&stage, Some(revision)).unwrap();
        let surviving = aliases
            .iter()
            .filter(|alias| store.read_asset_alias(&alias.kind, &alias.key, None).unwrap().is_some())
            .map(|alias| alias.key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(surviving, kept);
    }
}

#[test]
fn database_only_replacement_keeps_character_owner_heads_only_for_unchanged_assets() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let character = |id: &str, assets: Option<Value>| {
        let mut character = json!({ "type": "character", "chaId": id, "name": id, "chats": [] });
        if let Some(assets) = assets { character["additionalAssets"] = assets; }
        character
    };
    let owner = |id: &str| AssetOwnerLocator::CharacterAdditionalAssets { character_id: id.to_owned() };
    let module = |id: &str| AssetOwnerLocator::RootModuleAssets { module_id: id.to_owned() };
    let embedded = |persona: &str, id: &str| AssetOwnerLocator::PersonaEmbeddedModuleAssets { persona_id: persona.to_owned(), module_id: id.to_owned() };
    let heads = [
        ("kept", AssetOwnerHead::present(owner("kept"), "11".repeat(32), 1)),
        ("changed", AssetOwnerHead::present(owner("changed"), "22".repeat(32), 1)),
        ("absent", AssetOwnerHead::absent(owner("absent"))),
        ("malformed", AssetOwnerHead::absent(owner("malformed"))),
        ("kept-module", AssetOwnerHead::present(module("kept-module"), "33".repeat(32), 1)),
        ("changed-module", AssetOwnerHead::present(module("changed-module"), "44".repeat(32), 1)),
        ("kept-embedded", AssetOwnerHead::absent(embedded("kept-persona", "kept-embedded"))),
        ("changed-embedded", AssetOwnerHead::present(embedded("changed-persona", "changed-embedded"), "55".repeat(32), 1)),
    ];
    let root = |changed: &str| json!({
        "modules": [
            { "id": "kept-module", "assets": [["k", "assets/k", "bin"]] },
            { "id": "changed-module", "assets": [[changed, "assets/m", "bin"]] }
        ],
        "personas": [
            { "id": "kept-persona", "embeddedModule": { "id": "kept-embedded" } },
            { "id": "changed-persona", "embeddedModule": { "id": "changed-embedded", "assets": [[changed, "assets/p", "bin"]] } }
        ]
    });
    let initial = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&initial, &root("before")).unwrap();
    store.replace_add_characters(&initial, &[
        character("kept", Some(json!([["a", "assets/a", "bin"]]))),
        character("changed", Some(json!([["b", "assets/b", "bin"]]))),
        character("absent", None),
        character("malformed", None),
    ]).unwrap();
    store.replace_put_asset_owner_heads(&initial, &heads.clone().map(|(_, head)| head)).unwrap();
    store.replace_commit(&initial, Some(0)).unwrap();
    // Stored text that another writer left with repeated keys is read like a
    // parse, which keeps the last value of each.
    let active = crate::persistent_store::active_generation(&store.connection).unwrap();
    store.connection.execute(
        r#"UPDATE root SET value = replace(replace(value, '{"id":"kept-module"', '{"id":"stale","id":"kept-module"'),
            '"embeddedModule":{"id":"kept-embedded"}', '"embeddedModule":{"id":"stale","id":"kept-embedded"}')
         WHERE generation = ?1"#,
        [&active],
    ).unwrap();
    let repeated: i64 = store.connection.query_row(
        r#"SELECT (length(value) - length(replace(value, '"stale"', ''))) / 7 FROM root WHERE generation = ?1"#,
        [&active], |row| row.get(0)).unwrap();
    assert_eq!(repeated, 2);
    let stage = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&stage, &root("after")).unwrap();
    let mut kept = character("kept", Some(json!([["a", "assets/a", "bin"]])));
    kept["name"] = json!("Renamed");
    store.replace_add_characters(&stage, &[
        kept,
        character("changed", Some(json!([["c", "assets/c", "bin"]]))),
        character("absent", None),
        character("malformed", Some(json!({}))),
    ]).unwrap();
    store.replace_preserve_repositories(&stage, Some(1)).unwrap();
    store.replace_commit(&stage, Some(1)).unwrap();
    let retained = heads
        .iter()
        .filter(|(_, head)| store.read_asset_owner_head(&head.owner, None).unwrap().is_some_and(|stored| stored.value == *head))
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    assert_eq!(retained, ["kept", "absent", "kept-module", "kept-embedded"]);
}
