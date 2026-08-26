use super::*;
use reqwest::blocking::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::TcpStream,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

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
    staged: Vec<(CloneObjectKind, String, Vec<u8>)>,
}

impl CloneTargetAdapter for FixtureTarget {
    type Stage = FixtureStage;

    fn is_active(&self, manifest_id: &str) -> Result<bool, PeerSyncError> {
        Ok(self.active_manifest == manifest_id)
    }

    fn begin(&mut self, manifest_id: &str) -> Result<Self::Stage, PeerSyncError> {
        self.stage_count += 1;
        Ok(FixtureStage {
            manifest_id: manifest_id.to_owned(),
            staged: Vec::new(),
        })
    }

    fn stage_object(
        &mut self,
        stage: &mut Self::Stage,
        kind: CloneObjectKind,
        logical_key: &str,
        reader: &mut dyn Read,
    ) -> Result<(), PeerSyncError> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        stage.staged.push((kind, logical_key.to_owned(), bytes));
        Ok(())
    }

    fn abort(&mut self, _stage: Self::Stage) -> Result<(), PeerSyncError> {
        self.abort_count += 1;
        Ok(())
    }

    fn activate(&mut self, stage: &mut Self::Stage) -> Result<(), PeerSyncError> {
        assert_eq!(self.active_manifest, "old");
        self.active_manifest = stage.manifest_id.clone();
        self.activation_count += 1;
        Ok(())
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
        assert_eq!(stage.staged.last().unwrap().0, CloneObjectKind::Database);
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
        for (kind, logical_key, bytes) in &stage.staged {
            let expected = if *kind == CloneObjectKind::Database {
                assert_eq!(logical_key, "database");
                &manifest.database
            } else {
                &manifest
                    .payloads
                    .iter()
                    .find(|payload| payload.kind == *kind && payload.logical_key == *logical_key)
                    .unwrap()
                    .object
            };
            assert_eq!(hex::encode(Sha256::digest(bytes)), *expected);
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
}
