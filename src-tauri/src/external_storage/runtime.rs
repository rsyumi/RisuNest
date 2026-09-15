//! Native job admission and bounded renderer DTOs. Network stages own no PDS mutex.
use super::{
    connection_store::ConnectionStore,
    contract::*,
    job_store::{DurableJob, JobCommandState, JobKind, JobStore, Session, StartJobRequest},
    publication::ExecutionSession,
};
use crate::persistent_store::{
    self,
    external_conflicts::{ConflictPhase, ConflictPreservation},
    sync_selection::{Selection, SyncTarget},
    PersistentStore,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
pub(crate) fn local_error(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}
pub(crate) fn root(app: &AppHandle) -> Result<PathBuf> {
    app.state::<JobCommandState>()
        .root
        .get()
        .cloned()
        .ok_or_else(|| ProviderError::new(ErrorKind::Transient))
}
pub(crate) fn native_store(app: &AppHandle) -> Result<PersistentStore> {
    persistent_store::commands::with_store_mut(app.state(), |store| store.open_native_job_store())
        .map_err(local_error)
}
pub(crate) fn selection_dto(selection: Selection) -> Value {
    let (kind, id) = match selection.target {
        SyncTarget::None => ("none", None),
        SyncTarget::Server(id) => ("server", Some(id)),
        SyncTarget::External(id) => ("external", Some(id)),
    };
    let mut dto = json!({"kind":kind,"selectionEpoch":selection.epoch,"paused":selection.paused,"decisionRequired":selection.decision_required});
    if let Some(id) = id {
        dto["connectionId"] = json!(id);
    }
    dto
}
fn require_session(request: &StartJobRequest, current: &Session) -> Result<ExecutionSession> {
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
    match current.kind.as_str() {
        "foreground" => Ok(ExecutionSession::Foreground),
        "exitDrain" => Ok(ExecutionSession::ExitDrain),
        _ => Err(ProviderError::new(ErrorKind::Cancelled)),
    }
}
pub(crate) fn read_job_session(app: &AppHandle, id: &str) -> Result<ExecutionSession> {
    let job = JobStore::open(&root(app)?)?.read(id)?;
    let state = app.state::<JobCommandState>();
    let current = state.session.lock().map_err(local_error)?;
    require_session(&job.request, &current)
}
pub(crate) async fn require_connection_idle(app: &AppHandle, connection: &str) -> Result<()> {
    let state = app.state::<JobCommandState>();
    if state
        .active
        .lock()
        .map_err(local_error)?
        .values()
        .any(|(id, _)| id == connection)
        || JobStore::open(&root(app)?)?
            .list_pending()?
            .iter()
            .any(|job| job.request.connection_id == connection && !job.terminal())
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(())
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
    if !["foreground", "hidden", "exitDrain"].contains(&request.kind.as_str())
        || request.id.is_empty()
        || request.id.len() > 1024
    {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let state = app.state::<JobCommandState>();
    let mut session = state.session.lock().map_err(local_error)?;
    if session.kind == "exitDrain" && request.kind == "hidden" {
        return Ok(());
    }
    let hidden = request.kind == "hidden";
    *session = Session {
        kind: request.kind,
        id: request.id,
    };
    if hidden {
        for (_, cancel) in state.active.lock().map_err(local_error)?.values() {
            cancel.cancel();
        }
    }
    Ok(())
}
#[tauri::command]
pub(crate) fn external_storage_capture_exit_target(app: AppHandle) -> Result<Value> {
    let store = native_store(&app)?;
    let identity = store.external_identity().map_err(local_error)?;
    Ok(
        json!({"revision":identity.revision.to_string(),"libraryEpoch":identity.library_epoch,"selection":selection_dto(store.external_selection().map_err(local_error)?)}),
    )
}
#[tauri::command]
pub(crate) fn external_storage_get_state(app: AppHandle) -> Result<Value> {
    super::runtime_restore::reconcile_device_restore_settlements(&app)?;
    let root = root(&app)?;
    let connections = ConnectionStore::open(&root)?
        .list()?
        .iter()
        .map(super::connection_commands::summary)
        .collect::<Result<Vec<_>>>()?;
    let jobs = JobStore::open(&root)?
        .list_for_state()?
        .into_iter()
        .map(|job| reconcile_job(&app, job).map(|job| job_summary(&root, job)))
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"supported":true,"selection":selection_dto(native_store(&app)?.external_selection().map_err(local_error)?),"connections":connections,"jobs":jobs}),
    )
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
    if !app
        .state::<JobCommandState>()
        .active
        .lock()
        .map_err(local_error)?
        .is_empty()
    {
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
    Ok(selection_dto(selected))
}
#[tauri::command]
pub(crate) fn external_storage_get_job(app: AppHandle, job_id: String) -> Result<Value> {
    super::runtime_restore::reconcile_device_restore_settlements(&app)?;
    let directory = root(&app)?;
    Ok(job_summary(
        &directory,
        reconcile_job(&app, JobStore::open(&directory)?.read(&job_id)?)?,
    ))
}
fn job_summary(root: &std::path::Path, mut job: DurableJob) -> Value {
    if let Ok((done, total, items, count)) = super::journal::TransferJournal::progress(
        &job_directory(root, &job.request.connection_id, &job.id),
        &job.id,
    ) {
        job.summary["completedBytes"] = json!(done.to_string());
        job.summary["totalBytes"] = json!(total.to_string());
        job.summary["completedItems"] = json!(items.to_string());
        job.summary["totalItems"] = json!(count.to_string());
    }
    job.summary
}
#[tauri::command]
pub(crate) async fn external_storage_cancel_job(app: AppHandle, job_id: String) -> Result<Value> {
    let store = JobStore::open(&root(&app)?)?;
    let state = app.state::<JobCommandState>();
    {
        if let Some((_, cancel)) = state.active.lock().map_err(local_error)?.get(&job_id) {
            cancel.cancel();
        }
    }
    wait_for_job_release(&app, &job_id).await?;
    let mut job = reconcile_job(&app, store.read(&job_id)?)?;
    if job.summary["state"] != "succeeded" {
        let mut pds = native_store(&app)?;
        let authoritative = pds.external_job(&job_id).map_err(local_error)?;
        let conflict = pds.external_conflict(&job_id).map_err(local_error)?;
        let local_conflict = conflict
            .as_ref()
            .is_some_and(|record| record.preservation == ConflictPreservation::LocalOnly);
        if authoritative.as_ref().is_some_and(|item| {
            ["publishing", "publicationUnknown", "applying"].contains(&item.phase.as_str())
        }) {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        if local_conflict
            || authoritative
                .as_ref()
                .is_some_and(|item| item.phase == "conflictPreserving")
        {
            job.summary["state"] = json!("waiting");
            job.summary["phase"] = json!("conflict-preservation-paused");
            job.summary["error"] = error_dto(&ProviderError::new(ErrorKind::Cancelled));
            job.summary["updatedAtMs"] = json!(now_ms().to_string());
            store.put(&job)?;
            return Ok(job.summary);
        }
        if conflict.as_ref().is_some_and(|record| {
            record.phase == ConflictPhase::Pending
                && record.preservation == ConflictPreservation::RemoteComplete
        }) {
            job.summary["state"] = json!("conflict");
            job.summary["phase"] = json!("conflict-choice");
            job.summary["result"] = json!({"conflictId":job_id});
            job.summary["updatedAtMs"] = json!(now_ms().to_string());
            store.put(&job)?;
            return Ok(job.summary);
        }
        if authoritative
            .as_ref()
            .is_some_and(|item| ["preparing", "ready", "stale"].contains(&item.phase.as_str()))
        {
            if job.request.kind == JobKind::ResolveConflict {
                pds.external_reject_conflict_resolution(&job_id)
                    .map_err(local_error)?;
            } else {
                pds.external_cancel_prepared(&job_id).map_err(local_error)?;
            }
        }
        let preserved_choice = job.request.kind == JobKind::ResolveConflict
            && pds
                .external_conflict(&job_id)
                .map_err(local_error)?
                .is_some_and(|record| record.phase == ConflictPhase::Pending);
        job.summary["state"] = json!(if preserved_choice {
            "conflict"
        } else {
            "cancelled"
        });
        job.summary["phase"] = json!(if preserved_choice {
            "conflict-choice"
        } else {
            "cancelled"
        });
        if preserved_choice {
            job.summary["result"] = json!({"conflictId":job_id});
        }
        job.summary["updatedAtMs"] = json!(now_ms().to_string());
        store.put(&job)?;
    }
    Ok(job.summary)
}
#[tauri::command]
pub(crate) async fn external_storage_start_job(
    app: AppHandle,
    mut request: StartJobRequest,
) -> Result<Value> {
    request.validate()?;
    let root = root(&app)?;
    let connection = ConnectionStore::open(&root)?.read(&request.connection_id)?;
    let command_state = app.state::<JobCommandState>();
    {
        let current = command_state.session.lock().map_err(local_error)?;
        require_session(&request, &current)?;
        request.session = Some(current.kind.clone());
        request.session_id = Some(current.id.clone());
    }
    if matches!(request.kind, JobKind::Sync | JobKind::ResolveConflict) {
        let selected = native_store(&app)?
            .external_selection()
            .map_err(local_error)?;
        if selected.target != SyncTarget::External(request.connection_id.clone())
            || (selected.paused && request.session.as_deref() != Some("exitDrain"))
        {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        if connection.descriptor.publication_strategy.is_none() {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
    }
    let store = JobStore::open(&root)?;
    if let Some(mut pending) = store
        .list_pending()?
        .into_iter()
        .find(|job| job.request.connection_id == request.connection_id && !job.terminal())
    {
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
        let resolving = pending.summary["state"] == "conflict"
            && request.kind == JobKind::ResolveConflict
            && request
                .conflict_id
                .as_deref()
                .is_some_and(|id| pending.summary["result"]["conflictId"].as_str() == Some(id));
        if pending.request.kind != request.kind && !resolving {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        if !resolving && !same_requested_operation(&pending.request, &request) {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        if pending.terminal() {
            return Ok(pending.summary);
        }
        if !command_state
            .active
            .lock()
            .map_err(local_error)?
            .contains_key(&pending.id)
        {
            if resolving {
                pending.request.kind = request.kind;
                pending.request.conflict_id = request.conflict_id;
                pending.request.choice = request.choice;
            }
            pending.request.session = request.session;
            pending.request.session_id = request.session_id;
            store.put(&pending)?;
            wake_job(app.clone(), pending.id.clone())?;
        }
        return Ok(pending.summary);
    }
    // Section publication is not wired yet, so a backup captures the library
    // only and no device capture phase is scheduled.
    let device = false;
    let identity = native_store(&app)?
        .external_identity()
        .map_err(local_error)?;
    let job = DurableJob::new(request, device, now_ms(), identity);
    store.put(&job)?;
    if !device {
        wake_job(app, job.id.clone())?;
    }
    Ok(job.summary)
}
fn same_requested_operation(existing: &StartJobRequest, incoming: &StartJobRequest) -> bool {
    if existing.kind != incoming.kind || existing.connection_id != incoming.connection_id {
        return false;
    }
    match existing.kind {
        JobKind::Restore => {
            existing.snapshot_id == incoming.snapshot_id
                && existing.restore_areas == incoming.restore_areas
        }
        JobKind::PinHistory => existing.snapshot_id == incoming.snapshot_id,
        JobKind::ResolveConflict => {
            existing.conflict_id == incoming.conflict_id && existing.choice == incoming.choice
        }
        JobKind::Sync | JobKind::Backup => true,
    }
}
async fn wait_for_job_release(app: &AppHandle, id: &str) -> Result<()> {
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

fn completed_job_result(pds: &mut PersistentStore, job: &DurableJob) -> Result<Option<Value>> {
    if job.request.kind == JobKind::PinHistory {
        return super::history_jobs::completed_pin_history(pds, job);
    }
    let Some(completed) = pds
        .external_job(&job.id)
        .map_err(local_error)?
        .filter(|item| item.connection_id == job.request.connection_id && item.phase == "complete")
    else {
        return Ok(None);
    };
    if job.request.kind == JobKind::ResolveConflict
        && pds
            .external_conflict(&job.id)
            .map_err(local_error)?
            .is_some_and(|record| {
                matches!(
                    record.phase,
                    ConflictPhase::Resolving | ConflictPhase::PublicationUnknown
                )
            })
    {
        pds.external_finish_conflict(&job.id).map_err(local_error)?;
    }
    if job.request.kind == JobKind::Backup {
        let (snapshot, identity) = pds
            .external_backup_result(&job.id)
            .map_err(local_error)?
            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
        return Ok(Some(
            json!({"snapshotId":snapshot,"publishedRevision":identity.revision.to_string()}),
        ));
    }
    if matches!(job.request.kind, JobKind::Sync | JobKind::ResolveConflict) {
        if let Some(base) = pds
            .external_base(&job.request.connection_id)
            .map_err(local_error)?
        {
            if base.repository_id == completed.repository_id
                && base.commit_id == completed.commit_id
            {
                return Ok(Some(if completed.role == "restore" {
                    json!({"snapshotId":base.snapshot_id,"receivedRevision":base.identity.revision.to_string()})
                } else {
                    json!({"snapshotId":base.snapshot_id,"publishedRevision":completed.identity.revision.to_string()})
                }));
            }
        }
    }
    Ok(None)
}

pub(crate) fn require_admitted_library(
    job: &DurableJob,
    current: &persistent_store::sync_selection::CaptureIdentity,
) -> Result<()> {
    let admitted = &job.admission_identity;
    if admitted.store_id != current.store_id
        || admitted.library_epoch != current.library_epoch
        || admitted.generation != current.generation
        || (matches!(job.request.kind, JobKind::Sync | JobKind::ResolveConflict)
            && admitted.selection_epoch != current.selection_epoch)
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(())
}

fn reconcile_job(app: &AppHandle, mut job: DurableJob) -> Result<DurableJob> {
    let state = app.state::<JobCommandState>();
    let active = state.active.lock().map_err(local_error)?;
    if (job.terminal() && job.request.kind != JobKind::ResolveConflict)
        || job.summary["phase"] == "device-restore-maintenance"
        || active.contains_key(&job.id)
    {
        return Ok(job);
    }
    let mut pds = native_store(app)?;
    let complete = completed_job_result(&mut pds, &job)?;
    if complete.is_none() && job.summary["state"] != "running" {
        return Ok(job);
    }
    let uncertain = pds
        .external_job(&job.id)
        .map_err(local_error)?
        .is_some_and(|item| {
            ["publishing", "publicationUnknown", "applying"].contains(&item.phase.as_str())
        });
    settle_interrupted(&mut job, complete, uncertain);
    // The renderer still receives a settled result if its auxiliary cache is unwritable.
    let _ = JobStore::open(&root(app)?).and_then(|store| store.put(&job));
    Ok(job)
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
        job.summary["error"] = error_dto(&ProviderError::new(ErrorKind::Transient));
    }
    job.summary["updatedAtMs"] = json!(now_ms().to_string());
}
pub(crate) fn wake_job(app: AppHandle, id: String) -> Result<()> {
    let job = JobStore::open(&root(&app)?)?.read(&id)?;
    if job.terminal() {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    read_job_session(&app, &id)?;
    let cancel = Cancellation::default();
    {
        let state = app.state::<JobCommandState>();
        let mut active = state.active.lock().map_err(local_error)?;
        if active.contains_key(&id) {
            return Ok(());
        }
        if active
            .values()
            .any(|(connection, _)| connection == &job.request.connection_id)
        {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        active.insert(
            id.clone(),
            (job.request.connection_id.clone(), cancel.clone()),
        );
    }
    tauri::async_runtime::spawn(async move {
        let result = run_job(&app, &id, &cancel).await;
        let persisted = (|| -> Result<()> {
            let store = JobStore::open(&root(&app)?)?;
            let mut job = store.read(&id)?;
            match result {
                Ok(result) => {
                    let maintenance = result.get("maintenanceSessionId").is_some();
                    let receive = result.get("receiveReady").and_then(Value::as_bool) == Some(true);
                    job.summary["state"] = json!(if maintenance || receive {
                        "waiting"
                    } else if result.get("conflictId").is_some() {
                        "conflict"
                    } else {
                        "succeeded"
                    });
                    job.summary["phase"] = json!(if maintenance {
                        "device-restore-maintenance"
                    } else if receive {
                        "remote-apply"
                    } else {
                        "complete"
                    });
                    job.summary["result"] = result;
                }
                Err(error) => {
                    let mut pds = native_store(&app)?;
                    let authoritative = pds.external_job(&job.id).map_err(local_error)?;
                    let preserving = authoritative
                        .as_ref()
                        .is_some_and(|item| item.phase == "conflictPreserving")
                        || pds
                            .external_conflict(&job.id)
                            .map_err(local_error)?
                            .is_some_and(|record| {
                                record.preservation == ConflictPreservation::LocalOnly
                            });
                    let pending = authoritative.iter().any(|item| {
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
                    );
                    if !pending && !retryable && !preserving {
                        if let Some(item) = authoritative.iter().find(|item| {
                            item.id == id
                                && ["preparing", "ready", "stale"].contains(&item.phase.as_str())
                        }) {
                            if job.request.kind == JobKind::ResolveConflict {
                                pds.external_reject_conflict_resolution(&item.id)
                                    .map_err(local_error)?;
                            } else {
                                pds.external_cancel_prepared(&item.id)
                                    .map_err(local_error)?;
                            }
                        }
                    }
                    let preserved_choice = job.request.kind == JobKind::ResolveConflict
                        && pds
                            .external_conflict(&job.id)
                            .map_err(local_error)?
                            .is_some_and(|record| {
                                record.phase == ConflictPhase::Pending
                                    && record.preservation == ConflictPreservation::RemoteComplete
                            });
                    job.summary["state"] = json!(if pending {
                        "uncertain"
                    } else if preserved_choice {
                        "conflict"
                    } else if retryable || preserving {
                        "waiting"
                    } else {
                        "failed"
                    });
                    job.summary["phase"] = json!(if pending {
                        "publication-unknown"
                    } else if preserved_choice {
                        "conflict-choice"
                    } else if preserving {
                        "conflict-preservation-paused"
                    } else {
                        "paused"
                    });
                    if preserved_choice {
                        job.summary["result"] = json!({"conflictId":job.id});
                    }
                    job.summary["error"] = error_dto(&error);
                }
            }
            job.summary["updatedAtMs"] = json!(now_ms().to_string());
            store.put(&job)
        })();
        if persisted.is_err() {
            crate::nlog!("error", "External job outcome could not be persisted");
        }
        if let Ok(mut active) = app.state::<JobCommandState>().active.lock() {
            active.remove(&id);
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
            "resolve-conflict",
            false,
        ),
        ErrorKind::RateLimited | ErrorKind::DailyQuotaExhausted => {
            ("The provider request budget is exhausted.", "wait", true)
        }
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
pub(crate) fn job_directory(root: &std::path::Path, connection: &str, job: &str) -> PathBuf {
    root.join("external-storage")
        .join(hex::encode(
            risunest_external_storage_format::content_identity::hash(connection.as_bytes()),
        ))
        .join("jobs")
        .join(hex::encode(
            risunest_external_storage_format::content_identity::hash(job.as_bytes()),
        ))
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
    if matches!(
        job.request.kind,
        JobKind::Backup | JobKind::Sync | JobKind::ResolveConflict
    ) {
        require_admitted_library(
            &job,
            &native_store(app)?
                .external_identity()
                .map_err(local_error)?,
        )?;
    }
    job.summary["state"] = json!("running");
    job.summary["phase"] = json!("opening");
    job.summary["updatedAtMs"] = json!(now_ms().to_string());
    job.summary.as_object_mut().unwrap().remove("error");
    store.put(&job)?;
    let connected =
        super::connection_commands::open_connected(app, &job.request.connection_id).await?;
    cancel.check()?;
    match job.request.kind {
        JobKind::Backup => run_backup(app, &connected, &job, cancel).await,
        JobKind::Sync | JobKind::ResolveConflict => {
            super::sync_engine::run_sync(app, &connected, &job, cancel).await
        }
        JobKind::Restore => {
            super::runtime_restore::run_restore(app, &connected, &job, cancel).await
        }
        JobKind::PinHistory => {
            super::history_jobs::run_pin_history(app, &connected, &job, cancel).await
        }
    }
}
async fn run_backup(
    app: &AppHandle,
    connected: &super::connection_commands::ConnectedRepository,
    job: &DurableJob,
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
    let (capture, fingerprint) = tokio::task::spawn_blocking(move || -> Result<_> {
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
                let capture = pds
                    .capture_external_library(
                        &worker_job.request.connection_id,
                        hydration
                            .as_ref()
                            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?,
                        &probe,
                    )
                    .map_err(local_error)?;
                pds.external_prepare_backup(
                    &worker_job.id,
                    &worker_job.request.connection_id,
                    &repository_id,
                    &capture.id,
                    &worker_job.snapshot_id,
                )
                .map_err(local_error)?;
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
        Ok((capture, fingerprint))
    })
    .await
    .map_err(local_error)??;
    let identity = capture.identity.clone();
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
        },
    };
    let cache = directory
        .parent()
        .and_then(|path| path.parent())
        .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?
        .join("package-cache");
    let completed = super::packaging::package_and_upload(
        capture,
        &root,
        &cache,
        metadata,
        &connected.root_key,
        PackageLimits::from_capabilities(&connected.stored.capabilities)?,
        &mut journal,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
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
    ConnectionStore::open(&root)?.remember_discovery(
        &job.request.connection_id,
        &completed.snapshot_id,
        &completed.reference,
    )?;
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
        json!({"snapshotId":completed.snapshot_id,"publishedRevision":identity.revision.to_string()}),
    )
}
#[tauri::command]
pub(crate) async fn external_storage_get_quota(
    app: AppHandle,
    connection_id: String,
) -> Result<Value> {
    let connected = super::connection_commands::open_connected(&app, &connection_id).await?;
    let root = root(&app)?;
    let budget = super::connection_commands::budget(&app)?;
    let summaries = super::quota_profiles::configure_connection_budget(
        &budget,
        &connected.stored.config.provider,
        connected.stored.config.profile.as_deref(),
        connected.provider.as_ref(),
        &connected.handle,
        &[],
        now_ms(),
    )?;
    let buckets = summaries
        .into_iter()
        .map(|bucket| {
            let mut value =
                json!({"id":bucket.bucket,"used":bucket.used.to_string(),"unit":"requests"});
            if let Some(limit) = bucket.limit {
                value["limit"] = json!(limit.to_string());
                value["remaining"] = json!(limit.saturating_sub(bucket.used).to_string());
            }
            if let QuotaReset::At { unix_ms } = bucket.reset {
                value["resetAtMs"] = json!(unix_ms.to_string());
            }
            value
        })
        .collect::<Vec<_>>();
    let usage =
        super::usage::summarize(&root, &connection_id, &connected, &Cancellation::default())
            .await?;
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
        "locallyUploadedCoverage":"cached-upload-receipts"
    });
    if let Some(latest_reachable) = latest_reachable {
        storage["latestReachable"] = latest_reachable;
    }
    Ok(json!({
        "connectionId":connection_id,
        "buckets":buckets,
        "storage":storage
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_new_restore_or_conflict_choice_never_resumes_a_different_pending_request() {
        let existing: StartJobRequest = serde_json::from_value(json!({"connectionId":"x", "kind":"restore", "snapshotId":"a", "restoreAreas":["library"]})).unwrap();
        let mut incoming = existing.clone();
        assert!(same_requested_operation(&existing, &incoming));
        incoming.snapshot_id = Some("b".into());
        assert!(!same_requested_operation(&existing, &incoming));
        incoming.snapshot_id = existing.snapshot_id.clone();
        incoming.restore_areas = Some(vec!["deviceSettings".into()]);
        assert!(!same_requested_operation(&existing, &incoming));
        let existing: StartJobRequest = serde_json::from_value(json!({"connectionId":"x", "kind":"resolve-conflict", "conflictId":"conflict", "choice":"remote"})).unwrap();
        let mut incoming = existing.clone();
        incoming.choice = Some("local".into());
        assert!(!same_requested_operation(&existing, &incoming));
    }
    #[test]
    fn interrupted_outcome_recovers_only_authoritative_completion() {
        let request = serde_json::from_value(json!({"connectionId":"x","kind":"sync"})).unwrap();
        let identity = persistent_store::sync_selection::CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "library".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision: 1,
        };
        let mut job = DurableJob::new(request, false, 1, identity);
        job.summary["state"] = json!("running");
        settle_interrupted(&mut job, None, true);
        assert_eq!(job.summary["state"], "uncertain");
        assert!(job.summary.get("result").is_none());
        settle_interrupted(&mut job, None, false);
        assert_eq!(job.summary["state"], "waiting");
        assert_eq!(job.summary["error"]["action"], "retry");
        settle_interrupted(
            &mut job,
            Some(json!({"snapshotId":"snapshot", "receivedRevision":"5"})),
            false,
        );
        assert_eq!(job.summary["state"], "succeeded");
        assert_eq!(job.summary["result"]["receivedRevision"], "5");
        assert!(job.summary["result"].get("publishedRevision").is_none());
        assert!(job.summary.get("error").is_none());
    }
    #[test]
    fn queued_job_allows_edits_but_rejects_replacement_and_sync_reselection() {
        let request = serde_json::from_value(json!({"connectionId":"x","kind":"sync"})).unwrap();
        let identity = persistent_store::sync_selection::CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "library".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision: 5,
        };
        let mut job = DurableJob::new(request, false, 1, identity.clone());
        let mut current = identity;
        current.revision = 10;
        assert!(require_admitted_library(&job, &current).is_ok());
        current.selection_epoch = "another".into();
        assert!(require_admitted_library(&job, &current).is_err());
        job.request.kind = JobKind::Backup;
        assert!(require_admitted_library(&job, &current).is_ok());
        current.library_epoch = "replacement".into();
        assert!(require_admitted_library(&job, &current).is_err());
    }
    #[test]
    fn stale_and_hidden_sessions_never_publish() {
        let mut request:StartJobRequest=serde_json::from_value(json!({"connectionId":"x","kind":"sync","reason":"automatic","session":"foreground","sessionId":"old"})).unwrap();
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
        assert_eq!(
            require_session(
                &request,
                &Session {
                    kind: "foreground".into(),
                    id: "new".into()
                }
            )
            .unwrap(),
            ExecutionSession::Foreground
        );
        assert!(require_session(
            &request,
            &Session {
                kind: "hidden".into(),
                id: "new".into()
            }
        )
        .is_err());
    }
}
