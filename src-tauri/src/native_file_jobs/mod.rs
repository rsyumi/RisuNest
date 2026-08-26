use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

const MAX_WARNING_CODES: usize = 16;
const MAX_CODE_BYTES: usize = 64;
const MAX_ERROR_MESSAGE_BYTES: usize = 512;
const MAX_MANIFEST_BYTES: u64 = 4096;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum JobSource {
    DesktopPath { path: String },
    AndroidSpool { token: String },
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum SpoolState {
    Copying,
    Ready,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpoolManifest {
    token: String,
    state: SpoolState,
    display_name: String,
    bytes: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct JobOwnership {
    job_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeFileJobStartRequest {
    pub(crate) kind: JobKind,
    pub(crate) source: JobSource,
    pub(crate) expected_revision: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeFileJobStarted {
    pub(crate) job_id: String,
}

fn resolve_source(job_root: &Path, source: &JobSource) -> Result<PathBuf, String> {
    match source {
        JobSource::DesktopPath { path } => {
            let path = Path::new(path)
                .canonicalize()
                .map_err(|error| format!("desktop source is unavailable: {error}"))?;
            if !path
                .metadata()
                .map_err(|error| format!("desktop source metadata is unavailable: {error}"))?
                .is_file()
            {
                return Err("desktop source must be a regular file".to_owned());
            }
            Ok(path)
        }
        JobSource::AndroidSpool { token } => resolve_spool_source(job_root, token),
    }
}

fn resolve_spool_source(job_root: &Path, token: &str) -> Result<PathBuf, String> {
    let parsed = Uuid::parse_str(token).map_err(|_| "invalid Android spool token".to_owned())?;
    if parsed.hyphenated().to_string() != token {
        return Err("invalid Android spool token".to_owned());
    }
    let spool = job_root.join("sources").join(token);
    let manifest_path = spool.join("source.json");
    let manifest_metadata = manifest_path
        .metadata()
        .map_err(|error| format!("Android spool manifest is unavailable: {error}"))?;
    if !manifest_metadata.is_file() || manifest_metadata.len() > MAX_MANIFEST_BYTES {
        return Err("Android spool manifest is invalid".to_owned());
    }
    let manifest: SpoolManifest = serde_json::from_slice(
        &fs::read(&manifest_path)
            .map_err(|error| format!("Android spool manifest cannot be read: {error}"))?,
    )
    .map_err(|error| format!("Android spool manifest is invalid: {error}"))?;
    if manifest.token != token || manifest.state != SpoolState::Ready {
        return Err("Android spool source is not ready".to_owned());
    }
    let canonical_spool = spool
        .canonicalize()
        .map_err(|error| format!("Android spool directory is unavailable: {error}"))?;
    let source = spool
        .join("source.risudat")
        .canonicalize()
        .map_err(|error| format!("Android spool source is unavailable: {error}"))?;
    if source.parent() != Some(canonical_spool.as_path()) {
        return Err("Android spool source escapes its owned directory".to_owned());
    }
    let metadata = source
        .metadata()
        .map_err(|error| format!("Android spool source metadata is unavailable: {error}"))?;
    if !metadata.is_file() || manifest.bytes.is_some_and(|bytes| bytes != metadata.len()) {
        return Err("Android spool source does not match its ready manifest".to_owned());
    }
    Ok(source)
}

fn cleanup_owned_directories(jobs_root: &Path) -> Result<(), String> {
    if !jobs_root.is_dir() {
        return Ok(());
    }
    let canonical_root = jobs_root
        .canonicalize()
        .map_err(|error| format!("native job root cannot be resolved: {error}"))?;
    for entry in fs::read_dir(&canonical_root)
        .map_err(|error| format!("native job root cannot be read: {error}"))?
    {
        let entry = entry.map_err(|error| format!("native job entry cannot be read: {error}"))?;
        if !entry
            .file_type()
            .map_err(|error| format!("native job entry type is unavailable: {error}"))?
            .is_dir()
        {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(&name) else {
            continue;
        };
        if id.hyphenated().to_string() != name {
            continue;
        }
        let manifest_path = entry.path().join("ownership.json");
        let Ok(metadata) = manifest_path.metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
            continue;
        }
        let Ok(bytes) = fs::read(&manifest_path) else {
            continue;
        };
        let Ok(ownership) = serde_json::from_slice::<JobOwnership>(&bytes) else {
            continue;
        };
        if ownership.job_id != name {
            continue;
        }
        let owned = entry
            .path()
            .canonicalize()
            .map_err(|error| format!("native job directory cannot be resolved: {error}"))?;
        if owned.parent() != Some(canonical_root.as_path()) {
            continue;
        }
        fs::remove_dir_all(&owned)
            .map_err(|error| format!("native job directory cannot be removed: {error}"))?;
    }
    Ok(())
}

fn cleanup_spool_directories(sources_root: &Path) -> Result<(), String> {
    if !sources_root.is_dir() {
        return Ok(());
    }
    let canonical_root = sources_root
        .canonicalize()
        .map_err(|error| format!("native source root cannot be resolved: {error}"))?;
    for entry in fs::read_dir(&canonical_root)
        .map_err(|error| format!("native source root cannot be read: {error}"))?
    {
        let entry =
            entry.map_err(|error| format!("native source entry cannot be read: {error}"))?;
        if !entry
            .file_type()
            .map_err(|error| format!("native source entry type is unavailable: {error}"))?
            .is_dir()
        {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(&name) else {
            continue;
        };
        if id.hyphenated().to_string() != name {
            continue;
        }
        let manifest_path = entry.path().join("source.json");
        let Ok(metadata) = manifest_path.metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
            continue;
        }
        let Ok(bytes) = fs::read(&manifest_path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_slice::<SpoolManifest>(&bytes) else {
            continue;
        };
        if manifest.token != name {
            continue;
        }
        let owned = entry
            .path()
            .canonicalize()
            .map_err(|error| format!("native source directory cannot be resolved: {error}"))?;
        if owned.parent() != Some(canonical_root.as_path()) {
            continue;
        }
        fs::remove_dir_all(&owned)
            .map_err(|error| format!("native source directory cannot be removed: {error}"))?;
    }
    Ok(())
}

pub(crate) struct NativeFileJobState {
    root: PathBuf,
    registry: JobRegistry,
}

impl NativeFileJobState {
    pub(crate) fn initialize(root: PathBuf) -> Result<Self, String> {
        let jobs_root = root.join("jobs");
        let sources_root = root.join("sources");
        fs::create_dir_all(&jobs_root)
            .map_err(|error| format!("native job root cannot be created: {error}"))?;
        fs::create_dir_all(&sources_root)
            .map_err(|error| format!("native source root cannot be created: {error}"))?;
        cleanup_owned_directories(&jobs_root)?;
        cleanup_spool_directories(&sources_root)?;
        Ok(Self {
            root,
            registry: JobRegistry::default(),
        })
    }

    pub(crate) fn start(
        &self,
        request: NativeFileJobStartRequest,
        sink: Arc<dyn restore::ReplacementSink>,
    ) -> Result<NativeFileJobStarted, String> {
        let source_path = resolve_source(&self.root, &request.source)?;
        let job = self.registry.create(request.kind)?;
        let job_id = job.id();
        let owned_directory = match create_owned_directory(&self.root.join("jobs"), &job_id) {
            Ok(path) => path,
            Err(error) => {
                let _ = job.finish_failure("job-setup-failed", &error);
                let _ = self.registry.forget(&job_id);
                return Err(error);
            }
        };
        let root = self.root.clone();
        let source = request.source.clone();
        std::thread::spawn(move || {
            let outcome = match request.kind {
                JobKind::RestoreBlockRisuSave => restore::restore_block_risu_save(
                    &source_path,
                    request.expected_revision,
                    &job,
                    sink.as_ref(),
                ),
            };
            let _ = cleanup_one_owned_directory(&root.join("jobs"), &owned_directory, &job.id());
            if let JobSource::AndroidSpool { token } = source {
                let _ = cleanup_one_spool_directory(&root.join("sources"), &token);
            }
            match outcome {
                Ok(result) => {
                    let _ = job.finish_success(result);
                }
                Err(_error) if job.is_cancel_requested() => {
                    let _ = job.finish_cancelled();
                }
                Err(error) => {
                    let _ = job.finish_failure("restore-failed", &error);
                }
            }
        });
        Ok(NativeFileJobStarted { job_id })
    }

    pub(crate) fn status(&self, job_id: &str) -> Result<JobStatus, String> {
        self.registry.status(job_id)
    }

    pub(crate) fn cancel(&self, job_id: &str) -> Result<CancelOutcome, String> {
        self.registry.cancel(job_id)
    }

    pub(crate) fn forget(&self, job_id: &str) -> Result<bool, String> {
        self.registry.forget(job_id)
    }
}

fn create_owned_directory(jobs_root: &Path, job_id: &str) -> Result<PathBuf, String> {
    let path = jobs_root.join(job_id);
    fs::create_dir(&path)
        .map_err(|error| format!("native job directory cannot be created: {error}"))?;
    let manifest_path = path.join("ownership.json");
    let mut manifest = File::create(&manifest_path)
        .map_err(|error| format!("native job ownership cannot be created: {error}"))?;
    serde_json::to_writer(
        &mut manifest,
        &JobOwnership {
            job_id: job_id.to_owned(),
        },
    )
    .map_err(|error| format!("native job ownership cannot be written: {error}"))?;
    manifest
        .flush()
        .map_err(|error| format!("native job ownership cannot be flushed: {error}"))?;
    manifest
        .sync_all()
        .map_err(|error| format!("native job ownership cannot be synced: {error}"))?;
    Ok(path)
}

fn cleanup_one_owned_directory(
    jobs_root: &Path,
    owned_directory: &Path,
    job_id: &str,
) -> Result<(), String> {
    let canonical_root = jobs_root
        .canonicalize()
        .map_err(|error| format!("native job root cannot be resolved: {error}"))?;
    let canonical_owned = owned_directory
        .canonicalize()
        .map_err(|error| format!("native job directory cannot be resolved: {error}"))?;
    if canonical_owned.parent() != Some(canonical_root.as_path())
        || canonical_owned.file_name().and_then(|name| name.to_str()) != Some(job_id)
    {
        return Err("native job cleanup target is outside its owned root".to_owned());
    }
    let manifest_path = canonical_owned.join("ownership.json");
    let manifest: JobOwnership = serde_json::from_slice(
        &fs::read(&manifest_path)
            .map_err(|error| format!("native job ownership cannot be read: {error}"))?,
    )
    .map_err(|error| format!("native job ownership is invalid: {error}"))?;
    if manifest.job_id != job_id {
        return Err("native job ownership does not match cleanup target".to_owned());
    }
    fs::remove_dir_all(canonical_owned)
        .map_err(|error| format!("native job directory cannot be removed: {error}"))
}

fn cleanup_one_spool_directory(sources_root: &Path, token: &str) -> Result<(), String> {
    let parsed = Uuid::parse_str(token).map_err(|_| "invalid Android spool token".to_owned())?;
    if parsed.hyphenated().to_string() != token {
        return Err("invalid Android spool token".to_owned());
    }
    let canonical_root = sources_root
        .canonicalize()
        .map_err(|error| format!("native source root cannot be resolved: {error}"))?;
    let owned = sources_root
        .join(token)
        .canonicalize()
        .map_err(|error| format!("native source directory cannot be resolved: {error}"))?;
    if owned.parent() != Some(canonical_root.as_path()) {
        return Err("native source cleanup target is outside its owned root".to_owned());
    }
    let manifest: SpoolManifest = serde_json::from_slice(
        &fs::read(owned.join("source.json"))
            .map_err(|error| format!("native source manifest cannot be read: {error}"))?,
    )
    .map_err(|error| format!("native source manifest is invalid: {error}"))?;
    if manifest.token != token {
        return Err("native source ownership does not match cleanup target".to_owned());
    }
    fs::remove_dir_all(owned)
        .map_err(|error| format!("native source directory cannot be removed: {error}"))
}

struct PersistentReplacementSink {
    app: AppHandle,
}

impl restore::ReplacementSink for PersistentReplacementSink {
    fn begin(
        &self,
    ) -> crate::persistent_store::StoreResult<crate::persistent_store::StagingResult> {
        crate::persistent_store::commands::with_store_mut(
            self.app.state(),
            crate::persistent_store::PersistentStore::replace_begin,
        )
    }

    fn put_root(
        &self,
        staging_id: &str,
        root: &serde_json::Value,
    ) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_root(staging_id, root)
        })
    }

    fn put_presets(
        &self,
        staging_id: &str,
        presets: &[serde_json::Value],
    ) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_presets(staging_id, presets)
        })
    }

    fn add_characters(
        &self,
        staging_id: &str,
        characters: &[serde_json::Value],
    ) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_add_characters(staging_id, characters)
        })
    }

    fn commit(
        &self,
        staging_id: &str,
        expected_revision: i64,
    ) -> crate::persistent_store::StoreResult<crate::persistent_store::RevisionResult> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_commit(staging_id, Some(expected_revision))
        })
    }

    fn abort(&self, staging_id: &str) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_abort(staging_id)
        })
    }
}

#[tauri::command(async)]
pub(crate) fn native_file_job_start(
    app: AppHandle,
    state: State<'_, NativeFileJobState>,
    request: NativeFileJobStartRequest,
) -> Result<NativeFileJobStarted, String> {
    state.start(
        request,
        Arc::new(PersistentReplacementSink { app }) as Arc<dyn restore::ReplacementSink>,
    )
}

#[tauri::command(async)]
pub(crate) fn native_file_job_status(
    state: State<'_, NativeFileJobState>,
    job_id: String,
) -> Result<JobStatus, String> {
    state.status(&job_id)
}

#[tauri::command(async)]
pub(crate) fn native_file_job_cancel(
    state: State<'_, NativeFileJobState>,
    job_id: String,
) -> Result<CancelOutcome, String> {
    state.cancel(&job_id)
}

#[tauri::command(async)]
pub(crate) fn native_file_job_forget(
    state: State<'_, NativeFileJobState>,
    job_id: String,
) -> Result<bool, String> {
    state.forget(&job_id)
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum JobKind {
    RestoreBlockRisuSave,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum JobState {
    Queued,
    Running,
    WaitingForInput,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum JobPhase {
    Queued,
    ReadingSource,
    StagingDatabase,
    ActivatingDatabase,
    Complete,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobProgress {
    pub(crate) completed_bytes: u64,
    pub(crate) total_bytes: Option<u64>,
    pub(crate) completed_items: u64,
    pub(crate) total_items: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobStatus {
    pub(crate) job_id: String,
    pub(crate) kind: JobKind,
    pub(crate) state: JobState,
    pub(crate) phase: JobPhase,
    pub(crate) progress: JobProgress,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<JobResultSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<JobFailure>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobResultSummary {
    pub(crate) revision: i64,
    pub(crate) source_bytes: u64,
    pub(crate) source_sha256: String,
    pub(crate) character_count: u64,
    pub(crate) preset_count: u64,
    pub(crate) warning_codes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobFailure {
    pub(crate) code: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CancelOutcome {
    Requested,
    AlreadyRequested,
    TooLate,
    Terminal,
    Missing,
}

pub(crate) struct JobRegistry {
    jobs: Mutex<HashMap<String, Arc<JobControl>>>,
    max_terminal_jobs: usize,
    max_terminal_age: Duration,
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::with_retention(64, Duration::from_secs(60 * 60))
    }
}

impl JobRegistry {
    fn with_retention(max_terminal_jobs: usize, max_terminal_age: Duration) -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            max_terminal_jobs,
            max_terminal_age,
        }
    }

    pub(crate) fn create(&self, kind: JobKind) -> Result<Arc<JobControl>, String> {
        self.prune()?;
        let id = Uuid::new_v4().to_string();
        let job = Arc::new(JobControl {
            cancel_requested: AtomicBool::new(false),
            terminal_at: Mutex::new(None),
            status: Mutex::new(JobStatus {
                job_id: id.clone(),
                kind,
                state: JobState::Queued,
                phase: JobPhase::Queued,
                progress: JobProgress::default(),
                result: None,
                error: None,
            }),
        });
        self.jobs
            .lock()
            .map_err(|error| format!("native job registry mutex poisoned: {error}"))?
            .insert(id, Arc::clone(&job));
        Ok(job)
    }

    pub(crate) fn status(&self, id: &str) -> Result<JobStatus, String> {
        let job = self
            .lookup(id)?
            .ok_or_else(|| "native job not found".to_owned())?;
        Ok(job.status())
    }

    pub(crate) fn cancel(&self, id: &str) -> Result<CancelOutcome, String> {
        let Some(job) = self.lookup(id)? else {
            return Ok(CancelOutcome::Missing);
        };
        job.request_cancel()
    }

    pub(crate) fn forget(&self, id: &str) -> Result<bool, String> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|error| format!("native job registry mutex poisoned: {error}"))?;
        let Some(job) = jobs.get(id) else {
            return Ok(false);
        };
        if !job.status().state.is_terminal() {
            return Err("native job cannot be forgotten before it is terminal".to_owned());
        }
        jobs.remove(id);
        Ok(true)
    }

    fn lookup(&self, id: &str) -> Result<Option<Arc<JobControl>>, String> {
        Ok(self
            .jobs
            .lock()
            .map_err(|error| format!("native job registry mutex poisoned: {error}"))?
            .get(id)
            .cloned())
    }

    fn prune(&self) -> Result<(), String> {
        let now = Instant::now();
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|error| format!("native job registry mutex poisoned: {error}"))?;
        let mut terminal = jobs
            .iter()
            .filter_map(|(id, job)| job.terminal_time().map(|time| (id.clone(), time)))
            .collect::<Vec<_>>();
        for (id, completed_at) in &terminal {
            if now.saturating_duration_since(*completed_at) >= self.max_terminal_age {
                jobs.remove(id);
            }
        }
        terminal.retain(|(id, _)| jobs.contains_key(id));
        terminal.sort_by_key(|(_, completed_at)| *completed_at);
        let excess = terminal.len().saturating_sub(self.max_terminal_jobs);
        for (id, _) in terminal.into_iter().take(excess) {
            jobs.remove(&id);
        }
        Ok(())
    }
}

pub(crate) struct JobControl {
    cancel_requested: AtomicBool,
    terminal_at: Mutex<Option<Instant>>,
    status: Mutex<JobStatus>,
}

impl JobControl {
    pub(crate) fn id(&self) -> String {
        self.status().job_id
    }

    pub(crate) fn status(&self) -> JobStatus {
        self.status
            .lock()
            .expect("job status mutex poisoned")
            .clone()
    }

    pub(crate) fn is_cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::Acquire)
    }

    fn request_cancel(&self) -> Result<CancelOutcome, String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state.is_terminal() {
            return Ok(CancelOutcome::Terminal);
        }
        if status.phase == JobPhase::ActivatingDatabase {
            return Ok(CancelOutcome::TooLate);
        }
        if self.cancel_requested.swap(true, Ordering::AcqRel) {
            return Ok(CancelOutcome::AlreadyRequested);
        }
        status.state = JobState::Cancelling;
        Ok(CancelOutcome::Requested)
    }

    pub(crate) fn start(&self, phase: JobPhase) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state != JobState::Queued || phase != JobPhase::ReadingSource {
            return Err("native job can only start from queued".to_owned());
        }
        status.state = JobState::Running;
        status.phase = phase;
        Ok(())
    }

    pub(crate) fn set_progress(&self, progress: JobProgress) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state != JobState::Running {
            return Err("native job progress requires a running job".to_owned());
        }
        if progress.completed_bytes < status.progress.completed_bytes
            || progress.completed_items < status.progress.completed_items
            || progress
                .total_bytes
                .is_some_and(|total| progress.completed_bytes > total)
            || progress
                .total_items
                .is_some_and(|total| progress.completed_items > total)
            || status
                .progress
                .total_bytes
                .is_some_and(|total| progress.total_bytes != Some(total))
            || status
                .progress
                .total_items
                .is_some_and(|total| progress.total_items != Some(total))
        {
            return Err("native job progress is invalid or non-monotonic".to_owned());
        }
        status.progress = progress;
        Ok(())
    }

    pub(crate) fn set_phase(&self, phase: JobPhase) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state != JobState::Running {
            return Err("native job phase requires a running job".to_owned());
        }
        if phase == JobPhase::Complete || phase.rank() != status.phase.rank() + 1 {
            return Err("native job phase transition is invalid".to_owned());
        }
        status.phase = phase;
        Ok(())
    }

    pub(crate) fn finish_cancelled(&self) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if !self.is_cancel_requested() || status.state.is_terminal() {
            return Err("native job cannot finish cancelled from its current state".to_owned());
        }
        status.state = JobState::Cancelled;
        status.phase = JobPhase::Complete;
        drop(status);
        self.mark_terminal()?;
        Ok(())
    }

    pub(crate) fn finish_success(&self, result: JobResultSummary) -> Result<(), String> {
        validate_result(&result)?;
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state != JobState::Running || self.is_cancel_requested() {
            return Err("native job cannot succeed from its current state".to_owned());
        }
        status.state = JobState::Succeeded;
        status.phase = JobPhase::Complete;
        status.result = Some(result);
        status.error = None;
        drop(status);
        self.mark_terminal()?;
        Ok(())
    }

    pub(crate) fn finish_failure(&self, code: &str, message: &str) -> Result<(), String> {
        if code.is_empty() || code.len() > MAX_CODE_BYTES {
            return Err("native job error code is invalid".to_owned());
        }
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state.is_terminal() {
            return Err("native job is already terminal".to_owned());
        }
        status.state = JobState::Failed;
        status.phase = JobPhase::Complete;
        status.result = None;
        status.error = Some(JobFailure {
            code: code.to_owned(),
            message: bounded_message(message),
        });
        drop(status);
        self.mark_terminal()?;
        Ok(())
    }

    fn mark_terminal(&self) -> Result<(), String> {
        *self
            .terminal_at
            .lock()
            .map_err(|error| format!("native job terminal mutex poisoned: {error}"))? =
            Some(Instant::now());
        Ok(())
    }

    fn terminal_time(&self) -> Option<Instant> {
        *self
            .terminal_at
            .lock()
            .expect("native job terminal mutex poisoned")
    }
}

fn validate_result(result: &JobResultSummary) -> Result<(), String> {
    if result.warning_codes.len() > MAX_WARNING_CODES {
        return Err("native job result has too many warning codes".to_owned());
    }
    if result.source_sha256.len() != 64
        || !result
            .source_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("native job result has an invalid source hash".to_owned());
    }
    if result
        .warning_codes
        .iter()
        .any(|code| code.is_empty() || code.len() > MAX_CODE_BYTES)
    {
        return Err("native job result has an invalid warning code".to_owned());
    }
    Ok(())
}

fn bounded_message(message: &str) -> String {
    let sanitized = message.replace(['\r', '\n'], " ");
    if sanitized.len() <= MAX_ERROR_MESSAGE_BYTES {
        return sanitized;
    }
    let mut end = MAX_ERROR_MESSAGE_BYTES;
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    sanitized[..end].to_owned()
}

impl JobState {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

impl JobPhase {
    fn rank(self) -> u8 {
        match self {
            Self::Queued => 0,
            Self::ReadingSource => 1,
            Self::StagingDatabase => 2,
            Self::ActivatingDatabase => 3,
            Self::Complete => 4,
        }
    }
}

mod restore;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    fn result(revision: i64) -> JobResultSummary {
        JobResultSummary {
            revision,
            source_bytes: 1,
            source_sha256: "a".repeat(64),
            character_count: 0,
            preset_count: 0,
            warning_codes: Vec::new(),
        }
    }

    #[test]
    fn job_status_tracks_only_legal_monotonic_progress() {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();

        assert_eq!(job.status().state, JobState::Queued);
        job.start(JobPhase::ReadingSource).unwrap();
        job.set_progress(JobProgress {
            completed_bytes: 8,
            total_bytes: Some(16),
            completed_items: 1,
            total_items: None,
        })
        .unwrap();
        job.set_progress(JobProgress {
            completed_bytes: 7,
            total_bytes: Some(16),
            completed_items: 1,
            total_items: None,
        })
        .unwrap_err();

        let status = registry.status(&job.id()).unwrap();
        assert_eq!(status.kind, JobKind::RestoreBlockRisuSave);
        assert_eq!(status.state, JobState::Running);
        assert_eq!(status.phase, JobPhase::ReadingSource);
        assert_eq!(status.progress.completed_bytes, 8);
    }

    #[test]
    fn running_job_rejects_backward_phases_and_progress_past_known_totals() {
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        assert!(job.start(JobPhase::Complete).is_err());
        job.start(JobPhase::ReadingSource).unwrap();
        job.set_phase(JobPhase::StagingDatabase).unwrap();
        assert!(job.set_phase(JobPhase::ReadingSource).is_err());
        assert!(job
            .set_progress(JobProgress {
                completed_bytes: 17,
                total_bytes: Some(16),
                completed_items: 0,
                total_items: None,
            })
            .is_err());
    }

    #[test]
    fn cancel_and_forget_are_idempotent_without_removing_active_jobs() {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        job.start(JobPhase::ReadingSource).unwrap();

        assert_eq!(
            registry.cancel(&job.id()).unwrap(),
            CancelOutcome::Requested
        );
        assert_eq!(
            registry.cancel(&job.id()).unwrap(),
            CancelOutcome::AlreadyRequested
        );
        assert!(job.is_cancel_requested());
        assert!(registry.forget(&job.id()).is_err());

        job.finish_cancelled().unwrap();
        assert_eq!(registry.cancel(&job.id()).unwrap(), CancelOutcome::Terminal);
        assert!(registry.forget(&job.id()).unwrap());
        assert!(!registry.forget(&job.id()).unwrap());
    }

    #[test]
    fn terminal_status_rejects_unbounded_results_and_sanitizes_errors() {
        let registry = JobRegistry::default();
        let success = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        success.start(JobPhase::ReadingSource).unwrap();
        assert!(success
            .finish_success(JobResultSummary {
                revision: 2,
                source_bytes: 128,
                source_sha256: "a".repeat(64),
                character_count: 1,
                preset_count: 1,
                warning_codes: (0..=MAX_WARNING_CODES)
                    .map(|index| format!("warning-{index}"))
                    .collect(),
            })
            .is_err());

        let failed = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        failed.start(JobPhase::ReadingSource).unwrap();
        failed
            .finish_failure("invalid-source", &("x".repeat(900) + "\nsecret"))
            .unwrap();
        let status = failed.status();
        let error = status.error.unwrap();
        assert_eq!(error.code, "invalid-source");
        assert!(error.message.len() <= MAX_ERROR_MESSAGE_BYTES);
        assert!(!error.message.contains('\n'));
        assert!(status.result.is_none());
    }

    #[test]
    fn terminal_retention_is_bounded_by_count_and_age() {
        let registry = JobRegistry::with_retention(2, Duration::from_secs(60));
        let mut ids = Vec::new();
        for revision in 1..=3 {
            let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
            ids.push(job.id());
            job.start(JobPhase::ReadingSource).unwrap();
            job.finish_success(result(revision)).unwrap();
        }
        registry.prune().unwrap();

        assert!(registry.status(&ids[0]).is_err());
        assert!(registry.status(&ids[1]).is_ok());
        assert!(registry.status(&ids[2]).is_ok());

        let expiring = JobRegistry::with_retention(2, Duration::ZERO);
        let job = expiring.create(JobKind::RestoreBlockRisuSave).unwrap();
        let id = job.id();
        job.start(JobPhase::ReadingSource).unwrap();
        job.finish_success(result(1)).unwrap();
        expiring.prune().unwrap();
        assert!(expiring.status(&id).is_err());
    }

    #[test]
    fn source_descriptors_confine_spool_tokens_and_require_ready_files() {
        let directory = TempDir::new().unwrap();
        let desktop_file = directory.path().join("desktop.risudat");
        fs::write(&desktop_file, b"RISUSAVE\0").unwrap();
        let resolved = resolve_source(
            directory.path(),
            &JobSource::DesktopPath {
                path: desktop_file.to_string_lossy().into_owned(),
            },
        )
        .unwrap();
        assert_eq!(resolved, desktop_file.canonicalize().unwrap());
        assert!(resolve_source(
            directory.path(),
            &JobSource::AndroidSpool {
                token: "../outside".to_owned(),
            },
        )
        .is_err());

        let token = Uuid::new_v4().to_string();
        let spool = directory.path().join("sources").join(&token);
        fs::create_dir_all(&spool).unwrap();
        fs::write(spool.join("source.risudat"), b"RISUSAVE\0").unwrap();
        fs::write(
            spool.join("source.json"),
            serde_json::to_vec(&SpoolManifest {
                token: token.clone(),
                state: SpoolState::Copying,
                display_name: "external.risudat".to_owned(),
                bytes: Some(9),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(resolve_source(
            directory.path(),
            &JobSource::AndroidSpool {
                token: token.clone(),
            },
        )
        .is_err());

        fs::write(
            spool.join("source.json"),
            serde_json::to_vec(&SpoolManifest {
                token: token.clone(),
                state: SpoolState::Ready,
                display_name: "../../external.risudat".to_owned(),
                bytes: Some(9),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            resolve_source(directory.path(), &JobSource::AndroidSpool { token },).unwrap(),
            spool.join("source.risudat").canonicalize().unwrap(),
        );
    }

    #[test]
    fn startup_cleanup_removes_only_matching_owned_directories() {
        let directory = TempDir::new().unwrap();
        let jobs_root = directory.path().join("jobs");
        fs::create_dir_all(&jobs_root).unwrap();
        let owned_id = Uuid::new_v4().to_string();
        let owned = jobs_root.join(&owned_id);
        fs::create_dir_all(&owned).unwrap();
        fs::write(
            owned.join("ownership.json"),
            serde_json::to_vec(&JobOwnership {
                job_id: owned_id.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(owned.join("partial.tmp"), b"partial").unwrap();

        let unrelated = jobs_root.join("keep-me");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("partial.tmp"), b"unrelated").unwrap();
        let mismatched_id = Uuid::new_v4().to_string();
        let mismatched = jobs_root.join(&mismatched_id);
        fs::create_dir_all(&mismatched).unwrap();
        fs::write(
            mismatched.join("ownership.json"),
            serde_json::to_vec(&JobOwnership {
                job_id: Uuid::new_v4().to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        cleanup_owned_directories(&jobs_root).unwrap();

        assert!(!owned.exists());
        assert!(unrelated.exists());
        assert!(mismatched.exists());
        assert!(directory.path().exists());
    }

    #[test]
    fn startup_cleanup_removes_only_manifest_owned_spool_directories() {
        let directory = TempDir::new().unwrap();
        let sources_root = directory.path().join("sources");
        fs::create_dir_all(&sources_root).unwrap();
        let token = Uuid::new_v4().to_string();
        let owned = sources_root.join(&token);
        fs::create_dir_all(&owned).unwrap();
        fs::write(owned.join("source.risudat"), b"partial").unwrap();
        fs::write(
            owned.join("source.json"),
            serde_json::to_vec(&SpoolManifest {
                token: token.clone(),
                state: SpoolState::Copying,
                display_name: "chosen.risudat".to_owned(),
                bytes: None,
            })
            .unwrap(),
        )
        .unwrap();
        let unrelated = sources_root.join("not-a-token");
        fs::create_dir_all(&unrelated).unwrap();

        cleanup_spool_directories(&sources_root).unwrap();

        assert!(!owned.exists());
        assert!(unrelated.exists());
    }
}
