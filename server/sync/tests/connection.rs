use risunest_sync_server::{connection::ConnectionOptions, store::Store};

fn options(endpoint: &str) -> ConnectionOptions {
    ConnectionOptions {
        endpoint: Some(endpoint.into()),
        cloudflared: None,
        registry_url: Some("https://registry.example".into()),
    }
}

#[test]
fn publication_survives_restart_and_does_not_repeat_confirmed_address() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    store
        .configure_connection(options("https://sync.example/first"))
        .unwrap();
    let first = store.plan_publication().unwrap().unwrap();
    let envelope = first.envelope.clone();
    let uuid = first.directory.uuid.clone();
    drop(store);
    let store = Store::open(root.path()).unwrap();
    let retry = store.plan_publication().unwrap().unwrap();
    assert_eq!(retry.envelope, envelope);
    assert_eq!(retry.directory.uuid, uuid);
    store.confirm_publication(&retry).unwrap();
    assert!(store.plan_publication().unwrap().is_none());
    drop(store);
    let store = Store::open(root.path()).unwrap();
    assert!(store.plan_publication().unwrap().is_none());
    store
        .configure_connection(options("https://sync.example/second"))
        .unwrap();
    let changed = store.plan_publication().unwrap().unwrap();
    assert_eq!(changed.directory.uuid, uuid);
    assert_ne!(changed.envelope, envelope);
    assert!(store.confirm_publication(&first).is_err());
    assert_eq!(
        store.plan_publication().unwrap().unwrap().envelope,
        changed.envelope
    );
}

#[test]
fn issuance_uses_common_uri_and_public_status_contains_no_secrets() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    store
        .configure_connection(options("https://sync.example"))
        .unwrap();
    let uri = store.issue_registration().unwrap();
    let registration = risunest_sync_connect::Registration::parse_uri(&uri).unwrap();
    assert_eq!(registration.library_id, store.head().unwrap().library_id);
    assert!(store
        .authenticate(&registration.library_id, &registration.token)
        .is_ok());
    let status = serde_json::to_string(&store.connection_status().unwrap()).unwrap();
    assert!(!status.contains(&registration.token));
    assert!(!status.contains(&registration.directory.as_ref().unwrap().key));
    assert!(!status.contains(&registration.directory.as_ref().unwrap().uuid));
    let file = std::fs::read(root.path().join("connection-state")).unwrap();
    assert!(!file
        .windows(registration.token.len())
        .any(|v| v == registration.token.as_bytes()));
}

#[test]
fn invalid_private_state_is_rejected_without_reinitializing_identity() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    store
        .configure_connection(options("https://sync.example"))
        .unwrap();
    let head = store.head().unwrap();
    std::fs::write(root.path().join("connection-state"), vec![0u8; 32769]).unwrap();
    assert!(store.connection_status().is_err());
    assert!(store
        .configure_connection(options("https://different.example"))
        .is_err());
    assert_eq!(store.head().unwrap(), head);
}
#[test]
fn explicit_repost_reuses_identity_and_allocates_a_fresh_envelope() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    store
        .configure_connection(options("https://sync.example"))
        .unwrap();
    let first = store.plan_publication().unwrap().unwrap();
    store.confirm_publication(&first).unwrap();
    store.request_republication().unwrap();
    let next = store.plan_publication().unwrap().unwrap();
    assert_eq!(next.directory.uuid, first.directory.uuid);
    assert_eq!(next.directory.key, first.directory.key);
    assert_ne!(next.envelope, first.envelope);
}
