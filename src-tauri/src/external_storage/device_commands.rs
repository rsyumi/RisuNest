//! Durable bridge from external backup jobs to renderer-owned device capture.

use super::{
    connection_store::ConnectionStore,
    contract::{ErrorKind, ProviderError, Result},
    device_capture::{
        capture_root, reopen_device_snapshot, seal_device_snapshot, validate_snapshot_scope,
    },
    job_store::{DurableJob, JobKind, JobStore},
    runtime,
};
use crate::{
    device_backup::{DeviceBackupState, Operation},
    local_backup::CancellationProbe,
};
use risunest_external_storage_format::format::Scope;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tauri::{AppHandle, Manager};

fn workers() -> &'static Mutex<HashSet<String>> {
    static WORKERS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    WORKERS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

fn transient(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PrepareDeviceCaptureRequest {
    consumer_ids: Vec<String>,
    device_sections: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceCapturePreparation {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

struct ValidatedBatch {
    consumer_ids: Vec<String>,
    scope: Scope,
    sections: Vec<String>,
    ready_capture: Option<String>,
}

fn validate_sections(scope: &Scope, sections: &[String]) -> Result<()> {
    validate_snapshot_scope(scope, sections).map_err(|_| corrupt())
}

fn validate_batch(root: &Path, request: PrepareDeviceCaptureRequest) -> Result<ValidatedBatch> {
    if request.consumer_ids.is_empty() || request.consumer_ids.len() > 64 {
        return Err(corrupt());
    }
    let mut consumer_ids = request.consumer_ids;
    if consumer_ids
        .iter()
        .any(|id| id.is_empty() || id.len() > 1024 || id.contains('\0'))
    {
        return Err(corrupt());
    }
    consumer_ids.sort();
    if consumer_ids.windows(2).any(|ids| ids[0] == ids[1]) {
        return Err(corrupt());
    }
    let jobs = JobStore::open(root)?;
    let connections = ConnectionStore::open(root)?;
    let mut scope = None;
    let mut ready_capture = None;
    let mut all_ready = true;
    for id in &consumer_ids {
        let job = jobs.read(id)?;
        if job.request.kind != JobKind::Backup || job.terminal() {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        let connection = connections.read(&job.request.connection_id)?;
        let candidate = connection.descriptor.scope;
        if connection.descriptor.publication_strategy.is_some()
            || (!candidate.device_settings && !candidate.device_plugins)
        {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
        if scope.as_ref().is_some_and(|value| value != &candidate) {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        scope = Some(candidate);
        match (job.summary["phase"].as_str(), &job.device_capture_id) {
            (Some("device-capture"), None) => all_ready = false,
            (Some("queued"), Some(capture)) => {
                if ready_capture.as_ref().is_some_and(|value| value != capture) {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                ready_capture = Some(capture.clone());
            }
            _ => return Err(ProviderError::new(ErrorKind::PreconditionFailed)),
        }
    }
    let scope = scope.ok_or_else(corrupt)?;
    validate_sections(&scope, &request.device_sections)?;
    if all_ready && ready_capture.is_none() {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    if !all_ready && ready_capture.is_some() {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(ValidatedBatch {
        consumer_ids,
        scope,
        sections: request.device_sections,
        ready_capture: if all_ready { ready_capture } else { None },
    })
}

fn same_device_scope(job: &DurableJob, root: &Path, scope: &Scope) -> bool {
    job.request.kind == JobKind::Backup
        && !job.terminal()
        && job.summary["phase"] == "device-capture"
        && job.device_capture_id.is_none()
        && ConnectionStore::open(root)
            .and_then(|connections| connections.read(&job.request.connection_id))
            .is_ok_and(|connection| {
                connection.descriptor.publication_strategy.is_none()
                    && connection.descriptor.scope == *scope
            })
}

struct LiveConsumers {
    root: PathBuf,
    scope: Scope,
}

impl LiveConsumers {
    fn ids(&self) -> Result<Vec<String>> {
        Ok(JobStore::open(&self.root)?
            .list_pending()?
            .into_iter()
            .filter(|job| same_device_scope(job, &self.root, &self.scope))
            .map(|job| job.id)
            .collect())
    }
}

impl CancellationProbe for LiveConsumers {
    fn is_cancelled(&self) -> bool {
        self.ids().map_or(true, |ids| ids.is_empty())
    }
}

fn cleanup_after_worker(state: &DeviceBackupState, session_id: &str) {
    while state.is_blocking().unwrap_or(true) {
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Err(error) = state.cleanup(session_id) {
        crate::nlog!("error", "External device capture cleanup failed: {error}");
    }
}

fn run_capture_worker(app: AppHandle, session_id: String, root: PathBuf, scope: Scope) {
    let state = app.state::<DeviceBackupState>();
    let renderer_finished = loop {
        match state.session(&session_id) {
            Ok(session) if session.phase == "device-captured" => break true,
            Ok(session) if session.phase == "capturing" => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(_) | Err(_) => break false,
        }
    };
    if renderer_finished {
        let live = LiveConsumers {
            root: root.clone(),
            scope,
        };
        let result = (|| -> Result<()> {
            let snapshot = seal_device_snapshot(&state, &session_id, &capture_root(&root), &live)
                .map_err(transient)?;
            let ids = live.ids()?;
            if ids.is_empty() {
                return Err(ProviderError::new(ErrorKind::Cancelled));
            }
            JobStore::open(&root)?.attach_device(&ids, &snapshot.capture_id)?;
            state.confirm_capture(&session_id).map_err(transient)?;
            Ok(())
        })();
        if let Err(error) = result {
            crate::nlog!("error", "External device capture sealing failed: {error:?}");
            let _ = state.fail(&session_id, "external-device-capture-failed");
        }
        cleanup_after_worker(&state, &session_id);
    }
    if let Ok(mut active) = workers().lock() {
        active.remove(&session_id);
    }
}

fn spawn_capture_worker(
    app: AppHandle,
    session_id: String,
    root: PathBuf,
    scope: Scope,
) -> Result<()> {
    {
        let mut active = workers().lock().map_err(transient)?;
        if !active.insert(session_id.clone()) {
            return Ok(());
        }
    }
    let worker_session = session_id.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("external-device-capture".into())
        .spawn(move || run_capture_worker(app, worker_session, root, scope))
    {
        if let Ok(mut active) = workers().lock() {
            active.remove(&session_id);
        }
        return Err(transient(error));
    }
    Ok(())
}

#[tauri::command(async)]
pub(crate) fn external_storage_prepare_device_capture(
    app: AppHandle,
    request: PrepareDeviceCaptureRequest,
) -> Result<DeviceCapturePreparation> {
    let root = runtime::root(&app)?;
    let batch = validate_batch(&root, request)?;
    if let Some(capture_id) = batch.ready_capture {
        let snapshot =
            reopen_device_snapshot(&capture_root(&root), &capture_id).map_err(transient)?;
        if snapshot.sections != batch.sections {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        return Ok(DeviceCapturePreparation {
            state: "ready",
            capture_id: Some(capture_id),
            session_id: None,
        });
    }
    if app.webview_windows().len() != 1 {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let coordinator = app.state::<DeviceBackupState>();
    if coordinator.is_blocking().map_err(transient)? {
        let decision = coordinator.bootstrap().map_err(transient)?;
        let session = decision
            .session
            .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
        if session.operation != Operation::Capture
            || !batch.consumer_ids.contains(&session.job_id)
            || session.selected_sections != batch.sections
            || !matches!(session.phase.as_str(), "capturing" | "device-captured")
        {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        spawn_capture_worker(app.clone(), session.session_id.clone(), root, batch.scope)?;
        return Ok(DeviceCapturePreparation {
            state: "maintenance-required",
            capture_id: None,
            session_id: Some(session.session_id),
        });
    }
    let admission = app
        .state::<crate::native_file_jobs::NativeFileJobState>()
        .admission
        .file(true)
        .map_err(transient)?;
    let guard = app
        .state::<crate::persistent_store::PersistentStoreState>()
        .acquire_device_maintenance()
        .map_err(transient)?;
    coordinator
        .attach_startup_admission(admission)
        .map_err(transient)?;
    if let Err(error) = coordinator.attach_maintenance_guard(guard) {
        let _ = coordinator.release_unused_maintenance();
        return Err(transient(error));
    }
    let session_id = match coordinator.create_session(
        &batch.consumer_ids[0],
        Operation::Capture,
        false,
        &batch.sections,
        None,
        None,
    ) {
        Ok(id) => id,
        Err(error) => {
            let _ = coordinator.release_unused_maintenance();
            return Err(transient(error));
        }
    };
    if let Err(error) = spawn_capture_worker(app.clone(), session_id.clone(), root, batch.scope) {
        let _ = coordinator.fail(&session_id, "external-device-worker-failed");
        let _ = coordinator.recovery_complete(&session_id);
        let _ = coordinator.cleanup(&session_id);
        return Err(error);
    }
    Ok(DeviceCapturePreparation {
        state: "maintenance-required",
        capture_id: None,
        session_id: Some(session_id),
    })
}

#[cfg(test)]
#[path = "device_commands_tests.rs"]
mod tests;
