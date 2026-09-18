//! Native job admission and bounded renderer DTOs. Network stages own no PDS mutex.
use super::{
    connection_commands::ConnectedRepository,
    connection_store::ConnectionStore,
    contract::*,
    job_store::{DurableJob, JobCommandState, JobKind, JobStore, Session, StartJobRequest},
    leases,
    publication::{PublicationMode, PublicationPermit},
};
use crate::persistent_store::{
    self,
    external_conflicts,
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
pub(crate) fn record_connection_completion(
    app: &AppHandle,
    connection_id: &str,
    kind: super::connection_store::CompletionKind,
) {
    let result = root(app).and_then(|root| {
        ConnectionStore::open(&root)?.record_completion(connection_id, kind, now_ms())
    });
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
    let mut dto = json!({"kind":kind,"selectionEpoch":selection.epoch,"paused":selection.paused,"decisionRequired":selection.decision_required});
    if let Some(id) = id {
        dto["connectionId"] = json!(id);
    }
    dto
}
fn require_session(request: &StartJobRequest, current: &Session) -> Result<PublicationMode> {
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
        "foreground" => Ok(PublicationMode::Foreground),
        "exitDrain" => Ok(PublicationMode::ExitDrain),
        _ => Err(ProviderError::new(ErrorKind::Cancelled)),
    }
}
pub(crate) fn read_job_session(app: &AppHandle, id: &str) -> Result<PublicationMode> {
    let job = JobStore::open(&root(app)?)?.read(id)?;
    let state = app.state::<JobCommandState>();
    let current = state.session.lock().map_err(local_error)?;
    require_session(&job.request, &current)
}
pub(crate) fn publication_permit(app: &AppHandle, id: &str) -> Result<PublicationPermit> {
    let mode = read_job_session(app, id)?;
    let selection = native_store(app)?
        .external_selection()
        .map_err(local_error)?;
    PublicationPermit::new(id.to_owned(), selection.epoch, mode)
}
pub(crate) async fn require_connection_idle(app: &AppHandle, connection: &str) -> Result<()> {
    let state = app.state::<JobCommandState>();
    if state
        .active
        .lock()
        .map_err(local_error)?
        .values()
        .any(|(id, _)| id == connection)
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
    leases::set_system_foreground(!hidden);
    if hidden {
        state.automatic_targets.lock().map_err(local_error)?.clear();
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
    let root = root(&app)?;
    let connections = ConnectionStore::open(&root)?
        .list()?
        .iter()
        .map(super::connection_commands::summary)
        .collect::<Result<Vec<_>>>()?;
    let jobs = JobStore::open(&root)?
        .list_for_state()?
        .into_iter()
        .map(|job| reconcile_job(&app, job).map(|job| {
            continue_observed_automatic(&app, &job);
            job_summary(&root, job)
        }))
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
    state.automatic_targets.lock().map_err(local_error)?.clear();
    let prepared: Vec<_> = state.prepared_receives.lock().map_err(local_error)?
        .keys().cloned().collect();
    drop(_permit);
    for job in prepared {
        if super::sync_engine::discard_receive_preparation(&app, &job).is_err() {
            crate::nlog!("error", "Reselected external receive stage could not be discarded");
        }
    }
    drop(active);
    Ok(selection_dto(selected))
}
#[tauri::command]
pub(crate) fn external_storage_get_job(app: AppHandle, job_id: String) -> Result<Value> {
    let directory = root(&app)?;
    let job = reconcile_job(&app, JobStore::open(&directory)?.read(&job_id)?)?;
    continue_observed_automatic(&app, &job);
    Ok(job_summary(&directory, job))
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
    state.cancel_automatic_target(&store.read(&job_id)?)?;
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
    state.cancel_automatic_target(&job)?;
    super::sync_engine::discard_receive_preparation(&app, &job_id)?;
    job.receive_staging_id = None;
    if job.summary["state"] != "succeeded" {
        let mut pds = native_store(&app)?;
        let authoritative = pds.external_job(&job_id).map_err(local_error)?;
        let conflict = super::sync_engine::conflict_record(&app, &job_id)?;
        let local_conflict = conflict
            .as_ref()
            .is_some_and(|record| !record.resolved && record.remote_point.is_none());
        if authoritative.as_ref().is_some_and(|item| {
            ["publishing", "publicationUnknown", "applying"].contains(&item.phase.as_str())
        }) {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        if local_conflict {
            job.summary["state"] = json!("waiting");
            job.summary["phase"] = json!("conflict-preservation-paused");
            job.summary["error"] = error_dto(&ProviderError::new(ErrorKind::Cancelled));
            job.summary["updatedAtMs"] = json!(now_ms().to_string());
            store.put(&job)?;
            return Ok(job.summary);
        }
        if conflict.as_ref().is_some_and(|record| {
            !record.resolved && record.remote_point.is_some()
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
            pds.external_cancel_prepared(&job_id).map_err(local_error)?;
        }
        let preserved_choice = job.request.kind == JobKind::ResolveConflict
            && conflict.as_ref().is_some_and(|record| !record.resolved);
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
    let pending = store.list_pending()?.into_iter()
        .find(|job| pending_matches_request(job, &request))
        .map(|job| reconcile_job(&app, job))
        .transpose()?;
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
        if pending.summary["state"] == "uncertain"
            && pending.summary["phase"] == "publication-unknown"
            && request.reason.as_deref() == Some("automatic")
        {
            return Ok(pending.summary);
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
            let identity = native_store(&app)?.external_identity().map_err(local_error)?;
            let next = DurableJob::new(request, false, now_ms(), identity);
            store.put(&next)?;
            wake_job(app, next.id.clone())?;
            return Ok(next.summary);
        }
        let current_identity = native_store(&app)?.external_identity().map_err(local_error)?;
        command_state.coalesce_automatic(&pending, &request, &current_identity)?;
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
            if super::sync_engine::prepared_receive_result(&app, &pending.id)?.is_none() {
                wake_job(app.clone(), pending.id.clone())?;
            }
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
fn pending_matches_request(job: &DurableJob, request: &StartJobRequest) -> bool {
    if job.request.connection_id != request.connection_id {
        return false;
    }
    let publication_unknown = job.summary["state"] == "uncertain"
        && job.summary["phase"] == "publication-unknown";
    !publication_unknown
        || (job.request.kind == request.kind
            && matches!(request.kind, JobKind::Sync | JobKind::ResolveConflict))
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
        JobKind::Sync => {
            (existing.reason.as_deref() == Some("automatic"))
                == (incoming.reason.as_deref() == Some("automatic"))
        }
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
    if matches!(job.request.kind, JobKind::Sync | JobKind::ResolveConflict)
        && completed.role == "restore"
    {
        let received = pds.external_receive_completion(&job.id, &job.request.connection_id)
            .map_err(local_error)?
            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
        return Ok(Some(json!({"snapshotId":received.snapshot_id,
            "receivedRevision":received.revision.to_string()})));
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

fn reconcile_job(app: &AppHandle, job: DurableJob) -> Result<DurableJob> {
    let state = app.state::<JobCommandState>();
    let active = state.active.lock().map_err(local_error)?;
    if active.contains_key(&job.id) {
        return Ok(job);
    }
    reconcile_stopped_job(app, job)
}

// The caller either holds the active-jobs mutex or owns the cleanup claim.
fn reconcile_stopped_job(app: &AppHandle, mut job: DurableJob) -> Result<DurableJob> {
    let mut pds = native_store(app)?;
    let complete = if job.request.kind == JobKind::Restore {
        super::runtime_restore::completed_restore(app, &job)?
    } else {
        completed_job_result(&mut pds, &job)?
    };
    if let Some(result) = complete {
        if job.request.kind == JobKind::ResolveConflict {
            let settled = match super::sync_engine::conflict_record(app, &job.id) {
                Ok(Some(record)) if record.resolved => Ok(()),
                Ok(Some(record)) if record.remote_point.is_some() => {
                    super::sync_engine::mark_conflict_resolved(app, &job.id)
                }
                Ok(_) => Err(ProviderError::new(ErrorKind::Corrupt)),
                Err(error) => Err(error),
            };
            if settled.is_err() {
                crate::nlog!("error", "External job completed but conflict bookkeeping did not finish");
            }
        }
        settle_interrupted(&mut job, Some(result), false);
        let _ = JobStore::open(&root(app)?).and_then(|store| store.put(&job));
        return Ok(job);
    }
    let authoritative = pds.external_job(&job.id).map_err(local_error)?;
    if settle_unowned_publication(&mut pds, &mut job, authoritative.as_ref())? {
        let _ = JobStore::open(&root(app)?).and_then(|store| store.put(&job));
        return Ok(job);
    }
    {
        if let Some(intent) = authoritative.as_ref().filter(|item| matches!(item.phase.as_str(), "stale" | "cancelled")) {
            let preserving = super::sync_engine::conflict_record(app, &job.id)?
                .is_some_and(|record| !record.resolved && record.remote_point.is_some());
            settle_invalidated(&mut job, &intent.phase, preserving);
            if super::sync_engine::discard_receive_preparation(app, &job.id).is_ok() {
                job.receive_staging_id = None;
            }
            let _ = JobStore::open(&root(app)?).and_then(|store| store.put(&job));
            return Ok(job);
        }
        let prepared = if authoritative.as_ref().is_some_and(|item| item.role == "restore" && item.phase == "ready") {
            super::sync_engine::prepared_receive_result(app, &job.id)?
        } else { None };
        if settle_receive_preparation(&mut job, prepared) {
            let _ = JobStore::open(&root(app)?).and_then(|store| store.put(&job));
            return Ok(job);
        }
        if job.terminal() || job.summary["state"] != "running" {
            return Ok(job);
        }
    }
    settle_interrupted(&mut job, None, false);
    // The renderer still receives a settled result if its auxiliary cache is unwritable.
    let _ = JobStore::open(&root(app)?).and_then(|store| store.put(&job));
    Ok(job)
}

fn settle_unowned_publication(
    store: &mut PersistentStore,
    job: &mut DurableJob,
    authoritative: Option<&persistent_store::external_runtime::ExternalJob>,
) -> Result<bool> {
    let Some(intent) = authoritative.filter(|item| {
        item.id == job.id && item.connection_id == job.request.connection_id
            && matches!(item.phase.as_str(), "publishing" | "publicationUnknown")
    }) else {
        return Ok(false);
    };
    if intent.phase == "publishing" {
        store.external_publication_unknown(&job.id).map_err(local_error)?;
    }
    settle_interrupted(job, None, true);
    Ok(true)
}

fn settle_invalidated(job: &mut DurableJob, phase: &str, preserving: bool) {
    job.summary["state"] = json!(if preserving { "conflict" } else if phase == "cancelled" { "cancelled" } else { "failed" });
    job.summary["phase"] = json!(if preserving { "conflict-choice" } else { "paused" });
    job.summary.as_object_mut().unwrap().remove("result");
    if preserving {
        job.summary["result"] = json!({"conflictId":job.id});
    }
    let kind = if phase == "cancelled" { ErrorKind::Cancelled } else { ErrorKind::PreconditionFailed };
    job.summary["error"] = error_dto(&ProviderError::new(kind));
    job.summary["updatedAtMs"] = json!(now_ms().to_string());
}

fn settle_receive_preparation(job: &mut DurableJob, prepared: Option<Value>) -> bool {
    if let Some(result) = prepared {
        job.summary["state"] = json!("waiting");
        job.summary["phase"] = json!("remote-apply");
        job.summary["result"] = result;
        job.summary.as_object_mut().unwrap().remove("error");
    } else if job.summary["phase"] == "remote-apply" {
        job.summary["state"] = json!("waiting");
        job.summary["phase"] = json!("paused");
        job.summary.as_object_mut().unwrap().remove("result");
        job.summary["error"] = error_dto(&ProviderError::new(ErrorKind::Transient));
    } else {
        return false;
    }
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
    root: &'a std::path::Path,
    connected: &'a ConnectedRepository,
    writer_id: &'a str,
) -> leases::LeaseContext<'a> {
    leases::LeaseContext {
        root,
        connection_id: &connected.stored.id,
        writer_id,
        descriptor: &connected.stored.descriptor,
        root_key: &connected.root_key,
        provider: connected.provider.as_ref(),
        repository: &connected.handle,
        clock: leases::system_clock(),
        protection_supported: connected.stored.capabilities.lease_operations,
    }
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

pub(crate) async fn recheck_preserved_conflict(
    app: &AppHandle,
    id: &str,
    cancel: &Cancellation,
) -> Result<Value> {
    let root = root(app)?;
    let job = JobStore::open(&root)?.read(id)?;
    let record = super::sync_engine::conflict_record(app, id)?
        .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
    if job.id != id
        || record.id != id
        || !matches!(job.request.kind, JobKind::Sync | JobKind::ResolveConflict)
        || record.connection_id != job.request.connection_id
        || job.capture_id.as_deref() != Some(record.local.capture_id.as_str())
        || record.local.identity != job.admission_identity
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let connected =
        super::connection_commands::open_connected(app, &job.request.connection_id).await?;
    if connected.stored.id != record.connection_id
        || connected.stored.descriptor.repository_id != record.repository_id
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let writer_id = native_store(app)?
        .external_identity()
        .map_err(local_error)?
        .store_id;
    let context = lease_context(&root, &connected, &writer_id);
    match leases::admit(&context, id, LeaseKind::Work, cancel).await? {
        leases::Admission::Admitted(owner) => {
            let protection = RepositoryProtection {
                owner: &owner,
                context: &context,
            };
            owner
                .run(
                    &context,
                    cancel,
                    super::sync_engine::ensure_conflict_point(
                        app,
                        &connected,
                        &job,
                        record,
                        true,
                        Some(&protection),
                        cancel,
                    ),
                )
                .await
        }
        leases::Admission::Yield { .. } => {
            Err(ProviderError::new(ErrorKind::Transient))
        }
        leases::Admission::UnsupportedProtection => {
            Err(ProviderError::new(ErrorKind::Unsupported))
        }
    }
}

pub(crate) fn wake_job(app: AppHandle, id: String) -> Result<()> {
    let job = JobStore::open(&root(&app)?)?.read(&id)?;
    if job.terminal() {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    read_job_session(&app, &id)?;
    let (cancel, claim) = app.state::<JobCommandState>().claim(&job)?;
    tauri::async_runtime::spawn(async move {
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
        let confirmed = result.as_ref().ok().and_then(completed_revision);
        let cancelled = cancel.check().is_err();
        let persisted = (|| -> Result<()> {
            let store = JobStore::open(&root(&app)?)?;
            let mut job = store.read(&id)?;
            match result {
                Ok(result) => {
                    let receive = result.get("receiveReady").and_then(Value::as_bool) == Some(true);
                    // A removal that left a request whose end is unknown keeps
                    // its marker, so the job stays open until that is resolved.
                    let unresolved =
                        result.get("stopReason").and_then(Value::as_str) == Some("uncertain");
                    let publication_unknown = unresolved
                        && result.get("reason").and_then(Value::as_str)
                            == Some("publication-unknown");
                    job.summary["state"] = json!(if receive {
                        "waiting"
                    } else if result.get("conflictId").is_some() {
                        "conflict"
                    } else if unresolved {
                        "uncertain"
                    } else {
                        "succeeded"
                    });
                    job.summary["phase"] = json!(if receive {
                        "remote-apply"
                    } else if publication_unknown {
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
                    let mut pds = native_store(&app)?;
                    let authoritative = pds.external_job(&job.id).map_err(local_error)?;
                    let conflict = super::sync_engine::conflict_record(&app, &job.id)?;
                    let preserving = conflict
                        .as_ref()
                        .is_some_and(|record| !record.resolved && record.remote_point.is_none());
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
                        if super::sync_engine::discard_receive_preparation(&app, &job.id).is_ok() {
                            job.receive_staging_id = None;
                        } else {
                            crate::nlog!("error", "Rejected external receive stage could not be discarded");
                        }
                        if let Some(item) = authoritative.iter().find(|item| {
                            item.id == id
                                && ["preparing", "ready", "stale"].contains(&item.phase.as_str())
                        }) {
                            pds.external_cancel_prepared(&item.id).map_err(local_error)?;
                        }
                    }
                    let preserved_choice = job.request.kind == JobKind::ResolveConflict
                        && conflict.as_ref().is_some_and(|record| {
                            !record.resolved && record.remote_point.is_some()
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
                    if pending {
                        job.summary["error"]["reason"] = json!("publication-unknown");
                    }
                }
            }
            job.summary["updatedAtMs"] = json!(now_ms().to_string());
            store.put(&job)?;
            Ok(())
        })();
        let outcome_persisted = persisted.is_ok();
        if !outcome_persisted {
            crate::nlog!("error", "External job outcome could not be persisted");
        }
        drop(claim);
        if outcome_persisted {
            let cleaned = (|| -> Result<()> {
                let root = root(&app)?;
                let connection = ConnectionStore::open(&root)?.read(&job.request.connection_id)?;
                let directory = job_directory(&root, &job.request.connection_id, &job.id);
                let mut pds = native_store(&app)?;
                super::journal::TransferJournal::cleanup_terminal_spools_at(
                    &directory,
                    &job.id,
                    &mut pds,
                    &connection.descriptor.repository_id,
                )?;
                Ok(())
            })();
            if let Err(error) = cleaned {
                crate::nlog!("error", "External terminal spool cleanup failed: {error}");
            }
        }
        if let Some(revision) = confirmed.filter(|_| !cancelled) {
            if let Err(error) = start_queued_automatic(&app, &job, revision) {
                crate::nlog!("error", "External follow-up sync could not start: {error}");
            }
        }
    });
    Ok(())
}

fn completed_revision(result: &Value) -> Option<i64> {
    result.get("publishedRevision").or_else(|| result.get("receivedRevision"))
        .and_then(Value::as_str).and_then(|value| value.parse().ok())
}

fn continue_observed_automatic(app: &AppHandle, job: &DurableJob) {
    if job.summary["state"] == "succeeded" {
        if let Some(revision) = completed_revision(&job.summary["result"]) {
            if let Err(error) = start_queued_automatic(app, job, revision) {
                crate::nlog!("error", "External follow-up sync could not start: {error}");
            }
        }
    }
}

fn start_queued_automatic(app: &AppHandle, completed: &DurableJob, revision: i64) -> Result<()> {
    if completed.request.kind != JobKind::Sync
        || completed.request.reason.as_deref() != Some("automatic")
    {
        return Ok(());
    }
    let state = app.state::<JobCommandState>();
    if state.active.lock().map_err(local_error)?.values()
        .any(|(connection, _)| connection == &completed.request.connection_id)
    {
        return Ok(());
    }
    let target = state.automatic_targets.lock().map_err(local_error)?
        .get(&completed.request.connection_id).cloned();
    let Some(target) = target else { return Ok(()) };
    if target.owner_job_id != completed.id {
        return Ok(());
    }
    let target_revision = super::job_store::requested_revision(&target.request, target.identity.revision)?;
    if target_revision <= revision {
        forget_automatic_target(&state, &completed.request.connection_id, &completed.id, revision)?;
        return Ok(());
    }
    {
        let session = state.session.lock().map_err(local_error)?;
        require_session(&target.request, &session)?;
    }
    let pds = native_store(app)?;
    let selection = pds.external_selection().map_err(local_error)?;
    if selection.paused || selection.target != SyncTarget::External(target.request.connection_id.clone()) {
        return Ok(());
    }
    let current = pds.external_identity().map_err(local_error)?;
    let mut next = DurableJob::new(target.request, false, now_ms(), target.identity);
    require_admitted_library(&next, &current)?;
    next.admission_identity = current;
    let jobs = JobStore::open(&root(app)?)?;
    jobs.put(&next)?;
    wake_job(app.clone(), next.id)?;
    forget_automatic_target(&state, &completed.request.connection_id, &completed.id, target_revision)
}

fn forget_automatic_target(state: &JobCommandState, connection: &str, owner: &str, through: i64) -> Result<()> {
    let mut queued = state.automatic_targets.lock().map_err(local_error)?;
    if queued.get(connection).is_some_and(|target| {
        target.owner_job_id == owner && super::job_store::requested_revision(&target.request, target.identity.revision)
            .is_ok_and(|revision| revision <= through)
    }) {
        queued.remove(connection);
    }
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
        JobKind::Backup | JobKind::Sync | JobKind::ResolveConflict => {
            let root = root(app)?;
            let writer_id = native_store(app)?
                .external_identity()
                .map_err(local_error)?
                .store_id;
            let context = lease_context(&root, &connected, &writer_id);
            match leases::admit(&context, &job.id, LeaseKind::Work, cancel).await? {
                leases::Admission::Admitted(owner) => {
                    let protection = RepositoryProtection { owner: &owner, context: &context };
                    owner.run(&context, cancel, async {
                        match job.request.kind {
                            JobKind::Backup => {
                                run_backup(app, &connected, &job, Some(&protection), cancel).await
                            }
                            JobKind::Sync | JobKind::ResolveConflict => {
                                super::sync_engine::run_sync(
                                    app, &connected, &job, Some(&protection), cancel,
                                ).await
                            }
                            _ => unreachable!(),
                        }
                    }).await
                }
                leases::Admission::Yield { .. } => {
                    Err(ProviderError::new(ErrorKind::Transient))
                }
                leases::Admission::UnsupportedProtection if job.request.kind == JobKind::Backup => {
                    run_backup(app, &connected, &job, None, cancel).await
                }
                leases::Admission::UnsupportedProtection => {
                    Err(ProviderError::new(ErrorKind::Unsupported))
                }
            }
        }
        JobKind::Restore => {
            super::runtime_restore::run_restore(app, &connected, &job, cancel).await
        }
        JobKind::PinHistory => {
            super::history_jobs::run_pin_history(app, &connected, &job, cancel).await
        }
        JobKind::Cleanup => run_cleanup(app, &connected, &job, cancel).await,
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
    let root = root(app)?;
    let writer_id = native_store(app)?
        .external_identity()
        .map_err(local_error)?
        .store_id;
    let mut unfinished = JobStore::open(&root)?
        .list_pending()?
        .into_iter()
        .filter(|item| item.request.connection_id == job.request.connection_id && item.id != job.id)
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
    {
        let pds = native_store(app)?;
        let device = pds.device_store().map_err(local_error)?.connection();
        let mut cursor = None;
        loop {
            let page = external_conflicts::external_conflicts_page(device, cursor.as_ref(), 50)
                .map_err(local_error)?;
            for conflict in page.conflicts {
                if conflict.connection_id != job.request.connection_id {
                    continue;
                }
                if conflict.repository_id != connected.stored.descriptor.repository_id {
                    return Err(ProviderError::new(ErrorKind::Corrupt));
                }
                let mut references = vec![super::packaging::RemoteObject::from_stored(
                    &conflict.remote.snapshot,
                    &connected.handle,
                )?];
                if let Some(point) = conflict.remote_point.as_ref() {
                    references.push(super::packaging::RemoteObject::from_stored(
                        point,
                        &connected.handle,
                    )?);
                }
                if let Some(owner) = unfinished.iter_mut().find(|owner| owner.job_id == conflict.id)
                {
                    owner.references.extend(references);
                } else {
                    unfinished.push(super::cleanup::UnfinishedJob {
                        directory: job_directory(&root, &job.request.connection_id, &conflict.id),
                        snapshot_ids: std::collections::BTreeSet::new(),
                        job_id: conflict.id,
                        references,
                    });
                }
            }
            let Some(next) = page.next else {
                break;
            };
            cursor = Some(next);
        }
    }
    // One reading of the clock: the retention decision and the grace window
    // are measured against the same moment.
    let started = now_ms();
    let cleanup_directory = job_directory(&root, &job.request.connection_id, &job.id);
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
    };
    let documents = super::cleanup::ConnectedDocuments { connected, cancel, scratch: &scratch };
    let connection_time = |not_before| {
        Ok(connected.dependencies.requests
            .clock_sample_after(&connected.handle.account, not_before)?
            .is_some())
    };
    let outcome = super::cleanup::run(
        &lease_context(&root, connected, &writer_id),
        &super::cleanup::CleanupRequest {
            job_id: &job.id,
            cleanup_supported: connected.stored.capabilities.cleanup_supported(),
            limits: super::cleanup::CleanupLimits::default(),
            connection_time: &connection_time,
        },
        &view,
        &documents,
        cancel,
    )
    .await?;
    Ok(outcome.summary())
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
    let policy = connected.stored.capture_policy;
    let section_spool =
        job_directory(&root, &job.request.connection_id, &job.id).join("sections");
    let worker_spool = section_spool.clone();
    let (capture, fingerprint, sections) = tokio::task::spawn_blocking(move || -> Result<_> {
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
        // A backup keeps exactly what its connection selected. Capturing after
        // the library keeps one cancellation point for the whole job.
        let sections = match policy {
            Some(policy) => super::sections::capture_backup_sections(
                &mut pds,
                policy,
                &worker_spool,
                &probe.0,
            )?,
            None => Vec::new(),
        };
        Ok((capture, fingerprint, sections))
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
        sections,
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
    match super::packaging::verify_publication(
        &completed,
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
    let summaries = super::quota_profiles::connection_usage(
        &budget,
        &connected.stored.config,
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
        incoming.restore_areas = Some(vec!["hypa".into()]);
        assert!(!same_requested_operation(&existing, &incoming));
        let existing: StartJobRequest = serde_json::from_value(json!({"connectionId":"x", "kind":"resolve-conflict", "conflictId":"conflict", "choice":"remote"})).unwrap();
        let mut incoming = existing.clone();
        incoming.choice = Some("local".into());
        assert!(!same_requested_operation(&existing, &incoming));
    }
    fn automatic_job() -> DurableJob {
        let request = serde_json::from_value(json!({
            "connectionId":"x", "kind":"sync", "reason":"automatic", "targetRevision":"1"
        })).unwrap();
        DurableJob::new(request, false, 1, persistent_store::sync_selection::CaptureIdentity {
            store_id: "store".into(), library_epoch: "library".into(), generation: "generation".into(),
            selection_epoch: "selection".into(), revision: 1,
        })
    }
    #[test]
    fn detached_unknown_is_rechecked_only_by_the_same_publication_operation() {
        let mut unknown = automatic_job();
        unknown.summary["state"] = json!("uncertain");
        unknown.summary["phase"] = json!("publication-unknown");
        let same = unknown.request.clone();
        assert!(pending_matches_request(&unknown, &same));

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

    fn receive_job_fixture() -> (tempfile::TempDir, PersistentStore, DurableJob) {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let epoch = store.external_selection().unwrap().epoch;
        store.external_select(&epoch, &SyncTarget::External("x".into())).unwrap();
        let mut job = automatic_job();
        job.admission_identity = store.external_identity().unwrap();
        store.external_prepare_receive(&persistent_store::external_storage_state::ReceiveIntent {
            job_id: &job.id, connection_id: "x", repository_id: "synthetic-repository",
            snapshot_id: &job.snapshot_id, commit_id: "synthetic-commit",
            authenticated_head: "authenticated-head", identity: &job.admission_identity,
        }).unwrap();
        (directory, store, job)
    }

    #[test]
    fn committed_receive_result_is_idempotent() {
        let (_directory, mut store, mut job) = receive_job_fixture();
        let stage = store.replace_begin().unwrap();
        store.replace_put_root(&stage.staging_id, &json!({"marker":"synthetic-remote"})).unwrap();
        let prepared = store.prepare_replace_commit(&stage.staging_id, Some(0)).unwrap();
        store.finish_external_receive(prepared, &job.id).unwrap();
        job.request.kind = JobKind::ResolveConflict;
        for _ in 0..2 {
            let result = completed_job_result(&mut store, &job).unwrap().unwrap();
            assert_eq!(result["receivedRevision"], "1");
            assert_eq!(result["snapshotId"], job.snapshot_id);
        }
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(None).unwrap()["marker"], "synthetic-remote");
    }

    #[test]
    fn stopped_publication_is_unknown_regardless_of_cached_summary() {
        let (directory, mut store, original) = receive_job_fixture();
        let injector = rusqlite::Connection::open(
            directory.path().join("persistent/persistent.sqlite"),
        )
        .unwrap();
        for cached in ["queued", "running", "waiting", "succeeded", "failed", "cancelled"] {
            injector.execute(
                "UPDATE external_storage_jobs SET role='sync',strategy='cas',phase='publishing' WHERE id=?1",
                [&original.id],
            ).unwrap();
            let mut job = original.clone();
            job.summary["state"] = json!(cached);
            job.summary["result"] = json!({"publishedRevision":"1"});
            let intent = store.external_job(&job.id).unwrap().unwrap();
            assert!(settle_unowned_publication(&mut store, &mut job, Some(&intent)).unwrap());
            assert_eq!(job.summary["state"], "uncertain", "{cached}");
            assert_eq!(job.summary["phase"], "publication-unknown", "{cached}");
            assert!(job.summary.get("result").is_none(), "{cached}");
            let retained = store.external_job(&job.id).unwrap().unwrap();
            assert_eq!(retained.phase, "publicationUnknown");
            assert_eq!(retained.commit_id, intent.commit_id);
            assert_eq!(retained.identity, intent.identity);
            assert!(settle_unowned_publication(&mut store, &mut job, Some(&retained)).unwrap());
        }
        assert_eq!(store.revision().unwrap(), 0);
    }

    #[test]
    fn a_cached_ready_summary_requires_a_live_prepared_handle_after_restart() {
        let mut job = automatic_job();
        job.summary["state"] = json!("running");
        assert!(settle_receive_preparation(&mut job, Some(json!({
            "receiveReady":true, "snapshotId":"snapshot", "expectedRevision":"1"
        }))));
        assert_eq!(job.summary["phase"], "remote-apply");
        assert_eq!(job.summary["state"], "waiting");
        assert!(settle_receive_preparation(&mut job, None));
        assert_eq!(job.summary["phase"], "paused");
        assert!(job.summary.get("result").is_none());
        assert_eq!(job.summary["error"]["action"], "retry");
        assert_eq!(job.admission_identity.revision, 1);
    }

    #[test]
    fn invalidated_waiting_jobs_settle_without_losing_preserved_conflicts() {
        for cached in ["queued", "waiting", "running", "failed"] {
            let mut job = automatic_job();
            job.summary["state"] = json!(cached);
            settle_invalidated(&mut job, "stale", false);
            assert!(job.terminal());
            assert_eq!(job.summary["state"], "failed");
            settle_invalidated(&mut job, "cancelled", true);
            assert!(!job.terminal());
            assert_eq!(job.summary["state"], "conflict");
            assert_eq!(job.summary["result"]["conflictId"], job.id);
        }
    }

    #[test]
    fn older_completion_or_cancellation_cannot_discard_a_newer_jobs_automatic_target() {
        let state = JobCommandState::default();
        let old = automatic_job();
        let new = automatic_job();
        let mut current = new.admission_identity.clone();
        current.revision = 3;
        let mut request = new.request.clone();
        request.target_revision = Some("3".into());
        state.coalesce_automatic(&new, &request, &current).unwrap();
        state.cancel_automatic_target(&old).unwrap();
        forget_automatic_target(&state, "x", &old.id, 100).unwrap();
        assert_eq!(state.automatic_targets.lock().unwrap().len(), 1);
        forget_automatic_target(&state, "x", &new.id, 2).unwrap();
        assert_eq!(state.automatic_targets.lock().unwrap().len(), 1);
        forget_automatic_target(&state, "x", &new.id, 3).unwrap();
        assert!(state.automatic_targets.lock().unwrap().is_empty());
    }

    #[test]
    fn automatic_sync_does_not_absorb_a_manual_sync_request() {
        let automatic = automatic_job().request;
        let mut manual = automatic.clone();
        manual.reason = Some("manual".into());
        assert!(!same_requested_operation(&automatic, &manual));
        assert!(!same_requested_operation(&manual, &automatic));
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
        job.summary["state"] = json!("failed");
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
            PublicationMode::Foreground
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
