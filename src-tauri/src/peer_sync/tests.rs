use super::*;
use reqwest::blocking::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Barrier, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const CHILD_MODE_ENV: &str = "RISUNEST_P0_CLONE_CHILD_MODE";
const CHILD_SESSION_URL_ENV: &str = "RISUNEST_P0_CLONE_SESSION_URL";
const CHILD_STAGING_ROOT_ENV: &str = "RISUNEST_P0_CLONE_STAGING_ROOT";
const CHILD_MARKER_ENV: &str = "RISUNEST_P0_CLONE_MARKER";
const ANDROID_CHILD_JOB_ROOT_ENV: &str = "RISUNEST_P3_ANDROID_JOB_ROOT";

#[test]
#[ignore = "spawned by parent process tests"]
fn clone_process_child() {
    let Ok(mode) = std::env::var(CHILD_MODE_ENV) else {
        return;
    };
    match mode.as_str() {
        "proxy" => {
            let session_url = std::env::var(CHILD_SESSION_URL_ENV).unwrap();
            let staging_root = std::env::var(CHILD_STAGING_ROOT_ENV).unwrap();
            let mut client = LoopbackCloneClient::new(staging_root, session_url).unwrap();
            client.download(&TransferCancellation::new()).unwrap();
        }
        "mid-chunk-kill" => {
            let session_url = std::env::var(CHILD_SESSION_URL_ENV).unwrap();
            let staging_root = std::env::var(CHILD_STAGING_ROOT_ENV).unwrap();
            let mut client = LoopbackCloneClient::new(staging_root, session_url).unwrap();
            let marker = std::env::var(CHILD_MARKER_ENV).unwrap();
            client
                .download_with_progress(&TransferCancellation::new(), |bytes| {
                    if bytes >= CLONE_CHUNK_SIZE + 64 * 1024 {
                        write_durable_marker_and_wait(Path::new(&marker));
                    }
                })
                .unwrap();
            panic!("mid-chunk child completed before it was killed");
        }
        "cas-promotion-kill" => {
            let session_url = std::env::var(CHILD_SESSION_URL_ENV).unwrap();
            let staging_root = std::env::var(CHILD_STAGING_ROOT_ENV).unwrap();
            let mut client = LoopbackCloneClient::new(staging_root, session_url).unwrap();
            let marker = std::env::var(CHILD_MARKER_ENV).unwrap();
            client.pause_after_cas_promotion_for_test(marker);
            client.download(&TransferCancellation::new()).unwrap();
            panic!("CAS-promotion child completed before it was killed");
        }
        "android-verified-chunk-kill" => {
            let marker = std::env::var(CHILD_MARKER_ENV).unwrap();
            let job_root = std::env::var(ANDROID_CHILD_JOB_ROOT_ENV).unwrap();
            let mut job = AndroidResumableCloneJob::open(job_root).unwrap();
            job.download_with_progress(&TransferCancellation::new(), |bytes| {
                if bytes >= CLONE_CHUNK_SIZE + 64 * 1024 {
                    write_durable_marker_and_wait(Path::new(&marker));
                }
            })
            .unwrap();
            panic!("Android clone child completed before it was killed");
        }
        _ => panic!("unknown clone child mode: {mode}"),
    }
}

fn clone_child_command(mode: &str, session_url: &str, staging_root: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--ignored")
        .arg("--exact")
        .arg("peer_sync::tests::clone_process_child")
        .arg("--nocapture")
        .env("VITE_DISABLE_REALM", "true")
        .env(CHILD_MODE_ENV, mode)
        .env(CHILD_SESSION_URL_ENV, session_url)
        .env(CHILD_STAGING_ROOT_ENV, staging_root);
    command
}

fn write_durable_marker_and_wait(path: &Path) -> ! {
    let mut marker = File::create(path).unwrap();
    marker.write_all(b"ready").unwrap();
    marker.sync_all().unwrap();
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

fn spawn_clone_kill_child(
    mode: &str,
    session_url: &str,
    staging_root: &Path,
    marker: &Path,
) -> Child {
    clone_child_command(mode, session_url, staging_root)
        .env(CHILD_MARKER_ENV, marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn spawn_android_clone_kill_child(job_root: &Path, marker: &Path) -> Child {
    clone_child_command("android-verified-chunk-kill", "unused", job_root)
        .env(ANDROID_CHILD_JOB_ROOT_ENV, job_root)
        .env(CHILD_MARKER_ENV, marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_for_marker_and_kill(child: &mut Child, marker: &Path) {
    for _ in 0..400 {
        if marker.exists() {
            child.kill().unwrap();
            let status = child.wait().unwrap();
            assert!(
                !status.success(),
                "killed clone child unexpectedly succeeded"
            );
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("clone child exited before kill marker with {status}");
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("clone child did not reach the requested kill boundary");
}

struct FixtureSource {
    revision: u64,
    objects: Vec<PinnedSourceObject>,
    released: Arc<AtomicBool>,
}

struct FixtureLease {
    revision: u64,
    objects: Vec<PinnedSourceObject>,
    released: Arc<AtomicBool>,
}

impl Drop for FixtureLease {
    fn drop(&mut self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

impl CloneSource for FixtureSource {
    type Lease = FixtureLease;

    fn pin(&self) -> Result<Self::Lease, PeerSyncError> {
        self.released.store(false, Ordering::SeqCst);
        Ok(FixtureLease {
            revision: self.revision,
            objects: self.objects.clone(),
            released: Arc::clone(&self.released),
        })
    }
}

impl PinnedCloneRevision for FixtureLease {
    fn source_revision(&self) -> u64 {
        self.revision
    }

    fn objects(&self) -> Result<Vec<PinnedSourceObject>, PeerSyncError> {
        Ok(self.objects.clone())
    }
}

#[derive(Default)]
struct FixtureTarget {
    active_manifest: String,
    activation_count: usize,
    abort_count: usize,
    stage_count: usize,
}

#[derive(Default)]
struct FixtureStage {
    manifest_id: String,
    staged: Vec<StagedFixtureObject>,
    maximum_buffer_bytes: usize,
}

#[derive(Clone)]
struct StagedFixtureObject {
    kind: CloneObjectKind,
    logical_key: String,
    metadata: Value,
    sha256: String,
    byte_size: u64,
}

fn stage_fixture_object(
    stage: &mut FixtureStage,
    kind: CloneObjectKind,
    logical_key: &str,
    metadata: &Value,
    reader: &mut dyn Read,
) -> Result<(), PeerSyncError> {
    let mut hasher = Sha256::new();
    let mut byte_size = 0_u64;
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        stage.maximum_buffer_bytes = stage.maximum_buffer_bytes.max(buffer.len());
        hasher.update(&buffer[..read]);
        byte_size += read as u64;
    }
    stage.staged.push(StagedFixtureObject {
        kind,
        logical_key: logical_key.to_owned(),
        metadata: metadata.clone(),
        sha256: hex::encode(hasher.finalize()),
        byte_size,
    });
    Ok(())
}

impl CloneTargetAdapter for FixtureTarget {
    type Stage = FixtureStage;

    fn active_manifest_id(&self) -> Result<Option<String>, PeerSyncError> {
        Ok((!self.active_manifest.is_empty()).then(|| self.active_manifest.clone()))
    }

    fn begin(&mut self, manifest_id: &str) -> Result<Self::Stage, PeerSyncError> {
        self.stage_count += 1;
        Ok(FixtureStage {
            manifest_id: manifest_id.to_owned(),
            staged: Vec::new(),
            maximum_buffer_bytes: 0,
        })
    }

    fn stage_object(
        &mut self,
        stage: &mut Self::Stage,
        kind: CloneObjectKind,
        logical_key: &str,
        metadata: &Value,
        reader: &mut dyn Read,
    ) -> Result<(), PeerSyncError> {
        stage_fixture_object(stage, kind, logical_key, metadata, reader)
    }

    fn abort(&mut self, _stage: Self::Stage) -> Result<(), PeerSyncError> {
        self.abort_count += 1;
        Ok(())
    }

    fn activate_if_current(
        &mut self,
        stage: &mut Self::Stage,
        expected_manifest_id: Option<&str>,
        new_manifest_id: &str,
    ) -> Result<CloneActivation, PeerSyncError> {
        if self.active_manifest == new_manifest_id {
            return Ok(CloneActivation::AlreadyActive);
        }
        let actual = (!self.active_manifest.is_empty()).then(|| self.active_manifest.clone());
        if actual.as_deref() != expected_manifest_id {
            return Ok(CloneActivation::Conflict { actual });
        }
        assert_eq!(stage.manifest_id, new_manifest_id);
        self.active_manifest = stage.manifest_id.clone();
        self.activation_count += 1;
        Ok(CloneActivation::Activated)
    }
}

#[derive(Clone)]
struct ConcurrentFixtureTarget {
    state: Arc<Mutex<ConcurrentFixtureTargetState>>,
}

struct ConcurrentFixtureTargetState {
    active_manifest: Option<String>,
    activation_count: usize,
    abort_count: usize,
}

impl ConcurrentFixtureTarget {
    fn new(active_manifest: &str) -> Self {
        Self {
            state: Arc::new(Mutex::new(ConcurrentFixtureTargetState {
                active_manifest: Some(active_manifest.to_owned()),
                activation_count: 0,
                abort_count: 0,
            })),
        }
    }
}

impl CloneTargetAdapter for ConcurrentFixtureTarget {
    type Stage = FixtureStage;

    fn active_manifest_id(&self) -> Result<Option<String>, PeerSyncError> {
        Ok(self.state.lock().unwrap().active_manifest.clone())
    }

    fn begin(&mut self, manifest_id: &str) -> Result<Self::Stage, PeerSyncError> {
        Ok(FixtureStage {
            manifest_id: manifest_id.to_owned(),
            staged: Vec::new(),
            maximum_buffer_bytes: 0,
        })
    }

    fn stage_object(
        &mut self,
        stage: &mut Self::Stage,
        kind: CloneObjectKind,
        logical_key: &str,
        metadata: &Value,
        reader: &mut dyn Read,
    ) -> Result<(), PeerSyncError> {
        stage_fixture_object(stage, kind, logical_key, metadata, reader)
    }

    fn abort(&mut self, _stage: Self::Stage) -> Result<(), PeerSyncError> {
        self.state.lock().unwrap().abort_count += 1;
        Ok(())
    }

    fn activate_if_current(
        &mut self,
        stage: &mut Self::Stage,
        expected_manifest_id: Option<&str>,
        new_manifest_id: &str,
    ) -> Result<CloneActivation, PeerSyncError> {
        assert_eq!(stage.manifest_id, new_manifest_id);
        let mut state = self.state.lock().unwrap();
        if state.active_manifest.as_deref() == Some(new_manifest_id) {
            return Ok(CloneActivation::AlreadyActive);
        }
        if state.active_manifest.as_deref() != expected_manifest_id {
            return Ok(CloneActivation::Conflict {
                actual: state.active_manifest.clone(),
            });
        }
        state.active_manifest = Some(new_manifest_id.to_owned());
        state.activation_count += 1;
        Ok(CloneActivation::Activated)
    }
}

fn fixture_source(root: &Path, large_sizes: &[usize]) -> FixtureSource {
    let database = root.join("database.risusave");
    fs::write(&database, b"synthetic-roadmap-14-database").unwrap();
    let mut objects = vec![PinnedSourceObject::database(database)];
    for (index, size) in large_sizes.iter().copied().enumerate() {
        let path = root.join(format!("asset-{index}.bin"));
        write_pattern_file(&path, size, index as u8 + 11);
        objects.push(PinnedSourceObject::payload(
            CloneObjectKind::Asset,
            format!("assets/fixture-{index}.bin"),
            json!({ "mime": "application/octet-stream", "order": index }),
            path,
        ));
    }
    FixtureSource {
        revision: 42,
        objects,
        released: Arc::new(AtomicBool::new(false)),
    }
}

#[test]
fn production_source_manifest_declares_the_lossless_database_format() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let database = source_root.path().join("source.lossless");
    fs::write(&database, b"synthetic-lossless-package").unwrap();
    let source = FixtureSource {
        revision: 7,
        objects: vec![PinnedSourceObject::database_with_format(
            database,
            CLONE_LOSSLESS_DATABASE_FORMAT,
        )],
        released: Arc::new(AtomicBool::new(false)),
    };

    let session = prepare(&source, session_root.path());

    assert_eq!(
        session.manifest().database.format,
        CLONE_LOSSLESS_DATABASE_FORMAT
    );
    assert!(session.manifest().payloads.is_empty());
}

fn write_pattern_file(path: &Path, size: usize, seed: u8) {
    let mut file = File::create(path).unwrap();
    let mut remaining = size;
    let mut block = [0_u8; 64 * 1024];
    for (index, byte) in block.iter_mut().enumerate() {
        *byte = seed.wrapping_add(index as u8);
    }
    while remaining != 0 {
        let count = remaining.min(block.len());
        file.write_all(&block[..count]).unwrap();
        remaining -= count;
    }
    file.sync_all().unwrap();
}

fn prepare(source: &FixtureSource, root: &Path) -> PreparedCloneSession {
    fs::create_dir_all(root).unwrap();
    prepare_clone_session(source, root).unwrap()
}

fn payload_hash(session: &PreparedCloneSession, index: usize) -> String {
    session.manifest().payloads[index].object.clone()
}

#[test]
fn lan_host_binds_only_on_explicit_start_and_stops_completely() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    assert_eq!(host.address(), None);

    let pairing = host.start().unwrap();
    let address = host.address().unwrap();
    assert!(address.ip().is_unspecified());
    assert!(!pairing.claim.is_empty());
    host.stop().unwrap();
    assert!(TcpStream::connect(("127.0.0.1", address.port())).is_err());

    let restarted_pairing = host.start().unwrap();
    assert_eq!(restarted_pairing.session_id, pairing.session_id);
    assert!(host.address().is_some());
    host.stop().unwrap();
}

#[test]
fn private_lan_host_binds_the_exact_selected_interface() {
    let selected = if_addrs::get_if_addrs()
        .unwrap()
        .into_iter()
        .find_map(|interface| match interface.ip() {
            IpAddr::V4(address) if address.is_private() || address.is_link_local() => Some(address),
            _ => None,
        })
        .expect("test machine has no private or link-local IPv4 interface");

    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));

    host.start_private_lan(selected).unwrap();
    assert_eq!(host.address().unwrap().ip(), IpAddr::V4(selected));
    host.stop().unwrap();
}

#[test]
fn private_lan_host_rejects_non_lan_bind_addresses() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));

    for address in [
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::LOCALHOST,
        Ipv4Addr::new(203, 0, 113, 5),
    ] {
        assert!(host.start_private_lan(address).is_err());
        assert!(host.address().is_none());
    }
}

#[test]
fn quick_tunnel_origin_host_binds_only_ipv4_loopback() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));

    host.start_quick_tunnel_origin().unwrap();

    let address = host.address().unwrap();
    assert_eq!(
        address.ip(),
        "127.0.0.1".parse::<std::net::IpAddr>().unwrap()
    );
    assert_ne!(address.port(), 0);
    host.stop().unwrap();
}

#[test]
fn named_tunnel_probe_requires_a_started_loopback_host_and_is_one_time() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));

    assert!(host.issue_tunnel_probe().is_err());
    host.start_quick_tunnel_origin().unwrap();
    let address = host.address().unwrap();
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{}", address.port());

    // Windows loopback can abort sockets with transient errors under load. Every
    // retry issues a fresh probe, so the one-time assertions always run against a
    // probe whose exchange saw no transport interference; semantic mismatches
    // still panic immediately inside the attempt.
    let mut attempt = 0;
    loop {
        let probe = host.issue_tunnel_probe().unwrap();
        let exchange = (|| -> Result<(), reqwest::Error> {
            assert_eq!(
                client
                    .get(format!("{base}{}/wrong", probe.path_prefix))
                    .send()?
                    .status(),
                404
            );
            let response = client.get(format!("{base}{}", probe.path)).send()?;
            assert_eq!(response.status(), 200);
            assert_eq!(response.bytes()?.as_ref(), probe.expected_body);
            assert_eq!(
                client.get(format!("{base}{}", probe.path)).send()?.status(),
                404
            );
            Ok(())
        })();
        match exchange {
            Ok(()) => break,
            Err(error) if attempt < 3 => {
                attempt += 1;
                eprintln!("transient loopback failure, retrying with a fresh probe: {error}");
            }
            Err(error) => panic!("probe exchange failed after retries: {error}"),
        }
    }
    host.stop().unwrap();
}

#[test]
fn named_tunnel_public_seam_refuses_to_launch_before_loopback_source_start() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let host = LanCloneHost::prepare(prepare(&source, session_root.path()));

    let failure = match super::tunnel::start_named_desktop_tunnel(
        host,
        "eyJ-remotely-managed-tunnel-token".to_owned(),
        "https://sync.example.com",
    ) {
        Ok(_) => panic!("named tunnel started before its source host"),
        Err(failure) => failure,
    };
    assert_eq!(
        failure.error_message(),
        "tunnel origin must be 127.0.0.1 with a nonzero port"
    );
    let host = match failure.into_peer_session() {
        Ok(host) => host,
        Err(_) => panic!("source ownership was not recoverable"),
    };
    assert_eq!(host.address(), None);
}

#[test]
fn quick_tunnel_public_seam_refuses_to_launch_before_loopback_source_start() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let host = LanCloneHost::prepare(prepare(&source, session_root.path()));

    let failure = match super::tunnel::start_quick_desktop_tunnel(host) {
        Ok(_) => panic!("quick tunnel started before its source host"),
        Err(failure) => failure,
    };
    assert_eq!(
        failure.error_message(),
        "tunnel origin must be 127.0.0.1 with a nonzero port"
    );
    let host = match failure.into_peer_session() {
        Ok(host) => host,
        Err(_) => panic!("source ownership was not recoverable"),
    };
    assert_eq!(host.address(), None);
}

#[test]
fn named_tunnel_origin_uses_the_fixed_loopback_port() {
    // An ephemeral reservation stands in for the documented fixed port so a full
    // suite run never contends on the machine-global 32145.
    let occupied = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let fixed_port = occupied.local_addr().unwrap().port();
    let _override = super::lan::override_named_tunnel_origin_port_for_test(fixed_port);
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));

    let error = match host.start_named_tunnel_origin() {
        Ok(_) => panic!("named tunnel origin started on an occupied port"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        format!(
            "Transport(\"{}\")",
            super::lan::NAMED_TUNNEL_ORIGIN_UNAVAILABLE
        )
    );
    drop(occupied);

    host.start_named_tunnel_origin().unwrap();
    assert_eq!(
        host.address().unwrap(),
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, fixed_port))
    );
    host.stop().unwrap();
}

#[test]
fn host_control_does_not_retain_the_prepared_clone_after_owner_drop() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let control = host.control();

    assert!(control.is_attached_for_test());
    drop(host);

    assert!(!control.is_attached_for_test());
    assert!(control.devices().is_empty());
}

#[test]
fn lan_claim_bearer_progress_and_revoke_are_enforced_over_http() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let base = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let claim_url = format!("{base}/v1/sessions/{}/claim", pairing.session_id);
    let client = Client::new();

    assert_eq!(
        client
            .post(&claim_url)
            .json(&json!({"claim": "wrong"}))
            .send()
            .unwrap()
            .status(),
        403
    );
    host.expire_claim_for_test();
    assert_eq!(
        client
            .post(&claim_url)
            .json(&json!({"claim": pairing.claim}))
            .send()
            .unwrap()
            .status(),
        410
    );
    host.stop().unwrap();

    let second_session_root = tempfile::tempdir().unwrap();
    let mut host = LanCloneHost::prepare(prepare(&source, second_session_root.path()));
    let pairing = host.start().unwrap();
    let base = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let claim_url = format!("{base}/v1/sessions/{}/claim", pairing.session_id);
    let claim: Value = client
        .post(&claim_url)
        .json(&json!({"claim": pairing.claim}))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let bearer = claim["bearer"].as_str().unwrap();
    assert_eq!(
        client
            .post(&claim_url)
            .json(&json!({"claim": pairing.claim}))
            .send()
            .unwrap()
            .status(),
        410
    );

    let manifest_url = format!("{base}/v1/sessions/{}/manifest", pairing.session_id);
    assert_eq!(client.get(&manifest_url).send().unwrap().status(), 401);
    assert_eq!(
        client
            .get(&manifest_url)
            .bearer_auth("0".repeat(32))
            .send()
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(&manifest_url)
            .bearer_auth(bearer)
            .send()
            .unwrap()
            .status(),
        200
    );
    let object = host.manifest().payloads[0].object.clone();
    let object_url = format!("{base}/v1/sessions/{}/objects/{object}", pairing.session_id);
    assert_eq!(
        client
            .head(&object_url)
            .bearer_auth(bearer)
            .send()
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(&object_url)
            .bearer_auth(bearer)
            .header("range", "bytes=0-63")
            .send()
            .unwrap()
            .status(),
        206
    );
    let progress_url = format!("{base}/v1/sessions/{}/progress", pairing.session_id);
    assert_eq!(
        client
            .post(&progress_url)
            .bearer_auth(bearer)
            .json(&json!({"verifiedBytes": 42, "currentObject": object}))
            .send()
            .unwrap()
            .status(),
        204
    );
    assert_eq!(host.devices()[0].verified_bytes, 42);
    let device_id = claim["deviceId"].as_str().unwrap();
    assert!(host.revoke(device_id));
    assert_eq!(
        client
            .get(&manifest_url)
            .bearer_auth(bearer)
            .send()
            .unwrap()
            .status(),
        403
    );
    host.stop().unwrap();

    let restarted = host.start().unwrap();
    let restarted_manifest_url = format!(
        "http://127.0.0.1:{}/v1/sessions/{}/manifest",
        host.address().unwrap().port(),
        restarted.session_id,
    );
    assert_eq!(
        client
            .get(&restarted_manifest_url)
            .bearer_auth(bearer)
            .send()
            .unwrap()
            .status(),
        401
    );
    host.stop().unwrap();
}

#[test]
fn lan_object_routes_only_serve_exact_manifest_chunk_boundaries() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[CLONE_CHUNK_SIZE as usize + 1]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let base = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let client = Client::new();
    let claim: Value = client
        .post(format!("{base}/v1/sessions/{}/claim", pairing.session_id))
        .json(&json!({"claim": pairing.claim}))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let bearer = claim["bearer"].as_str().unwrap();
    let object = host.manifest().payloads[0].object.clone();
    let object_url = format!("{base}/v1/sessions/{}/objects/{object}", pairing.session_id);

    assert_eq!(
        client
            .get(&object_url)
            .bearer_auth(bearer)
            .header("range", format!("bytes=0-{}", CLONE_CHUNK_SIZE - 1))
            .send()
            .unwrap()
            .status(),
        206
    );
    assert_eq!(
        client
            .get(&object_url)
            .bearer_auth(bearer)
            .header(
                "range",
                format!("bytes={}-{}", CLONE_CHUNK_SIZE, CLONE_CHUNK_SIZE),
            )
            .send()
            .unwrap()
            .status(),
        206
    );
    assert_eq!(
        client
            .get(&object_url)
            .bearer_auth(bearer)
            .header("range", format!("bytes=1-{}", CLONE_CHUNK_SIZE - 1))
            .send()
            .unwrap()
            .status(),
        416
    );
    assert_eq!(
        client
            .get(&object_url)
            .bearer_auth(bearer)
            .header("range", "bytes=0-0,2-3")
            .send()
            .unwrap()
            .status(),
        416
    );
    host.stop().unwrap();
}

#[test]
fn lan_client_claims_without_putting_secret_in_request_urls_and_authenticates_reads() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    assert!(matches!(
        LanCloneClient::claim(
            "http://example.com:43123",
            &pairing.session_id,
            &pairing.claim,
        ),
        Err(PeerSyncError::Protocol(_))
    ));
    let client = LanCloneClient::claim(&endpoint, &pairing.session_id, &pairing.claim).unwrap();
    assert!(!client.session_url().contains(&pairing.claim));
    fs::write(
        session_root.path().join("manifest.json"),
        b"mutated-after-start",
    )
    .unwrap();
    assert!(matches!(
        client.fetch_manifest(&"f".repeat(64)),
        Err(PeerSyncError::StaleManifest { .. })
    ));
    let manifest = client.fetch_manifest(&pairing.manifest_id).unwrap();
    assert_eq!(manifest, host.manifest().canonical_bytes().unwrap());
    let object = host.manifest().payloads[0].object.clone();
    assert_eq!(client.head_object(&object).unwrap(), 64);
    assert_eq!(client.fetch_chunk(&object, 0, 63).unwrap().len(), 64);
    client.report_progress(64, Some(&object)).unwrap();
    assert!(host.revoke(&client.device_id));
    assert!(
        matches!(client.head_object(&object), Err(PeerSyncError::Transport(message)) if message.contains("403"))
    );
    host.stop().unwrap();
}

#[test]
fn lan_transport_reuses_the_p0_ledger_and_persisted_bearer_after_restart() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(
        source_root.path(),
        &[(CLONE_CHUNK_SIZE + 256 * 1024) as usize],
    );
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let credential_path = client_root.path().join("lan-credential.json");
    let claimed = LanCloneClient::claim_and_persist(
        &credential_path,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();

    let cancellation = TransferCancellation::new();
    let cancel_after_verified_chunk = cancellation.clone();
    let mut first =
        LoopbackCloneClient::from_lan(client_root.path(), claimed, &pairing.manifest_id).unwrap();
    assert!(matches!(
        first.download_with_progress(&cancellation, move |bytes| {
            if bytes > CLONE_CHUNK_SIZE {
                cancel_after_verified_chunk.cancel();
            }
        }),
        Err(PeerSyncError::Cancelled)
    ));
    let object = host.manifest().payloads[0].object.clone();
    assert_eq!(first.verified_chunk_count(&object), 1);
    drop(first);

    let persisted = LanCloneClient::open_persisted(&credential_path).unwrap();
    let mut restarted =
        LoopbackCloneClient::from_lan(client_root.path(), persisted, &pairing.manifest_id).unwrap();
    let report = restarted.download(&TransferCancellation::new()).unwrap();
    assert!(report.transferred_bytes < CLONE_CHUNK_SIZE);
    assert_eq!(report.maximum_buffer_bytes, 64 * 1024);
    let expected_verified = host
        .manifest()
        .objects
        .values()
        .map(|object| object.size)
        .sum::<u64>();
    assert_eq!(host.devices()[0].verified_bytes, expected_verified);
    host.stop().unwrap();
}

#[test]
fn persisted_lan_credential_rotates_atomically_over_an_existing_file() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let credential_root = tempfile::tempdir().unwrap();
    let credential_path = credential_root.path().join("lan-credential.json");
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));

    let first_pairing = host.start().unwrap();
    let first_endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let first = LanCloneClient::claim_and_persist(
        &credential_path,
        &first_endpoint,
        &first_pairing.session_id,
        &first_pairing.manifest_id,
        &first_pairing.claim,
    )
    .unwrap();
    let first_device_id = first.device_id.clone();
    let first_bytes = fs::read(&credential_path).unwrap();
    host.stop().unwrap();

    let second_pairing = host.start().unwrap();
    let second_endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let second = LanCloneClient::claim_and_persist(
        &credential_path,
        &second_endpoint,
        &second_pairing.session_id,
        &second_pairing.manifest_id,
        &second_pairing.claim,
    )
    .unwrap();
    assert_ne!(second.device_id, first_device_id);
    assert_ne!(fs::read(&credential_path).unwrap(), first_bytes);
    assert!(fs::read_dir(credential_root.path())
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".peer-credential-")));

    let reopened = LanCloneClient::open_persisted(&credential_path).unwrap();
    assert_eq!(reopened.device_id, second.device_id);
    assert_eq!(
        reopened
            .fetch_manifest(&second_pairing.manifest_id)
            .unwrap(),
        host.manifest().canonical_bytes().unwrap()
    );
    host.stop().unwrap();
}

#[cfg(unix)]
#[test]
fn persisted_lan_credential_is_owner_only_on_unix() {
    use std::os::unix::fs::PermissionsExt;

    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let credential_root = tempfile::tempdir().unwrap();
    let credential_parent = credential_root.path().join("peer-private");
    let credential_path = credential_parent.join("lan-credential.json");
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());

    LanCloneClient::claim_and_persist(
        &credential_path,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();

    assert_eq!(
        fs::metadata(&credential_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&credential_parent)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    host.stop().unwrap();
}

#[test]
fn android_clone_job_resumes_from_a_verified_chunk_after_actual_process_kill() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let jobs_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[(CLONE_CHUNK_SIZE * 2 + 97) as usize]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let job_root = jobs_root
        .path()
        .join("99999999-9999-4999-8999-999999999999");
    let job = AndroidResumableCloneJob::claim(
        &job_root,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();
    drop(job);
    let marker = jobs_root.path().join("verified-chunk.kill-ready");
    let mut child = spawn_android_clone_kill_child(&job_root, &marker);

    wait_for_marker_and_kill(&mut child, &marker);

    let mut completed = None;
    for _ in 0..4 {
        let mut reopened = AndroidResumableCloneJob::open(&job_root).unwrap();
        match reopened.download(&TransferCancellation::new()) {
            Ok(report) => {
                completed = Some((reopened, report));
                break;
            }
            Err(PeerSyncError::Transport(_)) => thread::sleep(Duration::from_millis(300)),
            Err(error) => panic!("Android clone resume failed: {error}"),
        }
    }
    let (reopened, report) = completed.expect("Android clone did not resume after LAN retry");
    let total_bytes = host
        .manifest()
        .objects
        .values()
        .map(|object| object.size)
        .sum::<u64>();
    assert_eq!(
        reopened.phase().unwrap(),
        AndroidCloneJobPhase::VerifiedAwaitingActivation
    );
    assert_eq!(report.verified_objects, host.manifest().objects.len());
    assert!(report.transferred_bytes <= total_bytes - CLONE_CHUNK_SIZE);
    assert_eq!(report.maximum_buffer_bytes, 64 * 1024);
    host.stop().unwrap();
}

#[test]
fn android_clone_rejects_a_missing_backup_before_receipt_publication() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let app_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[97]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let job_id = "99999999-9999-4999-8999-999999999998";
    let job_root = app_root.path().join("peer-clone-jobs").join(job_id);
    fs::create_dir_all(job_root.parent().unwrap()).unwrap();
    let job = AndroidResumableCloneJob::claim(
        &job_root,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();
    let backup_path = app_root
        .path()
        .join("peer-clone-activation/backups")
        .join(format!("pre-clone-{job_id}.lossless"));
    fs::create_dir_all(backup_path.parent().unwrap()).unwrap();
    fs::write(&backup_path, b"published backup").unwrap();
    fs::remove_file(&backup_path).unwrap();

    assert!(matches!(
        job.record_backup_path(&backup_path),
        Err(PeerSyncError::Storage(message))
            if message == "Android clone backup disappeared before receipt publication"
    ));
    assert!(job.status().unwrap().backup_path.is_none());
    host.stop().unwrap();
}

#[test]
fn android_clone_progress_status_failure_pauses_and_resumes_without_redownloading_verified_chunks()
{
    let source_root = tempfile::tempdir().unwrap();
    let jobs_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[(CLONE_CHUNK_SIZE * 2 + 97) as usize]);
    // Windows loopback can abort sockets with transient errors under load (mirrors the
    // bounded retry in the actual-process-kill fixture above). A LAN claim is single-use,
    // so each attempt stands up a fresh session and job root.
    let mut completed = None;
    for attempt in 0..4 {
        let session_root = tempfile::tempdir().unwrap();
        let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
        let pairing = host.start().unwrap();
        let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
        let job_root = jobs_root
            .path()
            .join(format!("66666666-6666-4666-8666-66666666666{attempt}"));
        let status_path = job_root.join("status.json");
        let status_backup = job_root.join("status.backup.json");
        let mut job = AndroidResumableCloneJob::claim(
            &job_root,
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        let cancellation = TransferCancellation::new();
        let mut sabotaged = false;
        let mut restored = false;

        let result = job.download_with_progress(&cancellation, |bytes| {
            if !sabotaged && bytes >= CLONE_CHUNK_SIZE + 64 * 1024 {
                fs::rename(&status_path, &status_backup).unwrap();
                fs::create_dir(&status_path).unwrap();
                sabotaged = true;
            } else if sabotaged && !restored && cancellation.is_cancelled() {
                // The wrapper cancels exactly when its status persist attempt fails and
                // still delivers that callback, so gating the restore on the cancellation
                // observes the injected failure deterministically regardless of how
                // loopback reads align with the 4MiB persistence steps.
                fs::remove_dir(&status_path).unwrap();
                fs::rename(&status_backup, &status_path).unwrap();
                restored = true;
            }
        });

        if !sabotaged && matches!(result, Err(PeerSyncError::Transport(_))) {
            host.stop().unwrap();
            thread::sleep(Duration::from_millis(300));
            continue;
        }
        // session_root must stay alive for the resume download below.
        completed = Some((session_root, host, job, result, sabotaged, restored));
        break;
    }
    let (_session_root, mut host, mut job, result, sabotaged, restored) =
        completed.expect("the LAN transfer kept failing before the sabotage could fire");

    assert!(sabotaged, "the status write failure was not injected");
    assert!(
        restored,
        "the status record was not restored after the injected failure"
    );
    assert!(matches!(result, Err(PeerSyncError::Storage(_))));
    assert_eq!(job.phase().unwrap(), AndroidCloneJobPhase::Paused);

    let report = job.download(&TransferCancellation::new()).unwrap();
    let total_bytes = host
        .manifest()
        .objects
        .values()
        .map(|object| object.size)
        .sum::<u64>();
    assert_eq!(
        job.phase().unwrap(),
        AndroidCloneJobPhase::VerifiedAwaitingActivation
    );
    assert!(report.transferred_bytes <= total_bytes - CLONE_CHUNK_SIZE);
    host.stop().unwrap();
}

#[test]
fn android_clone_progress_and_pause_status_failures_preserve_both_error_contexts() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let jobs_root = tempfile::tempdir().unwrap();
    // A small multi-callback object suffices: the sabotage fires on the first progress
    // callback, and the completion callback always attempts a status persist (the 4MiB
    // step gate is bypassed at completed == total), so the combined progress+pause
    // failure is exercised without a multi-chunk transfer.
    let source = fixture_source(source_root.path(), &[256 * 1024 + 97]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let job_root = jobs_root
        .path()
        .join("55555555-5555-4555-8555-555555555555");
    let status_path = job_root.join("status.json");
    let status_backup = job_root.join("status.backup.json");
    let mut job = AndroidResumableCloneJob::claim(
        &job_root,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();
    let mut sabotaged = false;

    let result = job.download_with_progress(&TransferCancellation::new(), |_| {
        if !sabotaged {
            fs::rename(&status_path, &status_backup).unwrap();
            fs::create_dir(&status_path).unwrap();
            sabotaged = true;
        }
    });

    assert!(sabotaged, "the status write failures were not injected");
    let error = result.unwrap_err().to_string();
    fs::remove_dir(&status_path).unwrap();
    fs::rename(&status_backup, &status_path).unwrap();
    assert!(error.contains("Android clone progress status persistence failed"));
    assert!(error.contains("failed to persist paused Android clone status"));
    host.stop().unwrap();
}

#[test]
fn android_clone_explicit_cancel_removes_only_the_owned_job_root() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let jobs_root = tempfile::tempdir().unwrap();
    let source = fixture_source(
        source_root.path(),
        &[64 * 1024, (CLONE_CHUNK_SIZE + 17) as usize],
    );
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let job_root = jobs_root
        .path()
        .join("88888888-8888-4888-8888-888888888888");
    let unrelated = jobs_root.path().join("keep.txt");
    fs::write(&unrelated, b"keep").unwrap();
    let cancellation = TransferCancellation::new();
    let cancellation_for_progress = cancellation.clone();
    let mut job = AndroidResumableCloneJob::claim(
        &job_root,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();

    let result = job.download_with_progress(&cancellation, move |bytes| {
        if bytes >= 128 * 1024 {
            cancellation_for_progress.cancel();
        }
    });

    assert_eq!(result.unwrap_err(), PeerSyncError::Cancelled);
    job.discard().unwrap();
    assert!(!job_root.exists());
    assert_eq!(fs::read(unrelated).unwrap(), b"keep");
    host.stop().unwrap();
}

#[test]
fn android_clone_cancel_marker_survives_reopen_before_owned_cleanup() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let jobs_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let job_root = jobs_root
        .path()
        .join("77777777-7777-4777-8777-777777777777");
    let unrelated = jobs_root.path().join("keep.txt");
    fs::write(&unrelated, b"keep").unwrap();
    let job = AndroidResumableCloneJob::claim(
        &job_root,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();

    job.request_cancel().unwrap();
    drop(job);
    assert!(AndroidResumableCloneJob::open(&job_root)
        .unwrap()
        .cancel_requested()
        .unwrap());

    AndroidResumableCloneJob::discard_at(&job_root).unwrap();
    assert!(!job_root.exists());
    assert_eq!(fs::read(unrelated).unwrap(), b"keep");
    host.stop().unwrap();
}

#[test]
fn android_clone_cancel_cannot_be_downgraded_by_a_late_pause() {
    let state = super::android_client::AndroidCloneStopState::new();

    state.request_cancel();
    state.request_pause();

    assert_eq!(
        state.current(),
        super::android_client::AndroidCloneStopReason::Cancel
    );
}

#[test]
fn android_clone_registry_recovers_one_persisted_job_without_exposing_the_bearer() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let app_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(app_root.path()).unwrap();

    let claimed = registry
        .claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();

    assert_eq!(claimed.phase, AndroidCloneJobPhase::Ready);
    assert_eq!(claimed.endpoint, endpoint);
    assert_eq!(claimed.session_id, pairing.session_id);
    assert_eq!(claimed.manifest_id, pairing.manifest_id);
    assert!(!serde_json::to_string(&claimed).unwrap().contains("bearer"));
    let legacy_job_root = app_root
        .path()
        .join("peer-clone-jobs")
        .join(&claimed.job_id);
    let descriptor: Value =
        serde_json::from_slice(&fs::read(legacy_job_root.join("job.json")).unwrap()).unwrap();
    let status: Value =
        serde_json::from_slice(&fs::read(legacy_job_root.join("status.json")).unwrap()).unwrap();
    assert!(descriptor.get("completionCapability").is_none());
    assert!(status.get("completionLeaseId").is_none());
    drop(registry);

    let reopened =
        super::android_client::AndroidCloneJobRegistry::initialize(app_root.path()).unwrap();
    assert_eq!(reopened.current().unwrap(), Some(claimed));
    host.stop().unwrap();
}

#[test]
fn android_clone_registry_starts_and_idempotently_resumes_a_registered_source() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let target_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    host.enable_v2_registry(
        source_root.path(),
        "Android source",
        super::device_registry::DevicePermissions::read(),
    )
    .unwrap();
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let credential = target_root.path().join("registration-credential.json");
    let _client = super::lan::LanCloneClient::claim_v2_and_persist_and_register(
        target_root.path(),
        "Android target",
        &credential,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();
    let source_device_id =
        super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
    let registered =
        super::device_registry::incoming_source_by_id(target_root.path(), &source_device_id)
            .unwrap()
            .unwrap();
    let target_device_id =
        super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();

    let started = registry
        .connect_registered(
            &registered.endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &registered.bearer,
            super::lan::PeerCompletionCapability::V1,
        )
        .unwrap();
    let job_root = target_root
        .path()
        .join("peer-clone-jobs")
        .join(&started.job_id);
    let descriptor: Value =
        serde_json::from_slice(&fs::read(job_root.join("job.json")).unwrap()).unwrap();
    let persisted_status: Value =
        serde_json::from_slice(&fs::read(job_root.join("status.json")).unwrap()).unwrap();
    assert_eq!(descriptor["completionCapability"], "v1");
    let completion_lease_id = persisted_status["completionLeaseId"].as_str().unwrap();
    assert_eq!(
        uuid::Uuid::parse_str(completion_lease_id)
            .unwrap()
            .get_version(),
        Some(uuid::Version::Random)
    );
    assert_ne!(completion_lease_id, started.job_id);
    let resumed = registry
        .connect_registered(
            &registered.endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &registered.bearer,
            super::lan::PeerCompletionCapability::V1,
        )
        .unwrap();

    assert_eq!(started, resumed);
    assert_eq!(started.phase, AndroidCloneJobPhase::Ready);
    assert!(!serde_json::to_string(&started)
        .unwrap()
        .contains(&registered.bearer));
    assert!(registry
        .connect_registered(
            &registered.endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &target_device_id,
            "00000000-0000-4000-8000-000000000299",
            &registered.bearer,
            super::lan::PeerCompletionCapability::V1,
        )
        .is_err());
    assert!(registry
        .connect_registered(
            &registered.endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            "00000000-0000-4000-8000-000000000298",
            &source_device_id,
            &registered.bearer,
            super::lan::PeerCompletionCapability::V1,
        )
        .is_err());
    let sources_path = target_root.path().join("peer-sync/sources.json");
    let before_rotation = fs::read(&sources_path).unwrap();
    let mut rotated_source = registered.clone();
    rotated_source.bearer = "0".repeat(64);
    assert!(
        super::registry_commands::register_incoming_source_if_compatible(
            target_root.path(),
            rotated_source,
        )
        .is_err()
    );
    assert_eq!(fs::read(&sources_path).unwrap(), before_rotation);
    let rotated_bearer = if registered.bearer == "f".repeat(64) {
        "e".repeat(64)
    } else {
        "f".repeat(64)
    };
    assert!(registry
        .connect_registered(
            &registered.endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &rotated_bearer,
            super::lan::PeerCompletionCapability::V1,
        )
        .is_err());

    drop(registry);
    let reopened =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();
    let recovered = reopened
        .connect_registered(
            &registered.endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &registered.bearer,
            super::lan::PeerCompletionCapability::V1,
        )
        .unwrap();
    assert_eq!(recovered, started);
    let safe_current = serde_json::to_string(&reopened.current_for_command().unwrap()).unwrap();
    assert!(safe_current.contains(&source_device_id));
    assert!(!safe_current.contains(&registered.endpoint));
    assert!(!safe_current.contains(&pairing.session_id));
    assert!(!safe_current.contains(&pairing.manifest_id));
    assert!(!safe_current.contains(&registered.bearer));
    let report = reopened
        .download(&recovered.job_id, &TransferCancellation::new())
        .unwrap();
    assert!(report.verified_objects >= 1);
    assert!(report.transferred_bytes >= 64);
    let outgoing_path = source_root.path().join("peer-sync/devices.json");
    let outgoing: Value = serde_json::from_slice(&fs::read(&outgoing_path).unwrap()).unwrap();
    let offer = outgoing["completionOffers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|offer| offer["leaseId"] == completion_lease_id)
        .unwrap();
    assert_eq!(offer["ready"], true);
    host.stop().unwrap();
    drop(reopened);
    let resumed_registry =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();
    let resumed_report = resumed_registry
        .download(&recovered.job_id, &TransferCancellation::new())
        .unwrap();
    assert_eq!(resumed_report.transferred_bytes, 0);
    assert_eq!(
        super::device_registry::accept_outgoing_completion_offer(
            source_root.path(),
            &target_device_id,
            super::device_registry::CompletionLane::Clone,
            completion_lease_id,
            &pairing.manifest_id,
            resumed_registry
                .current()
                .unwrap()
                .unwrap()
                .total_bytes
                .unwrap(),
        )
        .unwrap(),
        super::device_registry::CompletionAcceptance::Recorded
    );
}

#[test]
fn android_registered_clone_publication_serializes_source_removal_after_revalidation() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let target_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    host.enable_v2_registry(
        source_root.path(),
        "Android source",
        super::device_registry::DevicePermissions::read(),
    )
    .unwrap();
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let credential = target_root.path().join("registration-credential.json");
    let _client = super::lan::LanCloneClient::claim_v2_and_persist_and_register(
        target_root.path(),
        "Android target",
        &credential,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();
    let source_device_id =
        super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
    let registered =
        super::device_registry::incoming_source_by_id(target_root.path(), &source_device_id)
            .unwrap()
            .unwrap();
    let target_device_id =
        super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
    let registry = Arc::new(
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap(),
    );
    let reached_publish = Arc::new(Barrier::new(2));
    let resume_publish = Arc::new(Barrier::new(2));
    registry.pause_registered_publish_once_for_test(
        Arc::clone(&reached_publish),
        Arc::clone(&resume_publish),
    );
    let removal_source_device_id = source_device_id.clone();
    let worker_registry = Arc::clone(&registry);
    let worker = thread::spawn(move || {
        worker_registry.connect_registered(
            &registered.endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &registered.bearer,
            super::lan::PeerCompletionCapability::V1,
        )
    });

    reached_publish.wait();
    assert!(!target_root
        .path()
        .join("peer-clone-jobs")
        .join("current.json")
        .try_exists()
        .unwrap());
    let (removal_tx, removal_rx) = mpsc::channel();
    let removal_root = target_root.path().to_owned();
    let removal = thread::spawn(move || {
        let result = (|| {
            let _lifecycle = super::registry_commands::lock_registered_source_lifecycle()?;
            if super::android_client::registered_clone_source_is_active(
                &removal_root,
                &removal_source_device_id,
            )? {
                return Err(PeerSyncError::Validation(
                    "registered Android clone source is used by an active job".to_owned(),
                ));
            }
            super::device_registry::remove_incoming_source(&removal_root, &removal_source_device_id)
        })();
        removal_tx.send(result).unwrap();
    });
    assert!(removal_rx.recv_timeout(Duration::from_millis(100)).is_err());
    resume_publish.wait();

    worker.join().unwrap().unwrap();
    assert!(matches!(
        removal_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        Err(PeerSyncError::Validation(message))
            if message == "registered Android clone source is used by an active job"
    ));
    removal.join().unwrap();
    assert!(registry.current().unwrap().is_some());
    host.stop().unwrap();
}

#[test]
fn android_clone_recovery_discards_an_unpublished_registered_source_orphan() {
    let root = tempfile::tempdir().unwrap();
    let source_device_id = "00000000-0000-4000-8000-000000000291";
    let target_device_id = super::device_registry::load_or_create_device_id(root.path()).unwrap();
    let endpoint = "http://127.0.0.1:32145";
    let session_id = "00000000-0000-4000-8000-000000000292";
    let manifest_id = "e".repeat(64);
    let bearer = "f".repeat(64);
    super::device_registry::register_incoming_source(
        root.path(),
        super::device_registry::IncomingSource {
            device_id: source_device_id.to_owned(),
            name: "Android source".to_owned(),
            endpoint: endpoint.to_owned(),
            bearer: bearer.clone(),
            permissions: super::device_registry::DevicePermissions::read(),
            last_seen_ms: 0,
            total_bytes: 0,
        },
    )
    .unwrap();
    let registry = super::android_client::AndroidCloneJobRegistry::initialize(root.path()).unwrap();
    let started = registry
        .connect_registered(
            endpoint,
            session_id,
            &manifest_id,
            &target_device_id,
            source_device_id,
            &bearer,
            super::lan::PeerCompletionCapability::Unsupported,
        )
        .unwrap();
    let job_root = root.path().join("peer-clone-jobs").join(&started.job_id);
    assert!(job_root.exists());
    let resumed = registry
        .connect_registered(
            endpoint,
            session_id,
            &manifest_id,
            &target_device_id,
            source_device_id,
            &bearer,
            super::lan::PeerCompletionCapability::V1,
        )
        .unwrap();
    assert_eq!(resumed, started);
    let descriptor: Value =
        serde_json::from_slice(&fs::read(job_root.join("job.json")).unwrap()).unwrap();
    let status: Value =
        serde_json::from_slice(&fs::read(job_root.join("status.json")).unwrap()).unwrap();
    assert!(descriptor.get("completionCapability").is_none());
    assert!(status.get("completionLeaseId").is_none());
    drop(registry);

    let sources_path = root.path().join("peer-sync/sources.json");
    let mut sources: Value = serde_json::from_slice(&fs::read(&sources_path).unwrap()).unwrap();
    sources["sources"] = serde_json::json!([]);
    fs::write(&sources_path, serde_json::to_vec(&sources).unwrap()).unwrap();

    let recovered =
        super::android_client::AndroidCloneJobRegistry::initialize(root.path()).unwrap();
    assert!(recovered.current().unwrap().is_none());
    assert!(!job_root.exists());
}

#[test]
fn android_registration_claim_is_strict_v2_and_does_not_consume_a_legacy_claim() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let target_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let credential = target_root.path().join("strict-v2-credential.json");

    assert!(
        super::lan::LanCloneClient::claim_strict_v2_and_persist_and_register(
            target_root.path(),
            "Android target",
            &credential,
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .is_err()
    );
    assert!(!credential.exists());
    assert!(
        super::device_registry::incoming_source_summaries(target_root.path())
            .unwrap()
            .is_empty()
    );

    let legacy =
        super::lan::LanCloneClient::claim(&endpoint, &pairing.session_id, &pairing.claim).unwrap();
    assert!(legacy.registered_source_device_id().is_none());
    host.stop().unwrap();
}

#[test]
fn android_clone_registry_pauses_downloading_state_only_during_initial_recovery() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let app_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(app_root.path()).unwrap();
    let claimed = registry
        .claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
    fs::write(
        app_root
            .path()
            .join("peer-clone-jobs")
            .join(&claimed.job_id)
            .join("status.json"),
        serde_json::to_vec(&json!({
            "schema": "risunest.android-peer-clone-status/v1",
            "phase": "downloading",
            "completedBytes": 1,
            "totalBytes": 64,
            "error": null,
            "committedRevision": null,
        }))
        .unwrap(),
    )
    .unwrap();

    let current = registry.current().unwrap().unwrap();
    assert_eq!(current.phase, AndroidCloneJobPhase::Downloading);
    assert_eq!(current.backup_path, None);
    drop(registry);

    let reopened =
        super::android_client::AndroidCloneJobRegistry::initialize(app_root.path()).unwrap();
    assert_eq!(
        reopened.current().unwrap().unwrap().phase,
        AndroidCloneJobPhase::Paused
    );
    host.stop().unwrap();
}

#[test]
fn android_clone_registry_removes_a_pre_ownership_partial_claim_after_restart() {
    let app_root = tempfile::tempdir().unwrap();
    let job_id = "11111111-1111-4111-8111-111111111111";
    let job_root = app_root.path().join("peer-clone-jobs").join(job_id);
    fs::create_dir_all(&job_root).unwrap();

    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(app_root.path()).unwrap();

    assert!(registry.current().unwrap().is_none());
    assert!(!job_root.exists());
}

#[test]
fn android_clone_recovery_discards_a_valid_registered_job_without_published_ownership() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let target_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    host.enable_v2_registry(
        source_root.path(),
        "Android source",
        super::device_registry::DevicePermissions::read(),
    )
    .unwrap();
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registration_credential = target_root.path().join("registration-credential.json");
    let _registration = super::lan::LanCloneClient::claim_v2_and_persist_and_register(
        target_root.path(),
        "Android target",
        &registration_credential,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();
    let source_device_id =
        super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
    let registered =
        super::device_registry::incoming_source_by_id(target_root.path(), &source_device_id)
            .unwrap()
            .unwrap();
    let target_device_id =
        super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();
    let started = registry
        .connect_registered(
            &registered.endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &registered.bearer,
            super::lan::PeerCompletionCapability::Unsupported,
        )
        .unwrap();
    let jobs_root = target_root.path().join("peer-clone-jobs");
    let job_root = jobs_root.join(&started.job_id);
    fs::remove_file(jobs_root.join("current.json")).unwrap();
    drop(registry);

    let recovered =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();

    assert!(recovered.current().unwrap().is_none());
    assert!(!job_root.exists());
    assert!(
        super::device_registry::incoming_source_by_id(target_root.path(), &source_device_id)
            .unwrap()
            .is_some()
    );
    host.stop().unwrap();
}

#[test]
fn android_clone_registry_recovers_a_renamed_deletion_tombstone() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let app_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(app_root.path()).unwrap();
    let claimed = registry
        .claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
    drop(registry);
    let jobs_root = app_root.path().join("peer-clone-jobs");
    let tombstone = jobs_root.join(format!(".deleting-{}", claimed.job_id));
    fs::rename(jobs_root.join(&claimed.job_id), &tombstone).unwrap();
    let sibling = jobs_root.join("sibling.keep");
    fs::write(&sibling, b"keep").unwrap();

    let reopened =
        super::android_client::AndroidCloneJobRegistry::initialize(app_root.path()).unwrap();

    assert!(reopened.current().unwrap().is_none());
    assert!(!tombstone.exists());
    assert!(!jobs_root.join("current.json").exists());
    assert_eq!(fs::read(sibling).unwrap(), b"keep");
    host.stop().unwrap();
}

#[test]
fn android_clone_registry_rejects_a_second_target_until_the_owned_job_is_resolved() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let app_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(app_root.path()).unwrap();
    registry
        .claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();

    let error = registry
        .claim(
            &endpoint,
            "22222222-2222-4222-8222-222222222222",
            &"c".repeat(64),
            &"d".repeat(64),
        )
        .unwrap_err();

    assert!(matches!(error, PeerSyncError::Validation(_)));
    host.stop().unwrap();
}

#[test]
fn android_clone_registry_marks_a_transport_interruption_paused_for_foreground_resume() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let app_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(app_root.path()).unwrap();
    let claimed = registry
        .claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
    host.stop().unwrap();

    assert!(matches!(
        registry.download(&claimed.job_id, &TransferCancellation::new()),
        Err(PeerSyncError::Transport(_))
    ));
    assert_eq!(
        registry.current().unwrap().unwrap().phase,
        AndroidCloneJobPhase::Paused
    );
}

#[test]
fn android_clone_registry_reclaims_a_durable_cancel_marker_during_restart() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let app_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(app_root.path()).unwrap();
    let claimed = registry
        .claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
    registry.request_cancel(&claimed.job_id).unwrap();
    drop(registry);

    let reopened =
        super::android_client::AndroidCloneJobRegistry::initialize(app_root.path()).unwrap();

    assert!(reopened.current().unwrap().is_none());
    assert!(!app_root
        .path()
        .join("peer-clone-jobs")
        .join(claimed.job_id)
        .exists());
    host.stop().unwrap();
}

#[test]
fn android_clone_registry_retains_live_cancelled_ownership_until_native_cleanup() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let app_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(app_root.path()).unwrap();
    let claimed = registry
        .claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();

    registry.request_cancel(&claimed.job_id).unwrap();

    assert_eq!(
        registry.current().unwrap().unwrap().phase,
        AndroidCloneJobPhase::Cancelled
    );
    assert!(app_root
        .path()
        .join("peer-clone-jobs")
        .join(&claimed.job_id)
        .exists());
    assert!(super::android_jni::cancel_and_cleanup_job(
        &claimed.job_id,
        app_root.path().to_str().unwrap(),
    ));
    assert!(registry.current().unwrap().is_none());
    host.stop().unwrap();
}

#[test]
fn lossless_clone_stage_holds_maintenance_ownership_until_stage_drop() {
    let directory = tempfile::tempdir().unwrap();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let mut store = crate::persistent_store::PersistentStore::open(directory.path()).unwrap();
    let expected_revision = store.revision().unwrap();
    let activation_root = directory.path().join("peer-clone").join("activation");
    let mut target = LosslessCloneTargetAdapter::new(
        &mut store,
        &cas,
        &activation_root,
        expected_revision,
        &crate::local_backup::NeverCancelled,
    )
    .unwrap();
    let stage = target.begin(&"a".repeat(64)).unwrap();
    let stage_path = fs::read_dir(&activation_root)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name() != "backups")
        .unwrap()
        .path();
    fs::write(stage_path.join("payload"), b"active").unwrap();

    assert_eq!(
        super::maintenance::cleanup_temp(directory.path())
            .unwrap()
            .count,
        0
    );
    assert!(stage_path.exists());

    drop(stage);
    assert_eq!(
        super::maintenance::cleanup_temp(directory.path())
            .unwrap()
            .count,
        1
    );
    assert!(!stage_path.exists());
}

#[test]
fn android_registered_clone_accounts_once_and_releases_only_with_exact_durable_evidence() {
    let source_root = tempfile::tempdir().unwrap();
    let target_root = tempfile::tempdir().unwrap();
    let source_cas = crate::asset_repository::PayloadCas::new(source_root.path()).unwrap();
    let target_cas = crate::asset_repository::PayloadCas::new(target_root.path()).unwrap();
    let mut source_store =
        crate::persistent_store::PersistentStore::open(source_root.path()).unwrap();
    let mut target_store =
        crate::persistent_store::PersistentStore::open(target_root.path()).unwrap();
    seed_android_product_store(&mut source_store, "Source");
    seed_android_product_store(&mut target_store, "Target");
    let prepared = prepare_lossless_clone_session(
        &mut source_store,
        &source_cas,
        1,
        &source_root.path().join("preparation"),
        &source_root.path().join("session"),
        &crate::local_backup::NeverCancelled,
    )
    .unwrap();
    let mut host = LanCloneHost::prepare(prepared);
    host.enable_v2_registry(
        source_root.path(),
        "Android source",
        super::device_registry::DevicePermissions::read(),
    )
    .unwrap();
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registration_credential = target_root.path().join("registration-credential.json");
    let _registration = super::lan::LanCloneClient::claim_v2_and_persist_and_register(
        target_root.path(),
        "Android target",
        &registration_credential,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();
    let source_device_id =
        super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
    let registered =
        super::device_registry::incoming_source_by_id(target_root.path(), &source_device_id)
            .unwrap()
            .unwrap();
    let target_device_id =
        super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();
    let claimed = registry
        .connect_registered(
            &registered.endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &registered.bearer,
            super::lan::PeerCompletionCapability::V1,
        )
        .unwrap();

    registry
        .download(&claimed.job_id, &TransferCancellation::new())
        .unwrap();
    let verified_job_root = target_root
        .path()
        .join("peer-clone-jobs")
        .join(&claimed.job_id);
    let verified_job = AndroidResumableCloneJob::open(&verified_job_root).unwrap();
    assert!(!verified_job.request_cancel_for_platform_stop().unwrap());
    assert!(!verified_job_root.join("cancel.requested").exists());

    drop(registry);
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();

    assert_eq!(
        registry.current().unwrap().unwrap().phase,
        AndroidCloneJobPhase::VerifiedAwaitingActivation
    );
    assert_eq!(target_store.revision().unwrap(), 1);
    assert_eq!(
        target_store.read_root(None).unwrap().value["username"],
        "Target"
    );
    let status_path = verified_job_root.join("status.json");
    let verified_bytes = registry.current().unwrap().unwrap().total_bytes.unwrap();
    let mut forged_commit: Value =
        serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
    forged_commit["committedRevision"] = Value::from(2);
    target_store
        .set_app_kv(
            "peerCloneActiveManifest",
            &json!({ "manifestId": pairing.manifest_id, "revision": 2 }),
        )
        .unwrap();
    fs::write(&status_path, serde_json::to_vec(&forged_commit).unwrap()).unwrap();
    assert!(matches!(
        registry.finalize(
            &claimed.job_id,
            &mut target_store,
            &target_cas,
            &target_root.path().join("peer-clone-activation"),
            1,
            &crate::local_backup::NeverCancelled,
        ),
        Err(PeerSyncError::Validation(message))
            if message == "Android clone committed activation evidence is invalid"
    ));
    assert_eq!(
        target_store.read_root(None).unwrap().value["username"],
        "Target"
    );
    assert_eq!(
        super::device_registry::IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()[0]
            .total_bytes,
        0
    );
    forged_commit["committedRevision"] = Value::from(1);
    target_store
        .set_app_kv(
            "peerCloneActiveManifest",
            &json!({ "manifestId": "0".repeat(64), "revision": 1 }),
        )
        .unwrap();
    fs::write(&status_path, serde_json::to_vec(&forged_commit).unwrap()).unwrap();
    assert!(matches!(
        registry.finalize(
            &claimed.job_id,
            &mut target_store,
            &target_cas,
            &target_root.path().join("peer-clone-activation"),
            1,
            &crate::local_backup::NeverCancelled,
        ),
        Err(PeerSyncError::Validation(message))
            if message == "Android clone committed activation evidence is invalid"
    ));
    forged_commit["committedRevision"] = Value::from(0);
    target_store
        .set_app_kv(
            "peerCloneActiveManifest",
            &json!({ "manifestId": pairing.manifest_id, "revision": 1 }),
        )
        .unwrap();
    fs::write(&status_path, serde_json::to_vec(&forged_commit).unwrap()).unwrap();
    assert!(matches!(
        registry.finalize(
            &claimed.job_id,
            &mut target_store,
            &target_cas,
            &target_root.path().join("peer-clone-activation"),
            1,
            &crate::local_backup::NeverCancelled,
        ),
        Err(PeerSyncError::Validation(message))
            if message == "Android clone committed activation evidence is invalid"
    ));
    forged_commit["committedRevision"] = Value::Null;
    target_store
        .remove_app_kv("peerCloneActiveManifest")
        .unwrap();
    fs::write(&status_path, serde_json::to_vec(&forged_commit).unwrap()).unwrap();
    let mut tampered_status: Value =
        serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
    tampered_status["completedBytes"] = Value::from(1);
    tampered_status["totalBytes"] = Value::from(1);
    fs::write(&status_path, serde_json::to_vec(&tampered_status).unwrap()).unwrap();
    assert!(matches!(
        registry.finalize(
            &claimed.job_id,
            &mut target_store,
            &target_cas,
            &target_root.path().join("peer-clone-activation"),
            1,
            &crate::local_backup::NeverCancelled,
        ),
        Err(PeerSyncError::Storage(message))
            if message == "Android clone completion bytes differ from its verified transfer ledger"
    ));
    let mut retry_status: Value = serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
    let completion_lease_id = retry_status["completionLeaseId"]
        .as_str()
        .unwrap()
        .to_owned();
    retry_status["completedBytes"] = Value::from(verified_bytes);
    retry_status["totalBytes"] = Value::from(verified_bytes);
    fs::write(&status_path, serde_json::to_vec(&retry_status).unwrap()).unwrap();

    let receipt = registry
        .finalize(
            &claimed.job_id,
            &mut target_store,
            &target_cas,
            &target_root.path().join("peer-clone-activation"),
            2,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();

    assert_eq!(receipt.revision, 2);
    let backup_path = receipt.backup_path.as_ref().unwrap();
    assert!(backup_path.is_file());
    assert_eq!(
        fs::read_dir(target_root.path().join("peer-clone-activation/backups"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        target_store.read_root(None).unwrap().value["username"],
        "Source"
    );
    let committed = registry.current().unwrap().unwrap();
    assert_eq!(committed.committed_revision, Some(2));
    assert_eq!(committed.backup_path.as_ref(), Some(backup_path));
    assert_eq!(committed.total_bytes, Some(verified_bytes));
    let accounted =
        super::device_registry::IncomingSourceRegistry::load(target_root.path()).unwrap();
    assert_eq!(accounted.sources()[0].total_bytes, verified_bytes);
    let expected_receipt = super::device_registry::completion_receipt_id(
        "clone",
        &completion_lease_id,
        &pairing.manifest_id,
    );
    assert!(
        super::device_registry::incoming_completed_operation_recorded(
            target_root.path(),
            &source_device_id,
            &expected_receipt,
        )
        .unwrap()
    );
    assert_eq!(
        super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
            .unwrap()
            .devices()[0]
            .total_bytes,
        verified_bytes
    );
    let completion_status: Value =
        serde_json::from_slice(&fs::read(verified_job_root.join("status.json")).unwrap()).unwrap();
    assert_eq!(completion_status["completionAcknowledged"], true);
    let mut unrelated_source = registered.clone();
    let unrelated_source_id = uuid::Uuid::new_v4().to_string();
    unrelated_source.device_id = unrelated_source_id.clone();
    super::device_registry::register_incoming_source(target_root.path(), unrelated_source).unwrap();
    super::device_registry::remove_incoming_source(target_root.path(), &unrelated_source_id)
        .unwrap();
    assert_eq!(
        super::device_registry::remove_incoming_source(target_root.path(), &source_device_id)
            .unwrap_err(),
        PeerSyncError::Validation(
            "incoming source is used by the active Android clone job".to_owned()
        )
    );
    let committed_job = AndroidResumableCloneJob::open(&verified_job_root).unwrap();
    assert_eq!(
        committed_job
            .record_backup_path(backup_path)
            .unwrap()
            .backup_path
            .as_ref(),
        Some(backup_path)
    );
    assert!(committed_job
        .record_backup_path(&target_root.path().join("different.lossless"))
        .is_err());
    assert!(target_root
        .path()
        .join("peer-clone-jobs")
        .join(&claimed.job_id)
        .exists());
    assert_eq!(
        registry.request_cancel(&claimed.job_id).unwrap_err(),
        PeerSyncError::AlreadyActivated
    );

    let mut interrupted_status: Value =
        serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
    interrupted_status["committedRevision"] = Value::Null;
    interrupted_status["completionAcknowledged"] = Value::Bool(false);
    fs::write(
        &status_path,
        serde_json::to_vec(&interrupted_status).unwrap(),
    )
    .unwrap();
    assert_eq!(
        registry
            .finalize(
                &claimed.job_id,
                &mut target_store,
                &target_cas,
                &target_root.path().join("peer-clone-activation"),
                2,
                &crate::local_backup::NeverCancelled,
            )
            .unwrap(),
        receipt
    );
    assert_eq!(
        super::device_registry::IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()[0]
            .total_bytes,
        verified_bytes
    );
    assert_eq!(
        fs::read_dir(target_root.path().join("peer-clone-activation/backups"))
            .unwrap()
            .count(),
        1
    );

    drop(registry);
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();
    assert_eq!(
        registry.current().unwrap().unwrap().backup_path.as_ref(),
        Some(backup_path)
    );
    assert!(
        super::android_client::current_android_clone_job_references_backup(
            target_root.path(),
            backup_path
        )
        .unwrap()
    );
    assert_eq!(
        registry
            .finalize(
                &claimed.job_id,
                &mut target_store,
                &target_cas,
                &target_root.path().join("peer-clone-activation"),
                2,
                &crate::local_backup::NeverCancelled,
            )
            .unwrap(),
        receipt
    );
    assert_eq!(
        fs::read_dir(target_root.path().join("peer-clone-activation/backups"))
            .unwrap()
            .count(),
        1
    );

    super::device_registry::record_incoming_completed_operation_once(
        target_root.path(),
        &source_device_id,
        &super::device_registry::completion_receipt_id("clone", &claimed.job_id, &"0".repeat(64)),
        0,
    )
    .unwrap();
    assert!(registry.release(&claimed.job_id, &target_store).is_err());
    assert!(verified_job_root.exists());
    super::device_registry::record_incoming_completed_operation_once(
        target_root.path(),
        &source_device_id,
        &expected_receipt,
        verified_bytes,
    )
    .unwrap();
    registry.release(&claimed.job_id, &target_store).unwrap();

    assert!(registry.current().unwrap().is_none());
    assert!(
        !super::android_client::current_android_clone_job_references_backup(
            target_root.path(),
            backup_path
        )
        .unwrap()
    );
    assert!(!target_root
        .path()
        .join("peer-clone-jobs")
        .join(&claimed.job_id)
        .exists());
    host.stop().unwrap();
    super::device_registry::remove_incoming_source(target_root.path(), &source_device_id).unwrap();
}

#[test]
fn android_registered_clone_accounting_failure_survives_restart_without_ack_or_release() {
    let source_root = tempfile::tempdir().unwrap();
    let target_root = tempfile::tempdir().unwrap();
    let source_cas = crate::asset_repository::PayloadCas::new(source_root.path()).unwrap();
    let target_cas = crate::asset_repository::PayloadCas::new(target_root.path()).unwrap();
    let mut source_store =
        crate::persistent_store::PersistentStore::open(source_root.path()).unwrap();
    let mut target_store =
        crate::persistent_store::PersistentStore::open(target_root.path()).unwrap();
    seed_android_product_store(&mut source_store, "Source");
    seed_android_product_store(&mut target_store, "Target");
    let prepared = prepare_lossless_clone_session(
        &mut source_store,
        &source_cas,
        1,
        &source_root.path().join("preparation"),
        &source_root.path().join("session"),
        &crate::local_backup::NeverCancelled,
    )
    .unwrap();
    let mut host = LanCloneHost::prepare(prepared);
    host.enable_v2_registry(
        source_root.path(),
        "Android source",
        super::device_registry::DevicePermissions::read(),
    )
    .unwrap();
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registration_credential = target_root.path().join("registration-credential.json");
    let _registration = super::lan::LanCloneClient::claim_v2_and_persist_and_register(
        target_root.path(),
        "Android target",
        &registration_credential,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();
    let source_device_id =
        super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
    let registered =
        super::device_registry::incoming_source_by_id(target_root.path(), &source_device_id)
            .unwrap()
            .unwrap();
    let target_device_id =
        super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();
    let claimed = registry
        .connect_registered(
            &registered.endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &registered.bearer,
            super::lan::PeerCompletionCapability::V1,
        )
        .unwrap();
    registry
        .download(&claimed.job_id, &TransferCancellation::new())
        .unwrap();
    let verified_bytes = registry.current().unwrap().unwrap().total_bytes.unwrap();
    let mut incoming =
        super::device_registry::IncomingSourceRegistry::load(target_root.path()).unwrap();
    incoming.remove(&source_device_id).unwrap();
    let mut saturated = registered.clone();
    saturated.total_bytes = u64::MAX;
    incoming.upsert(saturated).unwrap();
    incoming.save().unwrap();
    let activation_root = target_root.path().join("peer-clone-activation");

    let error = registry
        .finalize(
            &claimed.job_id,
            &mut target_store,
            &target_cas,
            &activation_root,
            1,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap_err();

    assert_eq!(
        error,
        PeerSyncError::Validation("peer total bytes overflow".to_owned())
    );
    assert_eq!(target_store.revision().unwrap(), 2);
    let interrupted = registry.current().unwrap().unwrap();
    assert_eq!(interrupted.committed_revision, Some(2));
    let backup_path = interrupted.backup_path.unwrap();
    assert!(backup_path.is_file());
    let status_path = target_root
        .path()
        .join("peer-clone-jobs")
        .join(&claimed.job_id)
        .join("status.json");
    let persisted: Value = serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
    assert_ne!(persisted["completionAcknowledged"], true);
    assert_eq!(
        super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
            .unwrap()
            .devices()[0]
            .total_bytes,
        verified_bytes
    );
    let interrupted_sources: Value = serde_json::from_slice(
        &fs::read(target_root.path().join("peer-sync/sources.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        interrupted_sources["sources"][0]["totalBytes"],
        Value::from(u64::MAX)
    );
    assert_eq!(
        interrupted_sources["pendingCompletionDeliveries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        super::registry_commands::remove_incoming_source_if_inactive(
            target_root.path(),
            &source_device_id,
        )
        .is_err()
    );
    assert!(registry.release(&claimed.job_id, &target_store).is_err());
    assert!(status_path.is_file());
    target_store
        .remove_app_kv("peerCloneAndroidActiveOperation")
        .unwrap();
    let mut post_activation_root = target_store.read_root(None).unwrap().value;
    post_activation_root["username"] = Value::from("Post activation edit");
    let ordinary_commit: crate::persistent_store::WorkingSetCommit =
        serde_json::from_value(json!({
            "expectedRevision": 2,
            "root": post_activation_root,
        }))
        .unwrap();
    target_store.commit(&ordinary_commit).unwrap();
    assert_eq!(target_store.revision().unwrap(), 3);
    drop(registry);
    let mut legacy_status: Value =
        serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
    legacy_status
        .as_object_mut()
        .unwrap()
        .remove("completionAcknowledged");
    fs::write(&status_path, serde_json::to_vec(&legacy_status).unwrap()).unwrap();

    let restarted =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();
    assert_eq!(
        restarted.current().unwrap().unwrap().committed_revision,
        Some(2)
    );
    let sources_path = target_root.path().join("peer-sync/sources.json");
    let mut reset: Value = serde_json::from_slice(&fs::read(&sources_path).unwrap()).unwrap();
    reset["sources"][0]["totalBytes"] = Value::from(0);
    fs::write(&sources_path, serde_json::to_vec(&reset).unwrap()).unwrap();

    restarted.release(&claimed.job_id, &target_store).unwrap();

    assert_eq!(
        super::device_registry::IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()[0]
            .total_bytes,
        verified_bytes
    );
    assert_eq!(
        super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
            .unwrap()
            .devices()[0]
            .total_bytes,
        verified_bytes
    );
    assert!(!status_path.exists());
    assert_eq!(target_store.revision().unwrap(), 3);
    host.stop().unwrap();
}

#[test]
fn android_clone_registry_rebuilds_verified_missing_status_from_transfer_ledger() {
    let source_root = tempfile::tempdir().unwrap();
    let target_root = tempfile::tempdir().unwrap();
    let source_cas = crate::asset_repository::PayloadCas::new(source_root.path()).unwrap();
    let target_cas = crate::asset_repository::PayloadCas::new(target_root.path()).unwrap();
    let mut source_store =
        crate::persistent_store::PersistentStore::open(source_root.path()).unwrap();
    let mut target_store =
        crate::persistent_store::PersistentStore::open(target_root.path()).unwrap();
    seed_android_product_store(&mut source_store, "Source");
    seed_android_product_store(&mut target_store, "Target");
    let prepared = prepare_lossless_clone_session(
        &mut source_store,
        &source_cas,
        1,
        &source_root.path().join("preparation"),
        &source_root.path().join("session"),
        &crate::local_backup::NeverCancelled,
    )
    .unwrap();
    let mut host = LanCloneHost::prepare(prepared);
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();
    let claimed = registry
        .claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
    registry
        .download(&claimed.job_id, &TransferCancellation::new())
        .unwrap();
    let verified = registry.current().unwrap().unwrap();
    let status_path = target_root
        .path()
        .join("peer-clone-jobs")
        .join(&claimed.job_id)
        .join("status.json");
    fs::remove_file(&status_path).unwrap();
    drop(registry);

    let restarted =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();
    let rebuilt = restarted.current().unwrap().unwrap();
    assert_eq!(
        rebuilt.phase,
        AndroidCloneJobPhase::VerifiedAwaitingActivation
    );
    assert_eq!(rebuilt.completed_bytes, verified.completed_bytes);
    assert_eq!(rebuilt.total_bytes, verified.total_bytes);
    restarted
        .finalize(
            &claimed.job_id,
            &mut target_store,
            &target_cas,
            &target_root.path().join("peer-clone-activation"),
            1,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();
    restarted.release(&claimed.job_id, &target_store).unwrap();
    assert!(restarted.current().unwrap().is_none());
    host.stop().unwrap();
}

#[test]
fn android_registered_clone_new_job_for_same_manifest_counts_as_a_distinct_operation() {
    let source_root = tempfile::tempdir().unwrap();
    let target_root = tempfile::tempdir().unwrap();
    let source_cas = crate::asset_repository::PayloadCas::new(source_root.path()).unwrap();
    let target_cas = crate::asset_repository::PayloadCas::new(target_root.path()).unwrap();
    let mut source_store =
        crate::persistent_store::PersistentStore::open(source_root.path()).unwrap();
    let mut target_store =
        crate::persistent_store::PersistentStore::open(target_root.path()).unwrap();
    seed_android_product_store(&mut source_store, "Source");
    seed_android_product_store(&mut target_store, "Target");
    let activation_root = target_root.path().join("peer-clone-activation");
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();

    let prepared = prepare_lossless_clone_session(
        &mut source_store,
        &source_cas,
        1,
        &source_root.path().join("preparation-one"),
        &source_root.path().join("session-one"),
        &crate::local_backup::NeverCancelled,
    )
    .unwrap();
    let mut host = LanCloneHost::prepare(prepared);
    host.enable_v2_registry(
        source_root.path(),
        "Android source",
        super::device_registry::DevicePermissions::read(),
    )
    .unwrap();
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registration_credential = target_root.path().join("registration-credential.json");
    let _registration = super::lan::LanCloneClient::claim_v2_and_persist_and_register(
        target_root.path(),
        "Android target",
        &registration_credential,
        &endpoint,
        &pairing.session_id,
        &pairing.manifest_id,
        &pairing.claim,
    )
    .unwrap();
    let source_device_id =
        super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
    let registered =
        super::device_registry::incoming_source_by_id(target_root.path(), &source_device_id)
            .unwrap()
            .unwrap();
    let target_device_id =
        super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
    let first = registry
        .connect_registered(
            &registered.endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &registered.bearer,
            super::lan::PeerCompletionCapability::V1,
        )
        .unwrap();
    registry
        .download(&first.job_id, &TransferCancellation::new())
        .unwrap();
    let first_total = registry.current().unwrap().unwrap().total_bytes.unwrap();
    registry.leave_activation_stage_after_commit_once_for_test();
    let first_receipt = registry
        .finalize(
            &first.job_id,
            &mut target_store,
            &target_cas,
            &activation_root,
            1,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();
    let first_backup = first_receipt.backup_path.unwrap();
    assert_eq!(
        super::device_registry::IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()[0]
            .total_bytes,
        first_total
    );
    assert_eq!(
        super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
            .unwrap()
            .devices()[0]
            .total_bytes,
        first_total
    );
    registry.release(&first.job_id, &target_store).unwrap();
    let mut before_unsupported = target_store.read_root(None).unwrap().value;
    before_unsupported["username"] = Value::from("Edited before unsupported clone");
    let before_unsupported_commit: crate::persistent_store::WorkingSetCommit =
        serde_json::from_value(json!({
            "expectedRevision": 2,
            "root": before_unsupported,
        }))
        .unwrap();
    target_store.commit(&before_unsupported_commit).unwrap();
    assert_eq!(target_store.revision().unwrap(), 3);
    assert!(host.revoke(&target_device_id));
    let legacy_pairing = host.rotate_pairing_link().unwrap();
    let _registration = super::lan::LanCloneClient::claim_v2_and_persist_and_register(
        target_root.path(),
        "Android target",
        &target_root
            .path()
            .join("registration-credential-after-revoke.json"),
        &endpoint,
        &legacy_pairing.session_id,
        &legacy_pairing.manifest_id,
        &legacy_pairing.claim,
    )
    .unwrap();
    let registered =
        super::device_registry::incoming_source_by_id(target_root.path(), &source_device_id)
            .unwrap()
            .unwrap();
    let legacy_no_backup = registry
        .connect_registered(
            &registered.endpoint,
            &legacy_pairing.session_id,
            &legacy_pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &registered.bearer,
            super::lan::PeerCompletionCapability::Unsupported,
        )
        .unwrap();
    registry
        .download(&legacy_no_backup.job_id, &TransferCancellation::new())
        .unwrap();
    let legacy_total = registry.current().unwrap().unwrap().total_bytes.unwrap();
    assert_eq!(host.devices()[0].verified_bytes, legacy_total);
    let legacy_receipt = registry
        .finalize(
            &legacy_no_backup.job_id,
            &mut target_store,
            &target_cas,
            &activation_root,
            3,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();
    assert_eq!(legacy_receipt.revision, 4);
    assert!(legacy_receipt.backup_path.as_ref().unwrap().is_file());
    assert_eq!(
        super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
            .unwrap()
            .devices()[0]
            .total_bytes,
        0
    );
    assert_eq!(
        super::device_registry::IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()[0]
            .total_bytes,
        first_total + legacy_total
    );
    let unsupported_registry: Value = serde_json::from_slice(
        &fs::read(target_root.path().join("peer-sync/sources.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        unsupported_registry["pendingCompletionDeliveries"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        0
    );
    target_store
        .remove_app_kv("peerCloneAndroidActiveOperation")
        .unwrap();
    let legacy_status_path = target_root
        .path()
        .join("peer-clone-jobs")
        .join(&legacy_no_backup.job_id)
        .join("status.json");
    let mut legacy_status: Value =
        serde_json::from_slice(&fs::read(&legacy_status_path).unwrap()).unwrap();
    legacy_status["completionAcknowledged"] = Value::Bool(false);
    fs::write(
        &legacy_status_path,
        serde_json::to_vec(&legacy_status).unwrap(),
    )
    .unwrap();
    drop(registry);
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();
    registry
        .release(&legacy_no_backup.job_id, &target_store)
        .unwrap();
    let migrated_witness = target_store
        .get_app_kv("peerCloneAndroidActiveOperation")
        .unwrap()
        .unwrap();
    assert_eq!(migrated_witness["jobId"], legacy_no_backup.job_id);
    assert_eq!(migrated_witness["manifestId"], pairing.manifest_id);
    assert_eq!(migrated_witness["revision"], 4);
    let mut post_clone_root = target_store.read_root(None).unwrap().value;
    post_clone_root["username"] = Value::from("Edited after first clone");
    let ordinary_commit: crate::persistent_store::WorkingSetCommit =
        serde_json::from_value(json!({
            "expectedRevision": 4,
            "root": post_clone_root,
        }))
        .unwrap();
    target_store.commit(&ordinary_commit).unwrap();
    assert_eq!(target_store.revision().unwrap(), 5);
    let second_pairing = host.rotate_pairing_link().unwrap();
    assert_eq!(second_pairing.manifest_id, pairing.manifest_id);
    let second = registry
        .connect_registered(
            &registered.endpoint,
            &second_pairing.session_id,
            &second_pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &registered.bearer,
            super::lan::PeerCompletionCapability::V1,
        )
        .unwrap();
    assert_ne!(second.job_id, first.job_id);
    registry
        .download(&second.job_id, &TransferCancellation::new())
        .unwrap();
    let second_total = registry.current().unwrap().unwrap().total_bytes.unwrap();
    let second_status_path = target_root
        .path()
        .join("peer-clone-jobs")
        .join(&second.job_id)
        .join("status.json");
    let mut replayed_status: Value =
        serde_json::from_slice(&fs::read(&second_status_path).unwrap()).unwrap();
    replayed_status["committedRevision"] = Value::from(2);
    replayed_status["backupPath"] = Value::from(first_backup.to_string_lossy().to_string());
    fs::write(
        &second_status_path,
        serde_json::to_vec(&replayed_status).unwrap(),
    )
    .unwrap();
    assert_eq!(
        registry
            .finalize(
                &second.job_id,
                &mut target_store,
                &target_cas,
                &activation_root,
                5,
                &crate::local_backup::NeverCancelled,
            )
            .unwrap_err(),
        PeerSyncError::Validation(
            "Android clone committed activation evidence is invalid".to_owned()
        )
    );
    assert_eq!(
        super::device_registry::IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()[0]
            .total_bytes,
        first_total + legacy_total
    );
    replayed_status["committedRevision"] = Value::Null;
    replayed_status["backupPath"] = Value::Null;
    fs::write(
        &second_status_path,
        serde_json::to_vec(&replayed_status).unwrap(),
    )
    .unwrap();

    let second_receipt = registry
        .finalize(
            &second.job_id,
            &mut target_store,
            &target_cas,
            &activation_root,
            5,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();

    assert_eq!(second_receipt.revision, 6);
    let second_backup = second_receipt.backup_path.unwrap();
    assert!(second_backup.is_file());
    assert_eq!(
        registry.current().unwrap().unwrap().backup_path,
        Some(second_backup)
    );
    assert_eq!(
        super::device_registry::IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()[0]
            .total_bytes,
        first_total + legacy_total + second_total
    );
    assert_eq!(
        super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
            .unwrap()
            .devices()[0]
            .total_bytes,
        second_total
    );
    assert!(first_backup.is_file());
    assert_eq!(
        fs::read_dir(activation_root.join("backups"))
            .unwrap()
            .count(),
        3
    );

    registry.release(&second.job_id, &target_store).unwrap();
    let already_active = registry
        .connect_registered(
            &registered.endpoint,
            &second_pairing.session_id,
            &second_pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &registered.bearer,
            super::lan::PeerCompletionCapability::V1,
        )
        .unwrap();
    registry
        .download(&already_active.job_id, &TransferCancellation::new())
        .unwrap();
    assert!(registry
        .finalize(
            &already_active.job_id,
            &mut target_store,
            &target_cas,
            &activation_root,
            6,
            &crate::local_backup::NeverCancelled,
        )
        .is_err());
    assert_eq!(
        super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
            .unwrap()
            .devices()[0]
            .total_bytes,
        second_total
    );
    assert_eq!(
        super::device_registry::IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()[0]
            .total_bytes,
        first_total + legacy_total + second_total
    );
    let already_active_status = registry.current().unwrap().unwrap();
    assert_eq!(already_active_status.backup_path, None);
    let persisted_already_active: Value = serde_json::from_slice(
        &fs::read(
            target_root
                .path()
                .join("peer-clone-jobs")
                .join(&already_active.job_id)
                .join("status.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_ne!(persisted_already_active["completionAcknowledged"], true);
    host.stop().unwrap();
}

#[test]
fn android_clone_registry_retries_after_backup_receipt_persistence_fails() {
    let source_root = tempfile::tempdir().unwrap();
    let target_root = tempfile::tempdir().unwrap();
    let source_cas = crate::asset_repository::PayloadCas::new(source_root.path()).unwrap();
    let target_cas = crate::asset_repository::PayloadCas::new(target_root.path()).unwrap();
    let mut source_store =
        crate::persistent_store::PersistentStore::open(source_root.path()).unwrap();
    let mut target_store =
        crate::persistent_store::PersistentStore::open(target_root.path()).unwrap();
    seed_android_product_store(&mut source_store, "Source");
    seed_android_product_store(&mut target_store, "Target");
    let prepared = prepare_lossless_clone_session(
        &mut source_store,
        &source_cas,
        1,
        &source_root.path().join("preparation"),
        &source_root.path().join("session"),
        &crate::local_backup::NeverCancelled,
    )
    .unwrap();
    let mut host = LanCloneHost::prepare(prepared);
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let registry =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();
    let claimed = registry
        .claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
    registry
        .download(&claimed.job_id, &TransferCancellation::new())
        .unwrap();
    let activation_root = target_root.path().join("peer-clone-activation");
    registry.fail_backup_receipt_write_once_for_test();

    assert!(registry
        .finalize(
            &claimed.job_id,
            &mut target_store,
            &target_cas,
            &activation_root,
            1,
            &crate::local_backup::NeverCancelled,
        )
        .is_err());
    assert_eq!(target_store.revision().unwrap(), 2);
    let interrupted = registry.current().unwrap().unwrap();
    assert_eq!(interrupted.committed_revision, None);
    assert_eq!(interrupted.backup_path, None);

    let receipt = registry
        .finalize(
            &claimed.job_id,
            &mut target_store,
            &target_cas,
            &activation_root,
            2,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();

    assert_eq!(receipt.revision, 2);
    assert!(receipt.backup_path.as_ref().unwrap().is_file());
    assert_eq!(
        registry.current().unwrap().unwrap().committed_revision,
        Some(2)
    );
    assert_eq!(
        fs::read_dir(activation_root.join("backups"))
            .unwrap()
            .count(),
        1
    );
    registry.release(&claimed.job_id, &target_store).unwrap();
    let legacy_pairing = host.rotate_pairing_link().unwrap();
    let legacy = registry
        .claim(
            &endpoint,
            &legacy_pairing.session_id,
            &legacy_pairing.manifest_id,
            &legacy_pairing.claim,
        )
        .unwrap();
    registry
        .download(&legacy.job_id, &TransferCancellation::new())
        .unwrap();
    let legacy_receipt = registry
        .finalize(
            &legacy.job_id,
            &mut target_store,
            &target_cas,
            &activation_root,
            2,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();
    assert_eq!(legacy_receipt.revision, 2);
    assert_eq!(legacy_receipt.backup_path, None);
    target_store
        .remove_app_kv("peerCloneAndroidActiveOperation")
        .unwrap();
    let legacy_status_path = target_root
        .path()
        .join("peer-clone-jobs")
        .join(&legacy.job_id)
        .join("status.json");
    let mut legacy_status: Value =
        serde_json::from_slice(&fs::read(&legacy_status_path).unwrap()).unwrap();
    legacy_status
        .as_object_mut()
        .unwrap()
        .remove("completionAcknowledged");
    fs::write(
        &legacy_status_path,
        serde_json::to_vec(&legacy_status).unwrap(),
    )
    .unwrap();
    drop(registry);
    let restarted =
        super::android_client::AndroidCloneJobRegistry::initialize(target_root.path()).unwrap();
    restarted.release(&legacy.job_id, &target_store).unwrap();
    assert!(restarted.current().unwrap().is_none());
    host.stop().unwrap();
}

fn seed_android_product_store(
    store: &mut crate::persistent_store::PersistentStore,
    username: &str,
) {
    let staging = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(
            &staging,
            &json!({
                "username": username,
                "botPresetsId": 0,
                "personas": [{ "id": "persona" }],
                "selectedPersona": 0,
                "enabledModules": [],
                "characterOrder": [],
                "modules": [],
                "loadouts": [],
                "plugins": [],
                "pluginCustomStorage": {},
            }),
        )
        .unwrap();
    store
        .replace_put_presets(&staging, &[json!({ "name": "preset" })])
        .unwrap();
    store
        .replace_put_asset_repository_authority(
            &staging,
            &crate::persistent_store::AssetRepositoryAuthorityState::V2 {
                migration_id: "android-peer-clone-test-assets".to_owned(),
                compatibility_hash: "ab".repeat(32),
            },
        )
        .unwrap();
    store
        .replace_put_cold_payload_authority(
            &staging,
            &crate::persistent_store::ColdPayloadAuthorityState::V2 {
                migration_id: "android-peer-clone-test-cold".to_owned(),
                compatibility_hash: "cd".repeat(32),
            },
        )
        .unwrap();
    store.replace_commit(&staging, Some(0)).unwrap();
}

fn wait_for_no_active_connection(host: &LanCloneHost) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while host.has_active_connection_for_test() {
        assert!(
            Instant::now() < deadline,
            "LAN server kept a finished connection registered"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_active_stalled_connection(host: &LanCloneHost) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !host.has_active_connection_for_test() {
        assert!(
            Instant::now() < deadline,
            "LAN server never registered the stalled connection"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn assert_lan_stop_is_bounded(mut host: LanCloneHost, stalled: TcpStream) {
    // The product property is that stop() never waits on a stalled peer (the 120s
    // RESPONSE_WRITE_TIMEOUT hang); responsiveness itself is governed by the 250ms
    // CONNECTION_READ_POLL_TIMEOUT. Keep the bound far above scheduler noise on loaded
    // Windows runs (a 1s bound flaked at 1.92s in serial runs, and 5s was exceeded once
    // under a fully parallel suite) while staying an order of magnitude below the hang
    // it guards against. A pass proves stop() returned while the stalled socket was
    // still open, because only the timed-out path below drops it.
    const STOP_DEADLINE: Duration = Duration::from_secs(10);
    let (result_tx, result_rx) = mpsc::channel();
    let stopper = thread::spawn(move || {
        let started = Instant::now();
        let result = host.stop();
        let _ = result_tx.send((started.elapsed(), result));
    });

    let outcome = result_rx.recv_timeout(STOP_DEADLINE);
    let stop_waited_for_the_stalled_peer = outcome.is_err();
    if stop_waited_for_the_stalled_peer {
        drop(stalled);
    }
    let (elapsed, result) = outcome.unwrap_or_else(|_| {
        result_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("LAN host did not stop after the stalled peer disconnected")
    });
    stopper.join().unwrap();
    result.unwrap();
    assert!(
        !stop_waited_for_the_stalled_peer,
        "LAN host stop took {elapsed:?} while a peer was stalled"
    );
}

#[test]
fn lan_stop_interrupts_a_peer_stalled_in_an_incomplete_header() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let mut stalled = TcpStream::connect(("127.0.0.1", host.address().unwrap().port())).unwrap();
    write!(
        stalled,
        "GET /v1/sessions/{}/manifest HTTP/1.1\r\nHost: localhost\r\n",
        pairing.session_id
    )
    .unwrap();
    stalled.flush().unwrap();
    wait_for_active_stalled_connection(&host);

    assert_lan_stop_is_bounded(host, stalled);
}

#[test]
fn lan_stop_interrupts_a_peer_stalled_in_an_incomplete_body() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let mut stalled = TcpStream::connect(("127.0.0.1", host.address().unwrap().port())).unwrap();
    write!(
        stalled,
        "POST /v1/sessions/{}/claim HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n{{",
        pairing.session_id
    )
    .unwrap();
    stalled.flush().unwrap();
    wait_for_active_stalled_connection(&host);

    assert_lan_stop_is_bounded(host, stalled);
}

#[test]
fn lan_stop_interrupts_a_range_receiver_that_does_not_read() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[CLONE_CHUNK_SIZE as usize]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
    let claim: Value = Client::new()
        .post(format!(
            "{endpoint}/v1/sessions/{}/claim",
            pairing.session_id
        ))
        .json(&json!({"claim": pairing.claim}))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let bearer = claim["bearer"].as_str().unwrap();
    let object = host.manifest().payloads[0].object.clone();
    // The claim connection must be released before the stalled one can be
    // observed as the registered active connection.
    wait_for_no_active_connection(&host);
    let mut stalled = TcpStream::connect(("127.0.0.1", host.address().unwrap().port())).unwrap();
    write!(
        stalled,
        "GET /v1/sessions/{}/objects/{} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nRange: bytes=0-{}\r\n\r\n",
        pairing.session_id,
        object,
        bearer,
        CLONE_CHUNK_SIZE - 1
    )
    .unwrap();
    stalled.flush().unwrap();
    wait_for_active_stalled_connection(&host);

    assert_lan_stop_is_bounded(host, stalled);
}

#[test]
fn lan_server_rejects_oversized_request_lines_headers_and_bodies() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    host.start().unwrap();
    let address = ("127.0.0.1", host.address().unwrap().port());

    for request in [
        format!(
            "GET /{} HTTP/1.1\r\nHost: localhost\r\n\r\n",
            "x".repeat(600)
        ),
        format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nX-Fill: {}\r\n\r\n",
            "x".repeat(9 * 1024)
        ),
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1025\r\n\r\n".to_owned(),
    ] {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 413 "),
            "unexpected oversized-request response: {response:?}"
        );
    }
    host.stop().unwrap();
}

#[test]
fn lan_server_applies_an_overall_request_deadline_to_trickled_headers() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    host.start().unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", host.address().unwrap().port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let trickle = thread::spawn(move || {
        for byte in b"GET / HTTP/1.1\r\nHost: localhost\r\n" {
            if writer.write_all(&[*byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    trickle.join().unwrap();
    // The trickled request never sends the terminating CRLFCRLF, so a 408 can only come
    // from the overall request deadline firing; the deadline arithmetic itself is proven
    // deterministically by lan_server_rejects_a_header_completed_after_the_overall_deadline
    // and ..._a_body_completed_after_the_overall_deadline via the injected-elapsed seam.
    assert!(
        response.starts_with("HTTP/1.1 408 "),
        "unexpected trickled-request response: {response:?}"
    );
    host.stop().unwrap();
}

#[test]
fn lan_server_tolerates_read_poll_timeouts_within_the_overall_deadline() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64]);
    let mut host = LanCloneHost::prepare(prepare(&source, session_root.path()));
    let pairing = host.start().unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", host.address().unwrap().port())).unwrap();

    thread::sleep(Duration::from_millis(350));
    write!(
        stream,
        "GET /v1/sessions/{}/manifest HTTP/1.1\r\nHost: localhost\r\n\r\n",
        pairing.session_id
    )
    .unwrap();
    stream.flush().unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    assert!(
        response.starts_with("HTTP/1.1 401 "),
        "request inside the overall deadline was rejected: {response:?}"
    );

    let body = json!({ "claim": pairing.claim }).to_string();
    let mut body_stream =
        TcpStream::connect(("127.0.0.1", host.address().unwrap().port())).unwrap();
    write!(
        body_stream,
        "POST /v1/sessions/{}/claim HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
        pairing.session_id,
        body.len()
    )
    .unwrap();
    body_stream.flush().unwrap();
    thread::sleep(Duration::from_millis(350));
    body_stream.write_all(body.as_bytes()).unwrap();
    body_stream.flush().unwrap();
    let mut body_response = String::new();
    body_stream.read_to_string(&mut body_response).unwrap();
    assert!(
        body_response.starts_with("HTTP/1.1 200 "),
        "request body inside the overall deadline was rejected: {body_response:?}"
    );
    host.stop().unwrap();
}

#[test]
fn lan_server_rejects_a_header_completed_after_the_overall_deadline() {
    let (mut reader, elapsed) = scripted_request_reader(vec![(
        b"GET /manifest HTTP/1.1\r\nHost: localhost\r\n\r\n".as_slice(),
        Duration::from_secs(2),
    )]);

    assert_eq!(
        super::lan::read_request_with_elapsed_for_test(
            &mut reader,
            &AtomicBool::new(false),
            || *elapsed.lock().unwrap(),
        ),
        Err(408)
    );
}

#[test]
fn lan_server_rejects_a_body_completed_after_the_overall_deadline() {
    let (mut reader, elapsed) = scripted_request_reader(vec![
        (
            b"POST /claim HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n".as_slice(),
            Duration::ZERO,
        ),
        (b"{}".as_slice(), Duration::from_secs(2)),
    ]);

    assert_eq!(
        super::lan::read_request_with_elapsed_for_test(
            &mut reader,
            &AtomicBool::new(false),
            || *elapsed.lock().unwrap(),
        ),
        Err(408)
    );
}

struct ScriptedRequestReader {
    reads: std::vec::IntoIter<(Vec<u8>, Duration)>,
    elapsed: Arc<Mutex<Duration>>,
}

impl Read for ScriptedRequestReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let Some((bytes, elapsed)) = self.reads.next() else {
            return Ok(0);
        };
        assert!(bytes.len() <= buffer.len());
        buffer[..bytes.len()].copy_from_slice(&bytes);
        *self.elapsed.lock().unwrap() = elapsed;
        Ok(bytes.len())
    }
}

fn scripted_request_reader(
    reads: Vec<(&[u8], Duration)>,
) -> (ScriptedRequestReader, Arc<Mutex<Duration>>) {
    let elapsed = Arc::new(Mutex::new(Duration::ZERO));
    (
        ScriptedRequestReader {
            reads: reads
                .into_iter()
                .map(|(bytes, elapsed)| (bytes.to_vec(), elapsed))
                .collect::<Vec<_>>()
                .into_iter(),
            elapsed: Arc::clone(&elapsed),
        },
        elapsed,
    )
}

#[test]
fn lan_client_bounds_an_incomplete_oversized_claim_response() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (hold_tx, hold_rx) = mpsc::channel::<()>();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 2048];
        let _ = stream.read(&mut request).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000000\r\n\r\n{}",
            "x".repeat(1025)
        )
        .unwrap();
        stream.flush().unwrap();
        // Hold the socket open until the main thread has observed the claim error, so
        // the error provably occurred while the server still held the incomplete
        // oversized response, independent of scheduling.
        let _ = hold_rx.recv();
    });
    let endpoint = format!("http://{address}");
    let session_id = uuid::Uuid::new_v4().to_string();
    let claim = "a".repeat(64);
    let (result_tx, result_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = result_tx.send(LanCloneClient::claim(&endpoint, &session_id, &claim));
    });

    // Generous hang guard only; a client that waited for the oversized body would fail
    // the Protocol match below with the CONTROL_REQUEST_TIMEOUT transport error instead.
    let result = result_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("claim response reader waited for an oversized response to finish");
    assert!(matches!(result, Err(PeerSyncError::Protocol(_))));
    let _ = hold_tx.send(());
    server.join().unwrap();
    worker.join().unwrap();
}

fn assert_file_hash(path: &Path, expected: &str) {
    let bytes = fs::read(path).unwrap();
    assert_eq!(hex::encode(Sha256::digest(bytes)), expected);
}

#[test]
fn freezes_a_canonical_manifest_and_releases_the_revision_before_serving() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[CLONE_CHUNK_SIZE as usize + 17]);

    let session = prepare(&source, session_root.path());

    assert!(source.released.load(Ordering::SeqCst));
    assert_eq!(session.manifest().source_revision, 42);
    assert_eq!(session.manifest().chunk_size, CLONE_CHUNK_SIZE);
    let manifest_value: Value = serde_json::from_slice(session.manifest_bytes()).unwrap();
    let created_at = manifest_value["createdAt"].as_str().unwrap();
    assert!(created_at.contains('T') && created_at.ends_with('Z'));
    assert_eq!(manifest_value["database"]["format"], "risusave-v1");
    assert_eq!(
        manifest_value["database"]["object"],
        session.manifest().database.object
    );
    let object = &session.manifest().objects[&payload_hash(&session, 0)];
    assert_eq!(object.chunks.len(), 2);
    assert_eq!(object.chunks[0].size, CLONE_CHUNK_SIZE);
    assert_eq!(object.chunks[1].size, 17);
    assert_eq!(
        session.manifest_bytes(),
        session.manifest().canonical_bytes().unwrap()
    );
    assert_eq!(
        session.manifest_id(),
        hex::encode(Sha256::digest(session.manifest_bytes()))
    );

    let manifest_file = fs::read(session_root.path().join("manifest.json")).unwrap();
    assert_eq!(manifest_file, session.manifest_bytes());

    let frozen_hash = payload_hash(&session, 0);
    fs::write(&source.objects[1].path, b"later live-source mutation").unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let host = LoopbackCloneHost::start(session).unwrap();
    let mut client = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    client.download(&TransferCancellation::new()).unwrap();
    assert_file_hash(
        &client.verified_object_path(&frozen_hash).unwrap(),
        &frozen_hash,
    );
}

#[test]
fn serves_head_and_only_exact_single_chunk_ranges_on_loopback() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[CLONE_CHUNK_SIZE as usize + 31]);
    let session = prepare(&source, session_root.path());
    let hash = payload_hash(&session, 0);
    let host = LoopbackCloneHost::start(session).unwrap();
    assert!(host.address().ip().is_loopback());

    let client = Client::new();
    let object_url = format!("{}/objects/{hash}", host.session_url());
    let head = client.head(&object_url).send().unwrap();
    assert_eq!(head.status().as_u16(), 200);
    assert_eq!(
        head.headers()["content-length"],
        (CLONE_CHUNK_SIZE + 31).to_string()
    );
    assert_eq!(head.headers()["accept-ranges"], "bytes");

    let first = client
        .get(&object_url)
        .header("range", format!("bytes=0-{}", CLONE_CHUNK_SIZE - 1))
        .send()
        .unwrap();
    assert_eq!(first.status().as_u16(), 206);
    assert_eq!(first.content_length(), Some(CLONE_CHUNK_SIZE));
    assert_eq!(first.bytes().unwrap().len() as u64, CLONE_CHUNK_SIZE);

    for range in [
        None,
        Some("bytes=0-0"),
        Some("bytes=0-1,4-5"),
        Some("bytes=8388608-8388640"),
    ] {
        let mut request = client.get(&object_url);
        if let Some(range) = range {
            request = request.header("range", range);
        }
        assert_eq!(request.send().unwrap().status().as_u16(), 416);
    }
}

#[test]
fn refuses_nonloopback_session_urls_and_never_follows_http_redirects() {
    let staging_root = tempfile::tempdir().unwrap();
    for url in [
        "http://localhost:1234/v1/sessions/test",
        "http://192.0.2.1:1234/v1/sessions/test",
    ] {
        assert!(matches!(
            LoopbackCloneClient::new(staging_root.path(), url),
            Err(PeerSyncError::Protocol(_))
        ));
    }

    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64 * 1024]);
    let session = prepare(&source, session_root.path());
    let host = LoopbackCloneHost::start(session).unwrap();
    host.redirect_manifest_once_for_test(format!("http://{}/redirect-target", host.address()));
    let mut client = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();

    assert!(matches!(
        client.download(&TransferCancellation::new()),
        Err(PeerSyncError::Transport(message)) if message.contains("302")
    ));
}

#[test]
fn loopback_child_download_ignores_proxy_environment() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[64 * 1024]);
    let session = prepare(&source, session_root.path());
    let host = LoopbackCloneHost::start(session).unwrap();

    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let proxy_address = proxy.local_addr().unwrap();
    let proxy_hit = Arc::new(AtomicBool::new(false));
    let stop_proxy = Arc::new(AtomicBool::new(false));
    let thread_hit = Arc::clone(&proxy_hit);
    let thread_stop = Arc::clone(&stop_proxy);
    let proxy_thread = thread::spawn(move || {
        while !thread_stop.load(Ordering::SeqCst) {
            match proxy.accept() {
                Ok((mut stream, _)) => {
                    thread_hit.store(true, Ordering::SeqCst);
                    let mut request = [0_u8; 4096];
                    let _ = stream.read(&mut request);
                    let _ = stream.write_all(
                        b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("proxy probe failed: {error}"),
            }
        }
    });

    let proxy_url = format!("http://{proxy_address}");
    let output = clone_child_command("proxy", &host.session_url(), client_root.path())
        .env("HTTP_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("HTTPS_PROXY", &proxy_url)
        .env("https_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .output()
        .unwrap();
    stop_proxy.store(true, Ordering::SeqCst);
    proxy_thread.join().unwrap();

    assert!(
        output.status.success(),
        "child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!proxy_hit.load(Ordering::SeqCst));
}

#[test]
fn resumes_from_persisted_verified_offsets_after_disconnect_without_redownload() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[(CLONE_CHUNK_SIZE * 2 + 97) as usize]);
    let session = prepare(&source, session_root.path());
    let hash = payload_hash(&session, 0);
    let host = LoopbackCloneHost::start(session).unwrap();
    host.disconnect_once(&hash, CLONE_CHUNK_SIZE, 64 * 1024);

    let mut first = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    assert!(matches!(
        first.download(&TransferCancellation::new()),
        Err(PeerSyncError::Transport(_))
    ));
    assert_eq!(first.verified_chunk_count(&hash), 1);
    assert_eq!(host.range_request_count(&hash, 0), 1);
    OpenOptions::new()
        .append(true)
        .open(client_root.path().join("ledger.jsonl"))
        .unwrap()
        .write_all(b"{\"kind\":\"object\"")
        .unwrap();

    let mut restarted = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    let report = restarted.download(&TransferCancellation::new()).unwrap();
    assert_eq!(report.verified_objects, 2);
    assert_eq!(host.range_request_count(&hash, 0), 1);
    assert_eq!(host.range_request_count(&hash, CLONE_CHUNK_SIZE), 2);
    assert_eq!(host.range_request_count(&hash, CLONE_CHUNK_SIZE * 2), 1);
    assert_file_hash(&restarted.verified_object_path(&hash).unwrap(), &hash);
}

#[test]
fn never_requests_an_already_verified_object_after_process_style_restart() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(
        source_root.path(),
        &[64 * 1024, (CLONE_CHUNK_SIZE + 41) as usize],
    );
    let session = prepare(&source, session_root.path());
    let first_hash = payload_hash(&session, 0);
    let second_hash = payload_hash(&session, 1);
    let host = LoopbackCloneHost::start(session).unwrap();
    host.disconnect_once(&second_hash, 0, 32 * 1024);

    let mut first = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    assert!(first.download(&TransferCancellation::new()).is_err());
    let first_requests = host.total_range_requests(&first_hash);
    assert_eq!(first_requests, 1);

    let mut restarted = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    restarted.download(&TransferCancellation::new()).unwrap();
    assert_eq!(host.total_range_requests(&first_hash), first_requests);
}

#[test]
fn reopens_promoted_cas_object_without_redownload_before_verified_ledger_record() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[128 * 1024]);
    let session = prepare(&source, session_root.path());
    let hash = payload_hash(&session, 0);
    let host = LoopbackCloneHost::start(session).unwrap();
    let mut interrupted = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    interrupted.fail_after_cas_promotion_once_for_test();

    assert!(matches!(
        interrupted.download(&TransferCancellation::new()),
        Err(PeerSyncError::Storage(message))
            if message == "injected crash after CAS promotion"
    ));
    assert_eq!(host.total_range_requests(&hash), 1);
    drop(interrupted);

    let mut reopened = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    reopened.download(&TransferCancellation::new()).unwrap();
    assert_eq!(host.total_range_requests(&hash), 1);
    assert_file_hash(&reopened.verified_object_path(&hash).unwrap(), &hash);
}

#[test]
fn cancels_local_verification_and_reconciles_corrupt_verified_progress() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(
        source_root.path(),
        &[(CLONE_CHUNK_SIZE + 64 * 1024) as usize],
    );
    let session = prepare(&source, session_root.path());
    let hash = payload_hash(&session, 0);
    let host = LoopbackCloneHost::start(session).unwrap();
    let mut client = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    client.download(&TransferCancellation::new()).unwrap();
    let (verified_before, total_bytes) = client.transfer_progress().unwrap();
    assert_eq!(verified_before, total_bytes);
    let object_path = client.verified_object_path(&hash).unwrap();
    let object_size = fs::metadata(&object_path).unwrap().len();
    fs::write(&object_path, vec![b'x'; object_size as usize]).unwrap();

    let cancellation = TransferCancellation::new();
    let pause = Arc::new(Barrier::new(2));
    cancellation.pause_after_hash_read_for_test(Arc::clone(&pause));
    let worker_cancellation = cancellation.clone();
    let verification = thread::spawn(move || {
        let result = client.all_objects_verified(&worker_cancellation);
        (client, result)
    });
    pause.wait();
    cancellation.cancel();
    pause.wait();
    let (mut client, result) = verification.join().unwrap();
    assert_eq!(result.unwrap_err(), PeerSyncError::Cancelled);

    assert!(!client
        .all_objects_verified(&TransferCancellation::new())
        .unwrap());
    let (verified_after, recomputed_total) = client.transfer_progress().unwrap();
    assert_eq!(recomputed_total, total_bytes);
    assert!(verified_after < total_bytes);

    let requests_before = host.total_range_requests(&hash);
    let cancellation = TransferCancellation::new();
    let cas_pause = Arc::new(Barrier::new(2));
    cancellation.pause_after_cas_read_for_test(Arc::clone(&cas_pause));
    let worker_cancellation = cancellation.clone();
    let promotion = thread::spawn(move || {
        let result = client.download(&worker_cancellation);
        (client, result)
    });
    cas_pause.wait();
    cancellation.cancel();
    cas_pause.wait();
    let (mut client, result) = promotion.join().unwrap();
    assert_eq!(result.unwrap_err(), PeerSyncError::Cancelled);
    assert!(fs::read_dir(client_root.path().join("assets-v2/staging"))
        .unwrap()
        .next()
        .is_none());

    client.download(&TransferCancellation::new()).unwrap();
    assert!(host.total_range_requests(&hash) > requests_before);
    assert_eq!(
        client.transfer_progress().unwrap(),
        (total_bytes, total_bytes)
    );
}

#[test]
fn resumes_after_actual_child_process_kill_in_the_middle_of_a_chunk() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[(CLONE_CHUNK_SIZE * 2 + 97) as usize]);
    let session = prepare(&source, session_root.path());
    let hash = payload_hash(&session, 0);
    let host = LoopbackCloneHost::start(session).unwrap();
    let marker = client_root.path().join("mid-chunk.kill-ready");
    let mut child = spawn_clone_kill_child(
        "mid-chunk-kill",
        &host.session_url(),
        client_root.path(),
        &marker,
    );

    wait_for_marker_and_kill(&mut child, &marker);

    let mut reopened = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    assert_eq!(reopened.verified_chunk_count(&hash), 1);
    reopened.download(&TransferCancellation::new()).unwrap();
    assert_eq!(host.range_request_count(&hash, 0), 1);
    assert_eq!(host.range_request_count(&hash, CLONE_CHUNK_SIZE), 2);
    assert_eq!(host.range_request_count(&hash, CLONE_CHUNK_SIZE * 2), 1);
    assert_file_hash(&reopened.verified_object_path(&hash).unwrap(), &hash);
}

#[test]
fn recognizes_promoted_cas_object_after_actual_child_process_kill_before_ledger() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[128 * 1024]);
    let session = prepare(&source, session_root.path());
    let hash = payload_hash(&session, 0);
    let host = LoopbackCloneHost::start(session).unwrap();
    let marker = client_root.path().join("cas-promotion.kill-ready");
    let mut child = spawn_clone_kill_child(
        "cas-promotion-kill",
        &host.session_url(),
        client_root.path(),
        &marker,
    );

    wait_for_marker_and_kill(&mut child, &marker);
    assert_eq!(host.total_range_requests(&hash), 1);

    let mut reopened = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    reopened.download(&TransferCancellation::new()).unwrap();
    assert_eq!(host.total_range_requests(&hash), 1);
    assert_file_hash(&reopened.verified_object_path(&hash).unwrap(), &hash);
}

#[test]
fn rejects_corrupt_chunks_and_whole_objects_before_target_staging() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[128 * 1024]);
    let session = prepare(&source, session_root.path());
    let hash = payload_hash(&session, 0);
    let host = LoopbackCloneHost::start(session).unwrap();
    host.corrupt_once(&hash, 0);
    let mut client = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    assert_eq!(
        client.download(&TransferCancellation::new()).unwrap_err(),
        PeerSyncError::ChunkHashMismatch {
            object: hash.clone(),
            offset: 0,
        }
    );
    assert_eq!(client.verified_chunk_count(&hash), 0);

    let whole_session_root = tempfile::tempdir().unwrap();
    let whole_client_root = tempfile::tempdir().unwrap();
    let mut whole_session = prepare(&source, whole_session_root.path());
    let advertised = whole_session.force_whole_hash_mismatch_for_test(0).unwrap();
    let whole_host = LoopbackCloneHost::start(whole_session).unwrap();
    let mut whole_client =
        LoopbackCloneClient::new(whole_client_root.path(), whole_host.session_url()).unwrap();
    assert_eq!(
        whole_client
            .download(&TransferCancellation::new())
            .unwrap_err(),
        PeerSyncError::WholeObjectHashMismatch {
            object: advertised.clone(),
        }
    );
    assert_eq!(whole_client.verified_chunk_count(&advertised), 0);
}

#[test]
fn cancellation_keeps_only_verified_boundaries_and_can_resume() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[(CLONE_CHUNK_SIZE + 3) as usize]);
    let session = prepare(&source, session_root.path());
    let hash = payload_hash(&session, 0);
    let host = LoopbackCloneHost::start(session).unwrap();
    let cancellation = TransferCancellation::new();
    let cancellation_for_progress = cancellation.clone();
    let mut client = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();

    let result = client.download_with_progress(&cancellation, move |bytes| {
        if bytes >= 64 * 1024 {
            cancellation_for_progress.cancel();
        }
    });
    assert_eq!(result.unwrap_err(), PeerSyncError::Cancelled);
    assert_eq!(client.verified_chunk_count(&hash), 0);

    let mut restarted = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    restarted.download(&TransferCancellation::new()).unwrap();
    assert_eq!(restarted.verified_chunk_count(&hash), 2);
}

#[test]
fn refuses_a_different_immutable_manifest_in_an_existing_checkpoint() {
    let source_a_root = tempfile::tempdir().unwrap();
    let source_b_root = tempfile::tempdir().unwrap();
    let session_a_root = tempfile::tempdir().unwrap();
    let session_b_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source_a = fixture_source(source_a_root.path(), &[64 * 1024]);
    let source_b = fixture_source(source_b_root.path(), &[64 * 1024 + 1]);
    let session_a = prepare(&source_a, session_a_root.path());
    let session_b = prepare(&source_b, session_b_root.path());
    let manifest_a = session_a.manifest_id().to_owned();
    let manifest_b = session_b.manifest_id().to_owned();
    assert_ne!(manifest_a, manifest_b);
    let host_a = LoopbackCloneHost::start(session_a).unwrap();
    host_a.disconnect_once(&payload_hash_from_host(&host_a, 0), 0, 1024);
    let mut first = LoopbackCloneClient::new(client_root.path(), host_a.session_url()).unwrap();
    assert!(first.download(&TransferCancellation::new()).is_err());
    host_a.shutdown().unwrap();

    let host_b = LoopbackCloneHost::start(session_b).unwrap();
    let mut second = LoopbackCloneClient::new(client_root.path(), host_b.session_url()).unwrap();
    assert_eq!(
        second.download(&TransferCancellation::new()).unwrap_err(),
        PeerSyncError::StaleManifest {
            expected: manifest_a,
            received: manifest_b,
        }
    );
}

fn payload_hash_from_host(host: &LoopbackCloneHost, index: usize) -> String {
    host.manifest().payloads[index].object.clone()
}

#[test]
fn validates_staged_graph_and_hashes_before_one_atomic_activation() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[96 * 1024]);
    let session = prepare(&source, session_root.path());
    let manifest_id = session.manifest_id().to_owned();
    let host = LoopbackCloneHost::start(session).unwrap();
    let mut client = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    client.download(&TransferCancellation::new()).unwrap();
    let mut target = FixtureTarget {
        active_manifest: "old".to_owned(),
        ..FixtureTarget::default()
    };
    let validation_calls = Arc::new(AtomicUsize::new(0));
    let failing_calls = Arc::clone(&validation_calls);
    let mut failing_validator = move |_manifest: &CloneManifest, stage: &FixtureStage| {
        failing_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(stage.staged.last().unwrap().kind, CloneObjectKind::Database);
        Err(PeerSyncError::Validation(
            "injected graph mismatch".to_owned(),
        ))
    };

    assert!(matches!(
        activate_downloaded_clone(&mut client, &mut target, &mut failing_validator),
        Err(PeerSyncError::Validation(_))
    ));
    assert_eq!(target.active_manifest, "old");
    assert_eq!(target.activation_count, 0);
    assert_eq!(target.abort_count, 1);
    assert_eq!(validation_calls.load(Ordering::SeqCst), 1);

    let mut validator = |manifest: &CloneManifest, stage: &FixtureStage| {
        assert_eq!(stage.staged.len(), manifest.payloads.len() + 1);
        for staged in &stage.staged {
            let expected = if staged.kind == CloneObjectKind::Database {
                assert_eq!(staged.logical_key, "database");
                &manifest.database.object
            } else {
                &manifest
                    .payloads
                    .iter()
                    .find(|payload| {
                        payload.kind == staged.kind && payload.logical_key == staged.logical_key
                    })
                    .unwrap()
                    .object
            };
            assert_eq!(staged.sha256, *expected);
            assert_eq!(
                staged.byte_size, manifest.objects[expected].size,
                "bounded target sink must consume the complete object"
            );
        }
        Ok(())
    };
    activate_downloaded_clone(&mut client, &mut target, &mut validator).unwrap();
    assert_eq!(target.active_manifest, manifest_id);
    assert_eq!(target.activation_count, 1);
    assert_eq!(
        activate_downloaded_clone(&mut client, &mut target, &mut validator).unwrap_err(),
        PeerSyncError::AlreadyActivated
    );
    assert_eq!(target.activation_count, 1);
}

#[test]
fn persisted_manifest_allows_offline_activation_after_client_restart() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[96 * 1024]);
    let session = prepare(&source, session_root.path());
    let manifest_id = session.manifest_id().to_owned();
    let host = LoopbackCloneHost::start(session).unwrap();
    let session_url = host.session_url().to_owned();
    let mut client = LoopbackCloneClient::new(client_root.path(), &session_url).unwrap();
    client.download(&TransferCancellation::new()).unwrap();
    drop(client);
    host.shutdown().unwrap();
    let mut reopened = LoopbackCloneClient::new(client_root.path(), session_url).unwrap();
    let mut target = FixtureTarget {
        active_manifest: "old".to_owned(),
        ..FixtureTarget::default()
    };
    let mut validator = |_manifest: &CloneManifest, _stage: &FixtureStage| Ok(());

    activate_downloaded_clone(&mut reopened, &mut target, &mut validator).unwrap();

    assert_eq!(target.active_manifest, manifest_id);
    assert_eq!(target.activation_count, 1);
}

#[test]
fn corrupt_or_mismatched_persisted_manifest_never_stages_activation() {
    let source_a_root = tempfile::tempdir().unwrap();
    let source_b_root = tempfile::tempdir().unwrap();
    let session_a_root = tempfile::tempdir().unwrap();
    let session_b_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source_a = fixture_source(source_a_root.path(), &[96 * 1024]);
    let source_b = fixture_source(source_b_root.path(), &[96 * 1024 + 1]);
    let session_a = prepare(&source_a, session_a_root.path());
    let session_b = prepare(&source_b, session_b_root.path());
    let manifest_a = session_a.manifest_id().to_owned();
    let manifest_b = session_b.manifest_id().to_owned();
    let manifest_b_bytes = session_b.manifest_bytes().to_vec();
    let host = LoopbackCloneHost::start(session_a).unwrap();
    let session_url = host.session_url().to_owned();
    let mut client = LoopbackCloneClient::new(client_root.path(), &session_url).unwrap();
    client.download(&TransferCancellation::new()).unwrap();
    drop(client);
    host.shutdown().unwrap();
    let manifest_path = client_root.path().join("manifest.json");
    let mut target = FixtureTarget {
        active_manifest: "old".to_owned(),
        ..FixtureTarget::default()
    };
    let mut validator = |_manifest: &CloneManifest, _stage: &FixtureStage| Ok(());

    fs::write(&manifest_path, b"{").unwrap();
    let mut corrupt = LoopbackCloneClient::new(client_root.path(), &session_url).unwrap();
    assert!(matches!(
        activate_downloaded_clone(&mut corrupt, &mut target, &mut validator),
        Err(PeerSyncError::Storage(_))
    ));
    assert_eq!(target.active_manifest, "old");
    assert_eq!(target.activation_count, 0);
    assert_eq!(target.stage_count, 0);

    File::create(&manifest_path)
        .unwrap()
        .set_len(super::protocol::MAX_MANIFEST_BYTES as u64 + 1)
        .unwrap();
    let mut oversized = LoopbackCloneClient::new(client_root.path(), &session_url).unwrap();
    assert!(matches!(
        activate_downloaded_clone(&mut oversized, &mut target, &mut validator),
        Err(PeerSyncError::Storage(_))
    ));
    assert_eq!(target.active_manifest, "old");
    assert_eq!(target.activation_count, 0);
    assert_eq!(target.stage_count, 0);

    fs::write(&manifest_path, manifest_b_bytes).unwrap();
    let mut mismatched = LoopbackCloneClient::new(client_root.path(), session_url).unwrap();
    assert_eq!(
        activate_downloaded_clone(&mut mismatched, &mut target, &mut validator).unwrap_err(),
        PeerSyncError::StaleManifest {
            expected: manifest_a,
            received: manifest_b,
        }
    );
    assert_eq!(target.active_manifest, "old");
    assert_eq!(target.activation_count, 0);
    assert_eq!(target.stage_count, 0);
}

#[test]
fn concurrent_clone_jobs_use_one_atomic_idempotent_manifest_activation() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_a_root = tempfile::tempdir().unwrap();
    let client_b_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[96 * 1024]);
    let session = prepare(&source, session_root.path());
    let manifest_id = session.manifest_id().to_owned();
    let host = LoopbackCloneHost::start(session).unwrap();
    let mut client_a = LoopbackCloneClient::new(client_a_root.path(), host.session_url()).unwrap();
    let mut client_b = LoopbackCloneClient::new(client_b_root.path(), host.session_url()).unwrap();
    client_a.download(&TransferCancellation::new()).unwrap();
    client_b.download(&TransferCancellation::new()).unwrap();

    let target = ConcurrentFixtureTarget::new("old");
    let barrier = Arc::new(Barrier::new(2));
    let spawn_job = |mut client: LoopbackCloneClient,
                     mut target: ConcurrentFixtureTarget,
                     barrier: Arc<Barrier>| {
        thread::spawn(move || {
            let mut validator = move |_manifest: &CloneManifest, _stage: &FixtureStage| {
                barrier.wait();
                Ok(())
            };
            activate_downloaded_clone(&mut client, &mut target, &mut validator)
        })
    };
    let job_a = spawn_job(client_a, target.clone(), Arc::clone(&barrier));
    let job_b = spawn_job(client_b, target.clone(), Arc::clone(&barrier));
    let results = [job_a.join().unwrap(), job_b.join().unwrap()];

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(PeerSyncError::AlreadyActivated)))
            .count(),
        1
    );
    let state = target.state.lock().unwrap();
    assert_eq!(state.active_manifest.as_deref(), Some(manifest_id.as_str()));
    assert_eq!(state.activation_count, 1);
    assert_eq!(state.abort_count, 1);
}

#[test]
fn preserves_asset_inlay_and_cold_metadata_in_the_validated_bounded_stage() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let mut source = fixture_source(source_root.path(), &[96 * 1024]);
    source.objects[1].metadata = json!({
        "mime": "image/png",
        "nested": { "width": 4096, "flags": [true, false, null] }
    });
    let inlay = source_root.path().join("inlay.webp");
    let cold = source_root.path().join("cold.bin");
    write_pattern_file(&inlay, 80 * 1024, 51);
    write_pattern_file(&cold, 72 * 1024, 73);
    source.objects.push(PinnedSourceObject::payload(
        CloneObjectKind::Inlay,
        "inlays/original.webp",
        json!({ "mime": "image/webp", "animated": false, "quality": 91 }),
        inlay,
    ));
    source.objects.push(PinnedSourceObject::payload(
        CloneObjectKind::Cold,
        "cold/plugin-state",
        json!({ "owner": "plugin:test", "version": 7, "tags": ["a", "b"] }),
        cold,
    ));
    let session = prepare(&source, session_root.path());
    let host = LoopbackCloneHost::start(session).unwrap();
    let mut client = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    client.download(&TransferCancellation::new()).unwrap();
    let mut target = FixtureTarget {
        active_manifest: "old".to_owned(),
        ..FixtureTarget::default()
    };
    let validated = Arc::new(AtomicBool::new(false));
    let validated_view = Arc::clone(&validated);
    let mut validator = move |manifest: &CloneManifest, stage: &FixtureStage| {
        assert_eq!(stage.maximum_buffer_bytes, 32 * 1024);
        assert_eq!(stage.staged.len(), 4);
        for payload in &manifest.payloads {
            let staged = stage
                .staged
                .iter()
                .find(|staged| {
                    staged.kind == payload.kind && staged.logical_key == payload.logical_key
                })
                .unwrap();
            assert_eq!(staged.metadata, payload.metadata);
            assert_eq!(staged.sha256, payload.object);
        }
        let database = stage.staged.last().unwrap();
        assert_eq!(database.kind, CloneObjectKind::Database);
        assert_eq!(database.metadata, Value::Null);
        validated_view.store(true, Ordering::SeqCst);
        Ok(())
    };

    activate_downloaded_clone(&mut client, &mut target, &mut validator).unwrap();
    assert!(validated.load(Ordering::SeqCst));
}

#[test]
fn streams_with_a_bounded_buffer_and_stops_the_server_completely() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let client_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[(CLONE_CHUNK_SIZE * 2 + 123) as usize]);
    let session = prepare(&source, session_root.path());
    let host = LoopbackCloneHost::start(session).unwrap();
    let address = host.address();
    let mut client = LoopbackCloneClient::new(client_root.path(), host.session_url()).unwrap();
    let report = client.download(&TransferCancellation::new()).unwrap();
    assert!(report.maximum_buffer_bytes <= 64 * 1024);
    assert!(report.maximum_response_bytes <= CLONE_CHUNK_SIZE);
    host.shutdown().unwrap();
    assert!(TcpStream::connect(address).is_err());
}

#[test]
fn manifest_validation_rejects_noncanonical_or_malformed_object_graphs() {
    let source_root = tempfile::tempdir().unwrap();
    let session_root = tempfile::tempdir().unwrap();
    let source = fixture_source(source_root.path(), &[32]);
    let session = prepare(&source, session_root.path());
    let mut value: Value = serde_json::from_slice(session.manifest_bytes()).unwrap();
    value["chunkSize"] = json!(1024);
    let malformed: CloneManifest = serde_json::from_value(value).unwrap();
    assert!(matches!(
        malformed.validate(),
        Err(PeerSyncError::Protocol(_))
    ));

    for (field, invalid) in [
        ("createdAt", json!("not-rfc3339")),
        ("database.format", json!("sqlite-live-file")),
    ] {
        let mut value: Value = serde_json::from_slice(session.manifest_bytes()).unwrap();
        if field == "createdAt" {
            value["createdAt"] = invalid;
        } else {
            value["database"]["format"] = invalid;
        }
        let malformed: CloneManifest = serde_json::from_value(value).unwrap();
        assert!(
            matches!(malformed.validate(), Err(PeerSyncError::Protocol(_))),
            "manifest must reject {field}"
        );
    }
}
