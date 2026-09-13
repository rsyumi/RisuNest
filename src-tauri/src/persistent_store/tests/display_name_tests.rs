use super::*;
use crate::logical_records::{LogicalRecordEnvelope, LogicalRecordLocator};
use crate::persistent_store::record_apply::{apply_materialized_record, validate_locator_envelope};

fn blank_character(kind: &str) -> Value {
    json!({
        "type": kind, "chaId": "synthetic-blank", "name": "",
        "chats": [{"id": "synthetic-chat", "name": "", "message": []}]
    })
}

#[test]
fn empty_names_survive_creation_edit_and_reopen() {
    for kind in ["character", "group"] {
        let (directory, mut store, _) = open_fixture();
        let mut character = blank_character(kind);
        store
            .commit(&WorkingSetCommit {
                add_character: Some(character.clone()),
                ..empty_working_set_commit(1)
            })
            .expect("create unnamed character or group");

        let messages = vec![json!({"role": "user", "data": "synthetic message"})];
        store
            .commit(&WorkingSetCommit {
                character: Some(json!({"type": kind, "chaId": "synthetic-blank", "name": ""})),
                conversations: Some(vec![ConversationMutation::ReplaceRange {
                    character_id: "synthetic-blank".into(),
                    conversation_id: "synthetic-chat".into(),
                    start: 0,
                    delete_count: 0,
                    messages: messages.clone(),
                    conversation: Some(json!({"id": "synthetic-chat", "name": ""})),
                    configured_index: None,
                }]),
                ..empty_working_set_commit(2)
            })
            .expect("edit unnamed character and conversation");
        character["chats"][0]["message"] = json!(messages);
        drop(store);
        let store = PersistentStore::open(directory.path()).unwrap();
        assert_eq!(store.revision().unwrap(), 3);
        let database = store.materialize(None).unwrap();
        assert_eq!(
            database["characters"]
                .as_array()
                .unwrap()
                .iter()
                .find(|value| value["chaId"] == "synthetic-blank")
                .unwrap(),
            &character
        );
        let catalog = store
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
            .unwrap();
        assert_eq!(
            catalog
                .items
                .iter()
                .find(|item| item.id == "synthetic-blank")
                .unwrap()
                .name,
            ""
        );
        let conversations = store
            .query_conversations(
                &ConversationQuery {
                    character_id: "synthetic-blank".into(),
                    order: QueryOrder::Configured,
                    limit: 10,
                    cursor: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(conversations.items[0].name, "");
        assert_eq!(conversations.items[0].message_count, 1);
    }
}

#[test]
fn empty_names_round_trip_through_staged_replacement() {
    let (directory, mut store, mut database) = open_fixture();
    for character in database["characters"].as_array_mut().unwrap() {
        character["name"] = json!("");
        for chat in character["chats"].as_array_mut().unwrap() {
            chat["name"] = json!("");
        }
    }
    let stage = store.replace_begin().unwrap();
    store
        .replace_put_root(&stage.staging_id, &staged_root(&database))
        .unwrap();
    store
        .replace_put_presets(
            &stage.staging_id,
            database["botPresets"].as_array().unwrap(),
        )
        .unwrap();
    store
        .replace_add_characters(
            &stage.staging_id,
            database["characters"].as_array().unwrap(),
        )
        .expect("stage unnamed records");
    store.replace_commit(&stage.staging_id, Some(1)).unwrap();
    drop(store);
    let store = PersistentStore::open(directory.path()).unwrap();
    assert_eq!(store.materialize(None).unwrap(), database);
}

#[test]
fn invalid_names_and_empty_ids_still_reject_addition_atomically() {
    let (_directory, mut store, database) = open_fixture();
    for is_chat in [false, true] {
        for invalid in [None, Some(Value::Null), Some(json!(42)), Some(json!([]))] {
            let mut character = blank_character("character");
            let target = if is_chat {
                &mut character["chats"][0]
            } else {
                &mut character
            };
            match invalid {
                Some(value) => {
                    target["name"] = value;
                }
                None => {
                    target.as_object_mut().unwrap().remove("name");
                }
            }
            assert!(matches!(
                store.commit(&WorkingSetCommit {
                    add_character: Some(character),
                    ..empty_working_set_commit(1)
                }),
                Err(StoreError::Validation { .. })
            ));
            assert_eq!(store.revision().unwrap(), 1);
            assert_eq!(store.materialize(None).unwrap(), database);
        }
        let mut character = blank_character("character");
        if is_chat {
            character["chats"][0]["id"] = json!("");
        } else {
            character["chaId"] = json!("");
        }
        assert!(matches!(
            store.commit(&WorkingSetCommit {
                add_character: Some(character),
                ..empty_working_set_commit(1)
            }),
            Err(StoreError::Validation { .. })
        ));
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(None).unwrap(), database);
    }
}

fn logical_records(
    name: Option<Value>,
    id: &str,
) -> Vec<(LogicalRecordLocator, LogicalRecordEnvelope)> {
    let mut character = json!({"chaId": id, "type": "character"});
    let mut conversation = json!({"id": id});
    if let Some(name) = name {
        character["name"] = name.clone();
        conversation["name"] = name;
    }
    vec![
        (
            LogicalRecordLocator::Character {
                character_id: "synthetic-blank".into(),
            },
            LogicalRecordEnvelope::Character {
                configured_index: 3,
                detail: character,
                owner_heads: vec![],
            },
        ),
        (
            LogicalRecordLocator::Conversation {
                character_id: "synthetic-blank".into(),
                conversation_id: "synthetic-blank".into(),
            },
            LogicalRecordEnvelope::Conversation {
                configured_index: 0,
                recent_at: 0,
                detail: conversation,
                message_page_hashes: vec![],
            },
        ),
    ]
}

#[test]
fn empty_names_validate_and_apply_as_logical_records() {
    let (_directory, mut store, _) = open_fixture();
    let generation = active_generation(&store.connection).unwrap();
    let transaction = store.connection.transaction().unwrap();
    for (locator, envelope) in logical_records(Some(json!("")), "synthetic-blank") {
        validate_locator_envelope(&locator, &envelope).expect("validate unnamed logical record");
        apply_materialized_record(&transaction, &generation, locator, envelope, Some(&[]))
            .expect("apply unnamed logical record");
    }
    transaction.commit().unwrap();
    let database = store.materialize(None).unwrap();
    let character = database["characters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["chaId"] == "synthetic-blank")
        .unwrap();
    assert_eq!(character["name"], "");
    assert_eq!(character["chats"][0]["name"], "");
}

#[test]
fn logical_records_still_require_string_names_and_matching_nonempty_ids() {
    for name in [None, Some(Value::Null), Some(json!(42)), Some(json!([]))] {
        for (locator, envelope) in logical_records(name, "synthetic-blank") {
            assert!(matches!(
                validate_locator_envelope(&locator, &envelope),
                Err(StoreError::Validation { .. })
            ));
        }
    }
    for id in ["", "wrong-id"] {
        for (locator, envelope) in logical_records(Some(json!("")), id) {
            assert!(matches!(
                validate_locator_envelope(&locator, &envelope),
                Err(StoreError::Validation { .. })
            ));
        }
    }
}
