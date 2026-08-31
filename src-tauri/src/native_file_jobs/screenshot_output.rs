use super::{NativeFileJobStarted, NativeJobError};
use crate::persistent_store::export::destination::{
    self, DestinationWriteError, DestinationWriteResult,
};
use crate::trust_boundary::sync_directory;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tauri::State;
use uuid::Uuid;

pub(crate) const MAX_SCREENSHOT_OUTPUT_APPEND_BYTES: usize = 64 * 1024;
const OWNERSHIP_FILE: &str = "ownership";
const SPOOL_FILE: &str = "archive.zip.part";
const READY_FILE: &str = "ready";
pub(crate) const READY_HANDOFF_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ScreenshotOutputCancelOutcome {
    Requested,
    TooLate,
    Missing,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScreenshotOutputPublished {
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
    pub(crate) warning_codes: Vec<String>,
    pub(crate) source_path: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ScreenshotOutputState {
    root: PathBuf,
    jobs: Arc<Mutex<HashMap<String, Arc<ScreenshotOutputJob>>>>,
    capability_error: Option<NativeJobError>,
}

impl ScreenshotOutputState {
    pub(crate) fn initialize(root: PathBuf) -> Self {
        let capability_error = initialize_root(&root).err();
        Self {
            root,
            jobs: Arc::new(Mutex::new(HashMap::new())),
            capability_error,
        }
    }

    pub(crate) fn start(
        &self,
        destination: Option<PathBuf>,
    ) -> Result<NativeFileJobStarted, NativeJobError> {
        if let Some(error) = &self.capability_error {
            return Err(error.clone());
        }
        let destination = destination.map(validate_destination).transpose()?;
        let job_id = Uuid::new_v4().to_string();
        let owned_directory = self.root.join(&job_id);
        fs::create_dir(&owned_directory).map_err(|error| {
            io_error(
                "destination-write-failed",
                "create screenshot output directory",
                error,
            )
        })?;
        let created = (|| {
            sync_directory(&self.root).map_err(|error| {
                io_error(
                    "destination-write-failed",
                    "sync screenshot output root",
                    error,
                )
            })?;
            let mut ownership = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(owned_directory.join(OWNERSHIP_FILE))
                .map_err(|error| {
                    io_error(
                        "destination-write-failed",
                        "create screenshot output ownership",
                        error,
                    )
                })?;
            ownership.write_all(job_id.as_bytes()).map_err(|error| {
                io_error(
                    "destination-write-failed",
                    "write screenshot output ownership",
                    error,
                )
            })?;
            ownership.sync_all().map_err(|error| {
                io_error(
                    "destination-write-failed",
                    "sync screenshot output ownership",
                    error,
                )
            })?;
            drop(ownership);
            let spool_path = owned_directory.join(SPOOL_FILE);
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&spool_path)
                .map_err(|error| {
                    io_error(
                        "destination-write-failed",
                        "create screenshot output spool",
                        error,
                    )
                })?;
            sync_directory(&owned_directory).map_err(|error| {
                io_error(
                    "destination-write-failed",
                    "sync screenshot output directory",
                    error,
                )
            })?;
            Ok::<_, NativeJobError>(Arc::new(ScreenshotOutputJob {
                id: job_id.clone(),
                owned_directory: owned_directory.clone(),
                spool_path,
                destination,
                cancel_requested: AtomicBool::new(false),
                inner: Mutex::new(ScreenshotOutputJobInner {
                    phase: ScreenshotOutputPhase::Open,
                    file: Some(file),
                }),
            }))
        })();
        let job = match created {
            Ok(job) => job,
            Err(error) => {
                let _ = fs::remove_dir_all(&owned_directory);
                return Err(error);
            }
        };
        self.jobs
            .lock()
            .map_err(registry_error)?
            .insert(job_id.clone(), job);
        Ok(NativeFileJobStarted {
            job_id,
            warning_codes: Vec::new(),
        })
    }

    pub(crate) fn append(&self, job_id: &str, chunk: &[u8]) -> Result<(), NativeJobError> {
        if chunk.len() > MAX_SCREENSHOT_OUTPUT_APPEND_BYTES {
            return Err(NativeJobError::new(
                "invalid-input",
                "screenshot output chunks cannot exceed 64 KiB",
            ));
        }
        let job = self.lookup(job_id)?;
        let write_error = {
            let mut inner = job.inner.lock().map_err(job_error)?;
            if inner.phase != ScreenshotOutputPhase::Open {
                return Err(NativeJobError::new(
                    "invalid-state",
                    "screenshot output is not open for appends",
                ));
            }
            let result = inner
                .file
                .as_mut()
                .ok_or_else(|| NativeJobError::new("invalid-state", "screenshot spool is closed"))?
                .write_all(chunk)
                .map_err(|error| {
                    io_error(
                        "destination-write-failed",
                        "append screenshot output spool",
                        error,
                    )
                });
            if result.is_err() {
                inner.phase = ScreenshotOutputPhase::Cancelled;
                inner.file.take();
            }
            result.err()
        };
        match write_error {
            None => Ok(()),
            Some(error) => self.finish_failure(&job, error),
        }
    }

    pub(crate) fn publish(
        &self,
        job_id: &str,
    ) -> Result<ScreenshotOutputPublished, NativeJobError> {
        let job = self.lookup(job_id)?;
        let mut file = {
            let mut inner = job.inner.lock().map_err(job_error)?;
            if inner.phase != ScreenshotOutputPhase::Open {
                return Err(NativeJobError::new(
                    "invalid-state",
                    "screenshot output cannot be published from its current state",
                ));
            }
            inner.phase = ScreenshotOutputPhase::Publishing;
            inner.file.take().ok_or_else(|| {
                NativeJobError::new("invalid-state", "screenshot output spool is closed")
            })?
        };
        let prepared = file
            .flush()
            .map_err(|error| {
                io_error(
                    "destination-write-failed",
                    "flush screenshot output spool",
                    error,
                )
            })
            .and_then(|()| {
                file.sync_all().map_err(|error| {
                    io_error(
                        "destination-write-failed",
                        "sync screenshot output spool",
                        error,
                    )
                })
            });
        drop(file);
        if let Err(error) = prepared {
            return self.finish_failure(&job, error);
        }
        if let Err(error) = validate_screenshot_archive(&job) {
            return self.finish_failure(&job, error);
        }

        let Some(destination) = &job.destination else {
            return match self.prepare_android_handoff(&job) {
                Ok(prepared) => Ok(prepared),
                Err(error) => self.finish_failure(&job, error),
            };
        };
        let destination_root = destination.parent().ok_or_else(|| {
            NativeJobError::new(
                "invalid-destination",
                "destination directory is unavailable",
            )
        })?;
        let published = destination::write_screenshot_destination_controlled(
            &job.owned_directory,
            &job.spool_path,
            destination_root,
            destination,
            || job.cancel_requested.load(Ordering::Acquire),
            |_| {},
            || {
                let mut inner = job
                    .inner
                    .lock()
                    .map_err(|_| DestinationWriteError::Cancelled)?;
                if job.cancel_requested.load(Ordering::Acquire) {
                    return Err(DestinationWriteError::Cancelled);
                }
                inner.phase = ScreenshotOutputPhase::Finalizing;
                Ok(())
            },
        );
        match published {
            Ok(result) => self.finish_success(&job, result),
            Err(error) => self.finish_failure(&job, destination_error(error)),
        }
    }

    pub(crate) fn cancel(
        &self,
        job_id: &str,
    ) -> Result<ScreenshotOutputCancelOutcome, NativeJobError> {
        let Some(job) = self.lookup_optional(job_id)? else {
            return Ok(ScreenshotOutputCancelOutcome::Missing);
        };
        let cleanup_now = {
            let mut inner = job.inner.lock().map_err(job_error)?;
            match inner.phase {
                ScreenshotOutputPhase::Open => {
                    job.cancel_requested.store(true, Ordering::Release);
                    inner.phase = ScreenshotOutputPhase::Cancelled;
                    inner.file.take();
                    true
                }
                ScreenshotOutputPhase::Publishing => {
                    job.cancel_requested.store(true, Ordering::Release);
                    inner.phase = ScreenshotOutputPhase::Cancelling;
                    false
                }
                ScreenshotOutputPhase::Cancelling | ScreenshotOutputPhase::Cancelled => false,
                ScreenshotOutputPhase::Finalizing => {
                    return Ok(ScreenshotOutputCancelOutcome::TooLate)
                }
                ScreenshotOutputPhase::Handoff => {
                    job.cancel_requested.store(true, Ordering::Release);
                    inner.phase = ScreenshotOutputPhase::Cancelled;
                    true
                }
            }
        };
        if cleanup_now {
            self.remove_and_cleanup(&job)?;
        }
        Ok(ScreenshotOutputCancelOutcome::Requested)
    }

    fn finish_success(
        &self,
        job: &Arc<ScreenshotOutputJob>,
        result: DestinationWriteResult,
    ) -> Result<ScreenshotOutputPublished, NativeJobError> {
        let cleanup = self.remove_and_cleanup(job);
        Ok(ScreenshotOutputPublished {
            bytes: result.bytes,
            sha256: result.sha256,
            warning_codes: cleanup
                .err()
                .map(|_| vec!["cleanup-failed".to_owned()])
                .unwrap_or_default(),
            source_path: None,
        })
    }

    fn prepare_android_handoff(
        &self,
        job: &Arc<ScreenshotOutputJob>,
    ) -> Result<ScreenshotOutputPublished, NativeJobError> {
        let (bytes, sha256) = screenshot_spool_fingerprint(&job)?;
        write_owned_marker(&job.owned_directory, READY_FILE, &job.id)?;
        let cancelled = {
            let mut inner = job.inner.lock().map_err(job_error)?;
            if job.cancel_requested.load(Ordering::Acquire)
                || inner.phase == ScreenshotOutputPhase::Cancelling
            {
                true
            } else {
                inner.phase = ScreenshotOutputPhase::Handoff;
                false
            }
        };
        if cancelled {
            return Err(NativeJobError::new(
                "cancelled",
                "screenshot output publication was cancelled",
            ));
        }
        Ok(ScreenshotOutputPublished {
            bytes,
            sha256,
            warning_codes: Vec::new(),
            source_path: Some(job.spool_path.to_string_lossy().into_owned()),
        })
    }

    pub(crate) fn release(&self, job_id: &str) -> Result<(), NativeJobError> {
        parse_job_id(job_id)?;
        if let Some(job) = self.lookup_optional(job_id)? {
            {
                let mut inner = job.inner.lock().map_err(job_error)?;
                if inner.phase != ScreenshotOutputPhase::Handoff
                    && inner.phase != ScreenshotOutputPhase::Cancelled
                {
                    return Err(NativeJobError::new(
                        "invalid-state",
                        "screenshot output is not ready for release",
                    ));
                }
                inner.phase = ScreenshotOutputPhase::Cancelled;
            }
            return self.remove_and_cleanup(&job);
        }
        let owned_directory = self.root.join(job_id);
        if !is_owned_handoff(&owned_directory, job_id) {
            return Ok(());
        }
        fs::remove_dir_all(&owned_directory).map_err(|error| {
            io_error(
                "cleanup-failed",
                "remove recovered screenshot output directory",
                error,
            )
        })
    }

    fn finish_failure<T>(
        &self,
        job: &Arc<ScreenshotOutputJob>,
        error: NativeJobError,
    ) -> Result<T, NativeJobError> {
        match self.remove_and_cleanup(job) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(NativeJobError::new(
                "cleanup-failed",
                format!("{}; {}", error.message, cleanup.message),
            )),
        }
    }

    fn lookup(&self, job_id: &str) -> Result<Arc<ScreenshotOutputJob>, NativeJobError> {
        self.lookup_optional(job_id)?.ok_or_else(|| {
            NativeJobError::new("job-not-found", "screenshot output job was not found")
        })
    }

    fn lookup_optional(
        &self,
        job_id: &str,
    ) -> Result<Option<Arc<ScreenshotOutputJob>>, NativeJobError> {
        parse_job_id(job_id)?;
        Ok(self
            .jobs
            .lock()
            .map_err(registry_error)?
            .get(job_id)
            .cloned())
    }

    fn remove_and_cleanup(&self, job: &Arc<ScreenshotOutputJob>) -> Result<(), NativeJobError> {
        let mut jobs = self.jobs.lock().map_err(registry_error)?;
        if jobs
            .get(&job.id)
            .is_some_and(|registered| Arc::ptr_eq(registered, job))
        {
            jobs.remove(&job.id);
        }
        drop(jobs);
        match fs::remove_dir_all(&job.owned_directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(
                "cleanup-failed",
                "remove screenshot output directory",
                error,
            )),
        }
    }
}

struct ScreenshotOutputJob {
    id: String,
    owned_directory: PathBuf,
    spool_path: PathBuf,
    destination: Option<PathBuf>,
    cancel_requested: AtomicBool,
    inner: Mutex<ScreenshotOutputJobInner>,
}

struct ScreenshotOutputJobInner {
    phase: ScreenshotOutputPhase,
    file: Option<File>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScreenshotOutputPhase {
    Open,
    Publishing,
    Cancelling,
    Finalizing,
    Handoff,
    Cancelled,
}

fn initialize_root(root: &Path) -> Result<(), NativeJobError> {
    initialize_root_at(root, SystemTime::now())
}

pub(crate) fn initialize_root_at(root: &Path, now: SystemTime) -> Result<(), NativeJobError> {
    fs::create_dir_all(root).map_err(|error| {
        io_error(
            "capability-unavailable",
            "create screenshot output root",
            error,
        )
    })?;
    let entries = fs::read_dir(root).map_err(|error| {
        io_error(
            "capability-unavailable",
            "inspect screenshot output root",
            error,
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            io_error(
                "capability-unavailable",
                "inspect screenshot output entry",
                error,
            )
        })?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !path.is_dir() || Uuid::parse_str(&name).is_err() {
            continue;
        }
        if !is_owned_directory(&path, &name) {
            continue;
        }
        if is_owned_handoff(&path, &name) && !is_stale_handoff(&path, now) {
            continue;
        }
        fs::remove_dir_all(&path).map_err(|error| {
            io_error(
                "cleanup-failed",
                "remove abandoned screenshot output directory",
                error,
            )
        })?;
    }
    Ok(())
}

fn is_stale_handoff(directory: &Path, now: SystemTime) -> bool {
    fs::metadata(directory.join(READY_FILE))
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age > READY_HANDOFF_STALE_AFTER)
}

fn is_owned_directory(directory: &Path, job_id: &str) -> bool {
    read_owned_marker(directory, OWNERSHIP_FILE).as_deref() == Some(job_id)
}

fn is_owned_handoff(directory: &Path, job_id: &str) -> bool {
    is_owned_directory(directory, job_id)
        && read_owned_marker(directory, READY_FILE).as_deref() == Some(job_id)
        && directory.join(SPOOL_FILE).is_file()
}

fn read_owned_marker(directory: &Path, name: &str) -> Option<String> {
    let marker = directory.join(name);
    if fs::metadata(&marker).ok()?.len() > 64 {
        return None;
    }
    fs::read_to_string(marker).ok()
}

fn write_owned_marker(directory: &Path, name: &str, job_id: &str) -> Result<(), NativeJobError> {
    let temporary = directory.join(format!("{name}.tmp"));
    let target = directory.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| {
            io_error(
                "destination-write-failed",
                "create screenshot marker",
                error,
            )
        })?;
    file.write_all(job_id.as_bytes())
        .map_err(|error| io_error("destination-write-failed", "write screenshot marker", error))?;
    file.sync_all()
        .map_err(|error| io_error("destination-write-failed", "sync screenshot marker", error))?;
    drop(file);
    fs::rename(&temporary, &target).map_err(|error| {
        io_error(
            "destination-write-failed",
            "publish screenshot marker",
            error,
        )
    })?;
    sync_directory(directory).map_err(|error| {
        io_error(
            "destination-write-failed",
            "sync screenshot marker directory",
            error,
        )
    })?;
    Ok(())
}

fn screenshot_spool_fingerprint(
    job: &ScreenshotOutputJob,
) -> Result<(u64, String), NativeJobError> {
    screenshot_spool_fingerprint_controlled(&job.spool_path, || {
        job.cancel_requested.load(Ordering::Acquire)
    })
}

pub(crate) fn screenshot_spool_fingerprint_controlled(
    path: &Path,
    is_cancelled: impl Fn() -> bool,
) -> Result<(u64, String), NativeJobError> {
    use sha2::{Digest, Sha256};

    let mut file = File::open(path)
        .map_err(|error| io_error("invalid-source", "open screenshot output spool", error))?;
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = vec![0u8; MAX_SCREENSHOT_OUTPUT_APPEND_BYTES];
    loop {
        if is_cancelled() {
            return Err(NativeJobError::new(
                "cancelled",
                "screenshot output fingerprint was cancelled",
            ));
        }
        let read = file
            .read(&mut buffer)
            .map_err(|error| io_error("invalid-source", "read screenshot output spool", error))?;
        if read == 0 {
            break;
        }
        bytes = bytes.checked_add(read as u64).ok_or_else(|| {
            NativeJobError::new("invalid-source", "screenshot output size overflowed")
        })?;
        hasher.update(&buffer[..read]);
    }
    Ok((bytes, hex::encode(hasher.finalize())))
}

fn validate_destination(destination: PathBuf) -> Result<PathBuf, NativeJobError> {
    if !destination.is_absolute() {
        return Err(NativeJobError::new(
            "invalid-destination",
            "screenshot destination must be an absolute desktop path",
        ));
    }
    let file_name = destination.file_name().ok_or_else(|| {
        NativeJobError::new(
            "invalid-destination",
            "screenshot destination file name is unavailable",
        )
    })?;
    let parent = destination
        .parent()
        .and_then(|path| path.canonicalize().ok())
        .ok_or_else(|| {
            NativeJobError::new(
                "invalid-destination",
                "screenshot destination directory is unavailable",
            )
        })?;
    Ok(parent.join(file_name))
}

fn validate_screenshot_archive(job: &ScreenshotOutputJob) -> Result<(), NativeJobError> {
    let file = File::open(&job.spool_path).map_err(|error| {
        io_error(
            "invalid-source",
            "open screenshot output spool for validation",
            error,
        )
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| {
        NativeJobError::new(
            "invalid-input",
            format!("screenshot output is not a valid ZIP archive: {error}"),
        )
    })?;
    if archive.is_empty() {
        return Err(NativeJobError::new(
            "invalid-input",
            "screenshot output ZIP has no pages",
        ));
    }
    let mut buffer = vec![0u8; MAX_SCREENSHOT_OUTPUT_APPEND_BYTES];
    for index in 0..archive.len() {
        if job.cancel_requested.load(Ordering::Acquire) {
            return Err(NativeJobError::new(
                "cancelled",
                "screenshot output validation was cancelled",
            ));
        }
        let mut entry = archive.by_index(index).map_err(|error| {
            NativeJobError::new(
                "invalid-input",
                format!("screenshot output ZIP page is invalid: {error}"),
            )
        })?;
        let expected_name = format!("page-{:04}.png", index + 1);
        if entry.is_dir() || entry.name() != expected_name {
            return Err(NativeJobError::new(
                "invalid-input",
                "screenshot output ZIP page names are invalid",
            ));
        }
        loop {
            if job.cancel_requested.load(Ordering::Acquire) {
                return Err(NativeJobError::new(
                    "cancelled",
                    "screenshot output validation was cancelled",
                ));
            }
            let read = entry.read(&mut buffer).map_err(|error| {
                NativeJobError::new(
                    "invalid-input",
                    format!("screenshot output ZIP page failed validation: {error}"),
                )
            })?;
            if read == 0 {
                break;
            }
        }
    }
    Ok(())
}

fn parse_job_id(job_id: &str) -> Result<(), NativeJobError> {
    Uuid::parse_str(job_id)
        .map(|_| ())
        .map_err(|_| NativeJobError::new("invalid-input", "screenshot output job ID is invalid"))
}

fn destination_error(error: DestinationWriteError) -> NativeJobError {
    match error {
        DestinationWriteError::InvalidSource => {
            NativeJobError::new("invalid-source", "screenshot output spool is unavailable")
        }
        DestinationWriteError::InvalidDestination => NativeJobError::new(
            "invalid-destination",
            "desktop screenshot destination is invalid",
        ),
        DestinationWriteError::Cancelled => {
            NativeJobError::new("cancelled", "screenshot output publication was cancelled")
        }
        DestinationWriteError::Io { operation, source } => {
            NativeJobError::new("destination-write-failed", format!("{operation}: {source}"))
        }
    }
}

fn registry_error(error: impl std::fmt::Display) -> NativeJobError {
    NativeJobError::new(
        "store-error",
        format!("screenshot output registry is unavailable: {error}"),
    )
}

fn job_error(error: impl std::fmt::Display) -> NativeJobError {
    NativeJobError::new(
        "store-error",
        format!("screenshot output job is unavailable: {error}"),
    )
}

fn io_error(code: &str, operation: &str, error: impl std::fmt::Display) -> NativeJobError {
    NativeJobError::new(code, format!("{operation}: {error}"))
}

#[tauri::command(async)]
pub(crate) fn native_file_job_screenshot_output_start(
    state: State<'_, ScreenshotOutputState>,
    destination: Option<String>,
) -> Result<NativeFileJobStarted, NativeJobError> {
    state.start(destination.map(PathBuf::from))
}

#[tauri::command(async)]
pub(crate) fn native_file_job_screenshot_output_append(
    state: State<'_, ScreenshotOutputState>,
    job_id: String,
    chunk: Vec<u8>,
) -> Result<(), NativeJobError> {
    state.append(&job_id, &chunk)
}

#[tauri::command(async)]
pub(crate) async fn native_file_job_screenshot_output_publish(
    state: State<'_, ScreenshotOutputState>,
    job_id: String,
) -> Result<ScreenshotOutputPublished, NativeJobError> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.publish(&job_id))
        .await
        .map_err(|error| {
            NativeJobError::new(
                "store-error",
                format!("screenshot output worker failed: {error}"),
            )
        })?
}

#[tauri::command(async)]
pub(crate) fn native_file_job_screenshot_output_cancel(
    state: State<'_, ScreenshotOutputState>,
    job_id: String,
) -> Result<ScreenshotOutputCancelOutcome, NativeJobError> {
    state.cancel(&job_id)
}

#[tauri::command(async)]
pub(crate) fn native_file_job_screenshot_output_release(
    state: State<'_, ScreenshotOutputState>,
    job_id: String,
) -> Result<(), NativeJobError> {
    state.release(&job_id)
}
