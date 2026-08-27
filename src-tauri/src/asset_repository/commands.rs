use super::job_pins::{CasJobKind, CasObjectRole, CasReleaseOutcome, DurableCasJob};
use super::{PayloadCas, PreparedPayload};
use crate::persistent_store::{self, PersistentStoreState, StoreError};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

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
    app.path()
        .app_data_dir()
        .map_err(|error| format!("failed to resolve application data directory: {error}"))
}

fn now_ms() -> Result<i64, String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| "system time exceeds the supported millisecond range".to_owned())
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
    if !jobs.contains_key(session_id) {
        let recovered =
            DurableCasJob::open(&root, session_id).map_err(|error| error.to_string())?;
        jobs.insert(session_id.to_owned(), recovered);
    }
    operation(
        jobs.get_mut(session_id)
            .expect("recovered CAS job session must be present"),
        &cas,
    )
    .map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_begin(
    app: AppHandle,
    kind: CasJobKind,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
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
        let root = repository_root(&app)?;
        let state = app.state::<DurableCasJobState>();
        let mut jobs = state
            .jobs
            .lock()
            .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?;
        if !jobs.contains_key(&session_id) {
            let recovered =
                DurableCasJob::open(&root, &session_id).map_err(|error| error.to_string())?;
            jobs.insert(session_id.clone(), recovered);
        }
        let job = jobs
            .get_mut(&session_id)
            .expect("recovered CAS job session must be present");
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
        let root = repository_root(&app)?;
        let state = app.state::<DurableCasJobState>();
        let mut jobs = state
            .jobs
            .lock()
            .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?;
        if !jobs.contains_key(&session_id) {
            let recovered =
                DurableCasJob::open(&root, &session_id).map_err(|error| error.to_string())?;
            jobs.insert(session_id.clone(), recovered);
        }
        let release_result = jobs
            .get_mut(&session_id)
            .expect("recovered CAS job session must be present")
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
pub(crate) async fn asset_cas_prepare(
    app: AppHandle,
    data: Vec<u8>,
) -> Result<PreparedPayload, String> {
    let root = repository_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        PayloadCas::new(&root)
            .and_then(|cas| cas.prepare_bytes(&data))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("failed to join CAS prepare operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_read_object(
    app: AppHandle,
    content_hash: String,
) -> Result<Option<Vec<u8>>, String> {
    let root = repository_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
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
        PayloadCas::new(&root)
            .and_then(|cas| cas.stat_object(&content_hash))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("failed to join CAS stat operation: {error}"))?
}
