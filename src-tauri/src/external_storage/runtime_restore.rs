//! Manual external snapshot restore and durable commit recovery.
//! Metadata is verified before activation; missing bodies follow renderer adoption.
use super::{
    connection_commands::ConnectedRepository,
    connection_store::ConnectionStore,
    contract::{Cancellation, ErrorKind, ProviderError, Result},
    job_store::{DurableJob, JobKind, JobStore},
    runtime,
    snapshot_restore::{self, PreparedRemoteSnapshot},
};
use crate::persistent_store::{
    commands::PersistentStoreState,
    external_apply::{ExternalSnapshotApplication, ExternalSnapshotObject, ExternalSnapshotRecord},
    lww::{StagingObserver, StagingProgress},
    PersistentStore, StoreError,
};
use crate::native_log::logged;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeSet, path::Path};
use tauri::{AppHandle, Manager, Runtime};
#[cfg(test)]
use super::worker_observation::spawn_blocking;
#[cfg(not(test))]
use tokio::task::spawn_blocking;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RestoreSelection {
    library: bool,
    sections: BTreeSet<String>,
}

const RESTORE_COMMIT_SCHEMA: &str = "risunest.external-restore-commit/v1";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreCommitIntent {
    schema: String,
    job_id: String,
    connection_id: String,
    snapshot_id: String,
    expected_revision: String,
    header: crate::persistent_store::lww::Header,
    staging_id: String,
}

fn restore_intent(job: &DurableJob, expected_revision: i64, header: crate::persistent_store::lww::Header, staging_id:&str) -> Result<RestoreCommitIntent> {
    Ok(RestoreCommitIntent {
        schema: RESTORE_COMMIT_SCHEMA.into(), job_id:job.id.clone(),
        connection_id:job.request.connection_id.clone(),
        snapshot_id:job.request.snapshot_id.clone().ok_or_else(corrupt)?,
        expected_revision:expected_revision.to_string(), header, staging_id:staging_id.into(),
    })
}

fn completed_restore_in_store(store: &PersistentStore, job: &DurableJob) -> Result<Option<Value>> {
    if job.request.kind != JobKind::Restore {return Ok(None)}
    let Some(value) = job.summary.get("restoreCommit") else {return Ok(None)};
    let intent:RestoreCommitIntent = serde_json::from_value(value.clone()).map_err(|_| corrupt())?;
    let expected_revision = job.request.target_revision.as_deref().ok_or_else(corrupt)?
        .parse::<i64>().map_err(|_| corrupt())?;
    if serde_json::to_value(&intent).map_err(runtime::local_error)? != *value
        || intent != restore_intent(job,expected_revision,intent.header.clone(),&intent.staging_id)?
        || intent.header.request_id != format!("external-backup-restore:{}",job.id)
        || intent.staging_id.is_empty() {return Err(corrupt())}
    let Some(receipt) = store.lww_device_replacement_receipt(&intent.header,&intent.staging_id).map_err(pds_error)? else {return Ok(None)};
    Ok(Some(json!({"snapshotId":intent.snapshot_id,"receivedRevision":receipt.revision.to_string()})))
}

fn persist_restore_intent(root:&Path, job:&DurableJob, intent:&RestoreCommitIntent) -> Result<()> {
    let jobs=JobStore::open(root)?;
    let mut current=jobs.read(&job.id)?;
    let value=serde_json::to_value(intent).map_err(runtime::local_error)?;
    if current.summary.get("restoreCommit").is_some_and(|old| old != &value) {return Err(corrupt())}
    current.summary["restoreCommit"]=value;
    current.summary["applicationStarted"]=json!(true);
    current.summary["phase"]=json!("applying-local");
    current.summary["updatedAtMs"]=json!(runtime::now_ms().to_string());
    jobs.put(&current)
}

/// Withdraws the commit a restore recorded when this library holds nothing of its request,
/// so the restore counts as not applied and prepares again when resumed.
fn withdraw_unreserved_commit(store:&PersistentStore, root:&Path, job_id:&str) -> Result<()> {
    let jobs=JobStore::open(root)?;
    let mut current=jobs.read(job_id)?;
    if current.summary.get("restoreCommit").is_none() || completed_restore_in_store(store,&current)?.is_some() {return Ok(())}
    let intent:RestoreCommitIntent=serde_json::from_value(current.summary["restoreCommit"].clone()).map_err(|_| corrupt())?;
    if !store.lww_request_unreserved(&intent.header.request_id).map_err(pds_error)? {return Ok(())}
    let summary=current.summary.as_object_mut().ok_or_else(corrupt)?;
    summary.remove("restoreCommit");
    summary.remove("applicationStarted");
    current.summary["phase"]=json!("preparing-local");
    current.summary["updatedAtMs"]=json!(runtime::now_ms().to_string());
    jobs.put(&current)
}

/// Read-only completion recovery used before reopening a provider connection.
pub(crate) fn completed_restore<R: Runtime>(app: &AppHandle<R>, job: &DurableJob) -> Result<Option<Value>> {
    if job.summary.get("restoreAdopted").is_none() || job.summary["restoreBodiesComplete"]!=true
        || !JobStore::open(&runtime::root(app)?)?.restore_bodies_settled(&job.id)? {
        return Ok(None);
    }
    completed_restore_in_store(&runtime::native_store(app)?, job)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub(crate) struct RestoreAdoptionRequest {
    pub job_id:String,
    pub received_revision:String,
    pub selected_character_id:Option<String>,
}

fn confirm_restore_adoption_in_store(store:&PersistentStore,root:&Path,request:&RestoreAdoptionRequest)->Result<Value> {
    let revision=request.received_revision.parse::<i64>().map_err(|_|corrupt())?;
    if revision<0 || revision.to_string()!=request.received_revision {return Err(corrupt());}
    let jobs=JobStore::open(root)?;
    let mut job=jobs.read(&request.job_id)?;
    let receipt=completed_restore_in_store(store,&job)?.ok_or_else(||ProviderError::new(ErrorKind::PreconditionFailed))?;
    if receipt["receivedRevision"]!=request.received_revision {return Err(ProviderError::new(ErrorKind::PreconditionFailed));}
    require_activated_target(store,&job,revision)?;
    let adopted=serde_json::to_value(request).map_err(runtime::local_error)?;
    if let Some(existing)=job.summary.get("restoreAdopted") {
        if existing!=&adopted {return Err(ProviderError::new(ErrorKind::PreconditionFailed));}
        return Ok(receipt);
    }
    if let Some(id)=&request.selected_character_id {
        if id.is_empty() || id.contains('\0') {return Err(corrupt());}
        if store.read_character_summary(id,None).map_err(pds_error)?.is_none() {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
    }
    job.summary["restoreAdopted"]=adopted;
    job.summary["updatedAtMs"]=json!(runtime::now_ms().to_string());
    jobs.put(&job)?;
    Ok(receipt)
}

fn require_activated_target(store:&PersistentStore,job:&DurableJob,revision:i64)->Result<()> {
    let intent:RestoreCommitIntent=serde_json::from_value(job.summary["restoreCommit"].clone()).map_err(|_|corrupt())?;
    let identity=store.external_identity().map_err(pds_error)?;
    let activated=store.lww_activated_receipt(&intent.header.request_id).map_err(pds_error)?;
    if identity.store_id!=job.admission_identity.store_id
        || identity.library_epoch!=job.admission_identity.library_epoch
        || identity.selection_epoch!=job.admission_identity.selection_epoch
        || activated.is_none_or(|(activated_revision,generation)|activated_revision!=revision || generation!=identity.generation)
        || store.lww_binding_authority().map_err(pds_error)?!=intent.header.binding_authority {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(())
}

pub(crate) fn confirmed_restore_activation<R: Runtime>(app:&AppHandle<R>,job:&DurableJob)->Result<Option<Value>> {
    if job.summary["applicationStarted"]!=true {return Ok(None);}
    let store=runtime::native_store(app)?;
    let Some(receipt)=completed_restore_in_store(&store,job)? else {return Ok(None)};
    let revision=receipt["receivedRevision"].as_str().ok_or_else(corrupt)?.parse().map_err(|_|corrupt())?;
    match require_activated_target(&store,job,revision) {
        Ok(())=>Ok(Some(receipt)),
        Err(error) if error.kind==ErrorKind::PreconditionFailed=>Ok(None),
        Err(error)=>Err(error),
    }
}

#[tauri::command(async)]
pub(crate) fn external_storage_confirm_restore_adoption<R: Runtime>(app:AppHandle<R>,request:RestoreAdoptionRequest)->Result<Value> {
    logged("external_storage_confirm_restore_adoption", (|| confirm_restore_adoption_in_store(&runtime::native_store(&app)?,&runtime::root(&app)?,&request))())
}

pub(crate) fn application_started(job: &DurableJob) -> bool {
    job.request.kind == JobKind::Restore && job.summary["applicationStarted"] == true
}

/// Whether the restore that began a CAS journal can no longer use it. A restore
/// with a live worker keeps it. An unsettled local application keeps its
/// journal until recovery or an explicit stop resolves its receipt.
pub(crate) fn restore_journal_owner_ended(
    root: &Path,
    job_id: &str,
    worker_active: bool,
    open_store: &dyn Fn() -> std::result::Result<PersistentStore, String>,
) -> std::result::Result<bool, String> {
    if worker_active {
        return Ok(false);
    }
    let job = match JobStore::open(root).and_then(|jobs| jobs.read(job_id)) {
        Ok(job) => job,
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.to_string()),
    };
    if job.request.kind != JobKind::Restore || job.terminal() {
        return Ok(true);
    }
    // Before application a resumed restore pins what it needs again.
    if !application_started(&job) {
        return Ok(true);
    }
    let store = open_store()?;
    let Some(receipt) = completed_restore_in_store(&store, &job).map_err(|error| error.to_string())? else {
        return Ok(false);
    };
    let revision = receipt["receivedRevision"].as_str().and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| corrupt().to_string())?;
    match require_activated_target(&store, &job, revision) {
        Ok(()) => Ok(false),
        Err(error) if error.kind == ErrorKind::PreconditionFailed => Ok(true),
        Err(error) => Err(error.to_string()),
    }
}

/// Whether a person stopped this restore after its local application could
/// not be confirmed, which ends it as it is.
pub(crate) fn restore_stopped(job: &DurableJob) -> bool {
    job.terminal() && job.summary["restoreStopped"] == true
}

/// Ends a restore whose local application could not be confirmed. The library
/// stays as it is, and the files the restore still held are released.
pub(crate) fn stop_restore(root: &Path, job_id: &str) -> Result<DurableJob> {
    let jobs = JobStore::open(root)?;
    let mut job = jobs.read(job_id)?;
    if !application_started(&job) || job.summary["state"] != "uncertain" {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    job.summary["state"] = json!("failed");
    job.summary["phase"] = json!("paused");
    job.summary["restoreStopped"] = json!(true);
    job.summary.as_object_mut().ok_or_else(corrupt)?.remove("result");
    job.summary["error"] = runtime::error_dto(&ProviderError::new(ErrorKind::Cancelled));
    job.summary["updatedAtMs"] = json!(runtime::now_ms().to_string());
    jobs.put(&job)?;
    // A failed release leaves a journal whose restore has ended, which the
    // next page start releases.
    match crate::asset_repository::job_pins::DurableCasJob::open(root, job_id) {
        Ok(mut pins) => pins
            .release(crate::asset_repository::job_pins::CasReleaseOutcome::Aborted)
            .map_err(runtime::local_error)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(runtime::local_error(error)),
    }
    discard_finished_staging(root, &job);
    Ok(job)
}

#[cfg(test)]
fn mark_application_started(root: &Path, job: &DurableJob) -> Result<()> {
    let jobs = JobStore::open(root)?;
    let mut current = jobs.read(&job.id)?;
    current.summary["applicationStarted"] = json!(true);
    current.summary["phase"] = json!("applying-local");
    current.summary["updatedAtMs"] = json!(runtime::now_ms().to_string());
    jobs.put(&current)
}

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

fn pds_error(error: StoreError) -> ProviderError {
    match error {
        StoreError::RevisionConflict { .. } => ProviderError::new(ErrorKind::PreconditionFailed),
        StoreError::Validation { message } => {
            crate::nlog!("warn", "external restore rejected by the store: {message}");
            corrupt()
        }
        _ => ProviderError::new(ErrorKind::Transient),
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

const FULL_RESTORE_AREAS: [&str;5]=["library","referencedAssets","hypa","local-plugins","local-settings"];
/// The device sections a backup that a device captured carries.
pub(super) fn device_section_ids()->BTreeSet<String> {
    BTreeSet::from(["hypa".into(),"local-plugins".into(),"local-settings".into()])
}
fn restore_selection(restore_areas:Option<&[String]>)->Result<RestoreSelection> {
    if let Some(areas)=restore_areas {
        let unique=areas.iter().map(String::as_str).collect::<BTreeSet<_>>();
        if unique.len()!=areas.len() || unique!=FULL_RESTORE_AREAS.into_iter().collect() {return Err(corrupt());}
    }
    Ok(RestoreSelection{library:true,sections:device_section_ids()})
}
fn require_restorable_sections(_selection:&RestoreSelection,snapshot:&PreparedRemoteSnapshot,_store_id:&str)->Result<()> {
    if snapshot.captured_by_device.as_deref().is_none_or(str::is_empty) {return Err(ProviderError::new(ErrorKind::PreconditionFailed));}
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

pub(crate) async fn prepare_database_first_backup(
    root:&Path,connected:&ConnectedRepository,job:&DurableJob,cancel:&Cancellation,
)->Result<(snapshot_restore::DatabaseFirstSnapshot,Vec<super::sections::CapturedSection>)> {
    if job.request.kind!=JobKind::Restore {return Err(corrupt());}
    let snapshot_id=job.request.snapshot_id.as_deref().ok_or_else(corrupt)?;
    update_phase(&root, job, "downloading")?;
    let current=JobStore::open(&root)?.read(&job.id)?;
    let remote=if let Some(source)=current.summary.get("restoreSource") {
        let source:risunest_external_storage_format::snapshot::StoredObject=serde_json::from_value(source.clone()).map_err(|_|corrupt())?;
        super::packaging::RemoteObject::from_stored(&source,&connected.handle)?
    } else {
    let known = match ConnectionStore::open(&root)?
        .discovery_snapshot(&job.request.connection_id, snapshot_id)
    {
        Ok(value) => Some(value),
        Err(error) if error.kind == ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let cache_root = root.to_owned();
    let cache_connection = job.request.connection_id.clone();
    let cache_id = snapshot_id.to_owned();
    let remote = super::control::find_snapshot_with_locator_invalidation(
        connected,
        snapshot_id,
        known.as_ref(),
        move || {
            ConnectionStore::open(&cache_root)?.forget_discovery(&cache_connection, &cache_id)
        },
        cancel,
    )
    .await?;
    ConnectionStore::open(&root)?.remember_discovery(
        &job.request.connection_id,
        snapshot_id,
        &remote,
    )?;
    let jobs=JobStore::open(&root)?;
    let mut current=jobs.read(&job.id)?;
    current.summary["restoreSource"]=serde_json::to_value(remote.stored(&connected.handle)?).map_err(runtime::local_error)?;
    jobs.put(&current)?;
    remote
    };
    let transferred = runtime::transfer_progress(&root, &job.id);
    download_database_first_backup(root, connected, job, snapshot_id, &remote, &transferred, cancel).await
}

/// Reads every catalog the restore needs before it fetches a pack, so the
/// transfer it reports is planned once, the asset bodies it receives after
/// adoption included, and its total does not move from the first reading on.
async fn download_database_first_backup(
    root:&Path,connected:&ConnectedRepository,job:&DurableJob,snapshot_id:&str,
    remote:&super::packaging::RemoteObject,transferred:&super::phase_progress::PhaseProgress,cancel:&Cancellation,
)->Result<(snapshot_restore::DatabaseFirstSnapshot,Vec<super::sections::CapturedSection>)> {
    let staging_root = staging_directory(root, job);
    let walked = super::phase_progress::PhaseProgress::silent();
    let walk=snapshot_restore::walk_snapshot_database_first(remote,&staging_root,root,root,
        &job.request.connection_id,&connected.root_key,connected.provider.as_ref(),&connected.handle,&walked,cancel).await?;
    let selection = restore_selection(job.request.restore_areas.as_deref())?;
    let walked_sections = snapshot_restore::walk_sections(
        remote,
        &selection.sections,
        &staging_root,
        &connected.root_key,
        Some(&super::runtime::connection_directory(root,&job.request.connection_id).join("package-cache")),
        connected.provider.as_ref(),
        &connected.handle,
        &walked,
        cancel,
    )
    .await?;
    let (items, bytes) = walk.remaining()?;
    let (section_items, section_bytes) = walked_sections.remaining();
    transferred.plan_after(
        walked.read(),
        items.saturating_add(section_items),
        bytes.saturating_add(section_bytes),
    );
    let database=snapshot_restore::fetch_snapshot_database_first(walk,&connected.root_key,connected.provider.as_ref(),&connected.handle,transferred,cancel).await?;
    let snapshot=&database.snapshot;
    cancel.check()?;
    validate_download(connected, snapshot_id, snapshot)?;
    require_restorable_sections(&selection, snapshot, &job.admission_identity.store_id)?;
    let sections = walked_sections
        .fetch(
            &staging_root,
            &connected.root_key,
            connected.provider.as_ref(),
            &connected.handle,
            transferred,
            cancel,
        )
        .await?;
    transferred.flush();
    if sections.len() != selection.sections.len() {
        return Err(ProviderError::new(ErrorKind::NotFound));
    }
    let required = checked_required_bytes(snapshot, &selection)?;
    if available_space(&staging_root).map_err(runtime::local_error)? < required {
        return Err(ProviderError::new(ErrorKind::StorageFull));
    }
    update_phase(&root, job, "preparing-local")?;

    Ok((database,sections))
}

pub(crate) async fn run_restore<R: Runtime>(
    app: &AppHandle<R>,
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
    let root = runtime::root(app)?;
    let observation = super::progress::JobProgress::begin(runtime::job_directory(&root, &job.request.connection_id, &job.id));
    observation.scope(async {
    let store=runtime::native_store(app)?;
    if let Some(result)=completed_restore_in_store(&store,job)? {
        return finish_restore_bodies(app,connected,job,result,None,store,cancel).await;
    }
    // A commit the library never received is withdrawn before the download, so
    // a download that fails leaves the restore not applied.
    withdraw_unreserved_commit(&store,&root,&job.id)?;
    drop(store);
    let permit=app.state::<crate::native_file_jobs::NativeFileJobState>().admission.staging().map_err(runtime::local_error)?;
    let expected_revision = job
        .request
        .target_revision
        .as_deref()
        .ok_or_else(corrupt)?
        .parse::<i64>()
        .map_err(|_| corrupt())?;
    let (database,sections)=prepare_database_first_backup(&root,connected,job,cancel).await?;
    let snapshot=database.snapshot;
    let original_units=database.original_units;
    let selection=restore_selection(job.request.restore_areas.as_deref())?;

    let worker_app = app.clone();
    let worker_job = job.clone();
    let worker_cancel = cancel.clone();
    let report_staging = observation.staging_reporter();
    let prepared = spawn_blocking(move || {
        prepare_local_restore(
            &worker_app,
            &worker_job,
            expected_revision,
            snapshot,
            original_units,
            selection,
            sections,
            worker_cancel,
            database.sources,
            database.present,
            permit,
            &report_staging,
        )
    })
    .await.map_err(runtime::local_error)?;
    let result=match prepared {
        Ok(result)=>result,
        Err(error)=>{
            if !application_started(&JobStore::open(&root)?.read(&job.id)?) {
                if let Ok(mut pins)=crate::asset_repository::job_pins::DurableCasJob::open(&root,&job.id) {
                    pins.release(crate::asset_repository::job_pins::CasReleaseOutcome::Aborted).map_err(runtime::local_error)?;
                }
            }
            return Err(error);
        }
    };
    finish_restore_bodies(app,connected,job,result.0,Some(result.1),result.2,cancel).await
    }).await
}

fn restore_pins(root:&Path,id:&str)->Result<crate::asset_repository::job_pins::DurableCasJob> {
    use crate::asset_repository::job_pins::{DurableCasJob,CasJobKind};
    let pins=match DurableCasJob::begin(root,id,CasJobKind::LocalBackupRestore,crate::asset_repository::job_pins::CasJobOwner::external_restore(id),runtime::now_ms() as i64) {
        Ok(pins)=>pins,
        Err(error) if error.kind()==std::io::ErrorKind::AlreadyExists=>DurableCasJob::open(root,id).map_err(runtime::local_error)?,
        Err(error)=>return Err(runtime::local_error(error)),
    };
    if pins.kind()!=CasJobKind::LocalBackupRestore || pins.is_released() {return Err(corrupt());}
    Ok(pins)
}

/// `store` is the connection the restore committed or recovered through, and it
/// serves the rest of the restore without going through the renderer store.
async fn finish_restore_bodies<R: Runtime>(app:&AppHandle<R>,connected:&ConnectedRepository,job:&DurableJob,result:Value,
    mut permit:Option<crate::native_file_jobs::admission::Permit>,store:PersistentStore,cancel:&Cancellation)->Result<Value> {
    let root=runtime::root(app)?;
    let jobs=JobStore::open(&root)?;
    if jobs.read(&job.id)?.summary["restoreBodiesReady"]!=true {return Err(corrupt());}
    let revision=result["receivedRevision"].as_str().ok_or_else(corrupt)?.parse::<i64>().map_err(|_|corrupt())?;
    let adoption=loop {
        let current=jobs.read(&job.id)?;
        require_activated_target(&store,&current,revision)?;
        if let Some(value)=current.summary.get("restoreAdopted") {
            let request:RestoreAdoptionRequest=serde_json::from_value(value.clone()).map_err(|_|corrupt())?;
            confirm_restore_adoption_in_store(&store,&root,&request)?;
            break request;
        }
        if permit.is_none() {permit=Some(app.state::<crate::native_file_jobs::NativeFileJobState>().admission.file(true).map_err(runtime::local_error)?);}
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    drop(permit);
    receive_restore_bodies_in_store(store,&root,connected,job,result,adoption.selected_character_id,cancel).await
}

async fn receive_restore_bodies_in_store(mut store:PersistentStore,root:&Path,connected:&ConnectedRepository,job:&DurableJob,result:Value,selected_character_id:Option<String>,cancel:&Cancellation)->Result<Value> {
    let root=root.to_owned();
    let jobs=JobStore::open(&root)?;
    let current=jobs.read(&job.id)?;
    let job=&current;
    let revision=result["receivedRevision"].as_str().ok_or_else(corrupt)?.parse::<i64>().map_err(|_|corrupt())?;
    if completed_restore_in_store(&store,job)?.as_ref()!=Some(&result) {return Err(corrupt());}
    require_activated_target(&store,job,revision)?;
    update_phase(&root,job,"receiving-assets")?;
    let priority=match selected_character_id {
        Some(id)=>store.selected_character_asset_hashes(&id).map_err(pds_error)?.into_iter().collect::<BTreeSet<_>>(),
        None=>BTreeSet::new(),
    };
    let stage=root.join("native-file-jobs/jobs").join(&job.id).join("external-restore-bodies");
    let mut plan=snapshot_restore::RestoreBodyPlan::new(&stage)?;
    let transferred=runtime::resumed_transfer_progress(&root,job);
    let mut pins=restore_pins(&root,&job.id)?;
    let cas=crate::asset_repository::PayloadCas::new(&root).map_err(runtime::local_error)?;
    let mut after=String::new();
    loop {
        cancel.check()?;require_activated_target(&store,job,revision)?;
        let page=jobs.restore_body_page(&job.id,&after,&mut Default::default())?;
        if page.is_empty() {break;}
        let mut present=Vec::new();
        for source in page {
            after=source.hash.clone();
            super::lww_residency::validate_packed_source(&source,&connected.handle)?;
            match cas.stat_object(&source.hash).map_err(runtime::local_error)? {
                Some(size) if size==source.byte_length=>present.push((source.hash,source.byte_length,crate::asset_repository::job_pins::CasObjectRole::DirectObject)),
                Some(_)=>return Err(corrupt()),
                None=>plan.push(&source,priority.contains(&source.hash),&connected.handle)?,
            }
        }
        if !present.is_empty() {
            cancel.check()?;require_activated_target(&store,job,revision)?;
            pins.pin_existing_batch(&cas,&present).map_err(runtime::local_error)?;
            require_activated_target(&store,job,revision)?;
            for (hash,_,_) in &present {jobs.settle_restore_body(&job.id,hash)?;}
        }
    }
    plan.seal()?;
    loop {
        cancel.check()?;require_activated_target(&store,job,revision)?;
        while let Some((hash,path))=plan.ready()? {
            let source=jobs.restore_body(&job.id,&hash)?.ok_or_else(corrupt)?.interned(&mut Default::default());
            (store,pins)=receive_restore_body(store,pins,&root,job,revision,&source,path,cancel).await?;
            jobs.settle_restore_body(&job.id,&hash)?;
            plan.settled(&hash)?;
        }
        if !plan.next_pack(&connected.root_key,connected.provider.as_ref(),&connected.handle,&transferred,cancel).await? {break;}
    }
    transferred.flush();
    drop(plan);
    cleanup_staging(&stage);
    cancel.check()?;
    require_activated_target(&store,job,revision)?;
    let worker_root=root.clone();let worker_id=job.id.clone();let worker_job=job.clone();
    spawn_blocking(move || {
        require_activated_target(&store,&worker_job,revision)?;
        pins.seal(&mut store,runtime::now_ms() as i64).map_err(runtime::local_error)?;
        pins.release(crate::asset_repository::job_pins::CasReleaseOutcome::Committed).map_err(runtime::local_error)?;
        let jobs=JobStore::open(&worker_root)?;
        let mut current=jobs.read(&worker_id)?;
        if !jobs.restore_bodies_settled(&worker_id)? {return Err(corrupt());}
        current.summary["restoreBodiesComplete"]=json!(true);
        jobs.put(&current)
    }).await.map_err(runtime::local_error)??;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn receive_restore_body(store:PersistentStore,mut pins:crate::asset_repository::job_pins::DurableCasJob,root:&Path,job:&DurableJob,revision:i64,
    source:&super::lww_residency::SharedPackedSource,path:std::path::PathBuf,cancel:&Cancellation)->Result<(PersistentStore,crate::asset_repository::job_pins::DurableCasJob)> {
    cancel.check()?;
    require_activated_target(&store,job,revision)?;
    let cas=crate::asset_repository::PayloadCas::new(root).map_err(runtime::local_error)?;
    let (store,pins)=match cas.stat_object(&source.hash).map_err(runtime::local_error)? {
        Some(size) if size==source.byte_length=>{
            pins.pin_existing(&cas,&source.hash,source.byte_length,crate::asset_repository::job_pins::CasObjectRole::DirectObject).map_err(runtime::local_error)?;
            (store,pins)
        }
        Some(_)=>return Err(corrupt()),
        None=>{
            let worker_source=source.clone();let worker_cancel=cancel.clone();let worker_job=job.clone();
            spawn_blocking(move || {
                require_activated_target(&store,&worker_job,revision)?;
                let adopted=pins.adopt_import_payload(&cas,&path,&worker_source.hash,worker_source.byte_length,&|| {
                    worker_cancel.check().is_err() || require_activated_target(&store,&worker_job,revision).is_err()
                });
                require_activated_target(&store,&worker_job,revision)?;
                adopted.map_err(runtime::local_error)?;
                Ok::<_,ProviderError>((store,pins))
            }).await.map_err(runtime::local_error)??
        }
    };
    require_activated_target(&store,job,revision)?;
    Ok((store,pins))
}

/// The units as the store reads them. A read failure is kept in `failure` so
/// it is reported as itself rather than as invalid data.
fn store_units<'a>(
    units:&'a snapshot_restore::OriginalUnits,
    content:&'a super::content_store::ContentStore,
    failure:&'a mut Option<ProviderError>,
) -> impl Iterator<Item = crate::persistent_store::StoreResult<(risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue)>> + 'a {
    units.units(content).map(move |unit| unit.map_err(|error| {
        if failure.is_none() {*failure=Some(error);}
        StoreError::Validation{message:"Original unit control is unavailable".into()}
    }))
}

fn stage_original_backup_controls(
    store:&mut PersistentStore,
    snapshot:&PreparedRemoteSnapshot,
    units:&snapshot_restore::OriginalUnits,
    cancel:&Cancellation,
    report:&dyn Fn(Option<StagingProgress>),
) -> Result<()> {
    use crate::persistent_store::external_capture::{original_unit_control_lengths,streamed_unit_dependency_inventory,BackupBodyRole};
    let content=super::content_store::ContentStore::open(&snapshot.staging_root.join("external-storage")).map_err(pds_error)?;
    let declared=snapshot.objects.iter().map(|object| (object.content_hash.as_str(),object)).collect::<std::collections::BTreeMap<_,_>>();
    if declared.len() != snapshot.objects.len() {return Err(corrupt())}
    let read=|hash:&str,bound:u64| -> crate::persistent_store::StoreResult<Option<Vec<u8>>> {
        cancel.check().map_err(|_| StoreError::Validation{message:"External restore cancelled".into()})?;
        let Some(object)=declared.get(hash) else {return Ok(None)};
        if matches!(object.source,super::content_store::ObjectSource::Library(_)) {return Ok(None)}
        if object.byte_length > bound {return Err(StoreError::Validation{message:"Backup control exceeds its bound".into()})}
        let mut source=content.open_source(&object.source)?;
        let mut bytes=Vec::new();
        use std::io::Read;
        source.by_ref().take(object.byte_length.saturating_add(1)).read_to_end(&mut bytes)?;
        if bytes.len() as u64 != object.byte_length {return Err(StoreError::Validation{message:"Backup control size differs".into()})}
        Ok(Some(bytes))
    };
    let size=|hash:&str| Ok(declared.get(hash).map(|object| object.byte_length));
    let probe=runtime::CancelProbe(cancel.clone());
    let mut emit=|progress:StagingProgress| -> crate::persistent_store::StoreResult<()> {report(Some(progress));Ok(())};
    let mut progress=StagingObserver::new(&mut emit);
    // Both passes read every unit, so one share counts the units twice.
    progress.begin("read",Some(units.count()?.saturating_mul(2))).map_err(pds_error)?;
    // Manifests and archive metadata use their declared content lengths.
    // Message pages and large unit bodies are read in the second pass.
    let mut failure=None;
    let metadata=original_unit_control_lengths(store_units(units,&content,&mut failure).map(|unit| {progress.advance()?;unit}),
        &|hash| match size(hash)? {Some(length)=>read(hash,length),None=>Ok(None)},
        &size,&size,&probe);
    cancel.check()?;
    if let Some(failure)=failure.take() {return Err(failure)}
    let metadata=metadata.map_err(pds_error)?;
    let mut spool=super::capture::BackupDependencySpool::new(&snapshot.staging_root.join("verified-original-controls")).map_err(pds_error)?;
    let inventory=streamed_unit_dependency_inventory(store_units(units,&content,&mut failure).map(|unit| {progress.advance()?;unit}),
        &|hash| match metadata.controls.get(hash) {Some(length)=>read(hash,*length),None=>Ok(None)},
        &size,&probe,true,&mut |hash,bytes,role| Ok(spool.push(hash,bytes,role)?),
    );
    cancel.check()?;
    if let Some(failure)=failure.take() {return Err(failure)}
    let inventory=inventory.map_err(pds_error)?;
    if !inventory.spooled_payloads.is_empty() {return Err(corrupt())}
    cancel.check()?;
    spool.seal().map_err(pds_error)?;
    progress.begin("copy",Some(spool.count().map_err(pds_error)?)).map_err(pds_error)?;
    let result=spool.visit(&mut |hash,bytes,role| {
        if cancel.check().is_err() {return Err(StoreError::Validation{message:"External restore cancelled".into()})}
        if role != BackupBodyRole::Control {return Err(StoreError::Validation{message:"Backup control role differs".into()})}
        store.lww_put_object(hash,bytes)?;
        progress.advance()
    });
    cancel.check()?;
    result.map_err(pds_error)
}

/// Stages the original units as the source units of the replacement `staging_id`.
fn stage_original_units(
    store:&mut PersistentStore,
    staging_id:&str,
    staging_root:&Path,
    units:&snapshot_restore::OriginalUnits,
    cancel:&Cancellation,
) -> Result<()> {
    let content=super::content_store::ContentStore::open_existing(&staging_root.join("external-storage")).map_err(pds_error)?.ok_or_else(corrupt)?;
    let writer=store.replacement_source_writer(staging_id).map_err(pds_error)?;
    for unit in units.units(&content) {
        cancel.check()?;
        let (key,value)=unit?;
        writer.put(&key,&value).map_err(pds_error)?;
    }
    writer.finish().map_err(pds_error)
}

#[allow(clippy::too_many_arguments)]
fn prepare_local_restore<R: Runtime>(
    app: &AppHandle<R>,
    job: &DurableJob,
    expected_revision: i64,
    snapshot: PreparedRemoteSnapshot,
    original_units: snapshot_restore::OriginalUnits,
    selection: RestoreSelection,
    sections: Vec<super::sections::CapturedSection>,
    cancel: Cancellation,
    sources:Vec<super::lww_residency::SharedPackedSource>,
    present:BTreeSet<String>,
    permit:crate::native_file_jobs::admission::Permit,
    report:&dyn Fn(Option<StagingProgress>),
) -> Result<(Value,crate::native_file_jobs::admission::Permit,PersistentStore)> {
    let app_data_dir=crate::app_paths::data_root(app).map_err(runtime::local_error)?;
    let mut store=runtime::native_store(app)?;
    let (result,permit)=prepare_local_restore_in_store(&mut store,&app.state::<PersistentStoreState>(),&app_data_dir,job,expected_revision,snapshot,original_units,selection,sections,cancel,sources,present,permit,report)?;
    Ok((result,permit,store))
}

/// Device maintenance for a restore commit. The renderer store it closed, which
/// opens at `app_data_dir`, is open again before renderer operations resume,
/// whether or not the commit landed.
struct CommitMaintenance<'a> {
    state:&'a PersistentStoreState,
    app_data_dir:&'a Path,
    guard:crate::persistent_store::commands::DeviceMaintenanceGuard,
}

impl Drop for CommitMaintenance<'_> {
    fn drop(&mut self) {
        let _=logged("external_storage_restore_reopen",self.state.reopen_under_maintenance(&self.guard,self.app_data_dir));
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_local_restore_in_store(
    store:&mut PersistentStore,
    state:&PersistentStoreState,
    app_data_dir:&Path,
    job: &DurableJob,
    expected_revision: i64,
    snapshot: PreparedRemoteSnapshot,
    original_units: snapshot_restore::OriginalUnits,
    selection: RestoreSelection,
    sections: Vec<super::sections::CapturedSection>,
    cancel: Cancellation,
    sources:Vec<super::lww_residency::SharedPackedSource>,
    present:BTreeSet<String>,
    mut permit:crate::native_file_jobs::admission::Permit,
    report:&dyn Fn(Option<StagingProgress>),
) -> Result<(Value,crate::native_file_jobs::admission::Permit)> {
    cancel.check()?;
    // The caller opened this dedicated native connection while renderer
    // admission was available, so the commit runs through it under maintenance.
    let root=store.repository_root().to_owned();
    let current_identity=store.external_identity().map_err(pds_error)?;
    runtime::require_admitted_library(job,&current_identity)?;
    if current_identity.selection_epoch != job.admission_identity.selection_epoch {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    withdraw_unreserved_commit(store,&root,&job.id)?;
    cancel.check()?;
    if store.revision().map_err(pds_error)? != expected_revision {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    stage_original_backup_controls(store,&snapshot,&original_units,&cancel,report)?;
    // The steps up to the snapshot application report no share.
    report(None);
    JobStore::open(&root)?.freeze_restore_bodies(job,&sources,&present)?;
    // A catalog row needs its body or its remote source, so the sources are registered first.
    super::lww_residency::register_verified_packed_many(store.repository_root(),&sources)?;
    #[cfg(test)] registration_crash(&root,RegistrationCrash::AfterFirst)?;
    let registrations=sources.iter().map(|source|crate::persistent_store::asset_object_catalog::AssetObjectRegistration{object_hash:source.hash.clone(),byte_size:source.byte_length}).collect::<Vec<_>>();
    for batch in registrations.chunks(crate::persistent_store::asset_object_catalog::ASSET_OBJECT_CATALOG_MAX_PAGE as usize) {
        store.asset_object_catalog().register(batch,runtime::now_ms() as i64).map_err(pds_error)?;
    }
    #[cfg(test)] registration_crash(&root,RegistrationCrash::AfterSecond)?;
    let current=JobStore::open(&root)?.read(&job.id)?;
    let previous_intent=current.summary.get("restoreCommit").map(|value|serde_json::from_value::<RestoreCommitIntent>(value.clone()).map_err(|_|corrupt())).transpose()?;
    if previous_intent.is_some() {completed_restore_in_store(store,&current)?;}
    let cas=crate::asset_repository::PayloadCas::new(store.repository_root()).map_err(runtime::local_error)?;
    let mut pins=restore_pins(store.repository_root(),&job.id)?;
    for batch in sources.iter().filter(|source|present.contains(&source.hash)).map(|source|(source.hash.clone(),source.byte_length,crate::asset_repository::job_pins::CasObjectRole::DirectObject)).collect::<Vec<_>>().chunks(128) {
        pins.pin_existing_batch(&cas,batch).map_err(runtime::local_error)?;
    }
    let snapshot_id = snapshot.snapshot_id.clone();
    let staging_root = snapshot.staging_root.clone();
    let prepared = if selection.library && previous_intent.is_none() {
        let scope_id = risunest_external_storage_format::format::library_fingerprint_domain();
        let fingerprint = decode_hash(&snapshot.library_fingerprint)?;
        let records = snapshot.records.into_iter().map(|record| {
            Ok(ExternalSnapshotRecord {
                key: record.key,
                content_hash: record.content_hash,
                byte_length: record.byte_length,
                source: record.source,
            })
        });
        let objects = snapshot.objects.into_iter().map(|object| {
            Ok(ExternalSnapshotObject {
                content_hash: object.content_hash,
                byte_length: object.byte_length,
                source: object.source,
            })
        });
        let probe = runtime::CancelProbe(cancel.clone());
        Some(
            store
                .prepare_external_snapshot_application_observed(
                    &ExternalSnapshotApplication {
                        expected_revision,
                        staging_root: &staging_root,
                        scope_id: &scope_id,
                        fingerprint: &fingerprint,
                        probe: &probe,
                    },
                    records,
                    objects,
                    &mut |progress| { report(Some(progress)); Ok(()) },
                )
                .map_err(|error| {
                    if cancel.check().is_err() {
                        ProviderError::new(ErrorKind::Cancelled)
                    } else {
                        pds_error(error)
                    }
                })?,
        )
    } else {
        None
    };
    // The steps after the snapshot application report no share.
    report(None);

    // Preparing every selected section before touching the device file keeps a
    // bundle with a missing object from installing half of itself.
    let scratch=super::leftovers::scratch_directory(&root).map_err(runtime::local_error)?;
    let prepared_sections =
        super::sections::prepare_received_backup_sections(&sections, &scratch, &cancel)?;
    let intent=if let Some(intent)=previous_intent {intent} else {
    let header = crate::persistent_store::lww::Header {
        binding_authority:store.lww_binding_authority().map_err(pds_error)?,
        request_id:format!("external-backup-restore:{}",job.id),
    };
    restore_intent(job,expected_revision,header,prepared.as_ref().ok_or_else(corrupt)?.external_staging_id())?
    };
    stage_original_units(store,&intent.staging_id,&staging_root,&original_units,&cancel)?;
    cancel.check()?;
    permit.upgrade_staging().map_err(runtime::local_error)?;
    #[cfg(test)] write_before_commit(&root,store)?;
    let maintenance=CommitMaintenance{state,app_data_dir,guard:state.acquire_device_maintenance().map_err(pds_error)?};
    let committed=(||->Result<_> {
        if store.revision().map_err(pds_error)?!=expected_revision {return Err(ProviderError::new(ErrorKind::PreconditionFailed));}
        persist_restore_intent(&root,job,&intent)?;
        #[cfg(test)] commit_failure(&root,CommitFailure::BeforeStore)?;
        let revision = store.lww_commit_staged_replacement_with_device_sections(
            &intent.header,&intent.staging_id,&prepared_sections.iter().collect::<Vec<_>>(),
        ).map_err(pds_error)?;
        #[cfg(test)] commit_failure(&root,CommitFailure::AfterStore)?;
        store.release_replacement_source(&intent.staging_id).map_err(pds_error)?;
        Ok(revision)
    })();
    drop(prepared);
    drop(maintenance);
    if committed.is_err() {
        let _=logged("external_storage_restore_withdraw",withdraw_unreserved_commit(store,&root,&job.id));
    }
    let revision=committed?;
    cleanup_staging(&staging_root);
    let result=json!({
        "snapshotId": snapshot_id,
        "receivedRevision": revision.revision.to_string()
    });
    let jobs=JobStore::open(&root)?;
    let mut current=jobs.read(&job.id)?;
    current.summary["result"]=result.clone();
    current.summary["phase"]=json!("awaiting-adoption");
    jobs.put(&current)?;
    Ok((result,permit))
}

#[cfg(test)]
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
enum RegistrationCrash {AfterFirst,AfterSecond}

#[cfg(test)]
static REGISTRATION_CRASHES:std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf,RegistrationCrash>>=std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Stops a restore preparation of `root` at `point` as a process exit would, leaving what was
/// already on disk.
#[cfg(test)]
fn registration_crash(root:&Path,point:RegistrationCrash)->Result<()> {
    let root=std::fs::canonicalize(root).unwrap_or_else(|_|root.to_owned());
    if REGISTRATION_CRASHES.lock().unwrap().get(&root)==Some(&point) {return Err(ProviderError::new(ErrorKind::Cancelled));}
    Ok(())
}

#[cfg(test)]
struct RegistrationCrashGuard(std::path::PathBuf);
#[cfg(test)]
impl Drop for RegistrationCrashGuard {
    fn drop(&mut self) {REGISTRATION_CRASHES.lock().unwrap().remove(&self.0);}
}
#[cfg(test)]
fn crash_registration_at(root:&Path,point:RegistrationCrash)->RegistrationCrashGuard {
    let root=std::fs::canonicalize(root).unwrap();
    REGISTRATION_CRASHES.lock().unwrap().insert(root.clone(),point);
    RegistrationCrashGuard(root)
}

#[cfg(test)]
static WRITES_BEFORE_COMMIT:std::sync::Mutex<BTreeSet<std::path::PathBuf>>=std::sync::Mutex::new(BTreeSet::new());

/// Changes the library at `root` once, just before a restore commit takes maintenance, as a
/// write that lands after the restore was staged would.
#[cfg(test)]
fn write_before_commit(root:&Path,store:&mut PersistentStore)->Result<()> {
    let root=std::fs::canonicalize(root).unwrap_or_else(|_|root.to_owned());
    if !WRITES_BEFORE_COMMIT.lock().unwrap().remove(&root) {return Ok(());}
    store.commit(&crate::persistent_store::WorkingSetCommit{
        expected_revision:store.revision().map_err(pds_error)?,
        root_mutations:Some(vec![crate::persistent_store::RootMutation::Set{key:"synthetic".into(),value:json!(true)}]),
        ..Default::default()
    }).map_err(pds_error)?;
    Ok(())
}

#[cfg(test)]
fn write_before_commit_at(root:&Path) {
    WRITES_BEFORE_COMMIT.lock().unwrap().insert(std::fs::canonicalize(root).unwrap());
}

#[cfg(test)]
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
enum CommitFailure {BeforeStore,AfterStore}

#[cfg(test)]
static COMMIT_FAILURES:std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf,CommitFailure>>=std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Fails a restore commit of `root` once at `point`, after the job recorded its commit: before
/// the library received it, as a rejected replacement does, or after the library committed it.
#[cfg(test)]
fn commit_failure(root:&Path,point:CommitFailure)->Result<()> {
    let root=std::fs::canonicalize(root).unwrap_or_else(|_|root.to_owned());
    let mut failures=COMMIT_FAILURES.lock().unwrap();
    if failures.get(&root)!=Some(&point) {return Ok(());}
    failures.remove(&root);
    Err(corrupt())
}

#[cfg(test)]
fn fail_commit_at(root:&Path,point:CommitFailure) {
    COMMIT_FAILURES.lock().unwrap().insert(std::fs::canonicalize(root).unwrap(),point);
}

#[cfg(test)]
pub(crate) fn activate_database_first_backup(
    store:&mut PersistentStore,state:&PersistentStoreState,job:&DurableJob,
    prepared:snapshot_restore::DatabaseFirstSnapshot,sections:Vec<super::sections::CapturedSection>,
    cancel:Cancellation,permit:crate::native_file_jobs::admission::Permit,
)->Result<(Value,crate::native_file_jobs::admission::Permit)> {
    activate_database_first_backup_observed(store,state,job,prepared,sections,cancel,permit,&|_|{})
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn activate_database_first_backup_observed(
    store:&mut PersistentStore,state:&PersistentStoreState,job:&DurableJob,
    prepared:snapshot_restore::DatabaseFirstSnapshot,sections:Vec<super::sections::CapturedSection>,
    cancel:Cancellation,permit:crate::native_file_jobs::admission::Permit,report:&dyn Fn(Option<StagingProgress>),
)->Result<(Value,crate::native_file_jobs::admission::Permit)> {
    let expected=job.request.target_revision.as_deref().ok_or_else(corrupt)?.parse::<i64>().map_err(|_|corrupt())?;
    let selection=restore_selection(job.request.restore_areas.as_deref())?;
    require_restorable_sections(&selection,&prepared.snapshot,&job.admission_identity.store_id)?;
    let app_data_dir=store.repository_root().to_owned();
    prepare_local_restore_in_store(store,state,&app_data_dir,job,expected,prepared.snapshot,prepared.original_units,
        selection,sections,cancel,prepared.sources,prepared.present,permit,report)
}

#[cfg(test)]
pub(crate) async fn settle_database_first_backup(
    store:PersistentStore,connected:&ConnectedRepository,job:&DurableJob,
    adoption:RestoreAdoptionRequest,permit:&mut Option<crate::native_file_jobs::admission::Permit>,cancel:&Cancellation,
)->Result<Value> {
    if permit.is_none() {return Err(ProviderError::new(ErrorKind::PreconditionFailed));}
    let root=store.repository_root().to_owned();
    let result=confirm_restore_adoption_in_store(&store,&root,&adoption)?;
    drop(permit.take());
    receive_restore_bodies_in_store(store,&root,connected,job,result,adoption.selected_character_id,cancel).await
}

fn staging_directory(root: &Path, job: &DurableJob) -> std::path::PathBuf {
    runtime::job_directory(root, &job.request.connection_id, &job.id).join("restore-snapshot")
}

/// A finished restore never reads its download again, whether it succeeded,
/// failed or was cancelled.
pub(crate) fn discard_finished_staging(root: &Path, job: &DurableJob) {
    if job.request.kind == JobKind::Restore && job.terminal() {
        cleanup_staging(&staging_directory(root, job));
    }
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

#[cfg(test)]
pub(super) mod tests {
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
    fn original_control_admission_certifies_held_manifest_and_rejects_missing_tree_before_install() {
        use risunest_sync_wire::{descriptor::RecordDescriptor,unit::{UnitKey,UnitValue}};
        let directory=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(directory.path()).unwrap();
        let staging=directory.path().join("staging");
        let mut content=super::super::content_store::ContentStore::open(&staging.join("external-storage")).unwrap();
        let manifest=risunest_external_storage_format::message_pages::MessageManifest {
            schema:risunest_external_storage_format::message_pages::MANIFEST_SCHEMA.into(),message_count:0.into(),pages:Vec::new(),
        }.encode().unwrap();
        content.put(&manifest.hash,&manifest.bytes).unwrap();
        content.commit().unwrap();
        let units=std::collections::BTreeMap::from([(UnitKey::new(&["messages","missing-character","missing-conversation"]).unwrap(),UnitValue::object(RecordDescriptor::content(manifest.hash.clone())).unwrap())]);
        let mut snapshot=prepared(Some("source-device"));
        snapshot.staging_root=staging;
        snapshot.objects.push(snapshot_restore::PreparedObject {content_hash:manifest.hash.clone(),byte_length:manifest.bytes.len() as u64,source:super::super::content_store::ObjectSource::Captured(manifest.hash.clone())});
        let mut invalid_units=units.clone();
        let mut invalid=RecordDescriptor::content(risunest_sync_wire::hash(b"synthetic absent opaque payload"));
        invalid.dependency_root=Some("ab".repeat(32));
        invalid_units.insert(UnitKey::new(&["z-future-unit","missing-tree"]).unwrap(),UnitValue::object(invalid).unwrap());
        let (invalid_units,units)=(snapshot_restore::OriginalUnits::staged(&mut content,&invalid_units),snapshot_restore::OriginalUnits::staged(&mut content,&units));
        assert!(stage_original_backup_controls(&mut store,&snapshot,&invalid_units,&Cancellation::default(),&|_|{}).is_err());
        assert!(store.lww_object_body(&manifest.hash).unwrap().is_none());
        stage_original_backup_controls(&mut store,&snapshot,&units,&Cancellation::default(),&|_|{}).unwrap();
        assert_eq!(store.lww_object_body(&manifest.hash).unwrap(),Some(manifest.bytes));
    }

    #[test]
    fn original_control_admission_never_opens_present_opaque_payload() {
        use risunest_sync_wire::{descriptor::RecordDescriptor,unit::{UnitKey,UnitValue}};
        let directory=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(directory.path()).unwrap();
        let body=crate::asset_repository::PayloadCas::new(directory.path()).unwrap().prepare_bytes(b"synthetic present opaque payload").unwrap();
        let units=std::collections::BTreeMap::from([(UnitKey::new(&["future-unit","present-body"]).unwrap(),UnitValue::object(RecordDescriptor::content(body.content_hash.clone())).unwrap())]);
        let mut snapshot=prepared(Some("source-device"));
        snapshot.staging_root=directory.path().join("staging");
        snapshot.objects.push(snapshot_restore::PreparedObject {content_hash:body.content_hash.clone(),byte_length:body.byte_size,source:super::super::content_store::ObjectSource::Library(body.content_hash)});
        let units=snapshot_restore::OriginalUnits::staged(&mut super::super::content_store::ContentStore::open(&snapshot.staging_root.join("external-storage")).unwrap(),&units);
        crate::asset_repository::body_io::reset_body_io();
        stage_original_backup_controls(&mut store,&snapshot,&units,&Cancellation::default(),&|_|{}).unwrap();
        let observed=crate::asset_repository::body_io::take_body_io();
        assert!(observed.complete());
        assert_eq!(observed.asset_work(),Default::default());
        assert!(observed.objects.is_empty());
    }

    #[test]
    fn original_control_admission_accepts_large_archive_metadata_and_manifests() {
        let directory=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(directory.path()).unwrap();
        let staging=directory.path().join("staging");
        let mut content=super::super::content_store::ContentStore::open(&staging.join("external-storage")).unwrap();
        let (units,bodies,large)=crate::persistent_store::external_capture::large_metadata_fixture();
        let mut snapshot=prepared(Some("source-device"));snapshot.staging_root=staging;
        for (hash,bytes) in &bodies {
            content.put(hash,bytes).unwrap();
            snapshot.objects.push(snapshot_restore::PreparedObject {content_hash:hash.clone(),byte_length:bytes.len() as u64,source:super::super::content_store::ObjectSource::Captured(hash.clone())});
        }
        let archive:crate::persistent_store::archive::ArchivedObject=serde_json::from_slice(&bodies[&large[0]]).unwrap();
        for hash in archive.object_roots().collect::<std::collections::BTreeSet<_>>() {
            snapshot.objects.push(snapshot_restore::PreparedObject {content_hash:hash.to_owned(),byte_length:1,source:super::super::content_store::ObjectSource::Library(hash.to_owned())});
        }
        content.commit().unwrap();
        let units=snapshot_restore::OriginalUnits::staged(&mut content,&units);
        stage_original_backup_controls(&mut store,&snapshot,&units,&Cancellation::default(),&|_|{}).unwrap();
        for hash in &large {assert_eq!(store.lww_object_body(hash).unwrap().unwrap(),bodies[hash]);}
    }

    #[test]
    fn original_control_admission_stages_large_unit_bodies_above_the_metadata_bound() {
        use risunest_sync_wire::{descriptor::RecordDescriptor,unit::{UnitKey,UnitValue}};
        let directory=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(directory.path()).unwrap();
        let staging=directory.path().join("staging");
        let mut content=super::super::content_store::ContentStore::open(&staging.join("external-storage")).unwrap();
        let body=risunest_sync_wire::payload_value::encode(&json!("x".repeat(risunest_sync_wire::MAX_METADATA_BYTES))).unwrap();
        let padded=[b" ".as_slice(),&body].concat();
        let mut snapshot=prepared(Some("source-device"));
        snapshot.staging_root=staging;
        for bytes in [&body,&padded] {
            let hash=risunest_sync_wire::hash(bytes);
            content.put(&hash,bytes).unwrap();
            snapshot.objects.push(snapshot_restore::PreparedObject {content_hash:hash.clone(),byte_length:bytes.len() as u64,source:super::super::content_store::ObjectSource::Captured(hash)});
        }
        content.commit().unwrap();
        let mut unit=|key:&[&str],bytes:&[u8]| snapshot_restore::OriginalUnits::staged(&mut content,&std::collections::BTreeMap::from([(UnitKey::new(key).unwrap(),UnitValue::object(RecordDescriptor::content(risunest_sync_wire::hash(bytes))).unwrap())]));
        let (padded_units,body_units)=(unit(&["root","additionalPrompt"],&padded),unit(&["root","additionalPrompt"],&body));
        assert!(stage_original_backup_controls(&mut store,&snapshot,&padded_units,&Cancellation::default(),&|_|{}).is_err());
        assert!(store.lww_object_body(&risunest_sync_wire::hash(&body)).unwrap().is_none());
        crate::persistent_store::hash_work::reset_hash_work();
        stage_original_backup_controls(&mut store,&snapshot,&body_units,&Cancellation::default(),&|_|{}).unwrap();
        let work=crate::persistent_store::hash_work::take_hash_work();
        assert_eq!(work.domains["native_backup_large_unit_source"].calls,1);
        assert!(store.lww_object_body(&risunest_sync_wire::hash(&body)).unwrap()==Some(body));
    }

    fn commit_fixture(store:&mut PersistentStore,root:&Path,job:&DurableJob) -> DurableJob {
        use risunest_external_storage_format::section::SectionKind;
        JobStore::open(root).unwrap().put(job).unwrap();
        let source_dir=tempfile::tempdir().unwrap();
        let mut source=PersistentStore::open(source_dir.path()).unwrap();
        source.device_store_mut().unwrap().write_setting("accountst",&json!("synthetic restored setting")).unwrap();
        let sections=source.device_store_mut().unwrap().capture_backup_sections(&[SectionKind::Hypa,SectionKind::LocalPlugins,SectionKind::LocalSettings],source_dir.path()).unwrap();
        let stage=store.replace_begin().unwrap();
        store.replace_put_root(&stage.staging_id,&json!({"language":"synthetic restored language"})).unwrap();
        let header=crate::persistent_store::lww::Header {binding_authority:store.lww_binding_authority().unwrap(),request_id:format!("external-backup-restore:{}",job.id)};
        let intent=restore_intent(job,0,header,&stage.staging_id).unwrap();
        persist_restore_intent(root,job,&intent).unwrap();
        let pending=JobStore::open(root).unwrap().read(&job.id).unwrap();
        assert!(completed_restore_in_store(store,&pending).unwrap().is_none());
        store.lww_commit_replacement_with_device_sections(&intent.header,&intent.staging_id,Some(&Default::default()),&sections.iter().collect::<Vec<_>>()).unwrap();
        assert_eq!(store.device_store().unwrap().read_setting("accountst").unwrap(),Some(json!("synthetic restored setting")));
        pending
    }

    /// A data root of `length` characters, as a user name of a different
    /// length would make it.
    pub(crate) fn padded_root(base:&Path,length:usize)->std::path::PathBuf {
        let label=length.to_string();
        let used=base.to_string_lossy().chars().count()+1+label.len();
        let root=base.join(format!("{label}{}","r".repeat(length.saturating_sub(used))));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Both databases sit below the job directory, where Windows still limits
    /// the path SQLite opens to MAX_PATH.
    #[test]
    fn restore_staging_and_transfer_receipts_open_below_a_long_data_root() {
        let base=tempfile::tempdir().unwrap();
        for length in [47,90] {
            let root=padded_root(base.path(),length);
            let input=serde_json::from_value(json!({"connectionId":"synthetic-connection","kind":"restore","snapshotId":"synthetic-snapshot","targetRevision":"0"})).unwrap();
            let job=DurableJob::new(input,1,identity());
            let staging=staging_directory(&root,&job).join("external-storage");
            let mut content=super::super::content_store::ContentStore::open(&staging).expect("open restore staging below a long root");
            content.put(&hex::encode(risunest_external_storage_format::content_identity::hash(b"synthetic")),b"synthetic").unwrap();
            content.commit().unwrap();
            super::super::journal::TransferJournal::open(&runtime::job_directory(&root,&job.request.connection_id,&job.id),
                super::super::journal::JobIdentity{job_id:job.id.clone(),connection_id:job.request.connection_id.clone(),
                    repository_id:"synthetic-repository".into(),capture_id:"synthetic-capture".into(),capture:identity()})
                .expect("open transfer receipts below a long root")
                .record_parent(&[]).expect("record the parent below a long root");
        }
    }

    #[test]
    fn renderer_adoption_accepts_exact_durable_restore_and_refuses_another_generation() {
        let root=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(root.path()).unwrap();
        let input=serde_json::from_value(json!({"connectionId":"synthetic-connection","kind":"restore","snapshotId":"synthetic-snapshot","targetRevision":"0"})).unwrap();
        let job=DurableJob::new(input,1,store.external_identity().unwrap());
        let pending=commit_fixture(&mut store,root.path(),&job);
        let receipt=completed_restore_in_store(&store,&pending).unwrap().unwrap();
        let request=RestoreAdoptionRequest{job_id:job.id.clone(),received_revision:receipt["receivedRevision"].as_str().unwrap().into(),selected_character_id:None};
        assert_eq!(confirm_restore_adoption_in_store(&store,root.path(),&request).unwrap(),receipt);
        let adopted=JobStore::open(root.path()).unwrap().read(&job.id).unwrap();
        assert_eq!(confirm_restore_adoption_in_store(&store,root.path(),&request).unwrap(),receipt);
        assert_eq!(JobStore::open(root.path()).unwrap().read(&job.id).unwrap().summary,adopted.summary);
        let mut wrong=request.clone(); wrong.selected_character_id=Some("synthetic-missing-character".into());
        assert_eq!(confirm_restore_adoption_in_store(&store,root.path(),&wrong).unwrap_err().kind,ErrorKind::PreconditionFailed);
        let mut wrong=request.clone(); wrong.received_revision="01".into();
        assert_eq!(confirm_restore_adoption_in_store(&store,root.path(),&wrong).unwrap_err().kind,ErrorKind::Corrupt);
        wrong.received_revision="2".into();
        assert_eq!(confirm_restore_adoption_in_store(&store,root.path(),&wrong).unwrap_err().kind,ErrorKind::PreconditionFailed);
        let stage=store.replace_begin().unwrap();
        store.replace_put_root(&stage.staging_id,&json!({"language":"synthetic later replacement"})).unwrap();
        store.replace_commit(&stage.staging_id,Some(1)).unwrap();
        assert_eq!(confirm_restore_adoption_in_store(&store,root.path(),&request).unwrap_err().kind,ErrorKind::PreconditionFailed);
    }

    #[test]
    fn identical_adoption_survives_ordinary_selected_character_deletion() {
        let root=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(root.path()).unwrap();
        let input=serde_json::from_value(json!({"connectionId":"synthetic-connection","kind":"restore","snapshotId":"synthetic-snapshot","targetRevision":"0"})).unwrap();
        let job=DurableJob::new(input,1,store.external_identity().unwrap());
        let pending=commit_fixture(&mut store,root.path(),&job);
        let receipt=completed_restore_in_store(&store,&pending).unwrap().unwrap();
        store.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:1,
            add_character:Some(json!({"chaId":"synthetic-selected","type":"character","name":"Synthetic selected","chats":[]})),
            ..Default::default()}).unwrap();
        let request=RestoreAdoptionRequest{job_id:job.id.clone(),received_revision:receipt["receivedRevision"].as_str().unwrap().into(),selected_character_id:Some("synthetic-selected".into())};
        assert_eq!(confirm_restore_adoption_in_store(&store,root.path(),&request).unwrap(),receipt);
        store.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:2,
            delete_character_ids:Some(vec!["synthetic-selected".into()]),..Default::default()}).unwrap();
        assert!(store.read_character_summary("synthetic-selected",None).unwrap().is_none());
        assert_eq!(confirm_restore_adoption_in_store(&store,root.path(),&request).unwrap(),receipt);
    }

    #[test]
    fn local_application_evidence_survives_reopen_and_later_phase_updates() {
        let root=tempfile::tempdir().unwrap();
        let input=serde_json::from_value(json!({"connectionId":"synthetic-connection","kind":"restore","snapshotId":"synthetic-snapshot","targetRevision":"0"})).unwrap();
        let job=DurableJob::new(input,1,identity());
        JobStore::open(root.path()).unwrap().put(&job).unwrap();
        mark_application_started(root.path(),&job).unwrap();
        update_phase(root.path(),&job,"downloading").unwrap();
        let mut store=PersistentStore::open(root.path()).unwrap();
        let pending=commit_fixture(&mut store,root.path(),&job);
        let receipt=completed_restore_in_store(&store,&pending).unwrap().unwrap();
        update_phase(root.path(),&pending,"settling-bodies").unwrap();
        drop(store);
        let reopened=PersistentStore::open(root.path()).unwrap();
        let durable=JobStore::open(root.path()).unwrap().read(&job.id).unwrap();
        assert!(application_started(&durable));
        assert_eq!(durable.summary["phase"],"settling-bodies");
        for _ in 0..2 {assert_eq!(completed_restore_in_store(&reopened,&durable).unwrap(),Some(receipt.clone()));}
        assert_eq!(reopened.revision().unwrap().to_string(),receipt["receivedRevision"].as_str().unwrap());
    }

    #[test]
    fn restore_area_selection_matches_the_full_renderer_contract() {
        let areas=FULL_RESTORE_AREAS.map(str::to_owned);
        assert_eq!(restore_selection(None).unwrap(),restore_selection(Some(&areas)).unwrap());
        assert_eq!(restore_selection(None).unwrap().sections.len(),3);
    }
    #[test]
    fn restore_area_selection_rejects_partial_duplicate_and_undeclared_areas() {
        for areas in [vec!["library".into()],vec!["hypa".into()],vec!["devicePlugins".into()],vec!["library".into(),"library".into()],vec![]] {assert!(restore_selection(Some(&areas)).is_err());}
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

    #[test]
    fn complete_backup_restores_device_sections_across_devices_but_periodic_state_does_not() {
        let selection=restore_selection(None).unwrap();
        assert!(require_restorable_sections(&selection,&prepared(Some("other-device")),"this-device").is_ok());
        assert!(require_restorable_sections(&selection,&prepared(None),"this-device").is_err());
        assert!(require_restorable_sections(&selection,&prepared(Some("")),"this-device").is_err());
    }
    #[test]
    fn library_restore_receipt_recovers_only_the_exact_durable_request() {
        let root=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(root.path()).unwrap();
        let request=serde_json::from_value(json!({"connectionId":"synthetic-connection","kind":"restore","snapshotId":"synthetic-snapshot","targetRevision":"0"})).unwrap();
        let job=DurableJob::new(request,1,identity());
        assert!(completed_restore_in_store(&store,&job).unwrap().is_none());
        let pending=commit_fixture(&mut store,root.path(),&job);
        assert!(completed_restore_in_store(&store,&pending).unwrap().is_some());
        let mut different=pending.clone();
        different.request.snapshot_id=Some("different-snapshot".into());
        assert!(completed_restore_in_store(&store,&different).is_err());
        different=pending.clone();
        different.summary["restoreCommit"]["stagingId"]=json!("different-stage");
        assert!(matches!(completed_restore_in_store(&store,&different),Err(ProviderError{kind:ErrorKind::Corrupt,..})));
        different=pending.clone();
        different.summary["restoreCommit"]["header"]["bindingAuthority"]=json!("1");
        assert!(matches!(completed_restore_in_store(&store,&different),Err(ProviderError{kind:ErrorKind::Corrupt,..})));
        store.set_app_kv(&format!("external-restore-commit:{}",job.id),&json!({"receivedRevision":"999"})).unwrap();
        assert!(completed_restore_in_store(&store,&job).unwrap().is_none());
    }

    fn synthetic_connection(repository:&super::super::contract::RepositoryHandle) -> super::super::connection_store::StoredConnection {
        use super::super::{contract::{ConnectionConfig,RemoteLocator},fake};
        super::super::connection_store::StoredConnection {
            id:"synthetic-connection".into(),
            config:ConnectionConfig{provider:"synthetic".into(),profile:None,endpoint:"https://synthetic.invalid".into(),
                account_id:"account".into(),location:Default::default(),oauth_profile:None},
            descriptor:risunest_external_storage_format::format::Descriptor::new("format-repository".into(),
                Some(risunest_external_storage_format::format::Strategy::Cas)).unwrap(),
            descriptor_locator:RemoteLocator{connection_identity:repository.connection_identity.clone(),collection:None,object:"descriptor".into()},
            provider_repository_id:repository.repository_id.clone(),credential_ref:"credential".into(),root_key_ref:"key".into(),
            recovery_key_ref:"recovery".into(),retention_policy:None,transfer_concurrency:None,labels:Default::default(),capabilities:fake::capabilities(true),
            created_at_ms:1,verified_at_ms:1,last_sync_at_ms:None,last_backup_at_ms:None,
        }
    }

    fn sweep_journals(store:&PersistentStore) {
        let native_jobs=|| -> std::result::Result<Vec<crate::native_file_jobs::JobStatus>,String> {Ok(Vec::new())};
        let open_store=|| store.open_native_job_store().map_err(|error| error.to_string());
        crate::asset_repository::commands::DurableCasJobState::default().sweep_after_page_start(store.repository_root(),
            &crate::asset_repository::commands::CasJobOwnerProbe{native_jobs:&native_jobs,device_job_owned:&|_|Ok(false),external_job_active:&|_|Ok(false),open_store:&open_store}).unwrap();
    }

    #[test]
    fn a_restore_journal_outlives_a_restart_only_while_the_restore_can_still_finish() {
        let directory=tempfile::tempdir().unwrap();
        let root=directory.path();
        let mut store=PersistentStore::open(root).unwrap();
        let open_store=|| PersistentStore::open(root).map_err(|error| error.to_string());
        let ended=|id:&str,here:bool| restore_journal_owner_ended(root,id,here,&open_store).unwrap();
        let input=serde_json::from_value(json!({"connectionId":"synthetic-connection","kind":"restore","snapshotId":"synthetic-snapshot","targetRevision":"0"})).unwrap();
        let job=DurableJob::new(input,1,store.external_identity().unwrap());
        assert!(!ended(&job.id,true));
        assert!(ended(&job.id,false));
        JobStore::open(root).unwrap().put(&job).unwrap();
        assert!(!ended(&job.id,true));
        assert!(ended(&job.id,false));
        mark_application_started(root,&job).unwrap();
        assert!(!ended(&job.id,false));
        ConnectionStore::open(root).unwrap().insert(&synthetic_connection(&super::super::fake::repository())).unwrap();
        assert!(!ended(&job.id,false));
        commit_fixture(&mut store,root,&job);
        assert!(!ended(&job.id,false));
        ConnectionStore::open(root).unwrap().remove(&job.request.connection_id).unwrap();
        assert!(!ended(&job.id,false), "connection removal does not settle an application receipt");
        let stage=store.replace_begin().unwrap();
        store.replace_put_root(&stage.staging_id,&json!({"language":"synthetic later replacement"})).unwrap();
        store.replace_commit(&stage.staging_id,Some(1)).unwrap();
        assert!(ended(&job.id,false));
        assert!(!ended(&job.id,true));
        let jobs=JobStore::open(root).unwrap();
        let mut failed=jobs.read(&job.id).unwrap();
        failed.summary["state"]=json!("failed");
        jobs.put(&failed).unwrap();
        assert!(!ended(&job.id,true));
        assert!(ended(&job.id,false));
    }

    #[test]
    fn stopping_an_unfinished_restore_ends_it_and_keeps_the_library() {
        use crate::asset_repository::job_pins::durable_cas_job_ids;
        let directory=tempfile::tempdir().unwrap();
        let root=directory.path();
        let mut store=PersistentStore::open(root).unwrap();
        let input=serde_json::from_value(json!({"connectionId":"synthetic-connection","kind":"restore","snapshotId":"synthetic-snapshot","targetRevision":"0"})).unwrap();
        let job=DurableJob::new(input,1,store.external_identity().unwrap());
        assert_eq!(stop_restore(root,&job.id).err().unwrap().kind,ErrorKind::NotFound);
        commit_fixture(&mut store,root,&job);
        assert_eq!(stop_restore(root,&job.id).err().unwrap().kind,ErrorKind::PreconditionFailed);
        let jobs=JobStore::open(root).unwrap();
        let mut unfinished=jobs.read(&job.id).unwrap();
        unfinished.summary["state"]=json!("uncertain");
        unfinished.summary["phase"]=json!("local-apply-unknown");
        jobs.put(&unfinished).unwrap();
        let cas=crate::asset_repository::PayloadCas::new(root).unwrap();
        let received=cas.prepare_bytes(b"synthetic restored body").unwrap();
        let mut pins=restore_pins(root,&job.id).unwrap();
        pins.pin_existing(&cas,&received.content_hash,received.byte_size,crate::asset_repository::job_pins::CasObjectRole::DirectObject).unwrap();
        drop(pins);
        assert!(store.asset_residency_status().is_err());
        let revision=store.revision().unwrap();
        let library=store.read_root(None).unwrap().value;

        let stopped=stop_restore(root,&job.id).unwrap();

        assert_eq!(stopped.summary["state"],"failed");
        assert!(restore_stopped(&stopped));
        assert_eq!(jobs.read(&job.id).unwrap().summary,stopped.summary);
        assert!(durable_cas_job_ids(root).unwrap().is_empty());
        assert!(store.asset_residency_status().is_ok());
        assert_eq!(store.revision().unwrap(),revision);
        assert_eq!(store.read_root(None).unwrap().value,library);
        assert_eq!(stop_restore(root,&job.id).err().unwrap().kind,ErrorKind::PreconditionFailed);
    }

    /// A full backup of a library holding `assets`, uploaded to a fake repository.
    pub(in crate::external_storage) struct PackagedBackup {
        _source_root:Option<tempfile::TempDir>,
        _work:tempfile::TempDir,
        pub(in crate::external_storage) connected:ConnectedRepository,
        backup_id:String,
        provider:std::sync::Arc<super::super::fake::FakeProvider>,
        pub(in crate::external_storage) restore_source:Value,
        assets:Vec<String>,
    }

    async fn packaged_backup(assets:&[Vec<u8>]) -> PackagedBackup {
        packaged_backup_seeded(assets,|_|{}).await
    }

    /// A packaged backup whose source device `seed` prepared first.
    pub(in crate::external_storage) async fn packaged_backup_seeded(assets:&[Vec<u8>],seed:impl FnOnce(&mut PersistentStore)) -> PackagedBackup {
        let source_root=tempfile::tempdir().unwrap();
        let mut source=PersistentStore::open(source_root.path()).unwrap();
        seed(&mut source);
        let assets=assets.iter().enumerate().map(|(index,bytes)| crate::server_sync::lww_tests::put_asset(&mut source,&format!("assets/synthetic-restore-{index}.bin"),bytes).object_hash.unwrap()).collect::<Vec<_>>();
        let mut backup=uploaded_backup(&mut source,assets).await;
        backup._source_root=Some(source_root);
        backup
    }

    /// A full backup of `source` as it is now, uploaded to a fake repository.
    async fn uploaded_backup(source:&mut PersistentStore,assets:Vec<String>) -> PackagedBackup {
        uploaded_backup_with_legacy_units(source,assets,Default::default()).await
    }

    async fn uploaded_backup_with_legacy_units(source:&mut PersistentStore,assets:Vec<String>,
        legacy_units:std::collections::BTreeMap<risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue>,
    ) -> PackagedBackup {
        use super::super::{fake,journal::{JobIdentity,TransferJournal},packaging,phase_progress::PhaseProgress};
        use std::sync::Arc;
        let source_root=source.repository_root().to_owned();
        let probe=runtime::CancelProbe(Cancellation::default());
        let hydration=source.hydrate_external_capture_dependencies("sender",&probe).unwrap();
        let (lease,prepared)=source.lww_acquire_backup_capture(source.revision().unwrap()).unwrap();
        let sections=super::super::sections::capture_prepared_backup_sections(&prepared,&source_root.join("backup-sections"),&probe.0).unwrap();
        let capture=source.capture_external_library_from_lease_with_sections("sender",&hydration,&lease.lease,sections,&probe).unwrap();
        let sections=capture.catalog.backup_sections(&source_root).unwrap();
        let original_units=capture.catalog.original_backup_units().unwrap();
        let original_units=if legacy_units.is_empty() {original_units} else {
            let mut units=original_units.units().unwrap().collect::<crate::persistent_store::StoreResult<std::collections::BTreeMap<_,_>>>().unwrap();
            for (key,value) in legacy_units {assert!(units.insert(key,value).is_none());}
            units.into()
        };
        let repository=fake::repository();
        let provider=Arc::new(fake::FakeProvider::new(false));
        let connected=ConnectedRepository {
            stored:synthetic_connection(&repository),provider:provider.clone(),handle:repository,
            dependencies:fake::loopback_dependencies(fake::MemoryVault::default(),1).dependencies,root_key:zeroize::Zeroizing::new([21;32]),
        };
        let backup_id=uuid::Uuid::new_v4().to_string();
        let work=tempfile::tempdir().unwrap();
        let mut transfer=TransferJournal::open(&work.path().join("backup-journal"),JobIdentity {
            job_id:backup_id.clone(),connection_id:connected.stored.id.clone(),repository_id:connected.handle.repository_id.clone(),
            capture_id:capture.id.clone(),capture:capture.identity.clone(),
        }).unwrap();
        let metadata=packaging::SnapshotMetadata {
            snapshot_id:backup_id.clone(),repository_id:connected.stored.descriptor.repository_id.clone(),
            library_id:capture.identity.library_epoch.clone(),author_device_id:capture.identity.store_id.clone(),
            created_at_ms:runtime::now_ms(),logical_revision:capture.identity.revision as u64,parent_snapshot_id:None,
            content_fingerprint:capture.catalog.content_fingerprint(&risunest_external_storage_format::format::library_fingerprint_domain()).unwrap(),
            purpose:packaging::SnapshotPurpose::BackupBundle {
                source:risunest_external_storage_format::control::BundleSource::Device{writer_id:source.lww_clock_state().unwrap().writer_id},
                remote_generation:None,original_units,
            },
        };
        let backup=packaging::package_and_upload(capture,sections,&source_root,&work.path().join("cache"),metadata,
            &connected.root_key,packaging::PackageLimits::from_capabilities(&connected.stored.capabilities).unwrap(),None,&mut transfer,
            connected.provider.as_ref(),&connected.handle,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap();
        source.release_revision(&lease.lease).unwrap();
        let restore_source=serde_json::to_value(backup.reference.stored(&connected.handle).unwrap()).unwrap();
        PackagedBackup {_source_root:None,_work:work,connected,backup_id,provider,restore_source,assets}
    }

    /// A restore of `backup` admitted against the library at `root`.
    fn restore_job(root:&Path,store:&PersistentStore,backup:&PackagedBackup) -> DurableJob {
        let request=serde_json::from_value(json!({"connectionId":"synthetic-connection","kind":"restore","snapshotId":backup.backup_id,"targetRevision":"0"})).unwrap();
        let mut job=DurableJob::new(request,1,store.external_identity().unwrap());
        job.summary["restoreSource"]=backup.restore_source.clone();
        JobStore::open(root).unwrap().put(&job).unwrap();
        job
    }

    #[test]
    fn an_interrupted_restore_preparation_never_leaves_a_catalog_row_without_its_body_or_source() {
        use std::sync::Arc;
        for point in [RegistrationCrash::AfterFirst,RegistrationCrash::AfterSecond] {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let backup=packaged_backup(&[vec![41;4096],vec![42;8192]]).await;
                let destination=tempfile::tempdir().unwrap();
                let root=destination.path();
                let mut store=PersistentStore::open(root).unwrap();
                ConnectionStore::open(root).unwrap().insert(&backup.connected.stored).unwrap();
                let job=restore_job(root,&store,&backup);
                let (database,sections)=prepare_database_first_backup(root,&backup.connected,&job,&Cancellation::default()).await.unwrap();
                assert_eq!(database.missing.len(),backup.assets.len());
                let admission=Arc::new(crate::native_file_jobs::admission::Admission::default());
                let state=PersistentStoreState::default();
                let crash=crash_registration_at(root,point);
                assert!(activate_database_first_backup(&mut store,&state,&job,database,sections,Cancellation::default(),admission.staging().unwrap()).is_err());
                drop(crash);
                let cas=crate::asset_repository::PayloadCas::new(root).unwrap();
                for asset in &backup.assets {assert!(cas.stat_object(asset).unwrap().is_none());}
                store.asset_gc_dry_run(1024,None,runtime::now_ms() as i64,0).unwrap();
            });
        }
    }

    #[test]
    fn receiving_restored_bodies_opens_the_restore_journal_once() {
        use crate::asset_repository::job_pins::journal_opens;
        use std::sync::Arc;
        // Three source pages: two full ones and a short last one.
        super::super::job_store::TEST_RESTORE_BODY_PAGE.with(|page| page.set(16));
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let bodies=(0..40u16).map(|index| {
                let mut body=index.to_le_bytes().to_vec();
                body.resize(1024+usize::from(index%64),(index%251) as u8);
                body
            }).collect::<Vec<_>>();
            let backup=packaged_backup(&bodies).await;
            let destination=tempfile::tempdir().unwrap();
            let root=destination.path();
            let mut store=PersistentStore::open(root).unwrap();
            ConnectionStore::open(root).unwrap().insert(&backup.connected.stored).unwrap();
            let job=restore_job(root,&store,&backup);
            let (database,sections)=prepare_database_first_backup(root,&backup.connected,&job,&Cancellation::default()).await.unwrap();
            assert_eq!(database.missing.len(),bodies.len());
            let packs=database.sources.iter().flat_map(|source|source.packs.iter().map(|pack|pack.header.object_id.clone())).collect::<BTreeSet<_>>();
            let reads=packs.iter().map(|pack|(pack.clone(),backup.provider.read_attempts(pack))).collect::<std::collections::BTreeMap<_,_>>();
            let admission=Arc::new(crate::native_file_jobs::admission::Admission::default());
            let state=PersistentStoreState::default();
            let (receipt,permit)=activate_database_first_backup(&mut store,&state,&job,database,sections,Cancellation::default(),admission.staging().unwrap()).unwrap();
            assert_eq!(store.replacement_source_row_total().unwrap(),0);
            // Bodies that arrive locally before the transfer are pinned in place, the rest are downloaded.
            let local=crate::asset_repository::PayloadCas::new(root).unwrap();
            for body in bodies.iter().step_by(3) {local.prepare_bytes(body).unwrap();}
            let before=journal_opens(root,&job.id);
            let adoption=RestoreAdoptionRequest{job_id:job.id.clone(),received_revision:receipt["receivedRevision"].as_str().unwrap().into(),selected_character_id:None};
            settle_database_first_backup(store,&backup.connected,&job,adoption,&mut Some(permit),&Cancellation::default()).await.unwrap();
            assert_eq!(journal_opens(root,&job.id)-before,1);
            let cas=crate::asset_repository::PayloadCas::new(root).unwrap();
            for (asset,body) in backup.assets.iter().zip(&bodies) {assert_eq!(cas.read_object(asset).unwrap().unwrap(),*body);}
            for (pack,before) in reads {assert_eq!(backup.provider.read_attempts(&pack)-before,1,"a shared restore pack is read once across every source page");}
            assert!(!root.join("native-file-jobs/jobs").join(&job.id).join("external-restore-bodies").exists());
        });
    }

    #[test]
    fn a_page_reload_during_a_restore_keeps_its_journal_until_the_restore_settles() {
        use super::super::{fake,journal::{JobIdentity,TransferJournal},packaging,phase_progress::PhaseProgress};
        use crate::asset_repository::job_pins::durable_cas_job_ids;
        use std::sync::Arc;
        let source_root=tempfile::tempdir().unwrap();
        let mut source=PersistentStore::open(source_root.path()).unwrap();
        let asset=crate::server_sync::lww_tests::put_asset(&mut source,"assets/synthetic-restore.bin",&[57;4096]).object_hash.unwrap();
        let probe=runtime::CancelProbe(Cancellation::default());
        let hydration=source.hydrate_external_capture_dependencies("sender",&probe).unwrap();
        let (lease,prepared)=source.lww_acquire_backup_capture(source.revision().unwrap()).unwrap();
        let sections=super::super::sections::capture_prepared_backup_sections(&prepared,&source_root.path().join("backup-sections"),&probe.0).unwrap();
        let capture=source.capture_external_library_from_lease_with_sections("sender",&hydration,&lease.lease,sections,&probe).unwrap();
        let sections=capture.catalog.backup_sections(source_root.path()).unwrap();
        let original_units=capture.catalog.original_backup_units().unwrap();
        let repository=fake::repository();
        let connected=ConnectedRepository {
            stored:synthetic_connection(&repository),provider:Arc::new(fake::FakeProvider::new(false)),handle:repository,
            dependencies:fake::loopback_dependencies(fake::MemoryVault::default(),1).dependencies,root_key:zeroize::Zeroizing::new([21;32]),
        };
        let backup_id=uuid::Uuid::new_v4().to_string();
        let work=tempfile::tempdir().unwrap();
        let mut transfer=TransferJournal::open(&work.path().join("backup-journal"),JobIdentity {
            job_id:backup_id.clone(),connection_id:connected.stored.id.clone(),repository_id:connected.handle.repository_id.clone(),
            capture_id:capture.id.clone(),capture:capture.identity.clone(),
        }).unwrap();
        let metadata=packaging::SnapshotMetadata {
            snapshot_id:backup_id.clone(),repository_id:connected.stored.descriptor.repository_id.clone(),
            library_id:capture.identity.library_epoch.clone(),author_device_id:capture.identity.store_id.clone(),
            created_at_ms:runtime::now_ms(),logical_revision:capture.identity.revision as u64,parent_snapshot_id:None,
            content_fingerprint:capture.catalog.content_fingerprint(&risunest_external_storage_format::format::library_fingerprint_domain()).unwrap(),
            purpose:packaging::SnapshotPurpose::BackupBundle {
                source:risunest_external_storage_format::control::BundleSource::Device{writer_id:source.lww_clock_state().unwrap().writer_id},
                remote_generation:None,original_units,
            },
        };
        let destination=tempfile::tempdir().unwrap();
        let root=destination.path();
        let mut store=PersistentStore::open(root).unwrap();
        ConnectionStore::open(root).unwrap().insert(&connected.stored).unwrap();
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let backup=packaging::package_and_upload(capture,sections,source_root.path(),&work.path().join("cache"),metadata,
                &connected.root_key,packaging::PackageLimits::from_capabilities(&connected.stored.capabilities).unwrap(),None,&mut transfer,
                connected.provider.as_ref(),&connected.handle,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap();
            let request=serde_json::from_value(json!({"connectionId":"synthetic-connection","kind":"restore","snapshotId":backup_id,"targetRevision":"0"})).unwrap();
            let mut job=DurableJob::new(request,1,store.external_identity().unwrap());
            job.summary["restoreSource"]=serde_json::to_value(backup.reference.stored(&connected.handle).unwrap()).unwrap();
            JobStore::open(root).unwrap().put(&job).unwrap();
            let (database,sections)=prepare_database_first_backup(root,&connected,&job,&Cancellation::default()).await.unwrap();
            let admission=Arc::new(crate::native_file_jobs::admission::Admission::default());
            let state=PersistentStoreState::default();
            let (receipt,permit)=activate_database_first_backup(&mut store,&state,&job,database,sections,Cancellation::default(),admission.staging().unwrap()).unwrap();
            assert_eq!(durable_cas_job_ids(root).unwrap(),[job.id.clone()]);
            sweep_journals(&store);
            assert_eq!(durable_cas_job_ids(root).unwrap(),[job.id.clone()]);
            let adoption=RestoreAdoptionRequest{job_id:job.id.clone(),received_revision:receipt["receivedRevision"].as_str().unwrap().into(),selected_character_id:None};
            settle_database_first_backup(store,&connected,&job,adoption,&mut Some(permit),&Cancellation::default()).await.unwrap();
        });
        assert!(durable_cas_job_ids(root).unwrap().is_empty());
        assert!(crate::asset_repository::PayloadCas::new(root).unwrap().stat_object(&asset).unwrap().is_some());
        assert!(PersistentStore::open(root).unwrap().asset_residency_status().is_ok());
    }

    /// A running app whose renderer has its store open at `root`.
    fn restore_app(root:&Path)->tauri::App<tauri::test::MockRuntime> {
        let app=tauri::test::mock_builder().build(tauri::test::mock_context(tauri::test::noop_assets())).unwrap();
        app.manage(PersistentStoreState::with_test_store(PersistentStore::open(root).unwrap()));
        app.manage(crate::native_file_jobs::NativeFileJobState::initialize(root.join("native-file-jobs")));
        let jobs=super::super::job_store::JobCommandState::default();
        jobs.root.set(root.to_owned()).unwrap();
        app.manage(jobs);
        app
    }

    /// Commits a restore of `backup` in the running `app` as its worker does.
    async fn commit_in_session(app:&tauri::App<tauri::test::MockRuntime>,backup:&PackagedBackup)
        ->(DurableJob,super::super::job_store::JobClaim,Value,crate::native_file_jobs::admission::Permit,PersistentStore) {
        let root=runtime::root(app.handle()).unwrap();
        ConnectionStore::open(&root).unwrap().insert(&backup.connected.stored).unwrap();
        let mut store=runtime::native_store(app.handle()).unwrap();
        let job=restore_job(&root,&store,backup);
        let (_,claim)=app.state::<super::super::job_store::JobCommandState>().claim(&job).unwrap();
        let (database,sections)=prepare_database_first_backup(&root,&backup.connected,&job,&Cancellation::default()).await.unwrap();
        let permit=app.state::<crate::native_file_jobs::NativeFileJobState>().admission.staging().unwrap();
        let (receipt,permit)=activate_database_first_backup(&mut store,&app.state::<PersistentStoreState>(),&job,database,sections,Cancellation::default(),permit).unwrap();
        (job,claim,receipt,permit,store)
    }

    #[test]
    fn a_restore_commit_leaves_the_renderer_store_open_on_the_restored_library() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let backup=packaged_backup(&[vec![43;4096]]).await;
            let destination=tempfile::tempdir().unwrap();
            let app=restore_app(destination.path());
            let (job,_claim,receipt,_permit,_store)=commit_in_session(&app,&backup).await;
            // Nothing on the renderer side has opened the store since the commit. The panel
            // lists only connections to real providers.
            ConnectionStore::open(destination.path()).unwrap().remove(&backup.connected.stored.id).unwrap();
            let state=runtime::external_storage_get_state(app.handle().clone()).unwrap();
            let listed=state["jobs"].as_array().unwrap().iter().find(|item|item["id"]==job.id.as_str()).unwrap();
            assert_eq!((listed["state"].as_str(),listed["phase"].as_str()),(Some("running"),Some("awaiting-adoption")));
            let renderer=runtime::native_store(app.handle()).unwrap();
            assert_eq!(renderer.revision().unwrap().to_string(),receipt["receivedRevision"].as_str().unwrap());
            let reported=runtime::external_storage_get_job(app.handle().clone(),job.id.clone()).unwrap();
            assert_eq!(reported["result"],receipt);
            assert_eq!((reported["state"].as_str(),reported["phase"].as_str()),(Some("running"),Some("awaiting-adoption")));
            assert_eq!(reported["applicationStarted"],true);
            // The maintenance a commit holds refuses the store, and the running worker's record still answers.
            let maintenance=app.state::<PersistentStoreState>().acquire_device_maintenance().unwrap();
            assert_eq!(runtime::external_storage_get_job(app.handle().clone(),job.id.clone()).unwrap(),reported);
            drop(maintenance);
        });
    }

    #[test]
    fn a_restore_that_fails_under_maintenance_leaves_the_renderer_store_open() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let backup=packaged_backup(&[vec![47;4096]]).await;
            let destination=tempfile::tempdir().unwrap();
            let root=destination.path();
            let app=restore_app(root);
            ConnectionStore::open(root).unwrap().insert(&backup.connected.stored).unwrap();
            let mut store=runtime::native_store(app.handle()).unwrap();
            let job=restore_job(root,&store,&backup);
            let (database,sections)=prepare_database_first_backup(root,&backup.connected,&job,&Cancellation::default()).await.unwrap();
            let permit=app.state::<crate::native_file_jobs::NativeFileJobState>().admission.staging().unwrap();
            write_before_commit_at(root);
            let Err(failed)=activate_database_first_backup(&mut store,&app.state::<PersistentStoreState>(),&job,database,sections,Cancellation::default(),permit) else {
                panic!("a library changed after staging was restored over");
            };
            assert_eq!(failed.kind,ErrorKind::PreconditionFailed);
            assert!(JobStore::open(root).unwrap().read(&job.id).unwrap().summary.get("applicationStarted").is_none());
            ConnectionStore::open(root).unwrap().remove(&backup.connected.stored.id).unwrap();
            runtime::external_storage_get_state(app.handle().clone()).unwrap();
            assert_eq!(runtime::native_store(app.handle()).unwrap().revision().unwrap(),1);
        });
    }

    #[test]
    fn a_restore_committed_in_session_succeeds_once_the_renderer_adopts_it() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let bodies=vec![vec![44;4096],vec![45;8192]];
            let backup=packaged_backup(&bodies).await;
            let destination=tempfile::tempdir().unwrap();
            let root=destination.path();
            let app=restore_app(root);
            let (job,claim,receipt,permit,store)=commit_in_session(&app,&backup).await;
            let cancel=Cancellation::default();
            let worker=finish_restore_bodies(app.handle(),&backup.connected,&job,receipt.clone(),Some(permit),store,&cancel);
            let renderer=async {
                assert_eq!(runtime::external_storage_get_job(app.handle().clone(),job.id.clone()).unwrap()["result"],receipt);
                let adoption=RestoreAdoptionRequest{job_id:job.id.clone(),received_revision:receipt["receivedRevision"].as_str().unwrap().into(),selected_character_id:None};
                external_storage_confirm_restore_adoption(app.handle().clone(),adoption).unwrap();
            };
            let (finished,())=tokio::join!(worker,renderer);
            assert_eq!(finished.unwrap(),receipt);
            drop(claim);
            let settled=runtime::external_storage_get_job(app.handle().clone(),job.id.clone()).unwrap();
            assert_eq!((settled["state"].as_str(),settled["phase"].as_str()),(Some("succeeded"),Some("complete")));
            assert_eq!(settled["result"],receipt);
            let cas=crate::asset_repository::PayloadCas::new(root).unwrap();
            for (asset,body) in backup.assets.iter().zip(&bodies) {assert_eq!(cas.read_object(asset).unwrap().unwrap(),*body);}
        });
    }

    /// The transfer a restore reports is what it downloads, and before the
    /// renderer adopts it the total already holds the asset bodies it receives after.
    #[test]
    fn a_restore_reports_every_byte_it_downloads() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let bodies=vec![vec![48;4096],vec![49;8192]];
            let backup=packaged_backup(&bodies).await;
            let destination=tempfile::tempdir().unwrap();
            let root=destination.path();
            let app=restore_app(root);
            let read=|| backup.provider.transferred_body_bytes().1;
            let before=read();
            let counters=|job:&str| {
                let summary=JobStore::open(root).unwrap().read(job).unwrap().summary;
                assert_eq!(summary["counters"],"transferred");
                let value=|key:&str| summary[key].as_str().unwrap().parse::<u64>().unwrap();
                (value("completedBytes"),value("totalBytes"))
            };
            let (job,claim,receipt,permit,store)=commit_in_session(&app,&backup).await;
            let (downloaded,total)=counters(&job.id);
            assert_eq!(downloaded,read()-before);
            assert!(total>downloaded,"the asset bodies are still to come: {downloaded} of {total}");
            let cancel=Cancellation::default();
            let worker=finish_restore_bodies(app.handle(),&backup.connected,&job,receipt.clone(),Some(permit),store,&cancel);
            let renderer=async {
                let adoption=RestoreAdoptionRequest{job_id:job.id.clone(),received_revision:receipt["receivedRevision"].as_str().unwrap().into(),selected_character_id:None};
                external_storage_confirm_restore_adoption(app.handle().clone(),adoption).unwrap();
            };
            let (finished,())=tokio::join!(worker,renderer);
            assert_eq!(finished.unwrap(),receipt);
            drop(claim);
            assert_eq!(counters(&job.id),(read()-before,total));
            let settled=runtime::external_storage_get_job(app.handle().clone(),job.id.clone()).unwrap();
            assert_eq!(settled["state"],"succeeded");
            assert_eq!((settled["completedBytes"].as_str(),settled["totalBytes"].as_str()),(Some(total.to_string().as_str()),Some(total.to_string().as_str())));
        });
    }

    /// A restore reads every catalog it needs before it fetches a pack, so its
    /// transfer is planned once across the original units, the records, the
    /// device sections and the asset bodies it receives after adoption. The
    /// first reading already carries the total the restore ends at, no reading
    /// runs past it, and no object is read twice to get there.
    #[test]
    fn a_restore_plans_its_whole_transfer_before_the_first_pack() {
        use super::super::phase_progress::{PhaseCounters,PhaseProgress};
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let backup=packaged_backup_seeded(&[vec![51;4096],vec![52;8192]],|store| {
                store.device_store_mut().unwrap().write_setting("accountst",&json!("synthetic-planned")).unwrap();
            }).await;
            let destination=tempfile::tempdir().unwrap();
            let root=destination.path();
            let store=PersistentStore::open(root).unwrap();
            let job=admit(root,&store,&backup);
            let job=JobStore::open(root).unwrap().read(&job.id).unwrap();
            let stored:risunest_external_storage_format::snapshot::StoredObject=serde_json::from_value(backup.restore_source.clone()).unwrap();
            let remote=super::super::packaging::RemoteObject::from_stored(&stored,&backup.connected.handle).unwrap();
            let readings:std::sync::Arc<std::sync::Mutex<Vec<PhaseCounters>>>=Default::default();
            let collected=readings.clone();
            let progress=PhaseProgress::new(move|reading|collected.lock().unwrap().push(reading));
            let ids=backup.provider.uploaded_ids();
            let attempts=||ids.iter().map(|id|backup.provider.read_attempts(id)).collect::<Vec<_>>();
            let (attempts_before,received_before)=(attempts(),backup.provider.transferred_body_bytes().1);
            let (database,sections)=download_database_first_backup(root,&backup.connected,&job,&backup.backup_id,&remote,&progress,&Cancellation::default()).await.unwrap();
            assert!(sections.iter().any(|section|!section.sources.is_empty()));
            assert!(!database.missing.is_empty());
            for (before,after) in attempts_before.iter().zip(attempts()) {assert!(after-before<=1);}

            let last=progress.read();
            let readings=readings.lock().unwrap().clone();
            let first=readings.first().copied().unwrap();
            assert_eq!((first.total_items,first.total_bytes),(last.total_items,last.total_bytes));
            for reading in &readings {
                assert_eq!((reading.total_items,reading.total_bytes),(last.total_items,last.total_bytes));
                assert!(reading.items<=reading.total_items && reading.bytes<=reading.total_bytes);
            }
            // Everything read so far is counted, and what is left is exactly
            // the packs of the asset bodies this device lacks.
            assert_eq!(last.bytes,backup.provider.transferred_body_bytes().1-received_before);
            let mut asset_packs=std::collections::BTreeMap::new();
            for source in database.sources.iter().filter(|source|database.missing.contains(&source.hash)) {
                for chunk in &source.chunks {
                    let pack=source.packs.iter().find(|pack|pack.header.object_id==chunk.pack_id).unwrap();
                    asset_packs.insert(chunk.pack_id.clone(),pack.ciphertext_length);
                }
            }
            assert_eq!(last.total_items-last.items,asset_packs.len() as u64);
            assert_eq!(last.total_bytes-last.bytes,asset_packs.values().sum::<u64>());
            drop(store);
        });
    }

    /// The state read keeps answering while a restore commit holds the store.
    #[test]
    fn the_state_read_answers_while_a_restore_commit_holds_the_store() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let backup=packaged_backup(&[vec![50;4096]]).await;
            let destination=tempfile::tempdir().unwrap();
            let app=restore_app(destination.path());
            let (job,_claim,_receipt,_permit,_store)=commit_in_session(&app,&backup).await;
            ConnectionStore::open(destination.path()).unwrap().remove(&backup.connected.stored.id).unwrap();
            // An earlier stopped job is listed too.
            let jobs=JobStore::open(destination.path()).unwrap();
            let mut stopped=DurableJob::new(serde_json::from_value(json!({"connectionId":"synthetic-other","kind":"cleanup"})).unwrap(),1,job.admission_identity.clone());
            stopped.summary["state"]=json!("failed");
            jobs.put(&stopped).unwrap();
            let open=runtime::external_storage_get_state(app.handle().clone()).unwrap();
            let maintenance=app.state::<PersistentStoreState>().acquire_device_maintenance().unwrap();
            let held=runtime::external_storage_get_state(app.handle().clone()).unwrap();
            drop(maintenance);
            assert_eq!(held["selection"],open["selection"]);
            let listed=held["jobs"].as_array().unwrap().iter().find(|item|item["id"]==job.id.as_str()).unwrap();
            assert_eq!((listed["state"].as_str(),listed["phase"].as_str()),(Some("running"),Some("awaiting-adoption")));
            assert!(held["jobs"].as_array().unwrap().iter().any(|item|item["id"]==stopped.id.as_str()));
        });
    }

    #[test]
    fn a_restore_whose_worker_ended_after_its_commit_resumes_to_success_after_a_restart() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let bodies=vec![vec![46;4096]];
            let backup=packaged_backup(&bodies).await;
            let destination=tempfile::tempdir().unwrap();
            let root=destination.path();
            let app=restore_app(root);
            let (job,claim,receipt,permit,store)=commit_in_session(&app,&backup).await;
            drop((claim,permit,store));
            crate::persistent_store::commands::open_renderer_persistent_store(&app.state::<PersistentStoreState>(),root).unwrap();
            let unfinished=runtime::external_storage_get_job(app.handle().clone(),job.id.clone()).unwrap();
            assert_eq!((unfinished["state"].as_str(),unfinished["phase"].as_str()),(Some("uncertain"),Some("local-apply-unknown")));
            assert!(unfinished["result"].is_null());
            // Restoring the same backup again wakes the same job, which finds its commit.
            let jobs=JobStore::open(root).unwrap();
            let mut resumed=jobs.read(&job.id).unwrap();
            resumed.summary["state"]=json!("running");
            jobs.put(&resumed).unwrap();
            let (_,claim)=app.state::<super::super::job_store::JobCommandState>().claim(&resumed).unwrap();
            let store=runtime::native_store(app.handle()).unwrap();
            assert_eq!(completed_restore_in_store(&store,&resumed).unwrap(),Some(receipt.clone()));
            let cancel=Cancellation::default();
            let worker=finish_restore_bodies(app.handle(),&backup.connected,&resumed,receipt.clone(),None,store,&cancel);
            let renderer=async {
                assert_eq!(runtime::external_storage_get_job(app.handle().clone(),job.id.clone()).unwrap()["result"],receipt);
                let adoption=RestoreAdoptionRequest{job_id:job.id.clone(),received_revision:receipt["receivedRevision"].as_str().unwrap().into(),selected_character_id:None};
                external_storage_confirm_restore_adoption(app.handle().clone(),adoption).unwrap();
            };
            let (finished,())=tokio::join!(worker,renderer);
            assert_eq!(finished.unwrap(),receipt);
            drop(claim);
            let settled=runtime::external_storage_get_job(app.handle().clone(),job.id.clone()).unwrap();
            assert_eq!((settled["state"].as_str(),settled["phase"].as_str()),(Some("succeeded"),Some("complete")));
        });
    }

    /// A restore of `backup` admitted against the library at `root` at its current revision.
    fn admit(root:&Path,store:&PersistentStore,backup:&PackagedBackup) -> DurableJob {
        ConnectionStore::open(root).unwrap().insert(&backup.connected.stored).unwrap();
        let request=serde_json::from_value(json!({"connectionId":"synthetic-connection","kind":"restore","snapshotId":backup.backup_id,"targetRevision":store.revision().unwrap().to_string()})).unwrap();
        let mut job=DurableJob::new(request,1,store.external_identity().unwrap());
        job.summary["restoreSource"]=backup.restore_source.clone();
        JobStore::open(root).unwrap().put(&job).unwrap();
        job
    }

    /// Runs the restore `job` as its worker does, from the download to the local commit.
    async fn run_job(root:&Path,store:&mut PersistentStore,backup:&PackagedBackup,job:&DurableJob) -> Result<Value> {
        let job=JobStore::open(root).unwrap().read(&job.id).unwrap();
        let (database,sections)=prepare_database_first_backup(root,&backup.connected,&job,&Cancellation::default()).await?;
        let admission=std::sync::Arc::new(crate::native_file_jobs::admission::Admission::default());
        activate_database_first_backup(store,&PersistentStoreState::default(),&job,database,sections,Cancellation::default(),admission.staging().unwrap())
            .map(|(receipt,_)|receipt)
    }

    /// Restores `backup` over the library at `root`, admitted at its current revision.
    async fn restore_over(root:&Path,store:&mut PersistentStore,backup:&PackagedBackup) -> Result<Value> {
        let job=admit(root,store,backup);
        run_job(root,store,backup,&job).await
    }

    #[test]
    fn a_restore_reports_each_counted_preparation_step_and_clears_the_share_between_them() {
        run(async {
            let backup=packaged_backup_seeded(&[vec![53;4096]],|store|{
                let staging=store.replace_begin().unwrap().staging_id;
                store.replace_put_root(&staging,&json!({"temperature":1})).unwrap();
                store.replace_add_characters(&staging,&[json!({"chaId":"synthetic-character","type":"character","name":"Synthetic","chatPage":0,
                    "chats":[{"id":"synthetic-conversation","name":"Synthetic","message":[{"role":"user","data":"synthetic message","chatId":"synthetic-message"}]}]})]).unwrap();
                store.replace_commit(&staging,Some(store.revision().unwrap())).unwrap();
            }).await;
            let directory=tempfile::tempdir().unwrap();
            let root=directory.path();
            let mut store=PersistentStore::open(root).unwrap();
            let job=admit(root,&store,&backup);
            let (database,sections)=prepare_database_first_backup(root,&backup.connected,&job,&Cancellation::default()).await.unwrap();
            let (units,objects,records)=(database.original_units.count().unwrap(),database.snapshot.objects.len() as u64,database.snapshot.records.len() as u64);
            assert!(units>0 && objects>0 && records>0);
            let seen=std::cell::RefCell::new(Vec::new());
            let admission=std::sync::Arc::new(crate::native_file_jobs::admission::Admission::default());
            activate_database_first_backup_observed(&mut store,&PersistentStoreState::default(),&job,database,sections,Cancellation::default(),
                admission.staging().unwrap(),&|progress:Option<StagingProgress>| seen.borrow_mut().push(progress.map(|progress|(progress.phase,progress.done,progress.total)))).unwrap();
            let seen=seen.into_inner();
            // The steps between and after the counted ones clear the share instead of keeping a full bar.
            let mut steps=seen.iter().map(|progress|progress.map(|(phase,..)|phase)).collect::<Vec<_>>();
            steps.dedup();
            assert_eq!(steps,[Some("read"),Some("copy"),None,Some("validate"),Some("project"),None]);
            assert_eq!(seen.iter().filter(|progress|progress.is_none()).count(),2);
            // Each phase is one bar that fills to the count its input declares; both control passes read every unit.
            let mut phases=seen.iter().flatten().map(|&(phase,_,total)|(phase,total)).collect::<Vec<_>>();
            phases.dedup();
            assert!(matches!(phases[1],("copy",Some(controls)) if controls>0),"{phases:?}");
            assert_eq!([phases[0],phases[2],phases[3]],[("read",Some(2*units)),("validate",Some(objects)),("project",Some(records))]);
            for (phase,total) in phases {
                assert_eq!(seen.iter().flatten().rev().find(|(name,..)|*name==phase).map(|&(_,done,_)|done),total,"{phase}");
            }
        });
    }

    fn request_id(job:&DurableJob) -> String {
        format!("external-backup-restore:{}",job.id)
    }

    /// A restore that recorded its commit and stopped before the library received it, whose
    /// stage a later store open swept.
    fn interrupted_before_commit(root:&Path,store:&PersistentStore,backup:&PackagedBackup) -> DurableJob {
        let job=admit(root,store,backup);
        let header=crate::persistent_store::lww::Header{binding_authority:store.lww_binding_authority().unwrap(),request_id:request_id(&job)};
        let target=store.revision().unwrap();
        persist_restore_intent(root,&job,&restore_intent(&job,target,header,"synthetic-swept-stage").unwrap()).unwrap();
        JobStore::open(root).unwrap().read(&job.id).unwrap()
    }

    fn unapplied(root:&Path,job:&DurableJob) -> bool {
        let summary=JobStore::open(root).unwrap().read(&job.id).unwrap().summary;
        summary.get("applicationStarted").is_none() && summary.get("restoreCommit").is_none()
    }

    #[test]
    fn a_restore_stopped_before_the_library_received_its_commit_prepares_again_when_resumed() {
        run(async {
            let backup=packaged_backup(&[vec![48;4096]]).await;
            let directory=tempfile::tempdir().unwrap();
            let root=directory.path();
            let mut store=PersistentStore::open(root).unwrap();
            let job=interrupted_before_commit(root,&store,&backup);
            assert_eq!(job.summary["applicationStarted"],true);

            let receipt=run_job(root,&mut store,&backup,&job).await.unwrap();

            assert_eq!(receipt["receivedRevision"],store.revision().unwrap().to_string());
            let resumed=JobStore::open(root).unwrap().read(&job.id).unwrap();
            assert_eq!(resumed.summary["applicationStarted"],true);
            assert_ne!(resumed.summary["restoreCommit"]["stagingId"],"synthetic-swept-stage");
            assert_eq!(completed_restore_in_store(&store,&resumed).unwrap(),Some(receipt));
        });
    }

    #[test]
    fn a_restore_stopped_before_the_library_received_its_commit_ends_unapplied_once_the_library_changed() {
        run(async {
            let backup=packaged_backup(&[]).await;
            let directory=tempfile::tempdir().unwrap();
            let root=directory.path();
            let mut store=PersistentStore::open(root).unwrap();
            let job=interrupted_before_commit(root,&store,&backup);
            store.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:store.revision().unwrap(),
                root_mutations:Some(vec![crate::persistent_store::RootMutation::Set{key:"synthetic".into(),value:json!(true)}]),
                ..Default::default()}).unwrap();
            let revision=store.revision().unwrap();

            assert_eq!(run_job(root,&mut store,&backup,&job).await.unwrap_err().kind,ErrorKind::PreconditionFailed);

            assert!(unapplied(root,&job));
            assert_eq!(store.revision().unwrap(),revision);
        });
    }

    #[test]
    fn a_restore_commit_the_library_rejected_ends_unapplied() {
        run(async {
            let backup=packaged_backup(&[vec![49;4096]]).await;
            let directory=tempfile::tempdir().unwrap();
            let root=directory.path();
            let mut store=PersistentStore::open(root).unwrap();
            let job=admit(root,&store,&backup);
            let revision=store.revision().unwrap();
            fail_commit_at(root,CommitFailure::BeforeStore);

            assert_eq!(run_job(root,&mut store,&backup,&job).await.unwrap_err().kind,ErrorKind::Corrupt);

            assert!(unapplied(root,&job));
            assert!(store.lww_request_unreserved(&request_id(&job)).unwrap());
            assert_eq!(store.revision().unwrap(),revision);
        });
    }

    #[test]
    fn a_rejected_restore_commit_is_withdrawn_before_a_resumed_download_fails() {
        run(async {
            let backup=packaged_backup(&[vec![51;4096]]).await;
            let directory=tempfile::tempdir().unwrap();
            let root=directory.path();
            let app=restore_app(root);
            let job=interrupted_before_commit(root,&runtime::native_store(app.handle()).unwrap(),&backup);
            assert_eq!(job.summary["applicationStarted"],true);
            backup.provider.fail_read(backup.restore_source["locator"]["object"].as_str().unwrap(),ErrorKind::Transient);

            let failed=run_restore(app.handle(),&backup.connected,&job,&Cancellation::default()).await.unwrap_err();

            assert_eq!(failed.kind,ErrorKind::Transient);
            assert!(unapplied(root,&job));
        });
    }

    #[test]
    fn a_restore_commit_the_library_received_stays_unsettled_when_its_worker_fails() {
        run(async {
            let backup=packaged_backup(&[vec![50;4096]]).await;
            let directory=tempfile::tempdir().unwrap();
            let root=directory.path();
            let mut store=PersistentStore::open(root).unwrap();
            let job=admit(root,&store,&backup);
            fail_commit_at(root,CommitFailure::AfterStore);

            assert_eq!(run_job(root,&mut store,&backup,&job).await.unwrap_err().kind,ErrorKind::Corrupt);

            let current=JobStore::open(root).unwrap().read(&job.id).unwrap();
            assert_eq!(current.summary["applicationStarted"],true);
            assert!(!store.lww_request_unreserved(&request_id(&job)).unwrap());
            let receipt=completed_restore_in_store(&store,&current).unwrap().unwrap();
            assert_eq!(receipt["receivedRevision"],store.revision().unwrap().to_string());
        });
    }

    const DELETED:&str="synthetic-deleted";
    const DELETED_CHAT:&str="synthetic-deleted-chat";

    fn unit_key(parts:&[&str]) -> risunest_sync_wire::unit::UnitKey {
        risunest_sync_wire::unit::UnitKey::new(parts).unwrap()
    }

    fn retired_keys(store:&PersistentStore) -> Vec<String> {
        let mut statement=store.library_rows().prepare("SELECT key FROM lww_retired ORDER BY key").unwrap();
        statement.query_map([],|row|row.get(0)).unwrap().collect::<std::result::Result<_,_>>().unwrap()
    }

    fn live_unit(store:&PersistentStore,parts:&[&str]) -> bool {
        use rusqlite::OptionalExtension;
        let value:Option<String>=store.library_rows().query_row("SELECT value FROM lww_units WHERE key=?1",[unit_key(parts).as_str()],|row|row.get(0)).optional().unwrap();
        value.is_some_and(|value| !matches!(serde_json::from_str(&value).unwrap(),risunest_sync_wire::unit::UnitValue::Deleted))
    }

    /// A character with one conversation whose message carries a bookmark, a bookmark name and a Hypa memo.
    fn add_deleted_character(store:&mut PersistentStore) {
        store.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:store.revision().unwrap(),
            add_character:Some(json!({"chaId":DELETED,"type":"character","name":"Synthetic deleted","chats":[{
                "id":DELETED_CHAT,"name":"Synthetic chat",
                "message":[{"role":"user","data":"synthetic message","chatId":"synthetic-message"}],
                "bookmarks":["synthetic-message"],"bookmarkNames":{"synthetic-message":"kept"},"hypaV3Data":{"memos":["synthetic-message"]},
            }]})),
            unit_mutations:Some(vec![crate::persistent_store::lww::UnitMutation::Set{key:unit_key(&["order","characters"]),value:json!([DELETED])}]),
            ..Default::default()}).unwrap();
    }

    /// Hard-deletes as the renderer does: the existence units and the character order without the character.
    fn delete_existence(store:&mut PersistentStore,keys:&[&[&str]]) {
        use crate::persistent_store::lww::UnitMutation;
        let mut mutations=keys.iter().map(|key| UnitMutation::Delete{key:unit_key(key)}).collect::<Vec<_>>();
        mutations.push(UnitMutation::Set{key:unit_key(&["order","characters"]),value:json!([])});
        store.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:store.revision().unwrap(),
            unit_mutations:Some(mutations),..Default::default()}).unwrap();
    }

    /// Whether `store` publishes anything but a deletion under the deleted character's ID.
    fn publishes_under_deleted_id(store:&PersistentStore) -> bool {
        store.lww_read_outbox(store.lww_binding_authority().unwrap(),1000).unwrap().entries.iter()
            .any(|entry| entry.key.components().iter().any(|part| part==DELETED)
                && !matches!(entry.value,risunest_sync_wire::unit::UnitValue::Deleted))
    }

    /// The deleted character is back under a new ID with its conversation, whose ID is new when it
    /// was retired as well, and its messages and message references. Only the new IDs are published.
    fn assert_restored_under_new_ids(store:&PersistentStore,conversation_retired:bool) {
        let database=store.materialize(None).unwrap();
        let [character]=database["characters"].as_array().unwrap().as_slice() else {panic!("one character is restored")};
        let id=character["chaId"].as_str().unwrap();
        assert_ne!(id,DELETED);
        assert_eq!(character["name"],"Synthetic deleted");
        assert_eq!(database["characterOrder"],json!([id]));
        let [chat]=character["chats"].as_array().unwrap().as_slice() else {panic!("one conversation is restored")};
        let chat_id=chat["id"].as_str().unwrap();
        assert_eq!(chat_id!=DELETED_CHAT,conversation_retired);
        assert_eq!(chat["message"][0]["data"],"synthetic message");
        assert_eq!(chat["message"][0]["chatId"],"synthetic-message");
        assert_eq!(chat["bookmarks"],json!(["synthetic-message"]));
        assert_eq!(chat["bookmarkNames"],json!({"synthetic-message":"kept"}));
        assert_eq!(chat["hypaV3Data"],json!({"memos":["synthetic-message"]}));
        let outbox=store.lww_read_outbox(store.lww_binding_authority().unwrap(),1000).unwrap().entries;
        let published=|parts:&[&str]| outbox.iter().any(|entry| entry.key==unit_key(parts) && !matches!(entry.value,risunest_sync_wire::unit::UnitValue::Deleted));
        assert!(published(&["exists","character",id]));
        assert!(published(&["exists","conversation",id,chat_id]));
        assert!(published(&["messages",id,chat_id]));
        assert!(published(&["order","characters"]));
        let order=risunest_sync_wire::unit::UnitValue::inline(&serde_json::to_vec(&json!({"ids":[chat_id],"folders":[]})).unwrap()).unwrap();
        assert!(outbox.iter().any(|entry| entry.key==unit_key(&["order","conversations",id]) && entry.value==order));
        assert!(!publishes_under_deleted_id(store));
    }

    fn run<T>(future:impl std::future::Future<Output=T>) -> T {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
    }

    #[test]
    fn a_backup_restores_a_character_deleted_after_it_under_a_new_id() {
        let character:&[&str]=&["exists","character",DELETED];
        let conversation:&[&str]=&["exists","conversation",DELETED,DELETED_CHAT];
        // The renderer deletes the character's existence alone. A device can also hold the
        // conversation's existence as retired, which a deletion before the character's leaves.
        for retired in [vec![character],vec![conversation,character]] {
            run(async {
                let directory=tempfile::tempdir().unwrap();
                let root=directory.path();
                let mut store=PersistentStore::open(root).unwrap();
                add_deleted_character(&mut store);
                let backup=uploaded_backup(&mut store,Vec::new()).await;
                for key in &retired {delete_existence(&mut store,&[key]);}
                let mut expected=retired.iter().map(|parts| unit_key(parts).as_str().to_owned()).collect::<Vec<_>>();
                expected.sort();
                assert_eq!(retired_keys(&store),expected);

                restore_over(root,&mut store,&backup).await.unwrap();

                assert_restored_under_new_ids(&store,retired.contains(&conversation));
            });
        }
    }

    #[test]
    fn a_backup_restores_a_character_whose_deletion_was_received_under_a_new_id() {
        run(async {
            let mut f=super::super::lww_tests::CycleFixture::new();
            add_deleted_character(&mut f.a);
            f.publish_a().await;
            f.receive_b().await;
            let backup=uploaded_backup(&mut f.b,Vec::new()).await;
            delete_existence(&mut f.a,&[&["exists","character",DELETED]]);
            f.publish_a().await;
            f.receive_b().await;
            assert_eq!(retired_keys(&f.b),[unit_key(&["exists","character",DELETED]).as_str()]);

            let root=f.directory_b.path().to_owned();
            restore_over(&root,&mut f.b,&backup).await.unwrap();

            assert_restored_under_new_ids(&f.b,false);
        });
    }

    #[test]
    fn a_backup_holding_units_of_a_character_deleted_before_it_restores_without_them() {
        run(async {
            use risunest_sync_wire::unit::UnitValue;
            use std::collections::BTreeMap;
            let directory=tempfile::tempdir().unwrap();
            let root=directory.path();
            let mut store=PersistentStore::open(root).unwrap();
            add_deleted_character(&mut store);
            let children=[unit_key(&["conversation",DELETED,DELETED_CHAT,"bookmarkNames"]),unit_key(&["exists","conversation",DELETED,DELETED_CHAT])];
            let legacy_units=children.into_iter().map(|key| {
                let value:String=store.library_rows().query_row("SELECT value FROM lww_units WHERE key=?1",[key.as_str()],|row|row.get(0)).unwrap();
                let value:UnitValue=serde_json::from_str(&value).unwrap();
                assert!(matches!(value,UnitValue::Inline{..}));
                (key,value)
            }).collect::<BTreeMap<_,_>>();
            delete_existence(&mut store,&[&["exists","character",DELETED]]);
            assert!(!live_unit(&store,&["conversation",DELETED,DELETED_CHAT,"bookmarkNames"]));
            assert!(!live_unit(&store,&["exists","conversation",DELETED,DELETED_CHAT]));
            // Older backups carried live child units alongside the parent's permanent deletion.
            let backup=uploaded_backup_with_legacy_units(&mut store,Vec::new(),legacy_units.clone()).await;
            let stored:risunest_external_storage_format::snapshot::StoredObject=serde_json::from_value(backup.restore_source.clone()).unwrap();
            let remote=super::super::packaging::RemoteObject::from_stored(&stored,&backup.connected.handle).unwrap();
            let inspection=tempfile::tempdir().unwrap();
            let uploaded=snapshot_restore::download_backup_original_units(&remote,inspection.path(),&backup.connected.root_key,
                backup.provider.as_ref(),&backup.connected.handle,&Cancellation::default()).await.unwrap();
            let content=super::super::content_store::ContentStore::open(&inspection.path().join("external-storage")).unwrap();
            let uploaded=uploaded.units(&content).collect::<Result<BTreeMap<_,_>>>().unwrap();
            for (key,value) in &legacy_units {assert_eq!(uploaded.get(key),Some(value));}
            assert_eq!(uploaded.get(&unit_key(&["exists","character",DELETED])),Some(&UnitValue::Deleted));

            restore_over(root,&mut store,&backup).await.unwrap();

            assert_eq!(store.materialize(None).unwrap()["characters"],json!([]));
            assert_eq!(retired_keys(&store),[unit_key(&["exists","character",DELETED]).as_str()]);
            assert!(!publishes_under_deleted_id(&store));
        });
    }

    /// Adds a character without conversations, in the trash when `trash_time` is given.
    fn add_character(store:&mut PersistentStore,id:&str,trash_time:Option<i64>) {
        let mut character=json!({"chaId":id,"type":"character","name":"Synthetic","chats":[]});
        if let Some(time)=trash_time {character["trashTime"]=json!(time);}
        store.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:store.revision().unwrap(),
            add_character:Some(character),..Default::default()}).unwrap();
    }

    #[test]
    fn a_restore_keeps_trashed_characters_out_of_the_character_order() {
        const LIVE:&str="synthetic-live";
        const LISTED:&str="synthetic-trashed-listed";
        const UNLISTED:&str="synthetic-trashed-unlisted";
        run(async {
            let directory=tempfile::tempdir().unwrap();
            let root=directory.path();
            let mut store=PersistentStore::open(root).unwrap();
            add_character(&mut store,LIVE,None);
            add_character(&mut store,LISTED,Some(1));
            add_character(&mut store,UNLISTED,Some(2));
            store.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:store.revision().unwrap(),
                unit_mutations:Some(vec![crate::persistent_store::lww::UnitMutation::Set{key:unit_key(&["order","characters"]),value:json!([LIVE,LISTED])}]),
                ..Default::default()}).unwrap();
            let backup=uploaded_backup(&mut store,Vec::new()).await;
            delete_existence(&mut store,&[&["exists","character",LISTED],&["exists","character",UNLISTED]]);

            restore_over(root,&mut store,&backup).await.unwrap();

            let database=store.materialize(None).unwrap();
            assert_eq!(database["characterOrder"],json!([LIVE]));
            let characters=database["characters"].as_array().unwrap();
            assert_eq!(characters.len(),3);
            for character in characters.iter().filter(|character| character["chaId"]!=LIVE) {
                assert!(![LISTED,UNLISTED].contains(&character["chaId"].as_str().unwrap()));
                assert!(character["trashTime"].is_i64());
            }
        });
    }
}
