//! Manual external snapshot restore and durable device-maintenance continuation.
//! Remote reads finish before either the library writer fence or PDS is opened.
use super::{
    connection_commands::ConnectedRepository,
    contract::{Cancellation, ErrorKind, ProviderError, Result},
    job_store::{DurableJob, JobClaim, JobCommandState, JobKind, JobStore},
    runtime,
    snapshot_restore::{self, PreparedRemoteSnapshot},
};
use crate::{
    device_backup::{DeviceBackupError, DeviceBackupState, Operation, Session as DeviceSession},
    persistent_store::{
        commands::PersistentStoreState,
        external_apply::{
            ExternalSnapshotApplication, ExternalSnapshotObject, ExternalSnapshotRecord,
        },
        PersistentStore, StoreError,
    },
};
use risunest_external_storage_format::section::SectionKind;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeSet, path::Path, sync::Mutex, time::Duration};
use tauri::{AppHandle, Manager};

#[derive(Clone, Debug, PartialEq, Eq)]
struct RestoreSelection {
    library: bool,
    sections: BTreeSet<String>,
}

const RESTORE_COMMIT_SCHEMA: &str = "risunest.external-restore-commit/v1";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreCommitMarker {
    schema: String,
    job_id: String,
    connection_id: String,
    snapshot_id: String,
    expected_revision: String,
    received_revision: String,
}

fn restore_marker_key(job: &str) -> String {
    format!("external-restore-commit:{job}")
}

fn restore_marker(job: &DurableJob, expected_revision: i64) -> Result<RestoreCommitMarker> {
    let received_revision = expected_revision
        .checked_add(1)
        .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
    Ok(RestoreCommitMarker {
        schema: RESTORE_COMMIT_SCHEMA.into(),
        job_id: job.id.clone(),
        connection_id: job.request.connection_id.clone(),
        snapshot_id: job.request.snapshot_id.clone().ok_or_else(corrupt)?,
        expected_revision: expected_revision.to_string(),
        received_revision: received_revision.to_string(),
    })
}

fn completed_restore_in_store(store: &PersistentStore, job: &DurableJob) -> Result<Option<Value>> {
    if job.request.kind != JobKind::Restore {
        return Ok(None);
    }
    let expected_revision = job
        .request
        .target_revision
        .as_deref()
        .ok_or_else(corrupt)?
        .parse::<i64>()
        .map_err(|_| corrupt())?;
    let Some(value) = store
        .get_app_kv(&restore_marker_key(&job.id))
        .map_err(pds_error)?
    else {
        return Ok(None);
    };
    let marker: RestoreCommitMarker = serde_json::from_value(value).map_err(|_| corrupt())?;
    let expected = restore_marker(job, expected_revision)?;
    if marker != expected
        || store.revision().map_err(pds_error)?
            < marker
                .received_revision
                .parse::<i64>()
                .map_err(|_| corrupt())?
    {
        return Err(corrupt());
    }
    Ok(Some(json!({
        "snapshotId":marker.snapshot_id,
        "receivedRevision":marker.received_revision
    })))
}

/// Read-only completion recovery used before reopening a provider connection.
pub(crate) fn completed_restore(app: &AppHandle, job: &DurableJob) -> Result<Option<Value>> {
    completed_restore_in_store(&runtime::native_store(app)?, job)
}

struct RetainedRestore {
    job_id: String,
    store: PersistentStore,
    claim: Option<JobClaim>,
}

#[derive(Default)]
pub(crate) struct RuntimeRestoreState(Mutex<Option<RetainedRestore>>);

impl RuntimeRestoreState {
    fn retain(&self, job: &str, store: PersistentStore) -> Result<()> {
        let mut slot = self.0.lock().map_err(runtime::local_error)?;
        if slot.is_some() {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        *slot = Some(RetainedRestore { job_id: job.to_owned(), store, claim: None });
        Ok(())
    }

    fn with_store<T>(
        &self,
        job: &str,
        operation: impl FnOnce(&mut PersistentStore) -> Result<T>,
    ) -> Result<T> {
        let mut slot = self.0.lock().map_err(runtime::local_error)?;
        let retained = slot.as_mut().ok_or_else(corrupt)?;
        if retained.job_id != job {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        operation(&mut retained.store)
    }

    fn retain_claim(&self, job: &str, claim: JobClaim) -> Result<()> {
        let mut slot = self.0.lock().map_err(runtime::local_error)?;
        let retained = slot.as_mut().ok_or_else(corrupt)?;
        if retained.job_id != job || retained.claim.is_some() {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        retained.claim = Some(claim);
        Ok(())
    }

    fn release_claim(&self, job: &str) {
        if let Ok(mut slot) = self.0.lock() {
            if let Some(retained) = slot.as_mut().filter(|retained| retained.job_id == job) {
                retained.claim.take();
            }
        }
    }

    fn release(&self, job: &str) {
        if let Ok(mut slot) = self.0.lock() {
            if slot.as_ref().is_some_and(|retained| retained.job_id == job) {
                slot.take();
            }
        }
    }
}

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

fn pds_error(error: StoreError) -> ProviderError {
    match error {
        StoreError::RevisionConflict { .. } => ProviderError::new(ErrorKind::PreconditionFailed),
        StoreError::Validation { .. } => corrupt(),
        _ => ProviderError::new(ErrorKind::Transient),
    }
}

fn device_error(error: DeviceBackupError) -> ProviderError {
    if error.code.contains("cancel") {
        ProviderError::new(ErrorKind::Cancelled)
    } else if error.code.contains("invalid") || error.code.contains("missing") {
        corrupt()
    } else {
        ProviderError::new(ErrorKind::Transient)
    }
}

fn device_command_error() -> DeviceBackupError {
    DeviceBackupError {
        code: "external-restore-invalid".into(),
        message: "The external restore maintenance journal is invalid".into(),
    }
}

fn existing_external_jobs(root: &Path) -> crate::device_backup::Result<Option<JobStore>> {
    let path = root.join("external-jobs.sqlite");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !crate::trust_boundary::is_link_like(&metadata) => {
            crate::trust_boundary::open_regular_source(&path)
                .map_err(|_| device_command_error())?;
            JobStore::open(root)
                .map(Some)
                .map_err(|_| device_command_error())
        }
        Ok(_) => Err(device_command_error()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(device_command_error()),
    }
}

fn decode_hash(value: &str) -> Result<[u8; 32]> {
    if !crate::trust_boundary::is_lower_hex_256(value) {
        return Err(corrupt());
    }
    hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(corrupt)
}

/// A bundle declares what it covers, and a restore asks for a subset of that.
/// An area the request does not name is left alone on this device, and an
/// unknown one is rejected rather than quietly narrowed.
fn restore_selection(restore_areas: Option<&[String]>) -> Result<RestoreSelection> {
    let defaults;
    let areas = match restore_areas {
        Some(areas) => areas,
        None => {
            defaults = vec!["library".to_owned(), "referencedAssets".to_owned()];
            &defaults
        }
    };
    let unique = areas.iter().collect::<BTreeSet<_>>();
    if unique.len() != areas.len() {
        return Err(corrupt());
    }
    let library_areas = ["library", "referencedAssets"];
    let mut sections = BTreeSet::new();
    for area in areas {
        if library_areas.contains(&area.as_str()) {
            continue;
        }
        sections.insert(SectionKind::parse(area).map_err(|_| corrupt())?.id().to_owned());
    }
    let library = areas
        .iter()
        .any(|area| library_areas.contains(&area.as_str()));
    if !library {
        return Err(corrupt());
    }
    Ok(RestoreSelection { library, sections })
}

/// Local settings belong to the device that wrote them, so only that device's
/// own backup may bring them back. Published states and another device's
/// bundles are refused here rather than at the screen, because the request
/// names the areas and the screen is not the only caller.
fn require_restorable_sections(
    selection: &RestoreSelection,
    snapshot: &PreparedRemoteSnapshot,
    store_id: &str,
) -> Result<()> {
    if !selection.sections.contains(SectionKind::LocalSettings.id()) {
        return Ok(());
    }
    if store_id.is_empty() || snapshot.captured_by_device.as_deref() != Some(store_id) {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(())
}

fn checked_required_bytes(
    snapshot: &PreparedRemoteSnapshot,
    selection: &RestoreSelection,
) -> Result<u64> {
    let mut total = 0u64;
    let mut add = |length: u64| -> Result<()> {
        total = total.checked_add(length).ok_or_else(corrupt)?;
        Ok(())
    };
    if selection.library {
        for record in &snapshot.records {
            add(record.byte_length)?;
        }
        for object in &snapshot.objects {
            add(object.byte_length)?;
        }
    }
    Ok(total)
}

#[cfg(windows)]
fn available_space(path: &Path) -> std::io::Result<u64> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let mut available = 0u64;
    let result = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(available)
    }
}

#[cfg(unix)]
fn available_space(path: &Path) -> std::io::Result<u64> {
    use std::{ffi::CString, mem::MaybeUninit, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid path"))?;
    let mut stats = MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stats = unsafe { stats.assume_init() };
    Ok((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

fn validate_download(
    connected: &ConnectedRepository,
    requested_snapshot: &str,
    snapshot: &PreparedRemoteSnapshot,
) -> Result<()> {
    if snapshot.snapshot_id != requested_snapshot
        || snapshot.repository_id != connected.stored.descriptor.repository_id
    {
        return Err(corrupt());
    }
    decode_hash(&snapshot.fingerprint)?;
    decode_hash(&snapshot.library_fingerprint)?;
    Ok(())
}

fn update_phase(root: &Path, job: &DurableJob, phase: &str) -> Result<()> {
    let store = JobStore::open(root)?;
    let mut current = store.read(&job.id)?;
    if current.request.connection_id != job.request.connection_id || current.terminal() {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    current.summary["state"] = json!("running");
    current.summary["phase"] = json!(phase);
    current.summary["updatedAtMs"] = json!(runtime::now_ms().to_string());
    store.put(&current)
}

pub(crate) async fn run_restore(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    cancel: &Cancellation,
) -> Result<Value> {
    if job.request.kind != JobKind::Restore {
        return Err(corrupt());
    }
    if let Some(result) = completed_restore(app, job)? {
        return Ok(result);
    }
    let snapshot_id = job.request.snapshot_id.as_deref().ok_or_else(corrupt)?;
    let expected_revision = job
        .request
        .target_revision
        .as_deref()
        .ok_or_else(corrupt)?
        .parse::<i64>()
        .map_err(|_| corrupt())?;
    let root = runtime::root(app)?;
    update_phase(&root, job, "downloading")?;
    let remote = super::control::find_snapshot(connected, snapshot_id, cancel).await?;
    let staging_root =
        runtime::job_directory(&root, &job.request.connection_id, &job.id).join("restore-snapshot");
    let snapshot = snapshot_restore::download_snapshot(
        &remote,
        &staging_root,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
    cancel.check()?;
    validate_download(connected, snapshot_id, &snapshot)?;
    let selection = restore_selection(job.request.restore_areas.as_deref())?;
    require_restorable_sections(&selection, &snapshot, &job.admission_identity.store_id)?;
    let sections = snapshot_restore::download_sections(
        &remote,
        &selection.sections,
        &staging_root,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
    if sections.len() != selection.sections.len() {
        return Err(ProviderError::new(ErrorKind::NotFound));
    }
    let required = checked_required_bytes(&snapshot, &selection)?;
    if available_space(&staging_root).map_err(runtime::local_error)? < required {
        return Err(ProviderError::new(ErrorKind::StorageFull));
    }
    update_phase(&root, job, "preparing-local")?;

    let worker_app = app.clone();
    let worker_job = job.clone();
    let worker_cancel = cancel.clone();
    let result = tokio::task::spawn_blocking(move || {
        prepare_local_restore(
            &worker_app,
            &worker_job,
            expected_revision,
            snapshot,
            selection,
            sections,
            worker_cancel,
        )
    })
    .await
    .map_err(runtime::local_error)??;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn prepare_local_restore(
    app: &AppHandle,
    job: &DurableJob,
    expected_revision: i64,
    snapshot: PreparedRemoteSnapshot,
    selection: RestoreSelection,
    sections: Vec<snapshot_restore::PreparedSection>,
    cancel: Cancellation,
) -> Result<Value> {
    cancel.check()?;
    let admission = app
        .state::<crate::native_file_jobs::NativeFileJobState>()
        .admission
        .clone();
    let permit = admission.file(true).map_err(runtime::local_error)?;
    // Open a dedicated native connection while renderer admission is still
    // available. It remains owned across the maintenance WebView reload.
    let mut store = runtime::native_store(app)?;
    let maintenance = app
        .state::<PersistentStoreState>()
        .acquire_device_maintenance()
        .map_err(pds_error)?;
    cancel.check()?;
    if store.revision().map_err(pds_error)? != expected_revision {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let snapshot_id = snapshot.snapshot_id.clone();
    let staging_root = snapshot.staging_root.clone();
    let prepared = if selection.library {
        let scope_id = risunest_external_storage_format::format::library_fingerprint_domain();
        let fingerprint = decode_hash(&snapshot.library_fingerprint)?;
        let records = snapshot.records.into_iter().map(|record| {
            Ok(ExternalSnapshotRecord {
                key: record.key,
                content_hash: record.content_hash,
                byte_length: record.byte_length,
                path: record.path,
            })
        });
        let objects = snapshot.objects.into_iter().map(|object| {
            Ok(ExternalSnapshotObject {
                content_hash: object.content_hash,
                byte_length: object.byte_length,
                path: object.path,
            })
        });
        Some(
            store
                .prepare_external_snapshot_application(
                    &ExternalSnapshotApplication {
                        expected_revision,
                        staging_root: &staging_root,
                        scope_id: &scope_id,
                        fingerprint: &fingerprint,
                    },
                    records,
                    objects,
                )
                .map_err(pds_error)?,
        )
    } else {
        None
    };

    let prepared = prepared.ok_or_else(corrupt)?;
    // Decoding every selected section before touching the device file keeps a
    // bundle with a missing object from installing half of itself.
    let decoded = sections
        .into_iter()
        .map(|section| {
            let rows = super::sections::decode_section(
                section.kind,
                &section.entries,
                &section.content_fingerprint,
            )?;
            Ok((section.kind, rows))
        })
        .collect::<Result<Vec<_>>>()?;
    for (kind, rows) in decoded {
        cancel.check()?;
        let device = store.device_store_mut().map_err(pds_error)?;
        match super::sections::section_of(kind) {
            Some(section) => device.restore_section_rows(section, &rows).map_err(pds_error)?,
            None => device.restore_local_setting_rows(&rows).map_err(pds_error)?,
        }
    }
    let marker = restore_marker(job, expected_revision)?;
    let revision = store
        .finish_prepared_replace_with_app_kv(
            prepared,
            &restore_marker_key(&job.id),
            &serde_json::to_value(&marker).map_err(runtime::local_error)?,
        )
        .map_err(pds_error)?;
    if revision.revision.to_string() != marker.received_revision {
        return Err(corrupt());
    }
    drop(maintenance);
    drop(permit);
    cleanup_staging(&staging_root);
    Ok(json!({
        "snapshotId": snapshot_id,
        "receivedRevision": revision.revision.to_string()
    }))
}

fn cleanup_staging(path: &Path) {
    let safe = std::fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| {
            metadata.is_dir() && !crate::trust_boundary::is_link_like(&metadata)
        });
    if safe {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// Called after renderer rollback preparation. Portable restore sessions are a no-op.
pub(crate) fn resume_prepared_device_restore(
    app: AppHandle,
    session_id: &str,
) -> crate::device_backup::Result<()> {
    let state = app.state::<DeviceBackupState>();
    let session = state.session(session_id)?;
    if session.operation != Operation::Restore || session.phase != "awaiting-native-preparation" {
        return Err(device_command_error());
    }
    let root = state.repository_root();
    let Some(jobs) = existing_external_jobs(root)? else {
        return Ok(());
    };
    let job = match jobs.read(&session.job_id) {
        Ok(job) => job,
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(device_command_error()),
    };
    if job.request.kind != JobKind::Restore {
        return Err(device_command_error());
    }
    let task_app = app.clone();
    let task_session = session_id.to_owned();
    tauri::async_runtime::spawn(async move {
        resume_task(task_app, job, task_session).await;
    });
    Ok(())
}

async fn claim_continuation(app: &AppHandle, job: &DurableJob) -> Result<(Cancellation, JobClaim)> {
    runtime::wait_for_job_release(app, &job.id).await?;
    app.state::<JobCommandState>().claim(job)
}

async fn resume_task(app: AppHandle, original: DurableJob, session_id: String) {
    let claimed = claim_continuation(&app, &original).await;
    let mut worker_claim = None;
    let mut retained = false;
    let result = match claimed {
        Ok((cancel, claim)) => {
            let keep = app.state::<RuntimeRestoreState>().retain_claim(&original.id, claim.clone());
            worker_claim = Some(claim);
            match keep {
                Ok(()) => {
                    retained = true;
                    continue_device_restore(&app, &original, &session_id, &cancel).await
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    };
    let committed = result.is_ok();
    if !committed {
        let state = app.state::<DeviceBackupState>();
        if let Ok(session) = state.session(&session_id) {
            if matches!(
                session.phase.as_str(),
                "awaiting-native-preparation" | "prepared" | "applying-device"
            ) {
                let _ = state.fail(&session_id, "external-restore-native-failed");
            }
        }
    }
    if let Err(error) = persist_device_outcome(&app, &original, &session_id, result) {
        crate::nlog!(
            "error",
            "External restore continuation outcome could not be persisted: {error}"
        );
    }
    if !committed {
        if retained {
            app.state::<RuntimeRestoreState>().release_claim(&original.id);
        }
        drop(worker_claim);
        // The outcome above may have failed the job, which the caller's copy
        // does not show.
        if let Ok(settled) = JobStore::open(app.state::<DeviceBackupState>().repository_root())
            .and_then(|store| store.read(&original.id))
        {
            runtime::release_leases(&app, &settled);
        }
    }
}

fn validate_waiting_session(job: &DurableJob, session: &DeviceSession) -> Result<i64> {
    if job.request.kind != JobKind::Restore
        || job.summary["state"] != "waiting"
        || job.summary["phase"] != "device-restore-maintenance"
        || job.summary["result"]["maintenanceSessionId"].as_str()
            != Some(session.session_id.as_str())
        || session.job_id != job.id
        || session.operation != Operation::Restore
    {
        return Err(corrupt());
    }
    job.request
        .target_revision
        .as_deref()
        .ok_or_else(corrupt)?
        .parse::<i64>()
        .map_err(|_| corrupt())
}

async fn continue_device_restore(
    app: &AppHandle,
    original: &DurableJob,
    session_id: &str,
    cancel: &Cancellation,
) -> Result<Value> {
    let state = app.state::<DeviceBackupState>();
    let root = state.repository_root();
    let job = JobStore::open(root)?.read(&original.id)?;
    let session = state.session(session_id).map_err(device_error)?;
    let expected_revision = validate_waiting_session(&job, &session)?;
    if session.phase != "awaiting-native-preparation" {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    if session.includes_library {
        let stage = session.new_generation.as_deref().ok_or_else(corrupt)?;
        app.state::<RuntimeRestoreState>()
            .with_store(&job.id, |store| {
                store
                    .prepare_replace_commit(stage, Some(expected_revision))
                    .map_err(pds_error)
            })?;
    } else if session.new_generation.is_some() {
        return Err(corrupt());
    }
    state.allow_device_apply(session_id).map_err(device_error)?;

    loop {
        let session = state.session(session_id).map_err(device_error)?;
        match session.phase.as_str() {
            "prepared" | "applying-device" => {
                if cancel.check().is_err() {
                    let _ = state.fail(session_id, "external-restore-cancelled");
                    return Err(ProviderError::new(ErrorKind::Cancelled));
                }
            }
            "committing-library" => {
                let stage = session.new_generation.as_deref().ok_or_else(corrupt)?;
                let (key, marker) = state.commit_marker(session_id).map_err(device_error)?;
                let marker = serde_json::to_value(marker).map_err(runtime::local_error)?;
                let revision =
                    match app
                        .state::<RuntimeRestoreState>()
                        .with_store(&job.id, |store| {
                            let prepared = store
                                .prepare_replace_commit(stage, Some(expected_revision))
                                .map_err(pds_error)?;
                            store
                                .finish_prepared_replace_with_app_kv(prepared, &key, &marker)
                                .map_err(pds_error)
                        }) {
                        Ok(revision) => revision,
                        Err(error) => {
                            let _ = state.library_commit_failed(session_id);
                            return Err(error);
                        }
                    };
                if let Err(error) = state.mark_library_committed(session_id) {
                    crate::nlog!(
                        "error",
                        "External restore library marker committed before journal update: {error}"
                    );
                }
                return Ok(json!({
                    "snapshotId": job.request.snapshot_id.as_deref().ok_or_else(corrupt)?,
                    "receivedRevision": revision.revision.to_string()
                }));
            }
            "committed" => {
                let revision = app
                    .state::<RuntimeRestoreState>()
                    .with_store(&job.id, |store| store.revision().map_err(pds_error))?;
                return Ok(json!({
                    "snapshotId": job.request.snapshot_id.as_deref().ok_or_else(corrupt)?,
                    "receivedRevision": revision.to_string()
                }));
            }
            "rolling-back" | "rolled-back" | "recovery-required" => {
                return Err(ProviderError::new(ErrorKind::Transient));
            }
            _ => return Err(corrupt()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn persist_device_outcome(
    app: &AppHandle,
    original: &DurableJob,
    session_id: &str,
    result: Result<Value>,
) -> Result<()> {
    let state = app.state::<DeviceBackupState>();
    let store = JobStore::open(state.repository_root())?;
    let mut job = store.read(&original.id)?;
    if job.terminal() {
        return Ok(());
    }
    if job.summary["result"]["maintenanceSessionId"].as_str() != Some(session_id) {
        return Err(corrupt());
    }
    match result {
        Ok(result) => {
            job.summary["state"] = json!("waiting");
            job.summary["phase"] = json!("device-restore-maintenance");
            job.summary["maintenanceOutcome"] = json!({
                "state":"succeeded",
                "result":result
            });
            job.summary.as_object_mut().unwrap().remove("error");
        }
        Err(error) => {
            job.summary["state"] = json!("failed");
            job.summary["phase"] = json!("paused");
            job.summary["error"] = runtime::error_dto(&error);
        }
    }
    job.summary["updatedAtMs"] = json!(runtime::now_ms().to_string());
    store.put(&job)
}

fn finalize_maintenance_outcome(store: &JobStore, job: &mut DurableJob) -> Result<()> {
    let outcome = job
        .summary
        .get("maintenanceOutcome")
        .cloned()
        .ok_or_else(corrupt)?;
    match outcome["state"].as_str() {
        Some("succeeded") => {
            let result = outcome.get("result").cloned().ok_or_else(corrupt)?;
            if result["snapshotId"].as_str() != job.request.snapshot_id.as_deref()
                || result["receivedRevision"]
                    .as_str()
                    .and_then(|value| value.parse::<i64>().ok())
                    .is_none()
            {
                return Err(corrupt());
            }
            job.summary["state"] = json!("succeeded");
            job.summary["phase"] = json!("complete");
            job.summary["result"] = result;
            job.summary.as_object_mut().unwrap().remove("error");
        }
        Some("failed") => {
            let error = outcome.get("error").cloned().ok_or_else(corrupt)?;
            job.summary["state"] = json!("failed");
            job.summary["phase"] = json!("paused");
            job.summary["error"] = error;
        }
        _ => return Err(corrupt()),
    }
    job.summary
        .as_object_mut()
        .unwrap()
        .remove("maintenanceOutcome");
    job.summary["updatedAtMs"] = json!(runtime::now_ms().to_string());
    store.put(job)
}

/// Durably records the native outcome before the device journal releases its
/// writer fences. A later state read can finish this cross-database settlement.
pub(crate) fn prepare_device_restore_settlement(
    app: AppHandle,
    session: &DeviceSession,
) -> crate::device_backup::Result<()> {
    if session.operation != Operation::Restore {
        return Ok(());
    }
    let state = app.state::<DeviceBackupState>();
    let root = state.repository_root();
    let Some(store) = existing_external_jobs(root)? else {
        return Ok(());
    };
    let mut job = match store.read(&session.job_id) {
        Ok(job) => job,
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(device_command_error()),
    };
    if job.request.kind != JobKind::Restore {
        return Err(device_command_error());
    }
    if job.terminal() {
        return Ok(());
    }
    if job.summary["result"]["maintenanceSessionId"].as_str() != Some(session.session_id.as_str()) {
        return Err(device_command_error());
    }
    let outcome = match session.phase.as_str() {
        "committed" => {
            let revision = PersistentStore::external_revision_at_root(root)
                .map_err(|_| device_command_error())?;
            json!({
                "state":"succeeded",
                "result":{
                    "snapshotId":job.request.snapshot_id.as_deref().ok_or_else(device_command_error)?,
                    "receivedRevision":revision.to_string()
                }
            })
        }
        "rolling-back" | "rolled-back" => json!({
            "state":"failed",
            "error":runtime::error_dto(&ProviderError::new(ErrorKind::Transient))
        }),
        _ => return Err(device_command_error()),
    };
    if job
        .summary
        .get("maintenanceOutcome")
        .is_some_and(|existing| existing != &outcome)
    {
        return Err(device_command_error());
    }
    job.summary["state"] = json!("waiting");
    job.summary["phase"] = json!("device-restore-settling");
    job.summary["maintenanceOutcome"] = outcome;
    job.summary["updatedAtMs"] = json!(runtime::now_ms().to_string());
    store.put(&job).map_err(|_| device_command_error())
}

/// Called after the device journal releases its fences. It closes outcomes that
/// survived a process interruption after commit or while rolling back.
pub(crate) fn settle_device_restore(
    app: AppHandle,
    session: &DeviceSession,
) -> crate::device_backup::Result<()> {
    if session.operation != Operation::Restore {
        return Ok(());
    }
    let device_state = app.state::<DeviceBackupState>();
    let root = device_state.repository_root().to_path_buf();
    let Some(store) = existing_external_jobs(device_state.repository_root())? else {
        return Ok(());
    };
    let job = match store.read(&session.job_id) {
        Ok(job) => job,
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(device_command_error()),
    };
    if job.request.kind != JobKind::Restore {
        return Err(device_command_error());
    }
    let job = if job.terminal() {
        job
    } else {
        let mut settled = job;
        finalize_maintenance_outcome(&store, &mut settled).map_err(|_| device_command_error())?;
        settled
    };
    if session.phase != "committed" {
        if let Some(stage) = session.new_generation.as_deref() {
            let cleanup = app
                .state::<RuntimeRestoreState>()
                .with_store(&job.id, |store| {
                    store.replace_abort(stage).map_err(pds_error)
                })
                .or_else(|_| {
                    runtime::native_store(&app)
                        .and_then(|mut store| store.replace_abort(stage).map_err(pds_error))
                });
            if let Err(error) = cleanup {
                crate::nlog!(
                    "error",
                    "Rolled-back external restore staging cleanup failed: {error}"
                );
            }
        }
    }
    app.state::<RuntimeRestoreState>().release(&job.id);
    runtime::release_leases(&app, &job);
    let staging =
        runtime::job_directory(&root, &job.request.connection_id, &job.id).join("restore-snapshot");
    cleanup_staging(&staging);
    Ok(())
}

/// Completes a settlement left after a process loss between journal release and
/// external job update. Active maintenance keeps the candidate pending.
pub(crate) fn reconcile_device_restore_settlements(app: &AppHandle) -> Result<()> {
    let device = app.state::<DeviceBackupState>();
    if device.is_blocking().map_err(device_error)? {
        return Ok(());
    }
    let root = runtime::root(app)?;
    let jobs = JobStore::open(&root)?;
    let store = runtime::native_store(app)?;
    for mut job in jobs.list_pending()? {
        if !job.terminal()
            && job.request.kind == JobKind::Restore
            && job.summary["result"]["maintenanceSessionId"].is_null()
        {
            if let Some(result) = completed_restore_in_store(&store, &job)? {
                job.summary["state"] = json!("succeeded");
                job.summary["phase"] = json!("complete");
                job.summary["result"] = result;
                job.summary.as_object_mut().unwrap().remove("error");
                job.summary["updatedAtMs"] = json!(runtime::now_ms().to_string());
                jobs.put(&job)?;
            }
        }
    }
    let Some(store) = existing_external_jobs(device.repository_root()).map_err(device_error)?
    else {
        return Ok(());
    };
    for mut job in store.list_pending()? {
        if !job.terminal()
            && job.request.kind == JobKind::Restore
            && job.summary["phase"] == "device-restore-settling"
        {
            finalize_maintenance_outcome(&store, &mut job)?;
            let staging = runtime::job_directory(
                device.repository_root(),
                &job.request.connection_id,
                &job.id,
            )
            .join("restore-snapshot");
            cleanup_staging(&staging);
            app.state::<RuntimeRestoreState>().release(&job.id);
            runtime::release_leases(app, &job);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistent_store::sync_selection::CaptureIdentity;

    fn identity() -> CaptureIdentity {
        CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "library".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision: 0,
        }
    }

    #[test]
    fn settlement_cannot_release_a_restore_worker_that_has_not_returned() {
        let directory = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(directory.path()).unwrap();
        let request = serde_json::from_value(json!({
            "connectionId":"connection", "kind":"restore", "snapshotId":"snapshot",
            "targetRevision":"0", "restoreAreas":["library"]
        })).unwrap();
        let job = DurableJob::new(request, false, 1, identity());
        let commands = JobCommandState::default();
        let (cancel, worker) = commands.claim(&job).unwrap();
        let retained = RuntimeRestoreState::default();
        retained.retain(&job.id, store).unwrap();
        retained.retain_claim(&job.id, worker.clone()).unwrap();
        retained.release(&job.id);
        cancel.cancel();
        assert!(commands.claim(&job).is_err());
        drop(worker);
        assert!(commands.active.lock().unwrap().is_empty());
        assert!(commands.claim(&job).is_ok());
    }

    #[test]
    fn restore_area_selection_matches_the_renderer_contract() {
        assert_eq!(
            restore_selection(None).unwrap(),
            RestoreSelection {
                library: true,
                sections: BTreeSet::new()
            }
        );
        assert_eq!(
            restore_selection(Some(&["referencedAssets".into()])).unwrap(),
            RestoreSelection {
                library: true,
                sections: BTreeSet::new()
            }
        );
        assert_eq!(
            restore_selection(Some(&["library".into(), "hypa".into(), "local-settings".into()]))
                .unwrap(),
            RestoreSelection {
                library: true,
                sections: BTreeSet::from(["hypa".to_owned(), "local-settings".to_owned()])
            }
        );
    }

    /// A bundle declares what it covers. Asking for coverage it does not
    /// declare fails rather than restoring a narrowed selection.
    #[test]
    fn restore_area_selection_rejects_duplicates_and_undeclared_areas() {
        assert!(restore_selection(Some(&["library".into(), "library".into()])).is_err());
        assert!(restore_selection(Some(&["devicePlugins".into()])).is_err());
        assert!(restore_selection(Some(&["deviceSettings".into()])).is_err());
        assert!(restore_selection(Some(&["hypa".into()])).is_err());
        assert!(restore_selection(Some(&[])).is_err());
    }

    fn prepared(captured_by_device: Option<&str>) -> PreparedRemoteSnapshot {
        PreparedRemoteSnapshot {
            snapshot_id: "synthetic-snapshot".into(),
            repository_id: "synthetic-repository".into(),
            fingerprint: "00".repeat(32),
            library_fingerprint: "00".repeat(32),
            logical_revision: 1,
            staging_root: std::path::PathBuf::from("staging"),
            records: Vec::new(),
            objects: Vec::new(),
            captured_by_device: captured_by_device.map(ToOwned::to_owned),
        }
    }

    /// Invariant 32 on the restore side. Local settings belong to the device
    /// that wrote them, so only that device's own backup installs them. Another
    /// device's backup and a published state are refused even when the request
    /// names the area, and the areas beside them are unaffected.
    #[test]
    fn another_devices_backup_does_not_install_local_settings() {
        let areas = [
            "library".to_owned(),
            "hypa".to_owned(),
            "local-settings".to_owned(),
        ];
        let selection = restore_selection(Some(&areas)).unwrap();
        assert!(require_restorable_sections(&selection, &prepared(Some("this-device")), "this-device").is_ok());
        assert!(require_restorable_sections(
            &selection,
            &prepared(Some("other-device")),
            "this-device"
        )
        .is_err());
        assert!(require_restorable_sections(&selection, &prepared(None), "this-device").is_err());
        assert!(require_restorable_sections(&selection, &prepared(Some("")), "").is_err());

        let without = ["library".to_owned(), "hypa".to_owned(), "local-plugins".to_owned()];
        let selection = restore_selection(Some(&without)).unwrap();
        assert!(require_restorable_sections(&selection, &prepared(None), "this-device").is_ok());
        assert!(require_restorable_sections(
            &selection,
            &prepared(Some("other-device")),
            "this-device"
        )
        .is_ok());
    }

    #[test]
    fn dedicated_restore_store_survives_the_renderer_maintenance_gate() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(root.path()).unwrap();
        let renderer = PersistentStoreState::default();
        let retained = RuntimeRestoreState::default();

        retained.retain("restore-job", store).unwrap();
        let maintenance = renderer.acquire_device_maintenance().unwrap();
        assert_eq!(
            retained
                .with_store("restore-job", |store| {
                    store.revision().map_err(pds_error)
                })
                .unwrap(),
            0
        );
        assert!(retained.with_store("other-job", |_| Ok(())).is_err());
        drop(maintenance);
        retained.release("restore-job");
        assert!(retained.with_store("restore-job", |_| Ok(())).is_err());
    }

    #[test]
    fn durable_maintenance_outcome_finalizes_after_journal_release() {
        let root = tempfile::tempdir().unwrap();
        let store = JobStore::open(root.path()).unwrap();
        let request = serde_json::from_value(json!({
            "connectionId":"synthetic-connection",
            "kind":"restore",
            "snapshotId":"synthetic-snapshot",
            "targetRevision":"0"
        }))
        .unwrap();
        let mut job = DurableJob::new(request, false, 1, identity());
        job.summary["state"] = json!("waiting");
        job.summary["phase"] = json!("device-restore-settling");
        job.summary["result"] = json!({"maintenanceSessionId":"synthetic-session"});
        job.summary["maintenanceOutcome"] = json!({
            "state":"succeeded",
            "result":{
                "snapshotId":"synthetic-snapshot",
                "receivedRevision":"1"
            }
        });
        store.put(&job).unwrap();

        finalize_maintenance_outcome(&store, &mut job).unwrap();
        let reopened = store.read(&job.id).unwrap();
        assert_eq!(reopened.summary["state"], "succeeded");
        assert_eq!(reopened.summary["phase"], "complete");
        assert_eq!(reopened.summary["result"]["receivedRevision"], "1");
        assert!(reopened.summary.get("maintenanceOutcome").is_none());
    }

    #[test]
    fn library_restore_marker_recovers_only_the_exact_durable_request() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(root.path()).unwrap();
        let request = serde_json::from_value(json!({
            "connectionId":"synthetic-connection",
            "kind":"restore",
            "snapshotId":"synthetic-snapshot",
            "targetRevision":"0"
        }))
        .unwrap();
        let job = DurableJob::new(request, false, 1, identity());
        assert!(completed_restore_in_store(&store, &job).unwrap().is_none());
        let marker = restore_marker(&job, 0).unwrap();
        store
            .set_app_kv(
                &restore_marker_key(&job.id),
                &serde_json::to_value(marker).unwrap(),
            )
            .unwrap();
        let database =
            rusqlite::Connection::open(root.path().join("persistent/persistent.sqlite")).unwrap();
        database
            .execute("UPDATE meta SET value='1' WHERE key='currentRevision'", [])
            .unwrap();

        assert_eq!(
            completed_restore_in_store(&store, &job).unwrap().unwrap(),
            json!({"snapshotId":"synthetic-snapshot","receivedRevision":"1"})
        );
        let mut different = job.clone();
        different.request.snapshot_id = Some("different-snapshot".into());
        assert!(completed_restore_in_store(&store, &different).is_err());
    }
}
