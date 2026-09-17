use super::*;
use crate::asset_repository::job_pins::DurableCasJob;
use crate::server_sync::{backups::references, cache::Cache, client::ServerClient, transfer::Transfer};
use std::sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Mutex};

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    body: Value,
}
#[derive(Default)]
struct Trace {
    armed: AtomicBool,
    corrupt_retention: AtomicBool,
    expire_checkpoint: AtomicBool,
    released_before_marker: AtomicBool,
    root: Mutex<Option<std::path::PathBuf>>,
    requests: Mutex<Vec<Request>>,
}

async fn observe(
    axum::extract::State(trace): axum::extract::State<Arc<Trace>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::{body::{to_bytes, Body}, response::IntoResponse};
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 16 * 1024 * 1024).await.unwrap();
    let armed = trace.armed.load(Ordering::SeqCst);
    if armed {
        trace.requests.lock().unwrap().push(Request {
            method: method.clone(), path: path.clone(),
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        });
        if method == "DELETE" && path.starts_with("/checkpoints/") {
            let root = trace.root.lock().unwrap().clone().unwrap();
            let complete = fs::read_dir(root.join("server-sync/backups")).ok().is_some_and(|entries|
                entries.filter_map(std::result::Result::ok).any(|entry| entry.path().join("complete.json").is_file()));
            if !complete { trace.released_before_marker.store(true, Ordering::SeqCst); }
        }
        if method == "GET" && path.starts_with("/checkpoints/") && trace.expire_checkpoint.load(Ordering::SeqCst) {
            return (axum::http::StatusCode::GONE, axum::Json(json!({"error":"checkpoint-expired"}))).into_response();
        }
    }
    let response = next.run(axum::extract::Request::from_parts(parts, Body::from(bytes))).await;
    if armed && method == "POST" && path == "/objects/retention"
        && trace.corrupt_retention.load(Ordering::SeqCst) && response.status().is_success() {
        let (mut parts, body) = response.into_parts();
        let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
        let mut retained: Value = serde_json::from_slice(&bytes).unwrap();
        let first = &mut retained.as_array_mut().unwrap()[0];
        let size = first["size"].as_str().unwrap().parse::<u64>().unwrap();
        first["size"] = json!((size + 1).to_string());
        parts.headers.remove(axum::http::header::CONTENT_LENGTH);
        return axum::response::Response::from_parts(parts, Body::from(serde_json::to_vec(&retained).unwrap()));
    }
    response
}

fn measured_fixture() -> (Fixture, Arc<Trace>) {
    let root = tempfile::tempdir().unwrap();
    let server = Arc::new(Store::init(root.path()).unwrap());
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let trace = Arc::new(Trace::default());
    let remote = server.clone();
    let observed = trace.clone();
    let task = runtime.spawn(async move {
        let router = http::router(remote).layer(axum::middleware::from_fn_with_state(observed, observe));
        axum::serve(listener, router).await.unwrap();
    });
    (Fixture { _server_root: root, server, runtime, task, endpoint }, trace)
}

struct Scenario {
    fixture: Fixture,
    trace: Arc<Trace>,
    _first_root: tempfile::TempDir,
    _second_root: tempfile::TempDir,
    local: PersistentStore,
    older_head: risunest_sync_wire::RemoteHead,
    old_payload: String,
    remote_payload: String,
    unique_payload: String,
}
impl Scenario {
    fn new() -> Self {
        let (fixture, trace) = measured_fixture();
        let (first_root, mut first) = prepared();
        let (second_root, mut local) = prepared();
        fixture.bind(&mut first);
        fixture.bind(&mut local);
        assert_eq!(settle(&mut first).phase, "idle");
        assert_eq!(settle(&mut local).phase, "idle");
        local.asset_residency_set_policy(AssetPolicy::Remote, || Ok(())).unwrap();
        let old = put(&mut first, "assets/reference-shared.png", &vec![33; 128 * 1024]);
        assert_eq!(settle(&mut first).phase, "idle");
        assert_eq!(settle(&mut local).phase, "idle");
        let older_head = fixture.server.head().unwrap();
        let remote = put(&mut first, "assets/reference-shared.png", &vec![61; 128 * 1024 + 3]);
        let remote_payload = remote.object_hash.unwrap();
        commit_owner(&mut first, &[crate::asset_repository::owner_manifest_codec::OwnerManifestEntry {
            tuple: ["synthetic".into(), "assets/reference-shared.png".into(), "png".into()],
            payload_hash: Some(hex::decode(&remote_payload).unwrap().try_into().unwrap()),
        }]);
        assert_eq!(settle(&mut first).phase, "idle");
        let unique = put(&mut local, "assets/reference-local-only.png", b"unique local-only payload");
        *trace.root.lock().unwrap() = Some(local.repository_root().to_path_buf());
        trace.armed.store(true, Ordering::SeqCst);
        Self { fixture, trace, _first_root: first_root, _second_root: second_root, local, older_head,
            old_payload: old.object_hash.unwrap(), remote_payload, unique_payload: unique.object_hash.unwrap() }
    }

    fn capture(&mut self, head: &risunest_sync_wire::RemoteHead) -> crate::server_sync::Result<(references::Receipt, u64)> {
        let bytes = Arc::new(AtomicU64::new(0));
        let mut client = ServerClient::new(self.local.server_config()?.unwrap())?;
        client.verified_bytes = Some(bytes.clone());
        let cache = Cache::open(&self.local.repository_root().join("server-sync/reference-test-cache"))?;
        let transfer = Transfer::new(&client, &cache)?;
        let revision = self.local.revision()?;
        let receipt = self.local.server_conflict_references(&cache, &transfer, &client, revision, head)?;
        Ok((receipt, bytes.load(Ordering::SeqCst)))
    }

    fn assert_no_payload_transfers(&self) {
        let requests = self.trace.requests.lock().unwrap();
        for request in requests.iter() {
            assert!(!request.path.contains("/uploads") && request.path != "/objects/missing"
                && request.method != "PUT", "reference preparation must not upload: {} {}", request.method, request.path);
            if request.path == "/objects/transfer" {
                for target in request.body.as_array().unwrap() {
                    let hash = target["target"].as_str().unwrap();
                    assert!(hash != self.old_payload && hash != self.remote_payload && hash != self.unique_payload,
                        "reference preparation requested ordinary payload bytes");
                }
            }
            assert!(request.path != format!("/objects/{}", self.old_payload)
                && request.path != format!("/objects/{}", self.remote_payload));
        }
        assert!(!self.trace.released_before_marker.load(Ordering::SeqCst));
    }

    fn index_id(&self) -> String {
        let entries = fs::read_dir(self.local.repository_root().join("server-sync/backups")).unwrap()
            .collect::<std::io::Result<Vec<_>>>().unwrap();
        assert_eq!(entries.len(), 1);
        entries[0].file_name().into_string().unwrap()
    }
}

#[test]
fn reference_preparation_transfers_only_metadata_and_retains_remote_only_local_payloads() {
    let mut scenario = Scenario::new();
    let head = scenario.fixture.server.head().unwrap();
    let revision = scenario.local.revision().unwrap();
    let (receipt, metadata_bytes) = scenario.capture(&head).unwrap();
    assert_eq!(receipt.id, scenario.index_id());
    assert_eq!(receipt.local_revision, revision);
    assert_eq!(receipt.head, head);
    assert_eq!(scenario.local.revision().unwrap(), revision);
    assert_eq!(scenario.fixture.server.head().unwrap(), head);
    assert!(metadata_bytes > 0);
    scenario.assert_no_payload_transfers();
    let index = references::open(scenario.local.repository_root(), &receipt.id, &|| Ok(())).unwrap();
    let unique: (bool, bool) = index.query_row("SELECT local_required,context_id IS NULL FROM objects
        WHERE side='local' AND hash=?1", [&scenario.unique_payload], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(unique, (true, true));
    for (side, hash) in [("local", &scenario.old_payload), ("remote", &scenario.remote_payload)] {
        let state: (bool, bool) = index.query_row("SELECT local_required,context_id IS NOT NULL FROM objects
            WHERE side=?1 AND hash=?2", params![side, hash], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(state, (false, true));
    }
    let invalid_metadata: bool = index.query_row("SELECT EXISTS(SELECT 1 FROM objects
        WHERE role='metadata' AND local_required!=1)", [], |r| r.get(0)).unwrap();
    assert!(!invalid_metadata);
    let cas = PayloadCas::new(scenario.local.repository_root()).unwrap();
    assert_eq!(cas.stat_object(&scenario.old_payload).unwrap(), None);
    assert_eq!(cas.stat_object(&scenario.remote_payload).unwrap(), None);
    assert!(cas.stat_object(&scenario.unique_payload).unwrap().is_some());
    let directory = scenario.local.repository_root().join("server-sync/backups").join(&receipt.id);
    assert!(!directory.join("local.risunest").exists());
    assert!(!directory.join("remote.risunest").exists());
    let pins = DurableCasJob::open(scenario.local.repository_root(), &receipt.id).unwrap();
    assert!(pins.is_sealed());
    assert!(pins.root_set().unwrap().object_hashes.contains(&scenario.unique_payload));
    scenario.local.asset_residency_release_unused(&|| Ok(())).unwrap();
    assert!(Residency::open(scenario.local.repository_root()).unwrap().object(&scenario.remote_payload, None).unwrap().is_some(),
        "a remote object referenced only by the conflict copy must retain custody");
    println!("reference capture: metadata bytes={metadata_bytes}; ordinary payload downloads=0; uploads=0");
}

#[test]
fn wrong_retention_size_leaves_live_data_untouched_and_exact_retry_reuses_the_id() {
    let mut scenario = Scenario::new();
    let head = scenario.fixture.server.head().unwrap();
    let revision = scenario.local.revision().unwrap();
    scenario.trace.corrupt_retention.store(true, Ordering::SeqCst);
    assert_eq!(scenario.capture(&head).unwrap_err().code, "invalid-retention-response");
    let id = scenario.index_id();
    assert!(references::inspect(scenario.local.repository_root(), &id).is_err());
    assert!(!DurableCasJob::open(scenario.local.repository_root(), &id).unwrap().is_released());
    assert_eq!(scenario.local.revision().unwrap(), revision);
    assert_eq!(scenario.fixture.server.head().unwrap(), head);
    assert!(!scenario.trace.requests.lock().unwrap().iter().any(|request| request.method == "DELETE"));
    scenario.assert_no_payload_transfers();
    scenario.trace.corrupt_retention.store(false, Ordering::SeqCst);
    let (receipt, _) = scenario.capture(&head).unwrap();
    assert_eq!(receipt.id, id);
    assert_eq!(scenario.index_id(), id);
    assert_eq!(scenario.local.revision().unwrap(), revision);
    scenario.assert_no_payload_transfers();
}

#[test]
fn changed_remote_head_is_not_recaptured_under_the_old_preview() {
    let mut scenario = Scenario::new();
    let revision = scenario.local.revision().unwrap();
    let head = scenario.fixture.server.head().unwrap();
    let older_head = scenario.older_head.clone();
    assert_eq!(scenario.capture(&older_head).unwrap_err().code, "conflict-preview-stale");
    assert_eq!(scenario.local.revision().unwrap(), revision);
    assert_eq!(scenario.fixture.server.head().unwrap(), head);
    assert!(!scenario.local.repository_root().join("server-sync/backups").exists());
    scenario.assert_no_payload_transfers();
}

#[test]
fn expired_checkpoint_keeps_known_local_roots_without_restarting_the_remote_scan() {
    let mut scenario = Scenario::new();
    let head = scenario.fixture.server.head().unwrap();
    let revision = scenario.local.revision().unwrap();
    scenario.trace.expire_checkpoint.store(true, Ordering::SeqCst);
    assert!(scenario.capture(&head).is_err());
    let id = scenario.index_id();
    assert!(references::inspect(scenario.local.repository_root(), &id).is_err());
    let mut roots = Vec::new();
    references::visit_roots(scenario.local.repository_root(), |object| { roots.push(object.hash); Ok(()) }).unwrap();
    assert!(roots.contains(&scenario.unique_payload));
    assert_eq!(scenario.local.revision().unwrap(), revision);
    assert_eq!(scenario.fixture.server.head().unwrap(), head);
    let requests = scenario.trace.requests.lock().unwrap();
    assert_eq!(requests.iter().filter(|request| request.method == "POST" && request.path == "/checkpoints").count(), 1);
    assert!(!requests.iter().any(|request| request.method == "DELETE"));
    drop(requests);
    scenario.assert_no_payload_transfers();
}
