use risunest_sync_manager::client::Client;
use risunest_sync_manager::lifecycle;
use risunest_sync_server::{management::Management, store::Store};
use serde_json::json;
use std::{fs, sync::Arc};

static MANAGEMENT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn client_reconnects_without_replaying_issuance_and_rejects_stale_edits() {
    let _guard = MANAGEMENT_TEST_LOCK.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::init(temp.path()).unwrap());
    let manager = Management::start(store.clone(), "127.0.0.1:4320".parse().unwrap())
        .await
        .unwrap();
    let client = Client::new(temp.path().to_owned()).unwrap();
    let first = client.status().await.unwrap();
    client.mutate("connection", json!({"revision":first["revision"],"options":{"endpoint":"https://synthetic.example.com","cloudflared":null,"registryUrl":null}})).await.unwrap();
    let state = client.status().await.unwrap();
    let request = "a".repeat(64);
    let issued = client
        .mutate(
            "devices",
            json!({"revision":state["revision"],"name":"시험 기기","requestId":request}),
        )
        .await
        .unwrap();
    assert!(issued["uri"]
        .as_str()
        .unwrap()
        .starts_with("risunestlocal://"));
    let current = client.status().await.unwrap();
    assert_eq!(current["devices"].as_array().unwrap().len(), 1);
    assert!(!current
        .to_string()
        .contains(issued["uri"].as_str().unwrap()));
    assert_eq!(
        client
            .mutate(
                "devices",
                json!({"revision":current["revision"],"name":"시험 기기","requestId":request})
            )
            .await
            .unwrap_err(),
        "registration-already-issued"
    );
    assert_eq!(
        client.status().await.unwrap()["devices"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    manager.close().await;
    assert!(client.status().await.is_err());
    let restarted = Management::start(store, "127.0.0.1:4320".parse().unwrap())
        .await
        .unwrap();
    let refreshed = client.status().await.unwrap();
    assert_ne!(refreshed["revision"], current["revision"]);
    assert_eq!(client.mutate("devices",json!({"revision":current["revision"],"name":"다른 기기","requestId":"b".repeat(64)})).await.unwrap_err(),"management-stale-state");
    assert_eq!(
        client
            .mutate("../../shutdown", json!({}))
            .await
            .unwrap_err(),
        "invalid-management-action"
    );
    restarted.close().await;
}

#[tokio::test]
async fn stop_accepts_an_unchanged_locator_after_the_daemon_disappears() {
    let _guard = MANAGEMENT_TEST_LOCK.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::init(temp.path()).unwrap());
    let manager = Management::start(store, "127.0.0.1:4320".parse().unwrap())
        .await
        .unwrap();
    let path = temp.path().join("management-session");
    let bytes = fs::read(&path).unwrap();
    let permissions = fs::metadata(&path).unwrap().permissions();
    manager.close().await;
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(&path, permissions).unwrap();

    let client = Client::new(temp.path().to_owned()).unwrap();
    lifecycle::stop(temp.path(), &client).await.unwrap();

    assert!(path.exists());
}

#[tokio::test]
async fn stop_preserves_a_locator_that_cannot_be_authenticated() {
    let _guard = MANAGEMENT_TEST_LOCK.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("management-session");
    fs::write(&path, b"synthetic-invalid-locator").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let client = Client::new(temp.path().to_owned()).unwrap();

    assert!(lifecycle::stop(temp.path(), &client).await.is_err());
    assert!(path.exists());
}
