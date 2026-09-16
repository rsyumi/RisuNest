use super::*;
use crate::persistent_store::plugin_owner::UNOWNED_OWNER;

#[test]
fn root_mutations_preserve_unchanged_fields_and_leased_revision() {
    let (_directory, mut store, _) = open_fixture();
    let before = store.read_root(None).unwrap();
    let lease = store.acquire_revision(before.revision).unwrap();
    let input: WorkingSetCommit = serde_json::from_value(json!({
        "expectedRevision": before.revision,
        "rootMutations": [
            { "type": "set", "key": "username", "value": "Changed" },
            { "type": "set", "key": "__proto__", "value": {"nested": [1]} },
            { "type": "set", "key": "nullable", "value": null },
            { "type": "delete", "key": "missing" }
        ]
    }))
    .unwrap();
    let revision = store.commit(&input).unwrap().revision;
    let mut expected = before.value.clone();
    expected["username"] = json!("Changed");
    expected["__proto__"] = json!({"nested": [1]});
    expected["nullable"] = Value::Null;
    assert_eq!(store.read_root(None).unwrap().value, expected);
    assert_eq!(
        store.read_root(Some(&lease.lease)).unwrap().value,
        before.value
    );
    assert!(matches!(
        store.commit(&input),
        Err(StoreError::RevisionConflict { .. })
    ));
    assert_eq!(store.read_root(None).unwrap().revision, revision);
    store.release_revision(&lease.lease).unwrap();
}

#[test]
fn invalid_root_mutations_do_not_change_state() {
    let (_directory, mut store, _) = open_fixture();
    let before = store.read_root(None).unwrap();
    for changes in [
        json!({"root": {}, "rootMutations": []}),
        json!({"rootMutations": [{"type":"delete", "key":"characters"}]}),
        json!({"rootMutations": [{"type":"delete", "key":"botPresets"}]}),
        json!({"rootMutations": [{"type":"delete", "key":"pluginCustomStorage"}]}),
        json!({"rootMutations": [
            {"type":"set", "key":"username", "value":"Temporary"},
            {"type":"delete", "key":"username"}
        ]}),
    ] {
        let mut value = changes;
        value["expectedRevision"] = json!(before.revision);
        let input: WorkingSetCommit = serde_json::from_value(value).unwrap();
        assert!(matches!(
            store.commit(&input),
            Err(StoreError::Validation { .. })
        ));
        let after = store.read_root(None).unwrap();
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.value, before.value);
    }
}

#[test]
fn preset_catalog_reads_and_materializes_in_configured_order() {
    let (_directory, mut store, database) = open_fixture();

    assert!(store
        .read_root(None)
        .expect("read root")
        .value
        .get("botPresets")
        .is_none());
    assert_eq!(
        serde_json::to_value(store.query_presets(None).expect("query presets"))
            .expect("serialize preset catalog"),
        json!({
            "revision": 1,
            "items": [
                { "id": "0", "name": "Preset Beta", "image": "preset-beta.png", "configuredIndex": 0 },
                { "id": "1", "name": "Preset Alpha", "configuredIndex": 1 }
            ]
        })
    );
    assert_eq!(
        store
            .read_preset("1", None)
            .expect("read preset")
            .expect("preset exists")
            .value,
        database["botPresets"][1]
    );

    let lease = store.acquire_revision(1).expect("acquire preset lease");
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Preset commit", "botPresets": ["strip"] })),
            replace_presets: Some(vec![json!({ "name": "Replacement" })]),
            ..empty_working_set_commit(1)
        })
        .expect("replace presets");
    assert_eq!(
        store.materialize(None).expect("materialize replacement")["botPresets"],
        json!([{ "name": "Replacement" }])
    );
    assert_eq!(
        store
            .query_presets(Some(&lease.lease))
            .expect("query leased presets")
            .items[0]
            .name,
        "Preset Beta"
    );
    store
        .release_revision(&lease.lease)
        .expect("release preset lease");
    assert!(matches!(
        store.query_presets(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn plugin_storage_is_revisioned_per_key_and_lease_isolated() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let mut database = fixture();
    database["pluginCustomStorage"] = json!({
        "alpha": "old",
        "beta": { "enabled": true }
    });
    let staging = store.replace_begin().expect("begin staged replacement");
    store
        .replace_put_root(&staging.staging_id, &staged_root(&database))
        .expect("stage plugin storage");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"].as_array().expect("fixture presets"),
        )
        .expect("stage presets");
    store
        .replace_add_characters(
            &staging.staging_id,
            database["characters"]
                .as_array()
                .expect("fixture characters"),
        )
        .expect("stage characters");
    let imported = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("commit plugin storage");

    assert!(store
        .read_root(None)
        .expect("read stripped root")
        .value
        .get("pluginCustomStorage")
        .is_none());
    assert_eq!(
        serde_json::to_value(
            store
                .query_plugin_storage(None)
                .expect("query plugin storage")
        )
        .expect("serialize plugin catalog"),
        json!({
            "revision": 1,
            "items": [
                { "owner": UNOWNED_OWNER, "key": "alpha", "byteSize": 5 },
                { "owner": UNOWNED_OWNER, "key": "beta", "byteSize": 16 }
            ]
        })
    );
    assert_eq!(
        store
            .read_plugin_storage(UNOWNED_OWNER, "beta", None)
            .expect("read plugin key")
            .expect("plugin key exists")
            .value,
        json!({ "enabled": true })
    );
    let lease = store
        .acquire_revision(imported.revision)
        .expect("acquire plugin lease");

    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Plugin commit" })),
            plugin_storage: Some(vec![
                PluginStorageMutation::Set {
                    owner: UNOWNED_OWNER.to_owned(),
                    key: "alpha".to_owned(),
                    value: json!("new"),
                },
                PluginStorageMutation::Delete {
                    owner: UNOWNED_OWNER.to_owned(),
                    key: "beta".to_owned(),
                },
            ]),
            ..empty_working_set_commit(imported.revision)
        })
        .expect("mutate plugin storage");

    assert_eq!(
        store
            .read_plugin_storage(UNOWNED_OWNER, "alpha", Some(&lease.lease))
            .expect("read leased plugin key")
            .expect("leased plugin key exists")
            .value,
        json!("old")
    );
    assert_eq!(
        store.materialize(None).expect("materialize plugin storage")["pluginCustomStorage"],
        json!({ "alpha": "new" })
    );
    store
        .release_revision(&lease.lease)
        .expect("release plugin lease");
    assert!(matches!(
        store.read_plugin_storage(UNOWNED_OWNER, "alpha", Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn plugin_storage_preserves_legacy_object_key_order_across_reopen() {
    let directory = tempfile::tempdir().expect("create plugin order directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let staging = store.replace_begin().expect("begin ordered replacement");
    let mut storage = serde_json::Map::new();
    storage.insert("zeta".to_owned(), json!("first string"));
    storage.insert("10".to_owned(), json!("ten"));
    storage.insert("2".to_owned(), json!(0));
    storage.insert("01".to_owned(), json!("non-index"));
    storage.insert("4294967294".to_owned(), json!(true));
    storage.insert("4294967295".to_owned(), json!(false));
    storage.insert("\u{ffff}x".to_owned(), json!("unicode"));
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({ "pluginCustomStorage": Value::Object(storage) }),
        )
        .expect("stage ordered plugin storage");
    let imported = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("commit ordered plugin storage");
    let original_order = vec![
        "2",
        "10",
        "4294967294",
        "zeta",
        "01",
        "4294967295",
        "\u{ffff}x",
    ];
    assert_eq!(
        store
            .query_plugin_storage(None)
            .expect("query ordered storage")
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        original_order
    );
    assert_eq!(
        store
            .read_plugin_storage(UNOWNED_OWNER, "\u{ffff}x", None)
            .expect("read unicode key")
            .expect("unicode key exists")
            .value,
        json!("unicode")
    );

    let updated = store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![
                PluginStorageMutation::Set {
                    owner: UNOWNED_OWNER.to_owned(),
                    key: "zeta".to_owned(),
                    value: json!("updated"),
                },
                PluginStorageMutation::Delete {
                    owner: UNOWNED_OWNER.to_owned(),
                    key: "zeta".to_owned(),
                },
                PluginStorageMutation::Set {
                    owner: UNOWNED_OWNER.to_owned(),
                    key: "zeta".to_owned(),
                    value: json!("reinserted"),
                },
            ]),
            ..empty_working_set_commit(imported.revision)
        })
        .expect("reinsert string key");
    drop(store);

    let reopened = PersistentStore::open(directory.path()).expect("reopen ordered storage");
    let expected = vec![
        "2",
        "10",
        "4294967294",
        "01",
        "4294967295",
        "\u{ffff}x",
        "zeta",
    ];
    assert_eq!(
        reopened
            .query_plugin_storage(None)
            .expect("query reopened ordered storage")
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        reopened
            .materialize(Some(updated.revision))
            .expect("materialize ordered storage")["pluginCustomStorage"]
            .as_object()
            .expect("plugin storage object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn ordinary_root_commits_do_not_replace_plugin_records_and_empty_materializes() {
    let directory = tempfile::tempdir().expect("create root semantics directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    assert_eq!(
        store.materialize(None).expect("materialize empty store")["pluginCustomStorage"],
        json!({})
    );
    let staging = store.replace_begin().expect("begin plugin replacement");
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({ "pluginCustomStorage": { "retained": 0 } }),
        )
        .expect("stage plugin replacement");
    let imported = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("commit plugin replacement");

    store
        .commit(&WorkingSetCommit {
            root: Some(json!({
                "username": "ordinary root",
                "pluginCustomStorage": { "incidental": "ignored" }
            })),
            ..empty_working_set_commit(imported.revision)
        })
        .expect("commit ordinary root");

    assert_eq!(
        store
            .materialize(None)
            .expect("materialize retained storage")["pluginCustomStorage"],
        json!({ "retained": 0 })
    );
}

fn message(id: &str) -> Value {
    json!({ "role": "user", "data": id, "chatId": id, "time": 1_800_000_000_000i64 })
}

fn commit(store: &mut PersistentStore, revision: i64, mutation: ConversationMutation) -> i64 {
    store
        .commit(&WorkingSetCommit {
            conversations: Some(vec![mutation]),
            ..empty_working_set_commit(revision)
        })
        .expect("commit conversation mutation")
        .revision
}

#[test]
fn opens_new_store_at_revision_zero() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");

    assert_eq!(store.revision().expect("read revision"), 0);
    assert!(directory.path().join("persistent/persistent.sqlite").is_file());
}

#[test]
fn staged_fixture_round_trips_through_materialize() {
    let (_directory, store, database) = open_fixture();

    assert_eq!(
        store.read_root(None).expect("read root").value,
        root(&database)
    );
    assert_eq!(
        store.materialize(None).expect("materialize fixture"),
        database
    );
}

#[test]
fn pinned_materialization_is_not_affected_by_active_changes() {
    let (_directory, mut store, database) = open_fixture();
    let lease = store
        .acquire_revision(1)
        .expect("acquire materialization lease");
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Changed after lease" })),
            ..empty_working_set_commit(1)
        })
        .expect("change active generation after lease");

    assert_ne!(
        store.materialize(None).expect("materialize active data"),
        database
    );
    assert_eq!(
        store
            .materialize_lease(&lease.lease)
            .expect("materialize pinned generation"),
        database
    );
    store
        .release_revision(&lease.lease)
        .expect("release materialization lease");
    assert!(matches!(
        store.materialize_lease(&lease.lease),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn staged_materialization_is_exact_and_inaccessible_after_abort_or_reopen() {
    let (directory, mut store, active_database) = open_fixture();
    let aborted = store.replace_begin().expect("begin staged materialization");
    store
        .replace_put_root(
            &aborted.staging_id,
            &json!({
                "username": "Staged database",
                "pluginCustomStorage": { "staged": 0 }
            }),
        )
        .expect("write staged materialization root");

    assert_eq!(
        store
            .materialize_staging(&aborted.staging_id)
            .expect("materialize exact staging generation"),
        json!({
            "username": "Staged database",
            "characters": [],
            "botPresets": [],
            "pluginCustomStorage": { "staged": 0 }
        })
    );
    assert_eq!(
        store
            .materialize(None)
            .expect("materialize active generation"),
        active_database
    );

    store
        .replace_abort(&aborted.staging_id)
        .expect("abort staged materialization");
    assert!(matches!(
        store.materialize_staging(&aborted.staging_id),
        Err(StoreError::Validation { .. })
    ));

    let abandoned = store
        .replace_begin()
        .expect("begin abandoned staged materialization");
    store
        .replace_put_root(
            &abandoned.staging_id,
            &json!({ "username": "Abandoned database" }),
        )
        .expect("write abandoned staged root");
    drop(store);

    let reopened = PersistentStore::open(directory.path()).expect("reopen persistent store");
    assert!(matches!(
        reopened.materialize_staging(&abandoned.staging_id),
        Err(StoreError::Validation { .. })
    ));
}

#[test]
fn character_catalog_honors_order_search_trash_and_cursor() {
    let (_directory, store, _) = open_fixture();
    let configured = |search: Option<&str>, trash, cursor: Option<&str>| CharacterQuery {
        search: search.map(str::to_owned),
        order: QueryOrder::Configured,
        trash,
        limit: 1,
        cursor: cursor.map(str::to_owned),
    };

    let first = store
        .query_characters(&configured(None, false, None), None)
        .expect("first configured page");
    assert_eq!(first.items[0].id, "char-b");
    assert_eq!(first.next_cursor.as_deref(), Some("1"));
    assert_eq!(
        store
            .query_characters(&configured(None, false, first.next_cursor.as_deref()), None)
            .expect("second configured page")
            .items[0]
            .id,
        "char-a"
    );
    assert_eq!(
        store
            .query_characters(&configured(Some("LPH"), false, None), None)
            .expect("search catalog")
            .items[0]
            .id,
        "char-a"
    );
    assert_eq!(
        store
            .query_characters(
                &CharacterQuery {
                    search: None,
                    order: QueryOrder::Recent,
                    trash: true,
                    limit: 10,
                    cursor: None,
                },
                None,
            )
            .expect("trashed catalog")
            .items[0]
            .id,
        "char-c"
    );
    assert_eq!(
        store
            .query_characters(
                &CharacterQuery {
                    search: None,
                    order: QueryOrder::Recent,
                    trash: false,
                    limit: 10,
                    cursor: None,
                },
                None,
            )
            .expect("recent catalog")
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["char-a", "char-b"]
    );
    let detail = store
        .read_character("char-a", None)
        .expect("read character detail")
        .expect("character exists");
    assert_eq!(detail.value["chaId"], "char-a");
    assert!(detail.value.get("chats").is_none());
}

#[test]
fn character_search_uses_rust_unicode_lowercase_matching() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    store
        .commit(&WorkingSetCommit {
            add_character: Some(json!({
                "type": "character",
                "chaId": "unicode-name",
                "name": "Éclair",
                "chats": []
            })),
            ..empty_working_set_commit(0)
        })
        .expect("add character with Unicode name");

    let page = store
        .query_characters(
            &CharacterQuery {
                search: Some("éCL".to_owned()),
                order: QueryOrder::Configured,
                trash: false,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("search Unicode character name");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "unicode-name");
}

#[test]
fn conversation_catalog_honors_configured_recent_and_cursor() {
    let (_directory, store, _) = open_fixture();
    let query = |order, cursor: Option<&str>| ConversationQuery {
        character_id: "char-a".to_owned(),
        order,
        limit: 1,
        cursor: cursor.map(str::to_owned),
    };

    let first = store
        .query_conversations(&query(QueryOrder::Configured, None), None)
        .expect("first configured conversation page");
    assert_eq!(first.items[0].id, "conv-long");
    assert_eq!(first.next_cursor.as_deref(), Some("1"));
    assert_eq!(
        store
            .query_conversations(
                &query(QueryOrder::Configured, first.next_cursor.as_deref()),
                None,
            )
            .expect("second configured conversation page")
            .items[0]
            .id,
        "conv-short"
    );
    assert_eq!(
        store
            .query_conversations(&query(QueryOrder::Recent, None), None)
            .expect("recent conversation page")
            .items[0]
            .id,
        "conv-long"
    );
    assert_eq!(
        serde_json::to_value(
            store
                .query_conversations(
                    &ConversationQuery {
                        character_id: "missing".to_owned(),
                        order: QueryOrder::Configured,
                        limit: 10,
                        cursor: None,
                    },
                    None,
                )
                .expect("empty conversation page")
        )
        .expect("serialize empty conversation page"),
        json!({ "revision": 1, "items": [] })
    );
}

#[test]
fn conversation_catalog_includes_chat_list_metadata() {
    let (_directory, mut store, _) = open_fixture();
    let mut detail = store
        .read_conversation("char-a", "conv-short", None)
        .expect("read conversation")
        .expect("conversation exists")
        .value;
    let detail = detail.as_object_mut().expect("conversation object");
    detail.remove("message");
    detail.insert("folderId".to_owned(), json!("folder-a"));
    detail.insert("bindedPersona".to_owned(), json!("persona-a"));
    detail.insert("fmIndex".to_owned(), json!(-1));
    commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 0,
            delete_count: 0,
            messages: Vec::new(),
            conversation: Some(Value::Object(detail.clone())),
            configured_index: None,
        },
    );

    let page = store
        .query_conversations(
            &ConversationQuery {
                character_id: "char-a".to_owned(),
                order: QueryOrder::Configured,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query conversations");
    let summary = page
        .items
        .iter()
        .find(|item| item.id == "conv-short")
        .expect("conversation summary");

    assert_eq!(
        serde_json::to_value(summary).expect("serialize summary"),
        json!({
            "id": "conv-short",
            "characterId": "char-a",
            "name": "Short chat",
            "folderId": "folder-a",
            "bindedPersona": "persona-a",
            "configuredIndex": 1,
            "recentAt": 250,
            "messageCount": 2,
            "fmIndex": -1
        })
    );
}

#[test]
fn conversation_windows_cover_latest_and_anchor_boundaries() {
    let (_directory, store, _) = open_fixture();
    let window = |anchor: Option<&str>, before, after| ConversationWindowQuery {
        character_id: "char-a".to_owned(),
        conversation_id: "conv-long".to_owned(),
        start_index: None,
        limit: Some(4),
        anchor_message_id: anchor.map(str::to_owned),
        anchor_occurrence: None,
        before,
        after,
    };

    let latest = store
        .read_conversation_window(&window(None, None, None), None)
        .expect("read latest window")
        .expect("long conversation exists");
    assert_eq!(
        (latest.value.start_index, latest.value.end_index),
        (126, 130)
    );
    assert!(latest.value.has_more_before);
    assert!(!latest.value.has_more_after);

    let anchored = store
        .read_conversation_window(&window(Some("msg-127"), Some(2), Some(1)), None)
        .expect("read anchored window")
        .expect("long conversation exists");
    assert_eq!(
        (anchored.value.start_index, anchored.value.end_index),
        (125, 129)
    );
    assert_eq!(anchored.value.messages[2]["chatId"], "msg-127");
    assert!(anchored.value.has_more_before);
    assert!(anchored.value.has_more_after);
}

#[test]
fn conversation_windows_resolve_last_and_absent_far_duplicate_anchors() {
    let (_directory, mut store, _) = open_fixture();
    let mut first = message("first duplicate");
    first["chatId"] = json!("far-duplicate");
    let mut last = message("last duplicate");
    last["chatId"] = json!("far-duplicate");
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-long".to_owned(),
            start: 1,
            delete_count: 1,
            messages: vec![first],
            conversation: None,
            configured_index: None,
        },
    );
    commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-long".to_owned(),
            start: 128,
            delete_count: 1,
            messages: vec![last],
            conversation: None,
            configured_index: None,
        },
    );

    let first = store
        .read_conversation_window(
            &ConversationWindowQuery {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-long".to_owned(),
                start_index: None,
                limit: None,
                anchor_message_id: Some("far-duplicate".to_owned()),
                anchor_occurrence: None,
                before: Some(0),
                after: Some(0),
            },
            None,
        )
        .expect("read first duplicate anchor")
        .expect("conversation exists");
    let last = store
        .read_conversation_window(
            &ConversationWindowQuery {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-long".to_owned(),
                start_index: None,
                limit: None,
                anchor_message_id: Some("far-duplicate".to_owned()),
                anchor_occurrence: Some(AnchorOccurrence::Last),
                before: Some(0),
                after: Some(0),
            },
            None,
        )
        .expect("read duplicate anchor")
        .expect("conversation exists");
    let absent = store
        .read_conversation_window(
            &ConversationWindowQuery {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-long".to_owned(),
                start_index: None,
                limit: None,
                anchor_message_id: Some("absent".to_owned()),
                anchor_occurrence: Some(AnchorOccurrence::Last),
                before: Some(0),
                after: Some(0),
            },
            None,
        )
        .expect("read absent duplicate anchor");

    assert_eq!((first.value.start_index, first.value.end_index), (1, 2));
    assert_eq!((last.value.start_index, last.value.end_index), (128, 129));
    assert!(absent.is_none());
}

#[test]
fn conversation_windows_support_strict_absolute_ranges() {
    let (_directory, store, _) = open_fixture();
    let range = |start_index, limit| ConversationWindowQuery {
        character_id: "char-a".to_owned(),
        conversation_id: "conv-long".to_owned(),
        start_index,
        limit,
        anchor_message_id: None,
        anchor_occurrence: None,
        before: None,
        after: None,
    };

    for (start_index, limit, expected_start, expected_end, expected_ids) in [
        (0, 3, 0, 3, vec!["msg-000", "msg-001", "msg-002"]),
        (126, 3, 126, 129, vec!["msg-126", "msg-127", "msg-128"]),
        (127, 1, 127, 128, vec!["msg-127"]),
        (128, 10, 128, 130, vec!["msg-128", "msg-129"]),
        (130, 4, 130, 130, vec![]),
        (200, 4, 130, 130, vec![]),
    ] {
        let result = store
            .read_conversation_window(&range(Some(start_index), Some(limit)), None)
            .expect("read absolute range")
            .expect("conversation exists");
        assert_eq!(
            (result.value.start_index, result.value.end_index),
            (expected_start, expected_end)
        );
        assert_eq!(
            result
                .value
                .messages
                .iter()
                .map(|message| message["chatId"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected_ids
        );
    }

    for invalid in [
        range(Some(-1), Some(1)),
        range(Some(0), None),
        range(Some(0), Some(0)),
        range(Some(0), Some(4_097)),
        ConversationWindowQuery {
            anchor_message_id: Some("msg-000".to_owned()),
            ..range(Some(0), Some(1))
        },
    ] {
        assert!(matches!(
            store.read_conversation_window(&invalid, None),
            Err(StoreError::Validation { .. })
        ));
    }
}

#[test]
fn replace_range_supports_append_insert_delete_and_conversation_lifecycle() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 2,
            delete_count: 0,
            messages: vec![message("append")],
            conversation: None,
            configured_index: None,
        },
    );
    let revision = commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 1,
            delete_count: 0,
            messages: vec![message("insert")],
            conversation: None,
            configured_index: None,
        },
    );
    let revision = commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 2,
            delete_count: 1,
            messages: vec![],
            conversation: None,
            configured_index: None,
        },
    );

    let short = store
        .read_conversation("char-a", "conv-short", None)
        .expect("read edited conversation")
        .expect("short conversation exists");
    assert_eq!(
        short.value["message"].as_array().expect("messages").len(),
        3
    );
    assert_eq!(short.value["message"][1]["chatId"], "insert");
    assert_eq!(short.value["message"][2]["chatId"], "append");

    let revision = commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-new".to_owned(),
            start: 0,
            delete_count: 0,
            messages: vec![message("new-message")],
            conversation: Some(json!({
                "id": "conv-new", "name": "New chat", "note": "", "localLore": []
            })),
            configured_index: None,
        },
    );
    assert!(store
        .read_conversation("char-a", "conv-new", None)
        .expect("read new conversation")
        .is_some());
    commit(
        &mut store,
        revision,
        ConversationMutation::Delete {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-new".to_owned(),
        },
    );
    assert!(store
        .read_conversation("char-a", "conv-new", None)
        .expect("read deleted conversation")
        .is_none());
}

#[test]
fn replace_range_creates_conversation_at_explicit_configured_position() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-branch".to_owned(),
            start: 0,
            delete_count: 0,
            messages: vec![
                message("duplicate"),
                json!({ "role": "char", "data": "missing ID" }),
                message("duplicate"),
            ],
            conversation: Some(json!({
                "id": "conv-branch",
                "name": "Long chat (Branch)",
                "note": "branch detail",
                "localLore": [],
                "fmIndex": -1,
                "unknownDetail": { "keep": false }
            })),
            configured_index: Some(0),
        },
    );

    let conversations = store
        .query_conversations(
            &ConversationQuery {
                character_id: "char-a".to_owned(),
                order: QueryOrder::Configured,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query configured conversations");
    assert_eq!(
        conversations
            .items
            .iter()
            .map(|item| (item.id.as_str(), item.configured_index))
            .collect::<Vec<_>>(),
        vec![("conv-branch", 0), ("conv-long", 1), ("conv-short", 2)]
    );
    let branch = store
        .read_conversation("char-a", "conv-branch", None)
        .expect("read branch")
        .expect("branch exists");
    assert_eq!(branch.value["note"], "branch detail");
    assert_eq!(branch.value["unknownDetail"]["keep"], false);
    assert_eq!(branch.value["message"].as_array().unwrap().len(), 3);
    assert!(matches!(
        store.commit(&WorkingSetCommit {
            conversations: Some(vec![ConversationMutation::ReplaceRange {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-branch".to_owned(),
                start: 0,
                delete_count: 0,
                messages: vec![message("must-not-insert")],
                conversation: Some(json!({
                    "id": "conv-branch", "name": "Collision", "note": "", "localLore": []
                })),
                configured_index: Some(0),
            }]),
            ..empty_working_set_commit(revision)
        }),
        Err(StoreError::Validation { .. })
    ));
    assert_eq!(store.revision().expect("unchanged revision"), revision);
}

#[test]
fn cas_conflict_preserves_current_revision() {
    let (_directory, mut store, _) = open_fixture();
    let result = store.commit(&WorkingSetCommit {
        root: Some(json!({ "username": "stale" })),
        ..empty_working_set_commit(0)
    });

    assert!(matches!(
        result,
        Err(StoreError::RevisionConflict {
            expected: 0,
            actual: 1
        })
    ));
    assert_eq!(store.revision().expect("read revision"), 1);
}

#[test]
fn conversation_append_after_deletion_preserves_configured_order() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::Delete {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-long".to_owned(),
        },
    );
    commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-appended".to_owned(),
            start: 0,
            delete_count: 0,
            messages: vec![message("appended")],
            conversation: Some(json!({"name": "Appended"})),
            configured_index: None,
        },
    );
    let page = store
        .query_conversations(
            &ConversationQuery {
                character_id: "char-a".to_owned(),
                order: QueryOrder::Configured,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .unwrap();
    assert_eq!(
        page.items
            .iter()
            .map(|item| (item.id.as_str(), item.configured_index))
            .collect::<Vec<_>>(),
        vec![("conv-short", 1), ("conv-appended", 2)]
    );
}

#[test]
fn conversation_insert_after_deletion_uses_the_visible_position() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-third".to_owned(),
            start: 0,
            delete_count: 0,
            messages: Vec::new(),
            conversation: Some(json!({"name": "Third"})),
            configured_index: None,
        },
    );
    let revision = commit(
        &mut store,
        revision,
        ConversationMutation::Delete {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-long".to_owned(),
        },
    );
    commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-inserted".to_owned(),
            start: 0,
            delete_count: 0,
            messages: Vec::new(),
            conversation: Some(json!({"name": "Inserted"})),
            configured_index: Some(1),
        },
    );
    assert_eq!(
        store.materialize(None).unwrap()["characters"][1]["chats"]
            .as_array()
            .unwrap()
            .iter()
            .map(|chat| chat["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["conv-short", "conv-inserted", "conv-third"]
    );
}

#[test]
fn selected_character_replacement_is_atomic_and_preserves_catalog_order() {
    let (_directory, mut store, database) = open_fixture();
    let mut replacement = database["characters"][1].clone();
    let mut chats = replacement["chats"]
        .as_array()
        .expect("replacement chats")
        .clone();
    chats[1]["name"] = json!("Renamed inactive chat");
    chats[0]["message"] = json!([message("only-message")]);
    let short = chats.remove(1);
    chats.insert(0, short);
    chats.push(json!({
        "id": "conv-added",
        "name": "Added chat",
        "note": "supplied ID",
        "localLore": [],
        "message": [message("added-message")],
        "lastDate": 500
    }));
    replacement["chats"] = Value::Array(chats);
    let mut changed_root = root(&database);
    changed_root["username"] = json!("Committed with character");

    let revision = store
        .commit(&WorkingSetCommit {
            root: Some(changed_root),
            replace_character: Some(replacement),
            ..empty_working_set_commit(1)
        })
        .expect("replace selected character")
        .revision;
    assert_eq!(revision, 2);
    assert_eq!(
        store.read_root(None).expect("read changed root").value["username"],
        "Committed with character"
    );
    let characters = store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: false,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query characters after replacement");
    assert_eq!(
        characters
            .items
            .iter()
            .map(|item| (item.id.as_str(), item.configured_index))
            .collect::<Vec<_>>(),
        vec![("char-b", 0), ("char-a", 1)]
    );
    assert_eq!(characters.items[1].conversation_count, 3);
    assert_eq!(
        store
            .query_conversations(
                &ConversationQuery {
                    character_id: "char-a".to_owned(),
                    order: QueryOrder::Configured,
                    limit: 10,
                    cursor: None,
                },
                None,
            )
            .expect("query replaced conversations")
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["conv-short", "conv-long", "conv-added"]
    );
    assert_eq!(
        store
            .read_conversation("char-a", "conv-long", None)
            .expect("read replaced conversation")
            .expect("conversation exists")
            .value["message"]
            .as_array()
            .expect("messages")
            .len(),
        1
    );
}

#[test]
fn replacement_uses_the_greatest_configured_index_after_a_gap() {
    let (_directory, mut store, database) = open_fixture();
    let deleted = store
        .commit(&WorkingSetCommit {
            delete_character_id: Some("char-a".to_owned()),
            ..empty_working_set_commit(1)
        })
        .expect("delete middle configured character");
    assert!(store
        .read_character("char-a", None)
        .expect("read deleted character")
        .is_none());
    assert!(store
        .read_conversation("char-a", "conv-long", None)
        .expect("read deleted conversation")
        .is_none());
    let mut replacement = database["characters"][1].clone();
    replacement["chaId"] = json!("char-new");
    replacement["name"] = json!("New character");
    store
        .commit(&WorkingSetCommit {
            replace_character: Some(replacement),
            ..empty_working_set_commit(deleted.revision)
        })
        .expect("add replacement after configured gap");

    let items = store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: false,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query configured characters")
        .items;
    assert_eq!(items[0].id, "char-b");
    assert_eq!(
        (items[1].id.as_str(), items[1].configured_index),
        ("char-new", 3)
    );
}

#[test]
fn invalid_character_replacements_leave_revision_and_data_unchanged() {
    let (_directory, mut store, database) = open_fixture();
    let original = store
        .read_conversation("char-a", "conv-long", None)
        .expect("read original conversation");
    let mut invalid = database["characters"][1].clone();
    invalid["chats"][1]["id"] = invalid["chats"][0]["id"].clone();

    assert!(matches!(
        store.commit(&WorkingSetCommit {
            root: Some(json!({ "username": "must roll back" })),
            replace_character: Some(invalid.clone()),
            ..empty_working_set_commit(1)
        }),
        Err(StoreError::Validation { .. })
    ));
    assert_eq!(store.revision().expect("read unchanged revision"), 1);
    assert_eq!(
        store.read_root(None).expect("read unchanged root").value["username"],
        "Fixture User"
    );
    assert_eq!(
        store
            .read_conversation("char-a", "conv-long", None)
            .expect("read unchanged conversation"),
        original
    );
    assert!(matches!(
        store.commit(&WorkingSetCommit {
            replace_character: Some(invalid),
            ..empty_working_set_commit(0)
        }),
        Err(StoreError::RevisionConflict { .. })
    ));
}

#[test]
fn reopen_recovers_committed_data_and_sweeps_abandoned_staging() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let database = fixture();
    let staging_id = {
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let committed = store.replace_begin().expect("begin committed staging");
        store
            .replace_put_root(&committed.staging_id, &root(&database))
            .expect("stage committed root");
        store
            .replace_put_presets(
                &committed.staging_id,
                database["botPresets"].as_array().expect("fixture presets"),
            )
            .expect("stage committed presets");
        store
            .replace_add_characters(
                &committed.staging_id,
                database["characters"]
                    .as_array()
                    .expect("fixture characters"),
            )
            .expect("stage committed characters");
        store
            .replace_commit(&committed.staging_id, Some(0))
            .expect("commit fixture before reopen");
        let staging = store.replace_begin().expect("begin abandoned staging");
        store
            .replace_put_root(&staging.staging_id, &json!({ "username": "abandoned" }))
            .expect("stage abandoned root");
        staging.staging_id
    };

    let mut reopened = PersistentStore::open(directory.path()).expect("reopen persistent store");
    assert!(reopened.replace_commit(&staging_id, None).is_err());
    assert_eq!(reopened.revision().expect("read recovered revision"), 1);
    assert_eq!(
        reopened.read_root(None).expect("read recovered root").value["username"],
        "Fixture User"
    );
    assert_eq!(
        reopened
            .materialize(None)
            .expect("materialize reopened fixture"),
        database
    );
}

#[test]
fn invalid_staged_replacement_is_aborted_without_activation() {
    let (_directory, mut store, database) = open_fixture();
    let staging = store.replace_begin().expect("begin invalid staging");
    store
        .replace_put_root(&staging.staging_id, &json!({ "username": "invalid" }))
        .expect("stage replacement root");
    let first = database["characters"][0].clone();
    store
        .replace_add_characters(&staging.staging_id, std::slice::from_ref(&first))
        .expect("stage first character");
    let duplicate = first;

    assert!(matches!(
        store.replace_add_characters(&staging.staging_id, &[duplicate]),
        Err(StoreError::Validation { .. })
    ));
    store
        .replace_abort(&staging.staging_id)
        .expect("abort invalid staging");
    assert_eq!(store.revision().expect("read active revision"), 1);
    assert_eq!(
        store.materialize(None).expect("materialize active data"),
        database
    );
}

#[test]
fn staged_character_batches_preserve_order_and_roll_back_a_partial_duplicate_batch() {
    let directory = tempfile::tempdir().expect("create staged batch directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let database = fixture();
    let characters = database["characters"]
        .as_array()
        .expect("fixture characters");
    let staging = store.replace_begin().expect("begin staged replacement");
    store
        .replace_add_characters(&staging.staging_id, &characters[..1])
        .expect("stage first character batch");
    store
        .replace_add_characters(&staging.staging_id, &characters[1..])
        .expect("stage second character batch");

    let mut transient = characters[0].clone();
    transient["chaId"] = json!("char-transient");
    transient["name"] = json!("Transient");
    transient["chats"] = json!([]);
    assert!(matches!(
        store.replace_add_characters(&staging.staging_id, &[transient, characters[1].clone()],),
        Err(StoreError::Validation { .. })
    ));

    let staged_characters = store
        .materialize_staging(&staging.staging_id)
        .expect("materialize staged characters")["characters"]
        .as_array()
        .expect("staged character array")
        .iter()
        .map(|character| character["chaId"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(staged_characters, vec!["char-b", "char-a", "char-c"]);
}

#[test]
fn revision_leases_are_isolated_then_released() {
    let (_directory, mut store, _) = open_fixture();
    let lease = store.acquire_revision(1).expect("acquire revision lease");
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "apiType": "fixture-provider", "username": "Changed" })),
            ..empty_working_set_commit(1)
        })
        .expect("commit changed root");

    assert_eq!(
        store
            .read_root(Some(&lease.lease))
            .expect("read leased root")
            .value["username"],
        "Fixture User"
    );
    store.release_revision(&lease.lease).expect("release lease");
    assert!(matches!(
        store.read_root(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn ordinary_commit_during_a_lease_does_not_copy_any_generation_family() {
    let (_directory, mut store, _) = open_fixture();
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: UNOWNED_OWNER.to_owned(),
                key: "counted-zero".to_owned(),
                value: json!(0),
            }]),
            ..empty_working_set_commit(1)
        })
        .expect("seed counted plugin record");
    let count_records = |store: &PersistentStore| {
        super::GENERATION_TABLES
            .iter()
            .map(|(table, _)| {
                store
                    .connection
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .expect("count generation records")
            })
            .collect::<Vec<_>>()
    };
    let before = count_records(&store);
    let generation_before =
        super::active_generation(&store.connection).expect("read generation before leased commit");

    let lease = store.acquire_revision(2).expect("acquire revision lease");
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Changed without generation copy" })),
            ..empty_working_set_commit(2)
        })
        .expect("commit root while lease is active");

    assert_eq!(count_records(&store), before);
    assert_eq!(
        super::active_generation(&store.connection).expect("read generation after leased commit"),
        generation_before
    );
    assert_eq!(
        store
            .read_root(Some(&lease.lease))
            .expect("read pinned root")
            .value["username"],
        "Fixture User"
    );
    store.release_revision(&lease.lease).expect("release lease");
}

fn leased_family_canonical(store: &PersistentStore, lease: &str) -> Vec<u8> {
    let owner = AssetOwnerLocator::RootModuleAssets { index: 0 };
    serde_json::to_vec(&json!({
        "root": store.read_root(Some(lease)).expect("read leased root"),
        "presetCatalog": store.query_presets(Some(lease)).expect("query leased presets"),
        "preset": store.read_preset("0", Some(lease)).expect("read leased preset"),
        "liveCharacters": store.query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: false,
                limit: 100,
                cursor: None,
            },
            Some(lease),
        ).expect("query leased live characters"),
        "trashedCharacters": store.query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: true,
                limit: 100,
                cursor: None,
            },
            Some(lease),
        ).expect("query leased trashed characters"),
        "character": store.read_character("char-a", Some(lease))
            .expect("read leased character"),
        "conversationCatalog": store.query_conversations(
            &ConversationQuery {
                character_id: "char-a".to_owned(),
                order: QueryOrder::Configured,
                limit: 100,
                cursor: None,
            },
            Some(lease),
        ).expect("query leased conversations"),
        "conversation": store.read_conversation("char-a", "conv-short", Some(lease))
            .expect("read leased conversation"),
        "messageWindow": store.read_conversation_window(
            &ConversationWindowQuery {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
                start_index: None,
                limit: None,
                anchor_message_id: None,
                anchor_occurrence: None,
                before: None,
                after: None,
            },
            Some(lease),
        ).expect("read leased message window"),
        "pluginCatalog": store.query_plugin_storage(Some(lease))
            .expect("query leased plugin storage"),
        "plugin": store.read_plugin_storage(UNOWNED_OWNER, "lease-key", Some(lease))
            .expect("read leased plugin value"),
        "assetAliases": store.list_asset_aliases(Some(lease))
            .expect("list leased asset aliases"),
        "assetAlias": store.read_asset_alias("asset", "assets/lease.bin", Some(lease))
            .expect("read leased asset alias"),
        "assetOwnerHeads": store.list_asset_owner_heads(Some(lease))
            .expect("list leased asset owner heads"),
        "assetOwnerHead": store.read_asset_owner_head(&owner, Some(lease))
            .expect("read leased asset owner head"),
        "assetRepositoryAuthority": store.read_asset_repository_authority(Some(lease))
            .expect("read leased asset repository authority"),
        "materialized": store.materialize_lease(lease).expect("materialize leased revision"),
    }))
    .expect("serialize leased record families")
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[test]
fn wal_lease_keeps_every_final_record_family_and_native_export_canonical() {
    let (_directory, mut store, database) = open_fixture();
    let alias = AssetAlias {
        key: "assets/lease.bin".to_owned(),
        object_hash: Some("11".repeat(32)),
        kind: "asset".to_owned(),
        size: 7,
        mime: "application/octet-stream".to_owned(),
        name: "lease.bin".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({ "fixture": "lease" }),
    };
    let owner = AssetOwnerHead::present(
        AssetOwnerLocator::RootModuleAssets { index: 0 },
        "22".repeat(32),
        1,
    );
    let mut final_root = staged_root(&database);
    final_root["modules"] = json!([{
        "id": "lease-module",
        "assets": [["lease", "assets/lease.bin", "BIN"]]
    }]);
    let staging = store.replace_begin().expect("begin final-family staging");
    store
        .replace_put_root(&staging.staging_id, &final_root)
        .expect("stage final-family root");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"].as_array().expect("fixture presets"),
        )
        .expect("stage final-family presets");
    store
        .replace_add_characters(
            &staging.staging_id,
            database["characters"]
                .as_array()
                .expect("fixture characters"),
        )
        .expect("stage final-family characters");
    store
        .replace_put_asset_aliases(&staging.staging_id, std::slice::from_ref(&alias))
        .expect("stage final-family asset alias");
    store
        .replace_put_asset_owner_heads(&staging.staging_id, std::slice::from_ref(&owner))
        .expect("stage final-family owner head");
    let seeded = store
        .replace_commit(&staging.staging_id, Some(1))
        .expect("activate final-family staging");
    let seeded = store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: UNOWNED_OWNER.to_owned(),
                key: "lease-key".to_owned(),
                value: json!({ "nested": [0, false, ""] }),
            }]),
            ..empty_working_set_commit(seeded.revision)
        })
        .expect("seed plugin family before lease");
    let lease = store
        .acquire_revision(seeded.revision)
        .expect("acquire canonical lease");
    let canonical_before = leased_family_canonical(&store, &lease.lease);
    let export_before = store
        .export_risu_save(&lease.lease, false)
        .expect("export canonical lease before writes");
    let export_before_bytes = fs::read(&export_before.path).expect("read first native export");

    let mut changed_character = database["characters"][1].clone();
    changed_character["name"] = json!("Writer Alpha");
    let changed = store
        .commit(&WorkingSetCommit {
            root: Some(json!({
                "username": "Writer root",
                "modules": [{ "id": "writer-module" }]
            })),
            replace_presets: Some(vec![json!({ "name": "Writer preset" })]),
            character: Some(changed_character),
            conversations: Some(vec![ConversationMutation::ReplaceRange {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
                start: 2,
                delete_count: 0,
                messages: vec![message("writer-message")],
                conversation: None,
                configured_index: None,
            }]),
            delete_character_id: Some("char-b".to_owned()),
            asset_owner_heads: Some(vec![AssetOwnerHead::absent(
                AssetOwnerLocator::RootModuleAssets { index: 0 },
            )]),
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: UNOWNED_OWNER.to_owned(),
                key: "lease-key".to_owned(),
                value: json!("writer plugin"),
            }]),
            ..empty_working_set_commit(seeded.revision)
        })
        .expect("mutate writer record families");
    let replacement_alias = AssetAlias {
        object_hash: Some("44".repeat(32)),
        metadata: json!({ "fixture": "writer" }),
        ..alias.clone()
    };
    let changed = store
        .commit_asset_alias(&replacement_alias, changed.revision)
        .expect("mutate writer asset alias");
    let replacement = store
        .replace_begin()
        .expect("begin replacement during canonical lease");
    store
        .replace_put_root(
            &replacement.staging_id,
            &json!({ "username": "Replacement" }),
        )
        .expect("stage replacement during canonical lease");
    store
        .replace_commit(&replacement.staging_id, Some(changed.revision))
        .expect("activate replacement during canonical lease");

    let canonical_after = leased_family_canonical(&store, &lease.lease);
    let export_after = store
        .export_risu_save(&lease.lease, false)
        .expect("export canonical lease after writes");
    let export_after_bytes = fs::read(&export_after.path).expect("read second native export");
    assert_eq!(sha256(&canonical_after), sha256(&canonical_before));
    assert_eq!(sha256(&export_after_bytes), sha256(&export_before_bytes));

    store
        .cleanup_risu_save_export(Path::new(&export_before.path))
        .expect("clean first native export");
    store
        .cleanup_risu_save_export(Path::new(&export_after.path))
        .expect("clean second native export");
    store
        .release_revision(&lease.lease)
        .expect("release canonical lease");
}

#[test]
fn two_revision_leases_remain_independent_until_each_is_released() {
    let (_directory, mut store, _) = open_fixture();
    let first = store.acquire_revision(1).expect("acquire first lease");
    let second = store.acquire_revision(1).expect("acquire second lease");
    assert_eq!(store.lease_diagnostics().active_count, 2);
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Writer revision" })),
            ..empty_working_set_commit(1)
        })
        .expect("commit writer revision");

    store
        .release_revision(&first.lease)
        .expect("release first lease");
    assert_eq!(store.lease_diagnostics().active_count, 1);
    assert!(matches!(
        store.read_root(Some(&first.lease)),
        Err(StoreError::SnapshotReleased)
    ));
    assert_eq!(
        store
            .read_root(Some(&second.lease))
            .expect("second lease remains pinned")
            .value["username"],
        "Fixture User"
    );
    assert!(matches!(
        store.checkpoint(CheckpointMode::Truncate),
        Err(StoreError::Store { .. })
    ));

    store
        .release_revision(&second.lease)
        .expect("release second lease");
    assert_eq!(store.lease_diagnostics().active_count, 0);
    store
        .release_revision(&second.lease)
        .expect("repeat released lease cleanup");
}

#[test]
fn native_job_store_uses_an_independent_connection_and_shared_reader_registry() {
    let (_directory, mut store, _) = open_fixture();
    let mut job_store = store
        .open_native_job_store()
        .expect("open native job store");
    let lease = job_store
        .acquire_revision(1)
        .expect("acquire native job revision lease");

    assert_eq!(store.lease_diagnostics().active_count, 1);
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Writer revision" })),
            ..empty_working_set_commit(1)
        })
        .expect("advance live store while native job lease remains open");
    assert_eq!(
        job_store
            .read_root(Some(&lease.lease))
            .expect("read pinned root from native job connection")
            .value["username"],
        "Fixture User"
    );

    job_store
        .release_revision(&lease.lease)
        .expect("release native job revision lease");
    assert_eq!(store.lease_diagnostics().active_count, 0);
}

#[test]
fn revision_lease_survives_append_delete_root_change_and_staged_replace() {
    let (_directory, mut store, database) = open_fixture();
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: UNOWNED_OWNER.to_owned(),
                key: "pinned-zero".to_owned(),
                value: json!(0),
            }]),
            ..empty_working_set_commit(1)
        })
        .expect("seed pinned plugin value");
    let lease = store.acquire_revision(2).expect("acquire revision lease");
    let revision = store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "apiType": "fixture-provider", "username": "Changed" })),
            conversations: Some(vec![ConversationMutation::ReplaceRange {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
                start: 2,
                delete_count: 0,
                messages: vec![message("active-append")],
                conversation: None,
                configured_index: None,
            }]),
            delete_character_id: Some("char-b".to_owned()),
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: UNOWNED_OWNER.to_owned(),
                key: "pinned-zero".to_owned(),
                value: json!(1),
            }]),
            ..empty_working_set_commit(2)
        })
        .expect("commit active changes")
        .revision;
    assert!(store
        .read_character("char-b", None)
        .expect("read active character")
        .is_none());

    let staging = store.replace_begin().expect("begin staged replacement");
    store
        .replace_put_root(&staging.staging_id, &root(&database))
        .expect("stage root");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"].as_array().expect("fixture presets"),
        )
        .expect("stage presets");
    store
        .replace_add_characters(
            &staging.staging_id,
            database["characters"]
                .as_array()
                .expect("fixture characters"),
        )
        .expect("stage characters");
    store
        .replace_commit(&staging.staging_id, Some(revision))
        .expect("activate staged replacement");

    assert_eq!(
        store
            .read_root(Some(&lease.lease))
            .expect("read leased root")
            .value["username"],
        "Fixture User"
    );
    assert!(store
        .read_character("char-b", Some(&lease.lease))
        .expect("read leased character")
        .is_some());
    assert_eq!(
        store
            .read_conversation("char-a", "conv-short", Some(&lease.lease))
            .expect("read leased conversation")
            .expect("leased conversation exists")
            .value["message"]
            .as_array()
            .expect("leased messages")
            .len(),
        2
    );
    assert_eq!(
        store
            .read_plugin_storage(UNOWNED_OWNER, "pinned-zero", Some(&lease.lease))
            .expect("read leased plugin value")
            .expect("leased plugin value exists")
            .value,
        json!(0)
    );
    store.release_revision(&lease.lease).expect("release lease");
    assert!(matches!(
        store.read_root(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn releasing_a_non_snapshot_generation_cannot_delete_active_data() {
    let (_directory, mut store, database) = open_fixture();

    assert!(matches!(
        store.release_revision("revision-1"),
        Err(StoreError::Validation { .. })
    ));
    assert_eq!(
        store.materialize(None).expect("active data remains"),
        database
    );
}

#[test]
fn conversation_mutations_recalculate_character_summary() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::Delete {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
        },
    );
    assert_eq!(revision, 2);
    let summary = store
        .query_characters(
            &CharacterQuery {
                search: Some("alpha".to_owned()),
                order: QueryOrder::Configured,
                trash: false,
                limit: 1,
                cursor: None,
            },
            None,
        )
        .expect("query alpha summary");
    assert_eq!(summary.items[0].conversation_count, 1);
}

#[test]
fn character_detail_update_preserves_index_and_conversations() {
    let (_directory, mut store, database) = open_fixture();
    let mut detail = database["characters"][1].clone();
    detail
        .as_object_mut()
        .expect("character object")
        .remove("chats");
    detail["name"] = json!("Alpha Renamed");
    detail["lastInteraction"] = json!(999);

    store
        .commit(&WorkingSetCommit {
            character: Some(detail.clone()),
            ..empty_working_set_commit(1)
        })
        .expect("commit character detail update");

    let items = store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: false,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query configured characters")
        .items;
    let alpha = items
        .iter()
        .find(|item| item.id == "char-a")
        .expect("alpha summary");
    assert_eq!(alpha.name, "Alpha Renamed");
    assert_eq!(alpha.configured_index, 1);
    assert_eq!(alpha.recent_at, 999);
    assert_eq!(alpha.conversation_count, 2);
    assert_eq!(
        store
            .read_character("char-a", None)
            .expect("read updated detail")
            .expect("character exists")
            .value,
        detail
    );
    assert_eq!(
        store
            .read_conversation("char-a", "conv-short", None)
            .expect("read untouched conversation")
            .expect("conversation exists")
            .value["message"]
            .as_array()
            .expect("messages")
            .len(),
        2
    );
}

#[test]
fn batch_character_details_delete_atomically_and_preserve_plugin_zero() {
    let (_directory, mut store, database) = open_fixture();
    let mut group = database["characters"][0].clone();
    group.as_object_mut().expect("group object").remove("chats");
    group["type"] = json!("group");
    group["characters"] = json!(["char-a", "char-c"]);
    group["characterTalks"] = json!([0.25, 0.75]);
    group["characterActive"] = json!([false, true]);

    let prepared = store
        .commit(&WorkingSetCommit {
            character: Some(group.clone()),
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: UNOWNED_OWNER.to_owned(),
                key: "zero".to_owned(),
                value: json!(0),
            }]),
            ..empty_working_set_commit(1)
        })
        .expect("prepare group and plugin value");
    let configured_index_before = store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: false,
                limit: 100,
                cursor: None,
            },
            None,
        )
        .expect("query group before batch")
        .items
        .into_iter()
        .find(|summary| summary.id == "char-b")
        .expect("group summary before batch")
        .configured_index;
    let chats_before = store
        .materialize(None)
        .expect("materialize chats before batch")["characters"]
        .as_array()
        .expect("characters before batch")
        .iter()
        .find(|character| character["chaId"] == "char-b")
        .expect("group before batch")["chats"]
        .clone();
    let lease = store
        .acquire_revision(prepared.revision)
        .expect("acquire batch mutation lease");
    let mut updated_group = group.clone();
    updated_group["characters"] = json!(["char-c"]);
    updated_group["characterTalks"] = json!([0.75]);
    updated_group["characterActive"] = json!([true]);
    let mut invalid_detail = updated_group.clone();
    invalid_detail["chaId"] = json!("char-c");
    invalid_detail
        .as_object_mut()
        .expect("invalid detail object")
        .remove("name");

    let failed = store.commit(&WorkingSetCommit {
        root: Some(json!({ "username": "Must roll back" })),
        character_details: Some(vec![updated_group.clone(), invalid_detail]),
        delete_character_id: Some("char-a".to_owned()),
        ..empty_working_set_commit(prepared.revision)
    });

    assert!(failed.is_err());
    assert_eq!(
        store.revision().expect("revision after rollback"),
        prepared.revision
    );
    assert!(store
        .read_character("char-a", None)
        .expect("read rolled back target")
        .is_some());
    assert_eq!(
        store
            .read_character("char-b", None)
            .expect("read rolled back group")
            .expect("group exists")
            .value["characters"],
        json!(["char-a", "char-c"])
    );
    assert_eq!(
        store
            .read_plugin_storage(UNOWNED_OWNER, "zero", None)
            .expect("read plugin zero")
            .expect("plugin zero exists")
            .value,
        json!(0)
    );

    let committed = store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Committed" })),
            character_details: Some(vec![updated_group]),
            delete_character_id: Some("char-a".to_owned()),
            ..empty_working_set_commit(prepared.revision)
        })
        .expect("commit batch delete");

    assert_eq!(committed.revision, prepared.revision + 1);
    assert!(store
        .read_character("char-a", None)
        .expect("read deleted target")
        .is_none());
    assert_eq!(
        store
            .read_character("char-b", None)
            .expect("read committed group")
            .expect("group exists")
            .value["characters"],
        json!(["char-c"])
    );
    assert_eq!(
        store
            .read_plugin_storage(UNOWNED_OWNER, "zero", None)
            .expect("read preserved plugin zero")
            .expect("plugin zero exists")
            .value,
        json!(0)
    );
    assert_eq!(
        store
            .query_characters(
                &CharacterQuery {
                    search: None,
                    order: QueryOrder::Configured,
                    trash: false,
                    limit: 100,
                    cursor: None,
                },
                None,
            )
            .expect("query group after batch")
            .items
            .into_iter()
            .find(|summary| summary.id == "char-b")
            .expect("group summary after batch")
            .configured_index,
        configured_index_before
    );
    assert_eq!(
        store
            .materialize(None)
            .expect("materialize chats after batch")["characters"]
            .as_array()
            .expect("characters after batch")
            .iter()
            .find(|character| character["chaId"] == "char-b")
            .expect("group after batch")["chats"],
        chats_before
    );
    assert!(store
        .read_character("char-a", Some(&lease.lease))
        .expect("read leased target")
        .is_some());
    assert_eq!(
        store
            .read_character("char-b", Some(&lease.lease))
            .expect("read leased group")
            .expect("leased group exists")
            .value["characters"],
        json!(["char-a", "char-c"])
    );
    assert_eq!(
        store
            .read_plugin_storage(UNOWNED_OWNER, "zero", Some(&lease.lease))
            .expect("read leased plugin zero")
            .expect("leased plugin zero exists")
            .value,
        json!(0)
    );
    store
        .release_revision(&lease.lease)
        .expect("release batch mutation lease");
}

#[test]
fn invalid_batch_character_detail_ids_leave_every_character_row_unchanged() {
    let (_directory, mut store, _) = open_fixture();
    let before = store
        .materialize(None)
        .expect("materialize before invalid batches");
    let detail = store
        .read_character("char-b", None)
        .expect("read batch detail")
        .expect("batch detail exists")
        .value;
    let mut empty = detail.clone();
    empty["chaId"] = json!("");
    let mut missing = detail.clone();
    missing["chaId"] = json!("missing-character");
    let cases = vec![
        ("empty", vec![empty], None),
        ("duplicate", vec![detail.clone(), detail.clone()], None),
        ("deleted", vec![detail.clone()], Some("char-b".to_owned())),
        ("missing", vec![missing], None),
    ];

    for (name, character_details, delete_character_id) in cases {
        let result = store.commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Must not persist" })),
            character_details: Some(character_details),
            delete_character_id,
            ..empty_working_set_commit(1)
        });

        assert!(
            matches!(result, Err(StoreError::Validation { .. })),
            "{name} batch detail should be rejected"
        );
        assert_eq!(store.revision().expect("revision after invalid batch"), 1);
        assert_eq!(
            store
                .materialize(None)
                .expect("materialize after invalid batch"),
            before,
            "{name} batch detail changed stored character rows"
        );
    }
}

#[test]
fn replace_range_updates_conversation_detail_and_preserves_recent_at_without_it() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 2,
            delete_count: 0,
            messages: vec![message("appended")],
            conversation: Some(json!({
                "id": "conv-short", "name": "Renamed chat", "note": "updated",
                "localLore": [], "lastDate": 999
            })),
            configured_index: None,
        },
    );

    let query = |store: &PersistentStore| {
        store
            .query_conversations(
                &ConversationQuery {
                    character_id: "char-a".to_owned(),
                    order: QueryOrder::Recent,
                    limit: 10,
                    cursor: None,
                },
                None,
            )
            .expect("query conversations")
            .items
    };
    let items = query(&store);
    let short = items
        .iter()
        .find(|item| item.id == "conv-short")
        .expect("short summary");
    assert_eq!(short.name, "Renamed chat");
    assert_eq!(short.recent_at, 999);
    assert_eq!(short.message_count, 3);
    assert_eq!(
        store
            .read_conversation("char-a", "conv-short", None)
            .expect("read updated conversation")
            .expect("conversation exists")
            .value["note"],
        "updated"
    );

    commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 3,
            delete_count: 0,
            messages: vec![message("later")],
            conversation: None,
            configured_index: None,
        },
    );
    let items = query(&store);
    let short = items
        .iter()
        .find(|item| item.id == "conv-short")
        .expect("short summary after append");
    assert_eq!(short.recent_at, 999);
    assert_eq!(short.message_count, 4);
}

#[test]
fn conversation_metadata_preserves_detail_without_decoding_messages_across_revisions() {
    let (_directory, mut store, _) = open_fixture();
    let lease = store.acquire_revision(1).expect("acquire metadata lease");
    let revised_detail = json!({
        "id": "conv-short",
        "name": "Metadata only",
        "note": "nested fields survive",
        "localLore": [{ "key": "value", "nested": [1, { "ok": true }] }],
        "custom": { "array": [null, false, "text"], "number": 42 },
        "lastDate": 777
    });
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 2,
            delete_count: 0,
            messages: vec![message("metadata-append")],
            conversation: Some(revised_detail.clone()),
            configured_index: None,
        },
    );

    let active = store
        .read_conversation_metadata("char-a", "conv-short", None)
        .expect("read active metadata")
        .expect("active conversation exists");
    assert_eq!(
        serde_json::to_value(&active).expect("serialize active metadata"),
        json!({
            "revision": revision,
            "value": {
                "characterId": "char-a",
                "conversationId": "conv-short",
                "conversation": revised_detail,
                "totalMessages": 3
            }
        })
    );
    assert!(active.value.conversation.get("message").is_none());

    let pinned = store
        .read_conversation_metadata("char-a", "conv-short", Some(&lease.lease))
        .expect("read pinned metadata")
        .expect("pinned conversation exists");
    assert_eq!(pinned.revision, 1);
    assert_eq!(pinned.value.conversation["name"], "Short chat");
    assert!(pinned.value.conversation.get("message").is_none());
    assert_eq!(pinned.value.total_messages, 2);

    assert!(store
        .read_conversation_metadata("char-a", "missing", None)
        .expect("read missing conversation metadata")
        .is_none());
    assert!(store
        .read_conversation_metadata("char-b", "conv-short", None)
        .expect("read metadata with wrong owner")
        .is_none());

    let generation = active_generation(&store.connection).expect("read active generation");
    store
        .connection
        .execute(
            "UPDATE messages SET value = 'not-json' WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3 AND message_index = 0",
            params![generation, "char-a", "conv-short"],
        )
        .expect("corrupt one message payload");
    let metadata = store
        .read_conversation_metadata("char-a", "conv-short", None)
        .expect("metadata read ignores corrupt message payload")
        .expect("conversation still exists");
    assert_eq!(metadata.value.conversation, revised_detail);
    assert_eq!(metadata.value.total_messages, 3);
    assert!(store
        .read_conversation("char-a", "conv-short", None)
        .is_err());

    store
        .release_revision(&lease.lease)
        .expect("release metadata lease");
    assert!(matches!(
        store.read_conversation_metadata("char-a", "conv-short", Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn conversation_metadata_rejects_message_counts_outside_javascript_safe_range() {
    let (_directory, store, _) = open_fixture();
    let generation = active_generation(&store.connection).expect("read active generation");
    store
        .connection
        .execute(
            "UPDATE conversations SET message_count = ?1 WHERE generation = ?2 AND character_id = ?3 AND conversation_id = ?4",
            params![JAVASCRIPT_MAX_SAFE_INTEGER + 1, generation, "char-a", "conv-short"],
        )
        .expect("write unsafe synthetic message count");

    assert!(matches!(
        store.read_conversation_metadata("char-a", "conv-short", None),
        Err(StoreError::Validation { .. })
    ));
}

#[test]
fn summary_recent_at_falls_back_to_message_time_then_zero() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-from-message".to_owned(),
            start: 0,
            delete_count: 0,
            messages: vec![message("timed")],
            conversation: Some(json!({ "name": "From message", "note": "", "localLore": [] })),
            configured_index: None,
        },
    );
    let revision = commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-empty".to_owned(),
            start: 0,
            delete_count: 0,
            messages: vec![],
            conversation: Some(json!({ "name": "Empty", "note": "", "localLore": [] })),
            configured_index: None,
        },
    );
    store
        .commit(&WorkingSetCommit {
            add_character: Some(json!({ "chaId": "char-zero", "name": "Zero", "chats": [] })),
            ..empty_working_set_commit(revision)
        })
        .expect("add character without lastInteraction");

    let conversations = store
        .query_conversations(
            &ConversationQuery {
                character_id: "char-a".to_owned(),
                order: QueryOrder::Configured,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query conversations")
        .items;
    let recent_at = |id: &str| {
        conversations
            .iter()
            .find(|item| item.id == id)
            .expect("conversation summary")
            .recent_at
    };
    assert_eq!(recent_at("conv-from-message"), 1_800_000_000_000);
    assert_eq!(recent_at("conv-empty"), 0);

    let characters = store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: false,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query characters")
        .items;
    assert_eq!(
        characters
            .iter()
            .find(|item| item.id == "char-zero")
            .expect("zero summary")
            .recent_at,
        0
    );
}

#[test]
fn replace_range_clamps_out_of_bounds_indices() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 50,
            delete_count: 5,
            messages: vec![message("tail")],
            conversation: None,
            configured_index: None,
        },
    );
    commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: -3,
            delete_count: 100,
            messages: vec![message("only")],
            conversation: None,
            configured_index: None,
        },
    );

    let window = store
        .read_conversation_window(
            &ConversationWindowQuery {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
                start_index: None,
                limit: None,
                anchor_message_id: None,
                anchor_occurrence: None,
                before: None,
                after: None,
            },
            None,
        )
        .expect("read clamped window")
        .expect("conversation exists");
    assert_eq!(window.value.total_messages, 1);
    assert_eq!(window.value.messages[0]["chatId"], "only");
}

#[test]
fn revision_leases_isolate_conversation_reads() {
    let (_directory, mut store, _) = open_fixture();
    let lease = store.acquire_revision(1).expect("acquire revision lease");
    commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 2,
            delete_count: 0,
            messages: vec![message("live-only")],
            conversation: None,
            configured_index: None,
        },
    );

    let window = |store: &PersistentStore, lease: Option<&str>| {
        store
            .read_conversation_window(
                &ConversationWindowQuery {
                    character_id: "char-a".to_owned(),
                    conversation_id: "conv-short".to_owned(),
                    start_index: Some(1),
                    limit: Some(2),
                    anchor_message_id: None,
                    anchor_occurrence: None,
                    before: None,
                    after: None,
                },
                lease,
            )
            .expect("read conversation window")
            .expect("conversation exists")
    };
    assert_eq!(window(&store, Some(&lease.lease)).value.total_messages, 2);
    assert_eq!(window(&store, None).value.total_messages, 3);
    assert_eq!(window(&store, Some(&lease.lease)).value.messages.len(), 1);
    assert_eq!(window(&store, None).value.messages.len(), 2);
    assert_eq!(
        store
            .query_conversations(
                &ConversationQuery {
                    character_id: "char-a".to_owned(),
                    order: QueryOrder::Configured,
                    limit: 10,
                    cursor: None,
                },
                Some(&lease.lease),
            )
            .expect("query leased conversations")
            .items
            .iter()
            .find(|item| item.id == "conv-short")
            .expect("leased short summary")
            .message_count,
        2
    );

    store.release_revision(&lease.lease).expect("release lease");
    assert!(matches!(
        store.read_conversation_window(
            &ConversationWindowQuery {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
                start_index: None,
                limit: None,
                anchor_message_id: None,
                anchor_occurrence: None,
                before: None,
                after: None,
            },
            Some(&lease.lease),
        ),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn reopen_invalidates_runtime_and_legacy_leases_then_reclaims_inactive_generations() {
    let (directory, mut store, _) = open_fixture();
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: UNOWNED_OWNER.to_owned(),
                key: "ttl-zero".to_owned(),
                value: json!(0),
            }]),
            ..empty_working_set_commit(1)
        })
        .expect("seed leased plugin value");
    let lease = store.acquire_revision(2).expect("acquire revision lease");
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Active after lease" })),
            ..empty_working_set_commit(2)
        })
        .expect("fork active generation");
    store
        .connection
        .execute_batch(
            "
            INSERT INTO root (generation, value)
                VALUES ('revision-legacy', '{\"username\":\"Legacy pinned\"}');
            INSERT INTO snapshot_leases (lease, generation, revision, created_at)
                VALUES ('snapshot-legacy', 'revision-legacy', 2, 4102444800000);
            ",
        )
        .expect("seed rollback-compatible legacy lease");
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("reopen after runtime lease drop");
    assert!(matches!(
        store.read_root(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
    assert!(matches!(
        store.read_root(Some("snapshot-legacy")),
        Err(StoreError::SnapshotReleased)
    ));
    let persisted_leases: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM snapshot_leases", [], |row| row.get(0))
        .expect("count abandoned persisted leases");
    assert_eq!(persisted_leases, 0);
    let expired_generation_rows: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM root WHERE generation = 'revision-legacy'",
            [],
            |row| row.get(0),
        )
        .expect("count expired generation rows");
    assert_eq!(expired_generation_rows, 0);
    assert_eq!(
        store
            .read_plugin_storage(UNOWNED_OWNER, "ttl-zero", None)
            .expect("read active plugin value after sweep")
            .expect("active plugin value survives sweep")
            .value,
        json!(0)
    );
    assert_eq!(
        store
            .read_root(None)
            .expect("read active root after sweep")
            .value["username"],
        "Active after lease"
    );
}

#[test]
fn pilot_mutated_database_supports_generation_cow_compatible_reopen_read_and_commit() {
    let (directory, mut store, _) = open_fixture();
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Pilot-mutated root" })),
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: UNOWNED_OWNER.to_owned(),
                key: "rollback-compatible".to_owned(),
                value: json!({ "pilot": true }),
            }]),
            ..empty_working_set_commit(1)
        })
        .expect("mutate fixture through WAL pilot");
    let database_path = directory.path().join("persistent/persistent.sqlite");
    drop(store);

    let mut compatibility = Connection::open(&database_path).expect("open database for COW path");
    super::schema::initialize(&mut compatibility).expect("initialize compatible schema");
    assert_eq!(
        compatibility
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read schema version"),
        i64::from(super::schema::SCHEMA_VERSION)
    );
    assert_eq!(
        super::current_revision(&compatibility).expect("read pilot revision through COW path"),
        2
    );
    let source =
        super::active_generation(&compatibility).expect("read pilot generation through COW path");
    let source_root: String = compatibility
        .query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [&source],
            |row| row.get(0),
        )
        .expect("read pilot root through COW path");
    assert_eq!(
        serde_json::from_str::<Value>(&source_root).expect("parse pilot root")["username"],
        "Pilot-mutated root"
    );

    let legacy_lease = "snapshot-rollback-compatible";
    compatibility
        .execute(
            "INSERT INTO snapshot_leases (lease, generation, revision, created_at)
             VALUES (?1, ?2, 2, 4102444800000)",
            params![legacy_lease, source],
        )
        .expect("acquire generation COW lease");
    let target = "revision-3";
    let transaction = compatibility
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin generation COW commit");
    for (table, columns) in super::GENERATION_TABLES {
        transaction
            .execute(
                &format!(
                    "INSERT INTO {table} (generation, {columns})
                     SELECT ?1, {columns} FROM {table} WHERE generation = ?2"
                ),
                params![target, source],
            )
            .unwrap_or_else(|error| panic!("copy {table} through generation COW path: {error}"));
        let source_rows: i64 = transaction
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE generation = ?1"),
                [&source],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| panic!("count source {table}: {error}"));
        let target_rows: i64 = transaction
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE generation = ?1"),
                [target],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| panic!("count target {table}: {error}"));
        assert_eq!(target_rows, source_rows, "generation COW copied {table}");
    }
    transaction
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            params![
                target,
                serde_json::to_string(&json!({ "username": "Generation COW writer" }))
                    .expect("serialize COW root")
            ],
        )
        .expect("write generation COW root");
    transaction
        .execute(
            "UPDATE meta SET value = '3' WHERE key = 'currentRevision'",
            [],
        )
        .expect("advance generation COW revision");
    transaction
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = 'activeGeneration'",
            [serde_json::to_string(target).expect("serialize COW generation")],
        )
        .expect("activate generation COW target");
    transaction.commit().expect("commit generation COW write");

    let (leased_generation, leased_revision): (String, i64) = compatibility
        .query_row(
            "SELECT generation, revision FROM snapshot_leases WHERE lease = ?1",
            [legacy_lease],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("resolve generation COW lease");
    let leased_root: String = compatibility
        .query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [&leased_generation],
            |row| row.get(0),
        )
        .expect("read generation COW lease");
    assert_eq!(leased_revision, 2);
    assert_eq!(
        serde_json::from_str::<Value>(&leased_root).expect("parse leased COW root")["username"],
        "Pilot-mutated root"
    );
    assert_eq!(
        super::current_revision(&compatibility).expect("read generation COW revision"),
        3
    );
    assert_eq!(
        super::active_generation(&compatibility).expect("read generation COW target"),
        target
    );
    drop(compatibility);

    let reopened = PersistentStore::open(directory.path()).expect("reopen generation COW database");
    assert_eq!(reopened.revision().expect("read reopened COW revision"), 3);
    assert_eq!(
        reopened
            .read_root(None)
            .expect("read reopened COW root")
            .value["username"],
        "Generation COW writer"
    );
    assert_eq!(
        reopened
            .read_plugin_storage(UNOWNED_OWNER, "rollback-compatible", None)
            .expect("read COW-copied plugin value")
            .expect("COW-copied plugin value exists")
            .value,
        json!({ "pilot": true })
    );
    assert_eq!(
        reopened
            .connection
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .expect("check COW database integrity"),
        "ok"
    );
}

#[test]
fn app_kv_round_trips_json() {
    let (_directory, store, _) = open_fixture();
    let value = json!({ "sourceRevision": 1, "imported": true });
    store
        .set_app_kv("device-backup-commit:migration", &value)
        .expect("write app kv");
    assert_eq!(
        store
            .get_app_kv("device-backup-commit:migration")
            .expect("read app kv"),
        Some(value)
    );
}

#[test]
fn app_kv_remove_deletes_only_the_selected_key() {
    let (_directory, store, _) = open_fixture();
    store
        .set_app_kv("device-backup-commit:first", &json!({ "job": "first" }))
        .expect("write first marker");
    store
        .set_app_kv("external-restore-commit:second", &json!({ "job": "second" }))
        .expect("write second marker");

    store
        .remove_app_kv("device-backup-commit:first")
        .expect("remove first marker");

    assert_eq!(
        store
            .get_app_kv("device-backup-commit:first")
            .expect("read removed marker"),
        None
    );
    assert_eq!(
        store
            .get_app_kv("external-restore-commit:second")
            .expect("read remaining marker"),
        Some(json!({ "job": "second" }))
    );
}

#[test]
fn app_kv_accepts_only_the_two_job_marker_prefixes() {
    // Everything a device keeps now lives in device.sqlite or the OS vault.
    // app_kv holds only the markers that must share a commit with the
    // replacement they authorize.
    let (_directory, store, _) = open_fixture();
    for rejected in [
        "official-account.credential.v1",
        "official-account.association.v1",
        "official-account.asset-ledger.v1",
        "sync-conflict-backups.index.v1",
        "device-backup-commit:",
        "external-restore-commit:",
        "prefixed-device-backup-commit:job",
        "",
    ] {
        assert!(
            store.set_app_kv(rejected, &json!(true)).is_err(),
            "{rejected} must not be writable"
        );
        assert!(
            store.get_app_kv(rejected).is_err(),
            "{rejected} must not be readable"
        );
        assert!(
            store.remove_app_kv(rejected).is_err(),
            "{rejected} must not be removable"
        );
    }
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM app_kv", [], |row| row
                .get::<_, i64>(0))
            .expect("count app kv rows"),
        0
    );
}
