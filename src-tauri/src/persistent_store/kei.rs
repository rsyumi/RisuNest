use super::{
    checkpoint_after_detached_release, compare_plugin_storage_keys, RevisionReadLease, StoreError,
    StoreResult,
};
use crate::native_file_jobs::{
    JobControl, JobPhase, JobProgress, JobResultSummary, NativeJobError,
};
use futures::future::{select, Either};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::{Body, Url};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

const KEI_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const KEI_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

fn javascript_array_index(key: &str) -> Option<u32> {
    let value = key.parse::<u32>().ok()?;
    (value < u32::MAX && value.to_string() == key).then_some(value)
}

fn compare_utf16(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

fn compare_canonical_keys(left: &str, right: &str) -> Ordering {
    match (javascript_array_index(left), javascript_array_index(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => compare_utf16(left, right),
    }
}

fn write_canonical_value(writer: &mut impl Write, value: &Value) -> StoreResult<()> {
    match value {
        Value::Array(values) => {
            writer.write_all(b"[")?;
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",")?;
                }
                write_canonical_value(writer, value)?;
            }
            writer.write_all(b"]")?;
        }
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_by(|left, right| compare_canonical_keys(left, right));
            writer.write_all(b"{")?;
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",")?;
                }
                serde_json::to_writer(&mut *writer, key)?;
                writer.write_all(b":")?;
                write_canonical_value(writer, &object[key])?;
            }
            writer.write_all(b"}")?;
        }
        Value::Number(number) => {
            let number = number.as_f64().ok_or_else(|| StoreError::Validation {
                message: "Persistent JSON number is outside the JavaScript range".to_owned(),
            })?;
            writer.write_all(ryu_js::Buffer::new().format(number).as_bytes())?;
        }
        _ => serde_json::to_writer(writer, value)?,
    }
    Ok(())
}

pub(crate) struct PreparedKeiUpload {
    database_path: PathBuf,
    output_directory: PathBuf,
    reader: Option<RevisionReadLease>,
    revision: i64,
    url: Url,
    expected_account_id: String,
    token: String,
    account_validated: bool,
}

pub(super) struct KeiPayloadFile {
    path: PathBuf,
    bytes: u64,
    sha256: String,
    character_count: u64,
    preset_count: u64,
    armed: bool,
}

impl Drop for KeiPayloadFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl KeiPayloadFile {
    fn cleanup(mut self) -> StoreResult<()> {
        fs::remove_file(&self.path)?;
        self.armed = false;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KeiUploadResult {
    pub(crate) revision: i64,
    pub(crate) bytes: u64,
    pub(crate) status: u16,
}

pub(super) fn prepare_upload(
    snapshots_dir: &Path,
    lease: &str,
    reader: RevisionReadLease,
    url: &str,
    expected_account_id: &str,
    token: &str,
) -> Result<PreparedKeiUpload, (StoreError, RevisionReadLease)> {
    prepare_upload_inner(
        snapshots_dir,
        lease,
        reader,
        None,
        url,
        expected_account_id,
        token,
        true,
    )
}

pub(super) fn prepare_job_upload(
    snapshots_dir: &Path,
    lease: &str,
    reader: RevisionReadLease,
    expected_revision: i64,
    url: &str,
    expected_account_id: &str,
    token: &str,
) -> Result<PreparedKeiUpload, (StoreError, RevisionReadLease)> {
    prepare_upload_inner(
        snapshots_dir,
        lease,
        reader,
        Some(expected_revision),
        url,
        expected_account_id,
        token,
        false,
    )
}

fn prepare_upload_inner(
    snapshots_dir: &Path,
    lease: &str,
    reader: RevisionReadLease,
    expected_revision: Option<i64>,
    url: &str,
    expected_account_id: &str,
    token: &str,
    validate_root: bool,
) -> Result<PreparedKeiUpload, (StoreError, RevisionReadLease)> {
    let result = (|| -> StoreResult<(PathBuf, PathBuf, Url)> {
        if !lease.starts_with("snapshot-") {
            return Err(StoreError::Validation {
                message: "revision lease must be a snapshot lease".to_owned(),
            });
        }
        if let Some(expected_revision) = expected_revision {
            if reader.target.revision != expected_revision {
                return Err(StoreError::RevisionConflict {
                    expected: expected_revision,
                    actual: reader.target.revision,
                });
            }
        }
        if validate_root {
            let root: String = reader
                .connection
                .query_row(
                    "SELECT value FROM root WHERE generation = ?1",
                    [&reader.target.generation],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| StoreError::Validation {
                    message: "Pinned generation has no persistent root".to_owned(),
                })?;
            validate_account(&root, expected_account_id, token)?;
        }
        let url = Url::parse(url).map_err(|_| StoreError::Validation {
            message: "KEI backup URL is invalid".to_owned(),
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(StoreError::Validation {
                message: "KEI backup URL must use HTTP or HTTPS".to_owned(),
            });
        }
        let persistent_directory = snapshots_dir.parent().ok_or_else(|| StoreError::Store {
            message: "Persistent store directory is unavailable".to_owned(),
        })?;
        reader.publish_detached_asset_roots()?;
        Ok((
            persistent_directory.join(super::DATABASE_FILE),
            persistent_directory.join("kei-upload"),
            url,
        ))
    })();
    match result {
        Ok((database_path, output_directory, url)) => Ok(PreparedKeiUpload {
            database_path,
            output_directory,
            revision: reader.target.revision,
            reader: Some(reader),
            url,
            expected_account_id: expected_account_id.to_owned(),
            token: token.to_owned(),
            account_validated: validate_root,
        }),
        Err(error) => Err((error, reader)),
    }
}

pub(super) fn sweep_abandoned(snapshots_dir: &Path) {
    let Some(persistent_directory) = snapshots_dir.parent() else {
        return;
    };
    let output_directory = persistent_directory.join("kei-upload");
    let Ok(entries) = fs::read_dir(output_directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(id) = name
            .strip_prefix("kei-")
            .and_then(|name| name.strip_suffix(".json.tmp"))
        else {
            continue;
        };
        if Uuid::parse_str(id).is_ok() {
            let _ = fs::remove_file(path);
        }
    }
}

fn validate_account(root: &str, expected_account_id: &str, token: &str) -> StoreResult<()> {
    let root: Value = serde_json::from_str(root)?;
    validate_account_value(&root, expected_account_id, token)
}

fn validate_account_value(root: &Value, expected_account_id: &str, token: &str) -> StoreResult<()> {
    let account = root
        .get("account")
        .and_then(Value::as_object)
        .ok_or_else(|| StoreError::Validation {
            message: "Pinned KEI account is unavailable".to_owned(),
        })?;
    if account.get("kei").and_then(Value::as_bool) != Some(true)
        || account.get("id").and_then(Value::as_str) != Some(expected_account_id)
        || account.get("token").and_then(Value::as_str) != Some(token)
    {
        return Err(StoreError::Validation {
            message: "KEI account changed before the pinned backup".to_owned(),
        });
    }
    Ok(())
}

impl PreparedKeiUpload {
    pub(crate) fn revision(&self) -> i64 {
        self.revision
    }

    pub(super) fn create_payload(&self) -> StoreResult<KeiPayloadFile> {
        self.create_payload_controlled(|| false)
    }

    fn create_payload_controlled(
        &self,
        is_cancelled: impl Fn() -> bool,
    ) -> StoreResult<KeiPayloadFile> {
        fs::create_dir_all(&self.output_directory)?;
        let reader = self.reader.as_ref().ok_or(StoreError::SnapshotReleased)?;
        let character_count =
            record_count(&reader.connection, "characters", &reader.target.generation)?;
        let preset_count =
            record_count(&reader.connection, "bot_presets", &reader.target.generation)?;
        let path = self
            .output_directory
            .join(format!("kei-{}.json.tmp", Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut guard = PayloadOutputGuard::new(path.clone());
        let mut writer = HashingWriter::new(CancellableWriter {
            inner: &mut file,
            is_cancelled,
        });
        write_payload(
            &reader.connection,
            &reader.target.generation,
            &self.token,
            (!self.account_validated).then_some(self.expected_account_id.as_str()),
            &mut writer,
        )?;
        let (bytes, sha256) = writer.finish();
        file.flush()?;
        file.sync_all()?;
        drop(file);
        if fs::metadata(&path)?.len() != bytes {
            return Err(StoreError::Store {
                message: "KEI payload length changed after serialization".to_owned(),
            });
        }
        guard.disarm();
        Ok(KeiPayloadFile {
            path,
            bytes,
            sha256,
            character_count,
            preset_count,
            armed: true,
        })
    }

    fn release_reader(&mut self) -> StoreResult<()> {
        let Some(reader) = self.reader.take() else {
            return Ok(());
        };
        checkpoint_after_detached_release_after_close(&self.database_path, reader)
    }

    pub(super) async fn upload(mut self) -> StoreResult<KeiUploadResult> {
        let revision = self.revision;
        let url = self.url.clone();
        let payload = tokio::task::spawn_blocking(move || {
            let payload = self.create_payload();
            let release = self.release_reader();
            match (payload, release) {
                (Ok(payload), Ok(())) => Ok(payload),
                (Err(error), _) => Err(error),
                (Ok(_), Err(error)) => Err(error),
            }
        })
        .await
        .map_err(|_| StoreError::Store {
            message: "KEI payload worker stopped unexpectedly".to_owned(),
        })??;
        let bytes = payload.bytes;
        let upload = upload_payload(&url, &payload).await;
        let cleanup = payload.cleanup();
        let status = match upload {
            Ok(status) => status,
            Err(error) => return Err(error),
        };
        cleanup?;
        Ok(KeiUploadResult {
            revision,
            bytes,
            status,
        })
    }
}

impl Drop for PreparedKeiUpload {
    fn drop(&mut self) {
        let _ = self.release_reader();
    }
}

pub(crate) fn run_job(
    mut prepared: PreparedKeiUpload,
    job: Arc<JobControl>,
) -> Result<JobResultSummary, NativeJobError> {
    if job.is_cancel_requested() {
        return Err(cancelled("KEI backup cancelled before serialization"));
    }
    job.start(JobPhase::WritingExport).map_err(|error| {
        job_control_error(&job, "KEI backup cancelled before serialization", error)
    })?;
    let revision = prepared.revision;
    let payload = prepared
        .create_payload_controlled(|| job.is_cancel_requested())
        .map_err(|error| {
            if job.is_cancel_requested() {
                cancelled("KEI backup cancelled during serialization")
            } else {
                store_error(error)
            }
        })?;
    if let Err(error) = prepared.release_reader() {
        return cleanup_job_payload(payload, Err(store_error(error)));
    }
    if job.is_cancel_requested() {
        return cleanup_job_payload(
            payload,
            Err(cancelled("KEI backup cancelled before upload")),
        );
    }
    set_job_phase(
        &job,
        JobPhase::PublishingDestination,
        "KEI backup cancelled before upload",
    )?;
    set_job_progress(
        &job,
        JobProgress {
            completed_bytes: 0,
            total_bytes: Some(payload.bytes),
            completed_items: 1,
            total_items: Some(2),
        },
        "KEI backup cancelled before upload",
    )?;

    let upload = tauri::async_runtime::block_on(upload_payload_for_job(
        &prepared.url,
        &payload,
        Arc::clone(&job),
        KEI_CONNECT_TIMEOUT,
        KEI_IDLE_TIMEOUT,
    ));
    let outcome = match upload {
        Ok(_) => {
            if job.is_cancel_requested() {
                Err(cancelled("KEI backup cancelled during upload"))
            } else {
                set_job_phase(
                    &job,
                    JobPhase::FinalizingExport,
                    "KEI backup cancelled during upload",
                )?;
                set_job_progress(
                    &job,
                    JobProgress {
                        completed_bytes: payload.bytes,
                        total_bytes: Some(payload.bytes),
                        completed_items: 2,
                        total_items: Some(2),
                    },
                    "KEI backup cancelled during upload",
                )?;
                Ok(JobResultSummary {
                    export_exclusions: None,
                    revision,
                    source_bytes: payload.bytes,
                    source_sha256: payload.sha256.clone(),
                    character_count: payload.character_count,
                    preset_count: payload.preset_count,
                    warning_codes: Vec::new(),
                    handoff_path: None,
                    publication: None,
                })
            }
        }
        Err(_) if job.is_cancel_requested() => Err(cancelled("KEI backup cancelled during upload")),
        Err(error) => Err(store_error(error)),
    };
    cleanup_job_payload(payload, outcome)
}

fn set_job_phase(
    job: &JobControl,
    phase: JobPhase,
    cancellation_message: &str,
) -> Result<(), NativeJobError> {
    job.set_phase(phase)
        .map_err(|error| job_control_error(job, cancellation_message, error))
}

fn set_job_progress(
    job: &JobControl,
    progress: JobProgress,
    cancellation_message: &str,
) -> Result<(), NativeJobError> {
    job.set_progress(progress)
        .map_err(|error| job_control_error(job, cancellation_message, error))
}

fn job_control_error(
    job: &JobControl,
    cancellation_message: &str,
    error: impl AsRef<str>,
) -> NativeJobError {
    if job.is_cancel_requested() {
        cancelled(cancellation_message)
    } else {
        job_error(error)
    }
}

fn cleanup_job_payload(
    payload: KeiPayloadFile,
    outcome: Result<JobResultSummary, NativeJobError>,
) -> Result<JobResultSummary, NativeJobError> {
    let cleanup = payload.cleanup();
    match (outcome, cleanup) {
        (Ok(mut result), Err(_)) => {
            result.warning_codes.push("cleanup-failed".to_owned());
            Ok(result)
        }
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Err(cleanup)) => Err(NativeJobError::new(
            "cleanup-failed",
            format!("{}; KEI payload cleanup failed: {cleanup}", error.message),
        )),
        (Err(error), Ok(())) => Err(error),
    }
}

async fn upload_payload_for_job(
    url: &Url,
    payload: &KeiPayloadFile,
    job: Arc<JobControl>,
    connect_timeout: Duration,
    idle_timeout: Duration,
) -> StoreResult<u16> {
    let file = tokio::fs::File::open(&payload.path).await?;
    let body = Body::wrap_stream(ReaderStream::new(JobPayloadReader {
        file,
        job: Arc::clone(&job),
        completed: 0,
        total: payload.bytes,
    }));
    let client = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .build()
        .map_err(|_| StoreError::Store {
            message: "KEI backup HTTP client could not be created".to_owned(),
        })?;
    let request = client
        .post(url.clone())
        .header(CONTENT_TYPE, "application/json")
        .header(CONTENT_LENGTH, payload.bytes)
        .body(body)
        .send();
    let response = match await_upload_with_job_control(request, &job, idle_timeout).await {
        Ok(response) => response,
        Err(ControlledUploadError::Request(error)) => {
            return Err(sanitized_upload_error(error));
        }
        Err(ControlledUploadError::Cancelled) => {
            return Err(StoreError::Store {
                message: "KEI backup upload cancelled".to_owned(),
            });
        }
        Err(ControlledUploadError::IdleTimeout) => {
            return Err(StoreError::Store {
                message: "KEI backup upload made no progress before timeout".to_owned(),
            });
        }
    };
    Ok(response.status().as_u16())
}

#[derive(Debug)]
enum ControlledUploadError<E> {
    Request(E),
    Cancelled,
    IdleTimeout,
}

async fn await_upload_with_job_control<F, T, E>(
    future: F,
    job: &JobControl,
    idle_timeout: Duration,
) -> Result<T, ControlledUploadError<E>>
where
    F: Future<Output = Result<T, E>>,
{
    let poll_interval = idle_timeout
        .min(Duration::from_millis(50))
        .max(Duration::from_millis(1));
    let mut future = Box::pin(future);
    let mut completed_bytes = job.status().progress.completed_bytes;
    let mut last_progress = Instant::now();
    loop {
        let timer = Box::pin(tokio::time::sleep(poll_interval));
        match select(future, timer).await {
            Either::Left((result, _)) => {
                return result.map_err(ControlledUploadError::Request);
            }
            Either::Right((_, pending)) => future = pending,
        }
        if job.is_cancel_requested() {
            return Err(ControlledUploadError::Cancelled);
        }
        let current_bytes = job.status().progress.completed_bytes;
        if current_bytes != completed_bytes {
            completed_bytes = current_bytes;
            last_progress = Instant::now();
        } else if last_progress.elapsed() >= idle_timeout {
            return Err(ControlledUploadError::IdleTimeout);
        }
    }
}

fn sanitized_upload_error(error: reqwest::Error) -> StoreError {
    let message = if error.is_timeout() {
        "KEI backup upload timed out"
    } else if error.is_connect() {
        "KEI backup endpoint is unavailable"
    } else if error.is_body() {
        "KEI backup payload could not be streamed"
    } else {
        "KEI backup upload failed"
    };
    StoreError::Store {
        message: message.to_owned(),
    }
}

struct JobPayloadReader {
    file: tokio::fs::File,
    job: Arc<JobControl>,
    completed: u64,
    total: u64,
}

impl AsyncRead for JobPayloadReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.job.is_cancel_requested() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                "KEI backup upload cancelled",
            )));
        }
        let before = buffer.filled().len();
        match Pin::new(&mut self.file).poll_read(context, buffer) {
            Poll::Ready(Ok(())) => {
                let read = buffer.filled().len().saturating_sub(before) as u64;
                self.completed = self.completed.saturating_add(read);
                if let Err(error) = self.job.set_progress(JobProgress {
                    completed_bytes: self.completed,
                    total_bytes: Some(self.total),
                    completed_items: 1,
                    total_items: Some(2),
                }) {
                    return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, error)));
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

struct CancellableWriter<W, F> {
    inner: W,
    is_cancelled: F,
}

impl<W: Write, F: Fn() -> bool> Write for CancellableWriter<W, F> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if (self.is_cancelled)() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "KEI backup serialization cancelled",
            ));
        }
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        if (self.is_cancelled)() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "KEI backup serialization cancelled",
            ));
        }
        self.inner.flush()
    }
}

impl<W> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (u64, String) {
        (self.bytes, hex::encode(self.hasher.finalize()))
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        self.bytes = self.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn record_count(connection: &Connection, table: &str, generation: &str) -> StoreResult<u64> {
    let count = connection.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE generation = ?1"),
        [generation],
        |row| row.get::<_, i64>(0),
    )?;
    u64::try_from(count).map_err(|_| StoreError::Validation {
        message: format!("Pinned {table} count is invalid"),
    })
}

fn cancelled(message: &str) -> NativeJobError {
    NativeJobError::new("cancelled", message)
}

fn job_error(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("job-error", message)
}

fn store_error(error: StoreError) -> NativeJobError {
    match error {
        StoreError::RevisionConflict { .. } => {
            NativeJobError::new("revision-conflict", error.to_string())
        }
        StoreError::SnapshotReleased => NativeJobError::new("store-error", error.to_string()),
        StoreError::Validation { .. } => NativeJobError::new("invalid-input", error.to_string()),
        StoreError::Store { .. } => NativeJobError::new("transport-failed", error.to_string()),
    }
}

fn checkpoint_after_detached_release_after_close(
    database_path: &Path,
    reader: RevisionReadLease,
) -> StoreResult<()> {
    let active_readers = reader.active_readers();
    super::snapshot::close_revision(reader)?;
    checkpoint_after_detached_release(database_path, &active_readers)
}

async fn upload_payload(url: &Url, payload: &KeiPayloadFile) -> StoreResult<u16> {
    let file = tokio::fs::File::open(&payload.path).await?;
    let body = Body::wrap_stream(ReaderStream::new(file));
    let response = reqwest::Client::new()
        .post(url.clone())
        .header(CONTENT_TYPE, "application/json")
        .header(CONTENT_LENGTH, payload.bytes)
        .body(body)
        .send()
        .await
        .map_err(|error| {
            let summary = if error.is_timeout() {
                "KEI backup upload timed out"
            } else if error.is_connect() {
                "KEI backup endpoint is unavailable"
            } else if error.is_body() {
                "KEI backup payload could not be streamed"
            } else {
                "KEI backup upload failed"
            };
            StoreError::Store {
                message: format!("{summary}: {error}"),
            }
        })?;
    Ok(response.status().as_u16())
}

struct PayloadOutputGuard {
    path: PathBuf,
    armed: bool,
}

impl PayloadOutputGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PayloadOutputGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn write_payload(
    connection: &Connection,
    generation: &str,
    token: &str,
    expected_account_id: Option<&str>,
    writer: &mut impl Write,
) -> StoreResult<()> {
    writer.write_all(b"{\"token\":")?;
    serde_json::to_writer(&mut *writer, token)?;
    writer.write_all(b",\"database\":")?;
    write_database(connection, generation, expected_account_id, token, writer)?;
    writer.write_all(b"}")?;
    Ok(())
}

enum DatabaseField<'a> {
    Root(&'a str),
    Presets,
    Characters,
    PluginStorage,
}

impl DatabaseField<'_> {
    fn key(&self) -> &str {
        match self {
            Self::Root(key) => key,
            Self::Presets => "botPresets",
            Self::Characters => "characters",
            Self::PluginStorage => "pluginCustomStorage",
        }
    }
}

fn write_database(
    connection: &Connection,
    generation: &str,
    expected_account_id: Option<&str>,
    token: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let root_json: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [generation],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Validation {
            message: "Pinned generation has no persistent root".to_owned(),
        })?;
    let root: Value = serde_json::from_str(&root_json)?;
    drop(root_json);
    if let Some(expected_account_id) = expected_account_id {
        validate_account_value(&root, expected_account_id, token)?;
    }
    let root = root.as_object().ok_or_else(|| StoreError::Validation {
        message: "Persistent root must be an object".to_owned(),
    })?;
    let mut fields = root
        .keys()
        .filter(|key| {
            !matches!(
                key.as_str(),
                "botPresets" | "characters" | "pluginCustomStorage"
            )
        })
        .map(|key| DatabaseField::Root(key))
        .collect::<Vec<_>>();
    fields.extend([
        DatabaseField::Presets,
        DatabaseField::Characters,
        DatabaseField::PluginStorage,
    ]);
    fields.sort_by(|left, right| compare_canonical_keys(left.key(), right.key()));

    writer.write_all(b"{")?;
    for (index, field) in fields.into_iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        serde_json::to_writer(&mut *writer, field.key())?;
        writer.write_all(b":")?;
        match field {
            DatabaseField::Root(key) => write_canonical_value(writer, &root[key])?,
            DatabaseField::Presets => write_presets(connection, generation, writer)?,
            DatabaseField::Characters => write_characters(connection, generation, writer)?,
            DatabaseField::PluginStorage => write_plugin_storage(connection, generation, writer)?,
        }
    }
    writer.write_all(b"}")?;
    Ok(())
}

fn write_presets(
    connection: &Connection,
    generation: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "SELECT value FROM bot_presets WHERE generation = ?1 ORDER BY configured_index ASC",
    )?;
    let mut rows = statement.query([generation])?;
    writer.write_all(b"[")?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        write_stored_value(writer, &row.get::<_, String>(0)?)?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn write_characters(
    connection: &Connection,
    generation: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "SELECT character_id, detail FROM characters
         WHERE generation = ?1 ORDER BY configured_index ASC",
    )?;
    let mut rows = statement.query([generation])?;
    writer.write_all(b"[")?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        let character_id = row.get::<_, String>(0)?;
        let detail = row.get::<_, String>(1)?;
        write_character(connection, generation, &character_id, &detail, writer)?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn write_character(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    detail: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let detail: Value = serde_json::from_str(detail)?;
    let detail = detail.as_object().ok_or_else(|| StoreError::Validation {
        message: "Character detail must be an object".to_owned(),
    })?;
    write_object_with_virtual_field(writer, detail, "chats", |writer| {
        write_conversations(connection, generation, character_id, writer)
    })
}

fn write_conversations(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "SELECT conversation_id, detail FROM conversations
         WHERE generation = ?1 AND character_id = ?2 ORDER BY configured_index ASC",
    )?;
    let mut rows = statement.query(params![generation, character_id])?;
    writer.write_all(b"[")?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        let conversation_id = row.get::<_, String>(0)?;
        let detail = row.get::<_, String>(1)?;
        write_conversation(
            connection,
            generation,
            character_id,
            &conversation_id,
            &detail,
            writer,
        )?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn write_conversation(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    conversation_id: &str,
    detail: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let detail: Value = serde_json::from_str(detail)?;
    let detail = detail.as_object().ok_or_else(|| StoreError::Validation {
        message: "Conversation detail must be an object".to_owned(),
    })?;
    write_object_with_virtual_field(writer, detail, "message", |writer| {
        write_messages(
            connection,
            generation,
            character_id,
            conversation_id,
            writer,
        )
    })
}

fn write_messages(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    conversation_id: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "SELECT value FROM messages
         WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3
         ORDER BY message_index ASC",
    )?;
    let mut rows = statement.query(params![generation, character_id, conversation_id])?;
    writer.write_all(b"[")?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        write_stored_value(writer, &row.get::<_, String>(0)?)?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn write_object_with_virtual_field<W, F>(
    writer: &mut W,
    object: &serde_json::Map<String, Value>,
    virtual_key: &str,
    write_virtual: F,
) -> StoreResult<()>
where
    W: Write,
    F: FnOnce(&mut W) -> StoreResult<()>,
{
    let mut keys = object
        .keys()
        .filter(|key| key.as_str() != virtual_key)
        .map(String::as_str)
        .chain(std::iter::once(virtual_key))
        .collect::<Vec<_>>();
    keys.sort_by(|left, right| compare_canonical_keys(left, right));
    writer.write_all(b"{")?;
    let mut write_virtual = Some(write_virtual);
    for (index, key) in keys.into_iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        serde_json::to_writer(&mut *writer, key)?;
        writer.write_all(b":")?;
        if key == virtual_key {
            write_virtual.take().expect("virtual field is written once")(writer)?;
        } else {
            write_canonical_value(writer, &object[key])?;
        }
    }
    writer.write_all(b"}")?;
    Ok(())
}

fn write_plugin_storage(
    connection: &Connection,
    generation: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let mut statement = connection
        .prepare("SELECT storage_key, ordinal FROM plugin_storage WHERE generation = ?1")?;
    let mut entries = statement
        .query_map([generation], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|(left, left_ordinal), (right, right_ordinal)| {
        compare_plugin_storage_keys(left, *left_ordinal, right, *right_ordinal)
    });
    writer.write_all(b"{")?;
    for (index, (key, _)) in entries.into_iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        serde_json::to_writer(&mut *writer, &key)?;
        writer.write_all(b":")?;
        let value: String = connection.query_row(
            "SELECT value FROM plugin_storage WHERE generation = ?1 AND storage_key = ?2",
            params![generation, key],
            |row| row.get(0),
        )?;
        write_stored_value(writer, &value)?;
    }
    writer.write_all(b"}")?;
    Ok(())
}

fn write_stored_value(writer: &mut impl Write, serialized: &str) -> StoreResult<()> {
    let value: Value = serde_json::from_str(serialized)?;
    write_canonical_value(writer, &value)
}

#[cfg(test)]
mod tests {
    use super::{
        await_upload_with_job_control, run_job, set_job_phase, set_job_progress,
        write_canonical_value, CancellableWriter, ControlledUploadError,
    };
    use crate::native_file_jobs::{JobKind, JobPhase, JobProgress, JobRegistry};
    use crate::persistent_store::{AssetAlias, PersistentStore, WorkingSetCommit};
    use serde_json::json;
    use std::cell::Cell;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    const EXPECTED_PAYLOAD: &str = "{\"token\":\"secret-token\",\"database\":{\"account\":{\"data\":{},\"id\":\"account-1\",\"kei\":true,\"token\":\"secret-token\"},\"botPresets\":[{\"a\":1,\"name\":\"preset\",\"z\":2}],\"characters\":[{\"a\":1,\"chaId\":\"char-1\",\"chats\":[{\"id\":\"chat-1\",\"message\":[{\"chatId\":\"message-1\",\"data\":\"hello\",\"role\":\"user\"}],\"name\":\"Chat\",\"note\":\"\"}],\"name\":\"Char\",\"type\":\"character\",\"z\":2}],\"pluginCustomStorage\":{\"2\":\"index\",\"beta\":{\"a\":1,\"z\":2},\"alpha\":\"first\"},\"z\":{\"2\":\"two\",\"10\":\"ten\",\"a\":\"line\\n\",\"b\":2}}}";

    #[test]
    fn cancellation_writer_stops_write_all_without_retrying_into_output() {
        let cancellation_checks = Cell::new(0);
        let mut writer = CancellableWriter {
            inner: Vec::new(),
            is_cancelled: || {
                let checks = cancellation_checks.get() + 1;
                cancellation_checks.set(checks);
                checks <= 3
            },
        };

        let error = writer
            .write_all(b"synthetic payload")
            .expect_err("cancelled writer must stop");

        assert_ne!(error.kind(), std::io::ErrorKind::Interrupted);
        assert_eq!(error.to_string(), "KEI backup serialization cancelled");
        assert_eq!(cancellation_checks.get(), 1);
        assert!(writer.inner.is_empty());
    }

    fn open_store_with_fixture() -> (tempfile::TempDir, PersistentStore, String) {
        let directory = tempfile::tempdir().expect("create temp directory");
        let mut store = PersistentStore::open(directory.path()).expect("open store");
        let database = json!({
            "z": { "10": "ten", "2": "two", "b": 2, "a": "line\n" },
            "account": {
                "token": "secret-token",
                "kei": true,
                "id": "account-1",
                "data": {}
            },
            "botPresets": [{ "name": "preset", "z": 2, "a": 1 }],
            "pluginCustomStorage": {
                "beta": { "z": 2, "a": 1 },
                "2": "index",
                "alpha": "first"
            },
            "characters": [{
                "name": "Char",
                "type": "character",
                "chaId": "char-1",
                "chats": [{
                    "name": "Chat",
                    "id": "chat-1",
                    "message": [{ "role": "user", "data": "hello", "chatId": "message-1" }],
                    "note": ""
                }],
                "z": 2,
                "a": 1
            }]
        });
        let staging = store.replace_begin().expect("begin replacement");
        let mut root = database.clone();
        let root = root.as_object_mut().expect("database object");
        let characters = root
            .remove("characters")
            .expect("characters")
            .as_array()
            .expect("character array")
            .clone();
        let presets = root
            .remove("botPresets")
            .expect("presets")
            .as_array()
            .expect("preset array")
            .clone();
        store
            .replace_put_root(
                &staging.staging_id,
                &serde_json::Value::Object(root.clone()),
            )
            .expect("stage root");
        store
            .replace_put_presets(&staging.staging_id, &presets)
            .expect("stage presets");
        store
            .replace_add_characters(&staging.staging_id, &characters)
            .expect("stage characters");
        store
            .replace_put_asset_aliases(
                &staging.staging_id,
                &[AssetAlias {
                    key: "assets/kei-reader.bin".to_owned(),
                    object_hash: Some("ab".repeat(32)),
                    kind: "asset".to_owned(),
                    size: 1,
                    mime: "application/octet-stream".to_owned(),
                    name: "kei-reader.bin".to_owned(),
                    ext: "bin".to_owned(),
                    inlay_type: None,
                    width: None,
                    height: None,
                    metadata: json!({}),
                }],
            )
            .expect("stage KEI reader asset root");
        store
            .replace_commit(&staging.staging_id, Some(0))
            .expect("commit fixture");
        let lease = store.acquire_revision(1).expect("acquire lease").lease;
        (directory, store, lease)
    }

    #[test]
    fn canonical_writer_matches_javascript_property_order_and_escaping() {
        let value = json!({
            "10": "ten",
            "2": "two",
            "\u{e000}": "bmp",
            "\u{10000}": "supplementary",
            "a": "line\nquote\"",
            "large-decimal": 1e20,
            "large-exponent": 1e21,
            "small-decimal": 1e-6,
            "small-exponent": 1e-7,
            "negative-zero": -0.0,
        });
        let mut bytes = Vec::new();

        write_canonical_value(&mut bytes, &value).expect("write canonical JSON");

        assert_eq!(
            String::from_utf8(bytes).expect("UTF-8 JSON"),
            "{\"2\":\"two\",\"10\":\"ten\",\"a\":\"line\\nquote\\\"\",\"large-decimal\":100000000000000000000,\"large-exponent\":1e+21,\"negative-zero\":0,\"small-decimal\":0.000001,\"small-exponent\":1e-7,\"𐀀\":\"supplementary\",\"\":\"bmp\"}"
        );
    }

    #[test]
    fn pinned_payload_matches_the_current_kei_json_shape_byte_for_byte() {
        let (_directory, mut store, lease) = open_store_with_fixture();
        let prepared = store
            .prepare_kei_upload(
                &lease,
                "http://127.0.0.1/autobackup/save",
                "account-1",
                "secret-token",
            )
            .expect("prepare upload");

        let payload = prepared.create_payload().expect("create payload");
        let path = payload.path.clone();
        let body = fs::read_to_string(&path).expect("read payload");

        assert_eq!(payload.bytes, body.len() as u64);
        assert_eq!(body, EXPECTED_PAYLOAD);
        drop(payload);
        assert!(!path.exists());
    }

    #[test]
    fn transferred_reader_roots_remain_registered_until_prepared_upload_drops() {
        let (_directory, mut store, lease) = open_store_with_fixture();
        let prepared = store
            .prepare_kei_upload(
                &lease,
                "http://127.0.0.1/autobackup/save",
                "account-1",
                "secret-token",
            )
            .expect("prepare transferred reader");

        let roots = store
            .active_readers
            .detached_asset_roots()
            .expect("read detached KEI roots");
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].object_hashes, ["ab".repeat(32)].into());

        drop(prepared);
        assert!(store
            .active_readers
            .detached_asset_roots()
            .expect("read detached roots after drop")
            .is_empty());
    }

    #[test]
    fn file_upload_preserves_body_headers_and_ignored_http_status_semantics() {
        let (_directory, mut store, lease) = open_store_with_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock endpoint");
        let address = listener.local_addr().expect("mock address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let header_end = loop {
                let read = stream.read(&mut buffer).expect("read request");
                assert!(read > 0, "request ended before headers");
                request.extend_from_slice(&buffer[..read]);
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8(request[..header_end].to_vec()).expect("UTF-8 headers");
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .expect("content-length header");
            while request.len() - header_end < content_length {
                let read = stream.read(&mut buffer).expect("read body");
                assert!(read > 0, "request ended before body");
                request.extend_from_slice(&buffer[..read]);
            }
            request_tx
                .send((
                    headers,
                    request[header_end..header_end + content_length].to_vec(),
                ))
                .expect("send captured request");
            stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("write response");
            stream.flush().expect("flush response");
            stream.shutdown(Shutdown::Write).expect("finish response");
            while stream.read(&mut buffer).expect("drain client close") > 0 {}
        });
        let prepared = store
            .prepare_kei_upload(
                &lease,
                &format!("http://{address}/autobackup/save"),
                "account-1",
                "secret-token",
            )
            .expect("prepare upload");
        let output_directory = prepared.output_directory.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let result = runtime.block_on(prepared.upload()).expect("upload payload");
        let (headers, body) = request_rx.recv().expect("receive request");
        server.join().expect("join mock endpoint");

        assert_eq!(result.revision, 1);
        assert_eq!(result.status, 503);
        assert_eq!(result.bytes, body.len() as u64);
        assert!(headers.starts_with("POST /autobackup/save HTTP/1.1\r\n"));
        assert!(headers
            .lines()
            .any(|line| { line.eq_ignore_ascii_case("content-type: application/json") }));
        assert_eq!(body, EXPECTED_PAYLOAD.as_bytes());
        assert!(fs::read_dir(output_directory)
            .expect("read upload directory")
            .next()
            .is_none());
    }

    #[test]
    fn failed_upload_removes_the_temporary_payload_without_exposing_the_token() {
        let (_directory, mut store, lease) = open_store_with_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve closed endpoint");
        let address = listener.local_addr().expect("closed endpoint address");
        drop(listener);
        let prepared = store
            .prepare_kei_upload(
                &lease,
                &format!("http://{address}/autobackup/save"),
                "account-1",
                "secret-token",
            )
            .expect("prepare upload");
        let output_directory = prepared.output_directory.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let error = runtime
            .block_on(prepared.upload())
            .expect_err("closed endpoint must fail");

        let message = error.to_string();
        assert!(
            message.starts_with("KEI backup endpoint is unavailable: "),
            "request source was not preserved: {message}"
        );
        assert!(!message.contains("secret-token"));
        assert!(fs::read_dir(output_directory)
            .expect("read upload directory")
            .next()
            .is_none());
    }

    #[test]
    fn later_commits_do_not_change_the_pinned_payload() {
        let (_directory, mut store, lease) = open_store_with_fixture();
        let mut root = store.read_root(None).expect("read current root").value;
        root["account"]["token"] = json!("new-token");
        root["z"] = json!({ "changed": true });
        store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root_mutations: None,
                root: Some(root),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                asset_owner_heads: None,
                plugin_storage: None,
            })
            .expect("commit later revision");

        let prepared = store
            .prepare_kei_upload(
                &lease,
                "http://127.0.0.1/autobackup/save",
                "account-1",
                "secret-token",
            )
            .expect("prepare pinned upload");
        let payload = prepared.create_payload().expect("create pinned payload");
        let body = fs::read_to_string(&payload.path).expect("read pinned payload");

        assert!(body.contains("\"token\":\"secret-token\""));
        assert!(!body.contains("new-token"));
        assert!(!body.contains("\"changed\":true"));
    }

    #[test]
    fn account_mismatch_is_rejected_without_creating_a_payload() {
        let (_directory, mut store, lease) = open_store_with_fixture();

        let error = store
            .prepare_kei_upload(
                &lease,
                "http://127.0.0.1/autobackup/save",
                "another-account",
                "secret-token",
            )
            .err()
            .expect("reject account mismatch");

        assert_eq!(
            error.to_string(),
            "KEI account changed before the pinned backup"
        );
    }

    #[test]
    fn native_job_defers_root_validation_until_off_mutex_serialization() {
        let (_directory, mut store, source_lease) = open_store_with_fixture();

        let prepared = store
            .prepare_kei_job_upload(
                &source_lease,
                1,
                "http://127.0.0.1:9/autobackup/save",
                "another-account",
                "secret-token",
            )
            .expect("transfer reader without parsing the root");
        let output_directory = prepared.output_directory.clone();

        let error = prepared
            .create_payload()
            .err()
            .expect("reject account mismatch during serialization");

        assert_eq!(
            error.to_string(),
            "KEI account changed before the pinned backup"
        );
        assert!(
            !output_directory.exists()
                || fs::read_dir(output_directory)
                    .expect("read upload directory")
                    .next()
                    .is_none()
        );
    }

    #[test]
    fn native_job_releases_its_reader_and_cancels_while_waiting_for_response() {
        let (_directory, mut store, source_lease) = open_store_with_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock endpoint");
        let address = listener.local_addr().expect("mock address");
        let (request_tx, request_rx) = mpsc::channel();
        let (respond_tx, respond_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let header_end = loop {
                let read = stream.read(&mut buffer).expect("read request");
                assert!(read > 0, "request ended before headers");
                request.extend_from_slice(&buffer[..read]);
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8(request[..header_end].to_vec()).expect("headers");
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .expect("content length header");
            while request.len() - header_end < content_length {
                let read = stream.read(&mut buffer).expect("read body");
                assert!(read > 0, "request ended before body");
                request.extend_from_slice(&buffer[..read]);
            }
            request_tx
                .send(request[header_end..header_end + content_length].to_vec())
                .expect("send request body");
            respond_rx.recv().expect("wait to release response");
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let prepared = store
            .prepare_kei_job_upload(
                &source_lease,
                1,
                &format!("http://{address}/autobackup/save"),
                "account-1",
                "secret-token",
            )
            .expect("prepare job upload");
        store
            .release_revision(&source_lease)
            .expect("release renderer handoff lease");
        let mut root = store.read_root(None).expect("read current root").value;
        root["z"] = json!({ "changed": true });
        store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root_mutations: None,
                root: Some(root),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                asset_owner_heads: None,
                plugin_storage: None,
            })
            .expect("advance live revision");
        let registry = JobRegistry::default();
        let job = registry
            .create(JobKind::KeiBackupUpload)
            .expect("create job");
        let job_for_worker = job.clone();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            result_tx
                .send(run_job(prepared, job_for_worker))
                .expect("send job result");
        });

        let body = request_rx.recv().expect("receive request body");
        assert_eq!(store.active_readers.active_count(), 0);
        assert!(store
            .active_readers
            .detached_asset_roots()
            .expect("read detached roots")
            .is_empty());
        registry
            .cancel(&job.id())
            .expect("cancel job after body EOF");
        let error = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cancellation must interrupt response wait")
            .expect_err("cancelled KEI job must fail");
        respond_tx.send(()).expect("release response");
        worker.join().expect("join job worker");
        server.join().expect("join server");

        assert_eq!(body, EXPECTED_PAYLOAD.as_bytes());
        assert_eq!(error.code, "cancelled");
    }

    #[test]
    fn cancelled_native_job_releases_its_reader_without_writing_a_payload() {
        let (_directory, mut store, source_lease) = open_store_with_fixture();
        let prepared = store
            .prepare_kei_job_upload(
                &source_lease,
                1,
                "http://127.0.0.1:9/autobackup/save",
                "account-1",
                "secret-token",
            )
            .expect("prepare job upload");
        let output_directory = prepared.output_directory.clone();
        store
            .release_revision(&source_lease)
            .expect("release renderer handoff lease");
        let registry = JobRegistry::default();
        let job = registry
            .create(JobKind::KeiBackupUpload)
            .expect("create job");
        registry.cancel(&job.id()).expect("cancel job");

        let error = run_job(prepared, job).expect_err("cancelled job must fail");

        assert_eq!(error.code, "cancelled");
        assert_eq!(store.active_readers.active_count(), 0);
        assert!(
            !output_directory.exists()
                || fs::read_dir(output_directory)
                    .expect("read upload directory")
                    .next()
                    .is_none()
        );
    }

    #[test]
    fn startup_removes_only_abandoned_owned_payloads() {
        let (directory, mut store, lease) = open_store_with_fixture();
        let prepared = store
            .prepare_kei_upload(
                &lease,
                "http://127.0.0.1/autobackup/save",
                "account-1",
                "secret-token",
            )
            .expect("prepare upload");
        let output_directory = prepared.output_directory.clone();
        let payload = prepared.create_payload().expect("create payload");
        let payload_path = payload.path.clone();
        let unrelated = output_directory.join("keep-me.txt");
        fs::write(&unrelated, b"keep").expect("write unrelated file");
        std::mem::forget(payload);
        drop(prepared);
        drop(store);

        let reopened = PersistentStore::open(directory.path()).expect("reopen store");

        assert!(!payload_path.exists());
        assert_eq!(fs::read(unrelated).expect("read unrelated file"), b"keep");
        drop(reopened);
    }

    #[test]
    fn accepted_cancellation_maps_phase_and_progress_races_to_cancelled() {
        let phase_registry = JobRegistry::default();
        let phase_job = phase_registry
            .create(JobKind::KeiBackupUpload)
            .expect("create phase-race job");
        phase_job
            .start(JobPhase::WritingExport)
            .expect("start phase-race job");
        phase_registry
            .cancel(&phase_job.id())
            .expect("cancel phase-race job");

        let phase_error = set_job_phase(
            &phase_job,
            JobPhase::PublishingDestination,
            "KEI backup cancelled before upload",
        )
        .expect_err("accepted cancellation must reject the phase transition");

        assert_eq!(phase_error.code, "cancelled");

        let progress_registry = JobRegistry::default();
        let progress_job = progress_registry
            .create(JobKind::KeiBackupUpload)
            .expect("create progress-race job");
        progress_job
            .start(JobPhase::WritingExport)
            .expect("start progress-race job");
        progress_registry
            .cancel(&progress_job.id())
            .expect("cancel progress-race job");

        let progress_error = set_job_progress(
            &progress_job,
            JobProgress {
                completed_bytes: 0,
                total_bytes: Some(1),
                completed_items: 1,
                total_items: Some(2),
            },
            "KEI backup cancelled before upload",
        )
        .expect_err("accepted cancellation must reject the progress transition");

        assert_eq!(progress_error.code, "cancelled");
    }

    #[test]
    fn upload_idle_policy_allows_continuous_progress_beyond_one_idle_window() {
        let registry = JobRegistry::default();
        let job = registry
            .create(JobKind::KeiBackupUpload)
            .expect("create progress job");
        job.start(JobPhase::WritingExport)
            .expect("start progress job");
        job.set_phase(JobPhase::PublishingDestination)
            .expect("advance progress job to upload");
        let progress_job = job.clone();

        let result = tauri::async_runtime::block_on(await_upload_with_job_control(
            async move {
                for step in 1..=8 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    progress_job
                        .set_progress(JobProgress {
                            completed_bytes: step,
                            total_bytes: Some(8),
                            completed_items: 1,
                            total_items: Some(2),
                        })
                        .expect("advance upload progress");
                }
                Ok::<_, ()>(())
            },
            &job,
            Duration::from_millis(15),
        ));

        assert!(result.is_ok());
    }

    #[test]
    fn upload_idle_policy_times_out_only_after_progress_stops() {
        let registry = JobRegistry::default();
        let job = registry
            .create(JobKind::KeiBackupUpload)
            .expect("create idle job");
        job.start(JobPhase::WritingExport).expect("start idle job");
        job.set_phase(JobPhase::PublishingDestination)
            .expect("advance idle job to upload");

        let result = tauri::async_runtime::block_on(await_upload_with_job_control(
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok::<_, ()>(())
            },
            &job,
            Duration::from_millis(10),
        ));

        assert!(matches!(result, Err(ControlledUploadError::IdleTimeout)));
    }

    #[test]
    fn managed_transport_failure_does_not_expose_the_request_url() {
        let (_directory, mut store, source_lease) = open_store_with_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve closed endpoint");
        let address = listener.local_addr().expect("closed endpoint address");
        drop(listener);
        let secret = "do-not-log-this-query-secret";
        let url = format!("http://{address}/autobackup/save?api_key={secret}");
        let prepared = store
            .prepare_kei_job_upload(&source_lease, 1, &url, "account-1", "secret-token")
            .expect("prepare job upload");
        store
            .release_revision(&source_lease)
            .expect("release renderer handoff lease");
        let registry = JobRegistry::default();
        let job = registry
            .create(JobKind::KeiBackupUpload)
            .expect("create job");

        let error = run_job(prepared, job).expect_err("closed endpoint must fail");

        assert_eq!(error.code, "transport-failed");
        assert_eq!(error.message, "KEI backup endpoint is unavailable");
        assert!(!error.message.contains(secret));
        assert!(!error.message.contains(&url));
    }
}
