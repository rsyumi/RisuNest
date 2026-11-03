//! Official account asset restore. The renderer hands over the keys in pages,
//! reads follow the scheduling rules of the renderer pool this job replaces,
//! bodies spool to the job directory, and one thread installs them in batches.
//! The official snapshot restore reads cold items, and the asset publication
//! writes bodies, under the same rules.
mod cold;
mod download;
mod install;
mod scheduler;
pub(crate) mod upload;

pub(crate) use cold::{ColdSource, SpoolLimits};

use super::error::job_error;
use super::official_snapshot::OfficialSnapshotCredential;
use super::{JobControl, JobProgress, JobPhase, JobResultSummary, JobState, NativeFileJobState, NativeJobError, SourceFingerprintKind};
use crate::native_log::logged;
use crate::persistent_store::PersistentStore;
use download::{AccountReader, Read};
use futures::stream::{FuturesUnordered, StreamExt};
use install::{BatchError, Body, Installed, Installer};
use scheduler::{Attempt, Exchange, Outcome, Scheduler};
pub(crate) use scheduler::{Settled, StopReason};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::State;

const PAGE: usize = 512;
const MAX_KEY_BYTES: usize = 4096;
const BACKLOG_BODIES: usize = 32;
const BACKLOG_BYTES: u64 = 64 * 1024 * 1024;
const TICK: Duration = Duration::from_millis(150);

pub(crate) struct Request {
    pub(crate) base_url: String,
    pub(crate) credential: OfficialSnapshotCredential,
    pub(crate) account_id: String,
    pub(crate) expected_library: String,
    pub(crate) key_count: usize,
    pub(crate) concurrency: usize,
    pub(crate) reauthenticated: bool,
    pub(crate) store: PersistentStore,
    pub(crate) revision: i64,
    pub(crate) shared: Handle,
}

/// Checks a start request and returns the key count and concurrency it asks for.
pub(crate) fn validate_start(
    base_url: &str,
    account_id: &str,
    expected_library: &str,
    key_count: u64,
    concurrency: Option<u8>,
) -> Result<(usize, usize), NativeJobError> {
    download::validate_base_url(base_url)?;
    if account_id.is_empty() || account_id.len() > 512 || expected_library.is_empty() || expected_library.len() > 512 {
        return Err(NativeJobError::new("invalid-input", "Official asset restore requires an account and a library"));
    }
    let key_count = usize::try_from(key_count)
        .map_err(|_| NativeJobError::new("invalid-input", "Official asset restore key count is too large"))?;
    let concurrency = match concurrency {
        None => 2,
        Some(value @ (1 | 2 | 4)) => usize::from(value),
        Some(_) => return Err(NativeJobError::new("invalid-input", "Official asset restore concurrency must be 1, 2 or 4")),
    };
    Ok((key_count, concurrency))
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RestoreProgress {
    pub(crate) installed: u64,
    pub(crate) missing: u64,
    pub(crate) failed: u64,
    pub(crate) bytes: u64,
    pub(crate) active: usize,
    pub(crate) limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) waiting_until: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<StopReason>,
}

/// What the renderer and the job exchange while it runs: keys not yet taken by
/// the job, and every settled key in the order it settled.
pub(crate) struct Shared {
    inbox: Mutex<Inbox>,
    arrived: tokio::sync::Notify,
}

struct Inbox {
    key_count: usize,
    enqueued: usize,
    pending: Vec<String>,
    settled: Vec<(usize, Settled)>,
    closed: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SettledPage {
    entries: Vec<SettledEntry>,
    total: usize,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SettledEntry {
    index: usize,
    result: Settled,
}

impl Shared {
    fn inbox(&self) -> MutexGuard<'_, Inbox> {
        self.inbox.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Accepts the next page of keys; `false` means the job no longer reads keys.
    fn enqueue(&self, keys: Vec<String>) -> Result<bool, NativeJobError> {
        if keys.len() > PAGE {
            return Err(NativeJobError::new("invalid-input", "Official asset key page exceeds 512 keys"));
        }
        if keys.iter().any(|key| key.len() > MAX_KEY_BYTES || !(key.starts_with("assets/") || key.starts_with("assets\\"))) {
            return Err(NativeJobError::new("invalid-input", "Official asset restore accepts only asset keys"));
        }
        let mut inbox = self.inbox();
        if inbox.closed {
            return Ok(false);
        }
        if inbox.enqueued + keys.len() > inbox.key_count {
            return Err(NativeJobError::new("invalid-input", "Official asset keys exceed the declared key count"));
        }
        inbox.enqueued += keys.len();
        inbox.pending.extend(keys);
        drop(inbox);
        self.arrived.notify_one();
        Ok(true)
    }

    fn page(&self, offset: usize, limit: usize) -> Result<SettledPage, NativeJobError> {
        if limit == 0 || limit > PAGE {
            return Err(NativeJobError::new("invalid-input", "Official asset settled page must hold 1 to 512 entries"));
        }
        let inbox = self.inbox();
        let entries = inbox
            .settled
            .iter()
            .skip(offset)
            .take(limit)
            .map(|(index, result)| SettledEntry { index: *index, result: *result })
            .collect();
        Ok(SettledPage { entries, total: inbox.settled.len() })
    }
}

/// The shared state as a job status field; equality is identity, so cloning a status stays cheap.
#[derive(Clone)]
pub(crate) struct Handle(Arc<Shared>);

impl Handle {
    pub(crate) fn new(key_count: usize) -> Self {
        Self(Arc::new(Shared {
            inbox: Mutex::new(Inbox { key_count, enqueued: 0, pending: Vec::new(), settled: Vec::new(), closed: false }),
            arrived: tokio::sync::Notify::new(),
        }))
    }
}

impl PartialEq for Handle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Handle {}

impl std::fmt::Debug for Handle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OfficialAssetRestore")
    }
}

fn shared(state: &NativeFileJobState, job_id: &str) -> Result<Arc<Shared>, NativeJobError> {
    let job = state
        .registry
        .lookup(job_id)
        .map_err(|error| NativeJobError::new("store-error", error))?
        .ok_or_else(|| NativeJobError::new("invalid-input", "Official asset restore job is missing"))?;
    let status = job.status.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    status
        .official_asset_shared
        .as_ref()
        .map(|handle| Arc::clone(&handle.0))
        .ok_or_else(|| NativeJobError::new("invalid-input", "Native job is not an official asset restore"))
}

#[tauri::command(async)]
pub(crate) fn native_official_asset_restore_enqueue(
    state: State<'_, NativeFileJobState>,
    job_id: String,
    keys: Vec<String>,
) -> Result<bool, NativeJobError> {
    logged("native_official_asset_restore_enqueue", shared(&state, &job_id).and_then(|shared| shared.enqueue(keys)))
}

#[tauri::command(async)]
pub(crate) fn native_official_asset_restore_settled(
    state: State<'_, NativeFileJobState>,
    job_id: String,
    offset: usize,
    limit: usize,
) -> Result<SettledPage, NativeJobError> {
    logged("native_official_asset_restore_settled", shared(&state, &job_id).and_then(|shared| shared.page(offset, limit)))
}

type InstallResult = (Vec<Body>, Result<Installed, BatchError>);

pub(crate) fn run(request: Request, owned_directory: &Path, job: &JobControl) -> Result<JobResultSummary, NativeJobError> {
    let Request { base_url, credential, account_id, expected_library, key_count, concurrency, reauthenticated, store, revision, shared } = request;
    let shared = shared.0;
    let _closing = Closing(&shared);
    job.start(JobPhase::ReadingSource).map_err(job_error)?;
    let reader = AccountReader::new(&base_url, &credential, owned_directory)?;
    let mut installer = Installer::new(store, job.id(), expected_library)
        .map_err(|error| NativeJobError::new("store-error", error.to_string()))?;
    let pause = scheduler::shared_pause(&account_id, base_url.trim_end_matches('/'));
    let received = AtomicU64::new(0);
    let mut run = Run {
        job,
        shared: &shared,
        reader: &reader,
        received: &received,
        scheduler: Scheduler::new(concurrency, pause, key_count, reauthenticated, Box::new(jitter)),
        key_count,
        keys: Vec::new(),
        fingerprint: Sha256::new(),
        in_flight: HashMap::new(),
        sequence: 0,
        backlog: Backlog::default(),
        progress: RestoreProgress::default(),
        published: None,
        revision,
        installed_bytes: 0,
        cleanup_failed: false,
    };
    std::thread::scope(|scope| {
        let (batches, inbox) = mpsc::channel::<Vec<Body>>();
        let (results, installed) = tokio::sync::mpsc::unbounded_channel::<InstallResult>();
        scope.spawn(move || {
            while let Ok(batch) = inbox.recv() {
                let result = installer.install(&batch, &|| job.is_cancel_requested());
                if results.send((batch, result)).is_err() {
                    break;
                }
            }
        });
        tauri::async_runtime::block_on(run.drive(&batches, installed))
    })?;
    run.publish(true);
    Ok(JobResultSummary {
        export_exclusions: None,
        revision: run.revision,
        source_bytes: run.installed_bytes,
        source_sha256: hex::encode(run.fingerprint.finalize()),
        source_fingerprint_kind: SourceFingerprintKind::WholeFileSha256,
        character_count: 0,
        preset_count: 0,
        warning_codes: if run.cleanup_failed { vec!["cleanup-failed".to_owned()] } else { Vec::new() },
        handoff_path: None,
        publication: None,
    })
}

/// Stops accepting keys once the job is no longer running them.
struct Closing<'a>(&'a Shared);

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        let mut inbox = self.0.inbox();
        inbox.closed = true;
        inbox.pending.clear();
    }
}

enum Event {
    Read(u64, Read),
    Installed(Option<InstallResult>),
    Wake,
}

/// Spooled bodies waiting for installation and the batch the install thread holds.
#[derive(Default)]
struct Backlog {
    queue: VecDeque<Body>,
    queued_bytes: u64,
    /// Bodies and bytes of the batch being installed.
    installing: Option<(usize, u64)>,
}

impl Backlog {
    /// Admission waits while the spool holds a batch, counting the batch being installed.
    fn open(&self) -> bool {
        let (bodies, bytes) = self.installing.unwrap_or_default();
        self.queue.len() + bodies < BACKLOG_BODIES && self.queued_bytes + bytes < BACKLOG_BYTES
    }

    fn push(&mut self, body: Body) {
        self.queued_bytes += body.spooled.size;
        self.queue.push_back(body);
    }

    /// The next batch, once the previous one is installed.
    fn take(&mut self) -> Option<Vec<Body>> {
        if self.installing.is_some() || self.queue.is_empty() {
            return None;
        }
        let mut batch = Vec::new();
        let mut bytes = 0;
        while let Some(body) = self.queue.front() {
            if !batch.is_empty() && (batch.len() >= BACKLOG_BODIES || bytes + body.spooled.size > BACKLOG_BYTES) {
                break;
            }
            bytes += body.spooled.size;
            batch.extend(self.queue.pop_front());
        }
        self.queued_bytes -= bytes;
        self.installing = Some((batch.len(), bytes));
        Some(batch)
    }

    fn installed(&mut self) {
        self.installing = None;
    }

    /// Drops the bodies not yet handed to the install thread.
    fn discard(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
    }

    fn is_installing(&self) -> bool {
        self.installing.is_some()
    }

    fn is_idle(&self) -> bool {
        self.installing.is_none() && self.queue.is_empty()
    }
}

struct Run<'a> {
    job: &'a JobControl,
    shared: &'a Shared,
    reader: &'a AccountReader,
    received: &'a AtomicU64,
    scheduler: Scheduler,
    key_count: usize,
    keys: Vec<String>,
    fingerprint: Sha256,
    in_flight: HashMap<u64, Attempt>,
    sequence: u64,
    backlog: Backlog,
    progress: RestoreProgress,
    published: Option<Instant>,
    revision: i64,
    installed_bytes: u64,
    cleanup_failed: bool,
}

impl Run<'_> {
    async fn drive(
        &mut self,
        batches: &mpsc::Sender<Vec<Body>>,
        mut installed: tokio::sync::mpsc::UnboundedReceiver<InstallResult>,
    ) -> Result<(), NativeJobError> {
        let (job, reader, received, shared) = (self.job, self.reader, self.received, self.shared);
        let mut reads = FuturesUnordered::new();
        loop {
            self.absorb();
            let cancelled = job.is_cancel_requested();
            if (cancelled || self.scheduler.reason().is_some()) && !self.in_flight.is_empty() {
                // Dropping a read aborts its request and removes its partial body.
                reads = FuturesUnordered::new();
                for (_, attempt) in self.in_flight.drain() {
                    self.scheduler.abandon(attempt);
                }
            }
            if cancelled || matches!(self.scheduler.reason(), Some(StopReason::Local | StopReason::LibraryChanged)) {
                self.backlog.discard();
            }
            if cancelled && !self.backlog.is_installing() {
                return Err(NativeJobError::new("cancelled", "Official asset restore was cancelled"));
            }
            if !cancelled {
                let now = now_ms();
                while let Some(attempt) = self.scheduler.next(now, self.backlog.open()) {
                    self.sequence += 1;
                    let (sequence, key) = (self.sequence, self.keys[attempt.index].clone());
                    self.in_flight.insert(sequence, attempt);
                    reads.push(async move { (sequence, reader.read(&key, sequence, job, received).await) });
                }
            }
            if let Some(batch) = self.backlog.take() {
                batches.send(batch).map_err(|_| installer_stopped())?;
            }
            self.publish(false);
            if self.scheduler.done() && self.backlog.is_idle() {
                return Ok(());
            }
            let wait = self
                .scheduler
                .wake_at(self.backlog.open())
                .map_or(TICK, |at| TICK.min(Duration::from_millis(at.saturating_sub(now_ms()))));
            let installing = self.backlog.is_installing();
            let event = tokio::select! {
                Some((sequence, read)) = reads.next(), if !reads.is_empty() => Event::Read(sequence, read),
                result = installed.recv(), if installing => Event::Installed(result),
                _ = shared.arrived.notified() => Event::Wake,
                _ = tokio::time::sleep(wait) => Event::Wake,
            };
            match event {
                Event::Read(sequence, read) => self.read_finished(sequence, read),
                Event::Installed(Some((batch, result))) => self.batch_finished(batch, result),
                Event::Installed(None) => return Err(installer_stopped()),
                Event::Wake => {}
            }
        }
    }

    fn absorb(&mut self) {
        let keys = std::mem::take(&mut self.shared.inbox().pending);
        for key in &keys {
            self.fingerprint.update(key.as_bytes());
            self.fingerprint.update([0]);
        }
        self.scheduler.receive(keys.len());
        self.keys.extend(keys);
    }

    fn read_finished(&mut self, sequence: u64, read: Read) {
        let Some(attempt) = self.in_flight.remove(&sequence) else { return };
        let index = attempt.index;
        let (exchange, spooled) = match read {
            Read::Cancelled => {
                self.scheduler.abandon(attempt);
                return;
            }
            Read::Body(spooled) => (Exchange::Body, Some(spooled)),
            Read::Ended(exchange) => (exchange, None),
        };
        match (self.scheduler.finish(attempt, exchange, now_ms()), spooled) {
            (Outcome::Install, Some(spooled)) => {
                self.backlog.push(Body { index, key: self.keys[index].clone(), spooled });
            }
            (Outcome::Settled(result), _) => self.settle(index, result),
            _ => {}
        }
    }

    fn batch_finished(&mut self, batch: Vec<Body>, result: Result<Installed, BatchError>) {
        self.backlog.installed();
        let (settled, stop) = match result {
            Ok(installed) => {
                self.revision = installed.revision;
                self.cleanup_failed |= installed.cleanup_failed;
                self.installed_bytes += batch.iter().map(|body| body.spooled.size).sum::<u64>();
                (Settled::Installed, None)
            }
            Err(BatchError::Cancelled) => return,
            Err(BatchError::LibraryChanged) => (Settled::Failed, Some(StopReason::LibraryChanged)),
            Err(BatchError::Local) => (Settled::Failed, Some(StopReason::Local)),
        };
        for body in &batch {
            self.settle(body.index, settled);
        }
        if let Some(reason) = stop {
            self.scheduler.stop(reason);
        }
    }

    fn settle(&mut self, index: usize, result: Settled) {
        match result {
            Settled::Installed => self.progress.installed += 1,
            Settled::Missing => self.progress.missing += 1,
            Settled::Failed => self.progress.failed += 1,
        }
        self.shared.inbox().settled.push((index, result));
    }

    fn publish(&mut self, force: bool) {
        let now = Instant::now();
        if !force && self.published.is_some_and(|published| now.duration_since(published) < TICK) {
            return;
        }
        self.published = Some(now);
        let at = now_ms();
        self.progress.bytes = self.received.load(Ordering::Relaxed);
        self.progress.active = self.scheduler.active();
        self.progress.limit = self.scheduler.limit();
        self.progress.waiting_until = self.scheduler.waiting_until(at);
        self.progress.reason = self.scheduler.reason();
        let settled = self.progress.installed + self.progress.missing + self.progress.failed;
        let mut status = self.job.status.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if status.state != JobState::Running {
            return;
        }
        status.progress = JobProgress {
            completed_bytes: self.progress.bytes,
            total_bytes: None,
            completed_items: settled,
            total_items: Some(self.key_count as u64),
        };
        status.official_asset_restore = Some(self.progress.clone());
    }
}

fn installer_stopped() -> NativeJobError {
    NativeJobError::new("store-error", "Official asset installer stopped")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn jitter() -> f64 {
    // The low 53 bits of a v4 UUID are random.
    (uuid::Uuid::new_v4().as_u128() as u64 & ((1 << 53) - 1)) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests;
