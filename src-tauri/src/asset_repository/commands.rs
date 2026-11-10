use super::job_pins::{
    durable_cas_job_held, durable_cas_job_ids, inspect_durable_cas_job, CasJobKind, CasJobOwner,
    CasJobOwnerKind, CasObjectRole, CasReleaseOutcome, DurableCasJob, DurableCasJobOwnership,
};
use super::{PayloadCas, PreparedPayload};
use super::payload_cas::{ObjectPart, OwnedPayloadUpload};
use crate::asset_repository::owner_manifest_codec::{
    decode_owner_manifest, encode_owner_manifest, OWNER_MANIFEST_V1_MAX_CANONICAL_BYTES,
};
use crate::native_file_jobs::{
    JobKind as NativeJobKind, JobState as NativeJobState, JobStatus as NativeJobStatus,
    NativeFileJobState,
};
use crate::native_log::logged;
use crate::persistent_store::{self, PersistentStore, PersistentStoreState, StoreError};
use crate::trust_boundary::is_lower_hex_256;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Cursor, ErrorKind};
use std::sync::{Arc, Mutex, atomic::{AtomicU8, Ordering}};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};

#[derive(Clone, Debug)]
pub(crate) struct ContentDirectObject {
    object_hash: String,
    byte_size: u64,
}

pub(crate) struct DurableCasJobState {
    jobs: Mutex<HashMap<String, DurableCasJob>>,
    uploads: Mutex<CasUploadPool>,
}

impl Default for DurableCasJobState {
    fn default() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            uploads: Mutex::new(CasUploadPool::default()),
        }
    }
}

/// What the journal sweep asks about the work that may still own a journal.
pub(crate) struct CasJobOwnerProbe<'a> {
    pub(crate) native_jobs: &'a dyn Fn() -> Result<Vec<NativeJobStatus>, String>,
    pub(crate) device_job_owned: &'a dyn Fn(&str) -> Result<bool, String>,
    pub(crate) external_job_active: &'a dyn Fn(&str) -> Result<bool, String>,
    pub(crate) open_store: &'a dyn Fn() -> Result<PersistentStore, String>,
}

/// The journals present when a page started, judged on a blocking thread
/// because a release may wait for the repository lock for a long time.
struct AbandonedCasJobs {
    root: std::path::PathBuf,
    journal_ids: Vec<String>,
    listing_failure: Option<String>,
    cleanup_uploads: bool,
}

impl AbandonedCasJobs {
    fn snapshot(root: &std::path::Path) -> Self {
        let (journal_ids, listing_failure) = match durable_cas_job_ids(root) {
            Ok(ids) => (ids, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        Self { root: root.to_owned(), journal_ids, listing_failure, cleanup_uploads: false }
    }

    fn release(self, probe: &CasJobOwnerProbe) -> Result<(), String> {
        let mut failure = self.listing_failure;
        if self.cleanup_uploads {
            if let Err(error) = PayloadCas::new(&self.root).and_then(|cas| cas.sweep_abandoned_uploads()) {
                failure.get_or_insert(error.to_string());
            }
        }
        for id in &self.journal_ids {
            if let Err(error) = release_if_owner_ended(&self.root, id, probe) {
                failure.get_or_insert(format!("{id}: {error}"));
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

/// Releases a journal no running work can use any more. A journal that a
/// handle of this process holds is in use, whatever its owner record says.
fn release_if_owner_ended(
    root: &std::path::Path,
    id: &str,
    probe: &CasJobOwnerProbe,
) -> Result<bool, String> {
    if durable_cas_job_held(root, id).map_err(|error| error.to_string())? {
        return Ok(false);
    }
    let Some(journal) = inspect_durable_cas_job(root, id).map_err(|error| error.to_string())?
    else {
        return Ok(false);
    };
    if !owner_ended(root, id, &journal, probe)? {
        return Ok(false);
    }
    let mut job = match DurableCasJob::open(root, id) {
        Ok(job) => job,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    if job.held_elsewhere() || job.owner() != &journal.owner {
        return Ok(false);
    }
    job.release(CasReleaseOutcome::Aborted)
        .map_err(|error| error.to_string())?;
    Ok(true)
}

fn owner_ended(
    root: &std::path::Path,
    id: &str,
    journal: &DurableCasJobOwnership,
    probe: &CasJobOwnerProbe,
) -> Result<bool, String> {
    let owner = &journal.owner;
    let here = owner.started_in_this_process();
    let native_job = |id: &str| -> Result<Option<NativeJobStatus>, String> {
        Ok((probe.native_jobs)()?.into_iter().find(|job| job.job_id == id))
    };
    Ok(match owner.kind {
        // Direct writes and their content end with the page that started them,
        // and an export holds its journal for as long as it runs.
        CasJobOwnerKind::PageWrite | CasJobOwnerKind::SnapshotExport => true,
        CasJobOwnerKind::ContentImport => {
            !here || native_job(&owner.id)?.is_none_or(|job| native_job_finished(&job))
        }
        CasJobOwnerKind::NativeFileJob => {
            !(probe.device_job_owned)(&owner.id)?
                && (!here
                    || native_job(&owner.id)?.is_none_or(|job| {
                        native_job_finished(&job)
                            && !(job.kind == NativeJobKind::OfficialPublicationUpload
                                && job.state == NativeJobState::Succeeded)
                    }))
        }
        CasJobOwnerKind::ExternalCompaction | CasJobOwnerKind::ExternalHydration => true,
        // A saved publication keeps its journal until it is sent and settled;
        // one that never reached its segment row has nothing to resume.
        CasJobOwnerKind::ExternalPublication => {
            !journal.sealed || !publication_segment_exists(&(probe.open_store)()?, id)?
        }
        CasJobOwnerKind::ExternalRestore => {
            crate::external_storage::runtime_restore::restore_journal_owner_ended(
                root,
                &owner.id,
                (probe.external_job_active)(&owner.id)?,
                probe.open_store,
            )?
        }
    })
}

fn native_job_finished(job: &NativeJobStatus) -> bool {
    matches!(
        job.state,
        NativeJobState::Succeeded | NativeJobState::Failed | NativeJobState::Cancelled
    )
}

fn publication_segment_exists(store: &PersistentStore, job_id: &str) -> Result<bool, String> {
    store
        .device_store()
        .map_err(|error| error.to_string())?
        .connection()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM external_lww_segments
             WHERE json_extract(metadata,'$.assetJob.jobId')=?1)",
            [job_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}

fn release_abandoned<R: tauri::Runtime>(app: &AppHandle<R>, abandoned: AbandonedCasJobs) -> Result<(), String> {
    let operation = app
        .state::<PersistentStoreState>()
        .admit_renderer_operation()
        .map_err(|error| error.to_string())?;
    let native_jobs = || match app.try_state::<NativeFileJobState>() {
        Some(native) => native.list().map_err(|error| error.message),
        None => Ok(Vec::new()),
    };
    let open_store = || {
        persistent_store::commands::with_store_mut_admitted(
            app.state(),
            &operation,
            |store| store.open_native_job_store(),
        )
        .map_err(|error| error.to_string())
    };
    let device_job_owned = |id: &str| match app.try_state::<crate::device_backup::DeviceBackupState>() {
        Some(state) => state.owns_native_restore_job(id).map_err(|error| error.to_string()),
        None => Ok(false),
    };
    let external_job_active = |id: &str| match app.try_state::<crate::external_storage::job_store::JobCommandState>() {
        Some(state) => state.job_is_active(id).map_err(|error| error.to_string()),
        None => Ok(false),
    };
    abandoned.release(&CasJobOwnerProbe {
        native_jobs: &native_jobs,
        device_job_owned: &device_job_owned,
        external_job_active: &external_job_active,
        open_store: &open_store,
    })
}

fn release_logged<R: tauri::Runtime>(app: &AppHandle<R>, abandoned: AbandonedCasJobs) {
    match release_abandoned(app, abandoned) {
        // Opening the store judges these journals again (`open_for_renderer`).
        Err(error) if error.ends_with("persistent store has not been opened") => {}
        Err(error) => crate::nlog!("error", "failed to release abandoned CAS jobs: {error}"),
        Ok(()) => {}
    }
}

impl DurableCasJobState {
    pub(crate) fn close_for_cleanup(&self) -> Result<(), String> {
        self.clear_uploads()?;
        self.jobs.lock().map_err(|_| "cleanup-cas-busy")?.clear();
        Ok(())
    }

    fn clear_uploads(&self) -> Result<(), String> {
        self.uploads
            .lock()
            .map_err(|error| format!("CAS upload mutex poisoned: {error}"))?
            .clear();
        Ok(())
    }

    pub(crate) fn reset_renderer_session<R: tauri::Runtime>(&self, app: &AppHandle<R>) -> Result<(), String> {
        let abandoned = self.take_abandoned(&repository_root(app)?)?;
        self.release_in_background(app, abandoned);
        Ok(())
    }

    pub(crate) fn sweep_settled_jobs<R: tauri::Runtime>(&self, app: &AppHandle<R>) -> Result<(), String> {
        self.release_in_background(app, AbandonedCasJobs::snapshot(&repository_root(app)?));
        Ok(())
    }

    fn release_in_background<R: tauri::Runtime>(&self, app: &AppHandle<R>, abandoned: AbandonedCasJobs) {
        if abandoned.cleanup_uploads || !abandoned.journal_ids.is_empty() || abandoned.listing_failure.is_some() {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || release_logged(&app, abandoned));
        }
    }

    fn take_abandoned(&self, root: &std::path::Path) -> Result<AbandonedCasJobs, String> {
        self.clear_uploads()?;
        // Only page commands open sessions here, and the next page knows none
        // of them, so their handles no longer keep a journal in use.
        self.jobs
            .lock()
            .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?
            .clear();
        // Journals are listed now, before the new page can begin its own.
        let mut abandoned = AbandonedCasJobs::snapshot(root);
        abandoned.cleanup_uploads = true;
        Ok(abandoned)
    }

    #[cfg(test)]
    pub(crate) fn sweep_after_page_start(
        &self,
        root: &std::path::Path,
        probe: &CasJobOwnerProbe,
    ) -> Result<(), String> {
        self.take_abandoned(root)?.release(probe)
    }
}

const CAS_UPLOAD_CHUNK_BYTES: usize = 64 * 1024;
const MAX_CAS_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CONCURRENT_CAS_UPLOADS: usize = 4;
const MAX_TOTAL_CAS_UPLOAD_BYTES: u64 = 1024 * 1024 * 1024;

struct CasUpload {
    session_id: String,
    role: CasObjectRole,
    total_bytes: u64,
    state: AtomicU8,
    file: Mutex<Option<OwnedPayloadUpload>>,
    capacity: usize,
    _receiver: Option<Arc<crate::asset_byte_transport::AndroidReceiver>>,
}

const UPLOAD_RECEIVING: u8 = 0;
const UPLOAD_FINISHING: u8 = 1;
const UPLOAD_CANCELLED: u8 = 2;

impl CasUpload {
    fn append(&self, offset: u64, chunk: &[u8]) -> io::Result<u64> {
        let mut owned = self.file.lock().map_err(|_| io::Error::other("CAS upload poisoned"))?;
        if self.state.load(Ordering::Acquire) != UPLOAD_RECEIVING {
            owned.take();
            return Err(io::Error::new(ErrorKind::NotFound, "CAS upload is closed"));
        }
        let file = owned.as_mut().ok_or_else(|| io::Error::other("CAS upload is closed"))?;
        if offset != file.byte_size() || chunk.is_empty() || chunk.len() > self.capacity {
            return Err(io::Error::new(ErrorKind::InvalidInput, "invalid CAS upload chunk"));
        }
        let next = offset.checked_add(chunk.len() as u64)
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "CAS upload length overflow"))?;
        if next > self.total_bytes {
            return Err(io::Error::new(ErrorKind::InvalidInput, "CAS upload exceeds declared length"));
        }
        if let Err(error) = file.append(chunk) {
            self.state.store(UPLOAD_CANCELLED, Ordering::Release);
            owned.take();
            return Err(error);
        }
        if self.state.load(Ordering::Acquire) != UPLOAD_RECEIVING {
            owned.take();
            return Err(io::Error::new(ErrorKind::Interrupted, "CAS upload cancelled"));
        }
        Ok(next)
    }

    fn take_complete(&self) -> io::Result<OwnedPayloadUpload> {
        let mut owned = self.file.lock().map_err(|_| io::Error::other("CAS upload poisoned"))?;
        let file = owned.as_ref().ok_or_else(|| io::Error::new(ErrorKind::NotFound, "CAS upload is closed"))?;
        if file.byte_size() != self.total_bytes {
            return Err(io::Error::new(ErrorKind::InvalidInput, "CAS upload is incomplete"));
        }
        self.state.compare_exchange(UPLOAD_RECEIVING, UPLOAD_FINISHING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| io::Error::new(ErrorKind::NotFound, "CAS upload is closed"))?;
        Ok(owned.take().expect("completed CAS upload"))
    }

    fn cancel(&self) {
        if self.state.compare_exchange(UPLOAD_RECEIVING, UPLOAD_CANCELLED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            if let Ok(mut file) = self.file.try_lock() { file.take(); }
        }
    }
}

#[derive(Default)]
struct CasUploadPool {
    uploads: HashMap<String, Arc<CasUpload>>,
    reserved_bytes: u64,
    /// Files of uploads cancelled during maintenance, kept owned until the next reset.
    parked: Vec<OwnedPayloadUpload>,
}

impl CasUploadPool {
    #[cfg(test)]
    fn open(
        &mut self,
        cas: &PayloadCas,
        upload_id: String,
        session_id: String,
        role: CasObjectRole,
        total_bytes: u64,
    ) -> io::Result<()> {
        self.open_with_capacity(cas, upload_id, session_id, role, total_bytes, CAS_UPLOAD_CHUNK_BYTES, None)
    }

    fn open_with_capacity(
        &mut self, cas: &PayloadCas, upload_id: String, session_id: String, role: CasObjectRole,
        total_bytes: u64, capacity: usize, receiver: Option<Arc<crate::asset_byte_transport::AndroidReceiver>>,
    ) -> io::Result<()> {
        if uuid::Uuid::parse_str(&upload_id).is_err() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "invalid CAS upload ID",
            ));
        }
        if total_bytes <= CAS_UPLOAD_CHUNK_BYTES as u64 || total_bytes > MAX_CAS_UPLOAD_BYTES {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "CAS upload length is outside the streamed range",
            ));
        }
        if self.uploads.len() >= MAX_CONCURRENT_CAS_UPLOADS || self.uploads.contains_key(&upload_id)
        {
            return Err(io::Error::new(
                ErrorKind::WouldBlock,
                "CAS upload capacity is unavailable",
            ));
        }
        let reserved_bytes = self
            .reserved_bytes
            .checked_add(total_bytes)
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "CAS upload quota overflow"))?;
        if reserved_bytes > MAX_TOTAL_CAS_UPLOAD_BYTES {
            return Err(io::Error::new(
                ErrorKind::WouldBlock,
                "CAS upload byte quota is unavailable",
            ));
        }
        let file = cas.begin_owned_upload()?;
        self.uploads.insert(
            upload_id,
            Arc::new(CasUpload {
                session_id, role, total_bytes,
                state: AtomicU8::new(UPLOAD_RECEIVING), file: Mutex::new(Some(file)),
                capacity, _receiver: receiver,
            }),
        );
        self.reserved_bytes = reserved_bytes;
        Ok(())
    }

    fn get(&self, upload_id: &str) -> io::Result<Arc<CasUpload>> {
        self.uploads.get(upload_id).cloned()
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "CAS upload is missing"))
    }

    #[cfg(test)]
    fn append(&mut self, upload_id: &str, offset: u64, chunk: &[u8]) -> io::Result<u64> {
        self.get(upload_id)?.append(offset, chunk)
    }

    #[cfg(test)]
    fn take_complete(&mut self, upload_id: &str) -> io::Result<OwnedPayloadUpload> {
        let file = self.get(upload_id)?.take_complete()?;
        self.cancel(upload_id);
        Ok(file)
    }

    fn cancel(&mut self, upload_id: &str) {
        if let Some(upload) = self.uploads.remove(upload_id) {
            self.reserved_bytes -= upload.total_bytes;
            upload.cancel();
        }
    }

    /// Releases the upload's quota and Android receiver while maintenance
    /// keeps every repository file in place.
    fn release_during_maintenance(&mut self, upload_id: &str) {
        if let Some(upload) = self.uploads.remove(upload_id) {
            self.reserved_bytes -= upload.total_bytes;
            if upload.state.compare_exchange(UPLOAD_RECEIVING, UPLOAD_CANCELLED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                self.parked.extend(upload.file.lock().ok().and_then(|mut file| file.take()));
            }
        }
    }

    fn cancel_session(&mut self, session_id: &str) {
        let removed = self
            .uploads
            .iter()
            .filter(|(_, upload)| upload.session_id == session_id)
            .map(|(id, upload)| (id.clone(), upload.total_bytes))
            .collect::<Vec<_>>();
        for (id, _) in removed { self.cancel(&id); }
    }

    fn clear(&mut self) {
        for (_, upload) in self.uploads.drain() { upload.cancel(); }
        self.parked.clear();
        self.reserved_bytes = 0;
    }
}

fn repository_root<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<std::path::PathBuf, String> {
    crate::app_paths::data_root(app)
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
    logged("asset_cas_job_begin", async move {
        tauri::async_runtime::spawn_blocking(move || {
            let _operation = app
                .state::<PersistentStoreState>()
                .admit_renderer_operation()
                .map_err(|error| error.to_string())?;
            let root = repository_root(&app)?;
            let session_id = uuid::Uuid::new_v4().to_string();
            let job = DurableCasJob::begin(&root, &session_id, kind, CasJobOwner::page_write(&session_id), now_ms()?)
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
    }.await)
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_prepare(
    app: AppHandle,
    session_id: String,
    data: Vec<u8>,
    role: CasObjectRole,
) -> Result<PreparedPayload, String> {
    logged("asset_cas_job_prepare", async move {
        if data.len() > CAS_UPLOAD_CHUNK_BYTES {
            return Err("direct CAS prepare exceeds the IPC chunk limit".to_owned());
        }
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
    }.await)
}

#[derive(serde::Serialize)]
pub(crate) struct CasUploadOpened {
    capacity: usize,
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_upload_open(
    app: AppHandle,
    upload_id: String,
    session_id: String,
    role: CasObjectRole,
    total_bytes: u64,
    binary: Option<bool>,
) -> Result<CasUploadOpened, String> {
    logged("asset_cas_job_upload_open", async move {
        tauri::async_runtime::spawn_blocking(move || {
            let _operation = app
                .state::<PersistentStoreState>()
                .admit_renderer_operation()
                .map_err(|error| error.to_string())?;
            let root = repository_root(&app)?;
            let cas = PayloadCas::new(&root).map_err(|error| error.to_string())?;
            let state = app.state::<DurableCasJobState>();
            let mut uploads = state
                .uploads
                .lock()
                .map_err(|error| format!("CAS upload mutex poisoned: {error}"))?;
            let mut jobs = state
                .jobs
                .lock()
                .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?;
            let job = recover_job(&mut jobs, &root, &session_id)?;
            if job.is_sealed() || job.is_released() {
                return Err("sealed or released CAS job cannot accept an upload".to_owned());
            }
            let capacity = if binary.unwrap_or(false) { crate::asset_byte_transport::BINARY_PART_BYTES } else { CAS_UPLOAD_CHUNK_BYTES };
            #[cfg(not(target_os = "android"))]
            let receiver = None;
            #[cfg(target_os = "android")]
            let receiver = if binary.unwrap_or(false) {
                let receiver_app = app.clone();
                let receiver_id = upload_id.clone();
                Some(crate::asset_byte_transport::register_android_receiver(&upload_id, Arc::new(move |offset, bytes| {
                    append_upload(&receiver_app, &receiver_id, offset, bytes)
                }))?)
            } else { None };
            uploads
                .open_with_capacity(&cas, upload_id, session_id, role, total_bytes, capacity, receiver)
                .map_err(|error| error.to_string())?;
            Ok(CasUploadOpened {
                capacity,
            })
        })
        .await
        .map_err(|error| format!("failed to join CAS upload open operation: {error}"))?
    }.await)
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_upload_chunk(
    app: AppHandle,
    upload_id: String,
    offset: u64,
    data: Vec<u8>,
) -> Result<u64, String> {
    logged("asset_cas_job_upload_chunk", async move {
        tauri::async_runtime::spawn_blocking(move || {
            if data.len() > CAS_UPLOAD_CHUNK_BYTES { return Err("CAS JSON part exceeds its capacity".to_owned()); }
            append_upload(&app, &upload_id, offset, &data)
        })
        .await
        .map_err(|error| format!("failed to join CAS upload chunk operation: {error}"))?
    }.await)
}

fn append_upload(app: &AppHandle, upload_id: &str, offset: u64, data: &[u8]) -> Result<u64, String> {
    let _operation = app.state::<PersistentStoreState>().admit_renderer_operation().map_err(|error| error.to_string())?;
    let state = app.state::<DurableCasJobState>();
    let upload = state.uploads.lock().map_err(|error| format!("CAS upload mutex poisoned: {error}"))?
        .get(upload_id).map_err(|error| error.to_string())?;
    let result = upload.append(offset, data);
    if upload.state.load(Ordering::Acquire) == UPLOAD_CANCELLED {
        state.uploads.lock().map_err(|error| error.to_string())?.cancel(upload_id);
    }
    result.map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_upload_chunk_raw(
    app: AppHandle, request: tauri::ipc::Request<'_>,
) -> Result<u64, crate::asset_byte_transport::ByteTransferError> {
    logged("asset_cas_job_upload_chunk_raw", async move {
        let (id, offset, bytes) = crate::asset_byte_transport::raw_part(&request)?;
        let bytes = bytes.to_vec();
        tauri::async_runtime::spawn_blocking(move || append_upload(&app, &id, offset, &bytes))
            .await
            .map_err(|error| format!("failed to join CAS upload raw chunk operation: {error}"))?
            .map_err(Into::into)
    }.await)
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_upload_finish(
    app: AppHandle,
    upload_id: String,
) -> Result<PreparedPayload, String> {
    logged("asset_cas_job_upload_finish", async move {
        tauri::async_runtime::spawn_blocking(move || {
            let _operation = app
                .state::<PersistentStoreState>()
                .admit_renderer_operation()
                .map_err(|error| error.to_string())?;
            let root = repository_root(&app)?;
            let cas = PayloadCas::new(&root).map_err(|error| error.to_string())?;
            let state = app.state::<DurableCasJobState>();
            let upload = state.uploads.lock()
                .map_err(|error| format!("CAS upload mutex poisoned: {error}"))?
                .get(&upload_id).map_err(|error| error.to_string())?;
            let file = upload.take_complete().map_err(|error| error.to_string())?;
            state.uploads.lock().map_err(|error| error.to_string())?.cancel(&upload_id);
            let mut jobs = state
                .jobs
                .lock()
                .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?;
            recover_job(&mut jobs, &root, &upload.session_id)?
                .prepare_owned_upload(&cas, file, upload.role)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("failed to join CAS upload finish operation: {error}"))?
    }.await)
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_upload_cancel(
    app: AppHandle,
    upload_id: String,
) -> Result<(), String> {
    logged("asset_cas_job_upload_cancel", async move {
        tauri::async_runtime::spawn_blocking(move || {
            cancel_upload(&app.state::<PersistentStoreState>(), &app.state::<DurableCasJobState>(), &upload_id)
        })
        .await
        .map_err(|error| format!("failed to join CAS upload cancel operation: {error}"))?
    }.await)
}

fn cancel_upload(store: &PersistentStoreState, state: &DurableCasJobState, upload_id: &str) -> Result<(), String> {
    let operation = store.admit_renderer_operation();
    let mut uploads = state.uploads.lock().map_err(|error| format!("CAS upload mutex poisoned: {error}"))?;
    match operation {
        Ok(_operation) => uploads.cancel(upload_id),
        Err(_) if store.maintenance_active() => uploads.release_during_maintenance(upload_id),
        Err(error) => return Err(error.to_string()),
    }
    Ok(())
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_pin_existing(
    app: AppHandle,
    session_id: String,
    content_hash: String,
    byte_size: u64,
    role: CasObjectRole,
) -> Result<(), String> {
    logged("asset_cas_job_pin_existing", async move {
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
    }.await)
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_seal(app: AppHandle, session_id: String) -> Result<(), String> {
    logged("asset_cas_job_seal", async move {
        tauri::async_runtime::spawn_blocking(move || {
            let operation_guard = app
                .state::<PersistentStoreState>()
                .admit_renderer_operation()
                .map_err(|error| error.to_string())?;
            let root = repository_root(&app)?;
            let state = app.state::<DurableCasJobState>();
            state
                .uploads
                .lock()
                .map_err(|error| format!("CAS upload mutex poisoned: {error}"))?
                .cancel_session(&session_id);
            let mut jobs = state
                .jobs
                .lock()
                .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?;
            let job = recover_job(&mut jobs, &root, &session_id)?;
            persistent_store::commands::with_store_mut_admitted(
                app.state::<PersistentStoreState>(),
                &operation_guard,
                |store| {
                    job.seal(
                        store,
                        now_ms().map_err(|message| StoreError::Store { message })?,
                    )
                    .map_err(StoreError::from)
                },
            )
            .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("failed to join CAS job seal operation: {error}"))?
    }.await)
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_job_release(
    app: AppHandle,
    session_id: String,
    outcome: CasReleaseOutcome,
) -> Result<(), String> {
    logged("asset_cas_job_release", async move {
        tauri::async_runtime::spawn_blocking(move || {
            let _operation = app
                .state::<PersistentStoreState>()
                .admit_renderer_operation()
                .map_err(|error| error.to_string())?;
            let root = repository_root(&app)?;
            let state = app.state::<DurableCasJobState>();
            state
                .uploads
                .lock()
                .map_err(|error| format!("CAS upload mutex poisoned: {error}"))?
                .cancel_session(&session_id);
            let mut jobs = state
                .jobs
                .lock()
                .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?;
            release_job(&mut jobs, &root, &session_id, outcome)
        })
        .await
        .map_err(|error| format!("failed to join CAS job release operation: {error}"))?
    }.await)
}

fn release_job(
    jobs: &mut HashMap<String, DurableCasJob>,
    root: &std::path::Path,
    session_id: &str,
    outcome: CasReleaseOutcome,
) -> Result<(), String> {
    if !jobs.contains_key(session_id) {
        match DurableCasJob::open(root, session_id) {
            Ok(job) => { jobs.insert(session_id.to_owned(), job); }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        }
    }
    let result = recover_job(jobs, root, session_id)?
        .release(outcome)
        .map_err(|error| error.to_string());
    if jobs.get(session_id).is_some_and(DurableCasJob::is_released) {
        jobs.remove(session_id);
    }
    result
}

pub(crate) fn with_unsealed_content_session<T>(
    app: &AppHandle,
    session_id: &str,
    operation: impl FnOnce(&DurableCasJob) -> Result<T, crate::native_file_jobs::NativeJobError>,
) -> Result<T, crate::native_file_jobs::NativeJobError> {
    with_content_session(app, session_id, |job| {
        if job.is_sealed() || job.pin_count() != 0 {
            return Err(crate::native_file_jobs::NativeJobError::new("invalid-input", "Native content is not awaiting asset mapping"));
        }
        operation(job)
    })
}

pub(crate) fn with_content_session<T>(
    app: &AppHandle,
    session_id: &str,
    operation: impl FnOnce(&DurableCasJob) -> Result<T, crate::native_file_jobs::NativeJobError>,
) -> Result<T, crate::native_file_jobs::NativeJobError> {
    use crate::native_file_jobs::NativeJobError;
    let root = repository_root(app).map_err(|error| NativeJobError::new("store-error", error))?;
    let state = app.state::<DurableCasJobState>();
    with_content_session_at_root(&state, &root, session_id, operation)
}

fn with_content_session_at_root<T>(
    state: &DurableCasJobState,
    root: &std::path::Path,
    session_id: &str,
    operation: impl FnOnce(&DurableCasJob) -> Result<T, crate::native_file_jobs::NativeJobError>,
) -> Result<T, crate::native_file_jobs::NativeJobError> {
    use crate::native_file_jobs::NativeJobError;
    let mut jobs = state.jobs.lock().map_err(|error| NativeJobError::new("store-error", error.to_string()))?;
    let job = recover_job(&mut jobs, root, session_id).map_err(|error| NativeJobError::new("invalid-input", error))?;
    if job.kind() != CasJobKind::CardOrModuleContentImport || job.is_released() {
        return Err(NativeJobError::new("invalid-input", "Native content custody is unavailable"));
    }
    operation(job)
}

/// The largest part one CAS read command returns.
pub(crate) const CAS_READ_PART_BYTES: u64 = 1024 * 1024;
const CAS_OBJECT_MISSING: &str = "cas-object-missing";
const CAS_OBJECT_SIZE_MISMATCH: &str = "cas-object-size-mismatch";

/// Reads one part of an object of `expected_size` bytes. Nothing outlives the
/// call: the file is opened and closed inside it.
#[tauri::command(async)]
pub(crate) async fn asset_cas_read_bytes(
    app: AppHandle,
    content_hash: String,
    start: u64,
    end_exclusive: u64,
    expected_size: u64,
) -> Result<tauri::ipc::Response, String> {
    let result = async {
        let dispatched = app
            .state::<PersistentStoreState>()
            .renderer_dispatch_epoch()
            .map_err(|error| error.to_string())?;
        check_cas_read_range(start, end_exclusive, expected_size)?;
        let root = repository_root(&app)?;
        tauri::async_runtime::spawn_blocking(move || {
            read_cas_part(&app.state::<PersistentStoreState>(), dispatched, &root, &content_hash, start, end_exclusive, expected_size)
                .map(tauri::ipc::Response::new)
        })
        .await
        .map_err(|error| format!("failed to join CAS read: {error}"))?
    }
    .await;
    match result {
        // Remote-only aliases make misses routine, so a miss is not a failure.
        Err(code) if code == CAS_OBJECT_MISSING => Err(code),
        result => logged("asset_cas_read_bytes", result),
    }
}

fn check_cas_read_range(start: u64, end_exclusive: u64, expected_size: u64) -> Result<(), String> {
    if end_exclusive < start || end_exclusive - start > CAS_READ_PART_BYTES || end_exclusive > expected_size {
        return Err("invalid CAS read range".to_owned());
    }
    Ok(())
}

fn read_cas_part(
    store: &PersistentStoreState,
    dispatched: u64,
    root: &std::path::Path,
    content_hash: &str,
    start: u64,
    end_exclusive: u64,
    expected_size: u64,
) -> Result<Vec<u8>, String> {
    check_cas_read_range(start, end_exclusive, expected_size)?;
    let _operation = store
        .admit_renderer_operation_dispatched_at(dispatched)
        .map_err(|error| error.to_string())?;
    let part = PayloadCas::new(root)
        .and_then(|cas| cas.read_object_part(content_hash, start, end_exclusive, expected_size))
        .map_err(|error| error.to_string())?;
    match part {
        ObjectPart::Bytes(bytes) => Ok(bytes),
        ObjectPart::Missing => Err(CAS_OBJECT_MISSING.to_owned()),
        ObjectPart::SizeMismatch => Err(CAS_OBJECT_SIZE_MISMATCH.to_owned()),
    }
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_stat_object(
    app: AppHandle,
    content_hash: String,
) -> Result<Option<u64>, String> {
    logged("asset_cas_stat_object", async move {
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
    }.await)
}

#[tauri::command(async)]
pub(crate) async fn asset_remote_stat_object(
    app: AppHandle,
    content_hash: String,
) -> Result<Option<u64>, String> {
    logged("asset_remote_stat_object", async move {
        let root = repository_root(&app)?;
        tauri::async_runtime::spawn_blocking(move || {
            crate::server_sync::residency::Residency::open(&root)
                .and_then(|store| store.object(&content_hash, None))
                .map(|object| object.map(|object| object.size))
                .map_err(|error| error.code)
        })
        .await
        .map_err(|_| "remote-stat-unavailable".to_owned())?
    }.await)
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_stat_objects(
    app: AppHandle,
    content_hashes: Vec<String>,
) -> Result<Vec<Option<u64>>, String> {
    if content_hashes.len() > 512 { return Err("asset-stat-batch-too-large".to_owned()); }
    logged("asset_cas_stat_objects", async move {
        let root = repository_root(&app)?;
        tauri::async_runtime::spawn_blocking(move || {
            let _operation = app.state::<PersistentStoreState>()
                .admit_renderer_operation().map_err(|error| error.to_string())?;
            PayloadCas::new(&root)
                .and_then(|cas| cas.stat_objects(content_hashes.iter().map(String::as_str)))
                .map_err(|error| error.to_string())
        }).await.map_err(|error| format!("failed to join CAS batch stat operation: {error}"))?
    }.await)
}

#[tauri::command(async)]
pub(crate) async fn asset_remote_stat_objects(
    app: AppHandle,
    content_hashes: Vec<String>,
) -> Result<Vec<Option<u64>>, String> {
    if content_hashes.len() > 512 { return Err("asset-stat-batch-too-large".to_owned()); }
    logged("asset_remote_stat_objects", async move {
        let root = repository_root(&app)?;
        tauri::async_runtime::spawn_blocking(move || {
            let _operation = app.state::<PersistentStoreState>()
                .admit_renderer_operation().map_err(|error| error.to_string())?;
            let store = crate::server_sync::residency::Residency::open(&root).map_err(|error| error.code)?;
            content_hashes.iter().map(|hash| store.object(hash, None)
                .map(|object| object.map(|object| object.size)).map_err(|error| error.code)).collect()
        }).await.map_err(|_| "remote-stat-unavailable".to_owned())?
    }.await)
}

#[tauri::command(async)]
pub(crate) async fn asset_remote_hydrate_object(
    app: AppHandle,
    content_hash: String,
) -> Result<Option<u64>, String> {
    logged("asset_remote_hydrate_object", async move {
        let root = repository_root(&app)?;
        tauri::async_runtime::spawn_blocking(move || {
            let _operation = app
                .state::<PersistentStoreState>()
                .admit_renderer_operation()
                .map_err(|error| error.to_string())?;
            hydrate_remote_object(&root, &content_hash)
        })
        .await
        .map_err(|_| "remote-read-unavailable".to_owned())?
    }.await)
}

fn hydrate_remote_object(root: &std::path::Path, content_hash: &str) -> Result<Option<u64>, String> {
    crate::server_sync::residency::open_or_hydrate(root, content_hash)
        .map_err(|error| error.code)?
        .map(|file| file.metadata().map(|metadata| metadata.len()).map_err(|error| error.to_string()))
        .transpose()
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
    logged("asset_cas_job_finalize_content", async move {
        let native_jobs = NativeFileJobState::clone(&native_jobs);
        tauri::async_runtime::spawn_blocking(move || {
            let operation_guard = app
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
            let content_assets = native_jobs
                .content_asset_receipt(&session_id)
                .map_err(|error| format!("{}: {}", error.code, error.message))?
                .into_iter()
                .map(|(object_hash, byte_size)| ContentDirectObject {
                    object_hash,
                    byte_size,
                })
                .collect::<Vec<_>>();
            let job = recover_job(&mut jobs, &root, &session_id)?;
            persistent_store::commands::with_store_mut_admitted(
                app.state::<PersistentStoreState>(),
                &operation_guard,
                |store| {
                    finalize_content_job(
                        job,
                        &cas,
                        store,
                        &owner_manifest,
                        &content_assets,
                        now_ms().map_err(|message| StoreError::Store { message })?,
                    )
                    .map_err(StoreError::from)
                },
            )
            .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("failed to join content CAS finalization: {error}"))?
    }.await)
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
    logged("asset_cas_job_seal_prepared_content", async move {
        let native_jobs = NativeFileJobState::clone(&native_jobs);
        tauri::async_runtime::spawn_blocking(move || {
            let operation_guard = app
                .state::<PersistentStoreState>()
                .admit_renderer_operation()
                .map_err(|error| error.to_string())?;
            let root = repository_root(&app)?;
            let state = app.state::<DurableCasJobState>();
            let mut jobs = state
                .jobs
                .lock()
                .map_err(|error| format!("CAS job session mutex poisoned: {error}"))?;
            persistent_store::commands::with_store_mut_admitted(
                app.state::<PersistentStoreState>(),
                &operation_guard,
                |store| {
                    seal_prepared_content_job_by_id(
                        &native_jobs,
                        &root,
                        &mut jobs,
                        store,
                        &session_id,
                        now_ms().map_err(|message| StoreError::Store { message })?,
                    )
                    .map_err(|message| StoreError::Store { message })
                },
            )
            .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| format!("failed to join prepared content CAS seal: {error}"))?
    }.await)
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

    #[test]
    fn remote_hydration_returns_only_size_and_publishes_verified_cas_bytes() {
        let directory = TempDir::new().unwrap();
        let _store = PersistentStore::open(directory.path()).unwrap();
        let body = vec![73_u8; 200 * 1024];
        let hash = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&body));
        crate::server_sync::residency::test_remote::hold(directory.path(), &[(&hash, body.len() as u64)]);
        crate::server_sync::residency::test_remote::serve(directory.path(), &hash, body.clone());
        assert_eq!(hydrate_remote_object(directory.path(), &hash).unwrap(), Some(body.len() as u64));
        assert_eq!(PayloadCas::new(directory.path()).unwrap().read_object(&hash).unwrap(), Some(body));
    }

    #[test]
    fn release_retries_settle_both_unsealed_and_sealed_sessions_without_gc_blockers() {
        for sealed in [false, true] {
            let directory = TempDir::new().unwrap();
            let mut store = PersistentStore::open(directory.path()).unwrap();
            let cas = PayloadCas::new(directory.path()).unwrap();
            let mut job = DurableCasJob::begin(directory.path(), "activation-failure",
                CasJobKind::DirectAssetOrInlayWrite, crate::asset_repository::job_pins::CasJobOwner::for_test(), 1).unwrap();
            let object = job.prepare_bytes(&cas, b"synthetic activation", CasObjectRole::DirectObject).unwrap();
            if sealed { job.seal(&mut store, 0).unwrap(); }
            let mut jobs = HashMap::from([("activation-failure".to_owned(), job)]);
            release_job(&mut jobs, directory.path(), "activation-failure", CasReleaseOutcome::Aborted).unwrap();
            release_job(&mut jobs, directory.path(), "activation-failure", CasReleaseOutcome::Aborted).unwrap();
            assert!(jobs.is_empty());
            let roots = collect_durable_cas_job_roots(directory.path());
            assert!(roots.blockers.is_empty());
            assert!(roots.object_hashes.is_empty());
            assert!(cas.stat_object(&object.content_hash).unwrap().is_some());
        }
    }

    fn native_status(kind: NativeJobKind, state: NativeJobState) -> NativeJobStatus {
        let mut status = crate::native_file_jobs::JobRegistry::default().create(kind).unwrap().status();
        status.state = state;
        status
    }

    #[test]
    fn prepared_content_custody_allows_repeated_reads_until_release() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let mut store = PersistentStore::open(root).unwrap();
        let state = DurableCasJobState::default();
        let mut job = DurableCasJob::begin(root, "content-custody", CasJobKind::CardOrModuleContentImport,
            CasJobOwner::content_import("content-custody"), 1).unwrap();
        let read = || with_content_session_at_root(&state, root, "content-custody", |job| Ok(job.pin_count()));
        drop(job);
        assert_eq!(read().unwrap(), 0);
        assert_eq!(read().unwrap(), 0);
        {
            let mut jobs = state.jobs.lock().unwrap();
            let job = jobs.get_mut("content-custody").unwrap();
            job.prepare_bytes(&PayloadCas::new(root).unwrap(), b"synthetic content", CasObjectRole::DirectObject).unwrap();
            job.seal(&mut store, 1).unwrap();
        }
        assert_eq!(read().unwrap(), 1);
        assert_eq!(read().unwrap(), 1);
        release_job(&mut state.jobs.lock().unwrap(), root, "content-custody", CasReleaseOutcome::Committed).unwrap();
        assert!(read().is_err());
        job = DurableCasJob::begin(root, "content-custody", CasJobKind::DirectAssetOrInlayWrite,
            CasJobOwner::page_write("content-custody"), 1).unwrap();
        drop(job);
        assert!(read().is_err());
        with_content_session_at_root(&state, root, "missing", |_| Ok(())).unwrap_err();
    }

    #[test]
    fn prepared_content_read_holds_custody_until_the_callback_finishes() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;
        let directory = TempDir::new().unwrap();
        let root = directory.path().to_owned();
        let _store = PersistentStore::open(&root).unwrap();
        drop(DurableCasJob::begin(&root, "content-read", CasJobKind::CardOrModuleContentImport,
            CasJobOwner::content_import("content-read"), 1).unwrap());
        let state = Arc::new(DurableCasJobState::default());
        let (entered, wait_entered) = mpsc::channel();
        let (resume, wait_resume) = mpsc::channel();
        let reader = {
            let state = state.clone(); let root = root.clone();
            std::thread::spawn(move || with_content_session_at_root(&state, &root, "content-read", |_| {
                entered.send(()).unwrap(); wait_resume.recv().unwrap(); Ok(())
            }))
        };
        wait_entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let (released, wait_released) = mpsc::channel();
        let releaser = {
            let state = state.clone(); let root = root.clone();
            std::thread::spawn(move || {
                let result = release_job(&mut state.jobs.lock().unwrap(), &root, "content-read", CasReleaseOutcome::Aborted);
                released.send(result).unwrap();
            })
        };
        assert!(wait_released.recv_timeout(Duration::from_millis(100)).is_err());
        resume.send(()).unwrap();
        reader.join().unwrap().unwrap();
        wait_released.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        releaser.join().unwrap();
        assert!(with_content_session_at_root(&state, &root, "content-read", |_| Ok(())).is_err());
    }

    fn sweep(
        state: &DurableCasJobState,
        store: &PersistentStore,
        native: &[NativeJobStatus],
        device_job: Option<&str>,
    ) {
        let native_jobs = || -> Result<Vec<NativeJobStatus>, String> { Ok(native.to_vec()) };
        let open_store = || store.open_native_job_store().map_err(|error| error.to_string());
        state
            .sweep_after_page_start(store.repository_root(), &CasJobOwnerProbe {
                native_jobs: &native_jobs,
                device_job_owned: &|id| Ok(device_job == Some(id)),
                external_job_active: &|_| Ok(false),
                open_store: &open_store,
            })
            .unwrap();
    }

    fn journal(
        store: &mut PersistentStore,
        id: &str,
        owner: CasJobOwner,
        sealed: bool,
    ) -> (DurableCasJob, String) {
        let root = store.repository_root().to_owned();
        let cas = PayloadCas::new(&root).unwrap();
        let mut job =
            DurableCasJob::begin(&root, id, CasJobKind::DirectAssetOrInlayWrite, owner, 1).unwrap();
        let object = job
            .prepare_bytes(&cas, format!("synthetic {id}").as_bytes(), CasObjectRole::DirectObject)
            .unwrap();
        if sealed {
            job.seal(store, 1).unwrap();
        }
        (job, object.content_hash)
    }

    fn save_publication_segment(root: &std::path::Path, job_id: &str) {
        use crate::external_storage::journal::JobIdentity;
        use crate::persistent_store::external_lww::SealedPublication;
        use crate::persistent_store::sync_selection::CaptureIdentity;
        use risunest_sync_wire::stamp::DecimalU64;
        let publication = SealedPublication {
            target: "synthetic-target".into(),
            writer: "synthetic-writer".into(),
            seq: DecimalU64(1),
            authority: DecimalU64(1),
            captured_at_ms: Some(1),
            object_id: "synthetic-object".into(),
            sha256: String::new(),
            payload_sha256: String::new(),
            payload: String::new(),
            sealed: true,
            entries: Vec::new(),
            bodies: Vec::new(),
            assets: Vec::new(),
            controls: Vec::new(),
            reused_control_catalogs: Vec::new(),
            reused_assets: Vec::new(),
            asset_job: Some(JobIdentity {
                job_id: job_id.to_owned(),
                connection_id: "synthetic-connection".into(),
                repository_id: "synthetic-repository".into(),
                capture_id: "synthetic-capture".into(),
                capture: CaptureIdentity {
                    store_id: "store".into(),
                    library_epoch: "library".into(),
                    generation: "generation".into(),
                    selection_epoch: "selection".into(),
                    revision: 0,
                },
            }),
            data_catalogs: Vec::new(),
            asset_catalogs: Vec::new(),
            resume: None,
            dispatched: false,
            complete: false,
        };
        rusqlite::Connection::open(root.join("persistent/device.sqlite"))
            .unwrap()
            .execute(
                "INSERT INTO external_lww_segments(target,writer,seq,authority,metadata,sealed,complete)
                 VALUES(?1,?2,?3,?4,?5,x'00',0)",
                [
                    publication.target.clone(),
                    publication.writer.clone(),
                    publication.seq.0.to_string(),
                    publication.authority.0.to_string(),
                    serde_json::to_string(&publication).unwrap(),
                ],
            )
            .unwrap();
    }

    fn journal_ids(root: &std::path::Path) -> std::collections::BTreeSet<String> {
        durable_cas_job_ids(root).unwrap().into_iter().collect()
    }

    #[test]
    fn a_page_reload_releases_only_the_journals_whose_work_has_ended() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let mut store = PersistentStore::open(root).unwrap();
        let running_import = native_status(NativeJobKind::PrepareContentImport, NativeJobState::Running);
        let finished_import = native_status(NativeJobKind::PrepareContentImport, NativeJobState::Failed);
        let running_export = native_status(NativeJobKind::ExportPortableBackup, NativeJobState::Running);
        let finished_export = native_status(NativeJobKind::ExportPortableBackup, NativeJobState::Cancelled);
        let published = native_status(NativeJobKind::OfficialPublicationUpload, NativeJobState::Succeeded);
        let native = [
            running_import.clone(),
            finished_import.clone(),
            running_export.clone(),
            finished_export.clone(),
            published.clone(),
        ];
        let state = DurableCasJobState::default();
        let (page_unsealed, _) = journal(&mut store, "page-unsealed", CasJobOwner::page_write("page"), false);
        let (page_sealed, page_object) = journal(&mut store, "page-sealed", CasJobOwner::page_write("page"), true);
        state.jobs.lock().unwrap().extend([
            ("page-unsealed".to_owned(), page_unsealed),
            ("page-sealed".to_owned(), page_sealed),
        ]);
        let mut drop_journal = |id: &str, owner: CasJobOwner, sealed: bool| {
            drop(journal(&mut store, id, owner, sealed));
        };
        drop_journal("page-earlier", CasJobOwner::page_write("earlier-page"), false);
        drop_journal(&running_import.job_id, CasJobOwner::content_import(&running_import.job_id), false);
        drop_journal(&finished_import.job_id, CasJobOwner::content_import(&finished_import.job_id), false);
        drop_journal("import-without-job", CasJobOwner::content_import("import-without-job"), false);
        drop_journal(&running_export.job_id, CasJobOwner::native_file_job(&running_export.job_id), false);
        drop_journal(&finished_export.job_id, CasJobOwner::native_file_job(&finished_export.job_id), false);
        drop_journal("native-without-job", CasJobOwner::native_file_job("native-without-job"), false);
        drop_journal(&published.job_id, CasJobOwner::native_file_job(&published.job_id), true);
        drop_journal("snapshot-export", CasJobOwner::snapshot_export("snapshot-export"), false);
        drop_journal("publication-unsealed", CasJobOwner::external_publication("publication-unsealed"), false);
        drop_journal("publication-unsaved", CasJobOwner::external_publication("publication-unsaved"), true);
        drop_journal("publication-saved", CasJobOwner::external_publication("publication-saved"), true);
        drop_journal("compaction", CasJobOwner::external_compaction("compaction"), false);
        drop_journal("restore-without-job", CasJobOwner::external_restore("restore-without-job"), false);
        save_publication_segment(root, "publication-saved");
        let (held, _) = journal(&mut store, "native-held", CasJobOwner::native_file_job("native-held"), false);

        sweep(&state, &store, &native, None);

        assert!(state.jobs.lock().unwrap().is_empty());
        assert_eq!(
            journal_ids(root),
            [
                running_import.job_id.as_str(),
                running_export.job_id.as_str(),
                published.job_id.as_str(),
                "publication-saved",
                "native-held",
            ]
            .map(str::to_owned)
            .into(),
        );
        assert!(!collect_durable_cas_job_roots(root).object_hashes.contains(&page_object));
        drop(held);
    }

    #[test]
    fn a_device_restore_session_keeps_only_its_own_finished_native_job() {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let finished = native_status(NativeJobKind::RestoreBlockRisuSave, NativeJobState::Failed);
        drop(journal(&mut store, &finished.job_id, CasJobOwner::native_file_job(&finished.job_id), false));
        drop(journal(&mut store, "unrelated", CasJobOwner::native_file_job("unrelated"), false));
        let state = DurableCasJobState::default();

        sweep(&state, &store, &[finished.clone()], Some(&finished.job_id));
        assert_eq!(journal_ids(directory.path()), [finished.job_id.clone()].into());

        sweep(&state, &store, &[finished], None);
        assert!(journal_ids(directory.path()).is_empty());
    }

    #[test]
    fn a_compaction_worker_keeps_its_journal_only_until_its_actual_handle_settles() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let mut store = PersistentStore::open(root).unwrap();
        let (worker, _) = journal(&mut store, "compaction-worker", CasJobOwner::external_compaction("compaction-worker"), false);
        let state = DurableCasJobState::default();
        sweep(&state, &store, &[], None);
        assert_eq!(journal_ids(root), ["compaction-worker".to_owned()].into());
        drop(worker);
        sweep(&state, &store, &[], None);
        assert!(journal_ids(root).is_empty());
    }

    #[test]
    fn a_device_restore_after_restart_keeps_only_its_exact_journal() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let mut store = PersistentStore::open(root).unwrap();
        for id in ["device-restore", "ended-native"] {
            drop(journal(&mut store, id, CasJobOwner::from_another_process(CasJobOwnerKind::NativeFileJob, id), false));
        }
        let state = DurableCasJobState::default();
        sweep(&state, &store, &[], Some("device-restore"));
        assert_eq!(journal_ids(root), ["device-restore".to_owned()].into());
        sweep(&state, &store, &[], None);
        assert!(journal_ids(root).is_empty());
    }

    #[test]
    fn unreadable_device_ownership_keeps_native_journals_for_retry() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let mut store = PersistentStore::open(root).unwrap();
        drop(journal(&mut store, "unknown-device", CasJobOwner::native_file_job("unknown-device"), false));
        let open_store = || store.open_native_job_store().map_err(|error| error.to_string());
        let probe = CasJobOwnerProbe { native_jobs:&|| Ok(Vec::new()),
            device_job_owned:&|_| Err("synthetic unreadable session".into()),
            external_job_active:&|_| Ok(false), open_store:&open_store };
        assert!(DurableCasJobState::default().sweep_after_page_start(root, &probe).is_err());
        assert_eq!(journal_ids(root), ["unknown-device".to_owned()].into());
    }

    #[test]
    fn opening_the_store_releases_the_journals_a_page_start_could_not_judge() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        {
            let mut store = PersistentStore::open(root).unwrap();
            // A saved publication whose segment row is gone has ended, which only the store shows.
            let owner = CasJobOwner::from_another_process(CasJobOwnerKind::ExternalPublication, "publication");
            drop(journal(&mut store, "page-start-publication", owner, true));
        }
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        app.manage(crate::app_paths::AppPaths {
            data: root.to_owned(),
            webview: None,
            logs: root.join("logs"),
            cache: root.join("cache"),
            cleanup_control: root.join("cleanup"),
            tauri_derived: Vec::new(),
            install: None,
            integration: None,
        });
        app.manage(PersistentStoreState::unopened_without_purge());
        app.manage(DurableCasJobState::default());

        // A page that starts before the renderer opens the store cannot judge the journal.
        let error = release_abandoned(app.handle(), AbandonedCasJobs::snapshot(root)).unwrap_err();
        assert!(error.contains("has not been opened"), "{error}");
        assert_eq!(journal_ids(root), ["page-start-publication".to_owned()].into());
        // That attempt is retried below, so it is not logged as a failure.
        release_logged(app.handle(), AbandonedCasJobs::snapshot(root));
        assert!(!crate::native_log::global_state().tail(None).iter()
            .any(|entry| entry.message.contains("CAS jobs: page-start-publication")));

        persistent_store::commands::open_for_renderer(app.handle(), &app.state()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while !journal_ids(root).is_empty() {
            assert!(Instant::now() < deadline, "the journal stayed after the store opened");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_settlement_sweep_preserves_the_current_pages_open_sessions() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let mut store = PersistentStore::open(root).unwrap();
        let state = DurableCasJobState::default();
        let (held, _) = journal(&mut store, "current-page", CasJobOwner::page_write("current-page"), false);
        state.jobs.lock().unwrap().insert("current-page".into(), held);
        let open_store = || store.open_native_job_store().map_err(|error| error.to_string());
        AbandonedCasJobs::snapshot(root).release(&CasJobOwnerProbe {native_jobs:&||Ok(Vec::new()),
            device_job_owned:&|_|Ok(false), external_job_active:&|_|Ok(false), open_store:&open_store}).unwrap();
        assert_eq!(journal_ids(root), ["current-page".to_owned()].into());
        assert_eq!(state.jobs.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_first_sweep_after_a_restart_releases_what_no_restarted_work_can_use() {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let mut store = PersistentStore::open(root).unwrap();
        // A record of this process with the same ID does not make an earlier
        // run's journal its own.
        let running = native_status(NativeJobKind::ExportPortableBackup, NativeJobState::Running);
        let earlier = CasJobOwner::from_another_process;
        let mut drop_journal = |id: &str, owner: CasJobOwner, sealed: bool| {
            drop(journal(&mut store, id, owner, sealed));
        };
        drop_journal("page-write", earlier(CasJobOwnerKind::PageWrite, "page"), false);
        drop_journal("content-import", earlier(CasJobOwnerKind::ContentImport, &running.job_id), false);
        drop_journal(&running.job_id, earlier(CasJobOwnerKind::NativeFileJob, &running.job_id), false);
        drop_journal("native-sealed", earlier(CasJobOwnerKind::NativeFileJob, "native-sealed"), true);
        drop_journal("snapshot-export", earlier(CasJobOwnerKind::SnapshotExport, "snapshot-export"), false);
        drop_journal("compaction", earlier(CasJobOwnerKind::ExternalCompaction, "compaction"), false);
        drop_journal("publication-unsealed", earlier(CasJobOwnerKind::ExternalPublication, "publication-unsealed"), false);
        drop_journal("publication-unsaved", earlier(CasJobOwnerKind::ExternalPublication, "publication-unsaved"), true);
        drop_journal("publication-saved", earlier(CasJobOwnerKind::ExternalPublication, "publication-saved"), true);
        drop_journal("restore-without-job", earlier(CasJobOwnerKind::ExternalRestore, "restore-without-job"), false);
        save_publication_segment(root, "publication-saved");
        let (held, _) = journal(&mut store, "held", earlier(CasJobOwnerKind::NativeFileJob, "held"), true);
        assert!(!collect_durable_cas_job_roots(root).blockers.is_empty());

        sweep(&DurableCasJobState::default(), &store, &[running], None);

        assert_eq!(
            journal_ids(root),
            ["held", "publication-saved"].map(str::to_owned).into(),
        );
        assert!(collect_durable_cas_job_roots(root).blockers.is_empty());
        store.asset_residency_status().map_err(|error| error.code).unwrap();
        drop(held);
    }

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

    #[test]
    fn cas_upload_pool_enforces_offsets_lengths_and_cleanup() {
        let directory = TempDir::new().expect("create upload pool directory");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let upload_id = uuid::Uuid::new_v4().to_string();
        let mut pool = CasUploadPool::default();
        pool.open(
            &cas,
            upload_id.clone(),
            "session-1".to_owned(),
            CasObjectRole::DirectObject,
            CAS_UPLOAD_CHUNK_BYTES as u64 + 3,
        )
        .expect("open bounded upload");

        assert!(pool.append(&upload_id, 1, b"wrong offset").is_err());
        assert!(pool
            .append(&upload_id, 0, &vec![0; CAS_UPLOAD_CHUNK_BYTES + 1])
            .is_err());
        assert_eq!(
            pool.append(&upload_id, 0, &vec![7; CAS_UPLOAD_CHUNK_BYTES])
                .expect("append full chunk"),
            CAS_UPLOAD_CHUNK_BYTES as u64
        );
        assert!(pool.take_complete(&upload_id).is_err());
        assert_eq!(
            pool.append(&upload_id, CAS_UPLOAD_CHUNK_BYTES as u64, &[8, 9, 10])
                .expect("append final chunk"),
            CAS_UPLOAD_CHUNK_BYTES as u64 + 3
        );
        let late_sender = pool.get(&upload_id).unwrap();
        let upload = pool.take_complete(&upload_id).expect("take completed upload");
        let prepared = upload.finish(&cas).expect("publish received file");
        let bytes = std::fs::read(directory.path().join(prepared.physical_key)).unwrap();
        assert!(pool.append(&upload_id, CAS_UPLOAD_CHUNK_BYTES as u64 + 3, &[11]).is_err());
        assert!(late_sender.append(CAS_UPLOAD_CHUNK_BYTES as u64 + 3, &[11]).is_err());
        assert_eq!(bytes.len(), CAS_UPLOAD_CHUNK_BYTES + 3);
        assert_eq!(&bytes[..3], &[7, 7, 7]);
        assert_eq!(&bytes[CAS_UPLOAD_CHUNK_BYTES..], &[8, 9, 10]);
        assert_eq!(pool.reserved_bytes, 0);
    }

    #[test]
    fn binary_upload_routes_bounded_parts_to_the_owned_file_and_releases_the_receiver() {
        use crate::asset_byte_transport::{append_android, register_android_receiver, BINARY_PART_BYTES};
        let directory = TempDir::new().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let mut pool = CasUploadPool::default();
        let bytes: Vec<u8> = (0..BINARY_PART_BYTES + 17).map(|i| (i % 256) as u8).collect();
        pool.open_with_capacity(&cas, id.clone(), "binary-session".to_owned(), CasObjectRole::DirectObject,
            bytes.len() as u64, BINARY_PART_BYTES, None).unwrap();
        let upload = pool.get(&id).unwrap();
        let weak = Arc::downgrade(&upload);
        let receiver = register_android_receiver(&id, Arc::new(move |offset, bytes| {
            weak.upgrade().ok_or("closed upload")?.append(offset, bytes).map_err(|error| error.to_string())
        })).unwrap();
        assert!(register_android_receiver(&id, receiver.clone()).is_err());
        assert_eq!(append_android(&id, 0, &bytes[..BINARY_PART_BYTES]).unwrap().unwrap(), BINARY_PART_BYTES as u64);
        assert!(append_android(&id, 0, &bytes[..1]).unwrap().is_err());
        assert_eq!(append_android(&id, BINARY_PART_BYTES as u64, &bytes[BINARY_PART_BYTES..]).unwrap().unwrap(), bytes.len() as u64);
        let staged = pool.take_complete(&id).unwrap();
        let prepared = staged.finish(&cas).unwrap();
        assert!(append_android(&id, bytes.len() as u64, &[0]).unwrap().is_err());
        assert_eq!(cas.read_object(&prepared.content_hash).unwrap().unwrap(), bytes);
        drop(receiver);
        assert!(append_android(&id, 0, &[0]).is_none());
        assert_eq!(pool.reserved_bytes, 0);
    }

    #[test]
    fn cas_part_reads_check_the_range_size_and_admission_and_leave_no_handle_open() {
        use crate::asset_repository::body_io::{register_object_purpose, reset_body_io, take_body_io, BodyPurpose};
        let directory = TempDir::new().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let part = CAS_READ_PART_BYTES as usize;
        let bytes: Vec<u8> = (0..part + 7).map(|i| (i % 251) as u8).collect();
        let size = bytes.len() as u64;
        let prepared = cas.prepare_bytes(&bytes).unwrap();
        let hash = prepared.content_hash.as_str();
        let store = PersistentStoreState::default();
        let read = |start: u64, end: u64, expected: u64| {
            read_cas_part(&store, store.renderer_dispatch_epoch().unwrap(), directory.path(), hash, start, end, expected)
        };
        reset_body_io();
        register_object_purpose(hash, BodyPurpose::Asset);

        assert_eq!(read(0, part as u64, size).unwrap(), bytes[..part]);
        assert_eq!(read(part as u64, size, size).unwrap(), bytes[part..]);
        assert_eq!(read(size, size, size).unwrap(), Vec::<u8>::new());
        assert_eq!(read(0, 10, size + 1).unwrap_err(), CAS_OBJECT_SIZE_MISMATCH);
        assert!(read(0, part as u64 + 1, size).is_err());
        assert!(read(2, 1, size).is_err());
        assert!(read(0, size, size - 1).is_err());
        assert_eq!(read(0, 1, 1).unwrap_err(), CAS_OBJECT_SIZE_MISMATCH);

        let dispatched = store.renderer_dispatch_epoch().unwrap();
        let maintenance = store.acquire_raw_capture().unwrap();
        assert!(read(0, 1, size).is_err());
        drop(maintenance);
        assert!(read_cas_part(&store, dispatched, directory.path(), hash, 0, 1, size).is_err());
        assert_eq!(read(0, 1, size).unwrap(), bytes[..1]);

        let work = take_body_io();
        assert!(work.complete());
        assert_eq!(work.objects[hash].work.outstanding_readers, 0);
        std::fs::remove_file(directory.path().join(&prepared.physical_key)).unwrap();
        assert_eq!(read(0, 0, size).unwrap_err(), CAS_OBJECT_MISSING);
        assert_eq!(read(0, 1, size).unwrap_err(), CAS_OBJECT_MISSING);
    }

    #[test]
    fn cas_upload_pool_bounds_concurrency_and_releases_reserved_bytes_on_cancel() {
        let directory = TempDir::new().expect("create upload quota directory");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let mut pool = CasUploadPool::default();
        let mut ids = Vec::new();
        for _ in 0..MAX_CONCURRENT_CAS_UPLOADS {
            let id = uuid::Uuid::new_v4().to_string();
            pool.open(
                &cas,
                id.clone(),
                "session-quota".to_owned(),
                CasObjectRole::DirectObject,
                MAX_TOTAL_CAS_UPLOAD_BYTES / MAX_CONCURRENT_CAS_UPLOADS as u64,
            )
            .expect("reserve upload quota");
            ids.push(id);
        }
        assert!(pool
            .open(
                &cas,
                uuid::Uuid::new_v4().to_string(),
                "session-quota".to_owned(),
                CasObjectRole::DirectObject,
                CAS_UPLOAD_CHUNK_BYTES as u64 + 1,
            )
            .is_err());
        pool.cancel_session("session-quota");
        assert!(pool.uploads.is_empty());
        assert_eq!(pool.reserved_bytes, 0);
        assert_eq!(ids.len(), MAX_CONCURRENT_CAS_UPLOADS);
    }

    #[test]
    fn cas_upload_pool_clear_drops_tempfiles_and_releases_reservations() {
        let directory = TempDir::new().expect("create upload reset directory");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let mut pool = CasUploadPool::default();
        let upload_id = uuid::Uuid::new_v4().to_string();
        pool.open(
            &cas,
            upload_id.clone(),
            "session-reset".to_owned(),
            CasObjectRole::DirectObject,
            CAS_UPLOAD_CHUNK_BYTES as u64 + 1,
        )
        .expect("open upload for reset");
        let staging_path = pool.uploads[&upload_id].file.lock().unwrap().as_ref().unwrap().path().to_path_buf();
        assert!(staging_path.is_file());
        pool.clear();
        assert!(pool.uploads.is_empty());
        assert_eq!(pool.reserved_bytes, 0);
        assert!(!staging_path.exists());
    }

    #[test]
    fn an_upload_cancelled_during_maintenance_releases_its_quota_and_receiver_and_keeps_its_file() {
        use crate::asset_byte_transport::{append_android, register_android_receiver};
        let directory = TempDir::new().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let store = PersistentStoreState::default();
        let state = DurableCasJobState::default();
        let upload_id = uuid::Uuid::new_v4().to_string();
        let receiver = register_android_receiver(&upload_id, Arc::new(|offset: u64, _: &[u8]| Ok(offset))).unwrap();
        state.uploads.lock().unwrap().open_with_capacity(&cas, upload_id.clone(), "session-maintenance".to_owned(),
            CasObjectRole::DirectObject, CAS_UPLOAD_CHUNK_BYTES as u64 + 1, CAS_UPLOAD_CHUNK_BYTES, Some(receiver)).unwrap();
        let staging_path = state.uploads.lock().unwrap().uploads[&upload_id].file.lock().unwrap().as_ref().unwrap().path().to_path_buf();
        let maintenance = store.acquire_raw_capture().unwrap();
        cancel_upload(&store, &state, &upload_id).unwrap();
        assert_eq!(state.uploads.lock().unwrap().reserved_bytes, 0);
        assert!(append_android(&upload_id, 0, &[0]).is_none());
        assert!(staging_path.is_file());
        drop(maintenance);
        cancel_upload(&store, &state, &upload_id).unwrap();
        assert_eq!(state.uploads.lock().unwrap().reserved_bytes, 0);
        state.clear_uploads().unwrap();
        assert!(!staging_path.exists());
    }

    fn begin_content_job(root: &std::path::Path, id: &str) -> DurableCasJob {
        DurableCasJob::begin(root, id, CasJobKind::CardOrModuleContentImport, crate::asset_repository::job_pins::CasJobOwner::for_test(), 1).unwrap()
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
            crate::asset_repository::job_pins::CasJobOwner::for_test(),
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
