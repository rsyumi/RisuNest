use super::{restore, JobControl, JobPhase, JobResultSummary, NativeJobError, OpenedJobSource};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

const DATABASE_KEY: &str = "database/database.bin";
const SNAPSHOT_FILE: &str = "official-account-snapshot.risudat";
const SNAPSHOT_PART_FILE: &str = "official-account-snapshot.risudat.part";
const MAX_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const NETWORK_CANCEL_POLL: Duration = Duration::from_millis(250);
const NETWORK_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

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
    let downloaded =
        tauri::async_runtime::block_on(download_snapshot(&request, owned_directory, job))?;
    let DownloadOutcome::File(mut source) = downloaded else {
        return Err(NativeJobError::new(
            "remote-missing",
            "No official account snapshot exists",
        ));
    };
    require_native_prepared_format(&mut source)?;
    restore::restore_started_risu_save(
        source,
        expected_revision,
        job,
        sink,
        restore::RestoreProgressScale::default(),
    )
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
    download_snapshot_with_idle_timeout(request, owned_directory, job, NETWORK_IDLE_TIMEOUT).await
}

async fn download_snapshot_with_idle_timeout(
    request: &OfficialSnapshotRestoreRequest,
    owned_directory: &Path,
    job: &JobControl,
    idle_timeout: Duration,
) -> Result<DownloadOutcome, NativeJobError> {
    let endpoint = snapshot_endpoint(&request.base_url)?;
    let client = reqwest::Client::builder()
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
            .send(),
        job,
        idle_timeout,
        "Official account snapshot request was cancelled",
    )
    .await?;
    let status = response.status().as_u16();
    if status == 204 {
        return Ok(DownloadOutcome::Missing);
    }
    if status == 303 {
        let bytes = bounded_response_bytes(response, job, idle_timeout).await?;
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
    if !(200..300).contains(&status) {
        let message =
            String::from_utf8_lossy(&bounded_response_bytes(response, job, idle_timeout).await?)
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
    loop {
        let chunk = await_network_operation(
            response.chunk(),
            job,
            idle_timeout,
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
        output.write_all(&chunk).map_err(store_error)?;
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
    let file = File::open(&destination).map_err(store_error)?;
    Ok(DownloadOutcome::File(OpenedJobSource {
        file,
        total_bytes: written,
    }))
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

fn authenticated_headers(
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

async fn bounded_response_bytes(
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

async fn await_network_operation<F, T>(
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
            while stream.read(&mut buffer).unwrap() != 0 {}
            request
        });
        (format!("http://{address}"), handle)
    }

    fn stalled_server(
        response_prefix: Vec<u8>,
    ) -> (
        String,
        mpsc::Receiver<()>,
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
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            stream.write_all(&response_prefix).unwrap();
            stream.flush().unwrap();
            ready_tx.send(()).unwrap();
            let _ = stop_rx.recv_timeout(Duration::from_secs(5));
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
        let (base_url, request_received, stop_server, server) = stalled_server(Vec::new());
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
        let (base_url, body_started, stop_server, server) = stalled_server(response);
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
        let (base_url, _body_started, stop_server, server) = stalled_server(response);
        let directory = TempDir::new().unwrap();
        let job = job();

        let result = tauri::async_runtime::block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                download_snapshot_with_idle_timeout(
                    &request(base_url),
                    directory.path(),
                    &job,
                    Duration::from_millis(100),
                ),
            )
            .await
        });

        let _ = stop_server.send(());
        server.join().unwrap();
        let error = match result.expect("stalled body must respect its idle timeout") {
            Err(error) => error,
            Ok(_) => panic!("stalled body must fail"),
        };
        assert_eq!(error.code, "network-error");
    }

    #[test]
    fn idle_timeout_ends_a_stalled_error_response_body() {
        let response = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 16\r\nConnection: close\r\n\r\npartial"
            .to_vec();
        let (base_url, _body_started, stop_server, server) = stalled_server(response);
        let directory = TempDir::new().unwrap();
        let job = job();

        let result = tauri::async_runtime::block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                download_snapshot_with_idle_timeout(
                    &request(base_url),
                    directory.path(),
                    &job,
                    Duration::from_millis(100),
                ),
            )
            .await
        });

        let _ = stop_server.send(());
        server.join().unwrap();
        let error = match result.expect("stalled error body must respect its idle timeout") {
            Err(error) => error,
            Ok(_) => panic!("stalled error body must fail"),
        };
        assert_eq!(error.code, "network-error");
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

    #[test]
    fn routes_legacy_snapshots_to_the_prepared_compatibility_restore() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(SNAPSHOT_FILE);
        fs::write(&path, b"\0RISUSAVE\0\x07legacy").unwrap();
        let mut source = OpenedJobSource {
            file: File::open(path).unwrap(),
            total_bytes: 17,
        };

        let error = require_native_prepared_format(&mut source).unwrap_err();

        assert_eq!(error.code, "compatibility-required");
        assert_eq!(source.file.stream_position().unwrap(), 0);
    }
}
