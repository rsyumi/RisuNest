#![cfg(windows)]

use risunest_sync_manager::{client::Client, platform};
use risunest_sync_server::config::NetworkSettings;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

struct Registration {
    root: PathBuf,
    executable: PathBuf,
}
impl Drop for Registration {
    fn drop(&mut self) {
        let _ = platform::startup(&self.root, &self.executable, "remove");
    }
}

async fn ready(client: &Client) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(value) = client.status().await {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "synthetic daemon did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn stop(client: &Client) {
    let status = client.status().await.unwrap();
    client
        .mutate(
            "shutdown",
            serde_json::json!({"revision":status["revision"]}),
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while client.status().await.is_ok() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "creates a temporary current-user scheduled task; requires RISUNEST_TEST_SERVER"]
async fn manual_start_is_independent_of_login_and_survives_registration_changes() {
    let executable = PathBuf::from(
        std::env::var_os("RISUNEST_TEST_SERVER").expect("explicit synthetic server required"),
    );
    assert!(executable.is_absolute() && executable.is_file());
    let root = tempfile::tempdir().unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    NetworkSettings {
        port,
        ..Default::default()
    }
    .save(root.path())
    .unwrap();
    drop(reserved);
    let _registration = Registration {
        root: root.path().to_owned(),
        executable: executable.clone(),
    };
    let client = Client::new(root.path().to_owned()).unwrap();
    platform::start(root.path(), &executable).unwrap();
    let first = ready(&client).await;
    assert_eq!(first["listener"], format!("127.0.0.1:{port}"));
    let state = platform::startup(root.path(), &executable, "status").unwrap();
    assert!(!state.registered && !state.enabled);
    assert!(
        platform::startup(root.path(), &executable, "install")
            .unwrap()
            .enabled
    );
    assert_eq!(
        client.status().await.unwrap()["revision"],
        first["revision"]
    );
    assert!(
        !platform::startup(root.path(), &executable, "remove")
            .unwrap()
            .enabled
    );
    assert_eq!(
        client.status().await.unwrap()["revision"],
        first["revision"]
    );
    stop(&client).await;
    wait_owner_release(root.path()).await;
    platform::start(root.path(), &executable).unwrap();
    let second = ready(&client).await;
    assert_ne!(first["revision"], second["revision"]);
    assert!(
        !platform::startup(root.path(), &executable, "status")
            .unwrap()
            .enabled
    );
    stop(&client).await;
    wait_owner_release(root.path()).await;
}

async fn wait_owner_release(root: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if risunest_sync_server::store::Store::open(root).is_ok() {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
