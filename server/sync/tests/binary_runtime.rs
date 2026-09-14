use reqwest::{Client, Method, RequestBuilder};
use risunest_sync_wire::{hash, transfer::UPLOAD_CHUNK_BYTES, RemoteHead};
use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeviceCredential {
    #[serde(rename = "deviceId")]
    _device_id: String,
    library_id: String,
    token: String,
}

struct Daemon {
    child: Child,
    endpoint: String,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Daemon {
    fn start(root: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_risunest-sync-server"))
            .args(["serve", "--data-dir"])
            .arg(root)
            .args(["--listen", "127.0.0.1:0"])
            .env("PATH", "")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut daemon = Self {
            child,
            endpoint: String::new(),
        };
        let stderr = daemon.child.stderr.take().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let _ = send.send(line);
            // Continue draining without collecting runtime output.
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
        let line = receive.recv_timeout(Duration::from_secs(15)).unwrap();
        let address = line
            .strip_prefix("sync listener ready: ")
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        let address: std::net::SocketAddr = address.parse().unwrap();
        assert!(address.ip().is_loopback());
        daemon.endpoint = format!("http://{address}");
        daemon
    }
    fn request(
        &self,
        http: &Client,
        credential: &DeviceCredential,
        method: Method,
        path: &str,
    ) -> RequestBuilder {
        http.request(method, format!("{}{path}", self.endpoint))
            .bearer_auth(&credential.token)
            .header("x-risu-library", &credential.library_id)
    }
}
fn cli(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_risunest-sync-server"))
        .args(args)
        .arg("--data-dir")
        .arg(root)
        .env("PATH", "")
        .output()
        .unwrap()
}

#[test]
fn standalone_cli_rejects_missing_storage_and_busy_port_without_replacing_state() {
    let directory = tempfile::tempdir().unwrap();
    let absent = directory.path().join("absent-disk");
    assert!(!cli(&absent, &["serve"]).status.success());
    assert!(
        !absent.exists(),
        "serve must not initialize a missing volume"
    );
    let file = directory.path().join("not-a-directory");
    std::fs::write(&file, b"synthetic unrelated file").unwrap();
    assert!(!cli(&file, &["init"]).status.success());
    assert_eq!(std::fs::read(&file).unwrap(), b"synthetic unrelated file");

    let initialized = cli(directory.path(), &["init"]);
    assert!(initialized.status.success());
    assert!(!cli(directory.path(), &["init"]).status.success());
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = held.local_addr().unwrap().to_string();
    assert!(!cli(directory.path(), &["serve", "--listen", &address])
        .status
        .success());
    let status = cli(directory.path(), &["status"]);
    assert!(
        status.status.success(),
        "failed serve must release the owner lock"
    );
    assert_eq!(status.stdout, initialized.stdout);
}

#[cfg(unix)]
#[tokio::test]
async fn standalone_daemon_sigterm_releases_the_owner_and_reopens_exact_head() {
    let directory = tempfile::tempdir().unwrap();
    let initialized = cli(directory.path(), &["init"]);
    assert!(initialized.status.success());
    let mut daemon = Daemon::start(directory.path());
    // Only the child created by this test is signalled. No process enumeration.
    let status = Command::new("/bin/kill")
        .args(["-TERM", &daemon.child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            assert!(status.success(), "SIGTERM daemon exit was {status:?}");
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let reopened = cli(directory.path(), &["status"]);
    assert!(reopened.status.success());
    assert_eq!(reopened.stdout, initialized.stdout);
}

#[tokio::test]
async fn thousand_small_objects_use_one_verified_frame_batch() {
    let directory = tempfile::tempdir().unwrap();
    assert!(cli(directory.path(), &["init"]).status.success());
    let added = cli(directory.path(), &["device", "add"]);
    assert!(added.status.success());
    let credential: DeviceCredential = serde_json::from_slice(&added.stdout).unwrap();
    let daemon = Daemon::start(directory.path());
    let http = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();
    let objects = (0..1000)
        .map(|i| format!("synthetic unique asset body {i:016}").into_bytes())
        .collect::<Vec<_>>();
    let frames = objects
        .iter()
        .cloned()
        .map(risunest_sync_wire::transfer::Frame::Full)
        .collect::<Vec<_>>();
    let body = risunest_sync_wire::transfer::encode(&frames).unwrap();
    let body_bytes = body.len();
    let started = std::time::Instant::now();
    let response = daemon
        .request(&http, &credential, Method::POST, "/uploads/frames")
        .body(body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    assert!(response.bytes().await.unwrap().is_empty());
    let hashes = objects.iter().map(|b| hash(b)).collect::<Vec<_>>();
    eprintln!(
        "1000 small-object frame batch: request_body_bytes={body_bytes}, elapsed_ms={}",
        started.elapsed().as_millis()
    );
    drop(daemon);
    let store = risunest_sync_server::store::Store::open(directory.path()).unwrap();
    for (digest, expected) in hashes.iter().zip(objects) {
        assert_eq!(store.get_object(digest).unwrap(), expected);
    }
}

#[tokio::test]
async fn standalone_binary_serves_with_empty_path_and_resumes_after_process_kill() {
    let directory = tempfile::tempdir().unwrap();
    assert!(cli(directory.path(), &["init"]).status.success());
    let added = cli(directory.path(), &["device", "add"]);
    assert!(added.status.success());
    let credential: DeviceCredential = serde_json::from_slice(&added.stdout).unwrap();
    let http = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let mut daemon = Daemon::start(directory.path());
    let head: RemoteHead = daemon
        .request(&http, &credential, Method::GET, "/head")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(head.library_id, credential.library_id);
    // The independent binary owns the directory even when another command uses
    // the same identity. No shell, Node, Tauri, or WebView is in the child PATH.
    assert!(!cli(directory.path(), &["status"]).status.success());
    let body = (0..UPLOAD_CHUNK_BYTES + 17)
        .map(|n| (n % 251) as u8)
        .collect::<Vec<_>>();
    let digest = hash(&body);
    let started: serde_json::Value = daemon
        .request(&http, &credential, Method::POST, "/uploads")
        .json(&serde_json::json!({"hash":digest, "size":body.len().to_string()}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = started["uploadId"].as_str().unwrap();
    let part = &body[..UPLOAD_CHUNK_BYTES];
    daemon
        .request(
            &http,
            &credential,
            Method::PUT,
            &format!("/uploads/{id}/chunks/0"),
        )
        .header("x-content-sha256", hash(part))
        .body(part.to_vec())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    drop(daemon);
    daemon = Daemon::start(directory.path());
    let progress: serde_json::Value = daemon
        .request(&http, &credential, Method::GET, &format!("/uploads/{id}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(progress["verified"], serde_json::json!(["0"]));
    let tail = &body[UPLOAD_CHUNK_BYTES..];
    daemon
        .request(
            &http,
            &credential,
            Method::PUT,
            &format!("/uploads/{id}/chunks/1"),
        )
        .header("x-content-sha256", hash(tail))
        .body(tail.to_vec())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    daemon
        .request(
            &http,
            &credential,
            Method::POST,
            &format!("/uploads/{id}/complete"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let received = daemon
        .request(
            &http,
            &credential,
            Method::GET,
            &format!("/objects/{digest}"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(received.as_ref(), body);
    let final_head: RemoteHead = daemon
        .request(&http, &credential, Method::GET, "/head")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        final_head, head,
        "object recovery cannot advance the library head"
    );
}

#[cfg(windows)]
fn memory(child: &Child) -> (u64, u64) {
    let output = Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", &format!("$sample=Get-Process -Id {}; Write-Output ($sample.WorkingSet64.ToString()+','+$sample.PeakWorkingSet64.ToString())", child.id())]).output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let (current, peak) = text.trim().split_once(',').unwrap();
    (current.parse().unwrap(), peak.parse().unwrap())
}

#[cfg(target_os = "linux")]
fn memory(child: &Child) -> (u64, u64) {
    let status = std::fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap();
    let value = |name: &str| {
        let line = status.lines().find(|line| line.starts_with(name)).unwrap();
        line.split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u64>()
            .unwrap()
            * 1024
    };
    (value("VmRSS:"), value("VmHWM:"))
}

#[cfg(any(windows, target_os = "linux"))]
#[tokio::test]
#[ignore = "Explicit release daemon head latency and four-transfer RSS gate"]
async fn release_daemon_head_and_four_delta_transfers_resource_gate() {
    use risunest_sync_wire::{
        delta,
        transfer::{self, Frame},
    };
    #[allow(clippy::assertions_on_constants)] // Fail only when this ignored gate is explicitly run.
    {
        assert!(!cfg!(debug_assertions), "Run this gate with --release");
    }
    let directory = tempfile::tempdir().unwrap();
    assert!(cli(directory.path(), &["init"]).status.success());
    let mut credentials = Vec::new();
    for _ in 0..4 {
        let added = cli(directory.path(), &["device", "add"]);
        assert!(added.status.success());
        credentials.push(serde_json::from_slice::<DeviceCredential>(&added.stdout).unwrap());
    }
    let daemon = Daemon::start(directory.path());
    let http = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();
    let idle = memory(&daemon.child).0;
    let mut durations = Vec::new();
    for _ in 0..200 {
        let start = std::time::Instant::now();
        let response = daemon
            .request(&http, &credentials[0], Method::GET, "/head")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let etag = response.headers()["etag"].clone();
        assert!(response.bytes().await.unwrap().len() <= 1024);
        durations.push(start.elapsed().as_secs_f64() * 1000.0);
        let unchanged = daemon
            .request(&http, &credentials[0], Method::GET, "/head")
            .header("if-none-match", etag)
            .send()
            .await
            .unwrap();
        assert_eq!(unchanged.status(), 304);
        assert!(unchanged.bytes().await.unwrap().is_empty());
    }
    durations.sort_by(f64::total_cmp);
    let p95 = durations[189];
    let mut bases = Vec::new();
    for seed in 1u64..=4 {
        let mut state = seed;
        let bytes = (0..8 * 1024 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect::<Vec<_>>();
        bases.push(bytes);
    }
    let targets = bases
        .iter()
        .map(|base| {
            let mut target = base.clone();
            target[4 * 1024 * 1024..4 * 1024 * 1024 + 6].copy_from_slice(b"edited");
            target
        })
        .collect::<Vec<_>>();
    for bytes in bases.iter().chain(&targets) {
        let digest = hash(bytes);
        let start: serde_json::Value = daemon
            .request(&http, &credentials[0], Method::POST, "/uploads")
            .json(&serde_json::json!({"hash":digest,"size":bytes.len().to_string()}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = start["uploadId"].as_str().unwrap();
        for (index, chunk) in bytes.chunks(UPLOAD_CHUNK_BYTES).enumerate() {
            daemon
                .request(
                    &http,
                    &credentials[0],
                    Method::PUT,
                    &format!("/uploads/{id}/chunks/{index}"),
                )
                .header("x-content-sha256", hash(chunk))
                .body(chunk.to_vec())
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
        }
        daemon
            .request(
                &http,
                &credentials[0],
                Method::POST,
                &format!("/uploads/{id}/complete"),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    let expected = targets
        .iter()
        .map(|target| hash(target))
        .collect::<Vec<_>>();
    let base_ids = bases.iter().map(|b| hash(b)).collect::<Vec<_>>();
    assert_eq!(
        bases.iter().map(Vec::len).sum::<usize>(),
        delta::MAX_BASE_BYTES
    );
    let request = |index: usize| {
        daemon
            .request(
                &http,
                &credentials[index],
                Method::POST,
                "/objects/transfer",
            )
            .json(&serde_json::json!([{"target":expected[index],"bases":base_ids}]))
            .send()
    };
    let (a, b, c, d) = tokio::join!(request(0), request(1), request(2), request(3));
    for (response, target) in [a, b, c, d].into_iter().zip(targets) {
        let bytes = response
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let frames = transfer::decode(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
        let Frame::Delta(recipe) = &frames[0] else {
            panic!("Expected useful exact delta");
        };
        assert_eq!(
            recipe
                .apply(&bases.iter().map(Vec::as_slice).collect::<Vec<_>>())
                .unwrap(),
            target
        );
    }
    let peak = memory(&daemon.child).1;
    eprintln!("isolated release daemon: head_p95_ms={p95:.3}, idle_rss={idle}, four_delta_peak_rss={peak}");
    assert!(p95 <= 50.0, "head P95 exceeded candidate budget");
    assert!(
        idle <= 64 * 1024 * 1024,
        "idle RSS exceeded candidate budget"
    );
    assert!(
        peak <= 128 * 1024 * 1024,
        "four-transfer peak RSS exceeded candidate budget"
    );
}
