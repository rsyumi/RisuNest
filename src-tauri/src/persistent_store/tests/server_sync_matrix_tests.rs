use super::*;

#[test]
#[ignore = "Explicit 100k reference control-page transfer gate, excluding opaque bodies"]
fn server_sync_hundred_thousand_reference_pages_http_gate() {
    reference_pages_http_gate(false);
}

#[test]
#[ignore = "Explicit 100k reference parent split regression, excluding opaque bodies"]
fn server_sync_hundred_thousand_reference_parent_split_http_gate() {
    reference_pages_http_gate(true);
}

fn reference_pages_http_gate(split: bool) {
    use crate::server_sync::{cache::Cache, client::ServerClient, transfer::Transfer};
    use risunest_sync_wire::{descriptor::build_reference_tree, hash};
    let directory = tempfile::tempdir().unwrap();
    let server = Arc::new(Store::init(directory.path()).unwrap());
    let credential = server.add_device().unwrap();
    let device = server
        .authenticate(&credential.library_id, &credential.token)
        .unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let listener = CountedListener {
        listener,
        bytes: counter.clone(),
    };
    let server_clone = server.clone();
    let task = runtime.spawn(async move {
        axum::serve(listener, http::router(server_clone))
            .await
            .unwrap();
    });
    let client = ServerClient::new(ServerConfig {
        directory: None,
        endpoint,
        library_id: credential.library_id,
        device_id: credential.device_id,
        token: credential.token,
    })
    .unwrap();
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first = Cache::open(first_dir.path()).unwrap();
    let second = Cache::open(second_dir.path()).unwrap();
    let mut values: Vec<_> = (0..100_000u64)
        .map(|n| hash(format!("synthetic unique asset body {n:016}").as_bytes()))
        .collect();
    // Hashes of the three control/payload objects from the synthetic owner
    // rename. Replacing them splits a parent before a changed first leaf.
    let old_extra = [
        "10ec1e97fe46bf63832247e1cf0bf62f85b010977f087390bb62e0b8aa5f7d83",
        "15dcc14af31de88c886ce220573ec2b4940074973b849c31d1a3e624180913da",
        "c05ef04bb545b19957053f6b7cc250a5d9ee672fb9285e8803b9ffaed35e2512",
    ];
    if split {
        values.extend(old_extra.iter().map(|h| h.to_string()));
    }
    values.sort();
    let (base_root, base) = build_reference_tree(&values, false).unwrap();
    for (digest, bytes) in &base {
        first.put(bytes).unwrap();
        second.put(bytes).unwrap();
        server.put_object(&device, digest, bytes).unwrap();
    }
    if split {
        values.retain(|h| !old_extra.contains(&h.as_str()));
        values.extend(
            [
                "183175a0b890f822693220d23288f1af0fd4240653ad364af933cdb2d7816680",
                "4d7b7d341682f39790adc1227f9d7ca6cd14b45ef3b7eb7a92084b1fc5a3351b",
                "c8db11617b38c3ab5567178dcebb54592839396dfa199e53657d90d55a45b29b",
            ]
            .into_iter()
            .map(str::to_owned),
        );
    } else {
        values.remove(50_000);
        values.push(hash(b"synthetic changed owner segment"));
    }
    values.sort();
    let (changed_root, changed) = build_reference_tree(&values, false).unwrap();
    let base_hashes = base.iter().map(|(h, _)| h.clone()).collect::<Vec<_>>();
    let changed = changed
        .into_iter()
        .filter(|(h, _)| !base_hashes.contains(h))
        .collect::<Vec<_>>();
    for (_, bytes) in &changed {
        first.put(bytes).unwrap();
    }
    let targets = changed.iter().map(|(h, _)| h.clone()).collect::<Vec<_>>();
    counter.store(0, AtomicOrdering::Relaxed);
    let transfer = Transfer::new(&client, &first).unwrap();
    let hints = transfer
        .reference_tree_hints(vec![(
            changed_root.clone().unwrap(),
            vec![base_root.clone().unwrap()],
        )])
        .unwrap();
    transfer
        .upload_with_hints(&targets, &base_hashes, true, &hints)
        .unwrap();
    let upload = counter.swap(0, AtomicOrdering::Relaxed);
    Transfer::new(&client, &second)
        .unwrap()
        .download_reference_tree(vec![(changed_root.unwrap(), vec![base_root.unwrap()])])
        .unwrap();
    let download = counter.load(AtomicOrdering::Relaxed);
    for (digest, bytes) in changed {
        assert_eq!(
            second
                .read(&digest, risunest_sync_wire::MAX_METADATA_BYTES)
                .unwrap(),
            bytes
        );
    }
    eprintln!(
        "100k reference control pages (parent split={split}): upload={upload}, download={download}"
    );
    assert!(upload <= 16384 + 6, "reference upload {upload}");
    assert!(download <= 16384 + 6, "reference download {download}");
    task.abort();
    runtime.shutdown_timeout(Duration::from_secs(2));
}

fn messages(store: &PersistentStore) -> Vec<Value> {
    let generation = active_generation(&store.connection).unwrap();
    let mut query = store.connection.prepare("SELECT value FROM messages WHERE generation=?1 AND character_id='char-a' AND conversation_id='conv-long' ORDER BY message_index").unwrap();
    query
        .query_map([generation], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
        .collect()
}
fn replace(store: &mut PersistentStore, values: Vec<Value>) {
    let existing = messages(store).len() as i64;
    store
        .commit(&WorkingSetCommit {
            conversations: Some(vec![ConversationMutation::ReplaceRange {
                character_id: "char-a".into(),
                conversation_id: "conv-long".into(),
                start: 0,
                delete_count: existing,
                messages: values,
                conversation: None,
                configured_index: None,
            }]),
            ..empty_working_set_commit(store.revision().unwrap())
        })
        .unwrap();
}
fn row_json(store: &PersistentStore, sql: &str) -> Value {
    let generation = active_generation(&store.connection).unwrap();
    let text: String = store
        .connection
        .query_row(sql, [generation], |r| r.get(0))
        .unwrap();
    serde_json::from_str(&text).unwrap()
}
fn measure(
    first: &mut PersistentStore,
    second: &mut PersistentStore,
    counter: &std::sync::atomic::AtomicU64,
    label: &str,
    d: u64,
) -> bool {
    counter.store(0, AtomicOrdering::Relaxed);
    let start = std::time::Instant::now();
    assert_eq!(settle(first).phase, "idle", "{label} sender");
    let upload = counter.swap(0, AtomicOrdering::Relaxed);
    let upload_ms = start.elapsed().as_millis();
    let start = std::time::Instant::now();
    assert_eq!(settle(second).phase, "idle", "{label} receiver");
    let download = counter.load(AtomicOrdering::Relaxed);
    eprintln!("matrix HTTP ({label}, D={d}, upload={upload}, download={download}, upload_ms={upload_ms}, download_ms={})", start.elapsed().as_millis());
    assert_eq!(
        messages(first),
        messages(second),
        "{label} exact message sequence"
    );
    upload <= d + 16384 && download <= d + 16384
}

#[test]
#[ignore = "Explicit large-value and message sequence HTTP acceptance matrix"]
fn server_sync_large_value_and_sequence_http_matrix() {
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
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let listener = CountedListener {
        listener,
        bytes: counter.clone(),
    };
    let remote = server.clone();
    let task = runtime.spawn(async move {
        axum::serve(listener, http::router(remote)).await.unwrap();
    });
    let (_first_dir, mut first) = prepared();
    let (_second_dir, mut second) = prepared();
    for store in [&mut first, &mut second] {
        let credential = server.add_device().unwrap();
        store
            .server_bind(&ServerConfig {
                directory: None,
                endpoint: endpoint.clone(),
                library_id: credential.library_id,
                device_id: credential.device_id,
                token: credential.token,
            })
            .unwrap();
    }
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(settle(&mut second).phase, "idle");
    let mut text = "synthetic payload ".repeat(700_000);
    text.truncate(10 * 1024 * 1024);
    let set_plugin = |store: &mut PersistentStore, text: &str| {
        store
            .commit(&WorkingSetCommit {
                plugin_storage: Some(vec![PluginStorageMutation::Set {
                    owner: "synthetic-plugin".to_owned(),
                    key: "large-synthetic".into(),
                    value: Value::String(text.into()),
                }]),
                ..empty_working_set_commit(store.revision().unwrap())
            })
            .unwrap();
    };
    set_plugin(&mut first, &text);
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(settle(&mut second).phase, "idle");
    text.insert_str(5 * 1024 * 1024, "edited");
    set_plugin(&mut first, &text);
    let mut failures = Vec::new();
    if !measure(&mut first, &mut second, &counter, "plugin-10MiB-insert", 6) {
        failures.push("plugin-10MiB-insert");
    }
    let generation = active_generation(&second.connection).unwrap();
    let received: String = second.connection.query_row("SELECT value FROM plugin_storage WHERE generation=?1 AND storage_key='large-synthetic'", [generation], |r|r.get(0)).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&received).unwrap(),
        Value::String(text)
    );

    let initial = (0..1024).map(|index| json!({"role":"char","data":format!("{index}: {}", "synthetic ".repeat(96)), "chatId": if index % 3 == 0 {Value::Null} else {Value::String("duplicate-id".into())}})).collect::<Vec<_>>();
    replace(&mut first, initial);
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(settle(&mut second).phase, "idle");
    for label in [
        "prefix-insert",
        "middle-insert",
        "middle-delete",
        "prefix-delete",
        "message-edit",
        "message-reorder",
    ] {
        let mut values = messages(&first);
        let added = json!({"role":"user","data":"small synthetic insertion","chatId":null});
        let d = match label {
            "prefix-insert" => {
                values.insert(0, added.clone());
                serde_json::to_vec(&added).unwrap().len() as u64 + 1
            }
            "middle-insert" => {
                values.insert(512, added.clone());
                serde_json::to_vec(&added).unwrap().len() as u64 + 1
            }
            "middle-delete" => {
                values.remove(512);
                0
            }
            "prefix-delete" => {
                values.remove(0);
                0
            }
            "message-edit" => {
                values[512]["data"].as_str().unwrap();
                values[512]["data"] =
                    Value::String(format!("{}edited", values[512]["data"].as_str().unwrap()));
                6
            }
            _ => {
                values.swap(127, 513);
                0
            }
        };
        replace(&mut first, values);
        if !measure(&mut first, &mut second, &counter, label, d) {
            failures.push(label);
        }
    }
    // Exercise distinct semantic families through their production PDS mutation
    // paths, then compare exact values after both HTTP directions have settled.
    let mut root = first.read_root(None).unwrap().value;
    root["username"] = json!("edited");
    first
        .commit(&WorkingSetCommit {
            root: Some(root),
            ..empty_working_set_commit(first.revision().unwrap())
        })
        .unwrap();
    if !measure(&mut first, &mut second, &counter, "root-field", 6) {
        failures.push("root-field");
    }
    assert_eq!(
        second.read_root(None).unwrap().value["username"],
        json!("edited")
    );

    let generation = active_generation(&first.connection).unwrap();
    let mut presets = first
        .connection
        .prepare("SELECT value FROM bot_presets WHERE generation=?1 ORDER BY configured_index")
        .unwrap()
        .query_map([generation], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| serde_json::from_str::<Value>(&r.unwrap()).unwrap())
        .collect::<Vec<_>>();
    presets[0]["mainPrompt"] = json!("edited");
    first
        .commit(&WorkingSetCommit {
            replace_presets: Some(presets.clone()),
            ..empty_working_set_commit(first.revision().unwrap())
        })
        .unwrap();
    if !measure(&mut first, &mut second, &counter, "preset-field", 6) {
        failures.push("preset-field");
    }
    assert_eq!(
        row_json(
            &second,
            "SELECT value FROM bot_presets WHERE generation=?1 ORDER BY configured_index LIMIT 1"
        ),
        presets[0]
    );

    let character_sql =
        "SELECT detail FROM characters WHERE generation=?1 AND character_id='char-a'";
    let mut detail = row_json(&first, character_sql);
    detail["globalLore"] = json!([{"key":"synthetic", "content":"base lore", "enabled":true}]);
    first
        .commit(&WorkingSetCommit {
            character_details: Some(vec![detail.clone()]),
            ..empty_working_set_commit(first.revision().unwrap())
        })
        .unwrap();
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(settle(&mut second).phase, "idle");
    detail["globalLore"][0]["content"] = json!("edited");
    first
        .commit(&WorkingSetCommit {
            character_details: Some(vec![detail.clone()]),
            ..empty_working_set_commit(first.revision().unwrap())
        })
        .unwrap();
    if !measure(&mut first, &mut second, &counter, "lorebook-field", 6) {
        failures.push("lorebook-field");
    }
    assert_eq!(
        row_json(&second, character_sql)["globalLore"],
        detail["globalLore"]
    );

    for value in ["base", "edited"] {
        first
            .commit(&WorkingSetCommit {
                plugin_storage: Some(vec![PluginStorageMutation::Set {
                    owner: "synthetic-plugin".to_owned(),
                    key: "작은/키".into(),
                    value: json!(value),
                }]),
                ..empty_working_set_commit(first.revision().unwrap())
            })
            .unwrap();
        if value == "base" {
            assert_eq!(settle(&mut first).phase, "idle");
            assert_eq!(settle(&mut second).phase, "idle");
        } else if !measure(&mut first, &mut second, &counter, "plugin-small", 6) {
            failures.push("plugin-small");
        }
    }
    assert_eq!(
        row_json(
            &second,
            "SELECT value FROM plugin_storage WHERE generation=?1 AND storage_key='작은/키'"
        ),
        json!("edited")
    );

    let conversation = json!({"id":"conv-new", "name":"synthetic new", "note":"", "localLore":[]});
    let message = json!({"role":"user", "data":"new conversation message", "chatId":null});
    let d = (serde_json::to_vec(&conversation).unwrap().len()
        + serde_json::to_vec(&message).unwrap().len()) as u64;
    first
        .commit(&WorkingSetCommit {
            conversations: Some(vec![ConversationMutation::ReplaceRange {
                character_id: "char-a".into(),
                conversation_id: "conv-new".into(),
                start: 0,
                delete_count: 0,
                messages: vec![message.clone()],
                conversation: Some(conversation),
                configured_index: Some(2),
            }]),
            ..empty_working_set_commit(first.revision().unwrap())
        })
        .unwrap();
    if !measure(&mut first, &mut second, &counter, "new-conversation", d) {
        failures.push("new-conversation");
    }
    assert_eq!(row_json(&second, "SELECT value FROM messages WHERE generation=?1 AND character_id='char-a' AND conversation_id='conv-new'"), message);

    let bytes = b"synthetic asset body for metadata edits";
    let cas = crate::asset_repository::PayloadCas::new(&first.repository_root).unwrap();
    let object = cas.prepare_bytes(bytes).unwrap();
    first
        .asset_object_catalog()
        .register(
            &[
                super::super::asset_object_catalog::AssetObjectRegistration {
                    object_hash: object.content_hash.clone(),
                    byte_size: bytes.len() as u64,
                },
            ],
            1,
        )
        .unwrap();
    let mut alias = AssetAlias {
        key: "assets/synthetic-metadata".into(),
        object_hash: Some(object.content_hash),
        kind: "asset".into(),
        size: bytes.len() as i64,
        mime: "application/octet-stream".into(),
        name: "base".into(),
        ext: "bin".into(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({"name":"base", "nested":[null,false,7]}),
    };
    first
        .commit_asset_alias(&alias, first.revision().unwrap())
        .unwrap();
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(settle(&mut second).phase, "idle");
    alias.name = "edited".into();
    alias.metadata["name"] = json!("edited");
    first
        .commit_asset_alias(&alias, first.revision().unwrap())
        .unwrap();
    if !measure(&mut first, &mut second, &counter, "asset-metadata", 12) {
        failures.push("asset-metadata");
    }
    assert_eq!(
        second
            .read_asset_alias("asset", &alias.key, None)
            .unwrap()
            .unwrap()
            .value,
        alias
    );
    assert_eq!(
        crate::asset_repository::PayloadCas::new(&second.repository_root)
            .unwrap()
            .read_object(alias.object_hash.as_ref().unwrap())
            .unwrap()
            .unwrap(),
        bytes
    );
    task.abort();
    runtime.shutdown_timeout(Duration::from_secs(2));
    assert!(failures.is_empty(), "HTTP budget failures: {failures:?}");
}
