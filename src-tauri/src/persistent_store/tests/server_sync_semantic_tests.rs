use super::*;

#[test]
fn separate_conversations_keep_local_views_and_alias_conflicts_preserve_both_sides() {
    let directory = tempfile::tempdir().unwrap();
    let server = Arc::new(Store::init(directory.path()).unwrap());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let remote = server.clone();
    let task = runtime.spawn(async move {
        axum::serve(listener, http::router(remote)).await.unwrap();
    });
    let (_first_dir, mut first) = prepared();
    let (_second_dir, mut second) = prepared();
    for store in [&mut first, &mut second] {
        let device = server.add_device().unwrap();
        store
            .server_bind(&ServerConfig {
                directory: None,
                endpoint: endpoint.clone(),
                library_id: device.library_id,
                device_id: device.device_id,
                token: device.token,
            })
            .unwrap();
        assert_eq!(settle(store).phase, "idle");
    }
    // Seed ordered keys in a shared base. Concurrent insertion into the same
    // key-order position is a real order conflict, tested separately below.
    first
        .commit(&WorkingSetCommit {
            plugin_storage: Some(
                (0..2)
                    .map(|view| PluginStorageMutation::Set {
                        key: format!("independent-{view}"),
                        value: json!("base"),
                    })
                    .collect(),
            ),
            ..empty_working_set_commit(first.revision().unwrap())
        })
        .unwrap();
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(settle(&mut second).phase, "idle");
    for (store, conversation, view) in
        [(&mut first, "conv-long", 1), (&mut second, "conv-short", 0)]
    {
        let generation = active_generation(&store.connection).unwrap();
        let raw: String = store
            .connection
            .query_row(
                "SELECT detail FROM characters WHERE generation=?1 AND character_id='char-a'",
                [&generation],
                |r| r.get(0),
            )
            .unwrap();
        let mut detail: Value = serde_json::from_str(&raw).unwrap();
        detail["chatPage"] = json!(view);
        detail["lastInteraction"] = json!(1000 + view);
        let mut root = store.read_root(None).unwrap().value;
        root["statics"] = json!({"messages":100+view});
        store
            .commit(&WorkingSetCommit {
                root: Some(root),
                character_details: Some(vec![detail]),
                conversations: Some(vec![ConversationMutation::ReplaceRange {
                    character_id: "char-a".into(),
                    conversation_id: conversation.into(),
                    start: 0,
                    delete_count: 0,
                    messages: vec![json!({"role":"user","data":conversation,"chatId":null})],
                    conversation: None,
                    configured_index: None,
                }]),
                plugin_storage: Some(vec![PluginStorageMutation::Set {
                    key: format!("independent-{view}"),
                    value: json!(view),
                }]),
                ..empty_working_set_commit(store.revision().unwrap())
            })
            .unwrap();
    }
    let sent = settle(&mut first);
    assert_eq!(sent.phase, "idle", "sender conflicts: {:?}", sent.conflicts);
    let received = settle(&mut second);
    assert_eq!(
        received.phase, "idle",
        "receiver conflicts: {:?}",
        received.conflicts
    );
    let confirmed = settle(&mut first);
    assert_eq!(
        confirmed.phase, "idle",
        "confirmation conflicts: {:?}",
        confirmed.conflicts
    );
    for (store, view) in [(&first, 1), (&second, 0)] {
        let generation = active_generation(&store.connection).unwrap();
        let actual:i64=store.connection.query_row("SELECT json_extract(detail,'$.chatPage') FROM characters WHERE generation=?1 AND character_id='char-a'",[&generation],|r|r.get(0)).unwrap();
        assert_eq!(actual, view);
        assert_eq!(
            store.read_root(None).unwrap().value["statics"]["messages"],
            100 + view
        );
        for conversation in ["conv-long", "conv-short"] {
            let value:String=store.connection.query_row("SELECT json_extract(value,'$.data') FROM messages WHERE generation=?1 AND character_id='char-a' AND conversation_id=?2 AND message_index=0",params![generation,conversation],|r|r.get(0)).unwrap();
            assert_eq!(value, conversation);
        }
        let plugins:i64=store.connection.query_row("SELECT count(*) FROM plugin_storage WHERE generation=?1 AND storage_key LIKE 'independent-%'",[generation],|r|r.get(0)).unwrap();
        assert_eq!(plugins, 2);
    }
    for (store, id, index) in [
        (&mut first, "char-a", 0usize),
        (&mut second, "char-b", 1usize),
    ] {
        let generation = active_generation(&store.connection).unwrap();
        let raw: String = store
            .connection
            .query_row(
                "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
                params![generation, id],
                |r| r.get(0),
            )
            .unwrap();
        let mut detail: Value = serde_json::from_str(&raw).unwrap();
        detail["description"] = json!(format!("independent character {index}"));
        let mut presets = fixture()["botPresets"].as_array().unwrap().clone();
        presets[index]["mainPrompt"] = json!(format!("independent preset {index}"));
        store
            .commit(&WorkingSetCommit {
                character_details: Some(vec![detail]),
                replace_presets: Some(presets),
                ..empty_working_set_commit(store.revision().unwrap())
            })
            .unwrap();
    }
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(settle(&mut second).phase, "idle");
    assert_eq!(settle(&mut first).phase, "idle");
    for store in [&first, &second] {
        let generation = active_generation(&store.connection).unwrap();
        for (index, id) in ["char-a", "char-b"].into_iter().enumerate() {
            let description: String = store.connection.query_row(
                "SELECT json_extract(detail,'$.description') FROM characters WHERE generation=?1 AND character_id=?2",
                params![generation, id], |r|r.get(0),
            ).unwrap();
            let prompt: String = store.connection.query_row(
                "SELECT json_extract(value,'$.mainPrompt') FROM bot_presets WHERE generation=?1 AND configured_index=?2",
                params![generation, index as i64], |r|r.get(0),
            ).unwrap();
            assert_eq!(description, format!("independent character {index}"));
            assert_eq!(prompt, format!("independent preset {index}"));
        }
    }
    let bytes = b"synthetic complete alias payload";
    let object = crate::asset_repository::PayloadCas::new(first.repository_root())
        .unwrap()
        .prepare_bytes(bytes)
        .unwrap();
    first
        .asset_object_catalog()
        .register(
            &[
                super::super::super::asset_object_catalog::AssetObjectRegistration {
                    object_hash: object.content_hash.clone(),
                    byte_size: object.byte_size,
                },
            ],
            1,
        )
        .unwrap();
    let alias = |name: &str| AssetAlias {
        key: "synthetic/alias-conflict".into(),
        object_hash: Some(object.content_hash.clone()),
        kind: "asset".into(),
        size: object.byte_size as i64,
        mime: "application/octet-stream".into(),
        name: name.into(),
        ext: "bin".into(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({"name":name}),
    };
    first
        .commit_asset_alias(&alias("base"), first.revision().unwrap())
        .unwrap();
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(settle(&mut second).phase, "idle");
    first
        .commit_asset_alias(&alias("first"), first.revision().unwrap())
        .unwrap();
    second
        .commit_asset_alias(&alias("second"), second.revision().unwrap())
        .unwrap();
    assert_eq!(settle(&mut first).phase, "idle");
    let conflict = settle(&mut second);
    assert_eq!(conflict.phase, "conflict");
    assert_eq!(
        second
            .read_asset_alias("asset", "synthetic/alias-conflict", None)
            .unwrap()
            .unwrap()
            .value
            .name,
        "second"
    );
    second
        .server_cycle(&CycleOptions {
            resolution: Some(super::super::super::server_sync_engine::Resolution::KeepRemote),
            expected_revision: Some(conflict.local_revision),
            expected_head: Some(conflict.head),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(settle(&mut second).phase, "idle");
    assert_eq!(
        second
            .read_asset_alias("asset", "synthetic/alias-conflict", None)
            .unwrap()
            .unwrap()
            .value
            .name,
        "first"
    );
    assert_eq!(
        crate::server_sync::backups::list(second.repository_root())
            .unwrap()
            .len(),
        1
    );
    for (store, key) in [(&mut first, "new-a"), (&mut second, "new-b")] {
        store
            .commit(&WorkingSetCommit {
                plugin_storage: Some(vec![PluginStorageMutation::Set {
                    key: key.into(),
                    value: json!(true),
                }]),
                ..empty_working_set_commit(store.revision().unwrap())
            })
            .unwrap();
    }
    assert_eq!(settle(&mut first).phase, "idle");
    let order = settle(&mut second);
    assert_eq!(order.phase, "conflict");
    assert!(order
        .conflicts
        .iter()
        .all(|key| key.starts_with("r1:plugin:")));
    assert!(second.server_status().unwrap().dirty_records > 0);
    task.abort();
    runtime.shutdown_timeout(Duration::from_secs(2));
}
