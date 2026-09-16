use super::super::{server_sync_outbox::ServerDirtyKey, server_sync_projection as projection};
use super::*;
use crate::asset_repository::PayloadCas;

#[test]
fn server_projection_preserves_all_families_and_removes_only_local_activity() {
    let (directory, mut store, database) = open_fixture();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let unprepared = ServerDirtyKey {
        kind: "character".into(),
        key1: "char-a".into(),
        key2: "".into(),
        revision: 1,
    };
    // main now derives absent heads from valid sparse ownership. This is a
    // normal source representation, not a missing-payload error.
    assert!(projection::project(
        &store.connection,
        &cas,
        &active_generation(&store.connection).unwrap(),
        &unprepared
    )
    .unwrap()
    .is_some());
    // persistentStorageRuntime prepares owner coverage before native sync is available.
    // The fixture has no additional-assets properties, so every owner is absent.
    let heads = database["characters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            AssetOwnerHead::absent(AssetOwnerLocator::CharacterAdditionalAssets {
                character_id: c["chaId"].as_str().unwrap().into(),
            })
        })
        .collect();
    let details = database["characters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let mut detail = c.clone();
            detail.as_object_mut().unwrap().shift_remove("chats");
            detail
        })
        .collect();
    store
        .commit(&WorkingSetCommit {
            character_details: Some(details),
            asset_owner_heads: Some(heads),
            ..empty_working_set_commit(1)
        })
        .unwrap();
    // Library credentials/settings are shared losslessly with the trusted server.
    // Device credentials live outside these records (covered by backup tests).
    let mut root = store.read_root(None).unwrap().value;
    root["openAIKey"] = json!("synthetic-provider-secret");
    root["syntheticPreference"] = json!({"enabled":true,"value":"unchanged"});
    let plugin_value = json!({"token":"synthetic-plugin-secret","nested":[null,"가🦀",{}]});
    store
        .commit(&WorkingSetCommit {
            root: Some(root),
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: "synthetic-plugin".to_owned(),
                key: "synthetic-secret-contract".into(),
                value: plugin_value.clone(),
            }]),
            ..empty_working_set_commit(store.revision().unwrap())
        })
        .unwrap();
    let generation = active_generation(&store.connection).unwrap();
    let mut keys = Vec::new();
    let mut after = None;
    loop {
        let page = projection::all_keys_page(
            &store.connection,
            &generation,
            after
                .as_ref()
                .map(|k: &ServerDirtyKey| (k.kind.as_str(), k.key1.as_str(), k.key2.as_str())),
            2,
            1,
        )
        .unwrap();
        if page.is_empty() {
            break;
        }
        after = page.last().cloned();
        keys.extend(page);
    }
    assert!(keys
        .windows(2)
        .all(|w| (&w[0].kind, &w[0].key1, &w[0].key2) < (&w[1].kind, &w[1].key1, &w[1].key2)));
    for key in &keys {
        let payload = projection::project(&store.connection, &cas, &generation, key)
            .unwrap()
            .unwrap();
        let bytes = serde_json::to_vec(&payload).unwrap();
        let decoded: projection::ServerPayload = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);
        if key.kind == "conversation" {
            assert!(payload.messages.is_some());
        }
        match &payload.record {
            crate::logical_records::LogicalRecordEnvelope::Root { value, .. } => {
                assert_eq!(value["openAIKey"], "synthetic-provider-secret");
                assert_eq!(
                    value["syntheticPreference"],
                    json!({"enabled":true,"value":"unchanged"})
                );
            }
            crate::logical_records::LogicalRecordEnvelope::Plugin { value, .. }
                if key.key1 == "synthetic-secret-contract" =>
            {
                assert_eq!(value, &plugin_value)
            }
            _ => (),
        }
    }
    let root_key = ServerDirtyKey {
        kind: "root".into(),
        key1: "".into(),
        key2: "".into(),
        revision: 1,
    };
    store
        .connection
        .execute(
            "UPDATE root SET value=json_set(value,'$.statics.messages',1) WHERE generation=?1",
            [&generation],
        )
        .unwrap();
    let first = projection::project(&store.connection, &cas, &generation, &root_key)
        .unwrap()
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE root SET value=json_set(value,'$.statics.messages',2000) WHERE generation=?1",
            [&generation],
        )
        .unwrap();
    let second = projection::project(&store.connection, &cas, &generation, &root_key)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    let character = keys.iter().find(|k| k.kind == "character").unwrap();
    let before = projection::project(&store.connection, &cas, &generation, character)
        .unwrap()
        .unwrap();
    store.connection.execute("UPDATE characters SET detail=json_set(detail,'$.chatPage',3,'$.lastInteraction',123456) WHERE generation=?1 AND character_id=?2",params![generation,character.key1]).unwrap();
    let after = projection::project(&store.connection, &cas, &generation, character)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&before).unwrap(),
        serde_json::to_vec(&after).unwrap()
    );
    store.connection.execute("UPDATE characters SET configured_index=configured_index+1 WHERE generation=?1 AND character_id=?2",params![generation,character.key1]).unwrap();
    let reordered = projection::project(&store.connection, &cas, &generation, character)
        .unwrap()
        .unwrap();
    assert_ne!(
        serde_json::to_vec(&before).unwrap(),
        serde_json::to_vec(&reordered).unwrap()
    );
}

#[test]
fn server_projection_derives_imported_owner_objects_and_rejects_malformed_sources() {
    let (directory, store, _) = open_fixture();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let generation = active_generation(&store.connection).unwrap();
    let key = ServerDirtyKey {
        kind: "character".into(),
        key1: "char-a".into(),
        key2: String::new(),
        revision: 1,
    };
    store.connection.execute("UPDATE characters SET detail=json_set(detail,'$.additionalAssets',json(?2)) WHERE generation=?1 AND character_id='char-a'",params![generation,r#"[["synthetic","assets/unresolved","png"]]"#]).unwrap();
    let projected = projection::project(&store.connection, &cas, &generation, &key)
        .unwrap()
        .unwrap();
    assert_eq!(projected.derived_objects.len(), 1);
    let dependencies = projection::dependencies(&projected, &cas).unwrap();
    assert_eq!(
        dependencies,
        projected
            .derived_objects
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    );
    for (hash, bytes) in &projected.derived_objects {
        assert_eq!(*hash, risunest_sync_wire::hash(bytes));
    }
    store.connection.execute("UPDATE characters SET detail=json_set(detail,'$.additionalAssets',json(?2)) WHERE generation=?1 AND character_id='char-a'",params![generation,r#"[["malformed"]]"#]).unwrap();
    assert!(projection::project(&store.connection, &cas, &generation, &key).is_err());
}
