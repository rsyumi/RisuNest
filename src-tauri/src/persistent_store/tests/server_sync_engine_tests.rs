use super::super::server_sync_engine::{CycleOptions, CycleResult};
use super::*;
use crate::server_sync::client::ServerConfig;
use risunest_sync_server::{http, store::Store};
use risunest_sync_wire::{ChangeSet, ScopeFence};
use std::fs::OpenOptions;
#[path = "server_sync_initial_tests.rs"]
mod initial;
#[path = "server_sync_matrix_tests.rs"]
mod matrix;
mod residency;
#[path = "server_sync_retained_fixture_tests.rs"]
mod retained_fixture;
#[path = "server_sync_semantic_tests.rs"]
mod semantic;

fn prepared() -> (tempfile::TempDir, PersistentStore) {
    let (directory, mut store, database) = open_fixture();
    let characters = database["characters"].as_array().unwrap();
    let heads = characters
        .iter()
        .map(|c| {
            AssetOwnerHead::absent(AssetOwnerLocator::CharacterAdditionalAssets {
                character_id: c["chaId"].as_str().unwrap().into(),
            })
        })
        .collect();
    let details = characters
        .iter()
        .map(|c| {
            let mut detail = c.clone();
            detail.as_object_mut().unwrap().shift_remove("chats");
            detail
        })
        .collect();
    let mut root = store.read_root(None).unwrap().value;
    for (key,value) in json!({"botPresetsId":0,"personas":[{"id":"synthetic-persona"}],"selectedPersona":0,"enabledModules":[],"characterOrder":[],"modules":[],"loadouts":[],"plugins":[]}).as_object().unwrap(){root[key]=value.clone();}
    store
        .commit(&WorkingSetCommit {
            root: Some(root),
            character_details: Some(details),
            asset_owner_heads: Some(heads),
            ..empty_working_set_commit(1)
        })
        .unwrap();
    // This synthetic profile has a complete (empty) alias inventory. Its
    // deliberately missing images are proven missing, as on an initialized app.
    let generation = active_generation(&store.connection).unwrap();
    let assets = serde_json::to_string(&AssetRepositoryAuthorityState::V2 {
        migration_id: "synthetic".into(),
        compatibility_hash: "a".repeat(64),
    })
    .unwrap();
    let cold = serde_json::to_string(&ColdPayloadAuthorityState::V2 {
        migration_id: "synthetic".into(),
        compatibility_hash: "b".repeat(64),
    })
    .unwrap();
    store
        .connection
        .execute(
            "UPDATE asset_repository_authority SET value=?2 WHERE generation=?1",
            params![generation, assets],
        )
        .unwrap();
    store.connection.execute("INSERT INTO cold_payload_authority(generation,value) VALUES(?1,?2) ON CONFLICT(generation) DO UPDATE SET value=excluded.value",params![generation,cold]).unwrap();
    (directory, store)
}
fn settle(store: &mut PersistentStore) -> CycleResult {
    for _ in 0..8 {
        let result = store.server_cycle(&CycleOptions::default()).unwrap();
        if result.phase != "pending" {
            return result;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    panic!("Server operation did not settle")
}

#[test]
fn expired_server_operation_history_reproposes_without_losing_dirty_records() {
    let server_dir = tempfile::tempdir().unwrap();
    let server = Arc::new(Store::init(server_dir.path()).unwrap());
    let credential = server.add_device().unwrap();
    let server_device = server
        .authenticate(&credential.library_id, &credential.token)
        .unwrap();
    let staged = server
        .stage_changes(
            &server_device,
            &ChangeSet {
                changes: vec![],
                read_fences: vec![],
                scope_fences: vec![ScopeFence {
                    scope: "synthetic-expired-history".into(),
                    expected_version: server.scope_version("synthetic-expired-history").unwrap(),
                    clear: true,
                }],
            },
        )
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
    let server_clone = server.clone();
    let task = runtime.spawn(async move {
        axum::serve(listener, http::router(server_clone))
            .await
            .unwrap();
    });
    let (_local_dir, mut local) = prepared();
    local
        .server_bind(&ServerConfig {
            directory: None,
            endpoint,
            library_id: credential.library_id.clone(),
            device_id: credential.device_id.clone(),
            token: credential.token,
        })
        .unwrap();
    local
        .server_reserve(
            &server.head().unwrap(),
            staged.changes_digest,
            staged.staged_changes_id,
            local.revision().unwrap(),
        )
        .unwrap();
    let dirty = local.server_status().unwrap().dirty_records;
    let db = rusqlite::Connection::open(server_dir.path().join("metadata.sqlite")).unwrap();
    db.execute(
        "UPDATE devices SET watermark='1' WHERE id=?1",
        [&credential.device_id],
    )
    .unwrap();

    local
        .server_prepare_cycle(&CycleOptions::default())
        .unwrap();

    assert!(local.server_pending().unwrap().is_none());
    assert_eq!(local.server_status().unwrap().dirty_records, dirty);
    assert!(!local.server_status().unwrap().registration_required);
    let replacement = local
        .server_reserve(
            &server.head().unwrap(),
            "b".repeat(64),
            "replacement-stage".into(),
            local.revision().unwrap(),
        )
        .unwrap();
    assert_eq!(
        replacement.device_operation_seq,
        risunest_sync_wire::Sequence::from(2)
    );
    local.server_abandon_expired_operation().unwrap();
    task.abort();
    runtime.shutdown_timeout(Duration::from_secs(2));
}

#[test]
fn two_native_replicas_seed_publish_pull_and_preserve_same_key_conflicts() {
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
    let server_clone = server.clone();
    let task = runtime.spawn(async move {
        axum::serve(listener, http::router(server_clone))
            .await
            .unwrap();
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
    let first_result = settle(&mut first);
    assert_eq!(
        first_result.phase, "idle",
        "first replica: {:?}",
        first_result.conflicts
    );
    let second_result = settle(&mut second);
    assert_eq!(
        second_result.phase, "idle",
        "second replica: {:?}",
        second_result.conflicts
    );
    assert_eq!(first.server_status().unwrap().dirty_records, 0);
    assert!(!first.server_status().unwrap().full_scan);
    let revision = first.revision().unwrap();
    first
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "synthetic-shared".into(),
                value: json!({"text":"payload","unicode":"가🦀"}),
            }]),
            ..empty_working_set_commit(revision)
        })
        .unwrap();
    assert_eq!(settle(&mut first).phase, "idle");
    let items = Arc::new(super::super::server_sync_engine::CycleItemCounter::default());
    let super::super::server_sync_engine::Preparation::Ready(mut ready) = second
        .server_prepare_cycle(&CycleOptions {
            cycle_items: Some(items.clone()),
            ..Default::default()
        })
        .unwrap()
    else {
        panic!("expected prepared remote update")
    };
    // The pulled record is counted while it is applied, so the UI can show n/N.
    assert_eq!(items.total.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(items.done.load(std::sync::atomic::Ordering::Relaxed), 1);
    let previous_revision = second.revision().unwrap();
    second.connection.execute_batch("CREATE TRIGGER synthetic_server_activation_failure BEFORE UPDATE ON server_sync_state BEGIN SELECT RAISE(ABORT,'synthetic'); END;").unwrap();
    assert!(second.server_activate_cycle(&mut ready).is_err());
    assert_eq!(second.revision().unwrap(), previous_revision);
    second
        .connection
        .execute_batch("DROP TRIGGER synthetic_server_activation_failure;")
        .unwrap();
    let activated = second.server_activate_cycle(&mut ready).unwrap();
    assert_eq!(second.server_activate_cycle(&mut ready).unwrap(), activated);
    assert_eq!(second.revision().unwrap(), activated);
    assert_eq!(second.server_publish_cycle(&ready).unwrap().phase, "idle");
    let generation = active_generation(&second.connection).unwrap();
    let value:String=second.connection.query_row("SELECT value FROM plugin_storage WHERE generation=?1 AND storage_key='synthetic-shared'",[generation],|r|r.get(0)).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&value).unwrap(),
        json!({"text":"payload","unicode":"가🦀"})
    );
    for (store, value) in [(&mut first, "first edit"), (&mut second, "second edit")] {
        let revision = store.revision().unwrap();
        store
            .commit(&WorkingSetCommit {
                plugin_storage: Some(vec![PluginStorageMutation::Set {
                    key: "synthetic-shared".into(),
                    value: json!(value),
                }]),
                ..empty_working_set_commit(revision)
            })
            .unwrap();
    }
    assert_eq!(settle(&mut first).phase, "idle");
    let revision = second.revision().unwrap();
    let before = second.server_status().unwrap().head;
    let result = settle(&mut second);
    assert_eq!(result.phase, "conflict");
    assert_eq!(result.conflict_count, 1);
    assert_eq!(second.revision().unwrap(), revision);
    assert_eq!(second.server_status().unwrap().head, before);
    assert!(second.server_status().unwrap().dirty_records > 0);
    // A discarded metadata lease must rebuild staging without acknowledging or
    // replacing the unresolved local edit above.
    let config = second.server_config().unwrap().unwrap();
    let device = server
        .authenticate(&config.library_id, &config.token)
        .unwrap();
    let client = crate::server_sync::client::ServerClient::new(config).unwrap();
    let observed = server.head().unwrap();
    for collected in [false, true] {
        let checkpoint = server.create_checkpoint(&device).unwrap();
        let db = Connection::open(directory.path().join("metadata.sqlite")).unwrap();
        if collected {
            db.execute(
                "DELETE FROM checkpoints WHERE id=?1",
                [&checkpoint.checkpoint_id],
            )
            .unwrap();
        } else {
            db.execute(
                "UPDATE checkpoints SET expires=0 WHERE id=?1",
                [&checkpoint.checkpoint_id],
            )
            .unwrap();
        }
        let cursor = json!({"kind":"checkpoint","id":checkpoint.checkpoint_id,"after":null});
        second
            .connection
            .execute(
                "UPDATE server_sync_remote_cursor SET head=?1,cursor=?2,complete=0",
                params![
                    String::from_utf8(risunest_sync_wire::canonical::encode(&observed).unwrap())
                        .unwrap(),
                    String::from_utf8(risunest_sync_wire::canonical::encode(&cursor).unwrap())
                        .unwrap()
                ],
            )
            .unwrap();
        let dirty = second.server_status().unwrap().dirty_records;
        assert_eq!(
            crate::server_sync::remote::refresh(&mut second.connection, &client, &observed)
                .unwrap(),
            observed
        );
        assert_eq!(second.revision().unwrap(), revision);
        assert_eq!(second.server_status().unwrap().head, before);
        assert_eq!(second.server_status().unwrap().dirty_records, dirty);
    }
    let preview = settle(&mut second);
    let super::super::server_sync_engine::Preparation::Ready(mut choice) = second
        .server_prepare_cycle(&CycleOptions {
            resolution: Some(super::super::server_sync_engine::Resolution::KeepLocal),
            expected_revision: Some(preview.local_revision),
            expected_head: Some(preview.head),
            ..Default::default()
        })
        .unwrap()
    else {
        panic!("expected prepared conflict resolution")
    };
    second.server_activate_cycle(&mut choice).unwrap();
    drop(choice);
    // Simulate losing the prepared worker after local activation, before its
    // first server request. Durable dirty/base state must retain the choice.
    let resolved = second.server_cycle(&CycleOptions::default()).unwrap();
    assert_eq!(resolved.phase, "pending");
    assert_eq!(settle(&mut second).phase, "idle");
    assert_eq!(settle(&mut first).phase, "idle");
    let generation = active_generation(&first.connection).unwrap();
    let value:String=first.connection.query_row("SELECT value FROM plugin_storage WHERE generation=?1 AND storage_key='synthetic-shared'",[generation],|r|r.get(0)).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&value).unwrap(),
        json!("second edit")
    );
    let backups = fs::read_dir(second.repository_root.join("server-sync/backups"))
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(backups.len(), 1);
    for name in ["local.risunest", "remote.risunest", "complete.json"] {
        assert!(backups[0].path().join(name).is_file());
    }
    for name in ["local.risunest", "remote.risunest"] {
        let archive = crate::portable_backup::VerifiedArchive::open(
            std::fs::File::open(backups[0].path().join(name)).unwrap(),
            &backups[0].path(),
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();
        assert!(archive.manifest.library_included);
        assert!(!archive.manifest.device_included);
        assert!(!archive.manifest.repair_required);
        archive
            .validate_library(&crate::local_backup::NeverCancelled)
            .unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let mut restored = PersistentStore::open(scratch.path()).unwrap();
        let stage = restored
            .stage_portable_records(&archive.db, &crate::local_backup::NeverCancelled)
            .unwrap();
        let prepared = restored
            .prepare_replace_commit(&stage.staging_id, Some(0))
            .unwrap();
        let snapshot = prepared;
        restored.finish_prepared_replace(snapshot).unwrap();
        assert_eq!(restored.revision().unwrap(), 1);
    }
    let listed = crate::server_sync::backups::list(&second.repository_root).unwrap();
    assert_eq!(listed.len(), 1);
    let source = crate::server_sync::backups::source(
        &second.repository_root,
        &listed[0].id,
        crate::server_sync::backups::Side::Remote,
        || false,
    )
    .unwrap();
    assert!(Path::new(&source).is_file());
    assert!(crate::server_sync::backups::source(
        &second.repository_root,
        "../outside",
        crate::server_sync::backups::Side::Local,
        || false
    )
    .is_err());
    assert_eq!(
        crate::server_sync::backups::source(
            &second.repository_root,
            &listed[0].id,
            crate::server_sync::backups::Side::Local,
            || true
        )
        .unwrap_err()
        .code,
        "cancelled"
    );
    {
        use std::io::Write;
        OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap()
            .write_all(b"synthetic corruption")
            .unwrap();
    }
    assert_eq!(
        crate::server_sync::backups::source(
            &second.repository_root,
            &listed[0].id,
            crate::server_sync::backups::Side::Remote,
            || false
        )
        .unwrap_err()
        .code,
        "backup-hash-mismatch"
    );
    use super::super::server_sync_engine::Resolution;
    for (index, resolution) in [Resolution::KeepLocal, Resolution::KeepRemote]
        .into_iter()
        .enumerate()
    {
        let local_key = format!("after-clear-{index}");
        let remote_key = format!("remote-added-{index}");
        first
            .commit(&WorkingSetCommit {
                plugin_storage: Some(vec![
                    PluginStorageMutation::Clear,
                    PluginStorageMutation::Set {
                        key: local_key.clone(),
                        value: json!("local new"),
                    },
                ]),
                ..empty_working_set_commit(first.revision().unwrap())
            })
            .unwrap();
        second
            .commit(&WorkingSetCommit {
                plugin_storage: Some(vec![PluginStorageMutation::Set {
                    key: remote_key.clone(),
                    value: json!("remote new"),
                }]),
                ..empty_working_set_commit(second.revision().unwrap())
            })
            .unwrap();
        assert_eq!(settle(&mut second).phase, "idle");
        let preview = settle(&mut first);
        assert_eq!(preview.phase, "conflict");
        first
            .server_cycle(&CycleOptions {
                resolution: Some(resolution),
                expected_revision: Some(preview.local_revision),
                expected_head: Some(preview.head),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(settle(&mut first).phase, "idle");
        assert_eq!(settle(&mut second).phase, "idle");
        assert_eq!(
            first
                .connection
                .query_row::<i64, _, _>("SELECT count(*) FROM server_sync_clears", [], |r| r.get(0))
                .unwrap(),
            0
        );
        for store in [&first, &second] {
            let generation = active_generation(&store.connection).unwrap();
            let has = |key: &str| {
                store.connection.query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM plugin_storage WHERE generation=?1 AND storage_key=?2)",params![generation,key],|r|r.get(0)).unwrap()
            };
            assert_eq!(has(&local_key), matches!(resolution, Resolution::KeepLocal));
            assert_eq!(
                has(&remote_key),
                matches!(resolution, Resolution::KeepRemote)
            );
        }
    }
    // An offline new key must also conflict when the other side cleared first,
    // even though that key was absent from the original clear membership.
    first
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "offline-after-remote-clear".into(),
                value: json!("pending"),
            }]),
            ..empty_working_set_commit(first.revision().unwrap())
        })
        .unwrap();
    second
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Clear]),
            ..empty_working_set_commit(second.revision().unwrap())
        })
        .unwrap();
    assert_eq!(settle(&mut second).phase, "idle");
    let preview = settle(&mut first);
    assert_eq!(preview.phase, "conflict");
    assert!(preview
        .conflicts
        .contains(&"plugin-storage:remote-clear".into()));
    first
        .server_cycle(&CycleOptions {
            resolution: Some(Resolution::KeepRemote),
            expected_revision: Some(preview.local_revision),
            expected_head: Some(preview.head),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(settle(&mut first).phase, "idle");
    let before = server.scope_state("plugin-storage").unwrap();
    first
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Clear]),
            ..empty_working_set_commit(first.revision().unwrap())
        })
        .unwrap();
    assert_eq!(settle(&mut first).phase, "idle");
    assert_ne!(server.scope_state("plugin-storage").unwrap(), before);
    assert_eq!(settle(&mut second).phase, "idle");
    for resolution in [Resolution::KeepLocal, Resolution::KeepRemote] {
        second
            .commit(&WorkingSetCommit {
                conversations: Some(vec![ConversationMutation::ReplaceRange {
                    character_id: "char-b".into(),
                    conversation_id: "conv-beta".into(),
                    start: 0,
                    delete_count: 0,
                    messages: vec![json!({"role":"user","data":"offline child edit"})],
                    conversation: None,
                    configured_index: None,
                }]),
                ..empty_working_set_commit(second.revision().unwrap())
            })
            .unwrap();
        first
            .commit(&WorkingSetCommit {
                delete_character_id: Some("char-b".into()),
                ..empty_working_set_commit(first.revision().unwrap())
            })
            .unwrap();
        assert_eq!(settle(&mut first).phase, "idle");
        let preview = settle(&mut second);
        assert_eq!(preview.phase, "conflict");
        second
            .server_cycle(&CycleOptions {
                resolution: Some(resolution),
                expected_revision: Some(preview.local_revision),
                expected_head: Some(preview.head),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(settle(&mut second).phase, "idle");
        assert_eq!(settle(&mut first).phase, "idle");
        for store in [&first, &second] {
            let generation = active_generation(&store.connection).unwrap();
            let exists:bool=store.connection.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE generation=?1 AND character_id='char-b' AND conversation_id='conv-beta')",[generation],|r|r.get(0)).unwrap();
            assert_eq!(exists, matches!(resolution, Resolution::KeepLocal));
        }
    }
    task.abort();
    runtime.shutdown_timeout(Duration::from_secs(2));
}

struct CountedSocket {
    socket: tokio::net::TcpStream,
    bytes: Arc<std::sync::atomic::AtomicU64>,
}
struct CountedListener {
    listener: tokio::net::TcpListener,
    bytes: Arc<std::sync::atomic::AtomicU64>,
}
impl axum::serve::Listener for CountedListener {
    type Io = CountedSocket;
    type Addr = std::net::SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (socket, address) = self.listener.accept().await.unwrap();
        socket.set_nodelay(true).unwrap();
        (
            CountedSocket {
                socket,
                bytes: self.bytes.clone(),
            },
            address,
        )
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}
impl tokio::io::AsyncRead for CountedSocket {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buffer.filled().len();
        let result = std::pin::Pin::new(&mut this.socket).poll_read(cx, buffer);
        this.bytes.fetch_add(
            (buffer.filled().len() - before) as u64,
            AtomicOrdering::Relaxed,
        );
        result
    }
}
impl tokio::io::AsyncWrite for CountedSocket {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = std::pin::Pin::new(&mut this.socket).poll_write(cx, buffer);
        if let std::task::Poll::Ready(Ok(n)) = &result {
            this.bytes.fetch_add(*n as u64, AtomicOrdering::Relaxed);
        }
        result
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().socket).poll_flush(cx)
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().socket).poll_shutdown(cx)
    }
}

#[test]
#[ignore = "Explicit end-to-end HTTP traffic acceptance gate"]
fn server_sync_message_append_total_http_bytes_gate() {
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
    let server_clone = server.clone();
    let task = runtime.spawn(async move {
        axum::serve(listener, http::router(server_clone))
            .await
            .unwrap();
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
    let mut measurements = Vec::new();
    for count in [1usize, 127, 128, 129, 1024, 10000] {
        let generation = active_generation(&first.connection).unwrap();
        let existing:i64=first.connection.query_row("SELECT count(*) FROM messages WHERE generation=?1 AND character_id='char-a' AND conversation_id='conv-long'",[generation],|r|r.get(0)).unwrap();
        let messages = (0..count)
            .map(
                |n| json!({"role":"char","data":"synthetic ".repeat(96),"chatId":format!("m-{n}")}),
            )
            .collect();
        let revision = first.revision().unwrap();
        first
            .commit(&WorkingSetCommit {
                conversations: Some(vec![ConversationMutation::ReplaceRange {
                    character_id: "char-a".into(),
                    conversation_id: "conv-long".into(),
                    start: 0,
                    delete_count: existing,
                    messages,
                    conversation: None,
                    configured_index: None,
                }]),
                ..empty_working_set_commit(revision)
            })
            .unwrap();
        assert_eq!(settle(&mut first).phase, "idle");
        assert_eq!(settle(&mut second).phase, "idle");
        let message =
            json!({"role":"user","data":"small synthetic message","chatId":"new-message"});
        let d = serde_json::to_vec(&message).unwrap().len() as u64 + 1;
        let revision = first.revision().unwrap();
        first
            .commit(&WorkingSetCommit {
                conversations: Some(vec![ConversationMutation::ReplaceRange {
                    character_id: "char-a".into(),
                    conversation_id: "conv-long".into(),
                    start: count as i64,
                    delete_count: 0,
                    messages: vec![message],
                    conversation: None,
                    configured_index: None,
                }]),
                ..empty_working_set_commit(revision)
            })
            .unwrap();
        let before = counter.load(AtomicOrdering::Relaxed);
        let started = std::time::Instant::now();
        assert_eq!(settle(&mut first).phase, "idle");
        let upload = counter.load(AtomicOrdering::Relaxed) - before;
        let upload_ms = started.elapsed().as_millis();
        let before = counter.load(AtomicOrdering::Relaxed);
        let started = std::time::Instant::now();
        assert_eq!(settle(&mut second).phase, "idle");
        let download = counter.load(AtomicOrdering::Relaxed) - before;
        measurements.push((
            count,
            d,
            upload,
            download,
            upload_ms,
            started.elapsed().as_millis(),
        ));
    }
    eprintln!("HTTP totals (messages, D, upload bidirectional bytes, download bidirectional bytes, upload ms, download ms): {measurements:?}");
    for (count, d, upload, download, _, _) in measurements {
        assert!(
            upload <= d + 16 * 1024,
            "{count} messages: upload {upload} > D+16KiB {}",
            d + 16 * 1024
        );
        assert!(
            download <= d + 16 * 1024,
            "{count} messages: download {download} > D+16KiB {}",
            d + 16 * 1024
        );
    }
    task.abort();
    runtime.shutdown_timeout(Duration::from_secs(2));
}

#[test]
fn server_sync_large_opaque_file_delta_http_gate() {
    large_opaque_file_http_gate(17 * 1024 * 1024);
}
#[test]
#[ignore = "Explicit 1 GiB file delta HTTP traffic acceptance gate"]
fn server_sync_gib_opaque_file_delta_http_gate() {
    large_opaque_file_http_gate(1024 * 1024 * 1024);
}
fn large_opaque_file_http_gate(size: u64) {
    use crate::server_sync::{cache::Cache, client::ServerClient, transfer::Transfer};
    use std::io::{Read, Seek, SeekFrom};
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
    let server_clone = server.clone();
    let task = runtime.spawn(async move {
        axum::serve(listener, http::router(server_clone))
            .await
            .unwrap();
    });
    let credential = server.add_device().unwrap();
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
    struct Synthetic {
        left: u64,
        state: u64,
    }
    impl Read for Synthetic {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let n = bytes.len().min(self.left as usize);
            for byte in &mut bytes[..n] {
                self.state ^= self.state << 13;
                self.state ^= self.state >> 7;
                self.state ^= self.state << 17;
                *byte = self.state as u8;
            }
            self.left -= n as u64;
            Ok(n)
        }
    }
    let base = first
        .cas
        .prepare_reader(&mut Synthetic {
            left: size,
            state: 97,
        })
        .unwrap();
    let mut file = first.cas.open_object(&base.content_hash).unwrap().unwrap();
    second
        .cas
        .prepare_reader_expected(&mut file, &base.content_hash, size)
        .unwrap();
    Transfer::new(&client, &first)
        .unwrap()
        .upload(std::slice::from_ref(&base.content_hash), &[])
        .expect("initial full base upload");
    let insertion = b"small opaque insertion";
    let offset = size / 2 + 173;
    file.seek(SeekFrom::Start(0)).unwrap();
    let suffix = first.cas.open_object(&base.content_hash).unwrap().unwrap();
    let mut suffix = suffix;
    suffix.seek(SeekFrom::Start(offset)).unwrap();
    let mut changed = file
        .take(offset)
        .chain(std::io::Cursor::new(insertion))
        .chain(suffix);
    let target = first.cas.prepare_reader(&mut changed).unwrap();
    counter.store(0, AtomicOrdering::Relaxed);
    let started = std::time::Instant::now();
    Transfer::new(&client, &first)
        .unwrap()
        .upload(
            std::slice::from_ref(&target.content_hash),
            std::slice::from_ref(&base.content_hash),
        )
        .expect("warm delta upload");
    let upload = counter.swap(0, AtomicOrdering::Relaxed);
    let upload_ms = started.elapsed().as_millis();
    let started = std::time::Instant::now();
    Transfer::new(&client, &second)
        .unwrap()
        .download(
            std::slice::from_ref(&target.content_hash),
            std::slice::from_ref(&base.content_hash),
        )
        .expect("warm delta download");
    let download = counter.load(AtomicOrdering::Relaxed);
    eprintln!("Opaque HTTP totals (file bytes, D, upload bidirectional bytes, download bidirectional bytes, upload ms, download ms): ({size}, {}, {upload}, {download}, {upload_ms}, {})",insertion.len(),started.elapsed().as_millis());
    assert!(
        upload <= insertion.len() as u64 + 16384,
        "upload total {upload}"
    );
    assert!(
        download <= insertion.len() as u64 + 16384,
        "download total {download}"
    );
    assert_eq!(
        second.cas.stat_object(&target.content_hash).unwrap(),
        Some(size + insertion.len() as u64)
    );
    let mut downloaded = second
        .cas
        .open_object(&target.content_hash)
        .unwrap()
        .unwrap();
    risunest_sync_wire::stream_delta::verify(
        &mut downloaded,
        &risunest_sync_wire::delta::Base {
            hash: target.content_hash,
            size: size + insertion.len() as u64,
        },
        &mut || Ok(()),
    )
    .unwrap();
    task.abort();
    runtime.shutdown_timeout(Duration::from_secs(2));
}

#[test]
#[ignore = "Explicit 100k owner manifest end-to-end HTTP acceptance gate"]
fn server_sync_hundred_thousand_owner_entries_http_gate() {
    owner_entries_http_gate(false);
}

#[test]
#[ignore = "Explicit 100k unique CAS assets end-to-end HTTP acceptance gate"]
fn server_sync_hundred_thousand_resolved_assets_http_gate() {
    owner_entries_http_gate(true);
}

fn owner_entries_http_gate(resolved: bool) {
    use crate::asset_repository::owner_manifest_codec::{
        encode_owner_manifest, OwnerManifestEntry,
    };
    use crate::asset_repository::PayloadCas;
    let mut directory = tempfile::tempdir().unwrap();
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
    let server_clone = server.clone();
    let task = runtime.spawn(async move {
        axum::serve(listener, http::router(server_clone))
            .await
            .unwrap();
    });
    let (mut first_dir, mut first) = prepared();
    let (mut second_dir, mut second) = prepared();
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

    if let Some(report) = std::env::var_os("RISUNEST_SYNTHETIC_OWNER_REPORT") {
        use std::io::Write;
        let mut report = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(report)
            .unwrap();
        let marker = "risunest-sync-server-synthetic-owner-v1";
        for root in [directory.path(), first_dir.path(), second_dir.path()] {
            fs::write(root.join(".risunest-synthetic-owner-fixture"), marker).unwrap();
        }
        report.write_all(&serde_json::to_vec(&json!({"marker":marker,"server":directory.path(),"first":first_dir.path(),"second":second_dir.path()})).unwrap()).unwrap();
        report.sync_all().unwrap();
        directory.disable_cleanup(true);
        first_dir.disable_cleanup(true);
        second_dir.disable_cleanup(true);
    }
    // Establish the shared base before adding the large owner manifest. Two
    // independent nonempty libraries must otherwise ask for a conflict choice.
    eprintln!("100k owner establishing base");
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(settle(&mut second).phase, "idle");
    let mut entries = (0..100_000)
        .map(|index| OwnerManifestEntry {
            tuple: [
                format!("owner-{index}"),
                format!("assets/synthetic-missing-{index}"),
                "png".into(),
            ],
            payload_hash: None,
        })
        .collect::<Vec<_>>();
    if resolved {
        use super::asset_object_catalog::AssetObjectRegistration;
        let cas = PayloadCas::new(&first.repository_root).unwrap();
        for (page, entries) in entries.chunks_mut(4096).enumerate() {
            let mut registered = Vec::new();
            for (offset, entry) in entries.iter_mut().enumerate() {
                let index = page * 4096 + offset;
                let bytes = format!("synthetic unique asset body {index:016}");
                entry.payload_hash = Some(sha2::Sha256::digest(bytes.as_bytes()).into());
                let object = cas.prepare_bytes(bytes.as_bytes()).unwrap();
                registered.push(AssetObjectRegistration {
                    object_hash: object.content_hash,
                    byte_size: bytes.len() as u64,
                });
            }
            if !registered.is_empty() {
                first
                    .asset_object_catalog()
                    .register(&registered, 1)
                    .unwrap();
            }
        }
        eprintln!("100k unique synthetic CAS objects prepared");
    }
    let commit_owner = |store: &mut PersistentStore, entries: &[OwnerManifestEntry]| {
        let generation = active_generation(&store.connection).unwrap();
        let raw: String = store
            .connection
            .query_row(
                "SELECT detail FROM characters WHERE generation=?1 AND character_id='char-a'",
                [generation],
                |r| r.get(0),
            )
            .unwrap();
        let mut detail: Value = serde_json::from_str(&raw).unwrap();
        detail["additionalAssets"] = Value::Array(
            entries
                .iter()
                .map(|entry| Value::Array(entry.tuple.iter().cloned().map(Value::String).collect()))
                .collect(),
        );
        let cas = PayloadCas::new(&store.repository_root).unwrap();
        let manifest = cas
            .prepare_bytes(&encode_owner_manifest(entries).unwrap())
            .unwrap();
        store
            .commit(&WorkingSetCommit {
                character_details: Some(vec![detail]),
                asset_owner_heads: Some(vec![AssetOwnerHead::present(
                    AssetOwnerLocator::CharacterAdditionalAssets {
                        character_id: "char-a".into(),
                    },
                    manifest.content_hash,
                    entries.len() as i64,
                )]),
                ..empty_working_set_commit(store.revision().unwrap())
            })
            .unwrap();
    };
    commit_owner(&mut first, &entries);
    assert_eq!(settle(&mut first).phase, "idle");
    eprintln!("100k owner initial upload complete (resolved={resolved})");
    assert_eq!(settle(&mut second).phase, "idle");
    eprintln!("100k owner initial download complete (resolved={resolved})");
    entries[50_000].tuple[0] = "edited".into();
    commit_owner(&mut first, &entries);
    counter.store(0, AtomicOrdering::Relaxed);
    let start = std::time::Instant::now();
    eprintln!("100k owner warm upload starting");
    assert_eq!(settle(&mut first).phase, "idle");
    let upload = counter.swap(0, AtomicOrdering::Relaxed);
    let upload_ms = start.elapsed().as_millis();
    let start = std::time::Instant::now();
    eprintln!("100k owner warm download starting");
    assert_eq!(settle(&mut second).phase, "idle");
    let download = counter.load(AtomicOrdering::Relaxed);
    eprintln!("100k owner HTTP totals (resolved={resolved}; D, upload bytes, download bytes, upload ms, download ms): (6, {upload}, {download}, {upload_ms}, {})",start.elapsed().as_millis());
    let generation = active_generation(&second.connection).unwrap();
    let name:String=second.connection.query_row("SELECT json_extract(detail,'$.additionalAssets[50000][0]') FROM characters WHERE generation=?1 AND character_id='char-a'",[generation],|r|r.get(0)).unwrap();
    assert_eq!(name, "edited");
    if resolved {
        let cas = PayloadCas::new(&second.repository_root).unwrap();
        for index in 0..entries.len() {
            let expected = format!("synthetic unique asset body {index:016}");
            assert_eq!(
                cas.read_object(&risunest_sync_wire::hash(expected.as_bytes()))
                    .unwrap()
                    .unwrap(),
                expected.as_bytes()
            );
        }
    }
    // Extreme 100k-owner fixture: 16 KiB target, 32 KiB regression ceiling
    // for large owner metadata; ordinary record budgets remain unchanged.
    assert!(upload <= 32_768 + 6, "owner upload {upload}");
    assert!(download <= 32_768 + 6, "owner download {download}");
    task.abort();
    runtime.shutdown_timeout(Duration::from_secs(2));
}
