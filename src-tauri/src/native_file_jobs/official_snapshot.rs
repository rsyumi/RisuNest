use risunest_network_log::reqwest::RequestBuilderExt;
use super::official_assets::ColdSource;
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
        cold: ColdSource::new(&request.base_url, &request.credential, owned_directory, job, limit)?,
        incomplete: Mutex::default(),
    };
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
    // A cold item read that ended staging reports why, not the staging error it caused.
    .map_err(|error| sink.cold.fatal().unwrap_or(error))
}

/// Stages into the library like any restore, and reads the snapshot's cold
/// references from the account while their records are staged.
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

    fn commit(&self, staging_id: &str, expected_revision: i64, binding_authority: Option<&str>) -> StoreResult<RevisionResult> {
        self.inner.commit(staging_id, expected_revision, binding_authority)
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
    use std::net::{Shutdown, TcpListener};
    use std::sync::mpsc;
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

    /// Answers each request on its own connection, by path and `x-risu-key`,
    /// and keeps every request head it saw.
    fn account_server(respond: impl Fn(&str, &str) -> Vec<u8> + Send + 'static) -> (String, std::sync::Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                    request.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&request).into_owned();
                let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
                let key = head.lines()
                    .find_map(|line| line.split_once(':').filter(|(name, _)| name.eq_ignore_ascii_case("x-risu-key")))
                    .map_or("", |(_, value)| value.trim()).to_owned();
                seen.lock().unwrap().push(head);
                let _ = stream.write_all(&respond(&path, &key));
                let _ = stream.shutdown(Shutdown::Write);
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

    struct AccountRestore {
        _directory: TempDir,
        sink: restore::tests::StoreSink,
        job: std::sync::Arc<JobControl>,
        outcome: Result<JobResultSummary, NativeJobError>,
        cold_reads: Vec<String>,
        /// The job's detail as each cold item read arrived, by key.
        cold_read_details: Vec<(String, Option<JobDetail>)>,
    }

    /// Restores `blocks` as the account snapshot while `cold` answers each cold
    /// item read by its key.
    fn restore_from_account(blocks: Vec<Vec<u8>>, cold: impl Fn(&str) -> Vec<u8> + Send + 'static) -> AccountRestore {
        let snapshot = restore::tests::save_bytes(blocks);
        let job = JobRegistry::default().create(JobKind::RestoreOfficialAccountSnapshot).unwrap();
        let details = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (base_url, requests) = account_server({
            let (job, details) = (job.clone(), details.clone());
            move |path, key| {
                if path != "/hub/account/coldstorage" { return response("200 OK", "", &snapshot) }
                details.lock().unwrap().push((key.to_owned(), job.status().detail));
                cold(key)
            }
        });
        let (directory, sink) = restore::tests::fixture();
        let owned = TempDir::new().unwrap();
        let outcome = restore_official_snapshot(request(base_url), 1, owned.path(), &job, &sink);
        let cold_reads = requests.lock().unwrap().iter().filter(|head| head.contains(" /hub/account/coldstorage ")).cloned().collect();
        let cold_read_details = details.lock().unwrap().clone();
        AccountRestore { _directory: directory, sink, job, outcome, cold_reads, cold_read_details }
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
        let character = cold_item(&character, "gzip");
        let character_bytes = character.len() as u64;
        let restore = restore_from_account(blocks, move |key| match key {
            "cold-character" => response("200 OK", "", &character),
            "cold-chat" => response("200 OK", "", &cold_item(&chat, "zlib")),
            "cold-null" => response("200 OK", "", &cold_item(&Value::Null, "deflate")),
            "cold-flaky" => response("503 Service Unavailable", "Retry-After: 0\r\n", b""),
            _ => response("404 Not Found", "", b""),
        });

        let result = restore.outcome.unwrap();
        assert_eq!(result.warning_codes, ["unavailable-cold-payloads:1", "missing-backup-data:1", "damaged-plugin-storage:1"]);
        // Each item read counts with the blocks: the character and chat while the
        // blocks are read, and the plugin value once `staging-database` has begun.
        assert_eq!(restore.job.status().progress.completed_items, block_count + 3);
        // The character's chats are read within its block, and the database read
        // moves by the character's bytes, its total unknown from then on.
        let detail_at = |key: &str| restore.cold_read_details.iter().find(|(read, _)| read == key).and_then(|(_, detail)| detail.clone()).unwrap();
        let (before, after) = (detail_at("cold-character"), detail_at("cold-chat"));
        assert_eq!(before.stage, JobStage::ReadingDatabase);
        assert!(before.stage_total.is_some());
        assert_eq!((after.stage, after.stage_completed, after.stage_total), (before.stage, before.stage_completed + character_bytes, None));
        let preview = restore.job.status().incomplete_restore_preview.unwrap();
        assert_eq!(preview.unavailable_cold_keys, ["cold-missing"]);
        assert_eq!(preview.character_names, ["Expanded"]);
        let store = restore.sink.store.lock().unwrap();
        let restored = store.materialize(None).unwrap();
        assert_eq!(restored["characters"][0]["name"], "Expanded");
        assert_eq!(restored["characters"][0]["chats"][0]["message"], messages);
        assert_eq!(restored["characters"][0]["chats"][1]["message"], serde_json::json!([]));
        let unowned = |key| store.read_plugin_storage(crate::persistent_store::plugin_owner::UNOWNED_OWNER, key, None).unwrap().map(|row| row.value);
        // An item that holds JSON null is a stored null, not a missing item.
        assert_eq!(unowned("nullValue"), Some(Value::Null));
        assert_eq!(unowned("gone"), None);
        assert_eq!(unowned("flaky"), None);
        assert!(restore.cold_reads.iter().all(|head| head.contains("x-risu-auth: test-token\r\n")));
        // Each item is read once, and a failing one only within its retry budget.
        let reads = |key: &str| restore.cold_reads.iter().filter(|head| head.contains(&format!("x-risu-key: {key}\r\n"))).count();
        for key in ["cold-character", "cold-chat", "cold-missing", "cold-null", "cold-gone"] {
            assert_eq!(reads(key), 1, "{key}");
        }
        assert_eq!(reads("cold-flaky"), 3);
    }

    #[test]
    fn snapshot_restore_stops_before_activation_when_the_account_stops_cold_reads() {
        let storage = (0..5).map(|index| (format!("value-{index}"), Value::String(format!("cold-{index}")))).collect::<serde_json::Map<_, _>>();
        for (status, code, reads) in [
            ("401 Unauthorized", "reauthentication-needed", 1),
            ("403 Forbidden", "reauthentication-needed", 1),
            // Five items in a row that exhaust their retries mean the service is down.
            ("503 Service Unavailable", "network-error", 15),
        ] {
            let mut blocks = restore::tests::valid_blocks();
            blocks[5] = restore::tests::block(11, false, "pluginStorage", &serde_json::json!({"_coldplugin": storage}));
            let restore = restore_from_account(blocks, move |_| response(status, "Retry-After: 0\r\n", b""));

            let error = restore.outcome.unwrap_err();
            assert_eq!(error.code, code, "{status}");
            assert_eq!(restore.cold_reads.len(), reads, "{status}");
            assert_eq!(restore.sink.abort_calls.load(std::sync::atomic::Ordering::Acquire), 1);
            assert_eq!(restore.sink.store.lock().unwrap().revision().unwrap(), 1);
        }
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
