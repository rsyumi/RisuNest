use super::{
    CharacterQuery, CheckpointMode, ConversationMutation, ConversationPage, ConversationQuery,
    ConversationWindowQuery, PersistentStore, PluginStorageMutation, QueryOrder, StoreError,
    WorkingSetCommit,
};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../fixtures/persistent-fixture.json"))
        .expect("parse persistent store fixture")
}

fn staged_root(database: &Value) -> Value {
    let mut root = database.clone();
    let root = root.as_object_mut().expect("fixture database object");
    root.remove("characters");
    root.remove("botPresets");
    Value::Object(root.clone())
}

fn root(database: &Value) -> Value {
    let mut root = staged_root(database);
    let root = root.as_object_mut().expect("fixture staged root object");
    root.remove("pluginCustomStorage");
    Value::Object(root.clone())
}

fn open_fixture() -> (tempfile::TempDir, PersistentStore, Value) {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let database = fixture();
    let staging = store.replace_begin().expect("begin staged replacement");
    let root = staged_root(&database);
    let characters = database["characters"]
        .as_array()
        .expect("fixture characters");
    store
        .replace_put_root(&staging.staging_id, &root)
        .expect("stage fixture root");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"].as_array().expect("fixture presets"),
        )
        .expect("stage fixture presets");
    store
        .replace_add_characters(&staging.staging_id, characters)
        .expect("stage fixture characters");
    assert_eq!(
        store
            .replace_commit(&staging.staging_id, Some(0))
            .expect("commit fixture")
            .revision,
        1
    );
    (directory, store, database)
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
            expected_revision: 1,
            root: Some(json!({ "username": "Preset commit", "botPresets": ["strip"] })),
            replace_presets: Some(vec![json!({ "name": "Replacement" })]),
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
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
                { "key": "alpha", "byteSize": 5 },
                { "key": "beta", "byteSize": 16 }
            ]
        })
    );
    assert_eq!(
        store
            .read_plugin_storage("beta", None)
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
            expected_revision: imported.revision,
            root: Some(json!({ "username": "Plugin commit" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: Some(vec![
                PluginStorageMutation::Set {
                    key: "alpha".to_owned(),
                    value: json!("new"),
                },
                PluginStorageMutation::Delete {
                    key: "beta".to_owned(),
                },
            ]),
        })
        .expect("mutate plugin storage");

    assert_eq!(
        store
            .read_plugin_storage("alpha", Some(&lease.lease))
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
        store.read_plugin_storage("alpha", Some(&lease.lease)),
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
            .read_plugin_storage("\u{ffff}x", None)
            .expect("read unicode key")
            .expect("unicode key exists")
            .value,
        json!("unicode")
    );

    let updated = store
        .commit(&WorkingSetCommit {
            expected_revision: imported.revision,
            root: None,
            replace_presets: None,
            character: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: Some(vec![
                PluginStorageMutation::Set {
                    key: "zeta".to_owned(),
                    value: json!("updated"),
                },
                PluginStorageMutation::Delete {
                    key: "zeta".to_owned(),
                },
                PluginStorageMutation::Set {
                    key: "zeta".to_owned(),
                    value: json!("reinserted"),
                },
            ]),
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
            expected_revision: imported.revision,
            root: Some(json!({
                "username": "ordinary root",
                "pluginCustomStorage": { "incidental": "ignored" }
            })),
            replace_presets: None,
            character: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
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
            expected_revision: revision,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: Some(vec![mutation]),
            delete_character_id: None,
            plugin_storage: None,
        })
        .expect("commit conversation mutation")
        .revision
}

#[test]
fn opens_new_store_at_revision_zero() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");

    assert_eq!(store.revision().expect("read revision"), 0);
    assert!(directory.path().join("persistent/persistent.db").is_file());
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
            expected_revision: 0,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: Some(json!({
                "type": "character",
                "chaId": "unicode-name",
                "name": "Éclair",
                "chats": []
            })),
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
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
            "messageCount": 2
        })
    );
}

#[test]
fn conversation_windows_cover_latest_and_anchor_boundaries() {
    let (_directory, store, _) = open_fixture();
    let window = |anchor: Option<&str>, before, after| ConversationWindowQuery {
        character_id: "char-a".to_owned(),
        conversation_id: "conv-long".to_owned(),
        limit: Some(4),
        anchor_message_id: anchor.map(str::to_owned),
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
fn cas_conflict_preserves_current_revision() {
    let (_directory, mut store, _) = open_fixture();
    let result = store.commit(&WorkingSetCommit {
        expected_revision: 0,
        root: Some(json!({ "username": "stale" })),
        replace_presets: None,
        character: None,
        character_details: None,
        replace_character: None,
        add_character: None,
        conversations: None,
        delete_character_id: None,
        plugin_storage: None,
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
            expected_revision: 1,
            root: Some(changed_root),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: Some(replacement),
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
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
            expected_revision: 1,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: Some("char-a".to_owned()),
            plugin_storage: None,
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
            expected_revision: deleted.revision,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: Some(replacement),
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
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
            expected_revision: 1,
            root: Some(json!({ "username": "must roll back" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: Some(invalid.clone()),
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
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
            expected_revision: 0,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: Some(invalid),
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
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
fn revision_leases_are_isolated_then_released() {
    let (_directory, mut store, _) = open_fixture();
    let lease = store.acquire_revision(1).expect("acquire revision lease");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "apiType": "fixture-provider", "username": "Changed" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
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
            expected_revision: 1,
            root: None,
            replace_presets: None,
            character: Some(detail.clone()),
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
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
            expected_revision: 1,
            root: None,
            replace_presets: None,
            character: Some(group.clone()),
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "zero".to_owned(),
                value: json!(0),
            }]),
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
        expected_revision: prepared.revision,
        root: Some(json!({ "username": "Must roll back" })),
        replace_presets: None,
        character: None,
        character_details: Some(vec![updated_group.clone(), invalid_detail]),
        replace_character: None,
        add_character: None,
        conversations: None,
        delete_character_id: Some("char-a".to_owned()),
        plugin_storage: None,
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
            .read_plugin_storage("zero", None)
            .expect("read plugin zero")
            .expect("plugin zero exists")
            .value,
        json!(0)
    );

    let committed = store
        .commit(&WorkingSetCommit {
            expected_revision: prepared.revision,
            root: Some(json!({ "username": "Committed" })),
            replace_presets: None,
            character: None,
            character_details: Some(vec![updated_group]),
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: Some("char-a".to_owned()),
            plugin_storage: None,
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
            .read_plugin_storage("zero", None)
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
            .read_plugin_storage("zero", Some(&lease.lease))
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
            expected_revision: 1,
            root: Some(json!({ "username": "Must not persist" })),
            replace_presets: None,
            character: None,
            character_details: Some(character_details),
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id,
            plugin_storage: None,
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
        },
    );
    store
        .commit(&WorkingSetCommit {
            expected_revision: revision,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: Some(json!({ "chaId": "char-zero", "name": "Zero", "chats": [] })),
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
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
        },
    );

    let window = store
        .read_conversation_window(
            &ConversationWindowQuery {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
                limit: None,
                anchor_message_id: None,
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
        },
    );

    let window = |store: &PersistentStore, lease: Option<&str>| {
        store
            .read_conversation_window(
                &ConversationWindowQuery {
                    character_id: "char-a".to_owned(),
                    conversation_id: "conv-short".to_owned(),
                    limit: None,
                    anchor_message_id: None,
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
                limit: None,
                anchor_message_id: None,
                before: None,
                after: None,
            },
            Some(&lease.lease),
        ),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn expired_leases_are_swept_on_reopen_while_fresh_leases_survive() {
    let (directory, mut store, _) = open_fixture();
    let lease = store.acquire_revision(1).expect("acquire revision lease");
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("reopen with fresh lease");
    assert_eq!(
        store
            .read_root(Some(&lease.lease))
            .expect("fresh lease survives reopen")
            .value["username"],
        "Fixture User"
    );
    store
        .connection
        .execute(
            "UPDATE snapshot_leases SET created_at = created_at - 90000000",
            [],
        )
        .expect("age lease past the ttl");
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("reopen after expiry");
    assert!(matches!(
        store.read_root(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
    let orphan_rows: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM root WHERE generation = ?1",
            [lease.lease.as_str()],
            |row| row.get(0),
        )
        .expect("count swept generation rows");
    assert_eq!(orphan_rows, 0);
}

#[test]
fn app_kv_round_trips_json() {
    let (_directory, store, _) = open_fixture();
    let value = json!({ "sourceRevision": 1, "imported": true });
    store.set_app_kv("migration", &value).expect("write app kv");
    assert_eq!(
        store.get_app_kv("migration").expect("read app kv"),
        Some(value)
    );
}

#[test]
fn app_kv_remove_deletes_only_the_selected_key() {
    let (_directory, store, _) = open_fixture();
    store
        .set_app_kv("credential", &json!({ "token": "legacy" }))
        .expect("write credential");
    store
        .set_app_kv("ledger", &json!({ "version": 1 }))
        .expect("write ledger");

    store
        .remove_app_kv("credential")
        .expect("remove credential");

    assert_eq!(
        store.get_app_kv("credential").expect("read credential"),
        None
    );
    assert_eq!(
        store.get_app_kv("ledger").expect("read ledger"),
        Some(json!({ "version": 1 }))
    );
}

fn create_v1_database(path: &Path) {
    fs::create_dir_all(path.parent().expect("v1 database parent")).expect("create v1 parent");
    let connection = rusqlite::Connection::open(path).expect("create v1 database");
    connection
        .execute_batch(
            "
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE app_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE snapshot_leases (generation TEXT PRIMARY KEY, created_at INTEGER NOT NULL);
            CREATE TABLE root (generation TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE characters (
                generation TEXT NOT NULL, character_id TEXT NOT NULL,
                configured_index INTEGER NOT NULL, recent_at INTEGER NOT NULL,
                trashed INTEGER NOT NULL, name TEXT NOT NULL, image TEXT,
                conversation_count INTEGER NOT NULL, detail TEXT NOT NULL,
                PRIMARY KEY (generation, character_id)
            );
            CREATE INDEX characters_configured ON characters (generation, configured_index);
            CREATE INDEX characters_recent ON characters (generation, recent_at DESC, configured_index);
            CREATE TABLE conversations (
                generation TEXT NOT NULL, character_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL, configured_index INTEGER NOT NULL,
                recent_at INTEGER NOT NULL, name TEXT NOT NULL,
                message_count INTEGER NOT NULL, detail TEXT NOT NULL,
                PRIMARY KEY (generation, character_id, conversation_id)
            );
            CREATE INDEX conversations_configured
                ON conversations (generation, character_id, configured_index);
            CREATE INDEX conversations_recent
                ON conversations (generation, character_id, recent_at DESC, configured_index);
            CREATE TABLE messages (
                generation TEXT NOT NULL, character_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL, message_index INTEGER NOT NULL,
                message_id TEXT, value TEXT NOT NULL,
                PRIMARY KEY (generation, character_id, conversation_id, message_index)
            );
            CREATE INDEX messages_by_id
                ON messages (generation, character_id, conversation_id, message_id);
            INSERT INTO meta VALUES ('currentRevision', '7');
            INSERT INTO meta VALUES ('activeGeneration', '\"revision-7\"');
            PRAGMA user_version = 1;
            ",
        )
        .expect("create v1 schema");
    connection
        .execute(
            "INSERT INTO root VALUES (?1, ?2)",
            rusqlite::params![
                "revision-7",
                json!({
                    "username": "V1 active",
                    "characters": ["strip"],
                    "botPresets": [
                        { "name": "V1 first", "image": "v1.png" },
                        { "name": "V1 second" }
                    ],
                    "pluginCustomStorage": {
                        "active-memory": { "turns": [1, 2, 3] }
                    }
                })
                .to_string()
            ],
        )
        .expect("insert v1 active root");
    connection
        .execute(
            "INSERT INTO root VALUES (?1, ?2)",
            rusqlite::params![
                "revision-old",
                json!({
                    "username": "V1 old",
                    "botPresets": [{ "name": "Old preset" }],
                    "pluginCustomStorage": { "old-memory": "preserved" }
                })
                .to_string()
            ],
        )
        .expect("insert v1 old root");
    let detail = json!({
        "type": "character",
        "chaId": "v1-character",
        "name": "V1 character",
        "creatorNotes": "migrated notes",
        "trashTime": 123
    });
    connection
        .execute(
            "INSERT INTO characters
             (generation, character_id, configured_index, recent_at, trashed, name, image,
              conversation_count, detail)
             VALUES (?1, ?2, 0, 0, 1, ?3, NULL, 0, ?4)",
            rusqlite::params![
                "revision-7",
                "v1-character",
                "V1 character",
                detail.to_string()
            ],
        )
        .expect("insert v1 character");
}

fn create_v2_database(path: &Path) {
    create_v1_database(path);
    let connection = rusqlite::Connection::open(path).expect("open v1 database for v2 setup");
    connection
        .execute_batch(
            "
            CREATE TABLE bot_presets (
                generation TEXT NOT NULL,
                preset_id TEXT NOT NULL,
                configured_index INTEGER NOT NULL,
                name TEXT NOT NULL,
                image TEXT,
                value TEXT NOT NULL,
                PRIMARY KEY (generation, preset_id)
            );
            CREATE INDEX bot_presets_configured ON bot_presets (generation, configured_index);
            ALTER TABLE characters ADD COLUMN type TEXT NOT NULL DEFAULT '';
            ALTER TABLE characters ADD COLUMN creator_notes TEXT;
            ALTER TABLE characters ADD COLUMN trash_time INTEGER;
            PRAGMA user_version = 2;
            ",
        )
        .expect("create v2 schema additions");
    connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params![
                "revision-7",
                json!({
                    "username": "V2 active",
                    "pluginCustomStorage": { "v2-memory": { "lossless": true } }
                })
                .to_string()
            ],
        )
        .expect("write v2 plugin root");
}

fn create_v3_database(path: &Path) {
    create_v2_database(path);
    let connection = rusqlite::Connection::open(path).expect("open v2 database for v3 setup");
    connection
        .execute_batch(
            "
            CREATE TABLE plugin_storage (
                generation TEXT NOT NULL,
                storage_key TEXT NOT NULL,
                byte_size INTEGER NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (generation, storage_key)
            );
            ",
        )
        .expect("create v3 plugin table");
    let roots = {
        let mut statement = connection
            .prepare("SELECT generation, value FROM root")
            .expect("prepare v3 roots");
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query v3 roots")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect v3 roots")
    };
    for (generation, serialized) in roots {
        let mut value: Value = serde_json::from_str(&serialized).expect("parse v3 root");
        let object = value.as_object_mut().expect("v3 root object");
        if let Some(storage) = object.remove("pluginCustomStorage") {
            for (key, value) in storage.as_object().expect("v3 plugin object") {
                let serialized = serde_json::to_string(value).expect("serialize v3 plugin value");
                connection
                    .execute(
                        "INSERT INTO plugin_storage (generation, storage_key, byte_size, value)
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![generation, key, serialized.len() as i64, serialized],
                    )
                    .expect("insert v3 plugin value");
            }
        }
        connection
            .execute(
                "UPDATE root SET value = ?2 WHERE generation = ?1",
                rusqlite::params![generation, value.to_string()],
            )
            .expect("strip v3 root");
    }
    connection
        .execute(
            "INSERT INTO plugin_storage (generation, storage_key, byte_size, value)
             VALUES ('revision-7', 'zeta', 1, '0'),
                    ('revision-7', '10', 1, '0'),
                    ('revision-7', '2', 1, '0')",
            [],
        )
        .expect("insert v3 ordering values");
    connection
        .pragma_update(None, "user_version", 3)
        .expect("set v3 schema version");
}

#[test]
fn schema_v4_migrates_existing_v2_plugin_storage() {
    let directory = tempfile::tempdir().expect("create v2 migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v2_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate v2 store");

    assert!(store
        .read_root(None)
        .expect("read v2 migrated root")
        .value
        .get("pluginCustomStorage")
        .is_none());
    assert_eq!(
        store
            .read_plugin_storage("v2-memory", None)
            .expect("read v2 migrated plugin key")
            .expect("v2 migrated plugin key exists")
            .value,
        json!({ "lossless": true })
    );
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read v2 migrated version");
    assert_eq!(version, 4);
}

#[test]
fn schema_v4_adds_durable_plugin_ordinals_to_v3() {
    let directory = tempfile::tempdir().expect("create v3 migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v3_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate v3 store");

    assert_eq!(
        store
            .query_plugin_storage(None)
            .expect("query migrated v3 storage")
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        vec!["2", "10", "v2-memory", "zeta"]
    );
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated v3 version");
    assert_eq!(version, 4);
}

#[test]
fn schema_v4_migrates_large_retained_roots_one_generation_at_a_time() {
    let directory = tempfile::tempdir().expect("create retained root migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v2_database(&database_path);
    let connection = rusqlite::Connection::open(&database_path).expect("open retained roots");
    let payload = "x".repeat(512 * 1024);
    for index in 0..12 {
        connection
            .execute(
                "INSERT INTO root (generation, value) VALUES (?1, ?2)",
                rusqlite::params![
                    format!("retained-{index}"),
                    json!({
                        "username": format!("Retained {index}"),
                        "pluginCustomStorage": { format!("memory-{index}"): payload }
                    })
                    .to_string()
                ],
            )
            .expect("insert retained root");
    }
    drop(connection);

    let store = PersistentStore::open(directory.path()).expect("migrate retained roots");
    let migrated_count: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM plugin_storage WHERE generation LIKE 'retained-%'",
            [],
            |row| row.get(0),
        )
        .expect("count retained plugin rows");
    let retained_root_fields: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM root
             WHERE generation LIKE 'retained-%' AND value LIKE '%pluginCustomStorage%'",
            [],
            |row| row.get(0),
        )
        .expect("count retained root plugin fields");
    assert_eq!(migrated_count, 12);
    assert_eq!(retained_root_fields, 0);
}

#[test]
fn schema_v4_migrates_records_for_every_v1_generation() {
    let directory = tempfile::tempdir().expect("create migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v1_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate v1 store");
    assert_eq!(
        store
            .query_presets(None)
            .expect("query migrated presets")
            .items
            .len(),
        2
    );
    assert!(store
        .read_root(None)
        .expect("read migrated root")
        .value
        .get("botPresets")
        .is_none());
    assert!(store
        .read_root(None)
        .expect("read migrated root")
        .value
        .get("pluginCustomStorage")
        .is_none());
    assert_eq!(
        store
            .read_plugin_storage("active-memory", None)
            .expect("read migrated plugin storage")
            .expect("migrated plugin key exists")
            .value,
        json!({ "turns": [1, 2, 3] })
    );
    let old_root: String = store
        .connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-old'",
            [],
            |row| row.get(0),
        )
        .expect("read migrated old root");
    assert_eq!(
        serde_json::from_str::<Value>(&old_root).expect("parse old root"),
        json!({ "username": "V1 old" })
    );
    let old_presets: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM bot_presets WHERE generation = 'revision-old'",
            [],
            |row| row.get(0),
        )
        .expect("count old presets");
    assert_eq!(old_presets, 1);
    let old_plugin_storage: String = store
        .connection
        .query_row(
            "SELECT value FROM plugin_storage
             WHERE generation = 'revision-old' AND storage_key = 'old-memory'",
            [],
            |row| row.get(0),
        )
        .expect("read old plugin storage");
    assert_eq!(
        serde_json::from_str::<Value>(&old_plugin_storage).expect("parse old plugin storage"),
        json!("preserved")
    );
    let summary = &store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: true,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query migrated character")
        .items[0];
    assert_eq!(summary.r#type, "character");
    assert_eq!(summary.creator_notes.as_deref(), Some("migrated notes"));
    assert_eq!(summary.trash_time, Some(123));
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated version");
    assert_eq!(version, 4);
}

#[test]
fn schema_v4_rolls_back_when_v1_bot_presets_is_not_an_array() {
    let directory = tempfile::tempdir().expect("create migration rollback directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v1_database(&database_path);
    let invalid_root = json!({
        "username": "Invalid v1 root",
        "botPresets": { "legacy": "unsupported" }
    });
    let connection = rusqlite::Connection::open(&database_path).expect("open v1 database");
    let original_active: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-7'",
            [],
            |row| row.get(0),
        )
        .expect("read original active v1 root");
    connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params!["revision-old", invalid_root.to_string()],
        )
        .expect("write invalid v1 root");
    drop(connection);

    assert!(PersistentStore::open(directory.path()).is_err());

    let connection =
        rusqlite::Connection::open(&database_path).expect("reopen rolled back v1 database");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read rolled back schema version");
    let preserved: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-old'",
            [],
            |row| row.get(0),
        )
        .expect("read preserved v1 root");
    let preserved_active: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-7'",
            [],
            |row| row.get(0),
        )
        .expect("read preserved active v1 root");
    let preset_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'bot_presets'",
            [],
            |row| row.get(0),
        )
        .expect("check rolled back preset table");
    assert_eq!(version, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&preserved).expect("parse preserved root"),
        invalid_root
    );
    assert_eq!(preserved_active, original_active);
    assert_eq!(preset_table_count, 0);
}

#[test]
fn schema_v4_rolls_back_when_v1_plugin_storage_is_not_an_object() {
    let directory = tempfile::tempdir().expect("create plugin migration rollback directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v1_database(&database_path);
    let invalid_root = json!({
        "username": "Invalid plugin root",
        "botPresets": [{ "name": "Still valid" }],
        "pluginCustomStorage": ["unsupported"]
    });
    let connection = rusqlite::Connection::open(&database_path).expect("open v1 database");
    connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params!["revision-old", invalid_root.to_string()],
        )
        .expect("write invalid plugin storage");
    drop(connection);

    assert!(PersistentStore::open(directory.path()).is_err());

    let connection =
        rusqlite::Connection::open(&database_path).expect("reopen rolled back plugin migration");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read rolled back version");
    let preserved: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-old'",
            [],
            |row| row.get(0),
        )
        .expect("read preserved plugin root");
    let plugin_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'plugin_storage'",
            [],
            |row| row.get(0),
        )
        .expect("check rolled back plugin table");
    assert_eq!(version, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&preserved).expect("parse preserved plugin root"),
        invalid_root
    );
    assert_eq!(plugin_table_count, 0);
}

#[test]
fn pending_v1_snapshot_restores_then_migrates_to_v4() {
    let directory = tempfile::tempdir().expect("create restore directory");
    let store = PersistentStore::open(directory.path()).expect("open current v2 store");
    let candidate = directory
        .path()
        .join("persistent/snapshots/persistent-v1.db");
    create_v1_database(&candidate);
    store
        .snapshot_restore_request(&candidate)
        .expect("request v1 snapshot restore");
    drop(store);

    let restored =
        PersistentStore::open(directory.path()).expect("restore and migrate v1 snapshot");
    assert_eq!(restored.revision().expect("read restored revision"), 7);
    assert_eq!(
        restored
            .query_presets(None)
            .expect("query restored presets")
            .items[0]
            .name,
        "V1 first"
    );
    let version: i64 = restored
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read restored version");
    assert_eq!(version, 4);
}

#[test]
fn invalid_pending_v1_snapshot_preserves_live_database_and_restore_marker() {
    let directory = tempfile::tempdir().expect("create invalid restore directory");
    let mut store = PersistentStore::open(directory.path()).expect("open current v2 store");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({ "username": "Preserved live database" })),
            replace_presets: Some(vec![json!({ "name": "Live preset" })]),
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
        })
        .expect("seed live database");

    let candidate = directory
        .path()
        .join("persistent/snapshots/persistent-invalid-v1.db");
    create_v1_database(&candidate);
    let connection = rusqlite::Connection::open(&candidate).expect("open invalid v1 candidate");
    connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params![
                "revision-7",
                json!({ "username": "Invalid candidate", "botPresets": { "legacy": true } })
                    .to_string()
            ],
        )
        .expect("corrupt candidate preset shape");
    drop(connection);
    store
        .snapshot_restore_request(&candidate)
        .expect("request invalid v1 restore");
    drop(store);

    let reopened = PersistentStore::open(directory.path())
        .expect("invalid candidate must not replace live database");
    assert_eq!(reopened.revision().expect("read preserved revision"), 1);
    assert_eq!(
        reopened
            .read_root(None)
            .expect("read preserved live root")
            .value["username"],
        "Preserved live database"
    );
    assert_eq!(
        reopened
            .query_presets(None)
            .expect("query preserved live presets")
            .items[0]
            .name,
        "Live preset"
    );
    assert!(directory
        .path()
        .join("persistent/snapshots/pending-restore.json")
        .is_file());
    assert!(candidate.is_file());
}

#[test]
fn semantically_invalid_pending_v1_snapshot_preserves_live_database_and_restore_marker() {
    let directory = tempfile::tempdir().expect("create semantic restore directory");
    let mut store = PersistentStore::open(directory.path()).expect("open current v2 store");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({ "username": "Preserved semantic live database" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
        })
        .expect("seed semantic live database");

    let candidate = directory
        .path()
        .join("persistent/snapshots/persistent-semantic-invalid-v1.db");
    create_v1_database(&candidate);
    let connection = rusqlite::Connection::open(&candidate).expect("open semantic v1 candidate");
    connection
        .execute(
            "UPDATE meta SET value = '\"missing-generation\"' WHERE key = 'activeGeneration'",
            [],
        )
        .expect("make active generation semantically invalid");
    drop(connection);
    store
        .snapshot_restore_request(&candidate)
        .expect("request semantic invalid v1 restore");
    drop(store);

    let reopened = PersistentStore::open(directory.path())
        .expect("semantic invalid candidate must not replace live database");
    assert_eq!(reopened.revision().expect("read preserved revision"), 1);
    assert_eq!(
        reopened
            .read_root(None)
            .expect("read preserved semantic live root")
            .value["username"],
        "Preserved semantic live database"
    );
    assert!(directory
        .path()
        .join("persistent/snapshots/pending-restore.json")
        .is_file());
    assert!(candidate.is_file());
}

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
    assert_eq!(integer_pragma("user_version"), 4);
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
            expected_revision: 1,
            root: Some(json!({ "username": "Changed after snapshot" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
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
fn checkpoints_accept_both_documented_modes() {
    let (_directory, store, _) = open_fixture();

    store
        .checkpoint(CheckpointMode::Passive)
        .expect("passive checkpoint");
    store
        .checkpoint(CheckpointMode::Truncate)
        .expect("truncate checkpoint");
}

fn snapshots_dir(directory: &tempfile::TempDir) -> PathBuf {
    directory.path().join("persistent/snapshots")
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

fn stage_root(store: &mut PersistentStore, username: &str) -> String {
    let staging = store.replace_begin().expect("begin staged replacement");
    store
        .replace_put_root(&staging.staging_id, &json!({ "username": username }))
        .expect("stage replacement root");
    staging.staging_id
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
    assert!(created[1..].iter().all(|path| Path::new(path).is_file()));
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
    let sparse_bytes = logical_bytes * 3 / 4;
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
    assert!(!sparse[0].exists());
    assert!(sparse[1..].iter().all(|path| path.is_file()));
    assert!(Path::new(&created.path).is_file());
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
            expected_revision: 1,
            root: Some(json!({ "username": "Current before restore" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
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
                expected_revision: 1,
                root: Some(json!({ "username": "Current protected data" })),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
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
                .execute_batch("PRAGMA user_version = 5;")
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

#[test]
fn replacements_snapshot_only_nonzero_revisions_and_abort_on_snapshot_failure() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let seed = stage_root(&mut store, "Seed");
    store
        .replace_commit(&seed, Some(0))
        .expect("activate revision one");
    assert!(store
        .snapshot_list()
        .expect("list seed snapshots")
        .is_empty());

    let replacement = stage_root(&mut store, "Revision two");
    store
        .replace_commit(&replacement, Some(1))
        .expect("activate protected replacement");
    let snapshots = store.snapshot_list().expect("list pre-replace snapshots");
    assert_eq!(snapshots.len(), 1);
    let snapshot =
        rusqlite::Connection::open(&snapshots[0].path).expect("open pre-replace snapshot");
    let revision: String = snapshot
        .query_row(
            "SELECT value FROM meta WHERE key = 'currentRevision'",
            [],
            |row| row.get(0),
        )
        .expect("read snapshotted revision");
    assert_eq!(revision, "1");
    drop(snapshot);

    let failed = stage_root(&mut store, "Must not activate");
    for entry in fs::read_dir(snapshots_dir(&directory)).expect("read snapshots directory") {
        let path = entry.expect("read snapshot entry").path();
        fs::remove_file(path).expect("remove snapshot file");
    }
    fs::remove_dir(snapshots_dir(&directory)).expect("remove snapshots directory");
    fs::write(snapshots_dir(&directory), b"blocks directory creation")
        .expect("block snapshot directory");
    assert!(store.replace_commit(&failed, Some(2)).is_err());
    assert_eq!(store.revision().expect("read preserved revision"), 2);
    assert_eq!(
        store.read_root(None).expect("read preserved root").value["username"],
        "Revision two"
    );
}

#[test]
fn checkpoint_truncate_reports_busy_and_truncates_when_unblocked() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    store
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0; INSERT INTO app_kv VALUES ('first', '1');")
        .expect("create initial WAL frames");
    let database_path = directory.path().join("persistent/persistent.db");
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
