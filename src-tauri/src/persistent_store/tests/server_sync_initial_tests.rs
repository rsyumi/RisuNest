use super::*;

#[test]
fn empty_replicas_seed_and_receive_while_independent_libraries_require_comparison() {
    let server_dir = tempfile::tempdir().unwrap();
    let server = Arc::new(Store::init(server_dir.path()).unwrap());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let serving = server.clone();
    let task = runtime.spawn(async move {
        axum::serve(listener, http::router(serving)).await.unwrap();
    });
    let bind = |store: &mut PersistentStore| {
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
    };
    let empty_dir = tempfile::tempdir().unwrap();
    let mut empty = PersistentStore::open(empty_dir.path()).unwrap();
    bind(&mut empty);
    assert_eq!(settle(&mut empty).phase, "idle");
    assert_eq!(server.head().unwrap().seq, 0.into());
    let (_source_dir, mut source) = prepared();
    bind(&mut source);
    assert_eq!(settle(&mut source).phase, "idle");
    let seeded_head = server.head().unwrap();
    assert_ne!(seeded_head.seq, 0.into());
    assert_eq!(settle(&mut empty).phase, "idle");
    assert_eq!(
        empty.server_status().unwrap().head,
        Some(seeded_head.clone())
    );
    let generation = active_generation(&empty.connection).unwrap();
    assert_eq!(
        empty
            .connection
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM characters WHERE generation=?1",
                [&generation],
                |r| r.get(0)
            )
            .unwrap(),
        fixture()["characters"].as_array().unwrap().len() as i64
    );
    assert_eq!(
        empty.read_root(None).unwrap().value,
        source.read_root(None).unwrap().value
    );
    drop(empty);
    let mut empty = PersistentStore::open(empty_dir.path()).unwrap();
    assert_eq!(settle(&mut empty).phase, "idle");
    assert_eq!(server.head().unwrap(), seeded_head);

    let (_independent_dir, mut independent) = prepared();
    let mut root = independent.read_root(None).unwrap().value;
    root["username"] = json!("synthetic independent library");
    independent
        .commit(&WorkingSetCommit {
            root: Some(root.clone()),
            ..empty_working_set_commit(independent.revision().unwrap())
        })
        .unwrap();
    bind(&mut independent);
    let revision = independent.revision().unwrap();
    assert_eq!(settle(&mut independent).phase, "conflict");
    assert_eq!(independent.revision().unwrap(), revision);
    assert_eq!(independent.read_root(None).unwrap().value, root);
    assert!(independent.server_status().unwrap().head.is_none());
    assert_eq!(server.head().unwrap(), seeded_head);
    task.abort();
    runtime.shutdown_timeout(Duration::from_secs(2));
}
