//! Native job admission and bounded renderer DTOs. Network stages own no PDS mutex.
use super::{
    connection_commands::ConnectedRepository,
    connection_store::ConnectionStore,
    contract::*,
    job_store::{DurableJob, JobCommandState, JobKind, JobStore, Session, StartJobRequest},
    leases,
};
use crate::persistent_store::{
    self,
    sync_selection::{Selection, SyncTarget},
    PersistentStore,
};
use crate::native_log::logged;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use tauri::{AppHandle, Manager, Runtime};

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
pub(crate) fn local_error(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::LocalFailure).caused(&error)
}
pub(crate) fn root<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf> {
    app.state::<JobCommandState>()
        .root
        .get()
        .cloned()
        .ok_or_else(|| ProviderError::new(ErrorKind::Transient))
}
pub(crate) fn native_store<R: Runtime>(app: &AppHandle<R>) -> Result<PersistentStore> {
    persistent_store::commands::with_store_mut(app.state(), |store| store.open_native_job_store())
        .map_err(local_error)
}
pub(crate) fn record_connection_completion(
    app: &AppHandle,
    connection_id: &str,
    kind: super::connection_store::CompletionKind,
) {
    match root(app) {
        Ok(root) => record_connection_completion_at(&root, connection_id, kind),
        Err(_) => crate::nlog!("warn", "External connection completion timestamp could not be recorded"),
    }
}
pub(crate) fn record_connection_completion_at(
    root: &std::path::Path,
    connection_id: &str,
    kind: super::connection_store::CompletionKind,
) {
    let result = ConnectionStore::open(root)
        .and_then(|mut store| store.record_completion(connection_id, kind, now_ms()));
    if result.is_err() {
        crate::nlog!("warn", "External connection completion timestamp could not be recorded");
    }
}
pub(crate) fn selection_dto(selection: Selection) -> Value {
    let (kind, id) = match selection.target {
        SyncTarget::None => ("none", None),
        SyncTarget::Server(id) => ("server", Some(id)),
        SyncTarget::External(id) => ("external", Some(id)),
    };
    let mut dto = json!({"kind":kind,"selectionEpoch":selection.epoch,"paused":selection.paused});
    if let Some(id) = id {
        dto["connectionId"] = json!(id);
    }
    dto
}
fn require_session(request: &StartJobRequest, current: &Session) -> Result<()> {
    let manual = request.reason.as_deref().unwrap_or("manual") == "manual";
    if current.id.is_empty() || current.kind == "hidden" {
        return Err(ProviderError::new(ErrorKind::Cancelled));
    }
    if !manual || request.session_id.is_some() || request.session.is_some() {
        if request.session_id.as_deref() != Some(current.id.as_str())
            || request.session.as_deref() != Some(current.kind.as_str())
        {
            return Err(ProviderError::new(ErrorKind::Cancelled));
        }
    }
    if current.kind != "foreground" {
        return Err(ProviderError::new(ErrorKind::Cancelled));
    }
    Ok(())
}
pub(crate) fn read_job_session(app: &AppHandle, id: &str) -> Result<()> {
    let job = JobStore::open(&root(app)?)?.read(id)?;
    let state = app.state::<JobCommandState>();
    let current = state.session.lock().map_err(local_error)?;
    require_session(&job.request, &current)
}

/// Ends the jobs of a connection being removed. A job whose remote outcome is
/// unknown, or a restore already applying on this device, keeps its row until
/// that is resolved.
pub(crate) fn end_removed_connection_jobs(root: &std::path::Path, connection: &str) -> Result<()> {
    JobStore::open(root)?.end_connection_jobs(connection, |job| {
        if job.summary["state"] == "uncertain" || super::runtime_restore::application_started(job) {
            return false;
        }
        settle_invalidated(job, "cancelled");
        true
    })
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SetSessionRequest {
    kind: String,
    id: String,
}
#[tauri::command]
pub(crate) fn external_storage_set_execution_session(
    app: AppHandle,
    request: SetSessionRequest,
) -> Result<()> {
    logged("external_storage_set_execution_session", (|| {
        if !["foreground", "hidden"].contains(&request.kind.as_str())
            || request.id.is_empty()
            || request.id.len() > 1024
        {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        let state = app.state::<JobCommandState>();
        let mut session = state.session.lock().map_err(local_error)?;
        let hidden = request.kind == "hidden";
        *session = Session {
            kind: request.kind,
            id: request.id,
        };
        leases::set_system_foreground(!hidden);
        if hidden {
            for (_, cancel) in state.active.lock().map_err(local_error)?.values() {
                cancel.cancel();
            }
        }
        Ok(())
    })())
}
#[tauri::command(async)]
pub(crate) fn external_storage_capture_exit_target(app: AppHandle) -> Result<Value> {
    logged("external_storage_capture_exit_target", (|| {
        let store = native_store(&app)?;
        let identity = store.external_identity().map_err(local_error)?;
        Ok(
            json!({"revision":identity.revision.to_string(),"libraryEpoch":identity.library_epoch,"selection":selection_dto(store.external_selection().map_err(local_error)?)}),
        )
    })())
}
#[tauri::command(async)]
pub(crate) fn external_storage_get_state<R: Runtime>(app: AppHandle<R>) -> Result<Value> {
    logged("external_storage_get_state", (|| {
        let root = root(&app)?;
        let connection_store = ConnectionStore::open(&root)?;
        let mut connections = connection_store.list()?
            .iter()
            .map(|connection| {
                let mut summary = serde_json::to_value(super::connection_commands::summary(connection)?).map_err(local_error)?;
                let paused = connection_store.automatic_backup_paused(&connection.id)?;
                summary["automaticBackupPaused"] = json!(paused);
                if paused { summary["status"] = json!("paused"); }
                Ok(summary)
            })
            .collect::<Result<Vec<_>>>()?;
        let jobs = JobStore::open(&root)?
            .list_for_state()?
            .into_iter()
            .map(|job| reconcile_job_for_read(&app, job).map(|job| {
                job_summary(&root, job)
            }))
            .collect::<Result<Vec<_>>>()?;
        for connection in &mut connections { apply_job_connection_status(connection, &jobs); }
        let state = app.state::<JobCommandState>();
        let selection = match native_store(&app).and_then(|store| store.external_selection().map_err(local_error)) {
            Ok(selection) => shown_selection(&state, selection)?,
            Err(error) => match state.shown_selection.lock().map_err(local_error)?.clone() {
                Some(selection) if store_held(&app, &error) => selection,
                _ => return Err(error),
            },
        };
        Ok(json!({"supported":true,"selection":selection,"connections":connections,"jobs":jobs}))
    })())
}

fn shown_selection(state: &JobCommandState, selection: Selection) -> Result<Value> {
    let selection = selection_dto(selection);
    *state.shown_selection.lock().map_err(local_error)? = Some(selection.clone());
    Ok(selection)
}

/// Device maintenance refuses the store while it holds it, as a restore commit
/// does.
fn store_held<R: Runtime>(app: &AppHandle<R>, error: &ProviderError) -> bool {
    matches!(error.kind, ErrorKind::Transient | ErrorKind::LocalFailure)
        && app.state::<persistent_store::commands::PersistentStoreState>().maintenance_active()
}

/// A job as stored while device maintenance holds the store. The first read
/// after it reconciles the job.
fn reconcile_job_for_read<R: Runtime>(app: &AppHandle<R>, job: DurableJob) -> Result<DurableJob> {
    let stored = job.clone();
    match reconcile_job(app, job) {
        Err(error) if store_held(app, &error) => Ok(stored),
        reconciled => reconciled,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SetTargetRequest {
    connection_id: Option<String>,
    expected_selection_epoch: String,
}
#[tauri::command]
pub(crate) async fn external_storage_set_sync_target(
    app: AppHandle,
    request: SetTargetRequest,
) -> Result<Value> {
    logged("external_storage_set_sync_target", async move {
        let target = match request.connection_id {
            Some(id) => {
                let connection = ConnectionStore::open(&root(&app)?)?.read(&id)?;
                let strategy = connection
                    .descriptor
                    .publication_strategy
                    .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
                connection.capabilities.require(strategy)?;
                SyncTarget::External(id)
            }
            None => SyncTarget::None,
        };
        let state = app.state::<JobCommandState>();
        let active = state.active.lock().map_err(local_error)?;
        if !active.is_empty() {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        let admission = app
            .state::<crate::native_file_jobs::NativeFileJobState>()
            .admission
            .clone();
        let _permit = admission.file(true).map_err(local_error)?;
        let selected = native_store(&app)?
            .external_select(&request.expected_selection_epoch, &target)
            .map_err(local_error)?;
        drop(active);
        shown_selection(&state, selected)
    }.await)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SetPausedRequest {
    paused: bool,
    expected_selection_epoch: String,
}
#[tauri::command(async)]
pub(crate) fn external_storage_set_sync_paused(app: AppHandle, request: SetPausedRequest) -> Result<Value> {
    logged("external_storage_set_sync_paused", (|| {
        let selected = native_store(&app)?.external_set_paused(&request.expected_selection_epoch, request.paused).map_err(local_error)?;
        shown_selection(&app.state::<JobCommandState>(), selected)
    })())
}
#[tauri::command]
pub(crate) fn external_storage_set_automatic_backup_paused(app: AppHandle, connection_id: String, paused: bool) -> Result<()> {
    logged("external_storage_set_automatic_backup_paused", (|| ConnectionStore::open(&root(&app)?)?.set_automatic_backup_paused(&connection_id, paused))())
}
#[tauri::command(async)]
pub(crate) fn external_storage_get_job<R: Runtime>(app: AppHandle<R>, job_id: String) -> Result<Value> {
    logged("external_storage_get_job", (|| {
        let directory = root(&app)?;
        let job = reconcile_job_for_read(&app, JobStore::open(&directory)?.read(&job_id)?)?;
        Ok(job_summary(&directory, job))
    })())
}
fn apply_job_connection_status(connection: &mut Value, jobs: &[Value]) {
    // State rows are newest first. A later successful retry clears an older error.
    let Some(job) = jobs.iter().find(|job| job["connectionId"] == connection["id"]) else { return; };
    if matches!(job["state"].as_str(), Some("succeeded" | "cancelled")) { return; }
    let verified = connection["lastVerifiedAtMs"].as_str().and_then(|value| value.parse::<u64>().ok());
    let failed = job["updatedAtMs"].as_str().and_then(|value| value.parse::<u64>().ok());
    if verified.zip(failed).is_some_and(|(verified, failed)| verified >= failed) { return; }
    let error = &job["error"];
    let status = match error["action"].as_str() {
        Some("reauthenticate") => "reauth-required",
        Some("unlock-key") => "key-locked",
        Some("check-endpoint" | "free-space") => "error",
        Some("retry") if error["retryable"] == false => "error",
        _ => return,
    };
    connection["status"] = json!(status);
    connection["lastError"] = error.clone();
}

fn job_summary(root: &std::path::Path, mut job: DurableJob) -> Value {
    if !job.terminal() {
        if let Some(progress) = super::progress::job_snapshot(&job_directory(root, &job.request.connection_id, &job.id)) {
            job.summary["transfer"] = json!(progress);
        }
    }
    job.summary["reason"] = json!(job.request.reason);
    job.summary["targetRevision"] = json!(job.request.target_revision);
    if job.request.kind == JobKind::PinHistory {
        job.summary["pinRequest"] = json!({"snapshotId":job.request.snapshot_id});
    }
    if job.request.kind == JobKind::DeleteHistory {
        job.summary["deleteRequest"] = json!({"pointId":job.request.point_id,"pointObservation":job.request.point_observation,
            "confirmOtherDevice":job.request.confirm_other_device,"confirmLastRetained":job.request.confirm_last_retained});
    }
    if job.request.kind == JobKind::Restore {
        job.summary["restoreRequest"] = json!({
            "snapshotId": job.request.snapshot_id,
            "targetRevision": job.request.target_revision,
            "restoreAreas": job.request.restore_areas.clone()
                .unwrap_or_else(|| vec!["library".into(), "referencedAssets".into()]),
        });
    }
    if job.request.kind == JobKind::CheckRepository {
        // The scope a paused check resumes on, which its result no longer
        // carries once the job is settled.
        job.summary["checkRequest"] = json!({"snapshotId": job.request.snapshot_id});
    }
    // A publication uploads packs while it still prepares the rest, so its
    // preparation keeps the counters until it has done all it planned.
    if !job.terminal() && !preparing(&job.summary) {
        refresh_transfer_counters(root, &mut job);
    }
    job.summary
}

fn preparing(summary: &Value) -> bool {
    let count = |name: &str| summary[name].as_str().and_then(|value| value.parse::<u64>().ok());
    summary["counters"] == "prepared"
        && matches!((count("completedItems"), count("totalItems")), (Some(done), Some(total)) if done < total)
}

fn refresh_transfer_counters(root: &std::path::Path, job: &mut DurableJob) {
    if let Ok((done, total, items, count)) = super::journal::TransferJournal::progress(
        &job_directory(root, &job.request.connection_id, &job.id),
        &job.id,
    ) {
        // An empty journal is a job that has registered nothing yet, and what
        // it prepared is the only thing it knows. The first registration is
        // what hands the counters over.
        if count > 0 {
            job.summary["counters"] = json!("transferred");
            job.summary["completedBytes"] = json!(done.to_string());
            job.summary["totalBytes"] = json!(total.to_string());
            job.summary["completedItems"] = json!(items.to_string());
            job.summary["totalItems"] = json!(count.to_string());
        }
    }
}
#[tauri::command]
pub(crate) async fn external_storage_cancel_job(app: AppHandle, job_id: String, stopped_by_app: Option<bool>) -> Result<Value> {
    logged("external_storage_cancel_job", async move {
        let store = JobStore::open(&root(&app)?)?;
        let state = app.state::<JobCommandState>();
        let observed = store.read(&job_id)?;
        if super::runtime_restore::application_started(&observed) && !observed.terminal()
            && observed.summary.get("restoreAdopted").is_none() {
            return Ok(reconcile_job(&app, observed)?.summary);
        }
        {
            if let Some((_, cancel)) = state.active.lock().map_err(local_error)?.get(&job_id) {
                cancel.cancel();
            }
        }
        wait_for_job_release(&app, &job_id).await?;
        // Keep a new worker from claiming the job while cancellation settles it.
        let (_, _claim) = state.claim(&store.read(&job_id)?)?;
        let _permit = app.state::<crate::native_file_jobs::NativeFileJobState>()
            .admission.file(false).map_err(local_error)?;
        let mut job = reconcile_stopped_job(&app, store.read(&job_id)?)?;
        if super::runtime_restore::application_started(&job) && !job.terminal() {
            return Ok(job.summary);
        }
        if job.summary["state"] != "succeeded" {
            let mut pds = native_store(&app)?;
            let authoritative = pds.external_job(&job_id).map_err(local_error)?;
            if authoritative.as_ref().is_some_and(|item| {
                ["publishing", "publicationUnknown", "applying"].contains(&item.phase.as_str())
            }) {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
            if authoritative
                .as_ref()
                .is_some_and(|item| ["preparing", "ready", "stale"].contains(&item.phase.as_str()))
            {
                pds.external_cancel_prepared(&job_id).map_err(local_error)?;
            }
            settle_cancelled(&mut job, stopped_by_app == Some(true));
            store.put(&job)?;
        }
        super::runtime_restore::discard_finished_staging(&root(&app)?, &job);
        Ok(job.summary)
    }.await)
}
/// Settles a job as cancelled. A job the app gave up on after its own retries keeps that
/// mark, so the renderer shows its error instead of a cancellation the user never made.
fn settle_cancelled(job: &mut DurableJob, stopped_by_app: bool) {
    job.summary["state"] = json!("cancelled");
    job.summary["phase"] = json!("cancelled");
    job.summary["updatedAtMs"] = json!(now_ms().to_string());
    if stopped_by_app { job.summary["stoppedByApp"] = json!(true); }
}
#[tauri::command]
pub(crate) async fn external_storage_stop_restore(app: AppHandle, job_id: String) -> Result<Value> {
    logged("external_storage_stop_restore", async move {
        let root = root(&app)?;
        let store = JobStore::open(&root)?;
        // A restore that is running again continues instead of stopping, and
        // the claim keeps a worker from starting it while it stops.
        let (_, _claim) = app.state::<JobCommandState>().claim_restore_settlement(&store.read(&job_id)?)?;
        let _permit = app.state::<crate::native_file_jobs::NativeFileJobState>()
            .admission.file(false).map_err(local_error)?;
        // A restore found applied stays applied even when recording that failed.
        if reconcile_stopped_job(&app, store.read(&job_id)?)?.summary["state"] != "uncertain" {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        let job = super::runtime_restore::stop_restore(&root, &job_id)?;
        Ok(job_summary(&root, job))
    }.await)
}
#[tauri::command]
pub(crate) async fn external_storage_start_job(
    app: AppHandle,
    mut request: StartJobRequest,
    job_id: Option<String>,
) -> Result<Value> {
    logged("external_storage_start_job", Box::pin(async move {
        request.validate()?;
        if let Some(id) = &job_id {
            if uuid::Uuid::parse_str(id).ok().is_none_or(|parsed| parsed.to_string() != *id)
            {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
        }
        let root = root(&app)?;
        ConnectionStore::open(&root)?.read(&request.connection_id)?;
        if request.kind == JobKind::Backup && request.reason.as_deref() == Some("automatic")
            && ConnectionStore::open(&root)?.automatic_backup_paused(&request.connection_id)? {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        let command_state = app.state::<JobCommandState>();
        {
            let current = command_state.session.lock().map_err(local_error)?;
            require_session(&request, &current)?;
            request.session = Some(current.kind.clone());
            request.session_id = Some(current.id.clone());
        }

        let store = JobStore::open(&root)?;
        let pending = if let Some(id) = &job_id {
            match store.read(id) {
                Ok(job) => {
                    if !same_explicit_retry(&job, &request) {
                        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                    }
                    Some(reconcile_job(&app, job)?)
                }
                Err(error) if error.kind == ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            }
        } else {
            store.list_pending()?.into_iter()
                .find(|job| pending_matches_request(job, &request))
                .map(|job| reconcile_job(&app, job))
                .transpose()?
        };
        if job_id.is_some() && pending.as_ref().is_some_and(DurableJob::terminal) {
            return Ok(pending.unwrap().summary);
        }
        if let Some(mut pending) = pending.filter(|job| !job.terminal()) {
            let cancelled_worker = command_state
                .active
                .lock()
                .map_err(local_error)?
                .get(&pending.id)
                .is_some_and(|(_, cancel)| cancel.check().is_err());
            if cancelled_worker {
                wait_for_job_release(&app, &pending.id).await?;
                pending = reconcile_job(&app, store.read(&pending.id)?)?;
                let current = command_state.session.lock().map_err(local_error)?;
                require_session(&request, &current)?;
            }
            // What was read before a settled job let go of the connection is stale.
            if wait_for_settled_claim(&app, &pending.request.connection_id).await? {
                pending = reconcile_job(&app, store.read(&pending.id)?)?;
                let current = command_state.session.lock().map_err(local_error)?;
                require_session(&request, &current)?;
            }
            if pending.summary["state"] == "uncertain"
                && pending.summary["phase"] == "publication-unknown"
                && request.reason.as_deref() == Some("automatic")
            {
                return Ok(pending.summary);
            }
            if pending.request.kind != request.kind || !(same_requested_operation(&pending.request, &request)
                || (job_id.is_some() && same_explicit_retry(&pending, &request))) {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
            if pending.terminal() {
                if job_id.is_some() { return Ok(pending.summary); }
                let identity = native_store(&app)?.external_identity().map_err(local_error)?;
                let next = DurableJob::new(request, now_ms(), identity);
                store.put(&next)?;
                wake_job(app, next.id.clone())?;
                return Ok(next.summary);
            }
            if !command_state
                .active
                .lock()
                .map_err(local_error)?
                .contains_key(&pending.id)
            {
                pending.request.session = request.session;
                pending.request.session_id = request.session_id;
                store.put(&pending)?;
                wake_job(app.clone(), pending.id.clone())?;
            }
            return Ok(job_summary(&root, store.read(&pending.id)?));
        }
        wait_for_settled_claim(&app, &request.connection_id).await?;
        let pds = native_store(&app)?;
        let identity = pds.external_identity().map_err(local_error)?;
        if let Some(covered) = covering_backup(&store, &pds, &request, &identity)? {
            return Ok(job_summary(&root, covered));
        }
        let mut job = DurableJob::new(request, now_ms(), identity);
        if let Some(id) = job_id {
            job = job.with_restore_id(id)?;
            if !store.insert_new(&job)? {
                let existing = store.read(&job.id).map_err(|error| {
                    if error.kind == ErrorKind::NotFound { ProviderError::new(ErrorKind::PreconditionFailed) }
                    else { error }
                })?;
                if !same_requested_operation(&existing.request, &job.request) {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                return Ok(reconcile_job(&app, existing)?.summary);
            }
        } else {
            store.put(&job)?;
        }
        wake_job(app, job.id.clone())?;
        Ok(job.summary)
    }).await)
}
fn same_explicit_retry(job: &DurableJob, request: &StartJobRequest) -> bool {
    if same_requested_operation(&job.request, request) { return true; }
    if job.summary["phase"] != "publication-unknown" || request.reason.as_deref() != Some("manual")
        || job.request.target_revision != request.target_revision { return false; }
    let mut exact = request.clone();
    exact.reason = job.request.reason.clone();
    same_requested_operation(&job.request, &exact)
}

fn pending_matches_request(job: &DurableJob, request: &StartJobRequest) -> bool {
    if job.request.connection_id != request.connection_id {
        return false;
    }
    // A publication whose outcome is unknown resumes only by its own id.
    !(job.summary["state"] == "uncertain" && job.summary["phase"] == "publication-unknown")
}
fn same_requested_operation(existing: &StartJobRequest, incoming: &StartJobRequest) -> bool {
    if existing.kind != incoming.kind || existing.connection_id != incoming.connection_id {
        return false;
    }
    match existing.kind {
        JobKind::Restore => {
            existing.snapshot_id == incoming.snapshot_id
                && existing.restore_areas == incoming.restore_areas
                && existing.target_revision == incoming.target_revision
        }
        JobKind::PinHistory => existing.snapshot_id == incoming.snapshot_id,
        JobKind::DeleteHistory => {
            existing.point_id == incoming.point_id
                && existing.point_observation == incoming.point_observation
                && existing.confirm_other_device == incoming.confirm_other_device
                && existing.confirm_last_retained == incoming.confirm_last_retained
        }
        JobKind::CheckRepository => existing.snapshot_id == incoming.snapshot_id,
        JobKind::Backup | JobKind::Cleanup => true,
    }
}
pub(crate) async fn wait_for_job_release(app: &AppHandle, id: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if !app
            .state::<JobCommandState>()
            .active
            .lock()
            .map_err(local_error)?
            .contains_key(id)
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ProviderError::new(ErrorKind::Transient));
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Waits while a job of `connection` that has already settled still holds its
/// claim, and returns whether it waited. A job still running is left to the
/// caller's own checks.
async fn wait_for_settled_claim(app: &AppHandle, connection: &str) -> Result<bool> {
    let holder = app
        .state::<JobCommandState>()
        .active
        .lock()
        .map_err(local_error)?
        .iter()
        .find(|(_, (owned, _))| owned == connection)
        .map(|(id, _)| id.clone());
    let Some(holder) = holder else { return Ok(false) };
    match JobStore::open(&root(app)?)?.read(&holder) {
        Ok(job) if job.terminal() => wait_for_job_release(app, &holder).await.map(|()| true),
        _ => Ok(false),
    }
}

fn completed_job_result(pds: &mut PersistentStore, job: &DurableJob) -> Result<Option<Value>> {
    if job.request.kind == JobKind::PinHistory {
        return super::history_jobs::completed_pin_history(pds, job);
    }
    let Some(_completed) = pds
        .external_job(&job.id)
        .map_err(local_error)?
        .filter(|item| item.connection_id == job.request.connection_id && item.phase == "complete")
    else {
        return Ok(None);
    };
    if job.request.kind == JobKind::Backup {
        let (snapshot, identity) = pds
            .external_backup_result(&job.id)
            .map_err(local_error)?
            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
        return Ok(Some(
            json!({"snapshotId":snapshot,"publishedRevision":identity.revision.to_string()}),
        ));
    }
    Ok(None)
}

/// The connection's last successful backup, for an automatic backup of a revision that backup
/// already holds. Capturing the same library again would only hold up the file operations
/// that wait for it.
fn covering_backup(
    store: &JobStore,
    pds: &PersistentStore,
    request: &StartJobRequest,
    current: &persistent_store::sync_selection::CaptureIdentity,
) -> Result<Option<DurableJob>> {
    if request.kind != JobKind::Backup || request.reason.as_deref() != Some("automatic") {
        return Ok(None);
    }
    let Some(target) = request.target_revision.as_deref().and_then(|value| value.parse::<i64>().ok()) else {
        return Ok(None);
    };
    let Some(last) = store.list()?.into_iter().find(|job| job.request.connection_id == request.connection_id
        && job.request.kind == JobKind::Backup && job.summary["state"] == "succeeded") else {
        return Ok(None);
    };
    let Some((_, captured)) = pds.external_backup_result(&last.id).map_err(local_error)? else {
        return Ok(None);
    };
    let covered = captured.store_id == current.store_id
        && captured.library_epoch == current.library_epoch
        && captured.generation == current.generation
        && captured.revision >= target;
    Ok(covered.then_some(last))
}

pub(crate) fn require_admitted_library(
    job: &DurableJob,
    current: &persistent_store::sync_selection::CaptureIdentity,
) -> Result<()> {
    let admitted = &job.admission_identity;
    if admitted.store_id != current.store_id
        || admitted.library_epoch != current.library_epoch
        || admitted.generation != current.generation
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(())
}

fn reconcile_job<R: Runtime>(app: &AppHandle<R>, mut job: DurableJob) -> Result<DurableJob> {
    let state = app.state::<JobCommandState>();
    if job.request.kind==JobKind::Restore && !job.terminal() {
        match super::runtime_restore::confirmed_restore_activation(app,&job) {
            Ok(Some(result))=>{job.summary["result"]=result;}
            Ok(None)=>{job.summary.as_object_mut().unwrap().remove("result");}
            // A commit's device maintenance refuses the store, and meanwhile the
            // running worker's record stands.
            Err(error) if matches!(error.kind, ErrorKind::Transient | ErrorKind::LocalFailure)
                && state.job_is_active(&job.id)? => {}
            Err(error)=>return Err(error),
        }
    }
    let active = state.active.lock().map_err(local_error)?;
    if active.contains_key(&job.id) {
        return Ok(job);
    }
    reconcile_stopped_job(app, job)
}

// The caller either holds the active-jobs mutex or owns the cleanup claim.
fn reconcile_stopped_job<R: Runtime>(app: &AppHandle<R>, mut job: DurableJob) -> Result<DurableJob> {
    let before = job.clone();
    let mut pds = native_store(app)?;
    let complete = if job.request.kind == JobKind::Restore {
        super::runtime_restore::completed_restore(app, &job)?
    } else {
        completed_job_result(&mut pds, &job)?
    };
    if let Some(result) = complete {
        settle_interrupted(&mut job, Some(result), false);
        let _ = persist_reconciled_job(&root(app)?, &before, &mut job);
        return Ok(job);
    }
    if settle_local_restore_recovery(&mut job) {
        persist_reconciled_job(&root(app)?, &before, &mut job)?;
        return Ok(job);
    }
    let authoritative = pds.external_job(&job.id).map_err(local_error)?;
    {
        if let Some(intent) = authoritative.as_ref().filter(|item| matches!(item.phase.as_str(), "stale" | "cancelled")) {
            settle_invalidated(&mut job, &intent.phase);
            let _ = persist_reconciled_job(&root(app)?, &before, &mut job);
            return Ok(job);
        }
        if job.terminal() || job.summary["state"] != "running" {
            return Ok(job);
        }
    }
    settle_interrupted(&mut job, None, false);
    // The renderer still receives a settled result if its auxiliary cache is unwritable.
    let _ = persist_reconciled_job(&root(app)?, &before, &mut job);
    Ok(job)
}

fn persist_reconciled_job(root: &std::path::Path, before: &DurableJob, job: &mut DurableJob) -> Result<()> {
    let updated = job.summary["updatedAtMs"].clone();
    job.summary["updatedAtMs"] = before.summary["updatedAtMs"].clone();
    if serde_json::to_value(&*job).map_err(local_error)? == serde_json::to_value(before).map_err(local_error)? {
        return Ok(());
    }
    job.summary["updatedAtMs"] = updated;
    if job.terminal() { refresh_transfer_counters(root, job); }
    JobStore::open(root)?.put(job)
}

fn settle_invalidated(job: &mut DurableJob, phase: &str) {
    job.summary["state"] = json!(if phase == "cancelled" { "cancelled" } else { "failed" });
    job.summary["phase"] = json!("paused");
    job.summary.as_object_mut().unwrap().remove("result");
    let kind = if phase == "cancelled" { ErrorKind::Cancelled } else { ErrorKind::PreconditionFailed };
    job.summary["error"] = error_dto(&ProviderError::new(kind));
    job.summary["updatedAtMs"] = json!(now_ms().to_string());
}

fn settle_local_restore_recovery(job: &mut DurableJob) -> bool {
    if !super::runtime_restore::application_started(job) || super::runtime_restore::restore_stopped(job) { return false; }
    job.summary["state"] = json!("uncertain");
    job.summary["phase"] = json!("local-apply-unknown");
    job.summary.as_object_mut().unwrap().remove("result");
    job.summary["error"] = error_dto(&ProviderError::new(ErrorKind::Transient));
    job.summary["error"]["reason"] = json!("local-apply-unknown");
    job.summary["error"]["retryable"] = json!(false);
    job.summary["updatedAtMs"] = json!(now_ms().to_string());
    true
}

fn settle_interrupted(job: &mut DurableJob, complete: Option<Value>, uncertain: bool) {
    if let Some(result) = complete {
        job.summary["state"] = json!("succeeded");
        job.summary["phase"] = json!("complete");
        job.summary["result"] = result;
        job.summary.as_object_mut().unwrap().remove("error");
    } else {
        job.summary["state"] = json!(if uncertain { "uncertain" } else { "waiting" });
        job.summary["phase"] = json!(if uncertain {
            "publication-unknown"
        } else {
            "paused"
        });
        job.summary.as_object_mut().unwrap().remove("result");
        job.summary["error"] = error_dto(&ProviderError::new(ErrorKind::Transient));
        if uncertain {
            job.summary["error"]["reason"] = json!("publication-unknown");
        }
    }
    job.summary["updatedAtMs"] = json!(now_ms().to_string());
}
fn lease_context<'a>(
    app: &AppHandle,
    root: &'a std::path::Path,
    connected: &'a ConnectedRepository,
    writer_id: &'a str,
) -> Result<leases::LeaseContext<'a>> {
    Ok(leases::LeaseContext {
        root,
        connection_id: &connected.stored.id,
        writer_id,
        descriptor: &connected.stored.descriptor,
        root_key: &connected.root_key,
        provider: connected.provider.as_ref(),
        repository: &connected.handle,
        clock: leases::system_clock(),
        protection_supported: connected.stored.capabilities.lease_operations,
        ledger: Some(app.state::<JobCommandState>().lease_ledger()?),
    })
}

pub(crate) struct RepositoryProtection<'a> {
    owner: &'a leases::LeaseOwner,
    context: &'a leases::LeaseContext<'a>,
}
impl RepositoryProtection<'_> {
    pub(crate) async fn recheck(&self, cancel: &Cancellation) -> Result<()> {
        self.owner.check_control(self.context, false)?;
        if self.owner.recheck(self.context, cancel).await?.is_some() {
            return Err(ProviderError::new(ErrorKind::Transient));
        }
        self.owner.check_control(self.context, false)
    }
}

pub(crate) fn wake_job(app: AppHandle, id: String) -> Result<()> {
    let store = JobStore::open(&root(&app)?)?;
    let mut job = store.read(&id)?;
    if job.terminal() {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    read_job_session(&app, &id)?;
    let (cancel, claim) = app.state::<JobCommandState>().claim(&job)?;
    job.summary["state"] = json!("running");
    job.summary["updatedAtMs"] = json!(now_ms().to_string());
    store.put(&job)?;
    let worker = app.state::<JobCommandState>().track_worker();
    let _ = tauri::Emitter::emit(&app, "external-storage-job-started", ());
    tauri::async_runtime::spawn(async move {
        let _worker = worker;
        let result = match run_job(&app, &id, &cancel).await {
            Err(error) => {
                let completed = if job.request.kind == JobKind::Restore {
                    super::runtime_restore::completed_restore(&app, &job)
                } else {
                    native_store(&app).and_then(|mut pds| completed_job_result(&mut pds, &job))
                };
                match completed {
                    Ok(Some(result)) => Ok(result),
                    _ => Err(error),
                }
            }
            outcome => outcome,
        };
        if app.state::<JobCommandState>().settle_blocking(&id).await.is_err() {
            crate::nlog!("error", "External blocking work could not settle");
            return;
        }
        let settled = (|| -> Result<DurableJob> {
            let store = JobStore::open(&root(&app)?)?;
            let mut job = store.read(&id)?;
            match result {
                Ok(result) => {
                    // A removal that left a request whose end is unknown keeps
                    // its marker, so the job stays open until that is resolved.
                    let unresolved =
                        result.get("stopReason").and_then(Value::as_str) == Some("uncertain");
                    let publication_unknown = unresolved
                        && result.get("reason").and_then(Value::as_str)
                            == Some("publication-unknown");
                    job.summary["state"] = json!(if unresolved {
                        "uncertain"
                    } else {
                        "succeeded"
                    });
                    job.summary["phase"] = json!(if publication_unknown {
                        "publication-unknown"
                    } else if unresolved {
                        "removal-unknown"
                    } else {
                        "complete"
                    });
                    if publication_unknown {
                        job.summary.as_object_mut().unwrap().remove("result");
                        job.summary["error"] = error_dto(&ProviderError::new(ErrorKind::Transient));
                        job.summary["error"]["reason"] = json!("publication-unknown");
                    } else {
                        job.summary["result"] = result;
                    }
                }
                Err(error) => {
                    crate::native_log::record_command_failure(
                        "external_storage_job",
                        &error,
                        std::panic::Location::caller(),
                    );
                    let local_application = super::runtime_restore::application_started(&job);
                    // A restore past its commit is pending either way, without reading the store.
                    let mut pds = if local_application { None } else { Some(native_store(&app)?) };
                    let authoritative = match pds.as_mut() {
                        Some(pds) => pds.external_job(&job.id).map_err(local_error)?,
                        None => None,
                    };
                    let pending = local_application || authoritative.iter().any(|item| {
                        item.id == id
                            && ["publishing", "publicationUnknown", "applying"]
                                .contains(&item.phase.as_str())
                    });
                    let retryable = matches!(
                        error.kind,
                        ErrorKind::Cancelled
                            | ErrorKind::RateLimited
                            | ErrorKind::DailyQuotaExhausted
                            | ErrorKind::Transient
                            | ErrorKind::Unauthorized
                            | ErrorKind::ReauthRequired
                            | ErrorKind::StorageFull
                            | ErrorKind::EndpointRejected
                            | ErrorKind::DeviceVaultUnavailable
                            | ErrorKind::RepositoryKeyUnavailable
                            | ErrorKind::RepositoryBusy
                            | ErrorKind::ClockSkew
                            | ErrorKind::LocalStorageFull
                            | ErrorKind::LocalPermissionDenied
                            | ErrorKind::LocalFailure
                    );
                    if !pending && !retryable {
                        if let (Some(pds), Some(item)) = (pds.as_mut(), authoritative.iter().find(|item| {
                            item.id == id
                                && ["preparing", "ready", "stale"].contains(&item.phase.as_str())
                        })) {
                            pds.external_cancel_prepared(&item.id).map_err(local_error)?;
                        }
                    }
                    job.summary["state"] = json!(if pending {
                        "uncertain"
                    } else if retryable {
                        "waiting"
                    } else {
                        "failed"
                    });
                    job.summary["phase"] = json!(if local_application {
                        "local-apply-unknown"
                    } else if pending {
                        "publication-unknown"
                    } else {
                        "paused"
                    });
                    job.summary["error"] = error_dto(&error);
                    if pending {
                        job.summary["error"]["reason"] = json!(if local_application { "local-apply-unknown" } else { "publication-unknown" });
                    }
                    if local_application {
                        job.summary["error"]["retryable"] = json!(false);
                    }
                }
            }
            refresh_transfer_counters(&root(&app)?, &mut job);
            job.summary["updatedAtMs"] = json!(now_ms().to_string());
            Ok(job)
        })();
        let outcome_persisted = settled
            .and_then(|settled| JobStore::open(&root(&app)?)?.put(&settled))
            .is_ok();
        if !outcome_persisted {
            crate::nlog!("error", "External job outcome could not be persisted");
        }
        drop(claim);
        if outcome_persisted {
            let cleaned = (|| -> Result<()> {
                let root = root(&app)?;
                if !JobStore::open(&root)?.read(&job.id)?.terminal() { return Ok(()); }
                let connection = ConnectionStore::open(&root)?.read(&job.request.connection_id)?;
                let directory = job_directory(&root, &job.request.connection_id, &job.id);
                let mut pds = native_store(&app)?;
                let cleanup = super::journal::TransferJournal::cleanup_terminal_spools_at(
                    &directory,
                    &job.id,
                    &mut pds,
                    &connection.descriptor.repository_id,
                )?;
                if matches!(cleanup, super::journal::SpoolCleanup::Removed { .. }) {
                    let jobs = JobStore::open(&root)?;
                    jobs.release_spool(&job.id)?;
                    jobs.prune_released()?;
                }
                Ok(())
            })();
            if let Err(error) = cleaned {
                crate::nlog!("error", "External terminal spool cleanup failed: {error}");
            }
            if let Ok(root) = root(&app) {
                if let Ok(settled) = JobStore::open(&root).and_then(|store| store.read(&job.id)) {
                    super::runtime_restore::discard_finished_staging(&root, &settled);
                }
            }
        }

    });
    Ok(())
}

pub(crate) fn error_dto(error: &ProviderError) -> Value {
    let (message, action, retry) = match error.kind {
        ErrorKind::ReauthRequired | ErrorKind::Unauthorized => (
            "Provider authorization is required.",
            "reauthenticate",
            false,
        ),
        ErrorKind::PreconditionFailed => (
            "The local or remote state changed. Review the pending operation.",
            "retry",
            false,
        ),
        ErrorKind::RateLimited | ErrorKind::DailyQuotaExhausted => {
            ("The provider request budget is exhausted.", "wait", true)
        }
        ErrorKind::RepositoryBusy => ("Another repository operation is in progress.", "wait", true),
        ErrorKind::EndpointRejected => ("The server connection could not be verified.", "check-endpoint", false),
        ErrorKind::RepositoryKeyUnavailable => ("The repository recovery key is required.", "unlock-key", false),
        ErrorKind::DeviceVaultUnavailable => ("Unlock the device credential store and retry.", "retry", false),
        ErrorKind::ClockSkew => ("Correct the device clock and retry.", "retry", false),
        ErrorKind::LocationOccupied => ("Choose an empty repository folder.", "check-endpoint", false),
        ErrorKind::RepositoryMismatch => ("This location holds a different repository.", "check-endpoint", false),
        ErrorKind::RecoveryKeyMismatch => ("The recovery key does not match this repository.", "unlock-key", false),
        ErrorKind::LibraryFormatSwitchRequired => ("Switch the library data format to continue syncing.", "none", false),
        ErrorKind::AppUpdateRequired => ("Update the app to continue syncing.", "none", false),
        ErrorKind::LocalStorageFull => ("This device has insufficient space.", "free-space", false),
        ErrorKind::LocalPermissionDenied => ("Allow access to the local files and retry.", "retry", false),
        ErrorKind::LocalFailure => ("This device could not complete the operation.", "retry", true),
        ErrorKind::StorageFull => (
            "The destination has insufficient space.",
            "free-space",
            false,
        ),
        ErrorKind::Corrupt => ("Stored data failed verification.", "none", false),
        ErrorKind::Unsupported => (
            "This operation is unavailable for this repository.",
            "none",
            false,
        ),
        ErrorKind::Cancelled => ("The operation paused before completion.", "retry", true),
        ErrorKind::FileTooLarge => (
            "An encrypted object exceeds the provider limit.",
            "none",
            false,
        ),
        _ => ("The operation could not complete.", "retry", true),
    };
    let mut value = json!({"code":error.kind,"message":message,"action":action,"retryable":retry});
    if let Some(status) = error.http_status { value["httpStatus"] = json!(status); }
    if let Some(detail) = &error.cause.0 { value["detail"] = json!(detail); }
    if let Some(code) = &error.oauth_error { value["oauthError"] = json!(code); }
    if let Some(description) = &error.oauth_error_description { value["oauthErrorDescription"] = json!(description); }
    if let Some(at) = error.retry_at_ms {
        value["retryAtMs"] = json!(at.to_string());
    }
    value
}

pub(crate) struct CancelProbe(pub Cancellation);
impl crate::local_backup::CancellationProbe for CancelProbe {
    fn is_cancelled(&self) -> bool {
        self.0.check().is_err()
    }
}
/// Where one connection keeps what it already knows about this repository.
/// Shared by the job that publishes and the job that receives, so a receive
/// teaches the next publication instead of only itself.
pub(crate) fn package_cache_root(job_directory: &std::path::Path) -> Result<PathBuf> {
    job_directory
        .parent()
        .and_then(|path| path.parent())
        .map(|path| path.join("package-cache"))
        .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))
}

pub(crate) fn connection_directory(root: &std::path::Path, connection: &str) -> PathBuf {
    root.join("external-storage").join(hex::encode(
        risunest_external_storage_format::content_identity::hash(connection.as_bytes()),
    ))
}
/// The name is a short hash prefix because SQLite opens the databases below it
/// with paths Windows still limits to MAX_PATH.
pub(crate) fn job_directory(root: &std::path::Path, connection: &str, job: &str) -> PathBuf {
    connection_directory(root, connection)
        .join("jobs")
        .join(hex::encode(
            &risunest_external_storage_format::content_identity::hash(job.as_bytes())[..8],
        ))
}
fn retaining_spool_directories(root: &std::path::Path, store: &JobStore) -> Result<Vec<(String, PathBuf)>> {
    let mut directories = store.list_retaining()?.into_iter().map(|job| {
        let directory = job_directory(root, &job.request.connection_id, &job.id);
        (job.id, directory)
    }).collect::<Vec<_>>();
    let publications = root.join("external-storage").join("lww-publications");
    match std::fs::symlink_metadata(&publications) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(directories),
        Err(error) => return Err(local_error(error)),
        Ok(metadata) if crate::trust_boundary::is_link_like(&metadata) || !metadata.is_dir() => {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        Ok(_) => {},
    }
    for entry in std::fs::read_dir(publications).map_err(local_error)? {
        let entry = entry.map_err(local_error)?;
        let metadata = match std::fs::symlink_metadata(entry.path()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            result => result.map_err(local_error)?,
        };
        if crate::trust_boundary::is_link_like(&metadata) || !metadata.is_dir() {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        let id = entry.file_name().into_string().map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
        directories.push((id, entry.path()));
    }
    Ok(directories)
}

/// The transfer spool as `job_id` sees it when it seals another object: every
/// job that has not been swept, with the directory its spool lives in.
pub(crate) fn spool_budget(root: &std::path::Path, job_id: &str) -> super::journal::SpoolBudget {
    let root = root.to_path_buf();
    let store = std::sync::Mutex::new(None::<JobStore>);
    super::journal::SpoolBudget::new(job_id.to_owned(), move || {
        let mut held = store.lock().map_err(local_error)?;
        if held.is_none() { *held = Some(JobStore::open(&root)?); }
        retaining_spool_directories(&root, held.as_ref().ok_or_else(|| ProviderError::new(ErrorKind::Transient))?)
    })
}

/// Sealed ciphertext the other unfinished jobs are holding. A job that would
/// not find room for the packs of its first wave waits for it instead of
/// starting; nothing here removes what another job may still need.
fn spool_budget_owner(
    root: &std::path::Path,
    store: &JobStore,
    job: &DurableJob,
) -> Result<Option<(String, u64)>> {
    let mut total = 0u64;
    let mut owner: Option<(String, u64)> = None;
    for (id, directory) in retaining_spool_directories(root, store)? {
        if id == job.id { continue; }
        let held = super::journal::held_spool_bytes(&directory)?;
        total = total.saturating_add(held);
        if owner.as_ref().is_none_or(|(_, previous)| held > *previous) {
            owner = Some((id, held));
        }
    }
    let headroom = super::packaging::producing_job_spool_headroom()?;
    if total.saturating_add(headroom) <= super::journal::TRANSFER_SPOOL_BUDGET {
        return Ok(None);
    }
    Ok(owner)
}

async fn run_job(app: &AppHandle, id: &str, cancel: &Cancellation) -> Result<Value> {
    let store = JobStore::open(&root(app)?)?;
    let mut job = store.read(id)?;
    read_job_session(app, id)?;
    cancel.check()?;
    if job.request.kind == JobKind::Restore {
        if let Some(result) = super::runtime_restore::completed_restore(app, &job)? {
            return Ok(result);
        }
    }
    if let Some(result) = completed_job_result(&mut native_store(app)?, &job)? {
        return Ok(result);
    }
    if job.request.kind == JobKind::Backup {
        require_admitted_library(
            &job,
            &native_store(app)?
                .external_identity()
                .map_err(local_error)?,
        )?;
    }
    // Work that seals ciphertext waits for the transfer spool the unfinished
    // jobs are holding. A cleanup, a restore or a check is what frees it, so
    // none of them are held back by it.
    if matches!(
        job.request.kind,
        JobKind::Backup | JobKind::PinHistory
    ) {
        if let Some((owner, held)) = spool_budget_owner(&root(app)?, &store, &job)? {
            crate::nlog!(
                "warn",
                "External job {id} waits for the transfer spool budget; job {owner} holds {held} bytes"
            );
            return Err(ProviderError::new(ErrorKind::Transient));
        }
    }
    job.summary["state"] = json!("running");
    job.summary["phase"] = json!("opening");
    job.summary["updatedAtMs"] = json!(now_ms().to_string());
    job.summary.as_object_mut().unwrap().remove("error");
    store.put(&job)?;
    let connected =
        super::connection_commands::open_connected_with_cancel(app, &job.request.connection_id, cancel).await?;
    cancel.check()?;
    match job.request.kind {
        JobKind::Backup => {
            let root = root(app)?;
            let writer_id = native_store(app)?
                .external_identity()
                .map_err(local_error)?
                .store_id;
            let context = lease_context(&app, &root, &connected, &writer_id)?;
            match leases::admit(&context, &job.id, LeaseKind::Work, cancel).await? {
                leases::Admission::Admitted(owner) => {
                    let protection = RepositoryProtection { owner: &owner, context: &context };
                    owner.run(&context, cancel, async {
                        match job.request.kind {
                            JobKind::Backup => {
                                run_backup(app, &connected, &job, Some(&protection), cancel).await
                            }
                            _ => unreachable!(),
                        }
                    }).await
                }
                leases::Admission::Yield { reason } => Err(leases::yield_error(reason)),
                leases::Admission::UnsupportedProtection if job.request.kind == JobKind::Backup => {
                    run_backup(app, &connected, &job, None, cancel).await
                }
                leases::Admission::UnsupportedProtection => {
                    Err(ProviderError::new(ErrorKind::Unsupported))
                }
            }
        }
        JobKind::Restore => {
            let root=root(app)?;
            let writer=native_store(app)?.external_identity().map_err(local_error)?.store_id;
            let context=lease_context(app,&root,&connected,&writer)?;
            match leases::admit_shared_work(&context,&job.id,cancel).await? {
                leases::Admission::Admitted(owner)=>{
                    owner.run_restore(&context,cancel,super::runtime_restore::run_restore(app,&connected,&job,cancel)).await
                }
                leases::Admission::Yield{reason}=>Err(leases::yield_error(reason)),
                leases::Admission::UnsupportedProtection if !context.protection_supported=>{
                    super::runtime_restore::run_restore(app,&connected,&job,cancel).await
                }
                leases::Admission::UnsupportedProtection=>Err(ProviderError::new(ErrorKind::Unsupported)),
            }
        }
        JobKind::PinHistory => {
            super::history_jobs::run_pin_history(app, &connected, &job, cancel).await
        }
        JobKind::DeleteHistory => {
            super::history_deletion::run_delete_history(app, &connected, &job, cancel).await
        }
        JobKind::Cleanup => run_cleanup(app, &connected, &job, cancel).await,
        JobKind::CheckRepository => {
            run_repository_check(app, &connected, &job, cancel).await
        }
    }
}

/// Removes what the current roots no longer reach. The result is a summary
/// rather than progress: a cleanup writes no transfer journal, so the four
/// progress fields would be recomputed as zero on every read.
async fn run_cleanup(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    cancel: &Cancellation,
) -> Result<Value> {
    run_connected_cleanup(app,connected,&job.request.connection_id,&job.id,None,cancel).await
}
pub(crate) async fn run_connected_cleanup(
    app:&AppHandle,connected:&ConnectedRepository,connection_id:&str,job_id:&str,
    engine:Option<&super::lww_engine::ExternalLwwEngine>,cancel:&Cancellation,
)->Result<Value> {
    let root = root(app)?;
    let store = std::sync::Mutex::new(native_store(app)?);
    let writer_id = store
        .lock()
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?
        .external_identity()
        .map_err(local_error)?
        .store_id;
    let unfinished = JobStore::open(&root)?
        .list_pending()?
        .into_iter()
        .filter(|item| item.request.connection_id == connection_id && item.id != job_id)
        .map(|item| super::cleanup::UnfinishedJob {
            directory: job_directory(&root, &item.request.connection_id, &item.id),
            // A job publishes under its own snapshot identifier and may also
            // name one it reads, and neither is the job identifier.
            snapshot_ids: [Some(item.snapshot_id.clone()), item.request.snapshot_id.clone()]
                .into_iter()
                .flatten()
                .collect(),
            job_id: item.id,
            references: Vec::new(),
        })
        .collect::<Vec<_>>();

    // One reading of the clock: the retention decision and the grace window
    // are measured against the same moment.
    let started = now_ms();
    let cleanup_directory = job_directory(&root, connection_id, job_id);
    let cache_root = cleanup_directory
        .parent()
        .and_then(|path| path.parent())
        .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?
        .join("package-cache");
    let scratch = cleanup_directory.join("cleanup-probe");
    let view = super::cleanup::ConnectedRepositoryView {
        connected,
        writer_id: &writer_id,
        policy: connected
            .stored
            .retention_policy
            .unwrap_or(super::connection::RetentionPolicy::DEFAULT),
        now_ms: started,
        unfinished,
        cache_root: &cache_root,
        reads: Default::default(),
    };
    let documents = super::cleanup::ConnectedDocuments { connected, cancel, scratch: &scratch };
    let connection_time = |not_before| {
        Ok(connected.dependencies.requests
            .clock_sample_after(&connected.handle.account, not_before)?
            .is_some())
    };
    let protection=lease_context(app,&root,connected,&writer_id)?;
    let request=super::cleanup::CleanupRequest {
        job_id,cleanup_supported:connected.stored.capabilities.cleanup_supported(),
        limits:super::cleanup::CleanupLimits::default(),connection_time:&connection_time,
    };
    let mut opened=None;
    if engine.is_none() && connected.stored.descriptor.publication_strategy.is_some() {
        let selected_connection=super::connection_commands::open_connected_with_cancel(app,connection_id,cancel).await?;
        if selected_connection.handle.repository_id!=connected.handle.repository_id
            || selected_connection.handle.connection_identity!=connected.handle.connection_identity
            || selected_connection.stored.descriptor.repository_id!=connected.stored.descriptor.repository_id {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        let fresh_after=std::time::Instant::now();
        connected.provider.list_objects(&connected.handle,super::contract::Collection::Segments,None,1,cancel).await?;
        let sample=connected.dependencies.requests.require_clock_sample_after(&connected.handle.account,fresh_after)
            .map_err(super::clock::after_refresh)?;
        let mut selected=super::lww_engine::ExternalLwwEngine {
            provider:selected_connection.provider,repository:selected_connection.handle,
            library:connected.stored.descriptor.repository_id.clone(),root_key:connected.root_key.clone(),
            admission:None,connection_id:connection_id.into(),connection_root:root.clone(),
            #[cfg(test)]
            test_lease_clock: None,
            capabilities:connected.stored.capabilities.clone(),descriptor:connected.stored.descriptor.clone(),
        };
        selected.admit(&sample)?;opened=Some(selected);
    }
    let outcome=if let Some(engine)=engine.or(opened.as_ref()) {
        super::cleanup::run_lww(&protection,&request,engine,&store,view,&scratch,cancel).await?
    } else {
        super::cleanup::run(&protection,&request,&view,&documents,cancel).await?
    };
    Ok(outcome.summary())
}
/// Proves the selected published state can still be read. The result is a
/// summary rather than transfer progress: a check writes no transfer journal,
/// so the four progress fields stay as this job records them.
async fn run_repository_check(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    cancel: &Cancellation,
) -> Result<Value> {
    let root = root(app)?;
    let directory = job_directory(&root, &job.request.connection_id, &job.id);
    let cache_root = package_cache_root(&directory)?;
    let writer_id = native_store(app)?
        .external_identity()
        .map_err(local_error)?
        .store_id;
    let context = lease_context(&app, &root, connected, &writer_id)?;
    let _ = record_check_progress(&root, &job.id, "reading", None);
    let progress = |items: u64, total_items: u64, bytes: u64, total_bytes: u64| {
        let _ = record_check_progress(
            &root,
            &job.id,
            "verifying",
            Some((items, total_items, bytes, total_bytes)),
        );
    };
    let mut evidence =
        super::packaging::ObjectEvidence::open(&root, &job.request.connection_id)?;
    super::repository_check::run_job(
        &context,
        connected,
        &super::repository_check::CheckJob {
            job_id: &job.id,
            snapshot_id: job.request.snapshot_id.as_deref(),
            directory: &directory,
            cache_root: &cache_root,
            policy: connected
                .stored
                .retention_policy
                .unwrap_or(super::connection::RetentionPolicy::DEFAULT),
            now_ms: now_ms(),
        },
        &mut evidence,
        &progress,
        cancel,
    )
    .await
}

/// What one phase has done against what it found to do, and which of the two
/// readings the numbers are. Written where the work happens, because a summary
/// is only assembled when the renderer asks for one.
fn record_counters(
    root: &std::path::Path,
    job_id: &str,
    counters: &str,
    reading: super::phase_progress::PhaseCounters,
) -> Result<()> {
    let store = JobStore::open(root)?;
    let mut job = store.read(job_id)?;
    if job.terminal() {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    job.summary["counters"] = json!(counters);
    job.summary["completedItems"] = json!(reading.items.to_string());
    job.summary["totalItems"] = json!(reading.total_items.to_string());
    job.summary["completedBytes"] = json!(reading.bytes.to_string());
    job.summary["totalBytes"] = json!(reading.total_bytes.to_string());
    job.summary["updatedAtMs"] = json!(now_ms().to_string());
    store.put(&job)
}

fn job_phase_progress(
    root: &std::path::Path,
    job_id: &str,
    counters: &'static str,
) -> std::sync::Arc<super::phase_progress::PhaseProgress> {
    let root = root.to_path_buf();
    let job_id = job_id.to_owned();
    super::phase_progress::PhaseProgress::new(move |reading| {
        let _ = record_counters(&root, &job_id, counters, reading);
    })
}

/// What a job is reading, compressing, packing or staging.
pub(crate) fn preparation_progress(
    root: &std::path::Path,
    job_id: &str,
) -> std::sync::Arc<super::phase_progress::PhaseProgress> {
    job_phase_progress(root, job_id, "prepared")
}

/// What a job has moved to or from the repository and had confirmed.
pub(crate) fn transfer_progress(
    root: &std::path::Path,
    job_id: &str,
) -> std::sync::Arc<super::phase_progress::PhaseProgress> {
    job_phase_progress(root, job_id, "transferred")
}

/// Continues the transfer reading an earlier phase of `job` recorded, for a
/// phase whose downloads that phase already planned.
pub(crate) fn resumed_transfer_progress(
    root: &std::path::Path,
    job: &DurableJob,
) -> std::sync::Arc<super::phase_progress::PhaseProgress> {
    let summary = &job.summary;
    let count = |name: &str| {
        summary[name]
            .as_str()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let reading = if summary["counters"] == "transferred" {
        super::phase_progress::PhaseCounters {
            items: count("completedItems"),
            total_items: count("totalItems"),
            bytes: count("completedBytes"),
            total_bytes: count("totalBytes"),
        }
    } else {
        super::phase_progress::PhaseCounters::default()
    };
    let root = root.to_path_buf();
    let job_id = job.id.clone();
    super::phase_progress::PhaseProgress::resumed(reading, move |reading| {
        let _ = record_counters(&root, &job_id, "transferred", reading);
    })
}

fn record_check_progress(
    root: &std::path::Path,
    job_id: &str,
    phase: &str,
    counters: Option<(u64, u64, u64, u64)>,
) -> Result<()> {
    let store = JobStore::open(root)?;
    let mut job = store.read(job_id)?;
    job.summary["phase"] = json!(phase);
    if let Some((items, total_items, bytes, total_bytes)) = counters {
        job.summary["completedItems"] = json!(items.to_string());
        job.summary["totalItems"] = json!(total_items.to_string());
        job.summary["completedBytes"] = json!(bytes.to_string());
        job.summary["totalBytes"] = json!(total_bytes.to_string());
    }
    job.summary["updatedAtMs"] = json!(now_ms().to_string());
    store.put(&job)
}
/// Whether a publication that is already required may also spend the optional
/// maintenance portion. A daily allowance with less headroom than the portion
/// can spend is not spent on it.
pub(crate) fn maintenance_allowed(
    app: &AppHandle,
    connected: &super::connection_commands::ConnectedRepository,
) -> bool {
    let Ok(budget) = super::connection_commands::budget(app) else {
        return false;
    };
    let Ok(usage) =
        super::quota_profiles::connection_usage(&budget, &connected.stored.config, now_ms())
    else {
        return false;
    };
    usage.iter().all(|counter| {
        counter.limit.saturating_sub(counter.used)
            >= super::packaging::COALESCE_REQUEST_BUDGET
    })
}

async fn run_backup(
    app: &AppHandle,
    connected: &super::connection_commands::ConnectedRepository,
    job: &DurableJob,
    protection: Option<&RepositoryProtection<'_>>,
    cancel: &Cancellation,
) -> Result<Value> {
    use super::{
        control::{BackupPointDocument, BackupPointKind},
        journal::{JobIdentity, TransferJournal},
        packaging::{PackageLimits, SnapshotMetadata, SnapshotPurpose},
    };
    use risunest_external_storage_format::control::BundleSource;
    let root = root(app)?;
    let worker_app = app.clone();
    let worker_job = job.clone();
    let repository_id = connected.stored.descriptor.repository_id.clone();
    let probe = CancelProbe(cancel.clone());
    let section_spool =
        job_directory(&root, &job.request.connection_id, &job.id).join("sections");
    let worker_spool = section_spool.clone();
    let completion = app.state::<JobCommandState>().track_blocking(&job.id)?;
    let (capture, fingerprint, sections) = tokio::task::spawn_blocking(move || -> Result<_> {
        let _completion = completion;
        let admission = worker_app
            .state::<crate::native_file_jobs::NativeFileJobState>()
            .admission
            .clone();
        let mut pds = native_store(&worker_app)?;
        let authoritative = pds.external_job(&worker_job.id).map_err(local_error)?;
        let retained = authoritative
            .as_ref()
            .map(|item| item.capture_id.clone())
            .or(worker_job.capture_id.clone());
        let hydration = if retained.is_none() {
            Some(
                pds.hydrate_external_capture_dependencies(
                    &worker_job.request.connection_id,
                    &probe,
                )
                .map_err(local_error)?,
            )
        } else {
            None
        };
        let _permit = admission.file(true).map_err(local_error)?;
        require_admitted_library(&worker_job, &pds.external_identity().map_err(local_error)?)?;
        let capture = match &retained {
            Some(id) => pds.reopen_external_capture(id).map_err(local_error)?,
            None => {
                let revision = pds.revision().map_err(local_error)?;
                let (lease, prepared_sections) = pds.lww_acquire_backup_capture(revision).map_err(local_error)?;
                let sections = match super::sections::capture_prepared_backup_sections(&prepared_sections, &worker_spool, &probe.0) {
                    Ok(sections) => sections,
                    Err(error) => { let _ = pds.release_revision(&lease.lease); return Err(error); }
                };
                let capture = pds
                    .capture_external_library_from_lease_with_sections(
                        &worker_job.request.connection_id,
                        hydration
                            .as_ref()
                            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?,
                        &lease.lease,
                        sections,
                        &probe,
                    )
                    .map_err(|error| { let _ = pds.release_revision(&lease.lease); local_error(error) })?;
                probe.0.check()?;
                pds.external_prepare_backup(
                    &worker_job.id,
                    &worker_job.request.connection_id,
                    &repository_id,
                    &capture.id,
                    &worker_job.snapshot_id,
                )
                .map_err(local_error)?;
                probe.0.check()?;
                let store = JobStore::open(&self::root(&worker_app)?)?;
                let mut stored = store.read(&worker_job.id)?;
                stored.capture_id = Some(capture.id.clone());
                stored.summary["phase"] = json!("captured");
                store.put(&stored)?;
                capture
            }
        };
        if worker_job
            .request
            .target_revision
            .as_ref()
            .and_then(|r| r.parse::<i64>().ok())
            .is_some_and(|target| capture.identity.revision < target)
        {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        let fingerprint = capture
            .catalog
            .content_fingerprint(&risunest_external_storage_format::format::library_fingerprint_domain())
            .map_err(local_error)?;
        let sections = capture.catalog.backup_sections(&self::root(&worker_app)?).map_err(local_error)?;
        Ok((capture, fingerprint, sections))
    })
    .await
    .map_err(local_error)??;
    let identity = capture.identity.clone();
    let original_units = capture.catalog.original_backup_units().map_err(local_error)?;
    let capture_id = capture.id.clone();
    let directory = job_directory(&root, &job.request.connection_id, &job.id);
    let mut journal = TransferJournal::open(
        &directory,
        JobIdentity {
            job_id: job.id.clone(),
            connection_id: job.request.connection_id.clone(),
            repository_id: connected.handle.repository_id.clone(),
            capture_id,
            capture: identity.clone(),
        },
    )?;
    journal.set_spool_budget(spool_budget(&root, &job.id));
    let metadata = SnapshotMetadata {
        snapshot_id: job.snapshot_id.clone(),
        repository_id: connected.stored.descriptor.repository_id.clone(),
        library_id: identity.library_epoch.clone(),
        author_device_id: identity.store_id.clone(),
        created_at_ms: job.summary["startedAtMs"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?,
        parent_snapshot_id: None,
        content_fingerprint: fingerprint,
        logical_revision: identity.revision.try_into().map_err(local_error)?,
        purpose: SnapshotPurpose::BackupBundle {
            source: BundleSource::Device {
                writer_id: identity.store_id.clone(),
            },
            remote_generation: None,
            original_units,
        },
    };
    let cache = package_cache_root(&directory)?;
    let prepared = preparation_progress(&root, &job.id);
    let observation = super::progress::JobProgress::begin(directory.clone());
    let observed_provider = observation.wrap(connected.provider.clone());
    let completed = super::packaging::package_and_upload(
        capture,
        sections,
        &root,
        &cache,
        metadata,
        &connected.root_key,
        PackageLimits::from_capabilities(&connected.stored.capabilities)?
            .with_maintenance(maintenance_allowed(app, connected)),
        None,
        &mut journal,
        observed_provider.as_ref(),
        &connected.handle,
        &prepared,
        cancel,
    )
    .await?;
    prepared.flush();
    match super::packaging::verify_publication(
        &completed,
        &root,
        &cache,
        &mut journal,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    ).await? {
        super::packaging::PublicationReadiness::Verified => {}
        super::packaging::PublicationReadiness::Repackage => {
            return Err(ProviderError::new(ErrorKind::Transient));
        }
    }
    let kind = if job.request.reason.as_deref() == Some("automatic") {
        BackupPointKind::Automatic
    } else {
        BackupPointKind::Manual
    };
    let document = BackupPointDocument::single(
        &connected.stored.descriptor,
        job.snapshot_id.clone(),
        kind,
        job.summary["startedAtMs"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?,
        completed.reference.clone(),
    )?;
    if let Some(protection) = protection {
        protection.recheck(cancel).await?;
    }
    let point = super::control::upload_backup_point(
        &connected.stored.descriptor,
        &connected.root_key,
        document,
        &mut journal,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
    let observation = serde_json::to_string(&point).map_err(local_error)?;
    native_store(app)?
        .external_finish_backup(
            &job.id,
            &job.snapshot_id,
            &completed.snapshot_id,
            &observation,
        )
        .map_err(local_error)?;
    record_connection_completion(
        app,
        &job.request.connection_id,
        super::connection_store::CompletionKind::Backup,
    );
    ConnectionStore::open(&root)?.remember_discovery(
        &job.request.connection_id,
        &completed.snapshot_id,
        &completed.reference,
    )?;
    super::usage::remember_completed_snapshot(
        &root, &job.request.connection_id, &completed,
        job.summary["startedAtMs"].as_str().and_then(|value| value.parse().ok()).unwrap_or(0),
    );
    if journal
        .release_completed_sessions(connected.dependencies.vault.as_ref())
        .await
        .is_err()
    {
        crate::nlog!(
            "warn",
            "External completed upload secret cleanup is pending"
        );
    }
    Ok(
        json!({
            "snapshotId": completed.snapshot_id,
            "publishedRevision": identity.revision.to_string(),
            "maintenancePacks": completed.maintenance.packs.to_string(),
            "maintenanceBytes": completed.maintenance.source_bytes.to_string(),
            "maintenanceLeaves": completed.maintenance.leaves.to_string(),
            "maintenanceRequiredLeaves": completed.maintenance.required_leaves.to_string(),
            "sourceDownloadObjects": completed.hydration.objects.to_string(),
            "sourceDownloadBytes": completed.hydration.bytes.to_string(),
        }),
    )
}
#[tauri::command]
pub(crate) async fn external_storage_get_quota(
    app: AppHandle,
    connection_id: String,
) -> Result<Value> {
    logged("external_storage_get_quota", async move {
        let root = root(&app)?;
        let stored = ConnectionStore::open(&root)?.read(&connection_id)?;
        let budget = super::connection_commands::budget(&app)?;
        let summaries = super::quota_profiles::connection_usage(
            &budget,
            &stored.config,
            now_ms(),
        )?;
        let buckets = summaries
            .into_iter()
            .map(|bucket| {
                let mut value = json!({
                    "id":bucket.id,
                    "used":bucket.used.to_string(),
                    "limit":bucket.limit.to_string(),
                    "unit":"requests",
                    "localEstimate":bucket.local_estimate,
                });
                if let Some(reset_at_ms) = bucket.reset_at_ms {
                    value["resetAtMs"] = json!(reset_at_ms.to_string());
                }
                value
            })
            .collect::<Vec<_>>();
        let usage = super::usage::summarize(&root, &connection_id)?;
        let latest_reachable = usage.latest_reachable.map(|reachable| {
            json!({
                "snapshotId":reachable.snapshot_id,
                "knownDirectObjectCount":reachable.known_direct_objects.to_string(),
                "knownDirectBytes":reachable.known_direct_bytes.to_string(),
                "complete":reachable.complete,
                "coverage":"snapshot-and-catalog-roots"
            })
        });
        let mut storage = json!({
            "providerPhysicalBytes":usage.provider_physical_bytes.map(|value|value.to_string()),
            "providerPhysicalKnown":usage.provider_physical_bytes.is_some(),
            "locallyUploadedObjectCountLowerBound":usage.locally_uploaded_objects_lower_bound.to_string(),
            "locallyUploadedBytesLowerBound":usage.locally_uploaded_bytes_lower_bound.to_string(),
            "locallyUploadedCoverage":"observed-remote-files",
            "observedAtMs":usage.observed_at_ms.map(|value|value.to_string())
        });
        if let Some(latest_reachable) = latest_reachable {
            storage["latestReachable"] = latest_reachable;
        }
        Ok(json!({
            "connectionId":connection_id,
            "buckets":buckets,
            "storage":storage
        }))
    }.await)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_failures_name_the_device_and_stay_retryable() {
        let error = local_error("synthetic local failure");
        assert_eq!(error.kind, ErrorKind::LocalFailure);
        let dto = error_dto(&error);
        assert_eq!(dto["code"], "localFailure");
        assert_eq!(dto["action"], "retry");
        assert_eq!(dto["retryable"], true);
    }

    /// A long device-store write elsewhere, such as a publication persisting
    /// its segment, does not fail a state read.
    #[test]
    fn a_held_device_store_write_does_not_fail_the_state_read() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let app = tauri::test::mock_builder().build(tauri::test::mock_context(tauri::test::noop_assets())).unwrap();
        app.manage(persistent_store::commands::PersistentStoreState::with_test_store(PersistentStore::open(root).unwrap()));
        let jobs = JobCommandState::default();
        jobs.root.set(root.to_owned()).unwrap();
        app.manage(jobs);
        let holder = rusqlite::Connection::open(root.join("persistent").join("device.sqlite")).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let state = external_storage_get_state(app.handle().clone());
        holder.execute_batch("ROLLBACK").unwrap();
        assert_eq!(state.unwrap()["supported"], true);
    }

    #[test]
    fn stopped_unknown_reconciliation_drops_the_stale_result_until_it_completes() {
        let mut job=automatic_job();
        job.summary["result"]=json!({"stopReason":"uncertain","reason":"publication-unknown"});
        settle_interrupted(&mut job,None,true);
        assert!(job.summary["result"].is_null());
        assert_eq!(job.summary["state"],"uncertain");
        assert_eq!(job.summary["error"]["reason"],"publication-unknown");
        settle_interrupted(&mut job,Some(json!({"publishedRevision":"1"})),false);
        assert_eq!(job.summary["result"]["publishedRevision"],"1");
        assert_eq!(job.summary["state"],"succeeded");
    }

    #[test]
    fn connection_recovery_status_survives_reload_and_clears_after_verified_repair() {
        let jobs=vec![json!({"connectionId":"x","state":"waiting","updatedAtMs":"20",
            "error":{"action":"reauthenticate","retryable":false,"message":"Authorization required"}})];
        let mut connection=json!({"id":"x","status":"paused","automaticBackupPaused":true,"lastVerifiedAtMs":"10"});
        apply_job_connection_status(&mut connection,&jobs);
        assert_eq!(connection["status"],"reauth-required");
        assert_eq!(connection["automaticBackupPaused"],true);
        assert_eq!(connection["lastError"],jobs[0]["error"]);
        let mut repaired=json!({"id":"x","status":"ready","lastVerifiedAtMs":"21"});
        apply_job_connection_status(&mut repaired,&jobs);
        assert_eq!(repaired["status"],"ready");
        assert!(repaired["lastError"].is_null());
    }

    #[test]
    fn a_cancelled_job_carries_the_app_stop_only_when_the_app_stopped_it() {
        let mut by_user = automatic_job();
        by_user.summary["error"] = json!({"code":"transient","action":"retry","retryable":true});
        let mut by_app = by_user.clone();
        settle_cancelled(&mut by_user, false);
        settle_cancelled(&mut by_app, true);
        for job in [&by_user, &by_app] {
            assert_eq!(job.summary["state"], "cancelled");
            assert_eq!(job.summary["phase"], "cancelled");
            assert_eq!(job.summary["error"]["code"], "transient");
        }
        assert!(by_user.summary.get("stoppedByApp").is_none());
        assert_eq!(by_app.summary["stoppedByApp"], true);
    }

    #[test]
    fn a_new_restore_choice_never_resumes_a_different_pending_request() {
        let existing: StartJobRequest = serde_json::from_value(json!({"connectionId":"x", "kind":"restore", "snapshotId":"a", "restoreAreas":["library"]})).unwrap();
        let mut incoming = existing.clone();
        assert!(same_requested_operation(&existing, &incoming));
        incoming.snapshot_id = Some("b".into());
        assert!(!same_requested_operation(&existing, &incoming));
        incoming.snapshot_id = existing.snapshot_id.clone();
        incoming.restore_areas = Some(vec!["hypa".into()]);
        assert!(!same_requested_operation(&existing, &incoming));
        incoming.restore_areas = existing.restore_areas.clone();
        incoming.target_revision = Some("2".into());
        assert!(!same_requested_operation(&existing, &incoming));
    }
    /// A removed connection keeps no job that would wait to run again. A job
    /// whose remote outcome is unknown, or a restore applying on this device,
    /// keeps its row, and other connections' jobs are untouched.
    #[test]
    fn removing_a_connection_ends_its_waiting_jobs_and_keeps_unresolved_ones() {
        let job = |request: Value, state: &str| {
            let mut job = DurableJob::new(serde_json::from_value(request).unwrap(), 1, persistent_store::sync_selection::CaptureIdentity {
                store_id: "store".into(), library_epoch: "library".into(), generation: "generation".into(),
                selection_epoch: "selection".into(), revision: 1,
            });
            job.summary["state"] = json!(state);
            job
        };
        let backup = json!({"connectionId":"removed","kind":"backup","reason":"automatic"});
        let restore = json!({"connectionId":"removed","kind":"restore","snapshotId":"snapshot"});
        for (request, state, applying, ended) in [
            (&backup, "queued", false, true),
            (&backup, "waiting", false, true),
            (&backup, "running", false, true),
            (&backup, "uncertain", false, false),
            (&restore, "waiting", false, true),
            (&restore, "running", true, false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let store = JobStore::open(root.path()).unwrap();
            let finished = job(backup.clone(), "succeeded");
            store.put(&finished).unwrap();
            let mut live = job(request.clone(), state);
            if applying {
                live.summary["applicationStarted"] = json!(true);
            }
            store.put(&live).unwrap();
            let other = job(json!({"connectionId":"kept","kind":"backup","reason":"automatic"}), "waiting");
            store.put(&other).unwrap();

            end_removed_connection_jobs(root.path(), "removed").unwrap();

            let after = store.read(&live.id).unwrap();
            if ended {
                assert_eq!(after.summary["state"], "cancelled", "{state}");
                assert_eq!(after.summary["error"]["code"], error_dto(&ProviderError::new(ErrorKind::Cancelled))["code"]);
            } else {
                assert_eq!(after.summary, live.summary, "{state}");
            }
            assert_eq!(
                store.list_pending().unwrap().iter().any(|job| job.request.connection_id == "removed"),
                !ended
            );
            assert_eq!(store.read(&other.id).unwrap().summary, other.summary);
            assert_eq!(store.read(&finished.id).unwrap().summary, finished.summary);
        }
    }

    fn automatic_job() -> DurableJob {
        let request = serde_json::from_value(json!({
            "connectionId":"x", "kind":"backup", "reason":"automatic", "targetRevision":"1"
        })).unwrap();
        DurableJob::new(request, 1, persistent_store::sync_selection::CaptureIdentity {
            store_id: "store".into(), library_epoch: "library".into(), generation: "generation".into(),
            selection_epoch: "selection".into(), revision: 1,
        })
    }

    /// An automatic backup of a revision the connection's last backup already holds reports that
    /// backup instead of capturing the library again.
    #[test]
    fn an_automatic_backup_of_a_backed_up_revision_reports_the_last_backup() {
        use crate::persistent_store::{RootMutation, WorkingSetCommit};
        let root = tempfile::tempdir().unwrap();
        let store = JobStore::open(root.path()).unwrap();
        let mut pds = PersistentStore::open(root.path()).unwrap();
        let captured = pds.external_identity().unwrap();
        let request = |reason: &str, revision: i64| -> StartJobRequest { serde_json::from_value(json!({
            "connectionId":"connection", "kind":"backup", "reason":reason, "targetRevision":revision.to_string()
        })).unwrap() };
        let mut finished = DurableJob::new(request("automatic", captured.revision), 1, captured.clone());
        finished.summary["state"] = json!("succeeded");
        store.put(&finished).unwrap();
        // A finished job that left no backup point holds nothing.
        assert!(covering_backup(&store, &pds, &request("automatic", captured.revision), &captured).unwrap().is_none());
        let mut db = rusqlite::Connection::open(root.path().join("persistent/persistent.sqlite")).unwrap();
        let tx = db.transaction().unwrap();
        persistent_store::external_storage_state::register_capture(
            &tx, "capture", &captured, "scope", "logical-v1", "", &"a".repeat(64), "connection",
        ).unwrap();
        tx.commit().unwrap();
        drop(db);
        pds.external_prepare_backup(&finished.id, "connection", "format-repository", "capture", "point").unwrap();
        pds.external_finish_backup(&finished.id, "point", "snapshot", "observation").unwrap();
        let current = pds.external_identity().unwrap();
        let covered = covering_backup(&store, &pds, &request("automatic", current.revision), &current).unwrap();
        assert_eq!(covered.map(|job| job.id), Some(finished.id.clone()));
        assert!(covering_backup(&store, &pds, &request("manual", current.revision), &current).unwrap().is_none());
        pds.commit(&WorkingSetCommit {
            expected_revision: pds.revision().unwrap(),
            root_mutations: Some(vec![RootMutation::Set { key: "synthetic".into(), value: json!(1) }]),
            ..Default::default()
        }).unwrap();
        let changed = pds.external_identity().unwrap();
        assert!(changed.revision > current.revision);
        assert!(covering_backup(&store, &pds, &request("automatic", changed.revision), &changed).unwrap().is_none());
    }

    /// Invariant 23. An empty journal must not overwrite what preparation
    /// counted, and a publication uploads packs while it still prepares the
    /// rest. The first registration after preparation did all it planned is
    /// what hands the counters over, and the summary says which of the two
    /// the numbers are.
    #[test]
    fn a_summary_keeps_prepared_counters_until_preparation_finishes_and_the_journal_holds_a_transfer() {
        use super::super::journal::{JobIdentity, TransferJournal};
        let root = tempfile::tempdir().unwrap();
        let store = JobStore::open(root.path()).unwrap();
        let job = automatic_job();
        store.put(&job).unwrap();
        record_counters(
            root.path(),
            &job.id,
            "prepared",
            super::super::phase_progress::PhaseCounters {
                items: 7,
                total_items: 9,
                bytes: 70,
                total_bytes: 90,
            },
        )
        .unwrap();
        let prepared = job_summary(root.path(), store.read(&job.id).unwrap());
        assert_eq!(prepared["counters"], "prepared");
        assert_eq!(prepared["completedItems"], "7");
        assert_eq!(prepared["totalItems"], "9");
        assert_eq!(prepared["completedBytes"], "70");
        assert_eq!(prepared["totalBytes"], "90");

        let directory = job_directory(root.path(), &job.request.connection_id, &job.id);
        let identity = JobIdentity {
            job_id: job.id.clone(),
            connection_id: job.request.connection_id.clone(),
            repository_id: super::super::fake::repository().repository_id,
            capture_id: "capture".into(),
            capture: job.admission_identity.clone(),
        };
        let mut journal = TransferJournal::open(&directory, identity.clone()).unwrap();
        let empty = job_summary(root.path(), store.read(&job.id).unwrap());
        assert_eq!(empty["counters"], "prepared");
        assert_eq!(empty["totalItems"], "9");

        let bytes = b"synthetic sealed ciphertext";
        let intent = super::super::contract::ObjectIntent {
            repository_id: identity.repository_id.clone(),
            job_id: job.id.clone(),
            object_id: "pack".into(),
            role: super::super::contract::ObjectRole::Pack,
            byte_length: bytes.len() as u64,
            sha256: risunest_sync_wire::hash(bytes),
        };
        std::fs::write(journal.spool_path("pack"), bytes).unwrap();
        journal.register(&intent).unwrap();
        // A retry re-registers what it already placed, and the object it names
        // is the one that was already counted.
        journal.register(&intent).unwrap();
        drop(journal);
        let preparing = job_summary(root.path(), store.read(&job.id).unwrap());
        assert_eq!(preparing["counters"], "prepared");
        assert_eq!(preparing["completedItems"], "7");
        assert_eq!(preparing["totalItems"], "9");

        record_counters(
            root.path(),
            &job.id,
            "prepared",
            super::super::phase_progress::PhaseCounters {
                items: 9,
                total_items: 9,
                bytes: 90,
                total_bytes: 90,
            },
        )
        .unwrap();
        let transferred = job_summary(root.path(), store.read(&job.id).unwrap());
        assert_eq!(transferred["counters"], "transferred");
        assert_eq!(transferred["totalItems"], "1");
        assert_eq!(transferred["completedItems"], "0");
        assert_eq!(transferred["totalBytes"], bytes.len().to_string());
        assert_eq!(transferred["completedBytes"], "0");
    }

    /// The budget is what other jobs are holding, so a job is never held back
    /// by its own material and a job that ended still counts until a cleanup
    /// releases it.
    #[test]
    fn a_producing_job_waits_for_the_spool_another_job_still_holds() {
        use super::super::journal::TRANSFER_SPOOL_BUDGET;
        let full = TRANSFER_SPOOL_BUDGET - super::super::packaging::producing_job_spool_headroom().unwrap() + 1;
        let root = tempfile::tempdir().unwrap();
        let store = JobStore::open(root.path()).unwrap();
        let mut holder = automatic_job();
        holder.request.connection_id = "holding-connection".into();
        holder.summary["connectionId"] = json!("holding-connection");
        store.put(&holder).unwrap();
        let waiting = automatic_job();
        store.put(&waiting).unwrap();
        let directory = job_directory(root.path(), &holder.request.connection_id, &holder.id);
        std::fs::create_dir_all(&directory).unwrap();
        let held = std::fs::File::create(directory.join("held.spool")).unwrap();
        held.set_len(full - 1).unwrap();
        assert!(spool_budget_owner(root.path(), &store, &waiting).unwrap().is_none());
        held.set_len(full).unwrap();
        assert_eq!(
            spool_budget_owner(root.path(), &store, &waiting).unwrap(),
            Some((holder.id.clone(), full)),
        );
        assert!(spool_budget_owner(root.path(), &store, &holder).unwrap().is_none());
        holder.summary["state"] = json!("failed");
        store.put(&holder).unwrap();
        assert!(spool_budget_owner(root.path(), &store, &waiting).unwrap().is_some());
        holder.summary["state"] = json!("cancelled");
        store.put(&holder).unwrap();
        assert!(spool_budget_owner(root.path(), &store, &waiting).unwrap().is_none());
    }
    #[test]
    fn retained_lww_and_job_spools_share_admission_and_start_headroom() {
        let root = tempfile::tempdir().unwrap();
        let store = JobStore::open(root.path()).unwrap();
        let job = automatic_job();
        store.put(&job).unwrap();
        let lww = root.path().join("external-storage/lww-publications/synthetic-lww");
        std::fs::create_dir_all(&lww).unwrap();
        let retained = std::fs::File::create(lww.join("pack.spool")).unwrap();
        let limit = super::super::journal::TRANSFER_SPOOL_BUDGET;
        let data = lww.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let retained_control = std::fs::File::create(data.join("control.spool")).unwrap();
        retained.set_len(100).unwrap();
        retained_control.set_len(limit - 101).unwrap();
        assert!(spool_budget(root.path(), &job.id).try_reserve(2).unwrap().is_none());
        assert_eq!(spool_budget_owner(root.path(), &store, &job).unwrap(), Some(("synthetic-lww".into(), limit - 1)));
        retained.set_len(0).unwrap();
        retained_control.set_len(0).unwrap();
        let lww_budget = spool_budget(root.path(), "synthetic-lww");
        let in_flight = lww_budget.try_reserve(100).unwrap().unwrap();
        assert!(spool_budget(root.path(), &job.id).try_reserve(limit - 100).unwrap().is_some());
        drop(in_flight);
        let directory = job_directory(root.path(), &job.request.connection_id, &job.id);
        std::fs::create_dir_all(&directory).unwrap();
        let retained_job = std::fs::File::create(directory.join("pack.spool")).unwrap();
        retained_job.set_len(limit - 1).unwrap();
        assert!(spool_budget(root.path(), "synthetic-lww").try_reserve(2).unwrap().is_none());
        retained_job.set_len(0).unwrap();
        assert!(spool_budget(root.path(), "synthetic-lww").try_reserve(limit).unwrap().is_some());
    }

    /// A producing job starts only when the spool the other jobs hold leaves
    /// room for every pack its first wave seals, so a full spool makes it wait
    /// before it starts instead of refusing it partway through.
    #[test]
    fn a_producing_job_waits_to_start_until_its_first_wave_fits() {
        use super::super::journal::{SpoolBudget, TRANSFER_SPOOL_BUDGET};
        use super::super::packaging::{producing_job_spool_headroom, MAX_PACK_PLAINTEXT_BYTES};
        use risunest_external_storage_format::snapshot as wire;
        let headroom = producing_job_spool_headroom().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = JobStore::open(root.path()).unwrap();
        let mut holder = automatic_job();
        holder.request.connection_id = "holding-connection".into();
        holder.summary["connectionId"] = json!("holding-connection");
        store.put(&holder).unwrap();
        let starting = automatic_job();
        store.put(&starting).unwrap();
        let directory = job_directory(root.path(), &holder.request.connection_id, &holder.id);
        std::fs::create_dir_all(&directory).unwrap();
        let held = std::fs::File::create(directory.join("held.spool")).unwrap();

        held.set_len(TRANSFER_SPOOL_BUDGET - headroom + 1).unwrap();
        assert_eq!(
            spool_budget_owner(root.path(), &store, &starting).unwrap(),
            Some((holder.id.clone(), TRANSFER_SPOOL_BUDGET - headroom + 1)),
        );
        held.set_len(TRANSFER_SPOOL_BUDGET - headroom).unwrap();
        assert!(spool_budget_owner(root.path(), &store, &starting).unwrap().is_none());

        // The job that started seals its largest packs with the room sealing
        // asks for, and every one of them is admitted.
        let retaining = vec![
            (holder.id.clone(), directory.clone()),
            (starting.id.clone(), job_directory(root.path(), &starting.request.connection_id, &starting.id)),
        ];
        let budget = SpoolBudget::new(starting.id.clone(), move || Ok(retaining.clone()))
            .with_limit(TRANSFER_SPOOL_BUDGET);
        let largest_pack = wire::envelope_length(
            &wire::PublicObjectHeader::new(
                "r".repeat(128),
                format!("pack-{}", "0".repeat(64)),
                wire::ObjectRole::Pack,
                MAX_PACK_PLAINTEXT_BYTES,
            )
            .unwrap(),
        )
        .unwrap();
        let mut admitted = Vec::new();
        for waiting in 0..super::super::journal::ACTIVE_PACK_FAMILIES {
            let room = budget
                .try_reserve(largest_pack + super::super::control::inventory_page_headroom(waiting + 1))
                .unwrap()
                .unwrap_or_else(|| panic!("pack {waiting} of the first wave was refused"));
            admitted.push(room);
        }
    }

    /// A13. The transfer spool is an app-wide budget, so what every unfinished
    /// job holds counts together, whatever connection it belongs to and
    /// whether it is working or waiting for its turn. A job's own spool is not
    /// what holds it back, and a job stops counting when it is released rather
    /// than when it stops.
    #[test]
    fn the_transfer_spool_budget_sums_what_every_unfinished_job_holds() {
        use super::super::journal::TRANSFER_SPOOL_BUDGET;
        let root = tempfile::tempdir().unwrap();
        let store = JobStore::open(root.path()).unwrap();
        let hold = |connection: &str, state: &str, bytes: u64| -> DurableJob {
            let mut job = automatic_job();
            job.request.connection_id = connection.into();
            job.summary["connectionId"] = json!(connection);
            job.summary["state"] = json!(state);
            store.put(&job).unwrap();
            if bytes > 0 {
                let directory = job_directory(root.path(), connection, &job.id);
                std::fs::create_dir_all(&directory).unwrap();
                std::fs::File::create(directory.join("held.spool"))
                    .unwrap()
                    .set_len(bytes)
                    .unwrap();
            }
            job
        };
        let requester = hold("connection-c", "queued", 0);
        let smaller = hold("connection-a", "waiting", TRANSFER_SPOOL_BUDGET / 3);
        assert!(spool_budget_owner(root.path(), &store, &requester).unwrap().is_none());

        // Two connections together reach it, and the larger holder is the one
        // the waiting job is told about.
        let larger = hold("connection-b", "waiting", TRANSFER_SPOOL_BUDGET - TRANSFER_SPOOL_BUDGET / 3);
        assert_eq!(
            spool_budget_owner(root.path(), &store, &requester).unwrap(),
            Some((larger.id.clone(), TRANSFER_SPOOL_BUDGET - TRANSFER_SPOOL_BUDGET / 3)),
        );
        // Without its own spool the smaller holder is under the budget, so it
        // is never held back by what it is itself holding.
        assert!(spool_budget_owner(root.path(), &store, &smaller).unwrap().is_none());

        let mut released = larger.clone();
        released.summary["state"] = json!("succeeded");
        store.put(&released).unwrap();
        assert!(spool_budget_owner(root.path(), &store, &requester).unwrap().is_none());

        // A job that failed still owns its spool until a cleanup takes it.
        released.summary["state"] = json!("failed");
        store.put(&released).unwrap();
        assert!(spool_budget_owner(root.path(), &store, &requester).unwrap().is_some());
    }

    #[test]
    fn a_partial_restore_is_neither_a_failed_noop_nor_a_remote_publication() {
        let mut job = automatic_job();
        job.request.kind = JobKind::Restore;
        assert!(!settle_local_restore_recovery(&mut job));
        job.summary["applicationStarted"] = json!(true);
        for state in ["running", "failed", "cancelled", "waiting"] {
            job.summary["state"] = json!(state);
            assert!(settle_local_restore_recovery(&mut job));
            assert_eq!(job.summary["state"], "uncertain");
            assert_eq!(job.summary["phase"], "local-apply-unknown");
            assert_eq!(job.summary["error"]["retryable"], false);
            assert!(!job.terminal());
        }
        settle_interrupted(&mut job, Some(json!({"receivedRevision":"2"})), false);
        assert_eq!(job.summary["state"], "succeeded");
        assert_eq!(job.summary["result"]["receivedRevision"], "2");
        assert!(job.summary.get("error").is_none());
    }

    #[test]
    fn a_stopped_restore_stays_failed_when_it_is_reconciled() {
        let mut job = automatic_job();
        job.request.kind = JobKind::Restore;
        job.summary["applicationStarted"] = json!(true);
        job.summary["state"] = json!("failed");
        job.summary["restoreStopped"] = json!(true);
        assert!(!settle_local_restore_recovery(&mut job));
        assert_eq!(job.summary["state"], "failed");
        assert!(job.terminal());
    }

    #[test]
    fn a_detached_unknown_publication_resumes_only_by_its_own_id() {
        let mut unknown = automatic_job();
        let same = unknown.request.clone();
        assert!(pending_matches_request(&unknown, &same));
        let mut elsewhere = same.clone();
        elsewhere.connection_id = "other".into();
        assert!(!pending_matches_request(&unknown, &elsewhere));
        unknown.summary["state"] = json!("uncertain");
        unknown.summary["phase"] = json!("publication-unknown");
        assert!(!pending_matches_request(&unknown, &same));

        let restore: StartJobRequest = serde_json::from_value(json!({
            "connectionId": unknown.request.connection_id,
            "kind": "restore",
            "snapshotId": "snapshot",
            "targetRevision": "0",
            "restoreAreas": ["library"]
        }))
        .unwrap();
        assert!(!pending_matches_request(&unknown, &restore));
    }



    #[test]
    fn interrupted_outcome_recovers_only_authoritative_completion() {
        let request = serde_json::from_value(json!({"connectionId":"x","kind":"backup"})).unwrap();
        let identity = persistent_store::sync_selection::CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "library".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision: 1,
        };
        let mut job = DurableJob::new(request, 1, identity);
        job.summary["state"] = json!("running");
        settle_interrupted(&mut job, None, true);
        assert_eq!(job.summary["state"], "uncertain");
        assert!(job.summary.get("result").is_none());
        settle_interrupted(&mut job, None, false);
        assert_eq!(job.summary["state"], "waiting");
        assert_eq!(job.summary["error"]["action"], "retry");
        job.summary["state"] = json!("failed");
        settle_interrupted(
            &mut job,
            Some(json!({"snapshotId":"snapshot", "publishedRevision":"5"})),
            false,
        );
        assert_eq!(job.summary["state"], "succeeded");
        assert_eq!(job.summary["result"], json!({"snapshotId":"snapshot", "publishedRevision":"5"}));
        assert!(job.summary.get("error").is_none());
    }
    #[test]
    fn queued_job_allows_edits_and_reselection_but_rejects_replacement() {
        let request = serde_json::from_value(json!({"connectionId":"x","kind":"backup"})).unwrap();
        let identity = persistent_store::sync_selection::CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "library".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision: 5,
        };
        let job = DurableJob::new(request, 1, identity.clone());
        let mut current = identity;
        current.revision = 10;
        assert!(require_admitted_library(&job, &current).is_ok());
        current.selection_epoch = "another".into();
        assert!(require_admitted_library(&job, &current).is_ok());
        current.library_epoch = "replacement".into();
        assert!(require_admitted_library(&job, &current).is_err());
        current.library_epoch = "library".into();
        current.generation = "replacement".into();
        assert!(require_admitted_library(&job, &current).is_err());
    }
    #[test]
    fn stale_and_hidden_sessions_never_publish() {
        let mut request:StartJobRequest=serde_json::from_value(json!({"connectionId":"x","kind":"backup","reason":"automatic","session":"foreground","sessionId":"old"})).unwrap();
        assert!(require_session(
            &request,
            &Session {
                kind: "foreground".into(),
                id: "new".into()
            }
        )
        .is_err());
        request.reason = Some("manual".into());
        request.session = None;
        request.session_id = None;
        assert!(require_session(
            &request,
            &Session {
                kind: "foreground".into(),
                id: "new".into()
            }
        )
        .is_ok());
        assert!(require_session(
            &request,
            &Session {
                kind: "hidden".into(),
                id: "new".into()
            }
        )
        .is_err());
    }
    #[test]
    fn settled_reconciliation_preserves_timestamp_and_does_not_rewrite() {
        let root = tempfile::tempdir().unwrap();
        let store = JobStore::open(root.path()).unwrap();
        let mut before = automatic_job();
        before.summary["state"] = json!("succeeded");
        before.summary["phase"] = json!("completed");
        store.put(&before).unwrap();
        let db = rusqlite::Connection::open(root.path().join("external-jobs.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE updates(count INTEGER); INSERT INTO updates VALUES(0);
            CREATE TRIGGER count_updates AFTER UPDATE ON external_requests BEGIN UPDATE updates SET count=count+1; END;").unwrap();
        for _ in 0..5 {
            let mut current = before.clone();
            current.summary["updatedAtMs"] = json!("9999999");
            persist_reconciled_job(root.path(), &before, &mut current).unwrap();
            assert_eq!(current.summary["updatedAtMs"], before.summary["updatedAtMs"]);
        }
        let count: i64 = db.query_row("SELECT count FROM updates", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0);
        let mut transitioned = before.clone();
        transitioned.summary["phase"] = json!("settled");
        persist_reconciled_job(root.path(), &before, &mut transitioned).unwrap();
        assert_eq!(db.query_row("SELECT count FROM updates", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    }
    #[test]
    fn terminal_summary_never_reads_transfer_journals() {
        let root = tempfile::tempdir().unwrap();
        let mut job = automatic_job();
        job.summary["state"] = json!("succeeded");
        job.summary["completedBytes"] = json!("123");
        let path = job_directory(root.path(), &job.request.connection_id, &job.id);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("transfers.sqlite"), b"invalid database").unwrap();
        assert_eq!(job_summary(root.path(), job)["completedBytes"], "123");
    }

}
