use super::*;
use crate::asset_repository::job_pins::{
    CasJobKind, CasObjectRole, CasReleaseOutcome, DurableCasJob,
};
use crate::server_sync::{
    backups::{references, Side},
    cache::Cache,
    client::ServerClient,
    transfer::Transfer,
};
use std::sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Mutex};

#[path = "reference_boundaries.rs"]
mod boundaries;

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    query: Option<String>,
    body: Value,
}
#[derive(Default)]
struct Trace {
    armed: AtomicBool,
    corrupt_retention: AtomicBool,
    retention_pages: AtomicU64,
    fail_retention_page: AtomicU64,
    fail_release_once: AtomicBool,
    expire_checkpoint: AtomicBool,
    released_before_marker: AtomicBool,
    add_active_reference_on_session: AtomicBool,
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
    let query = request.uri().query().map(str::to_owned);
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 16 * 1024 * 1024).await.unwrap();
    let armed = trace.armed.load(Ordering::SeqCst);
    if armed {
        trace.requests.lock().unwrap().push(Request {
            method: method.clone(), path: path.clone(), query,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        });
        if method == "GET" && path == "/session"
            && trace.add_active_reference_on_session.swap(false, Ordering::SeqCst) {
            let root = trace.root.lock().unwrap().clone().unwrap();
            let mut store = PersistentStore::open(&root).unwrap();
            let active = put(&mut store, "assets/active-conflict-payload.png",
                &vec![61; 128 * 1024 + 3]);
            assert!(active.object_hash.is_some());
        }
        if method == "DELETE" && path.starts_with("/checkpoints/") {
            let root = trace.root.lock().unwrap().clone().unwrap();
            let complete = fs::read_dir(root.join("server-sync/backups")).ok().is_some_and(|entries|
                entries.filter_map(std::result::Result::ok).any(|entry| entry.path().join("complete.json").is_file()));
            if !complete { trace.released_before_marker.store(true, Ordering::SeqCst); }
        }
        if method == "POST" && path == "/objects/retention" {
            let page = trace.retention_pages.fetch_add(1, Ordering::SeqCst) + 1;
            if trace.fail_retention_page.load(Ordering::SeqCst) == page {
                return (axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error":"synthetic-retention-failure"}))).into_response();
            }
        }
        if method == "POST" && path == "/objects/retention/release"
            && trace.fail_release_once.swap(false, Ordering::SeqCst) {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error":"synthetic-release-failure"}))).into_response();
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
    assert_eq!(DurableCasJob::open(scenario.local.repository_root(), &receipt.id)
        .unwrap_err().kind(), std::io::ErrorKind::NotFound);
    scenario.local.asset_residency_release_unused(&|| Ok(())).unwrap();
    assert!(Residency::open(scenario.local.repository_root()).unwrap().object(&scenario.remote_payload, None).unwrap().is_some(),
        "a remote object referenced only by the conflict copy must retain custody");
    println!("reference capture: metadata bytes={metadata_bytes}; ordinary payload downloads=0; uploads=0");
}

#[test]
fn conflict_roots_replace_released_job_pins_without_forcing_custody_payloads_local() {
    let mut scenario = Scenario::new();
    let head = scenario.fixture.server.head().unwrap();
    let (receipt, _) = scenario.capture(&head).unwrap();
    let index = references::open(scenario.local.repository_root(), &receipt.id, &|| Ok(())).unwrap();
    let metadata = index.prepare("SELECT DISTINCT hash FROM objects WHERE role='metadata' ORDER BY hash")
        .unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap()
        .collect::<std::result::Result<Vec<_>, _>>().unwrap();
    assert!(!metadata.is_empty());
    drop(index);

    assert_eq!(DurableCasJob::open(scenario.local.repository_root(), &receipt.id)
        .unwrap_err().kind(), std::io::ErrorKind::NotFound);
    scenario.local.delete_asset_alias("asset", "assets/reference-local-only.png",
        scenario.local.revision().unwrap()).unwrap();
    for snapshot in scenario.local.snapshot_list().unwrap() {
        scenario.local.snapshot_delete(&snapshot.id).unwrap();
    }
    let cas = PayloadCas::new(scenario.local.repository_root()).unwrap();
    assert!(cas.stat_object(&scenario.unique_payload).unwrap().is_some());
    assert!(metadata.iter().all(|hash| cas.stat_object(hash).unwrap().is_some()));

    use std::io::Read;
    let mut hydrated = Vec::new();
    open_or_hydrate(scenario.local.repository_root(), &scenario.remote_payload).unwrap().unwrap()
        .read_to_end(&mut hydrated).unwrap();
    assert_eq!(hydrated, vec![61; 128 * 1024 + 3]);
    scenario.local.asset_residency_evict(|| Ok(())).unwrap();

    assert_eq!(cas.stat_object(&scenario.remote_payload).unwrap(), None);
    assert!(Residency::open(scenario.local.repository_root()).unwrap()
        .object(&scenario.remote_payload, None).unwrap().is_some());
    assert!(cas.stat_object(&scenario.unique_payload).unwrap().is_some());
    assert!(metadata.iter().all(|hash| cas.stat_object(hash).unwrap().is_some()));
}

#[test]
fn custody_release_waits_for_both_conflicts_and_the_active_library() {
    let mut scenario = Scenario::new();
    let head = scenario.fixture.server.head().unwrap();
    let context = Residency::context_id(&scenario.local.server_stored_config().unwrap().unwrap(), &head.epoch);
    let (first, _) = scenario.capture(&head).unwrap();
    let first_retention = Residency::open(scenario.local.repository_root()).unwrap()
        .object(&scenario.remote_payload, Some(&context)).unwrap().unwrap().retention_id;

    put(&mut scenario.local, "assets/second-conflict-divider.png", b"synthetic second conflict divider");
    let (second, _) = scenario.capture(&head).unwrap();
    assert_ne!(first.id, second.id);
    let second_retention = Residency::open(scenario.local.repository_root()).unwrap()
        .object(&scenario.remote_payload, Some(&context)).unwrap().unwrap().retention_id;
    assert_ne!(first_retention, second_retention);

    crate::server_sync::management::delete_backup(scenario.local.repository_root(), &first.id,
        None, &Default::default()).unwrap();
    scenario.local.asset_residency_release_unused(|| Ok(())).unwrap();
    assert_eq!(Residency::open(scenario.local.repository_root()).unwrap()
        .object(&scenario.remote_payload, Some(&context)).unwrap().unwrap().retention_id,
        second_retention);

    crate::server_sync::management::delete_backup(scenario.local.repository_root(), &second.id,
        None, &Default::default()).unwrap();
    scenario.trace.add_active_reference_on_session.store(true, Ordering::SeqCst);
    scenario.local.asset_residency_release_unused(|| Ok(())).unwrap();
    assert!(!scenario.trace.add_active_reference_on_session.load(Ordering::SeqCst));
    let active = scenario.local.read_asset_alias(
        "asset", "assets/active-conflict-payload.png", None).unwrap().unwrap();
    assert_eq!(active.value.object_hash.as_deref(), Some(scenario.remote_payload.as_str()));
    assert_eq!(Residency::open(scenario.local.repository_root()).unwrap()
        .object(&scenario.remote_payload, Some(&context)).unwrap().unwrap().retention_id,
        second_retention);

    scenario.local.delete_asset_alias("asset", &active.value.key,
        scenario.local.revision().unwrap()).unwrap();
    for snapshot in scenario.local.snapshot_list().unwrap() {
        scenario.local.snapshot_delete(&snapshot.id).unwrap();
    }
    scenario.local.asset_residency_release_unused(|| Ok(())).unwrap();
    assert!(Residency::open(scenario.local.repository_root()).unwrap()
        .object(&scenario.remote_payload, Some(&context)).unwrap().is_none());
    let config = scenario.local.server_config().unwrap().unwrap();
    let device = scenario.fixture.server.authenticate(&config.library_id, &config.token).unwrap();
    assert!(scenario.fixture.server.retained_objects(&device, &head.epoch, None).unwrap().objects
        .iter().all(|object| object.hash != scenario.remote_payload));
}

#[test]
fn failed_custody_release_preserves_other_roots_and_the_next_cleanup_recomputes_inventory() {
    let mut scenario = Scenario::new();
    let head = scenario.fixture.server.head().unwrap();
    let (receipt, _) = scenario.capture(&head).unwrap();
    assert_eq!(DurableCasJob::open(scenario.local.repository_root(), &receipt.id)
        .unwrap_err().kind(), std::io::ErrorKind::NotFound);
    crate::server_sync::management::delete_backup(scenario.local.repository_root(), &receipt.id,
        None, &Default::default()).unwrap();
    for snapshot in scenario.local.snapshot_list().unwrap() {
        scenario.local.snapshot_delete(&snapshot.id).unwrap();
    }
    let mut conflict_roots = std::collections::BTreeSet::new();
    references::visit_roots(scenario.local.repository_root(), |object| {
        conflict_roots.insert(object.hash);
        Ok(())
    }).unwrap();
    assert!(!conflict_roots.contains(&scenario.unique_payload));
    assert!(scenario.local.read_asset_alias(
        "asset", "assets/reference-local-only.png", None).unwrap().is_some());
    let cas = PayloadCas::new(scenario.local.repository_root()).unwrap();
    assert!(cas.stat_object(&scenario.unique_payload).unwrap().is_some());

    let config = scenario.local.server_config().unwrap().unwrap();
    let stored = scenario.local.server_stored_config().unwrap().unwrap();
    let device = scenario.fixture.server.authenticate(&config.library_id, &config.token).unwrap();
    let unused_bytes = b"synthetic unreferenced custody after conflict deletion";
    let unused_hash = risunest_sync_wire::hash(unused_bytes);
    scenario.fixture.server.put_object(&device, &unused_hash, unused_bytes).unwrap();
    let client = ServerClient::new(config).unwrap();
    let mut residency = Residency::open(scenario.local.repository_root()).unwrap();
    residency.retain(&client, &stored, &head,
        &[(unused_hash.clone(), Some(unused_bytes.len() as u64))]).unwrap();
    let context = Residency::context_id(&stored, &head.epoch);
    assert!(residency.release_object(&unused_hash, &context).unwrap().is_some());
    drop(residency);

    scenario.trace.fail_release_once.store(true, Ordering::SeqCst);
    assert!(scenario.local.asset_residency_release_unused(|| Ok(())).is_err());
    assert!(!scenario.trace.fail_release_once.load(Ordering::SeqCst));
    assert!(scenario.local.read_asset_alias(
        "asset", "assets/reference-local-only.png", None).unwrap().is_some());
    assert!(cas.stat_object(&scenario.unique_payload).unwrap().is_some());

    scenario.local.asset_residency_release_unused(|| Ok(())).unwrap();
    assert!(Residency::open(scenario.local.repository_root()).unwrap()
        .release_object(&unused_hash, &context)
        .unwrap().is_none());
    assert!(scenario.local.read_asset_alias(
        "asset", "assets/reference-local-only.png", None).unwrap().is_some());
    assert!(cas.stat_object(&scenario.unique_payload).unwrap().is_some());
    let release_requests = scenario.trace.requests.lock().unwrap().iter()
        .filter(|request| request.method == "POST"
            && request.path == "/objects/retention/release").count();
    assert_eq!(release_requests, 2);
}

#[test]
fn whole_pds_replacement_preserves_completed_and_interrupted_reference_roots() {
    for completed in [true, false] {
        let mut scenario = Scenario::new();
        let protected_bytes = format!("synthetic conflict root completed={completed}").into_bytes();
        let protected = put(
            &mut scenario.local,
            "assets/replacement-conflict-root.png",
            &protected_bytes,
        );
        let protected_hash = protected.object_hash.clone().unwrap();
        scenario
            .local
            .delete_asset_alias("asset", &protected.key, scenario.local.revision().unwrap())
            .unwrap();
        let before_conflict = scenario
            .local
            .snapshot_create(if completed {
                "before-completed-conflict"
            } else {
                "before-interrupted-conflict"
            })
            .unwrap();
        scenario
            .local
            .commit_asset_alias(&protected, scenario.local.revision().unwrap())
            .unwrap();

        let repository_root = scenario.local.repository_root().to_path_buf();
        let revision = scenario.local.revision().unwrap();
        let generation = active_generation(&scenario.local.connection).unwrap();
        let head = scenario.fixture.server.head().unwrap();
        let stored = scenario.local.server_stored_config().unwrap().unwrap();
        let context = Residency::context_id(&stored, &head.epoch);
        let client = ServerClient::new(scenario.local.server_config().unwrap().unwrap()).unwrap();
        let mut capture =
            references::Capture::begin(&repository_root, revision, &generation, &head).unwrap();
        let id = capture.id.clone();
        capture
            .local_payload(&protected_hash, protected_bytes.len() as u64, &|| Ok(()))
            .unwrap();
        capture
            .object(
                Side::Remote,
                &references::Object {
                    hash: scenario.remote_payload.clone(),
                    byte_size: Some(128 * 1024 + 3),
                    metadata: false,
                    context_id: Some(context.clone()),
                    local_required: false,
                },
            )
            .unwrap();
        capture.complete_side(Side::Local).unwrap();
        capture.complete_side(Side::Remote).unwrap();
        let mut residency = Residency::open(&repository_root).unwrap();
        capture.retain(&client, &stored, &mut residency).unwrap();
        drop(residency);

        if completed {
            capture.finish(&mut scenario.local, &|| Ok(())).unwrap();
            references::open(&repository_root, &id, &|| Ok(())).unwrap();
        } else {
            let capture_path = repository_root
                .join("server-sync/backups")
                .join(&id);
            let marker = capture_path.join("complete.json");
            let result = capture.finish(&mut scenario.local, &|| {
                if fs::read_dir(&capture_path).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("complete-")
                }) {
                    Err(crate::server_sync::SyncError::new("cancelled", 409))
                } else {
                    Ok(())
                }
            });
            assert!(matches!(result, Err(error) if error.code == "cancelled"));
            assert!(!marker.exists());
            assert!(DurableCasJob::open(&repository_root, &id)
                .unwrap()
                .is_sealed());
            assert!(references::open(&repository_root, &id, &|| Ok(())).is_err());
        }

        scenario
            .local
            .snapshot_restore_request(&before_conflict.id)
            .unwrap();
        drop(scenario.local);
        let mut local = PersistentStore::open(&repository_root).unwrap();
        assert_eq!(local.pending_restore_failure(), None);
        assert!(local
            .read_asset_alias("asset", &protected.key, None)
            .unwrap()
            .is_none());
        local.snapshot_delete(&before_conflict.id).unwrap();

        let mut roots = std::collections::BTreeSet::new();
        references::visit_roots(&repository_root, |object| {
            roots.insert(object.hash);
            Ok(())
        })
        .unwrap();
        assert!(roots.contains(&protected_hash));
        assert!(roots.contains(&scenario.remote_payload));
        if completed {
            references::open(&repository_root, &id, &|| Ok(())).unwrap();
        } else {
            assert!(references::open(&repository_root, &id, &|| Ok(())).is_err());
        }

        let gc = local
            .asset_gc_delete_page(4096, None, i64::MAX / 2, 0)
            .unwrap();
        assert!(gc.report.marked_hashes.contains(&protected_hash));
        assert!(!gc.report.deleted_hashes.contains(&protected_hash));
        assert!(PayloadCas::new(&repository_root)
            .unwrap()
            .stat_object(&protected_hash)
            .unwrap()
            .is_some());
        local.asset_residency_release_unused(&|| Ok(())).unwrap();
        assert!(Residency::open(&repository_root)
            .unwrap()
            .object(&scenario.remote_payload, Some(&context))
            .unwrap()
            .is_some());
    }
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

#[test]
fn reference_source_prepares_replacement_without_hydrating_custody_payloads() {
    let mut scenario = Scenario::new();
    let head = scenario.fixture.server.head().unwrap();
    let (receipt, _) = scenario.capture(&head).unwrap();
    let source = crate::server_sync::backups::source(
        scenario.local.repository_root(),
        &receipt.id,
        Side::Remote,
        &|| Ok(()),
    ).unwrap();
    let revision = scenario.local.revision().unwrap();
    let cas = PayloadCas::new(scenario.local.repository_root()).unwrap();
    assert_eq!(cas.stat_object(&scenario.remote_payload).unwrap(), None);

    let prepared = scenario.local.prepare_server_conflict_replacement(
        &source,
        revision,
        &|| Ok(()),
    ).unwrap();
    assert_eq!(cas.stat_object(&scenario.remote_payload).unwrap(), None);
    assert_eq!(scenario.local.finish_prepared_replace(prepared).unwrap().revision, revision + 1);
    assert_eq!(cas.stat_object(&scenario.remote_payload).unwrap(), None);
    let alias = scenario.local.read_asset_alias(
        "asset",
        "assets/reference-shared.png",
        None,
    ).unwrap().unwrap();
    assert_eq!(alias.value.object_hash.as_deref(), Some(scenario.remote_payload.as_str()));
}

#[test]
fn local_reference_source_prepares_replacement_after_server_unbind() {
    let mut scenario = Scenario::new();
    scenario.local.delete_asset_alias(
        "asset",
        "assets/reference-shared.png",
        scenario.local.revision().unwrap(),
    ).unwrap();
    let head = scenario.fixture.server.head().unwrap();
    let (receipt, _) = scenario.capture(&head).unwrap();
    let index = references::open(
        scenario.local.repository_root(),
        &receipt.id,
        &|| Ok(()),
    ).unwrap();
    let requirements = references::side_requirements(
        scenario.local.repository_root(),
        &index,
        Side::Local,
    ).unwrap();
    assert_eq!(requirements.remote_dependent_bytes, 0);
    assert!(requirements.local_required_available);
    drop(index);
    let source = crate::server_sync::backups::source(
        scenario.local.repository_root(),
        &receipt.id,
        Side::Local,
        &|| Ok(()),
    ).unwrap();
    let revision = scenario.local.revision().unwrap();

    use std::io::Read;
    let mut hydrated = Vec::new();
    open_or_hydrate(scenario.local.repository_root(), &scenario.remote_payload)
        .unwrap()
        .unwrap()
        .read_to_end(&mut hydrated)
        .unwrap();
    assert_eq!(hydrated, vec![61; 128 * 1024 + 3]);
    let cas = PayloadCas::new(scenario.local.repository_root()).unwrap();
    assert_eq!(
        cas.stat_object(&scenario.remote_payload).unwrap(),
        Some(hydrated.len() as u64),
    );
    scenario.local.server_unbind().unwrap();
    let prepared = scenario.local.prepare_server_conflict_replacement(
        &source,
        revision,
        &|| Ok(()),
    ).unwrap();
    assert_eq!(scenario.local.finish_prepared_replace(prepared).unwrap().revision, revision + 1);
    assert!(scenario.local.server_config().unwrap().is_none());
    let alias = scenario.local.read_asset_alias(
        "asset",
        "assets/reference-local-only.png",
        None,
    ).unwrap().unwrap();
    assert_eq!(alias.value.object_hash.as_deref(), Some(scenario.unique_payload.as_str()));
}

#[test]
fn reference_source_rejects_missing_local_required_and_corrupt_metadata() {
    {
        let mut scenario = Scenario::new();
        let head = scenario.fixture.server.head().unwrap();
        let (receipt, _) = scenario.capture(&head).unwrap();
        let source = crate::server_sync::backups::source(
            scenario.local.repository_root(),
            &receipt.id,
            Side::Local,
            &|| Ok(()),
        ).unwrap();
        let cas = PayloadCas::new(scenario.local.repository_root()).unwrap();
        fs::remove_file(cas.object_path(&scenario.unique_payload).unwrap().unwrap()).unwrap();
        let export_scratch = tempfile::tempdir().unwrap();
        let export_error = match crate::server_sync::backups::prepare_reference_portable_store(
            scenario.local.repository_root(),
            &source,
            export_scratch.path(),
            &|| Ok(()),
        ) {
            Ok(_) => panic!("portable export must not hydrate a local-required payload"),
            Err(error) => error,
        };
        assert_eq!(export_error.code, "conflict-local-object-missing");
        let revision = scenario.local.revision().unwrap();
        let error = match scenario.local.prepare_server_conflict_replacement(
            &source,
            revision,
            &|| Ok(()),
        ) {
            Ok(_) => panic!("missing local-required payload must be rejected"),
            Err(error) => error,
        };
        assert_eq!(
            error.code,
            "conflict-local-object-missing",
        );
    }

    {
        let mut scenario = Scenario::new();
        let head = scenario.fixture.server.head().unwrap();
        let (receipt, _) = scenario.capture(&head).unwrap();
        let source = crate::server_sync::backups::source(
            scenario.local.repository_root(),
            &receipt.id,
            Side::Remote,
            &|| Ok(()),
        ).unwrap();
        let index = references::open(
            scenario.local.repository_root(),
            &receipt.id,
            &|| Ok(()),
        ).unwrap();
        let (hash, size): (String, i64) = index.query_row(
            "SELECT hash,byte_size FROM objects WHERE side='remote' AND role='metadata' ORDER BY hash LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        drop(index);
        let cas = PayloadCas::new(scenario.local.repository_root()).unwrap();
        fs::write(
            cas.object_path(&hash).unwrap().unwrap(),
            vec![0; usize::try_from(size).unwrap()],
        ).unwrap();
        let revision = scenario.local.revision().unwrap();
        assert!(scenario.local.prepare_server_conflict_replacement(
            &source,
            revision,
            &|| Ok(()),
        ).is_err());
    }
}

#[test]
fn reference_source_rejects_a_record_body_with_mismatched_descriptor_semantics() {
    let mut scenario = Scenario::new();
    let head = scenario.fixture.server.head().unwrap();
    let (receipt, _) = scenario.capture(&head).unwrap();
    let directory = scenario.local.repository_root()
        .join("server-sync/backups")
        .join(&receipt.id);
    let index_path = directory.join("index.sqlite");
    {
        let db = rusqlite::Connection::open(&index_path).unwrap();
        let records = db.prepare(
            "SELECT record_key,version_json FROM records
             WHERE side='local' AND body_hash IS NOT NULL ORDER BY record_key LIMIT 2",
        ).unwrap().query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }).unwrap().collect::<std::result::Result<Vec<_>, _>>().unwrap();
        assert_eq!(records.len(), 2);
        let (first_key, first_version) = &records[0];
        let (_, second_version) = &records[1];
        assert_ne!(first_version, second_version);
        db.execute(
            "UPDATE records SET version_json=?2 WHERE side='local' AND record_key=?1",
            rusqlite::params![first_key, second_version],
        ).unwrap();
    }
    let index = fs::read(&index_path).unwrap();
    let marker_path = directory.join("complete.json");
    let mut marker: Value = serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
    marker["indexHash"] = json!(risunest_sync_wire::hash(&index));
    marker["indexBytes"] = json!(index.len() as u64);
    fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();

    let source = crate::server_sync::backups::source(
        scenario.local.repository_root(),
        &receipt.id,
        Side::Local,
        &|| Ok(()),
    ).unwrap();
    let revision = scenario.local.revision().unwrap();
    let error = match scenario.local.prepare_server_conflict_replacement(
        &source,
        revision,
        &|| Ok(()),
    ) {
        Ok(_) => panic!("descriptor and body mismatch must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.code, "server-descriptor-semantics-mismatch");
}

#[test]
fn remote_reference_exports_a_restore_validated_portable_library_with_exact_records() {
    use crate::local_backup::NeverCancelled;
    use std::fs::File;

    let mut scenario = Scenario::new();
    let mut remote = PersistentStore::open(scenario._first_root.path()).unwrap();
    let generation = active_generation(&remote.connection).unwrap();
    let mut root = remote.read_root(None).unwrap().value;
    root["referenceExportRoot"] = json!("exact-root-value");
    let mut presets = remote.connection.prepare(
        "SELECT value FROM bot_presets WHERE generation=?1 ORDER BY configured_index",
    ).unwrap().query_map([&generation], |row| row.get::<_, String>(0)).unwrap()
        .map(|value| serde_json::from_str::<Value>(&value.unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert!(!presets.is_empty());
    presets[0]["referenceExportPreset"] = json!("exact-preset-value");
    let (character_id, mut character): (String, Value) = remote.connection.query_row(
        "SELECT character_id,detail FROM characters WHERE generation=?1 ORDER BY configured_index LIMIT 1",
        [&generation],
        |row| Ok((row.get(0)?, serde_json::from_str::<Value>(&row.get::<_, String>(1)?).unwrap())),
    ).unwrap();
    character["referenceExportCharacter"] = json!("exact-character-value");
    let (conversation_id, mut conversation, message_count): (String, Value, i64) = remote.connection.query_row(
        "SELECT conversation_id,detail,message_count FROM conversations
         WHERE generation=?1 AND character_id=?2 ORDER BY configured_index LIMIT 1",
        params![generation, character_id],
        |row| Ok((row.get(0)?, serde_json::from_str::<Value>(&row.get::<_, String>(1)?).unwrap(), row.get(2)?)),
    ).unwrap();
    conversation["referenceExportConversation"] = json!({"nested": true, "rank": 7});
    let message = json!({
        "role": "user",
        "data": "portable conflict exact text",
        "chatId": "portable-conflict-message",
        "metadata": {"speaker": "synthetic", "sequence": 7}
    });
    let plugin = json!({"text": "exact-plugin-value", "metadata": {"enabled": true}});
    remote.commit(&WorkingSetCommit {
        root: Some(root),
        replace_presets: Some(presets),
        character_details: Some(vec![character]),
        conversations: Some(vec![ConversationMutation::ReplaceRange {
            character_id: character_id.clone(),
            conversation_id: conversation_id.clone(),
            start: 0,
            delete_count: message_count,
            messages: vec![message.clone()],
            conversation: Some(conversation.clone()),
            configured_index: None,
        }]),
        plugin_storage: Some(vec![PluginStorageMutation::Set {
            owner: "reference-export-plugin".into(),
            key: "exact".into(),
            value: plugin.clone(),
        }]),
        ..empty_working_set_commit(remote.revision().unwrap())
    }).unwrap();
    assert_eq!(settle(&mut remote).phase, "idle");
    drop(remote);

    assert_eq!(PayloadCas::new(scenario.local.repository_root()).unwrap()
        .stat_object(&scenario.remote_payload).unwrap(), None);
    let head = scenario.fixture.server.head().unwrap();
    let (receipt, _) = scenario.capture(&head).unwrap();
    let source = crate::server_sync::backups::source(
        scenario.local.repository_root(),
        &receipt.id,
        Side::Remote,
        &|| Ok(()),
    ).unwrap();
    let work = tempfile::tempdir().unwrap();
    let prepared = crate::server_sync::backups::prepare_reference_portable_store(
        scenario.local.repository_root(),
        &source,
        work.path(),
        &|| Ok(()),
    ).unwrap();
    assert_eq!(PayloadCas::new(scenario.local.repository_root()).unwrap()
        .stat_object(&scenario.remote_payload).unwrap(), Some(128 * 1024 + 3));
    let (mut export_store, revision, _scratch) = prepared.into_parts();
    let archive_scratch = work.path().join("archive-scratch");
    fs::create_dir(&archive_scratch).unwrap();
    let candidate = work.path().join("reference-export.risunest");
    crate::portable_backup::create_verified_library_backup(
        &mut export_store,
        revision,
        &candidate,
        &archive_scratch,
        &NeverCancelled,
    ).unwrap();
    let archive = crate::portable_backup::VerifiedArchive::open(
        File::open(&candidate).unwrap(),
        work.path(),
        &NeverCancelled,
    ).unwrap();
    archive.validate_library(&NeverCancelled).unwrap();

    let restored_root = tempfile::tempdir().unwrap();
    let mut restored = PersistentStore::open(restored_root.path()).unwrap();
    let inventory = crate::portable_backup::RestoreInventory::build(
        &archive,
        work.path(),
        &NeverCancelled,
    ).unwrap();
    let mut pins = DurableCasJob::begin(
        restored.repository_root(),
        &uuid::Uuid::new_v4().to_string(),
        CasJobKind::LocalBackupRestore,
        0,
    ).unwrap();
    let restored_cas = PayloadCas::new(restored.repository_root()).unwrap();
    let mut statement = inventory.db
        .prepare("SELECT hash,owner FROM live_objects ORDER BY hash")
        .unwrap();
    let mut rows = statement.query([]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        let hash: String = row.get(0).unwrap();
        let owner: bool = row.get(1).unwrap();
        let (mut input, size) = archive.open_object(&hash).unwrap();
        pins.prepare_reader_expected(
            &restored_cas,
            &mut input,
            &hash,
            size,
            if owner {
                CasObjectRole::OwnerManifest
            } else {
                CasObjectRole::DirectObject
            },
        ).unwrap();
    }
    drop(rows);
    drop(statement);
    pins.seal(&mut restored, 0).unwrap();
    let stage = restored.stage_portable_records(&archive.db, &NeverCancelled).unwrap();
    let prepared = restored.prepare_replace_commit(&stage.staging_id, Some(0)).unwrap();
    restored.finish_prepared_replace(prepared).unwrap();
    pins.release(CasReleaseOutcome::Committed).unwrap();
    let restored_generation = active_generation(&restored.connection).unwrap();
    assert_eq!(restored.read_root(None).unwrap().value["referenceExportRoot"], "exact-root-value");
    let preset: Value = serde_json::from_str(&restored.connection.query_row::<String, _, _>(
        "SELECT value FROM bot_presets WHERE generation=?1 ORDER BY configured_index LIMIT 1",
        [&restored_generation], |row| row.get(0),
    ).unwrap()).unwrap();
    assert_eq!(preset["referenceExportPreset"], "exact-preset-value");
    let restored_character: Value = serde_json::from_str(&restored.connection.query_row::<String, _, _>(
        "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
        params![restored_generation, character_id], |row| row.get(0),
    ).unwrap()).unwrap();
    assert_eq!(restored_character["referenceExportCharacter"], "exact-character-value");
    let restored_conversation: Value = serde_json::from_str(&restored.connection.query_row::<String, _, _>(
        "SELECT detail FROM conversations WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",
        params![restored_generation, character_id, conversation_id], |row| row.get(0),
    ).unwrap()).unwrap();
    assert_eq!(restored_conversation["referenceExportConversation"], conversation["referenceExportConversation"]);
    let restored_message: Value = serde_json::from_str(&restored.connection.query_row::<String, _, _>(
        "SELECT value FROM messages WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",
        params![restored_generation, character_id, conversation_id], |row| row.get(0),
    ).unwrap()).unwrap();
    assert_eq!(restored_message, message);
    let restored_plugin: Value = serde_json::from_str(&restored.connection.query_row::<String, _, _>(
        "SELECT value FROM plugin_storage WHERE generation=?1 AND owner='reference-export-plugin' AND storage_key='exact'",
        [&restored_generation], |row| row.get(0),
    ).unwrap()).unwrap();
    assert_eq!(restored_plugin, plugin);
    let alias = restored.read_asset_alias("asset", "assets/reference-shared.png", None)
        .unwrap().unwrap();
    assert_eq!(alias.value.object_hash.as_deref(), Some(scenario.remote_payload.as_str()));
    assert_eq!(PayloadCas::new(restored.repository_root()).unwrap()
        .read_object(&scenario.remote_payload).unwrap().unwrap(), vec![61; 128 * 1024 + 3]);
}
