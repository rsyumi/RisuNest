use super::{Result, SyncError};
use reqwest::{blocking::Client, Method, Url};
use risunest_sync_wire::{canonical, RemoteHead, MAX_METADATA_BYTES};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    io::Read,
    sync::{atomic::AtomicU32, Arc, RwLock},
    time::{Duration, Instant, SystemTime},
};

const RETRY_BUDGET: Duration = Duration::from_secs(5 * 60);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;
type Sleeper = Arc<dyn Fn(Duration) + Send + Sync>;

pub(crate) struct RetryBudget {
    attempt: AtomicU32,
    spent: std::sync::Mutex<Duration>,
    cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
    clock: Clock,
    sleep: Sleeper,
}

impl RetryBudget {
    fn new(cancelled: Option<Arc<std::sync::atomic::AtomicBool>>) -> Self {
        Self::with_driver(
            cancelled,
            Arc::new(Instant::now),
            Arc::new(std::thread::sleep),
        )
    }

    pub(crate) fn with_driver(
        cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
        clock: Clock,
        sleep: Sleeper,
    ) -> Self {
        Self {
            attempt: AtomicU32::new(0),
            spent: std::sync::Mutex::new(Duration::ZERO),
            cancelled,
            clock,
            sleep,
        }
    }

    fn ensure_active(&self) -> Result<()> {
        if self
            .cancelled
            .as_ref()
            .is_some_and(|value| value.load(std::sync::atomic::Ordering::Acquire))
        {
            Err(SyncError::new("cancelled", 409))
        } else {
            Ok(())
        }
    }

    fn remaining(&self) -> Result<Duration> {
        let spent = self
            .spent
            .lock()
            .map_err(|_| SyncError::new("sync-retry-state-unavailable", 503))?;
        Ok(RETRY_BUDGET.saturating_sub(*spent))
    }

    fn wait(&self, retry_after: Option<Duration>, failed_for: Duration) -> Result<()> {
        self.ensure_active()?;
        let attempt = self
            .attempt
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let exponent = attempt.min(5);
        let base = Duration::from_secs((1u64 << exponent).min(30));
        let requested = retry_after.unwrap_or(Duration::ZERO).min(MAX_RETRY_DELAY);
        let delay = jitter(base.max(requested), attempt).min(MAX_RETRY_DELAY);
        {
            let mut spent = self
                .spent
                .lock()
                .map_err(|_| SyncError::new("sync-retry-state-unavailable", 503))?;
            let required = failed_for.saturating_add(delay);
            if required > RETRY_BUDGET.saturating_sub(*spent) {
                return Err(SyncError::new("sync-retry-budget-exhausted", 503));
            }
            *spent += required;
        }
        let deadline = (self.clock)() + delay;
        loop {
            self.ensure_active()?;
            let remaining = deadline.saturating_duration_since((self.clock)());
            if remaining.is_zero() {
                return Ok(());
            }
            (self.sleep)(remaining.min(Duration::from_millis(100)));
        }
    }

    fn charge(&self, elapsed: Duration) -> Result<()> {
        self.ensure_active()?;
        let mut spent = self
            .spent
            .lock()
            .map_err(|_| SyncError::new("sync-retry-state-unavailable", 503))?;
        if elapsed > RETRY_BUDGET.saturating_sub(*spent) {
            return Err(SyncError::new("sync-retry-budget-exhausted", 503));
        }
        *spent += elapsed;
        Ok(())
    }
}

fn jitter(delay: Duration, attempt: u32) -> Duration {
    if delay.is_zero() {
        return delay;
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    let extra_millis = delay.as_millis() as u64 / 5;
    delay
        + Duration::from_millis(
            nanos.wrapping_add(u64::from(attempt) * 7919) % (extra_millis.saturating_add(1)),
        )
}

pub(crate) use risunest_sync_connect::Registration as ServerConfig;
pub(crate) struct ServerClient {
    http: Client,
    url: RwLock<Url>,
    config: RwLock<ServerConfig>,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    retry_budget: Arc<RetryBudget>,
    pub(crate) verified_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    pub(crate) retryable_failure: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
}
pub(crate) struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_range: Option<String>,
    pub retry_after: Option<Duration>,
    pub attempt_duration: Duration,
}
pub(crate) enum RequestAttempt {
    Response(Reply),
    Failure {
        error: SyncError,
        attempt_duration: Duration,
    },
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Identity {
    head: risunest_sync_wire::RemoteHead,
    device_id: String,
    operation_watermark: risunest_sync_wire::Sequence,
    operation_pending: bool,
}
impl ServerClient {
    /// Resolve only at an identity GET boundary. Never replay a mutation on a new URL.
    pub fn resolve_identity(&self, new_registration: bool) -> Result<RemoteHead> {
        let verify = |client: &Self| -> Result<RemoteHead> {
            let identity = client.identity()?;
            if new_registration
                && (identity.operation_watermark != 0.into() || identity.operation_pending)
            {
                return Err(SyncError::new("new-device-registration-required", 409));
            }
            Ok(identity.head)
        };
        let original = match verify(self) {
            Ok(head) => return Ok(head),
            Err(error) => error,
        };
        if matches!(original.status, 401 | 403 | 409)
            || matches!(
                original.code.as_str(),
                "cancelled" | "new-device-registration-required"
            )
        {
            return Err(original);
        }
        let current = self.config();
        let Some(directory) = &current.directory else {
            return Err(original);
        };
        self.ensure_active()?;
        // A separate unauthenticated request. Device and library headers are never sent.
        let response = self
            .http
            .get(directory.record_url()?)
            .header("accept-encoding", "identity")
            .header("cache-control", "no-cache")
            .timeout(Duration::from_secs(15))
            .send()
            .map_err(|_| SyncError::new("directory-unreachable", 503))?;
        if response.status().as_u16() != 200 {
            return Err(SyncError::new("directory-response-error", 503));
        }
        let limit = risunest_sync_connect::MAX_ENVELOPE_TEXT;
        if response
            .headers()
            .get("content-encoding")
            .is_some_and(|v| v != "identity")
            || response.content_length().is_some_and(|v| v > limit as u64)
        {
            return Err(SyncError::new("invalid-directory-envelope", 502));
        }
        let mut bytes = Vec::new();
        response
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| SyncError::new("directory-incomplete-response", 503))?;
        if bytes.len() > limit {
            return Err(SyncError::new("invalid-directory-envelope", 502));
        }
        let envelope = std::str::from_utf8(&bytes)
            .map_err(|_| SyncError::new("invalid-directory-envelope", 502))?;
        let endpoint =
            risunest_sync_connect::open_endpoint(&directory.uuid, &directory.key, envelope)?;
        if risunest_sync_connect::validate_endpoint(&endpoint, false)?
            == *self.url.read().unwrap_or_else(|error| error.into_inner())
        {
            return Err(original);
        }
        self.ensure_active()?;
        let mut config = current;
        config.endpoint = endpoint;
        let candidate_config = config.clone();
        let mut candidate = Self::with_cancellation(config, self.cancelled.clone())?;
        candidate.http = self.http.clone();
        candidate.retry_budget = self.retry_budget.clone();
        candidate.verified_bytes = self.verified_bytes.clone();
        candidate.retryable_failure = self.retryable_failure.clone();
        let head = verify(&candidate)?;
        *self.url.write().unwrap_or_else(|error| error.into_inner()) = candidate
            .url
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        *self
            .config
            .write()
            .unwrap_or_else(|error| error.into_inner()) = candidate_config;
        Ok(head)
    }
    pub fn config(&self) -> ServerConfig {
        self.config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    fn identity(&self) -> Result<Identity> {
        let reply = self.request_with_policy(
            Method::GET,
            "session",
            &[],
            None,
            &[],
            MAX_METADATA_BYTES,
            false,
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(response_error(reply));
        }
        let identity: Identity = canonical::decode(&reply.body, MAX_METADATA_BYTES)?;
        identity.head.validate()?;
        let config = self.config();
        if identity.head.library_id != config.library_id || identity.device_id != config.device_id {
            return Err(SyncError::new("device-identity-mismatch", 409));
        }
        Ok(identity)
    }
    pub fn new(config: ServerConfig) -> Result<Self> {
        Self::with_cancellation(config, None)
    }
    pub fn with_cancellation(
        config: ServerConfig,
        cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Self> {
        let retry_budget = Arc::new(RetryBudget::new(cancelled.clone()));
        Self::with_retry_budget(config, cancelled, retry_budget)
    }

    pub(crate) fn with_retry_budget(
        config: ServerConfig,
        cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        retry_budget: Arc<RetryBudget>,
    ) -> Result<Self> {
        let url = config.validate()?;
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| SyncError::new("http-client-unavailable", 503))?;
        Ok(Self {
            http,
            url: RwLock::new(url),
            config: RwLock::new(config),
            cancelled,
            retry_budget,
            verified_bytes: None,
            retryable_failure: None,
        })
    }
    pub(crate) fn retry_budget(&self) -> Arc<RetryBudget> {
        self.retry_budget.clone()
    }
    pub fn verified(&self, bytes: u64) {
        if let Some(counter) = &self.verified_bytes {
            counter.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        }
    }
    pub fn report_retryable_failure(&self, code: Option<&str>) {
        let Some(progress) = &self.retryable_failure else {
            return;
        };
        let sanitized = sanitize_retryable_failure(code);
        if let Ok(mut current) = progress.lock() {
            *current = sanitized;
        }
    }
    pub fn ensure_active(&self) -> Result<()> {
        if self
            .cancelled
            .as_ref()
            .is_some_and(|v| v.load(std::sync::atomic::Ordering::Acquire))
        {
            Err(SyncError::new("cancelled", 409))
        } else {
            Ok(())
        }
    }
    pub fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Vec<u8>>,
        headers: &[(&str, String)],
        limit: usize,
    ) -> Result<Reply> {
        self.request_with_policy(method, path, query, body, headers, limit, true)
    }

    pub(crate) fn request_ambiguous_mutation(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        headers: &[(&str, String)],
        limit: usize,
    ) -> Result<RequestAttempt> {
        loop {
            let attempted = (self.retry_budget.clock)();
            let result = self.request_once(method.clone(), path, &[], body.clone(), headers, limit);
            let elapsed = (self.retry_budget.clock)().saturating_duration_since(attempted);
            match result {
                Ok(mut reply) => {
                    reply.attempt_duration = elapsed;
                    let maintenance = reply.status == 503
                        && response_code(&reply).as_deref() == Some("server-updating");
                    if maintenance {
                        self.report_retryable_failure(Some("server-updating"));
                        self.retry_budget.wait(reply.retry_after, elapsed)?;
                        continue;
                    }
                    return Ok(RequestAttempt::Response(reply));
                }
                Err(error) => {
                    return Ok(RequestAttempt::Failure {
                        error,
                        attempt_duration: elapsed,
                    })
                }
            }
        }
    }

    fn request_with_policy(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Vec<u8>>,
        headers: &[(&str, String)],
        limit: usize,
        retry_generic: bool,
    ) -> Result<Reply> {
        let replay_safe = replay_safe(&method, path);
        let mut recovering = false;
        loop {
            let attempted = (self.retry_budget.clock)();
            let result =
                self.request_once(method.clone(), path, query, body.clone(), headers, limit);
            match result {
                Ok(mut reply) => {
                    let elapsed = (self.retry_budget.clock)().saturating_duration_since(attempted);
                    reply.attempt_duration = elapsed;
                    let code = response_code(&reply);
                    let maintenance =
                        reply.status == 503 && code.as_deref() == Some("server-updating");
                    let transient = matches!(reply.status, 502 | 503 | 504);
                    if maintenance || (retry_generic && replay_safe && transient) {
                        self.report_retryable_failure(if maintenance {
                            Some("server-updating")
                        } else {
                            Some("server-unreachable")
                        });
                        if !maintenance {
                            let _ = self.resolve_identity(false);
                        }
                        recovering = true;
                        self.retry_budget.wait(reply.retry_after, elapsed)?;
                        continue;
                    }
                    if recovering {
                        self.report_retryable_failure(None);
                    }
                    return Ok(reply);
                }
                Err(error) if retry_generic && replay_safe && is_ambiguous_transient(&error) => {
                    self.report_retryable_failure(Some(&error.code));
                    let _ = self.resolve_identity(false);
                    recovering = true;
                    self.retry_budget.wait(
                        None,
                        (self.retry_budget.clock)().saturating_duration_since(attempted),
                    )?;
                }
                Err(error) => {
                    if recovering {
                        self.report_retryable_failure(None);
                    }
                    return Err(error);
                }
            }
        }
    }

    fn request_once(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Vec<u8>>,
        headers: &[(&str, String)],
        limit: usize,
    ) -> Result<Reply> {
        self.ensure_active()?;
        if path.starts_with('/') || path.contains("..") || path.contains('?') || path.contains('#')
        {
            return Err(SyncError::new("invalid-request-path", 400));
        }
        let base_url = self
            .url
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let url = base_url
            .join(path)
            .map_err(|_| SyncError::new("invalid-request-path", 400))?;
        if url.origin() != base_url.origin() {
            return Err(SyncError::new("invalid-request-origin", 400));
        }
        let config = self.config();
        let ordinary_timeout = if path == "objects/transfer"
            || path == "uploads/frames"
            || path.starts_with("object-deltas/")
            || (path.starts_with("uploads/") && path.ends_with("/delta"))
        {
            Duration::from_secs(120)
        } else {
            Duration::from_secs(30)
        };
        let timeout = self.retry_budget.remaining()?.min(ordinary_timeout);
        if timeout.is_zero() {
            return Err(SyncError::new("sync-retry-budget-exhausted", 503));
        }
        let mut request = self
            .http
            .request(method, url)
            .query(query)
            .bearer_auth(&config.token)
            .header("x-risu-library", &config.library_id)
            .header("accept-encoding", "identity")
            .timeout(timeout);
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        if let Some(body) = body {
            if body.len() > 8 * 1024 * 1024 {
                return Err(SyncError::new("request-too-large", 413));
            }
            request = request.body(body);
        }
        let response = request.send().map_err(|e| {
            SyncError::new(
                if e.is_timeout() {
                    "server-timeout"
                } else {
                    "server-unreachable"
                },
                503,
            )
        })?;
        let status = response.status().as_u16();
        let content_range = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after);
        if response
            .headers()
            .get("content-encoding")
            .is_some_and(|v| v != "identity")
        {
            return Err(SyncError::new("unexpected-content-encoding", 502));
        }
        if response.content_length().is_some_and(|v| v > limit as u64) {
            return Err(SyncError::new("response-too-large", 502));
        }
        let mut bytes = Vec::new();
        response
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| SyncError::new("incomplete-response", 503))?;
        if bytes.len() > limit {
            return Err(SyncError::new("response-too-large", 502));
        }
        Ok(Reply {
            status,
            body: bytes,
            content_range,
            retry_after,
            attempt_duration: Duration::ZERO,
        })
    }

    pub(crate) fn wait_after_ambiguous(
        &self,
        error: &SyncError,
        attempt_duration: Duration,
    ) -> Result<()> {
        if !is_ambiguous_transient(error) {
            return Err(SyncError::new("non-transient-retry-requested", 409));
        }
        self.report_retryable_failure(Some(&error.code));
        self.retry_budget.wait(None, attempt_duration)
    }

    pub(crate) fn wait_transient_response(
        &self,
        retry_after: Option<Duration>,
        code: &str,
        attempt_duration: Duration,
    ) -> Result<()> {
        self.report_retryable_failure(Some(code));
        self.retry_budget.wait(retry_after, attempt_duration)
    }

    pub(crate) fn charge_retry_resolution(&self, elapsed: Duration) -> Result<()> {
        self.retry_budget.charge(elapsed)
    }
    pub fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&impl Serialize>,
        headers: &[(&str, String)],
    ) -> Result<(u16, T)> {
        let reply = self.request(
            method,
            path,
            query,
            body.map(canonical::encode).transpose()?,
            headers,
            MAX_METADATA_BYTES,
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(response_error(reply));
        }
        Ok((
            reply.status,
            canonical::decode(&reply.body, MAX_METADATA_BYTES)?,
        ))
    }
    pub fn head(&self) -> Result<RemoteHead> {
        let (_, head): (u16, RemoteHead) = self.json(Method::GET, "head", &[], None::<&()>, &[])?;
        head.validate()?;
        if head.library_id != self.config().library_id {
            return Err(SyncError::new("library-mismatch", 409));
        }
        Ok(head)
    }
}

fn response_code(reply: &Reply) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(&reply.body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|code| code.as_str())
                .map(str::to_owned)
        })
        .filter(|code| {
            code.len() <= 64
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
        })
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds).min(MAX_RETRY_DELAY));
    }
    let requested = httpdate::parse_http_date(value).ok()?;
    Some(
        requested
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO)
            .min(MAX_RETRY_DELAY),
    )
}

fn replay_safe(method: &Method, path: &str) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::DELETE) {
        return true;
    }
    if *method == Method::PUT {
        return path.contains("/chunks/")
            || path.contains("/pages/")
            || (path.starts_with("uploads/") && path.ends_with("/delta"));
    }
    *method == Method::POST
        && (matches!(
            path,
            "objects/missing"
                | "objects/transfer"
                | "uploads/frames"
                | "objects/pins"
                | "objects/retention/release"
                | "acks"
        ) || (path.starts_with("uploads/") && path.ends_with("/complete"))
            || (path.starts_with("staged-changes/") && path.ends_with("/seal")))
}

pub(crate) fn is_ambiguous_transient(error: &SyncError) -> bool {
    error.status == 503
        && matches!(
            error.code.as_str(),
            "server-timeout" | "server-unreachable" | "incomplete-response"
        )
}

fn sanitize_retryable_failure(code: Option<&str>) -> Option<String> {
    code.map(|value| {
        if value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
        {
            value.to_owned()
        } else {
            "server-response-error".into()
        }
    })
}

#[cfg(test)]
mod progress_tests {
    use super::sanitize_retryable_failure;

    #[test]
    fn retryable_failure_progress_exposes_only_a_bounded_error_code() {
        assert_eq!(
            sanitize_retryable_failure(Some("storage-io")).as_deref(),
            Some("storage-io")
        );
        let oversized = "x".repeat(65);
        for private in [
            "token=synthetic-secret",
            "Response Body",
            oversized.as_str(),
        ] {
            assert_eq!(
                sanitize_retryable_failure(Some(private)).as_deref(),
                Some("server-response-error")
            );
        }
        assert_eq!(sanitize_retryable_failure(None), None);
    }
}
pub(crate) fn response_error(reply: Reply) -> SyncError {
    let value: Option<serde_json::Value> = serde_json::from_slice(&reply.body).ok();
    let code = value
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|v| v.as_str())
        .filter(|s| s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
        .unwrap_or("server-response-error");
    SyncError::new(code, reply.status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn no_sleep_budget() -> Arc<RetryBudget> {
        let current = Arc::new(Mutex::new(Instant::now()));
        let clock_state = current.clone();
        let sleep_state = current;
        Arc::new(RetryBudget::with_driver(
            None,
            Arc::new(move || *clock_state.lock().unwrap()),
            Arc::new(move |duration| *sleep_state.lock().unwrap() += duration),
        ))
    }
    fn config(endpoint: &str) -> ServerConfig {
        ServerConfig {
            directory: None,
            endpoint: endpoint.into(),
            library_id: "library".into(),
            device_id: "device".into(),
            token: "a".repeat(64),
        }
    }
    #[test]
    fn fixed_endpoint_requires_https_except_loopback_and_rejects_embedded_credentials() {
        for url in [
            "https://sync.example/base",
            "http://127.0.0.1:4319",
            "http://[::1]:4319",
        ] {
            assert!(config(url).validate().is_ok());
        }
        for url in [
            "http://192.168.0.1",
            "http://sync.example",
            "https://name:secret@sync.example",
            "https://sync.example/?token=x",
            "https://sync.example/#fragment",
            "file:///tmp",
        ] {
            assert!(config(url).validate().is_err());
        }
    }

    #[test]
    fn retry_policy_only_replays_reads_and_identity_bound_mutations() {
        assert!(replay_safe(&Method::GET, "head"));
        assert!(replay_safe(&Method::PUT, "staged-changes/stage/pages/0"));
        assert!(replay_safe(&Method::POST, "uploads/id/complete"));
        assert!(replay_safe(&Method::POST, "uploads/frames"));
        assert!(replay_safe(&Method::POST, "objects/transfer"));
        assert!(!replay_safe(&Method::POST, "uploads/batch"));
        assert!(!replay_safe(&Method::POST, "objects/batch"));
        assert!(!replay_safe(&Method::POST, "commits"));
        assert!(!replay_safe(&Method::POST, "uploads"));
        assert!(!replay_safe(&Method::POST, "staged-changes/start"));
        assert!(!replay_safe(&Method::POST, "staged-changes"));
    }

    #[test]
    fn retry_budget_uses_bounded_backoff_retry_after_and_total_budget() {
        let current = Arc::new(Mutex::new(Instant::now()));
        let clock_state = current.clone();
        let sleep_state = current.clone();
        let budget = RetryBudget::with_driver(
            None,
            Arc::new(move || *clock_state.lock().unwrap()),
            Arc::new(move |duration| *sleep_state.lock().unwrap() += duration),
        );
        *current.lock().unwrap() += Duration::from_secs(60 * 60);
        let before = *current.lock().unwrap();
        budget.wait(None, Duration::ZERO).unwrap();
        let first = current.lock().unwrap().duration_since(before);
        assert!((Duration::from_secs(1)..=Duration::from_millis(1200)).contains(&first));
        let before = *current.lock().unwrap();
        budget.wait(None, Duration::ZERO).unwrap();
        let second = current.lock().unwrap().duration_since(before);
        assert!((Duration::from_secs(2)..=Duration::from_millis(2400)).contains(&second));

        let capped_now = Arc::new(Mutex::new(Instant::now()));
        let clock_state = capped_now.clone();
        let sleep_state = capped_now.clone();
        let capped = RetryBudget::with_driver(
            None,
            Arc::new(move || *clock_state.lock().unwrap()),
            Arc::new(move |duration| *sleep_state.lock().unwrap() += duration),
        );
        let before = *capped_now.lock().unwrap();
        capped
            .wait(Some(Duration::from_secs(90)), Duration::ZERO)
            .unwrap();
        assert_eq!(
            capped_now.lock().unwrap().duration_since(before),
            MAX_RETRY_DELAY
        );
        *capped.spent.lock().unwrap() = RETRY_BUDGET;
        assert_eq!(
            capped.wait(None, Duration::ZERO).unwrap_err().code,
            "sync-retry-budget-exhausted"
        );
        *capped.spent.lock().unwrap() = RETRY_BUDGET - Duration::from_millis(250);
        assert_eq!(
            capped.charge(Duration::from_millis(251)).unwrap_err().code,
            "sync-retry-budget-exhausted"
        );
        assert_eq!(
            *capped.spent.lock().unwrap(),
            RETRY_BUDGET - Duration::from_millis(250)
        );
    }

    #[test]
    fn cancellation_interrupts_a_retry_wait() {
        let current = Arc::new(Mutex::new(Instant::now()));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let clock_state = current.clone();
        let sleep_state = current.clone();
        let cancel_on_sleep = cancelled.clone();
        let budget = RetryBudget::with_driver(
            Some(cancelled),
            Arc::new(move || *clock_state.lock().unwrap()),
            Arc::new(move |duration| {
                *sleep_state.lock().unwrap() += duration;
                cancel_on_sleep.store(true, std::sync::atomic::Ordering::Release);
            }),
        );
        assert_eq!(
            budget.wait(None, Duration::ZERO).unwrap_err().code,
            "cancelled"
        );
    }

    #[test]
    fn explicit_maintenance_retries_an_unadmitted_mutation_with_retry_after() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.server_addr());
        let task = std::thread::spawn(move || {
            let mut request = server.recv().unwrap();
            assert_eq!(request.method(), &tiny_http::Method::Post);
            std::io::copy(request.as_reader(), &mut std::io::sink()).unwrap();
            request
                .respond(
                    tiny_http::Response::from_string(r#"{"error":"server-updating"}"#)
                        .with_status_code(503)
                        .with_header(tiny_http::Header::from_bytes("Retry-After", "45").unwrap()),
                )
                .unwrap();
            let mut request = server.recv().unwrap();
            assert_eq!(request.method(), &tiny_http::Method::Post);
            std::io::copy(request.as_reader(), &mut std::io::sink()).unwrap();
            request.respond(tiny_http::Response::empty(201)).unwrap();
        });
        let mut client = ServerClient::new(config(&endpoint)).unwrap();
        client.retry_budget = no_sleep_budget();
        let reply = client
            .request(
                Method::POST,
                "uploads",
                &[],
                Some(b"synthetic".to_vec()),
                &[],
                MAX_METADATA_BYTES,
            )
            .unwrap();
        assert_eq!(reply.status, 201);
        assert_eq!(*client.retry_budget.spent.lock().unwrap(), MAX_RETRY_DELAY);
        task.join().unwrap();
    }

    #[test]
    fn generic_transient_does_not_blindly_replay_an_unidentified_mutation() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.server_addr());
        let task = std::thread::spawn(move || {
            let mut request = server.recv().unwrap();
            std::io::copy(request.as_reader(), &mut std::io::sink()).unwrap();
            request
                .respond(tiny_http::Response::from_string("upstream").with_status_code(503))
                .unwrap();
            assert!(server
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_none());
        });
        let mut client = ServerClient::new(config(&endpoint)).unwrap();
        client.retry_budget = no_sleep_budget();
        let reply = client
            .request(
                Method::POST,
                "uploads",
                &[],
                Some(b"synthetic".to_vec()),
                &[],
                MAX_METADATA_BYTES,
            )
            .unwrap();
        assert_eq!(reply.status, 503);
        assert_eq!(*client.retry_budget.spent.lock().unwrap(), Duration::ZERO);
        task.join().unwrap();
    }
}

#[cfg(test)]
#[path = "directory_tests.rs"]
mod directory_tests;
