use risunest_sync_manager::client::Client;
use risunest_sync_server::{management::Management, store::Store};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn client_reconnects_without_replaying_issuance_and_rejects_stale_edits() {
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
