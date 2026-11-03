use risunest_network_log::reqwest::RequestBuilderExt;
use super::official_assets::{ColdSource, SpoolLimits};
use super::{restore, ImportCounts, IncompleteRestorePreview, JobControl, JobDetail, JobKind, JobPhase, JobProgress, JobResultSummary, JobStage, JobState, NativeJobError, OpenedJobSource, StageUnit};
use crate::persistent_store::commit::StagedPluginPreview;
use crate::persistent_store::upstream_stream::RootSpool;
use crate::persistent_store::{RevisionResult, StagingResult, StoreError, StoreResult};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const DATABASE_KEY: &str = "database/database.bin";
const SNAPSHOT_FILE: &str = "official-account-snapshot.risudat";
const SNAPSHOT_PART_FILE: &str = "official-account-snapshot.risudat.part";
const MAX_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const NETWORK_CANCEL_POLL: Duration = Duration::from_millis(250);
pub(super) const NETWORK_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum OfficialSnapshotCredential {
    RisuAuth { token: String },
    Bearer { token: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OfficialSnapshotRestoreRequest {
    pub(crate) base_url: String,
    pub(crate) credential: OfficialSnapshotCredential,
}

enum DownloadOutcome {
    Missing,
    File(OpenedJobSource),
}

pub(crate) fn restore_official_snapshot(
    request: OfficialSnapshotRestoreRequest,
    expected_revision: i64,
    owned_directory: &Path,
    job: &JobControl,
    sink: &dyn restore::ReplacementSink,
) -> Result<JobResultSummary, NativeJobError> {
    restore_with_spool(request, expected_revision, owned_directory, job, sink, SpoolLimits::default())
}

fn restore_with_spool(
    request: OfficialSnapshotRestoreRequest,
    expected_revision: i64,
    owned_directory: &Path,
    job: &JobControl,
    sink: &dyn restore::ReplacementSink,
    spool: SpoolLimits,
) -> Result<JobResultSummary, NativeJobError> {
    job.start(JobPhase::ReadingSource)
        .map_err(job_state_error)?;
    report_download_progress(job, 0, None)?;
    let downloaded =
        tauri::async_runtime::block_on(download_snapshot(&request, owned_directory, job))?;
    let DownloadOutcome::File(mut source) = downloaded else {
        return Err(NativeJobError::new(
            "remote-missing",
            "No official account snapshot exists",
        ));
    };
    require_native_prepared_format(&mut source)?;
    let downloaded_bytes = source.total_bytes;
    let limit = restore::RestoreLimits::default().max_decoded_block_bytes;
    let sink = OfficialSink {
        inner: sink,
        cold: ColdSource::new(&request.base_url, &request.credential, owned_directory, job, limit, spool)?,
        incomplete: Mutex::default(),
    };
    sink.cold.with_read_ahead(&owned_directory.join(SNAPSHOT_FILE), || {
        restore::restore_started_risu_save(
            source,
            expected_revision,
            job,
            &sink,
            restore::RestoreProgressScale {
                base_bytes: downloaded_bytes,
                total_bytes: Some(downloaded_bytes * 2),
                spool_directory: Some(owned_directory.to_path_buf()),
                cold_payloads: Some(sink.cold.progress()),
                ..Default::default()
            },
        )
    })
    // A cold item read that ended staging reports why, not the staging error it caused.
    .map_err(|error| sink.cold.fatal().unwrap_or(error))
}

/// Stages into the library like any restore, and reads the snapshot's cold
/// references from the account ahead of the records that hold them.
struct OfficialSink<'a> {
    inner: &'a dyn restore::ReplacementSink,
    cold: ColdSource<'a>,
    incomplete: Mutex<IncompleteRestorePreview>,
}

impl restore::ReplacementSink for OfficialSink<'_> {
    fn prepare_activation_receipt(&self, staging_id: &str, job: &JobControl, result: &JobResultSummary) -> StoreResult<Option<String>> {
        self.inner.prepare_activation_receipt(staging_id, job, result)
    }

    fn activation_receipt(&self, staging_id: &str, authority: &str) -> StoreResult<Option<i64>> {
        self.inner.activation_receipt(staging_id, authority)
    }

    fn begin(&self) -> StoreResult<StagingResult> {
        self.inner.begin()
    }

    fn put_root(&self, staging_id: &str, root: &Value) -> StoreResult<()> {
        self.inner.put_root(staging_id, root)
    }

    fn put_presets(&self, staging_id: &str, presets: &[Value]) -> StoreResult<()> {
        self.inner.put_presets(staging_id, presets)
    }

    fn put_legacy_root(&self, staging_id: &str, root: &RootSpool) -> StoreResult<()> {
        self.inner.put_legacy_root(staging_id, root)
    }

    fn add_characters(&self, staging_id: &str, characters: &[Value]) -> StoreResult<()> {
        self.inner.add_characters(staging_id, characters)
    }

    fn prepare_character(&self, character: Value) -> StoreResult<Value> {
        self.inner.prepare_character(character)
    }

    fn supports_incremental_characters(&self) -> bool {
        self.inner.supports_incremental_characters()
    }

    fn expands_cold_payloads(&self) -> bool {
        true
    }

    fn cold_payload_path(&self, key: &str) -> StoreResult<Option<PathBuf>> {
        self.cold.lookup(key).map_err(|error| StoreError::Store { message: error.message })
    }

    fn unavailable_cold_payload_reason(&self, key: &str) -> Option<restore::ColdPayloadUnavailable> {
        self.cold.unavailable(key)
    }

    fn unavailable_cold_payload(&self, key: &str, character_name: &str) -> StoreResult<()> {
        let mut preview = self.incomplete.lock().map_err(|error| StoreError::Store { message: error.to_string() })?;
        preview.unavailable_cold_keys.push(key.to_owned());
        if !character_name.is_empty() { preview.character_names.push(character_name.to_owned()); }
        Ok(())
    }

    fn put_character_detail(&self, staging_id: &str, detail: &Value, conversation_count: i64) -> StoreResult<()> {
        self.inner.put_character_detail(staging_id, detail, conversation_count)
    }

    fn put_conversation_row(&self, staging_id: &str, character_id: &str, configured_index: i64, detail: &Value, recent_at: i64, message_count: i64) -> StoreResult<()> {
        self.inner.put_conversation_row(staging_id, character_id, configured_index, detail, recent_at, message_count)
    }

    fn add_conversation_messages(&self, staging_id: &str, character_id: &str, conversation_id: &str, start: i64, messages: &[Value]) -> StoreResult<()> {
        self.inner.add_conversation_messages(staging_id, character_id, conversation_id, start, messages)
    }

    fn preserve_active_repositories(&self, staging_id: &str, expected_revision: i64) -> StoreResult<()> {
        self.inner.preserve_active_repositories(staging_id, expected_revision)
    }

    fn staged_plugin_preview(&self, staging_id: &str) -> StoreResult<StagedPluginPreview> {
        self.inner.staged_plugin_preview(staging_id)
    }

    fn incomplete_restore_preview(&self) -> StoreResult<IncompleteRestorePreview> {
        let mut preview = self.incomplete.lock().map_err(|error| StoreError::Store { message: error.to_string() })?.clone();
        preview.unavailable_cold_keys.sort();
        preview.unavailable_cold_keys.dedup();
        preview.character_names.sort();
        preview.character_names.dedup();
        Ok(preview)
    }

    fn commit(&self, staging_id: &str, expected_revision: i64) -> StoreResult<RevisionResult> {
        self.inner.commit(staging_id, expected_revision)
    }

    fn abort(&self, staging_id: &str) -> StoreResult<()> {
        self.inner.abort(staging_id)
    }
}

fn require_native_prepared_format(source: &mut OpenedJobSource) -> Result<(), NativeJobError> {
    let mut prefix = [0u8; 10];
    let read = source.file.read(&mut prefix).map_err(store_error)?;
    source.file.seek(SeekFrom::Start(0)).map_err(store_error)?;
    if prefix[..read].starts_with(b"\0RISUSAVE\0") || prefix[..read].starts_with(b"\0\0RISU") {
        return Err(NativeJobError::new(
            "compatibility-required",
            "Legacy official snapshots require the existing prepared compatibility restore",
        ));
    }
    Ok(())
}

async fn download_snapshot(
    request: &OfficialSnapshotRestoreRequest,
    owned_directory: &Path,
    job: &JobControl,
) -> Result<DownloadOutcome, NativeJobError> {
    for retries in 0..=2 {
        let mut retry_after = Duration::from_secs(1 << retries);
        let outcome = download_snapshot_attempt(request, owned_directory, job, NETWORK_IDLE_TIMEOUT, NETWORK_IDLE_TIMEOUT, &mut retry_after).await;
        match outcome {
            Err(error) if retries < 2 && matches!(error.code.as_str(), "network-error" | "http-retryable") => {
                match fs::remove_file(owned_directory.join(SNAPSHOT_PART_FILE)) {
                    Ok(()) => {},
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                    Err(error) => return Err(store_error(error)),
                }
                reset_download_progress(job)?;
                super::transfer_retry::wait_for_retry(job, retry_after).await?;
            }
            outcome => return outcome,
        }
    }
    unreachable!()
}

#[cfg(test)]
async fn download_snapshot_with_idle_timeout(
    request: &OfficialSnapshotRestoreRequest,
    owned_directory: &Path,
    job: &JobControl,
    response_timeout: Duration,
    body_idle_timeout: Duration,
) -> Result<DownloadOutcome, NativeJobError> {
    let mut elapsed = Duration::ZERO;
    download_snapshot_attempt(request, owned_directory, job, response_timeout, body_idle_timeout, &mut elapsed).await
}

async fn download_snapshot_attempt(
    request: &OfficialSnapshotRestoreRequest,
    owned_directory: &Path,
    job: &JobControl,
    response_timeout: Duration,
    body_idle_timeout: Duration,
    retry_after: &mut Duration,
) -> Result<DownloadOutcome, NativeJobError> {
    let endpoint = snapshot_endpoint(&request.base_url)?;
    let client = crate::platform_tls::client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(transport_error)?;
    let response = await_network_operation(
        client
            .get(endpoint)
            .headers(authenticated_headers(&request.credential)?)
            .header("x-risu-key", DATABASE_KEY)
            .header("x-risu-save-date", "0")
            .send_logged("snapshot-download"),
        job,
        response_timeout,
        "Official account snapshot request was cancelled",
    )
    .await?;
    let status = response.status().as_u16();
    if status == 204 {
        return Ok(DownloadOutcome::Missing);
    }
    if status == 303 {
        let bytes = bounded_response_bytes(response, job, body_idle_timeout).await?;
        let value = serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|_| {
            NativeJobError::new(
                "invalid-response",
                "Official account cache response is not valid JSON",
            )
        })?;
        let match_cached = value
            .get("match")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| {
                NativeJobError::new(
                    "invalid-response",
                    "Official account cache response requires a boolean match field",
                )
            })?;
        return if match_cached {
            Err(NativeJobError::new(
                "native-cache-miss",
                "Official account reported an unchanged snapshot without native cached bytes",
            ))
        } else {
            Ok(DownloadOutcome::Missing)
        };
    }
    if status == 403 {
        return Err(NativeJobError::new(
            "reauthentication-needed",
            "Official account authorization failed",
        ));
    }
    if matches!(status, 429 | 502 | 503 | 504) {
        *retry_after = super::transfer_retry::retry_delay(response.headers(), 0);
        return Err(NativeJobError::new("http-retryable", format!("Official account snapshot download returned {status}")));
    }
    if !(200..300).contains(&status) {
        let message =
            String::from_utf8_lossy(&bounded_response_bytes(response, job, body_idle_timeout).await?)
                .into_owned();
        return Err(NativeJobError::new(
            "http-status",
            format!("Official account snapshot download returned {status}: {message}"),
        ));
    }
    let declared_length = response
        .headers()
        .get("x-body-size")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| response.content_length());
    if let Some(length) = declared_length {
        if length > MAX_SNAPSHOT_BYTES {
            return Err(NativeJobError::new(
                "invalid-input",
                "Official account snapshot exceeds the native restore limit",
            ));
        }
    }

    let part = owned_directory.join(SNAPSHOT_PART_FILE);
    let destination = owned_directory.join(SNAPSHOT_FILE);
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .map_err(store_error)?;
    let mut response = response;
    let mut written = 0u64;
    let mut reported = 0u64;
    let mut last_report = std::time::Instant::now();
    loop {
        let chunk = await_network_operation(
            response.chunk(),
            job,
            body_idle_timeout,
            "Official account snapshot download was cancelled",
        )
        .await?;
        let Some(chunk) = chunk else { break };
        written = written.checked_add(chunk.len() as u64).ok_or_else(|| {
            NativeJobError::new("invalid-input", "Official account snapshot length overflow")
        })?;
        if written > MAX_SNAPSHOT_BYTES {
            return Err(NativeJobError::new(
                "invalid-input",
                "Official account snapshot exceeds the native restore limit",
            ));
        }
        if declared_length.is_some_and(|length| written > length) {
            return Err(NativeJobError::new("invalid-response", "Official account snapshot exceeds its response metadata"));
        }
        output.write_all(&chunk).map_err(store_error)?;
        if written - reported >= 256 * 1024 || last_report.elapsed() >= Duration::from_millis(100) {
            report_download_progress(job, written, declared_length)?;
            reported = written;
            last_report = std::time::Instant::now();
        }
    }
    output.flush().map_err(store_error)?;
    output.sync_all().map_err(store_error)?;
    drop(output);
    fs::rename(&part, &destination).map_err(store_error)?;
    if declared_length.is_some_and(|length| length != written) {
        return Err(NativeJobError::new(
            "invalid-response",
            "Official account snapshot length differs from its response metadata",
        ));
    }
    report_download_progress(job, written, Some(written))?;
    let file = File::open(&destination).map_err(store_error)?;
    Ok(DownloadOutcome::File(OpenedJobSource {
        file,
        total_bytes: written,
        custody: None,
    }))
}

fn reset_download_progress(job: &JobControl) -> Result<(), NativeJobError> {
    let mut status = job.status.lock().map_err(|_| job_state_error("native job status mutex poisoned".to_owned()))?;
    if status.kind != JobKind::RestoreOfficialAccountSnapshot || status.state != JobState::Running
        || status.phase != JobPhase::ReadingSource {
        return Err(job_state_error("official snapshot retry requires a running download".to_owned()));
    }
    // A retry replaces the partial response, including its advertised length.
    status.progress = JobProgress::default();
    status.detail = Some(JobDetail::new(JobStage::DownloadingAccountBackup, StageUnit::Bytes, 0, None, ImportCounts::default()));
    Ok(())
}

fn report_download_progress(job: &JobControl, written: u64, length: Option<u64>) -> Result<(), NativeJobError> {
    let status = job.status();
    let stage_completed = status.detail.as_ref().map(|detail| detail.stage_completed).unwrap_or(0).max(written);
    job.set_detail(JobDetail::new(JobStage::DownloadingAccountBackup, StageUnit::Bytes,
        stage_completed, length.map(|length| length.max(stage_completed)), ImportCounts::default())).map_err(job_state_error)?;
    let previous = status.progress;
    job.set_progress(JobProgress {
        completed_bytes: written.max(previous.completed_bytes),
        total_bytes: previous.total_bytes.or(length.map(|length| length * 2)),
        completed_items: 0,
        total_items: None,
    }).map_err(job_state_error)
}

fn snapshot_endpoint(base_url: &str) -> Result<String, NativeJobError> {
    let parsed = url::Url::parse(base_url)
        .map_err(|error| NativeJobError::new("invalid-input", error.to_string()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(NativeJobError::new(
            "invalid-input",
            "Official account base URL must use HTTP or HTTPS",
        ));
    }
    let key = hex::encode(DATABASE_KEY.as_bytes());
    Ok(format!(
        "{}/api/account/read/{key}|{}",
        base_url.trim_end_matches('/'),
        uuid::Uuid::new_v4()
    ))
}

pub(super) fn authenticated_headers(
    credential: &OfficialSnapshotCredential,
) -> Result<HeaderMap, NativeJobError> {
    let mut headers = HeaderMap::new();
    let (name, value) = match credential {
        OfficialSnapshotCredential::RisuAuth { token } => (
            HeaderName::from_static("x-risu-auth"),
            token.as_str().to_owned(),
        ),
        OfficialSnapshotCredential::Bearer { token } => (AUTHORIZATION, format!("Bearer {token}")),
    };
    headers.insert(
        name,
        HeaderValue::from_str(&value).map_err(|_| {
            NativeJobError::new(
                "invalid-input",
                "Official account credential contains invalid header bytes",
            )
        })?,
    );
    Ok(headers)
}

pub(super) async fn bounded_response_bytes(
    mut response: reqwest::Response,
    job: &JobControl,
    idle_timeout: Duration,
) -> Result<Vec<u8>, NativeJobError> {
    let mut bytes = Vec::new();
    loop {
        let chunk = await_network_operation(
            response.chunk(),
            job,
            idle_timeout,
            "Official account snapshot request was cancelled",
        )
        .await?;
        let Some(chunk) = chunk else { break };
        if bytes.len().saturating_add(chunk.len()) > MAX_ERROR_BYTES {
            return Err(NativeJobError::new(
                "invalid-response",
                "Official account response exceeds 64 KiB",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(super) async fn await_network_operation<F, T>(
    operation: F,
    job: &JobControl,
    idle_timeout: Duration,
    cancelled_message: &'static str,
) -> Result<T, NativeJobError>
where
    F: std::future::Future<Output = Result<T, reqwest::Error>>,
{
    let mut operation = Box::pin(operation);
    let deadline = tokio::time::Instant::now() + idle_timeout;
    loop {
        if job.is_cancel_requested() {
            return Err(NativeJobError::new("cancelled", cancelled_message));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(network_idle_timeout());
        }
        let wait = std::cmp::min(NETWORK_CANCEL_POLL, remaining);
        if let Ok(result) = tokio::time::timeout(wait, operation.as_mut()).await {
            if job.is_cancel_requested() {
                return Err(NativeJobError::new("cancelled", cancelled_message));
            }
            return result.map_err(transport_error);
        }
    }
}

fn network_idle_timeout() -> NativeJobError {
    NativeJobError::new(
        "network-error",
        "Official account snapshot request timed out while waiting for network progress",
    )
}

fn transport_error(error: reqwest::Error) -> NativeJobError {
    NativeJobError::new("network-error", error.to_string())
}

// Local override: snapshot spool IO failures keep the "store-error" code the
// snapshot job protocol already reports, unlike the shared "io-error".
fn store_error(error: std::io::Error) -> NativeJobError {
    NativeJobError::new("store-error", error.to_string())
}

use super::error::job_error as job_state_error;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_file_jobs::{JobKind, JobRegistry};
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Condvar};
    use std::thread;
    use std::time::Instant;
    use tempfile::TempDir;

    fn server(response: Vec<u8>) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            stream.write_all(&response).unwrap();
            stream.flush().unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
            request
        });
        (format!("http://{address}"), handle)
    }

    fn stalled_server(
        response_prefix: Vec<u8>,
        header_delay: Duration,
    ) -> (
        String,
        mpsc::Receiver<Instant>,
        mpsc::Sender<()>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = match stream.read(&mut buffer) {
                    Ok(read) => read,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) => return,
                    Err(error) => panic!("fixture request read failed: {error}"),
                };
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            thread::sleep(header_delay);
            if let Err(error) = stream.write_all(&response_prefix).and_then(|_| stream.flush()) {
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                ) {
                    return;
                }
                panic!("fixture response write failed: {error}");
            }
            let _ = ready_tx.send(Instant::now());
            let _ = stop_rx.recv_timeout(Duration::from_secs(30));
        });
        (format!("http://{address}"), ready_rx, stop_tx, handle)
    }

    fn scripted_server(steps: Vec<(Duration, Vec<u8>)>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            for (delay, bytes) in steps {
                thread::sleep(delay);
                stream.write_all(&bytes).unwrap();
                stream.flush().unwrap();
            }
            stream.shutdown(Shutdown::Write).unwrap();
            while stream.read(&mut buffer).unwrap() != 0 {}
        });
        (format!("http://{address}"), handle)
    }

    fn request(base_url: String) -> OfficialSnapshotRestoreRequest {
        OfficialSnapshotRestoreRequest {
            base_url,
            credential: OfficialSnapshotCredential::RisuAuth {
                token: "test-token".to_owned(),
            },
        }
    }

    fn job() -> std::sync::Arc<JobControl> {
        let registry = JobRegistry::default();
        let job = registry
            .create_with_context(JobKind::RestoreOfficialAccountSnapshot, Some(1), Vec::new())
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        job
    }

    #[test]
    fn cancellation_interrupts_a_response_stalled_before_headers() {
        let (base_url, request_received, stop_server, server) =
            stalled_server(Vec::new(), Duration::ZERO);
        let directory = TempDir::new().unwrap();
        let job = job();
        let cancelling_job = std::sync::Arc::clone(&job);
        let canceller = thread::spawn(move || {
            request_received
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            cancelling_job.request_cancel().unwrap();
        });

        let started = Instant::now();
        let result = tauri::async_runtime::block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                download_snapshot(&request(base_url), directory.path(), &job),
            )
            .await
        });

        let _ = stop_server.send(());
        server.join().unwrap();
        canceller.join().unwrap();
        let error = match result.expect("header-stalled request must stop after cancellation") {
            Err(error) => error,
            Ok(_) => panic!("header-stalled request must be cancelled"),
        };
        assert_eq!(error.code, "cancelled");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cancellation_interrupts_a_response_stalled_between_body_chunks() {
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\npartial".to_vec();
        let (base_url, body_started, stop_server, server) = stalled_server(response, Duration::ZERO);
        let directory = TempDir::new().unwrap();
        let job = job();
        let cancelling_job = std::sync::Arc::clone(&job);
        let canceller = thread::spawn(move || {
            body_started.recv_timeout(Duration::from_secs(1)).unwrap();
            cancelling_job.request_cancel().unwrap();
        });

        let result = tauri::async_runtime::block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                download_snapshot(&request(base_url), directory.path(), &job),
            )
            .await
        });

        let _ = stop_server.send(());
        server.join().unwrap();
        canceller.join().unwrap();
        let error = match result.expect("body-stalled request must stop after cancellation") {
            Err(error) => error,
            Ok(_) => panic!("body-stalled request must be cancelled"),
        };
        assert_eq!(error.code, "cancelled");
    }

    #[test]
    fn idle_timeout_ends_a_stalled_snapshot_body() {
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\npartial".to_vec();
        let (base_url, body_started, stop_server, server) =
            stalled_server(response, Duration::from_millis(300));
        let directory = TempDir::new().unwrap();
        let job = job();

        let result = tauri::async_runtime::block_on(async {
            tokio::time::timeout(
                Duration::from_secs(15),
                download_snapshot_with_idle_timeout(
                    &request(base_url),
                    directory.path(),
                    &job,
                    Duration::from_secs(10),
                    Duration::from_millis(100),
                ),
            )
            .await
        });

        let completed = Instant::now();
        let body_ready = body_started.recv_timeout(Duration::from_secs(10));
        let _ = stop_server.send(());
        server.join().unwrap();
        assert!(
            body_ready.expect("fixture must send headers and partial body") <= completed,
            "body startup must precede the idle failure"
        );
        let error = match result.expect("stalled body must respect its idle timeout") {
            Err(error) => error,
            Ok(_) => panic!("stalled body must fail"),
        };
        assert_eq!(error.code, "network-error");
        assert_eq!(error.message, network_idle_timeout().message);
        assert_eq!(
            fs::read(directory.path().join(SNAPSHOT_PART_FILE)).unwrap(),
            b"partial"
        );
    }

    #[test]
    fn idle_timeout_ends_a_stalled_error_response_body() {
        let response = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 16\r\nConnection: close\r\n\r\npartial"
            .to_vec();
        let (base_url, body_started, stop_server, server) =
            stalled_server(response, Duration::from_millis(300));
        let directory = TempDir::new().unwrap();
        let job = job();

        let result = tauri::async_runtime::block_on(async {
            tokio::time::timeout(
                Duration::from_secs(15),
                download_snapshot_with_idle_timeout(
                    &request(base_url),
                    directory.path(),
                    &job,
                    Duration::from_secs(10),
                    Duration::from_millis(100),
                ),
            )
            .await
        });

        let completed = Instant::now();
        let body_ready = body_started.recv_timeout(Duration::from_secs(10));
        let _ = stop_server.send(());
        server.join().unwrap();
        assert!(
            body_ready.expect("fixture must send headers and partial body") <= completed,
            "body startup must precede the idle failure"
        );
        let error = match result.expect("stalled error body must respect its idle timeout") {
            Err(error) => error,
            Ok(_) => panic!("stalled error body must fail"),
        };
        assert_eq!(error.code, "network-error");
        assert_eq!(error.message, network_idle_timeout().message);
    }

    #[test]
    fn body_progress_resets_the_idle_timeout() {
        let headers = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\n".to_vec();
        let steps = vec![
            (Duration::ZERO, headers),
            (Duration::from_millis(300), b"a".to_vec()),
            (Duration::from_millis(300), b"b".to_vec()),
            (Duration::from_millis(300), b"c".to_vec()),
        ];
        let (base_url, server) = scripted_server(steps);
        let directory = TempDir::new().unwrap();
        let job = job();

        let outcome = tauri::async_runtime::block_on(async {
            tokio::time::timeout(
                Duration::from_secs(3),
                download_snapshot_with_idle_timeout(
                    &request(base_url),
                    directory.path(),
                    &job,
                    Duration::from_secs(10),
                    Duration::from_millis(500),
                ),
            )
            .await
        })
        .expect("progressing download must finish")
        .unwrap();

        let DownloadOutcome::File(mut source) = outcome else {
            panic!("snapshot must download");
        };
        let mut bytes = Vec::new();
        source.file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"abc");
        server.join().unwrap();
    }

    #[test]
    fn chunked_download_reports_known_or_unknown_total_before_staging() {
        // Reach the byte reporting threshold without depending on elapsed time.
        const FIRST_CHUNK: usize = 256 * 1024;
        const LENGTH: usize = FIRST_CHUNK + 3;
        for declared in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let (release, released) = mpsc::channel();
            let server = thread::spawn(move || {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if matches!(released.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
                                return;
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let metadata = if declared { format!("Content-Length: {LENGTH}\r\n") } else { String::new() };
                write!(stream, "HTTP/1.1 200 OK\r\n{metadata}Connection: close\r\n\r\n").unwrap();
                stream.write_all(&vec![b'a'; FIRST_CHUNK]).unwrap();
                stream.flush().unwrap();
                if released.recv_timeout(Duration::from_secs(30)).is_ok() {
                    stream.write_all(b"def").unwrap();
                    stream.flush().unwrap();
                    stream.shutdown(Shutdown::Write).unwrap();
                }
            });
            let directory = TempDir::new().unwrap();
            let job = job();
            let result = tauri::async_runtime::block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    let request = request(base_url);
                    let download = download_snapshot(&request, directory.path(), &job);
                    let observe = async {
                        let mut observations = Vec::new();
                        loop {
                            let progress = job.status().progress;
                            let ready = progress.completed_bytes > 0;
                            observations.push(progress);
                            if ready {
                                release.send(()).unwrap();
                                return observations;
                            }
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                    };
                    tokio::join!(download, observe)
                }).await
            });
            drop(release);
            server.join().unwrap();
            let (outcome, mut observations) = result.expect("download did not report its held first chunk");
            assert!(matches!(outcome.unwrap(), DownloadOutcome::File(_)));
            let before_release = observations.last().unwrap();
            assert!(before_release.completed_bytes > 0 && before_release.completed_bytes <= FIRST_CHUNK as u64);
            assert_eq!(observations.last().unwrap().total_bytes, if declared { Some((LENGTH * 2) as u64) } else { None });
            observations.push(job.status().progress);
            assert_eq!(job.status().progress.completed_bytes, LENGTH as u64);
            assert_eq!(job.status().progress.total_bytes, Some((LENGTH * 2) as u64));
            assert!(observations.windows(2).all(|pair| pair[0].completed_bytes <= pair[1].completed_bytes));
            assert!(observations.iter().all(|progress| progress.total_bytes.is_none_or(|total| progress.completed_bytes <= total)));
            let bytes = fs::read(directory.path().join(SNAPSHOT_FILE)).unwrap();
            assert_eq!(&bytes[..FIRST_CHUNK], vec![b'a'; FIRST_CHUNK]);
            assert_eq!(&bytes[FIRST_CHUNK..], b"def");
            assert!(!directory.path().join(SNAPSHOT_PART_FILE).exists());
        }
    }

    #[test]
    fn snapshot_transient_status_retries_are_bounded_and_keep_exact_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for status in [429, 503, 200] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                stream.read(&mut request).unwrap();
                let body = if status == 200 { "abc" } else { "" };
                write!(stream, "HTTP/1.1 {status} Response\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let directory = TempDir::new().unwrap();
        let job = job();
        let outcome = tauri::async_runtime::block_on(download_snapshot(&request(format!("http://{address}")), directory.path(), &job)).unwrap();
        let DownloadOutcome::File(mut source) = outcome else { panic!("snapshot required") };
        let mut bytes = Vec::new();
        source.file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"abc");
        assert_eq!(job.status().progress.total_bytes, Some(6));
        server.join().unwrap();

        for length in [Some(3), Some(768 * 1024), None] {
            let mut partial = b"HTTP/1.1 200 OK\r\nContent-Length: 524288\r\nConnection: close\r\n\r\n".to_vec();
            partial.extend(vec![b'a'; 256 * 1024]);
            let expected = vec![b'b'; length.unwrap_or(3)];
            let metadata = length.map(|n| format!("Content-Length: {n}\r\n")).unwrap_or_default();
            let mut complete = format!("HTTP/1.1 200 OK\r\n{metadata}Connection: close\r\n\r\n").into_bytes();
            complete.extend(&expected);
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = thread::spawn(move || {
                for response in [partial, complete] {
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => thread::sleep(Duration::from_millis(1)),
                            Err(error) => panic!("fixture accept failed: {error}"),
                        }
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                    stream.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
                    let mut request = Vec::new();
                    let mut byte = [0];
                    while !request.ends_with(b"\r\n\r\n") { stream.read_exact(&mut byte).unwrap(); request.push(byte[0]); }
                    stream.write_all(&response).unwrap();
                    stream.shutdown(Shutdown::Write).unwrap();
                }
            });
            let directory = TempDir::new().unwrap();
            let job = self::job();
            let outcome = tauri::async_runtime::block_on(download_snapshot(&request(url), directory.path(), &job)).unwrap();
            let DownloadOutcome::File(mut source) = outcome else { panic!("snapshot required") };
            let mut bytes = Vec::new();
            source.file.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, expected);
            let detail = job.status().detail.unwrap();
            assert_eq!(detail.stage_completed, expected.len() as u64);
            assert_eq!(detail.stage_total, Some(expected.len() as u64));
            server.join().unwrap();
        }
    }

    #[test]
    fn streams_the_official_snapshot_to_a_job_owned_file_with_exact_headers() {
        let body = b"RISUSAVE\0bounded-body";
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        let (base_url, server) = server(response);
        let directory = TempDir::new().unwrap();
        let job = job();

        let outcome = tauri::async_runtime::block_on(download_snapshot(
            &request(base_url),
            directory.path(),
            &job,
        ))
        .unwrap();

        let DownloadOutcome::File(mut source) = outcome else {
            panic!("snapshot must download");
        };
        let mut bytes = Vec::new();
        source.file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, body);
        assert_eq!(source.total_bytes, body.len() as u64);
        let request = String::from_utf8(server.join().unwrap()).unwrap();
        assert!(request
            .starts_with("GET /api/account/read/64617461626173652f64617461626173652e62696e|"));
        assert!(request
            .to_ascii_lowercase()
            .contains("x-risu-key: database/database.bin"));
        assert!(request.to_ascii_lowercase().contains("x-risu-save-date: 0"));
        assert!(request
            .to_ascii_lowercase()
            .contains("x-risu-auth: test-token"));
    }

    #[test]
    fn reports_missing_without_creating_a_snapshot_file() {
        let (base_url, server) = server(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let directory = TempDir::new().unwrap();
        let job = job();

        let outcome = tauri::async_runtime::block_on(download_snapshot(
            &request(base_url),
            directory.path(),
            &job,
        ))
        .unwrap();

        assert!(matches!(outcome, DownloadOutcome::Missing));
        assert!(!directory.path().join(SNAPSHOT_FILE).exists());
        server.join().unwrap();
    }

    #[test]
    fn classifies_unchanged_snapshot_responses_strictly() {
        let cases = [
            (br#"{"match":false}"#.as_slice(), None),
            (br#"{"match":true}"#.as_slice(), Some("native-cache-miss")),
            (br#"{"other":false}"#.as_slice(), Some("invalid-response")),
            (br#"{"match":"false"}"#.as_slice(), Some("invalid-response")),
            (b"not-json".as_slice(), Some("invalid-response")),
        ];
        for (body, expected_error) in cases {
            let mut response = format!(
                "HTTP/1.1 303 See Other\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len(),
            )
            .into_bytes();
            response.extend_from_slice(body);
            let (base_url, server) = server(response);
            let directory = TempDir::new().unwrap();
            let job = job();

            let result = tauri::async_runtime::block_on(download_snapshot(
                &request(base_url),
                directory.path(),
                &job,
            ));

            match expected_error {
                None => assert!(matches!(result, Ok(DownloadOutcome::Missing))),
                Some(code) => match result {
                    Err(error) => assert_eq!(error.code, code),
                    Ok(_) => panic!("303 payload must return {code}"),
                },
            }
            server.join().unwrap();
        }
    }

    #[test]
    fn rejects_an_oversized_declared_snapshot_before_writing() {
        let (base_url, server) = server(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_SNAPSHOT_BYTES + 1,
            )
            .into_bytes(),
        );
        let directory = TempDir::new().unwrap();
        let job = job();

        let result = tauri::async_runtime::block_on(download_snapshot(
            &request(base_url),
            directory.path(),
            &job,
        ));
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("oversized snapshot must be rejected"),
        };

        assert_eq!(error.code, "invalid-input");
        assert!(!directory.path().join(SNAPSHOT_FILE).exists());
        server.join().unwrap();
    }

    #[test]
    fn returns_retryable_reauthentication_without_touching_the_live_store() {
        let (base_url, server) = server(
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let directory = TempDir::new().unwrap();
        let job = job();

        let result = tauri::async_runtime::block_on(download_snapshot(
            &request(base_url),
            directory.path(),
            &job,
        ));
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("authorization failure must not download a snapshot"),
        };

        assert_eq!(error.code, "reauthentication-needed");
        assert!(!directory.path().join(SNAPSHOT_FILE).exists());
        server.join().unwrap();
    }

    #[test]
    fn warning_authorization_response_still_requests_reauthentication() {
        let (base_url, server) = server(
            b"HTTP/1.1 403 Forbidden\r\nx-risu-status: warn\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
        );
        let directory = TempDir::new().unwrap();
        let job = job();

        let result = tauri::async_runtime::block_on(download_snapshot(
            &request(base_url),
            directory.path(),
            &job,
        ));
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("authorization warning must request reauthentication"),
        };

        assert_eq!(error.code, "reauthentication-needed");
        assert!(!directory.path().join(SNAPSHOT_FILE).exists());
        server.join().unwrap();
    }

    /// Answers each request on a thread of its own, by path and `x-risu-key`,
    /// and keeps every request head in the order the requests arrived.
    fn account_server(respond: impl Fn(&str, &str, &mut TcpStream) + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (seen, respond) = (requests.clone(), Arc::new(respond));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (seen, respond) = (seen.clone(), respond.clone());
                thread::spawn(move || {
                    let mut request = Vec::new();
                    let mut byte = [0];
                    while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                        request.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&request).into_owned();
                    let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
                    let key = head.lines()
                        .find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("x-risu-key")))
                        .map_or("", |(_, value)| value.trim()).to_owned();
                    seen.lock().unwrap().push(head);
                    respond(&path, &key, &mut stream);
                    let _ = stream.shutdown(Shutdown::Write);
                });
            }
        });
        (format!("http://{address}"), requests)
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes = format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    /// The account compresses cold items with gzip, zlib or raw deflate.
    fn cold_item(value: &Value, format: &str) -> Vec<u8> {
        use flate2::{write::{DeflateEncoder, GzEncoder, ZlibEncoder}, Compression};
        let json = serde_json::to_vec(value).unwrap();
        match format {
            "gzip" => { let mut encoder = GzEncoder::new(Vec::new(), Compression::default()); encoder.write_all(&json).unwrap(); encoder.finish().unwrap() }
            "zlib" => { let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default()); encoder.write_all(&json).unwrap(); encoder.finish().unwrap() }
            _ => { let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default()); encoder.write_all(&json).unwrap(); encoder.finish().unwrap() }
        }
    }

    fn cold_chat(id: &str, key: &str) -> Value {
        serde_json::json!({"id": id, "name": id, "message": [{"role": "char", "data": format!("\u{ef01}COLDSTORAGE\u{ef01}{key}"), "chatId": format!("{id}-message")}]})
    }

    /// A snapshot whose plugin values are the cold items `cold-0`, `cold-1` and on.
    fn cold_plugin_values(count: usize) -> Vec<Vec<u8>> {
        let storage = (0..count).map(|index| (format!("value-{index}"), Value::String(format!("cold-{index}")))).collect::<serde_json::Map<_, _>>();
        let mut blocks = restore::tests::valid_blocks();
        blocks[5] = restore::tests::block(11, false, "pluginStorage", &serde_json::json!({"_coldplugin": storage}));
        blocks
    }

    fn plugin_value(store: &crate::persistent_store::PersistentStore, key: &str) -> Option<Value> {
        store.read_plugin_storage(crate::persistent_store::plugin_owner::UNOWNED_OWNER, key, None).unwrap().map(|row| row.value)
    }

    /// Events the account server saw, which a reply can wait on.
    type Events = Arc<(Mutex<Vec<String>>, Condvar)>;

    fn note(events: &Events, event: String) {
        events.0.lock().unwrap().push(event);
        events.1.notify_all();
    }

    /// Waits up to `timeout` for `event`, and returns whether it happened.
    fn wait_for(events: &Events, event: &str, timeout: Duration) -> bool {
        let (events, changed) = &**events;
        let (events, _) = changed.wait_timeout_while(events.lock().unwrap(), timeout, |events| !events.iter().any(|seen| seen == event)).unwrap();
        events.iter().any(|seen| seen == event)
    }

    struct AccountRestore {
        _directory: TempDir,
        owned: TempDir,
        sink: restore::tests::StoreSink,
        job: Arc<JobControl>,
        outcome: Result<JobResultSummary, NativeJobError>,
        cold_reads: Vec<String>,
        /// Each conversation row staging put, by name, with the job's detail then.
        rows: Vec<(String, Option<JobDetail>)>,
    }

    impl AccountRestore {
        /// How many times the restore read `key`.
        fn reads(&self, key: &str) -> usize {
            self.cold_reads.iter().filter(|head| head.contains(&format!("x-risu-key: {key}\r\n"))).count()
        }
    }

    /// A restore job and the directory it owns.
    struct Account {
        job: Arc<JobControl>,
        owned: TempDir,
    }

    impl Account {
        fn new() -> Self {
            Self { job: JobRegistry::default().create(JobKind::RestoreOfficialAccountSnapshot).unwrap(), owned: TempDir::new().unwrap() }
        }

        /// Restores `blocks` as the account snapshot under `spool` while `cold`
        /// writes the reply to each cold item read, by its key.
        fn restore(self, blocks: Vec<Vec<u8>>, spool: SpoolLimits, cold: impl Fn(&str, &mut TcpStream) + Send + Sync + 'static) -> AccountRestore {
            let snapshot = restore::tests::save_bytes(blocks);
            let (base_url, requests) = account_server(move |path, key, stream| {
                if path == "/hub/account/coldstorage" {
                    cold(key, stream);
                } else {
                    let _ = stream.write_all(&response("200 OK", "", &snapshot));
                }
            });
            let (directory, mut sink) = restore::tests::fixture();
            let rows = Arc::new(Mutex::new(Vec::new()));
            sink.on_conversation_row = Some(Box::new({
                let (job, rows) = (self.job.clone(), rows.clone());
                move |detail: &Value| rows.lock().unwrap().push((detail["name"].as_str().unwrap_or_default().to_owned(), job.status().detail))
            }));
            let outcome = restore_with_spool(request(base_url), 1, self.owned.path(), &self.job, &sink, spool);
            let cold_reads = requests.lock().unwrap().iter().filter(|head| head.contains(" /hub/account/coldstorage ")).cloned().collect();
            let rows = rows.lock().unwrap().clone();
            AccountRestore { _directory: directory, owned: self.owned, sink, job: self.job, outcome, cold_reads, rows }
        }
    }

    /// Restores `blocks` as the account snapshot while `cold` answers each cold
    /// item read by its key.
    fn restore_from_account(blocks: Vec<Vec<u8>>, cold: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> AccountRestore {
        Account::new().restore(blocks, SpoolLimits::default(), move |key, stream| {
            let _ = stream.write_all(&cold(key));
        })
    }

    #[test]
    fn snapshot_restore_reads_cold_items_from_the_account() {
        let mut blocks = restore::tests::valid_blocks();
        blocks[5] = restore::tests::block(11, false, "pluginStorage", &serde_json::json!({
            "_coldplugin": {"nullValue": "cold-null", "gone": "cold-gone", "flaky": "cold-flaky"}
        }));
        blocks[6] = restore::tests::block(2, true, "char-1", &serde_json::json!({
            "chaId": "char-1", "name": "Stub", "chats": [], "coldstorage": "cold-character"
        }));
        let messages = serde_json::json!([{"role": "char", "data": "restored", "chatId": "message-1"}]);
        let character = serde_json::json!({"character": {"chaId": "char-1", "name": "Expanded", "chats": [
            {"id": "chat-1", "name": "Present", "message": [{"role": "char", "data": "\u{ef01}COLDSTORAGE\u{ef01}cold-chat"}]},
            {"id": "chat-2", "name": "Missing", "message": [{"role": "char", "data": "\u{ef01}COLDSTORAGE\u{ef01}cold-missing"}]}
        ]}});
        let chat = serde_json::json!({"message": messages});
        let block_count = blocks.len() as u64;
        // The database bytes read once the character block is read.
        let database_bytes = restore::tests::save_bytes(blocks[..7].to_vec()).len() as u64;
        let character = cold_item(&character, "gzip");
        let chat = cold_item(&chat, "zlib");
        let item_bytes = (character.len() + chat.len()) as u64;
        let restore = restore_from_account(blocks, move |key| match key {
            "cold-character" => response("200 OK", "", &character),
            "cold-chat" => response("200 OK", "", &chat),
            "cold-null" => response("200 OK", "", &cold_item(&Value::Null, "deflate")),
            "cold-flaky" => response("503 Service Unavailable", "Retry-After: 0\r\n", b""),
            _ => response("404 Not Found", "", b""),
        });

        let result = restore.outcome.as_ref().unwrap();
        assert_eq!(result.warning_codes, ["unavailable-cold-payloads:1", "missing-backup-data:1", "damaged-plugin-storage:1"]);
        // Each item read counts with the blocks: the character and chat while the
        // blocks are read, and the plugin value once `staging-database` has begun.
        assert_eq!(restore.job.status().progress.completed_items, block_count + 3);
        // An item counts when staging takes it, however early it was read: once
        // the chat is staged, the database read has moved by the character and
        // chat bytes alone, its total unknown from then on.
        let detail = restore.rows.iter().find(|(name, _)| name == "Present").and_then(|(_, detail)| detail.clone()).unwrap();
        assert_eq!((detail.stage, detail.stage_completed, detail.stage_total), (JobStage::ReadingDatabase, database_bytes + item_bytes, None));
        let preview = restore.job.status().incomplete_restore_preview.unwrap();
        assert_eq!(preview.unavailable_cold_keys, ["cold-missing"]);
        assert_eq!(preview.character_names, ["Expanded"]);
        let store = restore.sink.store.lock().unwrap();
        let restored = store.materialize(None).unwrap();
        assert_eq!(restored["characters"][0]["name"], "Expanded");
        assert_eq!(restored["characters"][0]["chats"][0]["message"], messages);
        assert_eq!(restored["characters"][0]["chats"][1]["message"], serde_json::json!([]));
        // An item that holds JSON null is a stored null, not a missing item.
        assert_eq!(plugin_value(&store, "nullValue"), Some(Value::Null));
        assert_eq!(plugin_value(&store, "gone"), None);
        assert_eq!(plugin_value(&store, "flaky"), None);
        assert!(restore.cold_reads.iter().all(|head| head.contains("x-risu-auth: test-token\r\n")));
        // Each item is read once, and a failing one only within its retry budget.
        for key in ["cold-character", "cold-chat", "cold-missing", "cold-null", "cold-gone"] {
            assert_eq!(restore.reads(key), 1, "{key}");
        }
        assert_eq!(restore.reads("cold-flaky"), 3);
    }

    #[test]
    fn snapshot_restore_stops_before_activation_when_the_account_stops_cold_reads() {
        for (status, code) in [
            ("401 Unauthorized", "reauthentication-needed"),
            ("403 Forbidden", "reauthentication-needed"),
            ("503 Service Unavailable", "network-error"),
        ] {
            let restore = restore_from_account(cold_plugin_values(6), move |_| response(status, "Retry-After: 0\r\n", b""));

            let error = restore.outcome.as_ref().unwrap_err();
            assert_eq!(error.code, code, "{status}");
            let reads = (0..6).map(|index| restore.reads(&format!("cold-{index}"))).collect::<Vec<_>>();
            if status.starts_with('5') {
                // Five items in a row that exhaust their retries mean the service
                // is down, and nothing more is read.
                assert_eq!(reads, [3, 3, 3, 3, 3, 0]);
            } else {
                // A rejected credential ends the reads both slots hold, and the
                // restore starts over once the renderer signs in again.
                let total = reads.iter().sum::<usize>();
                assert!((1..=2).contains(&total) && reads.iter().all(|&count| count <= 1), "{status}: {reads:?}");
            }
            assert_eq!(restore.sink.abort_calls.load(Ordering::Acquire), 1);
            assert_eq!(restore.sink.store.lock().unwrap().revision().unwrap(), 1);
        }
    }

    #[test]
    fn snapshot_restore_reads_cold_items_ahead_of_staging_in_its_order() {
        let mut blocks = restore::tests::valid_blocks();
        blocks[5] = restore::tests::block(11, false, "pluginStorage", &serde_json::json!({"_coldplugin": {"value": "cold-p"}}));
        blocks[6] = restore::tests::block(2, true, "char-1", &serde_json::json!({"chaId": "char-1", "name": "Stub", "chats": [], "coldstorage": "cold-c"}));
        blocks.push(restore::tests::block(2, false, "char-2", &serde_json::json!({"chaId": "char-2", "name": "Second", "chats": [cold_chat("chat-s", "cold-s")]})));
        let events = Events::default();
        let late = Arc::new(Mutex::new(Vec::<String>::new()));
        let restore = Account::new().restore(blocks, SpoolLimits::default(), {
            let (events, late) = (events.clone(), late.clone());
            move |key, stream| {
                note(&events, key.to_owned());
                // Staging asks for the second chat only once it has the first, so
                // the first waits for a read of the second that only reading ahead
                // starts. The other character's chat holds the second slot until then.
                let after = match key {
                    "cold-c1" => Some("cold-c2"),
                    "cold-s" => Some("cold-c1"),
                    _ => None,
                };
                if after.is_some_and(|after| !wait_for(&events, after, Duration::from_secs(5))) {
                    late.lock().unwrap().push(key.to_owned());
                }
                let item = match key {
                    "cold-c" => serde_json::json!({"character": {"chaId": "char-1", "name": "Expanded", "chats": [cold_chat("chat-1", "cold-c1"), cold_chat("chat-2", "cold-c2")]}}),
                    "cold-p" => serde_json::json!("plugin value"),
                    _ => serde_json::json!({"message": [{"role": "char", "data": key, "chatId": format!("{key}-message")}]}),
                };
                let _ = stream.write_all(&response("200 OK", "", &cold_item(&item, "gzip")));
            }
        });

        restore.outcome.as_ref().unwrap();
        assert_eq!(*late.lock().unwrap(), Vec::<String>::new());
        // The character's chats, named only inside its item, are read before the
        // plugin value the snapshot named long before them.
        let order = events.0.lock().unwrap().clone();
        let at = |key: &str| order.iter().position(|seen| seen == key).unwrap();
        assert!(at("cold-c1") < at("cold-c2") && at("cold-c2") < at("cold-p") && at("cold-s") < at("cold-p"), "{order:?}");
        let store = restore.sink.store.lock().unwrap();
        let restored = store.materialize(None).unwrap();
        assert_eq!(restored["characters"][0]["chats"][1]["message"][0]["data"], "cold-c2");
        assert_eq!(restored["characters"][1]["chats"][0]["message"][0]["data"], "cold-s");
        assert_eq!(plugin_value(&store, "value"), Some(serde_json::json!("plugin value")));
        for key in ["cold-c", "cold-c1", "cold-c2", "cold-s", "cold-p"] {
            assert_eq!(restore.reads(key), 1, "{key}");
        }
    }

    #[test]
    fn snapshot_restore_reads_ahead_only_as_far_as_the_spool_allows() {
        // Two items, or the bytes of one.
        for spool in [SpoolLimits { items: 2, bytes: u64::MAX }, SpoolLimits { items: 64, bytes: 1 }] {
            let events = Events::default();
            let restore = Account::new().restore(cold_plugin_values(4), spool, {
                let events = events.clone();
                move |key, stream| {
                    note(&events, format!("read {key}"));
                    if key == "cold-0" {
                        // With the second item spooled, a third read waits until
                        // staging takes the first.
                        wait_for(&events, "answered cold-1", Duration::from_secs(5));
                        wait_for(&events, "read cold-2", Duration::from_millis(500));
                        note(&events, "release cold-0".to_owned());
                    }
                    let _ = stream.write_all(&response("200 OK", "", &cold_item(&Value::String(key.to_owned()), "gzip")));
                    note(&events, format!("answered {key}"));
                }
            });

            restore.outcome.as_ref().unwrap();
            let events = events.0.lock().unwrap().clone();
            let at = |event: &str| events.iter().position(|seen| seen == event).unwrap();
            assert!(at("release cold-0") < at("read cold-2"), "{spool:?}: {events:?}");
            let store = restore.sink.store.lock().unwrap();
            for index in 0..4 {
                assert_eq!(plugin_value(&store, &format!("value-{index}")), Some(Value::String(format!("cold-{index}"))));
            }
        }
    }

    #[test]
    fn snapshot_restore_reading_ahead_stages_what_reading_on_demand_stages() {
        let mut blocks = restore::tests::valid_blocks();
        // The expanded character replaces the stub's chats, whose item is never read.
        blocks[6] = restore::tests::block(2, true, "char-1", &serde_json::json!({
            "chaId": "char-1", "name": "Stub", "chats": [cold_chat("stub", "cold-stub")], "coldstorage": "cold-c1"
        }));
        // A chat item named twice is read twice, as staging asks for it twice.
        blocks.push(restore::tests::block(2, false, "char-2", &serde_json::json!({
            "chaId": "char-2", "name": "Second",
            "chats": [cold_chat("c2-a", "cold-s1"), cold_chat("c2-b", "cold-c1-a"), {"id": "c2-c", "name": "c2-c", "message": []}]
        })));
        // A character whose item is missing keeps its own chats.
        blocks.push(restore::tests::block(2, true, "char-3", &serde_json::json!({
            "chaId": "char-3", "name": "Third", "chats": [cold_chat("c3-a", "cold-s2")], "coldstorage": "cold-c3"
        })));
        // A group is not restored, so its item is never read.
        blocks.push(restore::tests::block(2, false, "group", &serde_json::json!({
            "chaId": "group", "name": "Group", "type": "group", "chats": [], "coldstorage": "cold-group"
        })));
        fn reply(key: &str) -> Vec<u8> {
            let item = match key {
                "cold-c1" => serde_json::json!({"character": {"chaId": "char-1", "name": "Expanded", "chats": [cold_chat("c1-a", "cold-c1-a"), cold_chat("c1-b", "cold-c1-b")]}}),
                "cold-index" => serde_json::json!({"type": "risu-plugin-storage-index", "version": 1, "trash": [], "entries": [
                    {"k": "a", "l": "cold-large"}, {"k": "s", "s": "\"small\""}
                ]}),
                "cold-pa" => serde_json::json!("plugin value"),
                "cold-large" => serde_json::json!("large value"),
                "cold-null" => Value::Null,
                "cold-c1-a" | "cold-s2" => serde_json::json!({"message": [{"role": "char", "data": key, "chatId": format!("{key}-message")}]}),
                "cold-s1" | "cold-flaky" => return response("503 Service Unavailable", "Retry-After: 0\r\n", b""),
                _ => return response("404 Not Found", "", b""),
            };
            response("200 OK", "", &cold_item(&item, "zlib"))
        }
        let staged = |blocks: &[Vec<u8>], spool: SpoolLimits| {
            let restore = Account::new().restore(blocks.to_vec(), spool, move |key, stream| {
                let _ = stream.write_all(&reply(key));
            });
            let result = restore.outcome.as_ref().unwrap();
            let status = restore.job.status();
            let mut reads = std::collections::BTreeMap::new();
            for head in &restore.cold_reads {
                let key = head.lines().find_map(|line| line.strip_prefix("x-risu-key: ")).unwrap_or_default();
                *reads.entry(key.to_owned()).or_insert(0) += 1;
            }
            let store = restore.sink.store.lock().unwrap();
            let plugins = ["a", "n", "f", "g", "s", "inline"].map(|key| plugin_value(&store, key));
            // Records outside the characters take new ids in every restore.
            let library = store.materialize(None).unwrap();
            let characters = &library["characters"];
            serde_json::json!({
                "warnings": result.warning_codes,
                "characters": characters,
                "plugins": plugins,
                "preview": status.incomplete_restore_preview,
                "progress": status.progress,
                "detail": status.detail,
                "reads": reads,
            })
        };

        let character_reads = serde_json::json!({"cold-c1": 1, "cold-c1-a": 2, "cold-c1-b": 1, "cold-c3": 1, "cold-s1": 3, "cold-s2": 1});
        for (storage, plugin_reads) in [
            (
                serde_json::json!({"_coldplugin": {"a": "cold-pa", "n": "cold-null", "f": "cold-flaky", "g": "cold-gone"}}),
                serde_json::json!({"cold-pa": 1, "cold-null": 1, "cold-flaky": 3, "cold-gone": 1}),
            ),
            // The index names the values to read, and the legacy values it
            // replaces, only once it is read.
            (
                serde_json::json!({"_coldpluginIndex": "cold-index", "_coldplugin": {"a": "cold-pa", "g": "cold-gone"}, "inline": 1}),
                serde_json::json!({"cold-index": 1, "cold-large": 1, "cold-gone": 1}),
            ),
        ] {
            blocks[5] = restore::tests::block(11, false, "pluginStorage", &storage);
            let ahead = staged(&blocks, SpoolLimits::default());
            // With no spool, each item is read only when staging asks for it.
            let on_demand = staged(&blocks, SpoolLimits { items: 0, bytes: 0 });
            assert_eq!(ahead, on_demand, "{storage}");
            let mut reads = character_reads.clone();
            reads.as_object_mut().unwrap().extend(plugin_reads.as_object().unwrap().clone());
            assert_eq!(ahead["reads"], reads, "{storage}");
        }
    }

    #[test]
    fn cancelling_a_snapshot_restore_while_reading_ahead_leaves_no_items_behind() {
        let account = Account::new();
        let (job, owned) = (account.job.clone(), account.owned.path().to_owned());
        let cancelled_at = Arc::new(Mutex::new(None));
        let started = AtomicUsize::new(0);
        let restore = account.restore(cold_plugin_values(4), SpoolLimits::default(), {
            let cancelled_at = cancelled_at.clone();
            move |_, stream| {
                // Half of the body, so each read holds part of an item when it stops.
                let body = cold_item(&Value::String("cold".repeat(1024)), "gzip");
                let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes());
                let _ = stream.write_all(&body[..body.len() / 2]);
                let _ = stream.flush();
                if started.fetch_add(1, Ordering::AcqRel) == 1 {
                    // Both slots hold a read: cancel once both have spooled bytes.
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let spooled = || fs::read_dir(&owned).unwrap().filter(|entry| entry.as_ref().unwrap().path().extension().is_some_and(|extension| extension == "part")).count();
                    while spooled() < 2 && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(5));
                    }
                    *cancelled_at.lock().unwrap() = Some(Instant::now());
                    job.request_cancel().unwrap();
                }
                // Hold the rest until the restore drops the read.
                let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                let _ = stream.read(&mut [0; 1]);
            }
        });

        assert_eq!(restore.outcome.as_ref().unwrap_err().code, "cancelled");
        assert!(cancelled_at.lock().unwrap().unwrap().elapsed() < Duration::from_secs(5));
        assert_eq!(restore.sink.abort_calls.load(Ordering::Acquire), 1);
        assert_eq!(restore.sink.store.lock().unwrap().revision().unwrap(), 1);
        let left = fs::read_dir(restore.owned.path()).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).filter(|name| name != SNAPSHOT_FILE).collect::<Vec<_>>();
        assert_eq!(left, Vec::<String>::new());
    }

    #[test]
    fn routes_legacy_snapshots_to_the_prepared_compatibility_restore() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(SNAPSHOT_FILE);
        fs::write(&path, b"\0RISUSAVE\0\x07legacy").unwrap();
        let mut source = OpenedJobSource {
            file: File::open(path).unwrap(),
            total_bytes: 17,
            custody: None,
        };

        let error = require_native_prepared_format(&mut source).unwrap_err();

        assert_eq!(error.code, "compatibility-required");
        assert_eq!(source.file.stream_position().unwrap(), 0);
    }
}
