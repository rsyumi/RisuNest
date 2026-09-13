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
fn binary_rejects_public_cleartext_before_creating_storage() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("not-created");
    let output = cli(
        &target,
        &["init", "--listen", "0.0.0.0:4319", "--https-proxy"],
    );
    assert!(!output.status.success());
    assert!(!target.exists());
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
