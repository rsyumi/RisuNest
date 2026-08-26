use super::*;
use reqwest::blocking::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
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

fn assert_lan_stop_is_bounded(mut host: LanCloneHost, stalled: TcpStream) {
    const STOP_DEADLINE: Duration = Duration::from_secs(1);
    let (result_tx, result_rx) = mpsc::channel();
    let stopper = thread::spawn(move || {
        let started = Instant::now();
        let result = host.stop();
        let _ = result_tx.send((started.elapsed(), result));
    });

    let outcome = result_rx.recv_timeout(STOP_DEADLINE);
    if outcome.is_err() {
        drop(stalled);
    }
    let (elapsed, result) = outcome.unwrap_or_else(|_| {
        result_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("LAN host did not stop after the stalled peer disconnected")
    });
    stopper.join().unwrap();
    result.unwrap();
    assert!(
        elapsed <= STOP_DEADLINE,
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
    thread::sleep(Duration::from_millis(50));

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
    thread::sleep(Duration::from_millis(50));

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
    thread::sleep(Duration::from_millis(100));

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

    let started = Instant::now();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let response_elapsed = started.elapsed();
    trickle.join().unwrap();
    assert!(
        response.starts_with("HTTP/1.1 408 "),
        "unexpected trickled-request response: {response:?}"
    );
    assert!(
        response_elapsed < Duration::from_secs(3),
        "trickled request exceeded the overall deadline"
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
        thread::sleep(Duration::from_secs(1));
    });
    let endpoint = format!("http://{address}");
    let session_id = uuid::Uuid::new_v4().to_string();
    let claim = "a".repeat(64);
    let (result_tx, result_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = result_tx.send(LanCloneClient::claim(&endpoint, &session_id, &claim));
    });

    let result = result_rx
        .recv_timeout(Duration::from_millis(750))
        .expect("claim response reader waited for an oversized response to finish");
    assert!(matches!(result, Err(PeerSyncError::Protocol(_))));
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
