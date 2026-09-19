use super::error::{self, cancelled, invalid_input};
use super::{
    JobControl, JobPhase, JobProgress, JobResultSummary, NativeJobError,
    OfficialPublicationAttemptResult, OfficialPublicationCredential, OfficialPublicationJobRequest,
    OfficialPublicationRetryRequest,
};
use crate::persistent_store::{OfficialPublicationPayload, StoreError};
use futures::future::{select, Either};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::{Client, Response, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::io::ReaderStream;

const DATABASE_KEY: &str = "database/database.bin";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_ACCOUNT_BYTES: usize = 512;
const MAX_PRIVATE_STRING_BYTES: usize = 4096;
const MAX_SAVE_DATE_BYTES: usize = 128;
const MAX_REPLACEMENT_STRING_BYTES: usize = 4096;
const MAX_REPLACEMENT_ENTRIES: usize = 1_000_000;
const MAX_REPLACEMENT_AGGREGATE_BYTES: usize = 64 * 1024 * 1024;

pub(super) fn validate_start_request(
    request: &OfficialPublicationJobRequest,
) -> Result<(), NativeJobError> {
    if request.expected_revision < 0
        || !request.lease.starts_with("snapshot-")
        || request.lease.len() > MAX_PRIVATE_STRING_BYTES
    {
        return Err(invalid_input(
            "official publication revision lease is invalid",
        ));
    }
    validate_string(&request.account_id, MAX_ACCOUNT_BYTES, false, "account")?;
    validate_string(
        &request.base_url,
        MAX_PRIVATE_STRING_BYTES,
        false,
        "base URL",
    )?;
    validate_string(&request.save_date, MAX_SAVE_DATE_BYTES, false, "save date")?;
    if let Some(session) = &request.session {
        validate_string(session, MAX_PRIVATE_STRING_BYTES, true, "session")?;
        validate_header_value(session, "session")?;
    }
    validate_credential(&request.credential)?;
    endpoint(&request.base_url, "/api/account/getsessionnumber")?;
    endpoint(&request.base_url, "/api/account/write")?;
    if request.replacements.len() > MAX_REPLACEMENT_ENTRIES {
        return Err(invalid_input(
            "official publication replacement map has too many entries",
        ));
    }
    let aggregate = request.replacements.iter().try_fold(
        0usize,
        |total, (source, replacement)| -> Result<usize, NativeJobError> {
            validate_string(
                source,
                MAX_REPLACEMENT_STRING_BYTES,
                true,
                "replacement source",
            )?;
            validate_string(
                replacement,
                MAX_REPLACEMENT_STRING_BYTES,
                true,
                "replacement target",
            )?;
            total
                .checked_add(source.len())
                .and_then(|total| total.checked_add(replacement.len()))
                .ok_or_else(|| invalid_input("official publication replacements are too large"))
        },
    )?;
    if aggregate > MAX_REPLACEMENT_AGGREGATE_BYTES {
        return Err(invalid_input(
            "official publication replacements are too large",
        ));
    }
    Ok(())
}

pub(super) fn validate_retry_request(
    request: &OfficialPublicationRetryRequest,
) -> Result<(), NativeJobError> {
    validate_string(&request.account_id, MAX_ACCOUNT_BYTES, false, "account")?;
    validate_string(&request.save_date, MAX_SAVE_DATE_BYTES, false, "save date")?;
    if let Some(session) = &request.session {
        validate_string(session, MAX_PRIVATE_STRING_BYTES, true, "session")?;
        validate_header_value(session, "session")?;
    }
    validate_credential(&request.credential)
}

pub(super) fn run_job(
    request: OfficialPublicationJobRequest,
    app: AppHandle,
    job: Arc<JobControl>,
) -> Result<JobResultSummary, NativeJobError> {
    if job.is_cancel_requested() {
        return Err(cancelled("official publication cancelled before export"));
    }
    job.start(JobPhase::WritingExport)
        .map_err(|error| job_control_error(&job, error))?;
    let prepared = crate::persistent_store::commands::with_store_mut(app.state(), |store| {
        store.prepare_official_publication_for_job(
            &request.lease,
            request.expected_revision,
            &job.id(),
            now_millis(),
        )
    })
    .map_err(store_error)?;
    let revision = prepared.revision;
    let progress_failure = RefCell::new(None);
    let payload = prepared
        .create_payload(
            &request.account_id,
            &request.replacements,
            || job.is_cancel_requested() || progress_failure.borrow().is_some(),
            |completed_bytes, _, _| {
                if job.is_cancel_requested() {
                    return;
                }
                if let Err(error) = job.set_progress(JobProgress {
                    completed_bytes,
                    total_bytes: None,
                    completed_items: 0,
                    total_items: Some(2),
                }) {
                    *progress_failure.borrow_mut() = Some(error);
                }
            },
        )
        .map_err(|error| {
            progress_failure
                .borrow_mut()
                .take()
                .map(|error| job_control_error(&job, error))
                .unwrap_or_else(|| store_error(error))
        })?;
    if let Some(error) = progress_failure.into_inner() {
        return finish_payload_cleanup(payload, Err(job_control_error(&job, error)));
    }
    if job.is_cancel_requested() {
        return finish_payload_cleanup(
            payload,
            Err(cancelled("official publication cancelled before upload")),
        );
    }
    if let Err(error) = job.set_phase(JobPhase::UploadingDatabase) {
        return finish_payload_cleanup(payload, Err(job_control_error(&job, error)));
    }
    if let Err(error) = job.set_progress(JobProgress {
        completed_bytes: payload.bytes,
        total_bytes: None,
        completed_items: 1,
        total_items: Some(2),
    }) {
        return finish_payload_cleanup(payload, Err(job_control_error(&job, error)));
    }

    let result =
        tauri::async_runtime::block_on(run_publication_loop(&payload, request, Arc::clone(&job)));
    let result = result.and_then(|publication| {
        job.begin_official_publication_finalization()
            .map_err(|error| job_control_error(&job, error))?;
        let progress = job.status().progress;
        job.set_progress(JobProgress {
            completed_bytes: progress.completed_bytes,
            total_bytes: None,
            completed_items: 2,
            total_items: Some(2),
        })
        .map_err(|error| job_control_error(&job, error))?;
        Ok(JobResultSummary {
            export_exclusions: None,
            revision,
            source_bytes: payload.bytes,
            source_sha256: payload.sha256.clone(),
            character_count: payload.character_count,
            preset_count: payload.preset_count,
            warning_codes: Vec::new(),
            handoff_path: None,
            publication: Some(publication),
        })
    });
    finish_payload_cleanup(payload, result)
}

async fn run_publication_loop(
    payload: &OfficialPublicationPayload,
    request: OfficialPublicationJobRequest,
    job: Arc<JobControl>,
) -> Result<OfficialPublicationAttemptResult, NativeJobError> {
    let client = build_client()?;
    let mut session = request.session;
    let mut save_date = request.save_date;
    let mut credential = request.credential;
    loop {
        let attempt = upload_attempt(
            &client,
            payload,
            &request.base_url,
            &request.account_id,
            session,
            &save_date,
            &credential,
            Arc::clone(&job),
        )
        .await?;
        match attempt {
            attempt @ OfficialPublicationAttemptResult::ReauthenticationNeeded { .. } => {
                let retry = job.wait_for_official_publication_retry(attempt)?;
                session = retry.session;
                save_date = retry.save_date;
                credential = retry.credential;
            }
            terminal => return Ok(terminal),
        }
    }
}

async fn upload_attempt(
    client: &Client,
    payload: &OfficialPublicationPayload,
    base_url: &str,
    account_id: &str,
    requested_session: Option<String>,
    save_date: &str,
    credential: &OfficialPublicationCredential,
    job: Arc<JobControl>,
) -> Result<OfficialPublicationAttemptResult, NativeJobError> {
    let session = match requested_session {
        Some(session) if !session.is_empty() => Some(session),
        _ => Some(acquire_session(client, base_url, credential, Arc::clone(&job)).await?),
    };
    let (mut file, bytes) = payload.open().map_err(store_error)?;
    if bytes != payload.bytes {
        return Err(NativeJobError::new(
            "invalid-source",
            "official publication payload length changed",
        ));
    }
    verify_and_rewind_payload(&mut file, &payload.sha256, &job)?;
    let activity = Arc::new(AtomicU64::new(0));
    let file = tokio::fs::File::from_std(file);
    let body = reqwest::Body::wrap_stream(ReaderStream::new(PublicationPayloadReader {
        file,
        job: Arc::clone(&job),
        activity: Arc::clone(&activity),
    }));
    let mut headers = authenticated_headers(credential)?;
    for (name, value) in [
        (CONTENT_TYPE, "application/octet-stream"),
        (HeaderName::from_static("x-risu-key"), DATABASE_KEY),
        (HeaderName::from_static("x-format"), "nocheck"),
        (
            HeaderName::from_static("x-risu-session"),
            session.as_deref().unwrap_or_default(),
        ),
        (HeaderName::from_static("x-risu-save-date"), save_date),
    ] {
        headers.insert(
            name,
            HeaderValue::from_str(value)
                .map_err(|_| invalid_input("official publication header is invalid"))?,
        );
    }
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&bytes.to_string()).expect("file length is a valid header"),
    );
    let response = await_controlled(
        client
            .post(endpoint(base_url, "/api/account/write")?)
            .headers(headers)
            .body(body)
            .send(),
        &job,
        &activity,
        IDLE_TIMEOUT,
    )
    .await?;
    let status = response.status().as_u16();
    let warning_status =
        response.headers().get("x-risu-status") == Some(&HeaderValue::from_static("warn"));
    let json_response = is_json_response(&response);
    if status == 304 || (status == 403 && !json_response) {
        return classify_response(
            status,
            warning_status,
            json_response,
            None,
            account_id,
            session,
            save_date,
        );
    }
    if status != 403 && !(200..300).contains(&status) {
        return Err(NativeJobError::new(
            "http-status",
            format!("official publication server returned HTTP {status}"),
        ));
    }
    activity.store(0, Ordering::Release);
    let bytes = response_bytes_controlled(response, &job, &activity, IDLE_TIMEOUT).await?;
    classify_response(
        status,
        warning_status,
        json_response,
        Some(bytes),
        account_id,
        session,
        save_date,
    )
}

fn verify_and_rewind_payload(
    file: &mut std::fs::File,
    expected_sha256: &str,
    job: &JobControl,
) -> Result<(), NativeJobError> {
    let mut buffer = [0u8; 64 * 1024];
    let mut hasher = Sha256::new();
    loop {
        if job.is_cancel_requested() {
            return Err(cancelled(
                "official publication payload verification was cancelled",
            ));
        }
        let read = file.read(&mut buffer).map_err(|_| {
            NativeJobError::new(
                "invalid-source",
                "official publication payload could not be verified",
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    if hex::encode(hasher.finalize()) != expected_sha256 {
        return Err(NativeJobError::new(
            "invalid-source",
            "official publication payload content changed",
        ));
    }
    file.seek(SeekFrom::Start(0)).map_err(|_| {
        NativeJobError::new(
            "invalid-source",
            "official publication payload could not be verified",
        )
    })?;
    Ok(())
}

async fn acquire_session(
    client: &Client,
    base_url: &str,
    credential: &OfficialPublicationCredential,
    job: Arc<JobControl>,
) -> Result<String, NativeJobError> {
    let activity = Arc::new(AtomicU64::new(0));
    let response = await_controlled(
        client
            .get(endpoint(base_url, "/api/account/getsessionnumber")?)
            .headers(authenticated_headers(credential)?)
            .send(),
        &job,
        &activity,
        IDLE_TIMEOUT,
    )
    .await?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(NativeJobError::new(
            "http-status",
            format!("official publication session server returned HTTP {status}"),
        ));
    }
    let bytes = response_bytes_controlled(response, &job, &activity, IDLE_TIMEOUT).await?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        NativeJobError::new(
            "invalid-response",
            "official publication session response is invalid",
        )
    })?;
    let session = match &value["sessionNumber"] {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => {
            return Err(NativeJobError::new(
                "invalid-response",
                "official publication session response has no session number",
            ));
        }
    };
    validate_string(&session, MAX_PRIVATE_STRING_BYTES, false, "session")?;
    validate_header_value(&session, "session")?;
    Ok(session)
}

async fn response_bytes_controlled(
    mut response: Response,
    job: &JobControl,
    activity: &AtomicU64,
    idle_timeout: Duration,
) -> Result<Vec<u8>, NativeJobError> {
    let mut bytes = Vec::new();
    loop {
        let chunk = await_controlled(response.chunk(), job, activity, idle_timeout).await?;
        let Some(chunk) = chunk else {
            return Ok(bytes);
        };
        activity.fetch_add(chunk.len() as u64, Ordering::AcqRel);
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(NativeJobError::new(
                "response-too-large",
                "official publication response exceeds 64 KiB",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
}

async fn await_controlled<F, T>(
    future: F,
    job: &JobControl,
    activity: &AtomicU64,
    idle_timeout: Duration,
) -> Result<T, NativeJobError>
where
    F: Future<Output = Result<T, reqwest::Error>>,
{
    let mut future = Box::pin(future);
    let mut observed = activity.load(Ordering::Acquire);
    let mut last_activity = Instant::now();
    loop {
        let timer = Box::pin(tokio::time::sleep(CONTROL_POLL_INTERVAL));
        match select(future, timer).await {
            Either::Left((Ok(value), _)) => return Ok(value),
            Either::Left((Err(_), _)) if job.is_cancel_requested() => {
                return Err(cancelled(
                    "official publication network request was cancelled",
                ));
            }
            Either::Left((Err(error), _)) => return Err(transport_error(error)),
            Either::Right((_, pending)) => future = pending,
        }
        if job.is_cancel_requested() {
            return Err(cancelled(
                "official publication network request was cancelled",
            ));
        }
        let current = activity.load(Ordering::Acquire);
        if current != observed {
            observed = current;
            last_activity = Instant::now();
        } else if last_activity.elapsed() >= idle_timeout {
            return Err(NativeJobError::new(
                "transport-timeout",
                "official publication network request made no progress",
            ));
        }
    }
}

fn classify_response(
    status: u16,
    warning_status: bool,
    json_response: bool,
    bytes: Option<Vec<u8>>,
    account_id: &str,
    session: Option<String>,
    save_date: &str,
) -> Result<OfficialPublicationAttemptResult, NativeJobError> {
    if status == 304 {
        return Ok(OfficialPublicationAttemptResult::NotModified {
            account_id: account_id.to_owned(),
            session,
            save_date: save_date.to_owned(),
            status,
            replacement_key: DATABASE_KEY.to_owned(),
        });
    }
    if status == 403 {
        // Warning content is optional; malformed JSON still follows the 403 auth path.
        let warning = if json_response {
            bytes
                .as_deref()
                .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
                .and_then(|value| {
                    value
                        .get("warning")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
        } else {
            None
        };
        if let Some(warning) = &warning {
            validate_string(warning, MAX_PRIVATE_STRING_BYTES, true, "warning")?;
        }
        return Ok(if warning_status {
            OfficialPublicationAttemptResult::AuthWarning {
                account_id: account_id.to_owned(),
                session,
                save_date: save_date.to_owned(),
                status,
                warning,
            }
        } else {
            OfficialPublicationAttemptResult::ReauthenticationNeeded {
                account_id: account_id.to_owned(),
                session,
                save_date: save_date.to_owned(),
                status,
                warning,
            }
        });
    }
    if !(200..300).contains(&status) {
        return Err(NativeJobError::new(
            "http-status",
            format!("official publication server returned HTTP {status}"),
        ));
    }
    let bytes = bytes.ok_or_else(|| {
        NativeJobError::new(
            "invalid-response",
            "official publication response body is missing",
        )
    })?;
    let text = String::from_utf8(bytes).map_err(|_| {
        NativeJobError::new(
            "invalid-response",
            "official publication response is not UTF-8",
        )
    })?;
    if text.is_empty() || text.len() > MAX_REPLACEMENT_STRING_BYTES {
        return Err(NativeJobError::new(
            "invalid-response",
            "official publication replacement key is invalid",
        ));
    }
    let (warning, reload_session) = if json_response {
        let value: Value = serde_json::from_str(&text).map_err(|_| {
            NativeJobError::new(
                "invalid-response",
                "official publication JSON response is invalid",
            )
        })?;
        let warning = value
            .get("warning")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(warning) = &warning {
            if warning.len() > MAX_PRIVATE_STRING_BYTES {
                return Err(NativeJobError::new(
                    "invalid-response",
                    "official publication warning is too large",
                ));
            }
        }
        (
            warning,
            value
                .get("reloadSession")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )
    } else {
        (None, false)
    };
    Ok(OfficialPublicationAttemptResult::Written {
        account_id: account_id.to_owned(),
        session,
        save_date: save_date.to_owned(),
        status,
        replacement_key: text,
        warning,
        reload_session,
    })
}

fn record_uploaded_bytes(job: &JobControl, added: u64) -> Result<(), String> {
    let progress = job.status().progress;
    job.set_progress(JobProgress {
        completed_bytes: progress.completed_bytes.saturating_add(added),
        total_bytes: None,
        completed_items: 1,
        total_items: Some(2),
    })
}

struct PublicationPayloadReader {
    file: tokio::fs::File,
    job: Arc<JobControl>,
    activity: Arc<AtomicU64>,
}

impl AsyncRead for PublicationPayloadReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.job.is_cancel_requested() {
            return Poll::Ready(Err(io::Error::other(
                "official publication upload cancelled",
            )));
        }
        let before = buffer.filled().len();
        match Pin::new(&mut self.file).poll_read(context, buffer) {
            Poll::Ready(Ok(())) => {
                let read = buffer.filled().len().saturating_sub(before) as u64;
                if read > 0 {
                    self.activity.fetch_add(read, Ordering::AcqRel);
                    if let Err(error) = record_uploaded_bytes(&self.job, read) {
                        return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, error)));
                    }
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

fn authenticated_headers(
    credential: &OfficialPublicationCredential,
) -> Result<HeaderMap, NativeJobError> {
    let mut headers = HeaderMap::new();
    match credential {
        OfficialPublicationCredential::RisuAuth { token } => {
            headers.insert(
                HeaderName::from_static("x-risu-auth"),
                HeaderValue::from_str(token)
                    .map_err(|_| invalid_input("official publication credential is invalid"))?,
            );
        }
    }
    Ok(headers)
}

fn build_client() -> Result<Client, NativeJobError> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|_| NativeJobError::new("transport-failed", "HTTP client could not be created"))
}

fn endpoint(base_url: &str, path: &str) -> Result<Url, NativeJobError> {
    let parsed = Url::parse(base_url)
        .map_err(|_| invalid_input("official publication base URL is invalid"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(invalid_input(
            "official publication base URL must use HTTP or HTTPS",
        ));
    }
    Url::parse(&format!("{}{path}", base_url.trim_end_matches('/')))
        .map_err(|_| invalid_input("official publication endpoint URL is invalid"))
}

fn is_json_response(response: &Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
}

fn validate_credential(credential: &OfficialPublicationCredential) -> Result<(), NativeJobError> {
    match credential {
        OfficialPublicationCredential::RisuAuth { token } => {
            validate_string(token, MAX_PRIVATE_STRING_BYTES, false, "credential")?;
            validate_header_value(token, "credential")
        }
    }
}

fn validate_header_value(value: &str, label: &str) -> Result<(), NativeJobError> {
    HeaderValue::from_str(value)
        .map(|_| ())
        .map_err(|_| invalid_input(format!("official publication {label} is invalid")))
}

fn validate_string(
    value: &str,
    maximum_bytes: usize,
    allow_empty: bool,
    label: &str,
) -> Result<(), NativeJobError> {
    if value.len() > maximum_bytes || (!allow_empty && value.is_empty()) {
        return Err(invalid_input(format!(
            "official publication {label} is invalid"
        )));
    }
    Ok(())
}

fn finish_payload_cleanup(
    payload: OfficialPublicationPayload,
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
            format!(
                "{}; official publication payload cleanup failed: {cleanup}",
                error.message
            ),
        )),
        (Err(error), Ok(())) => Err(error),
    }
}

fn job_control_error(job: &JobControl, error: impl AsRef<str>) -> NativeJobError {
    if job.is_cancel_requested() {
        cancelled("official publication was cancelled")
    } else {
        NativeJobError::new("store-error", error)
    }
}

fn store_error(error: StoreError) -> NativeJobError {
    // Local override: a store-level export cancellation surfaces as
    // "cancelled" here; everything else uses the shared mapping.
    match error {
        StoreError::Validation { ref message }
            if message == crate::persistent_store::export::EXPORT_CANCELLED_MESSAGE =>
        {
            cancelled("official publication export was cancelled")
        }
        error => error::store_error(error),
    }
}

fn transport_error(error: reqwest::Error) -> NativeJobError {
    let message = if error.is_timeout() {
        "official publication network request timed out"
    } else if error.is_connect() {
        "official publication endpoint is unavailable"
    } else if error.is_body() {
        "official publication payload could not be streamed"
    } else {
        "official publication network request failed"
    };
    NativeJobError::new("transport-failed", message)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_file_jobs::{
        CancelOutcome, JobKind, JobPhase, JobRegistry, OfficialPublicationAttemptResult,
        OfficialPublicationCredential, OfficialPublicationJobRequest,
    };
    use crate::persistent_store::{OfficialPublicationPayload, PersistentStore};
    use serde_json::json;
    use std::collections::{BTreeMap, HashMap};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::io::AsyncReadExt;

    fn request() -> OfficialPublicationJobRequest {
        OfficialPublicationJobRequest {
            lease: "snapshot-publication".to_owned(),
            expected_revision: 7,
            account_id: "account-1".to_owned(),
            base_url: "http://127.0.0.1:3000".to_owned(),
            replacements: HashMap::from([("asset".to_owned(), "remote".to_owned())]),
            session: Some("session-1".to_owned()),
            save_date: "save-date".to_owned(),
            credential: OfficialPublicationCredential::RisuAuth {
                token: "secret".to_owned(),
            },
        }
    }

    #[test]
    fn cancelled_payload_reader_uses_a_non_retryable_error() {
        let directory = tempfile::TempDir::new().unwrap();
        let source = directory.path().join("payload.bin");
        std::fs::write(&source, b"payload").unwrap();
        let job = running_job();
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::Requested);
        let mut reader = PublicationPayloadReader {
            file: tokio::fs::File::from_std(std::fs::File::open(source).unwrap()),
            job,
            activity: Arc::new(AtomicU64::new(0)),
        };

        let error = tauri::async_runtime::block_on(async {
            let mut byte = [0u8; 1];
            reader.read(&mut byte).await.unwrap_err()
        });

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "official publication upload cancelled");
    }

    #[derive(Debug, PartialEq, Eq)]
    struct RecordedRequest {
        method: String,
        path: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    struct MockResponse {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    }

    fn publication_payload() -> (tempfile::TempDir, OfficialPublicationPayload) {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let staging = store.replace_begin().unwrap();
        store
            .replace_put_root(
                &staging.staging_id,
                &json!({
                    "account": { "id": "account-1", "token": "local-token" },
                    "customBackground": "local-asset"
                }),
            )
            .unwrap();
        store.replace_put_presets(&staging.staging_id, &[]).unwrap();
        let revision = store
            .replace_commit(&staging.staging_id, Some(0))
            .unwrap()
            .revision;
        let lease = store.acquire_revision(revision).unwrap().lease;
        let prepared = store
            .prepare_official_publication(&lease, revision)
            .unwrap();
        let payload = prepared
            .create_payload(
                "account-1",
                &HashMap::from([("local-asset".to_owned(), "remote-asset".to_owned())]),
                || false,
                |_, _, _| {},
            )
            .unwrap();
        (directory, payload)
    }

    fn running_job() -> Arc<JobControl> {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::OfficialPublicationUpload).unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        job.set_phase(JobPhase::UploadingDatabase).unwrap();
        job.set_progress(JobProgress {
            completed_bytes: 1,
            total_bytes: None,
            completed_items: 1,
            total_items: Some(2),
        })
        .unwrap();
        job
    }

    fn read_request(stream: &mut std::net::TcpStream) -> RecordedRequest {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "request ended before headers completed");
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let header_text = std::str::from_utf8(&bytes[..header_end]).unwrap();
        let mut lines = header_text.split("\r\n");
        let mut request_line = lines.next().unwrap().split_whitespace();
        let method = request_line.next().unwrap().to_owned();
        let path = request_line.next().unwrap().to_owned();
        let mut headers = BTreeMap::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        let content_length = headers
            .get("content-length")
            .map(|value| value.parse::<usize>().unwrap())
            .unwrap_or(0);
        while bytes.len() - header_end < content_length {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "request ended before body completed");
            bytes.extend_from_slice(&chunk[..read]);
        }
        RecordedRequest {
            method,
            path,
            headers,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }

    fn mock_server(
        responses: Vec<MockResponse>,
    ) -> (String, std::thread::JoinHandle<Vec<RecordedRequest>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                let reason = match response.status {
                    200 => "OK",
                    304 => "Not Modified",
                    403 => "Forbidden",
                    _ => "Response",
                };
                let mut head = format!("HTTP/1.1 {} {reason}\r\n", response.status);
                for (name, value) in response.headers {
                    head.push_str(name);
                    head.push_str(": ");
                    head.push_str(&value);
                    head.push_str("\r\n");
                }
                if !head.to_ascii_lowercase().contains("content-length:") {
                    head.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
                }
                head.push_str("Connection: close\r\n\r\n");
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(&response.body).unwrap();
                stream.flush().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut closed = [0u8; 1];
                let _ = stream.read(&mut closed);
            }
            requests
        });
        (base_url, server)
    }

    #[test]
    fn start_validation_bounds_every_private_input_and_aggregate_replacements() {
        validate_start_request(&request()).unwrap();

        let mut invalid = request();
        invalid.account_id = "a".repeat(MAX_ACCOUNT_BYTES + 1);
        assert_eq!(
            validate_start_request(&invalid).unwrap_err().code,
            "invalid-input"
        );

        let mut invalid = request();
        invalid.replacements = HashMap::from([(
            "key".to_owned(),
            "v".repeat(MAX_REPLACEMENT_STRING_BYTES + 1),
        )]);
        assert_eq!(
            validate_start_request(&invalid).unwrap_err().code,
            "invalid-input"
        );

        let mut invalid = request();
        invalid.base_url = "file:///private/database".to_owned();
        assert_eq!(
            validate_start_request(&invalid).unwrap_err().code,
            "invalid-input"
        );
    }

    #[test]
    fn response_classification_keeps_exact_written_text_and_never_requires_403_bodies() {
        let written = classify_response(
            200,
            false,
            true,
            Some(br#"{"warning":"quota","reloadSession":true}"#.to_vec()),
            "account-1",
            Some("session".to_owned()),
            "date",
        )
        .unwrap();
        assert_eq!(
            written,
            OfficialPublicationAttemptResult::Written {
                account_id: "account-1".to_owned(),
                session: Some("session".to_owned()),
                save_date: "date".to_owned(),
                status: 200,
                replacement_key: r#"{"warning":"quota","reloadSession":true}"#.to_owned(),
                warning: Some("quota".to_owned()),
                reload_session: true,
            }
        );
        assert!(matches!(
            classify_response(403, false, false, None, "account-1", None, "date").unwrap(),
            OfficialPublicationAttemptResult::ReauthenticationNeeded { status: 403, .. }
        ));
        assert!(matches!(
            classify_response(403, true, false, None, "account-1", None, "date").unwrap(),
            OfficialPublicationAttemptResult::AuthWarning { status: 403, .. }
        ));
        assert!(matches!(
            classify_response(304, false, false, None, "account-1", None, "date").unwrap(),
            OfficialPublicationAttemptResult::NotModified { status: 304, .. }
        ));
    }

    #[test]
    fn forbidden_json_preserves_warning_without_changing_authentication_outcome() {
        for warning_status in [false, true] {
            for (body, expected) in [
                (
                    br#"{"warning":"quota","reloadSession":true}"#.as_slice(),
                    Some("quota"),
                ),
                (b"not-json".as_slice(), None),
                (br#"{"warning":42}"#.as_slice(), None),
            ] {
                let result = classify_response(
                    403,
                    warning_status,
                    true,
                    Some(body.to_vec()),
                    "account-1",
                    None,
                    "date",
                )
                .unwrap();
                let value = serde_json::to_value(result).unwrap();
                assert_eq!(
                    value["kind"],
                    if warning_status {
                        "auth-warning"
                    } else {
                        "reauthentication-needed"
                    }
                );
                assert_eq!(value["warning"], serde_json::to_value(expected).unwrap());
                assert!(value.get("reloadSession").is_none());
            }
        }
    }

    #[test]
    fn written_response_and_errors_are_bounded_without_echoing_credentials() {
        let error = classify_response(
            200,
            false,
            false,
            Some(vec![b'x'; MAX_REPLACEMENT_STRING_BYTES + 1]),
            "account-1",
            None,
            "date",
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid-response");
        assert!(error.message.len() <= 512);
        assert!(!error.message.contains("secret"));
    }

    #[test]
    fn publication_progress_is_monotonic_across_repeated_attempts() {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::OfficialPublicationUpload).unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        job.set_phase(JobPhase::UploadingDatabase).unwrap();
        record_uploaded_bytes(&job, 10).unwrap();
        record_uploaded_bytes(&job, 5).unwrap();
        assert_eq!(job.status().progress.completed_bytes, 15);
        assert_eq!(job.status().progress.total_bytes, None);
        assert_eq!(job.status().progress.completed_items, 1);
        assert_eq!(job.status().progress.total_items, Some(2));
    }

    #[test]
    fn per_attempt_activity_is_independent_from_monotonic_public_progress() {
        let activity = Arc::new(AtomicU64::new(0));
        activity.fetch_add(8, Ordering::Release);
        let first = activity.load(Ordering::Acquire);
        activity.store(0, Ordering::Release);
        activity.fetch_add(3, Ordering::Release);
        assert_eq!(first, 8);
        assert_eq!(activity.load(Ordering::Acquire), 3);
        assert_eq!(CONTROL_POLL_INTERVAL, Duration::from_millis(50));
    }

    #[test]
    fn controlled_wait_observes_cancellation_and_per_attempt_idle_timeout() {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::OfficialPublicationUpload).unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        job.set_phase(JobPhase::UploadingDatabase).unwrap();
        let cancelled = Arc::clone(&job);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            cancelled.request_cancel().unwrap()
        });
        let activity = AtomicU64::new(0);
        let error = tauri::async_runtime::block_on(await_controlled(
            std::future::pending::<Result<(), reqwest::Error>>(),
            &job,
            &activity,
            Duration::from_secs(1),
        ))
        .expect_err("cancel pending response wait");
        assert_eq!(error.code, "cancelled");
        assert_eq!(canceller.join().unwrap(), CancelOutcome::Requested);

        let idle = registry.create(JobKind::OfficialPublicationUpload).unwrap();
        idle.start(JobPhase::WritingExport).unwrap();
        idle.set_phase(JobPhase::UploadingDatabase).unwrap();
        let error = tauri::async_runtime::block_on(await_controlled(
            std::future::pending::<Result<(), reqwest::Error>>(),
            &idle,
            &AtomicU64::new(0),
            Duration::from_millis(1),
        ))
        .expect_err("time out an inactive attempt");
        assert_eq!(error.code, "transport-timeout");
    }

    #[test]
    fn body_transport_error_after_cancel_is_reported_as_cancellation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let job = running_job();
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::Requested);
        let error = tauri::async_runtime::block_on(await_controlled(
            build_client()
                .unwrap()
                .get(format!("http://{address}/closed"))
                .send(),
            &job,
            &AtomicU64::new(0),
            Duration::from_secs(1),
        ))
        .expect_err("map a cancelled request error to cancellation");
        assert_eq!(error.code, "cancelled");
    }

    #[test]
    fn production_http_client_does_not_follow_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = 0usize;
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            requests += 1;
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: /followed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            stream.flush().unwrap();
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_millis(200);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.read(&mut request);
                        requests += 1;
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("redirect server failed: {error}"),
                }
            }
            requests
        });

        let response = tauri::async_runtime::block_on(
            build_client()
                .unwrap()
                .get(format!("http://{address}/start"))
                .send(),
        )
        .unwrap();
        assert_eq!(response.status().as_u16(), 302);
        assert_eq!(server.join().unwrap(), 1);
    }

    #[test]
    fn session_lookup_and_upload_preserve_exact_headers_and_payload_bytes() {
        let (_directory, payload) = publication_payload();
        let (mut expected, _) = payload.open().unwrap();
        let mut expected_body = Vec::new();
        expected.read_to_end(&mut expected_body).unwrap();
        let (base_url, server) = mock_server(vec![
            MockResponse {
                status: 200,
                headers: vec![("Content-Type", "application/json".to_owned())],
                body: br#"{"sessionNumber":"server-session"}"#.to_vec(),
            },
            MockResponse {
                status: 304,
                headers: Vec::new(),
                body: Vec::new(),
            },
        ]);
        let job = running_job();
        let result = tauri::async_runtime::block_on(upload_attempt(
            &build_client().unwrap(),
            &payload,
            &base_url,
            "account-1",
            None,
            "save-date",
            &OfficialPublicationCredential::RisuAuth {
                token: "account-secret".to_owned(),
            },
            job,
        ))
        .unwrap();

        assert!(matches!(
            result,
            OfficialPublicationAttemptResult::NotModified {
                status: 304,
                session: Some(ref session),
                ..
            } if session == "server-session"
        ));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/api/account/getsessionnumber");
        assert_eq!(requests[0].headers["x-risu-auth"], "account-secret");
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].path, "/api/account/write");
        assert_eq!(requests[1].headers["x-risu-key"], DATABASE_KEY);
        assert_eq!(requests[1].headers["x-format"], "nocheck");
        assert_eq!(requests[1].headers["x-risu-session"], "server-session");
        assert_eq!(requests[1].headers["x-risu-save-date"], "save-date");
        assert_eq!(
            requests[1].headers["content-type"],
            "application/octet-stream"
        );
        assert_eq!(
            requests[1].headers["content-length"],
            expected_body.len().to_string()
        );
        assert_eq!(requests[1].body, expected_body);
        payload.cleanup().unwrap();
    }

    #[test]
    fn same_length_payload_mutation_is_rejected_before_any_upload() {
        let (directory, payload) = publication_payload();
        let payload_path = std::fs::read_dir(directory.path().join("persistent").join("exports"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "risudat")
            })
            .expect("managed publication payload");
        let mut mutated = std::fs::read(&payload_path).unwrap();
        mutated[0] ^= 0xff;
        std::fs::write(&payload_path, &mutated).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());

        let error = tauri::async_runtime::block_on(upload_attempt(
            &build_client().unwrap(),
            &payload,
            &base_url,
            "account-1",
            Some("session".to_owned()),
            "date",
            &OfficialPublicationCredential::RisuAuth {
                token: "token".to_owned(),
            },
            running_job(),
        ))
        .expect_err("same-length payload mutation must be rejected");

        assert_eq!(error.code, "invalid-source");
        assert_eq!(
            error.message,
            "official publication payload content changed"
        );
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        payload.cleanup().unwrap();
    }

    #[test]
    fn repeated_ordinary_403_reuses_exact_payload_with_fresh_private_headers() {
        let (_directory, payload) = publication_payload();
        let (base_url, server) = mock_server(vec![
            MockResponse {
                status: 403,
                headers: Vec::new(),
                body: Vec::new(),
            },
            MockResponse {
                status: 304,
                headers: Vec::new(),
                body: Vec::new(),
            },
        ]);
        let client = build_client().unwrap();
        let job = running_job();
        let first = tauri::async_runtime::block_on(upload_attempt(
            &client,
            &payload,
            &base_url,
            "account-1",
            Some("stale-session".to_owned()),
            "stale-date",
            &OfficialPublicationCredential::RisuAuth {
                token: "stale-token".to_owned(),
            },
            Arc::clone(&job),
        ))
        .unwrap();
        assert!(matches!(
            first,
            OfficialPublicationAttemptResult::ReauthenticationNeeded { .. }
        ));
        let second = tauri::async_runtime::block_on(upload_attempt(
            &client,
            &payload,
            &base_url,
            "account-1",
            Some("fresh-session".to_owned()),
            "fresh-date",
            &OfficialPublicationCredential::RisuAuth {
                token: "fresh-token".to_owned(),
            },
            job,
        ))
        .unwrap();
        assert!(matches!(
            second,
            OfficialPublicationAttemptResult::NotModified { .. }
        ));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body, requests[1].body);
        assert_eq!(requests[0].headers["x-risu-session"], "stale-session");
        assert_eq!(requests[0].headers["x-risu-save-date"], "stale-date");
        assert_eq!(requests[0].headers["x-risu-auth"], "stale-token");
        assert_eq!(requests[1].headers["x-risu-session"], "fresh-session");
        assert_eq!(requests[1].headers["x-risu-save-date"], "fresh-date");
        assert_eq!(requests[1].headers["x-risu-auth"], "fresh-token");
        payload.cleanup().unwrap();
    }

    #[test]
    fn cancellation_after_payload_eof_interrupts_response_header_wait() {
        let (_directory, payload) = publication_payload();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (received, wait_for_eof) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            received.send(request.body.len()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            let _ = stream.write_all(
                b"HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        });
        let job = running_job();
        let cancelling_job = Arc::clone(&job);
        let cancellation = std::thread::spawn(move || {
            assert!(wait_for_eof.recv().unwrap() > 0);
            assert_eq!(
                cancelling_job.request_cancel().unwrap(),
                CancelOutcome::Requested
            );
        });

        let error = tauri::async_runtime::block_on(upload_attempt(
            &build_client().unwrap(),
            &payload,
            &base_url,
            "account-1",
            Some("session".to_owned()),
            "date",
            &OfficialPublicationCredential::RisuAuth {
                token: "token".to_owned(),
            },
            job,
        ))
        .expect_err("cancel while waiting for response headers after upload EOF");

        assert_eq!(error.code, "cancelled");
        cancellation.join().unwrap();
        server.join().unwrap();
        payload.cleanup().unwrap();
    }

    #[test]
    fn cancellation_interrupts_a_stalled_success_response_body() {
        let (_directory, payload) = publication_payload();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (started_body, wait_for_body) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 10\r\nConnection: close\r\n\r\nk",
                )
                .unwrap();
            stream.flush().unwrap();
            started_body.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            let _ = stream.write_all(b"ey-part-2");
        });
        let job = running_job();
        let cancelling_job = Arc::clone(&job);
        let cancellation = std::thread::spawn(move || {
            wait_for_body.recv().unwrap();
            assert_eq!(
                cancelling_job.request_cancel().unwrap(),
                CancelOutcome::Requested
            );
        });

        let error = tauri::async_runtime::block_on(upload_attempt(
            &build_client().unwrap(),
            &payload,
            &base_url,
            "account-1",
            Some("session".to_owned()),
            "date",
            &OfficialPublicationCredential::RisuAuth {
                token: "token".to_owned(),
            },
            job,
        ))
        .expect_err("cancel while waiting for the response body");

        assert_eq!(error.code, "cancelled");
        cancellation.join().unwrap();
        server.join().unwrap();
        payload.cleanup().unwrap();
    }

    #[test]
    fn upload_reads_json_warnings_for_both_403_authentication_outcomes() {
        for warning_status in [false, true] {
            let (_directory, payload) = publication_payload();
            let mut headers = vec![("Content-Type", "application/json; charset=utf-8".to_owned())];
            if warning_status {
                headers.push(("x-risu-status", "warn".to_owned()));
            }
            let (base_url, server) = mock_server(vec![MockResponse {
                status: 403,
                headers,
                body: br#"{"warning":"quota","reloadSession":true}"#.to_vec(),
            }]);
            let result = tauri::async_runtime::block_on(upload_attempt(
                &build_client().unwrap(),
                &payload,
                &base_url,
                "account-1",
                Some("session".to_owned()),
                "date",
                &OfficialPublicationCredential::RisuAuth {
                    token: "token".to_owned(),
                },
                running_job(),
            ))
            .unwrap();
            let value = serde_json::to_value(result).unwrap();
            assert_eq!(value["warning"], "quota");
            assert_eq!(
                value["kind"],
                if warning_status {
                    "auth-warning"
                } else {
                    "reauthentication-needed"
                }
            );
            assert!(value.get("reloadSession").is_none());
            assert_eq!(server.join().unwrap().len(), 1);
            payload.cleanup().unwrap();
        }
    }

    #[test]
    fn warning_403_returns_without_draining_its_declared_body() {
        let (_directory, payload) = publication_payload();
        let (base_url, server) = mock_server(vec![MockResponse {
            status: 403,
            headers: vec![
                ("x-risu-status", "warn".to_owned()),
                ("Content-Length", (8 * 1024 * 1024).to_string()),
            ],
            body: Vec::new(),
        }]);
        let result = tauri::async_runtime::block_on(upload_attempt(
            &build_client().unwrap(),
            &payload,
            &base_url,
            "account-1",
            Some("session".to_owned()),
            "date",
            &OfficialPublicationCredential::RisuAuth {
                token: "token".to_owned(),
            },
            running_job(),
        ))
        .unwrap();
        assert!(matches!(
            result,
            OfficialPublicationAttemptResult::AuthWarning { status: 403, .. }
        ));
        assert_eq!(server.join().unwrap().len(), 1);
        payload.cleanup().unwrap();
    }
}
