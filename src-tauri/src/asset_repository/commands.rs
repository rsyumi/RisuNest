use super::job_pins::{CasJobKind, CasObjectRole, CasReleaseOutcome, DurableCasJob};
use super::{PayloadCas, PreparedPayload};
use crate::asset_repository::owner_manifest_codec::{
    decode_owner_manifest, encode_owner_manifest, OWNER_MANIFEST_V1_MAX_CANONICAL_BYTES,
};
use crate::native_file_jobs::NativeFileJobState;
use crate::persistent_store::{self, PersistentStore, PersistentStoreState, StoreError};
use crate::trust_boundary::is_lower_hex_256;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Cursor, ErrorKind};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};

#[derive(Clone, Debug)]
pub(crate) struct ContentDirectObject {
    object_hash: String,
    byte_size: u64,
}

pub(crate) struct DurableCasJobState {
    jobs: Mutex<HashMap<String, DurableCasJob>>,
}

impl Default for DurableCasJobState {
    fn default() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
        }
    }
}

fn repository_root(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    crate::app_data_root::resolve(app)
        .map_err(|error| format!("failed to resolve application data directory: {error}"))
}

fn now_ms() -> Result<i64, String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| "system time exceeds the supported millisecond range".to_owned())
}

// Recovers (opens and caches) an interrupted durable CAS job session, or
// returns the cached one. Shared by every command that needs the recovered
// job, including those that also hold the job map or the store lock.
fn recover_job<'a>(
    jobs: &'a mut HashMap<String, DurableCasJob>,
    root: &std::path::Path,
    session_id: &str,
) -> Result<&'a mut DurableCasJob, String> {
    if !jobs.contains_key(session_id) {
        let recovered = DurableCasJob::open(root, session_id).map_err(|error| error.to_string())?;
        jobs.insert(session_id.to_owned(), recovered);
    }
    Ok(jobs
        .get_mut(session_id)
        .expect("recovered CAS job session must be present"))
}

fn with_recovered_job<T>(
    app: &AppHandle,
    session_id: &str,
    operation: impl FnOnce(&mut DurableCasJob, &PayloadCas) -> std::io::Result<T>,
) -> Result<T, String> {
    let root = repository_root(app)?;
    let cas = PayloadCas::new(&root).map_err(|error| error.to_string())?;
    let state = app.state::<DurableCasJobState>();
    let mut jobs = state
        .jobs
        .lock()
        .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?;
    operation(recover_job(&mut jobs, &root, session_id)?, &cas).map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_begin(
    app: AppHandle,
    kind: CasJobKind,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        let root = repository_root(&app)?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let job = DurableCasJob::begin(&root, &session_id, kind, now_ms()?)
            .map_err(|error| error.to_string())?;
        app.state::<DurableCasJobState>()
            .jobs
            .lock()
            .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?
            .insert(session_id.clone(), job);
        Ok(session_id)
    })
    .await
    .map_err(|error| format!("failed to join CAS job begin operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_prepare(
    app: AppHandle,
    session_id: String,
    data: Vec<u8>,
    role: CasObjectRole,
) -> Result<PreparedPayload, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        with_recovered_job(&app, &session_id, |job, cas| {
            job.prepare_bytes(cas, &data, role)
        })
    })
    .await
    .map_err(|error| format!("failed to join CAS job prepare operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_pin_existing(
    app: AppHandle,
    session_id: String,
    content_hash: String,
    byte_size: u64,
    role: CasObjectRole,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        with_recovered_job(&app, &session_id, |job, cas| {
            job.pin_existing(cas, &content_hash, byte_size, role)
        })
    })
    .await
    .map_err(|error| format!("failed to join existing CAS pin operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_seal(app: AppHandle, session_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        let root = repository_root(&app)?;
        let state = app.state::<DurableCasJobState>();
        let mut jobs = state
            .jobs
            .lock()
            .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?;
        let job = recover_job(&mut jobs, &root, &session_id)?;
        persistent_store::commands::with_store_mut(app.state::<PersistentStoreState>(), |store| {
            job.seal(
                store,
                now_ms().map_err(|message| StoreError::Store { message })?,
            )
            .map_err(StoreError::from)
        })
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("failed to join CAS job seal operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_release(
    app: AppHandle,
    session_id: String,
    outcome: CasReleaseOutcome,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        let root = repository_root(&app)?;
        let state = app.state::<DurableCasJobState>();
        let mut jobs = state
            .jobs
            .lock()
            .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?;
        let release_result = recover_job(&mut jobs, &root, &session_id)?
            .release(outcome)
            .map_err(|error| error.to_string());
        if jobs
            .get(&session_id)
            .is_some_and(DurableCasJob::is_released)
        {
            jobs.remove(&session_id);
        }
        release_result
    })
    .await
    .map_err(|error| format!("failed to join CAS job release operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_read_object(
    app: AppHandle,
    content_hash: String,
) -> Result<Option<Vec<u8>>, String> {
    let root = repository_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        PayloadCas::new(&root)
            .and_then(|cas| cas.read_object(&content_hash))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("failed to join CAS read operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_read_object_range(
    app: AppHandle,
    content_hash: String,
    start: u64,
    end_exclusive: u64,
) -> Result<Option<Vec<u8>>, String> {
    let root = repository_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        PayloadCas::new(&root)
            .and_then(|cas| cas.read_object_range(&content_hash, start, end_exclusive))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("failed to join CAS range operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_stat_object(
    app: AppHandle,
    content_hash: String,
) -> Result<Option<u64>, String> {
    let root = repository_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        PayloadCas::new(&root)
            .and_then(|cas| cas.stat_object(&content_hash))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("failed to join CAS stat operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn asset_remote_stat_object(
    app: AppHandle,
    content_hash: String,
) -> Result<Option<u64>, String> {
    let root = repository_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        crate::server_sync::residency::Residency::open(&root)
            .and_then(|store| store.object(&content_hash, None))
            .map(|object| object.map(|object| object.size))
            .map_err(|error| error.code)
    })
    .await
    .map_err(|_| "remote-stat-unavailable".to_owned())?
}

#[tauri::command(async)]
pub(crate) async fn asset_remote_read_object(
    app: AppHandle,
    content_hash: String,
    start: Option<u64>,
    end_exclusive: Option<u64>,
) -> Result<Option<Vec<u8>>, String> {
    let root = repository_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        use std::io::{Read, Seek, SeekFrom};
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        if start.is_some() != end_exclusive.is_some()
            || start
                .zip(end_exclusive)
                .is_some_and(|(start, end)| start > end)
        {
            return Err("invalid-asset-range".into());
        }
        let Some(mut file) = crate::server_sync::residency::open_or_hydrate(&root, &content_hash)
            .map_err(|error| error.code)?
        else {
            return Ok(None);
        };
        let size = file.metadata().map_err(|error| error.to_string())?.len();
        let from = start.unwrap_or(0).min(size);
        let to = end_exclusive.unwrap_or(size).min(size);
        file.seek(SeekFrom::Start(from))
            .map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        file.take(to - from)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        Ok(Some(bytes))
    })
    .await
    .map_err(|_| "remote-read-unavailable".to_owned())?
}

fn finalize_content_job(
    job: &mut DurableCasJob,
    cas: &PayloadCas,
    store: &mut crate::persistent_store::PersistentStore,
    owner_manifest: &[u8],
    direct_objects: &[ContentDirectObject],
    created_at_ms: i64,
) -> io::Result<PreparedPayload> {
    if job.kind() != CasJobKind::CardOrModuleContentImport {
        return invalid_content_finalization("CAS job is not a content import session");
    }
    if job.is_sealed() || job.is_released() || job.pin_count() != 0 {
        return invalid_content_finalization("content import CAS session is not fresh");
    }
    if created_at_ms < 0 {
        return invalid_content_finalization(
            "content import finalization time must be nonnegative",
        );
    }
    if owner_manifest.len() > OWNER_MANIFEST_V1_MAX_CANONICAL_BYTES {
        return invalid_content_finalization("owner manifest exceeds its canonical byte limit");
    }
    if direct_objects.len() > super::job_pins::MAX_DURABLE_CAS_JOB_PINS {
        return invalid_content_finalization("content import direct object list exceeds its limit");
    }
    let decoded = decode_owner_manifest(owner_manifest)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?;
    if encode_owner_manifest(&decoded)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?
        != owner_manifest
    {
        return invalid_content_finalization("owner manifest bytes are not canonical");
    }
    let owner_hash = hex::encode(Sha256::digest(owner_manifest));
    let owner_size = owner_manifest.len() as u64;
    let mut complete = BTreeMap::<String, (u64, CasObjectRole)>::new();
    for object in direct_objects {
        validate_content_object_hash(&object.object_hash)?;
        if object.byte_size > i64::MAX as u64 {
            return invalid_content_finalization(
                "content import direct object exceeds the catalog size limit",
            );
        }
        match complete.get(&object.object_hash) {
            Some((byte_size, CasObjectRole::DirectObject)) if *byte_size == object.byte_size => {}
            Some(_) => {
                return invalid_content_finalization(
                    "content import direct object has conflicting sizes",
                )
            }
            None => {
                complete.insert(
                    object.object_hash.clone(),
                    (object.byte_size, CasObjectRole::DirectObject),
                );
            }
        }
    }
    for entry in &decoded {
        let Some(payload_hash) = entry.payload_hash else {
            continue;
        };
        let payload_hash = hex::encode(payload_hash);
        if !complete.contains_key(&payload_hash) {
            return invalid_content_finalization(
                "owner manifest references an unlisted content object",
            );
        }
    }
    if let Some((byte_size, _)) = complete.get(&owner_hash) {
        if *byte_size != owner_size {
            return invalid_content_finalization(
                "owner manifest hash has a conflicting direct object size",
            );
        }
    }
    complete.insert(
        owner_hash.clone(),
        (owner_size, CasObjectRole::OwnerManifest),
    );
    if complete.len() > super::job_pins::MAX_DURABLE_CAS_JOB_PINS {
        return invalid_content_finalization("content import CAS pin set exceeds its limit");
    }
    for (object_hash, (byte_size, role)) in &complete {
        if *role == CasObjectRole::OwnerManifest {
            continue;
        }
        match cas.stat_object(object_hash)? {
            Some(actual_size) if actual_size == *byte_size => {}
            Some(_) => {
                return invalid_content_finalization(
                    "content import direct object size does not match CAS",
                )
            }
            None => return invalid_content_finalization("content import direct object is missing"),
        }
    }

    let mut reader = Cursor::new(owner_manifest);
    let prepared = job.prepare_reader(cas, &mut reader, CasObjectRole::OwnerManifest)?;
    if prepared.content_hash != owner_hash || prepared.byte_size != owner_size {
        return invalid_content_finalization("prepared owner manifest does not match exact bytes");
    }
    let direct_pins = complete
        .into_iter()
        .filter(|(_, (_, role))| *role == CasObjectRole::DirectObject)
        .map(|(object_hash, (byte_size, role))| (object_hash, byte_size, role))
        .collect::<Vec<_>>();
    job.pin_existing_batch(cas, &direct_pins)?;
    job.seal(store, created_at_ms)?;
    Ok(prepared)
}

fn validate_content_object_hash(hash: &str) -> io::Result<()> {
    if is_lower_hex_256(hash) {
        return Ok(());
    }
    invalid_content_finalization("content import object hash must be lowercase SHA-256")
}

fn invalid_content_finalization<T>(message: &str) -> io::Result<T> {
    Err(io::Error::new(ErrorKind::InvalidData, message))
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_finalize_content(
    app: AppHandle,
    native_jobs: State<'_, NativeFileJobState>,
    session_id: String,
    owner_manifest: Vec<u8>,
) -> Result<PreparedPayload, String> {
    let content_assets = native_jobs
        .content_asset_receipt(&session_id)
        .map_err(|error| format!("{}: {}", error.code, error.message))?
        .into_iter()
        .map(|(object_hash, byte_size)| ContentDirectObject {
            object_hash,
            byte_size,
        })
        .collect::<Vec<_>>();
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        let root = repository_root(&app)?;
        let cas = PayloadCas::new(&root).map_err(|error| error.to_string())?;
        let state = app.state::<DurableCasJobState>();
        let mut jobs = state
            .jobs
            .lock()
            .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?;
        let job = recover_job(&mut jobs, &root, &session_id)?;
        persistent_store::commands::with_store_mut(app.state::<PersistentStoreState>(), |store| {
            finalize_content_job(
                job,
                &cas,
                store,
                &owner_manifest,
                &content_assets,
                now_ms().map_err(|message| StoreError::Store { message })?,
            )
            .map_err(StoreError::from)
        })
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("failed to join content CAS finalization: {error}"))?
}

fn seal_prepared_content_job_by_id(
    native_jobs: &NativeFileJobState,
    repository_root: &std::path::Path,
    jobs: &mut HashMap<String, DurableCasJob>,
    store: &mut PersistentStore,
    session_id: &str,
    created_at_ms: i64,
) -> Result<(), String> {
    let content = native_jobs
        .prepared_content_receipt(session_id)
        .map_err(|error| format!("{}: {}", error.code, error.message))?;
    if content.format != crate::native_file_jobs::PreparedContentFormat::RisuModule {
        return Err("prepared content does not own native RISUM roots".to_owned());
    }
    let mut expected = content
        .assets
        .iter()
        .map(|asset| {
            (
                asset.object_hash.clone(),
                asset.byte_size,
                CasObjectRole::DirectObject,
            )
        })
        .collect::<Vec<_>>();
    let owner_head = content
        .owner_head
        .as_ref()
        .ok_or_else(|| "prepared RISUM content has no owner head".to_owned())?;
    if owner_head.present {
        if owner_head.entry_count != content.assets.len() {
            return Err("prepared RISUM owner head count does not match its assets".to_owned());
        }
        let manifest_hash = owner_head
            .manifest_hash
            .as_ref()
            .ok_or_else(|| "prepared RISUM owner head has no manifest hash".to_owned())?;
        let cas = PayloadCas::new(repository_root).map_err(|error| error.to_string())?;
        let manifest_size = cas
            .stat_object(manifest_hash)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "prepared RISUM owner manifest is missing".to_owned())?;
        expected.push((
            manifest_hash.clone(),
            manifest_size,
            CasObjectRole::OwnerManifest,
        ));
    } else if owner_head.manifest_hash.is_some()
        || owner_head.entry_count != 0
        || !content.assets.is_empty()
    {
        return Err("absent RISUM owner head is inconsistent".to_owned());
    }
    let job = recover_job(jobs, repository_root, session_id)?;
    if job.kind() != CasJobKind::CardOrModuleContentImport
        || job.is_sealed()
        || job.is_released()
        || !job.has_exact_pins(&expected)
    {
        return Err("prepared RISUM CAS pin set does not match its receipt".to_owned());
    }
    job.seal(store, created_at_ms)
        .map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_seal_prepared_content(
    app: AppHandle,
    native_jobs: State<'_, NativeFileJobState>,
    session_id: String,
) -> Result<(), String> {
    let native_jobs = NativeFileJobState::clone(&native_jobs);
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        let root = repository_root(&app)?;
        let state = app.state::<DurableCasJobState>();
        let mut jobs = state
            .jobs
            .lock()
            .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?;
        persistent_store::commands::with_store_mut(app.state::<PersistentStoreState>(), |store| {
            seal_prepared_content_job_by_id(
                &native_jobs,
                &root,
                &mut jobs,
                store,
                &session_id,
                now_ms().map_err(|message| StoreError::Store { message })?,
            )
            .map_err(|message| StoreError::Store { message })
        })
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("failed to join prepared content CAS seal: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_repository::job_pins::{
        collect_durable_cas_job_roots, CasJobKind, DurableCasJob,
    };
    use crate::asset_repository::owner_manifest_codec::{
        encode_owner_manifest, OwnerManifestEntry,
    };
    use crate::native_file_jobs::{
        JobSource, JobState, NativeFileJobStartRequest, PreparedContent,
    };
    use crate::persistent_store::PersistentStore;
    use serde_json::{json, Value};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn owner_manifest() -> Vec<u8> {
        encode_owner_manifest(&[OwnerManifestEntry {
            tuple: [
                "character".to_owned(),
                "card-1".to_owned(),
                "icon".to_owned(),
            ],
            payload_hash: None,
        }])
        .unwrap()
    }

    fn owner_manifest_with_payload(payload_hash: &str) -> Vec<u8> {
        let payload_hash: [u8; 32] = hex::decode(payload_hash).unwrap().try_into().unwrap();
        encode_owner_manifest(&[OwnerManifestEntry {
            tuple: [
                "character".to_owned(),
                "card-1".to_owned(),
                "icon".to_owned(),
            ],
            payload_hash: Some(payload_hash),
        }])
        .unwrap()
    }

    fn begin_content_job(root: &std::path::Path, id: &str) -> DurableCasJob {
        DurableCasJob::begin(root, id, CasJobKind::CardOrModuleContentImport, 1).unwrap()
    }

    fn risum_fixture(module: Value, assets: &[&[u8]]) -> Vec<u8> {
        let map = include_bytes!("../../../src/ts/rpack/rpack_map.bin");
        let encode = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| map[*byte as usize])
                .collect::<Vec<_>>()
        };
        let metadata = encode(
            serde_json::to_string(&json!({ "type": "risuModule", "module": module }))
                .unwrap()
                .as_bytes(),
        );
        let mut bytes = vec![111, 0];
        bytes.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&metadata);
        for asset in assets {
            let encoded = encode(asset);
            bytes.push(1);
            bytes.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&encoded);
        }
        bytes.push(0);
        bytes
    }

    fn prepare_risum_job(
        repository_root: &std::path::Path,
        native_jobs: &NativeFileJobState,
        name: &str,
        assets: &[&[u8]],
    ) -> (String, PreparedContent) {
        let source = repository_root.join(name);
        let tuples = assets
            .iter()
            .enumerate()
            .map(|(index, _)| json!([format!("asset-{index}"), "", "bin"]))
            .collect::<Vec<_>>();
        std::fs::write(
            &source,
            risum_fixture(json!({ "name": name, "assets": tuples }), assets),
        )
        .unwrap();
        let started = native_jobs
            .start_content_for_test(NativeFileJobStartRequest::PrepareContentImport {
                source: JobSource::DesktopPath {
                    path: source.to_string_lossy().into_owned(),
                },
                display_name: name.to_owned(),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = native_jobs.status(&started.job_id).unwrap();
            if status.state == JobState::Succeeded {
                return (
                    started.job_id.clone(),
                    native_jobs
                        .prepared_content_receipt(&started.job_id)
                        .unwrap(),
                );
            }
            assert!(
                !matches!(status.state, JobState::Failed | JobState::Cancelled),
                "RISUM preparation failed: {:?}",
                status.error,
            );
            assert!(Instant::now() < deadline, "RISUM preparation timed out");
            thread::yield_now();
        }
    }

    #[test]
    fn content_finalizer_prioritizes_owner_manifest_when_a_direct_hash_matches() {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let manifest = owner_manifest();
        let existing = cas.prepare_bytes(&manifest).unwrap();
        let mut job = begin_content_job(directory.path(), "content-owner-priority");

        let prepared = finalize_content_job(
            &mut job,
            &cas,
            &mut store,
            &manifest,
            &[ContentDirectObject {
                object_hash: existing.content_hash.clone(),
                byte_size: existing.byte_size,
            }],
            2,
        )
        .unwrap();

        assert_eq!(prepared.content_hash, existing.content_hash);
        assert_eq!(prepared.byte_size, manifest.len() as u64);
        assert_eq!(
            cas.read_object(&prepared.content_hash).unwrap(),
            Some(manifest)
        );
        assert!(job.is_sealed());
        assert_eq!(job.pin_count(), 1);
        let roots = job.root_set().unwrap();
        assert_eq!(roots.manifest_hashes, [prepared.content_hash].into());
        assert!(roots.object_hashes.is_empty());
    }

    #[test]
    fn content_finalizer_normalizes_exact_direct_duplicates_and_rejects_size_conflicts() {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let direct = cas.prepare_bytes(b"direct object").unwrap();
        let manifest = owner_manifest();
        let duplicate = ContentDirectObject {
            object_hash: direct.content_hash.clone(),
            byte_size: direct.byte_size,
        };
        let mut normalized = begin_content_job(directory.path(), "content-normalized");

        finalize_content_job(
            &mut normalized,
            &cas,
            &mut store,
            &manifest,
            &[duplicate.clone(), duplicate.clone()],
            2,
        )
        .unwrap();

        assert!(normalized.is_sealed());
        assert_eq!(normalized.pin_count(), 2);

        let mut conflict = begin_content_job(directory.path(), "content-conflict");
        let error = finalize_content_job(
            &mut conflict,
            &cas,
            &mut store,
            &manifest,
            &[
                duplicate,
                ContentDirectObject {
                    object_hash: direct.content_hash,
                    byte_size: direct.byte_size + 1,
                },
            ],
            2,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(conflict.pin_count(), 0);
        assert!(!conflict.is_sealed());
    }

    #[test]
    fn content_finalizer_rejects_the_wrong_job_kind_before_writing() {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut job = DurableCasJob::begin(
            directory.path(),
            "wrong-kind",
            CasJobKind::LocalBackupRestore,
            1,
        )
        .unwrap();
        let manifest = owner_manifest();
        let manifest_hash = hex::encode(Sha256::digest(&manifest));

        let error =
            finalize_content_job(&mut job, &cas, &mut store, &manifest, &[], 2).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(job.pin_count(), 0);
        assert!(!job.is_sealed());
        assert_eq!(cas.stat_object(&manifest_hash).unwrap(), None);
    }

    #[test]
    fn content_finalizer_validates_all_direct_objects_before_manifest_prepare() {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let manifest = owner_manifest();
        let manifest_hash = hex::encode(Sha256::digest(&manifest));
        let mut job = begin_content_job(directory.path(), "content-missing-direct");

        let error = finalize_content_job(
            &mut job,
            &cas,
            &mut store,
            &manifest,
            &[ContentDirectObject {
                object_hash: "0".repeat(64),
                byte_size: 1,
            }],
            2,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(job.pin_count(), 0);
        assert!(!job.is_sealed());
        assert_eq!(cas.stat_object(&manifest_hash).unwrap(), None);
    }

    #[test]
    fn content_finalizer_rejects_an_unlisted_manifest_payload_before_owner_prepare() {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let unlisted = cas
            .prepare_bytes(b"present in CAS but omitted from receipt")
            .unwrap();
        let manifest = owner_manifest_with_payload(&unlisted.content_hash);
        let manifest_hash = hex::encode(Sha256::digest(&manifest));
        let mut job = begin_content_job(directory.path(), "content-unlisted-payload");

        let error =
            finalize_content_job(&mut job, &cas, &mut store, &manifest, &[], 2).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(job.pin_count(), 0);
        assert!(!job.is_sealed());
        assert_eq!(cas.stat_object(&manifest_hash).unwrap(), None);
    }

    #[test]
    fn content_finalizer_seals_manifest_and_direct_pins_into_catalog_and_gc_roots() {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let direct = cas.prepare_bytes(b"direct object").unwrap();
        let manifest = owner_manifest_with_payload(&direct.content_hash);
        let mut job = begin_content_job(directory.path(), "content-finalized");

        let prepared = finalize_content_job(
            &mut job,
            &cas,
            &mut store,
            &manifest,
            &[ContentDirectObject {
                object_hash: direct.content_hash.clone(),
                byte_size: direct.byte_size,
            }],
            7,
        )
        .unwrap();

        assert!(job.is_sealed());
        assert_eq!(job.pin_count(), 2);
        let roots = collect_durable_cas_job_roots(directory.path());
        assert_eq!(
            roots.manifest_hashes,
            [prepared.content_hash.clone()].into()
        );
        assert_eq!(roots.object_hashes, [direct.content_hash.clone()].into());
        assert!(roots.blockers.is_empty());
        let catalog = store.query_asset_object_catalog(16, None).unwrap();
        assert_eq!(catalog.items.len(), 2);
        assert!(catalog
            .items
            .iter()
            .any(|item| item.object_hash == prepared.content_hash));
        assert!(catalog
            .items
            .iter()
            .any(|item| item.object_hash == direct.content_hash));
    }

    #[test]
    fn prepared_risum_seal_recovers_exact_job_roots_by_id_and_rejects_mismatch_or_reuse() {
        let directory = TempDir::new().unwrap();
        let native_jobs = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let (job_id, content) = prepare_risum_job(
            directory.path(),
            &native_jobs,
            "exact.risum",
            &[b"first exact asset", b"second exact asset"],
        );
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let mut recovered_jobs = HashMap::new();

        seal_prepared_content_job_by_id(
            &native_jobs,
            directory.path(),
            &mut recovered_jobs,
            &mut store,
            &job_id,
            7,
        )
        .unwrap();

        let sealed = recovered_jobs.get(&job_id).unwrap();
        assert!(sealed.is_sealed());
        assert_eq!(sealed.pin_count(), 3);
        let roots = sealed.root_set().unwrap();
        assert_eq!(
            roots.object_hashes,
            content
                .assets
                .iter()
                .map(|asset| asset.object_hash.clone())
                .collect::<std::collections::BTreeSet<_>>(),
        );
        assert_eq!(
            roots.manifest_hashes,
            [content
                .owner_head
                .as_ref()
                .and_then(|head| head.manifest_hash.clone())
                .unwrap()]
            .into(),
        );

        let reuse = seal_prepared_content_job_by_id(
            &native_jobs,
            directory.path(),
            &mut recovered_jobs,
            &mut store,
            &job_id,
            8,
        )
        .unwrap_err();
        assert!(reuse.contains("pin set does not match"));
        assert!(recovered_jobs.get(&job_id).unwrap().is_sealed());

        let (mismatch_id, _) = prepare_risum_job(
            directory.path(),
            &native_jobs,
            "mismatch.risum",
            &[b"expected asset"],
        );
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut mismatched = DurableCasJob::open(directory.path(), &mismatch_id).unwrap();
        mismatched
            .prepare_bytes(&cas, b"unlisted extra pin", CasObjectRole::DirectObject)
            .unwrap();
        drop(mismatched);

        let mismatch = seal_prepared_content_job_by_id(
            &native_jobs,
            directory.path(),
            &mut recovered_jobs,
            &mut store,
            &mismatch_id,
            9,
        )
        .unwrap_err();
        assert!(mismatch.contains("pin set does not match"));
        assert!(!recovered_jobs.get(&mismatch_id).unwrap().is_sealed());
    }
}
