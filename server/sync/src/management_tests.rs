use crate::{connection::ConnectionOptions, store::Store};

fn configured_store() -> (tempfile::TempDir, Store) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    store
        .configure_connection(ConnectionOptions {
            endpoint: Some("https://sync.example.com".into()),
            cloudflared: None,
            registry_url: Some("https://registry.example.com".into()),
        })
        .unwrap();
    (root, store)
}

#[test]
fn named_issuance_is_not_replayed_and_status_has_no_credentials() {
    let (_root, store) = configured_store();
    let request = "a".repeat(64);
    let uri = store
        .issue_named_registration("테스트 기기", &request)
        .unwrap();
    let registration = risunest_sync_connect::Registration::parse_uri(&uri).unwrap();
    assert_eq!(
        store
            .issue_named_registration("테스트 기기", &request)
            .unwrap_err()
            .code,
        "registration-already-issued"
    );
    let devices = store.managed_devices().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "테스트 기기");
    let public = serde_json::to_string(&devices).unwrap();
    assert!(!public.contains(&registration.token));
    assert!(!public.contains("verifier"));
    store.revoke_device(&registration.device_id).unwrap();
    assert!(store
        .authenticate(&registration.library_id, &registration.token)
        .is_err());
    assert!(store.managed_devices().unwrap()[0].revoked);
}

#[test]
fn disabling_registry_preserves_identity_without_publication_or_uri_key() {
    let (_root, store) = configured_store();
    let first = store.plan_publication().unwrap().unwrap();
    store
        .configure_connection(ConnectionOptions {
            endpoint: Some("https://sync.example.com".into()),
            cloudflared: None,
            registry_url: None,
        })
        .unwrap();
    assert!(store.plan_publication().unwrap().is_none());
    let registration =
        risunest_sync_connect::Registration::parse_uri(&store.issue_registration().unwrap())
            .unwrap();
    assert!(registration.directory.is_none());
    assert_eq!(
        store.management_connection().unwrap().uuid.as_deref(),
        Some(first.directory.uuid.as_str())
    );
    store
        .configure_connection(ConnectionOptions {
            endpoint: Some("https://sync.example.com".into()),
            cloudflared: None,
            registry_url: Some("https://different-registry.example.com".into()),
        })
        .unwrap();
    let next = store.plan_publication().unwrap().unwrap();
    assert_eq!(first.directory.uuid, next.directory.uuid);
    assert_eq!(first.directory.key, next.directory.key);
}

#[test]
fn bad_registration_input_allocates_no_device() {
    let (_root, store) = configured_store();
    assert!(store
        .issue_named_registration("\u{1b}[31m", &"a".repeat(64))
        .is_err());
    assert!(store.issue_named_registration("", &"b".repeat(64)).is_err());
    assert!(store
        .issue_named_registration("기기", "bad request")
        .is_err());
    assert!(store.managed_devices().unwrap().is_empty());
}

#[tokio::test]
async fn live_revoke_cancels_reserved_work_without_removing_committed_data() {
    use risunest_sync_wire::{hash, ChangeSet, CommitIntent, RecordChange, RecordVersion};
    let root = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::init(root.path()).unwrap());
    let credential = store.add_device().unwrap();
    let device = store
        .authenticate(&credential.library_id, &credential.token)
        .unwrap();
    store
        .put_object(&device, &hash(b"synthetic"), b"synthetic")
        .unwrap();
    let intent = |key: &str, seq: u64| {
        let staged = store
            .stage_changes(
                &device,
                &ChangeSet {
                    changes: vec![RecordChange {
                        key: key.into(),
                        before: RecordVersion::Absent,
                        after: RecordVersion::Live {
                            object_hash: hash(b"synthetic"),
                            descriptor_hash: None,
                        },
                    }],
                    read_fences: vec![],
                    scope_fences: vec![],
                },
            )
            .unwrap();
        CommitIntent {
            device_operation_seq: seq.into(),
            expected_head: store.head().unwrap(),
            changes_digest: staged.changes_digest,
            staged_changes_id: staged.staged_changes_id,
        }
    };
    let committed = intent("committed", 1);
    let receipt = store
        .commit(&device, &committed, &committed.expected_head.etag())
        .unwrap();
    let head = store.head().unwrap();
    let pending = intent("pending", 2);
    store
        .submit_commit(&device, &pending, &pending.expected_head.etag())
        .unwrap();
    assert!(store.managed_devices().unwrap()[0].pending);
    let manager =
        crate::management::Management::start(store.clone(), "127.0.0.1:4320".parse().unwrap())
            .await
            .unwrap();
    let discovery = crate::management::discovery::Discovery::load(root.path()).unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://{}", discovery.address);
    let status: serde_json::Value = client
        .get(format!("{base}/status"))
        .bearer_auth(&discovery.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(client
        .post(format!("{base}/devices/{}/revoke", credential.device_id))
        .bearer_auth(&discovery.token)
        .json(&serde_json::json!({"revision":status["revision"]}))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(store
        .authenticate(&credential.library_id, &credential.token)
        .is_err());
    assert!(store.run_pending_commit().unwrap());
    assert!(!store.managed_devices().unwrap()[0].pending);
    assert_eq!(store.head().unwrap(), head);
    let db = rusqlite::Connection::open_with_flags(
        root.path().join("metadata.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let persisted: String = db
        .query_row(
            "SELECT body FROM receipts WHERE operation=?1",
            [&receipt.operation_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&persisted).unwrap(),
        serde_json::to_value(&receipt).unwrap()
    );
    manager.close().await;
}

#[tokio::test]
async fn management_http_auth_revision_and_live_issuance() {
    let root = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::init(root.path()).unwrap());
    store
        .configure_connection(ConnectionOptions {
            endpoint: Some("https://sync.example.com".into()),
            cloudflared: None,
            registry_url: None,
        })
        .unwrap();
    let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let manager = crate::management::Management::start(store.clone(), origin.local_addr().unwrap())
        .await
        .unwrap();
    let discovery = crate::management::discovery::Discovery::load(root.path()).unwrap();
    assert_eq!(discovery.address, manager.address());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://{}", manager.address());
    assert_eq!(
        client
            .get(format!("{url}/status"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(format!("{url}/status"))
            .bearer_auth(&discovery.token)
            .header("origin", "https://outside.example.com")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let before: serde_json::Value = client
        .get(format!("{url}/status"))
        .bearer_auth(&discovery.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let request_id = "c".repeat(64);
    let response = client.post(format!("{url}/devices")).bearer_auth(&discovery.token).json(&serde_json::json!({"revision":before["revision"],"requestId":request_id,"name":"HTTP 테스트"})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let issued: serde_json::Value = response.json().await.unwrap();
    let registration =
        risunest_sync_connect::Registration::parse_uri(issued["uri"].as_str().unwrap()).unwrap();
    assert!(store
        .authenticate(&registration.library_id, &registration.token)
        .is_ok());
    assert_eq!(
        client
            .get(format!("{url}/status"))
            .bearer_auth(&registration.token)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let stale = client
        .post(format!("{url}/devices/{}/revoke", registration.device_id))
        .bearer_auth(&discovery.token)
        .json(&serde_json::json!({"revision":before["revision"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    assert!(store
        .authenticate(&registration.library_id, &registration.token)
        .is_ok());
    let latest: serde_json::Value = client
        .get(format!("{url}/status"))
        .bearer_auth(&discovery.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let public = latest.to_string();
    assert!(!public.contains(&registration.token));
    assert!(!public.contains(&discovery.token));
    let removed = client
        .post(format!("{url}/devices/{}/revoke", registration.device_id))
        .bearer_auth(&discovery.token)
        .json(&serde_json::json!({"revision":latest["revision"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), 200);
    assert!(store
        .authenticate(&registration.library_id, &registration.token)
        .is_err());
    manager.close().await;
    assert!(!root.path().join("management-session").exists());
}

#[test]
fn storage_counts_files_without_reading_payloads() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("objects")).unwrap();
    std::fs::create_dir(root.path().join("staging")).unwrap();
    for (name, size) in [
        ("objects/test", 11),
        ("staging/part", 13),
        ("metadata.sqlite", 17),
        ("metadata.sqlite-wal", 19),
        ("other", 23),
    ] {
        std::fs::File::create(root.path().join(name))
            .unwrap()
            .set_len(size)
            .unwrap();
    }
    let usage = crate::management::storage::measure(root.path()).unwrap();
    assert_eq!(usage.total_bytes, Some(83));
    assert_eq!(usage.data_bytes, 11);
    assert_eq!(usage.database_bytes, 36);
    assert_eq!(usage.temporary_bytes, 13);
    assert_eq!(usage.other_bytes, 23);
    assert!(usage.available_bytes.is_some());
}
