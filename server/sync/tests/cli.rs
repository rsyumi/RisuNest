use std::process::{Command, Output};

fn cli(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_risunest-sync-server"))
        .args(args)
        .arg("--data-dir")
        .arg(dir)
        .output()
        .unwrap()
}
#[test]
fn independent_binary_initializes_registers_reports_and_revokes() {
    let dir = tempfile::tempdir().unwrap();
    let initialized = cli(dir.path(), &["init"]);
    assert!(initialized.status.success());
    let head: serde_json::Value = serde_json::from_slice(&initialized.stdout).unwrap();
    let added = cli(dir.path(), &["device", "add"]);
    assert!(added.status.success());
    let credential: serde_json::Value = serde_json::from_slice(&added.stdout).unwrap();
    assert_eq!(credential["libraryId"], head["libraryId"]);
    assert_eq!(credential["token"].as_str().unwrap().len(), 64);
    let status = cli(dir.path(), &["status"]);
    assert!(status.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&status.stdout).unwrap(),
        head
    );
    let revoked = cli(
        dir.path(),
        &["device", "revoke", credential["deviceId"].as_str().unwrap()],
    );
    assert!(revoked.status.success());
    let repeated = cli(dir.path(), &["init"]);
    assert!(!repeated.status.success());
    assert_eq!(
        String::from_utf8(repeated.stderr).unwrap().trim(),
        "already-initialized"
    );
}
#[test]
fn binary_rejects_multicast_before_creating_storage() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("not-created");
    let output = cli(
        &target,
        &["init", "--listen", "224.0.0.1:14319", "--https-proxy"],
    );
    assert!(!output.status.success());
    assert!(!target.exists());
}

#[tokio::test]
async fn saved_wildcard_listener_is_used_and_management_remains_local() {
    use risunest_sync_server::management::discovery::Discovery;
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    let address = format!("0.0.0.0:{port}");
    assert!(
        cli(dir.path(), &["network", "configure", "--listen", &address])
            .status
            .success()
    );
    assert!(cli(dir.path(), &["init"]).status.success());
    drop(reserved);
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _child = Child(
        Command::new(env!("CARGO_BIN_EXE_risunest-sync-server"))
            .args(["serve", "--data-dir"])
            .arg(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let http = reqwest::Client::new();
    let status = loop {
        if let Ok(locator) = Discovery::load(dir.path()) {
            assert!(locator.address.ip().is_loopback());
            if let Ok(response) = http
                .get(format!("http://{}/status", locator.address))
                .bearer_auth(locator.token)
                .send()
                .await
            {
                if let Ok(status) = response.json::<serde_json::Value>().await {
                    break status;
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(status["listener"], address);
    assert!(http
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .is_ok());
    assert!(!cli(
        dir.path(),
        &["network", "configure", "--listen", "0.0.0.0:0"]
    )
    .status
    .success());
    let saved = cli(dir.path(), &["network", "status"]);
    let saved: serde_json::Value = serde_json::from_slice(&saved.stdout).unwrap();
    assert_eq!(saved["port"], port);
}

#[test]
fn configured_device_emits_parseable_uri_and_qr_without_status_secrets() {
    let dir = tempfile::tempdir().unwrap();
    assert!(cli(dir.path(), &["init"]).status.success());
    assert!(cli(
        dir.path(),
        &[
            "connection",
            "configure",
            "--endpoint",
            "https://sync.example",
            "--registry",
            "https://registry.example"
        ]
    )
    .status
    .success());
    let output = cli(dir.path(), &["device", "add", "--qr"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let registration =
        risunest_sync_connect::Registration::parse_uri(text.lines().next().unwrap()).unwrap();
    assert!(text.lines().count() > 10);
    let status = cli(dir.path(), &["connection", "status"]);
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(!status.contains(&registration.token));
    assert!(!status.contains(&registration.directory.unwrap().key));
}

#[test]
fn offline_managed_registration_requires_directory_before_issuing_a_device() {
    let dir = tempfile::tempdir().unwrap();
    assert!(cli(dir.path(), &["init"]).status.success());
    let executable = std::env::current_exe().unwrap();
    assert!(cli(
        dir.path(),
        &[
            "connection",
            "configure",
            "--cloudflared",
            executable.to_str().unwrap()
        ]
    )
    .status
    .success());
    let output = cli(dir.path(), &["device", "add"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        "managed-registration-needs-directory"
    );
}
