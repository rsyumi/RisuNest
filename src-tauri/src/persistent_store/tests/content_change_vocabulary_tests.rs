//! The `content_changes` locator vocabulary every consumer dispatches on.
use super::super::content_change_index as changes;
use super::*;

fn recorded(store: &PersistentStore, after_revision: i64) -> Vec<(String, String, String)> {
    let generation = active_generation(&store.connection).expect("active generation");
    let mut statement = store
        .connection
        .prepare(
            "SELECT kind,key1,key2 FROM content_changes
             WHERE generation=?1 AND revision>?2 ORDER BY kind,key1,key2",
        )
        .expect("prepare content change query");
    statement
        .query_map(params![generation, after_revision], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .expect("query content changes")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect content changes")
}

fn key(kind: &str, key1: &str, key2: &str) -> (String, String, String) {
    (kind.to_owned(), key1.to_owned(), key2.to_owned())
}

#[test]
fn root_and_preset_mutations_record_their_own_locators() {
    let (_directory, mut store, database) = open_fixture();
    let before = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit {
            root_mutations: Some(vec![super::super::RootMutation::Set {
                key: "theme".to_owned(),
                value: json!("changed"),
            }]),
            ..empty_working_set_commit(before)
        })
        .unwrap();
    assert_eq!(recorded(&store, before), vec![key("root", "", "")]);

    let before = store.revision().unwrap();
    let mut presets = database["botPresets"].as_array().unwrap().clone();
    presets[0]["name"] = json!("Renamed");
    store
        .commit(&WorkingSetCommit {
            replace_presets: Some(presets),
            ..empty_working_set_commit(before)
        })
        .unwrap();
    assert_eq!(
        recorded(&store, before),
        vec![key("preset", "0", ""), key("preset", "1", "")]
    );
}

#[test]
fn character_detail_edits_record_only_the_character() {
    let (_directory, mut store, _) = open_fixture();
    let before = store.revision().unwrap();
    let mut detail = store.read_character("char-a", None).unwrap().unwrap().value;
    detail["name"] = json!("Renamed A");
    store
        .commit(&WorkingSetCommit {
            character: Some(detail),
            ..empty_working_set_commit(before)
        })
        .unwrap();
    assert_eq!(recorded(&store, before), vec![key("character", "char-a", "")]);
}

#[test]
fn character_addition_records_the_character_and_every_conversation() {
    let (_directory, mut store, _) = open_fixture();
    let before = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit {
            add_character: Some(json!({
                "type": "character",
                "chaId": "char-new",
                "name": "New",
                "chats": [
                    { "id": "conv-new-one", "name": "One", "message": [] },
                    { "id": "conv-new-two", "name": "Two", "message": [] }
                ]
            })),
            ..empty_working_set_commit(before)
        })
        .unwrap();
    assert_eq!(
        recorded(&store, before),
        vec![
            key("character", "char-new", ""),
            key("conversation", "char-new", "conv-new-one"),
            key("conversation", "char-new", "conv-new-two"),
        ]
    );
}

#[test]
fn character_deletion_records_the_character_and_its_removed_conversations() {
    let (_directory, mut store, _) = open_fixture();
    let before = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit {
            delete_character_id: Some("char-a".to_owned()),
            ..empty_working_set_commit(before)
        })
        .unwrap();
    assert_eq!(
        recorded(&store, before),
        vec![
            key("character", "char-a", ""),
            key("conversation", "char-a", "conv-long"),
            key("conversation", "char-a", "conv-short"),
        ]
    );
}

#[test]
fn message_edits_and_conversation_deletes_record_the_conversation_and_its_character() {
    let (_directory, mut store, _) = open_fixture();
    let before = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit {
            conversations: Some(vec![ConversationMutation::ReplaceRange {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
                start: 0,
                delete_count: 0,
                messages: vec![json!({ "role": "user", "data": "added", "chatId": "added" })],
                conversation: None,
                configured_index: None,
            }]),
            ..empty_working_set_commit(before)
        })
        .unwrap();
    assert_eq!(
        recorded(&store, before),
        vec![
            key("character", "char-a", ""),
            key("conversation", "char-a", "conv-short"),
        ]
    );

    let before = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit {
            conversations: Some(vec![ConversationMutation::Delete {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
            }]),
            ..empty_working_set_commit(before)
        })
        .unwrap();
    assert_eq!(
        recorded(&store, before),
        vec![
            key("character", "char-a", ""),
            key("conversation", "char-a", "conv-short"),
        ]
    );
}

#[test]
fn plugin_writes_record_owner_and_storage_key_separately() {
    let (_directory, mut store, _) = open_fixture();
    let before = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![
                PluginStorageMutation::Set {
                    owner: "alpha".to_owned(),
                    key: "shared".to_owned(),
                    value: json!(1),
                },
                PluginStorageMutation::Set {
                    owner: "beta".to_owned(),
                    key: "shared".to_owned(),
                    value: json!(2),
                },
            ]),
            ..empty_working_set_commit(before)
        })
        .unwrap();
    assert_eq!(
        recorded(&store, before),
        vec![
            key("plugin", "alpha", "shared"),
            key("plugin", "beta", "shared"),
        ]
    );

    let before = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Delete {
                owner: "alpha".to_owned(),
                key: "shared".to_owned(),
            }]),
            ..empty_working_set_commit(before)
        })
        .unwrap();
    assert_eq!(recorded(&store, before), vec![key("plugin", "alpha", "shared")]);
}

#[test]
fn asset_and_inlay_aliases_record_their_own_kinds() {
    let (_directory, mut store, _) = open_fixture();
    let before = store.revision().unwrap();
    store
        .commit_asset_alias(
            &AssetAlias {
                key: "asset-one".to_owned(),
                object_hash: Some("aa".repeat(32)),
                kind: "asset".to_owned(),
                size: 3,
                mime: "application/octet-stream".to_owned(),
                name: "one".to_owned(),
                ext: "bin".to_owned(),
                inlay_type: None,
                width: None,
                height: None,
                metadata: json!({}),
            },
            before,
        )
        .unwrap();
    assert_eq!(recorded(&store, before), vec![key("asset", "asset-one", "")]);

    let before = store.revision().unwrap();
    store
        .commit_asset_alias(
            &AssetAlias {
                key: "inlay-one".to_owned(),
                object_hash: Some("bb".repeat(32)),
                kind: "inlay".to_owned(),
                size: 3,
                mime: "image/png".to_owned(),
                name: "one".to_owned(),
                ext: "png".to_owned(),
                inlay_type: Some("image".to_owned()),
                width: Some(1),
                height: Some(1),
                metadata: json!({}),
            },
            before,
        )
        .unwrap();
    assert_eq!(recorded(&store, before), vec![key("inlay", "inlay-one", "")]);

    let before = store.revision().unwrap();
    store.delete_asset_alias("inlay", "inlay-one", before).unwrap();
    assert_eq!(recorded(&store, before), vec![key("inlay", "inlay-one", "")]);
}

#[test]
fn owner_head_changes_always_accompany_their_parent_record() {
    let (_directory, mut store, database) = open_fixture();
    let before = store.revision().unwrap();
    let mut value = root(&database);
    value["modules"] = json!([{ "id": "one", "assets": [["a", "assets/a", "bin"]] }]);
    store
        .commit(&WorkingSetCommit {
            root: Some(value),
            asset_owner_heads: Some(vec![AssetOwnerHead::present(
                AssetOwnerLocator::RootModuleAssets { index: 0 },
                "11".repeat(32),
                1,
            )]),
            ..empty_working_set_commit(before)
        })
        .unwrap();
    assert_eq!(
        recorded(&store, before),
        vec![key("owner", "root-module-assets", "0"), key("root", "", "")]
    );

    let before = store.revision().unwrap();
    let mut detail = store.read_character("char-a", None).unwrap().unwrap().value;
    detail["additionalAssets"] = json!([["extra", "assets/extra", "bin"]]);
    store
        .commit(&WorkingSetCommit {
            character: Some(detail),
            asset_owner_heads: Some(vec![AssetOwnerHead::present(
                AssetOwnerLocator::CharacterAdditionalAssets {
                    character_id: "char-a".to_owned(),
                },
                "22".repeat(32),
                1,
            )]),
            ..empty_working_set_commit(before)
        })
        .unwrap();
    assert_eq!(
        recorded(&store, before),
        vec![
            key("character", "char-a", ""),
            key("owner", "character-additional-assets", "char-a"),
        ]
    );
}

#[test]
fn archiving_and_restoring_record_the_character_and_its_conversations() {
    let (_directory, mut store, _) = open_fixture();
    let before = store.revision().unwrap();
    store.archive_character("char-a", before, 1_700_000_000_000).unwrap();
    assert_eq!(
        recorded(&store, before),
        vec![
            key("character", "char-a", ""),
            key("conversation", "char-a", "conv-long"),
            key("conversation", "char-a", "conv-short"),
        ]
    );

    let before = store.revision().unwrap();
    store.restore_character("char-a", before).unwrap();
    assert_eq!(
        recorded(&store, before),
        vec![
            key("character", "char-a", ""),
            key("conversation", "char-a", "conv-long"),
            key("conversation", "char-a", "conv-short"),
        ]
    );
}

/// `cold` retired with cold storage and nothing replaced it: archiving reaches
/// consumers as the character and conversation locators above.
#[test]
fn the_recorded_kinds_stay_inside_the_published_vocabulary() {
    let (_directory, mut store, _) = open_fixture();
    store
        .commit(&WorkingSetCommit {
            root_mutations: Some(vec![super::super::RootMutation::Set {
                key: "theme".to_owned(),
                value: json!("changed"),
            }]),
            ..empty_working_set_commit(store.revision().unwrap())
        })
        .unwrap();
    let mut kinds = store
        .connection
        .prepare("SELECT DISTINCT kind FROM content_changes")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    kinds.sort();
    for kind in &kinds {
        assert!(
            matches!(
                kind.as_str(),
                "root"
                    | "preset"
                    | "character"
                    | "conversation"
                    | "plugin"
                    | "asset"
                    | "inlay"
                    | "owner"
                    | "full"
            ),
            "unexpected content change kind {kind}"
        );
    }
}

#[test]
fn a_full_replacement_clears_the_window_and_demands_a_rebuild() {
    let (_directory, mut store, database) = open_fixture();
    let tx = store.connection.transaction().unwrap();
    let generation = active_generation(&tx).unwrap();
    changes::commit_cursor(&tx, "ui-working-set", &generation, 1).unwrap();
    tx.commit().unwrap();
    store
        .commit(&WorkingSetCommit {
            root_mutations: Some(vec![super::super::RootMutation::Set {
                key: "theme".to_owned(),
                value: json!("changed"),
            }]),
            ..empty_working_set_commit(store.revision().unwrap())
        })
        .unwrap();

    let staging = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&staging, &staged_root(&database)).unwrap();
    store.replace_put_presets(&staging, &[]).unwrap();
    store.replace_commit(&staging, None).unwrap();

    assert_eq!(recorded(&store, 0), Vec::new());
    let rebuild: i64 = store
        .connection
        .query_row(
            "SELECT rebuild_required FROM content_change_consumers WHERE id='ui-working-set'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rebuild, 1);
}
