use super::{Result, SyncError};
use reqwest::{blocking::Client, Method, Url};
use risunest_sync_wire::{body as body_codec, canonical, RemoteHead, MAX_METADATA_BYTES};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    io::Read,
    sync::{atomic::AtomicU32, Arc, RwLock},
    time::{Duration, Instant, SystemTime},
};

const RETRY_BUDGET: Duration = Duration::from_secs(5 * 60);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Default)]
struct DirectoryRetries(std::collections::HashMap<String, (Instant, u32)>);
impl DirectoryRetries {
    fn admit(&mut self, record: &str, now: Instant) -> bool {
        if self.0.get(record).is_some_and(|(next, _)| now < *next) { return false; }
        if self.0.len() >= 64 && !self.0.contains_key(record) {
            if let Some(oldest) = self.0.iter().min_by_key(|(_, (next, _))| *next).map(|(key, _)| key.clone()) {
                self.0.remove(&oldest);
            }
        }
        let (_, attempt) = self.0.entry(record.to_owned()).or_insert((now, 0));
        let delay = Duration::from_secs((30 * (1u64 << (*attempt).min(4))).min(300));
        *attempt = attempt.saturating_add(1);
        let attempt = *attempt;
        self.0.insert(record.to_owned(), (now + delay, attempt));
        true
    }
}
fn directory_retries() -> &'static std::sync::Mutex<DirectoryRetries> {
    static RETRIES: std::sync::OnceLock<std::sync::Mutex<DirectoryRetries>> = std::sync::OnceLock::new();
    RETRIES.get_or_init(Default::default)
}
fn reset_directory_retry(config: &ServerConfig) {
    if let Some(record) = config.directory.as_ref().and_then(|directory| directory.record_url().ok()) {
        directory_retries().lock().unwrap_or_else(|error| error.into_inner()).0.remove(record.as_str());
    }
}

struct ObservedBodyRead<'a, R> {
    inner: R,
    position: u64,
    high_water: &'a std::sync::atomic::AtomicU64,
    observer: Option<&'a (dyn Fn(u64) + Send + Sync)>,
}
impl<R: Read> Read for ObservedBodyRead<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.position = self.position.saturating_add(count as u64);
        if let Some(observer) = self.observer {
            let previous = self.high_water.fetch_max(self.position, std::sync::atomic::Ordering::Relaxed);
            if self.position > previous { observer(self.position - previous); }
        }
        Ok(count)
    }
}

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

    fn progress(&self) {
        if let Ok(mut spent) = self.spent.lock() {
            *spent = Duration::ZERO;
            self.attempt.store(0, std::sync::atomic::Ordering::Relaxed);
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
    pub(crate) body_download_observer: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    #[cfg(test)]
    pub(crate) test_io: Option<Arc<TestIoCounters>>,
    http: Client,
    url: RwLock<Url>,
    config: RwLock<ServerConfig>,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    retry_budget: Arc<RetryBudget>,
    // Initial receives leave retries to the scheduler so plugins can start offline.
    pub(crate) retry_transient: bool,
    pub(crate) verified_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    pub(crate) retryable_failure: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    /// The cycle's activity slot. The transfer layer raises it while object
    /// bytes are on the wire and restores what it replaced afterwards.
    pub(crate) activity: Option<std::sync::Arc<std::sync::atomic::AtomicU8>>,
    /// Where this client's requests and verified bodies are counted, taken
    /// from the operation that created it.
    lane: Arc<super::progress::ProgressLane>,
}
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestIoCounters {
    pub requests: std::sync::atomic::AtomicU64,
    /// Bytes on the wire, encoded when the body was.
    pub request_body_bytes: std::sync::atomic::AtomicU64,
    pub response_body_bytes: std::sync::atomic::AtomicU64,
    /// The same bodies decoded.
    pub request_raw_bytes: std::sync::atomic::AtomicU64,
    pub response_raw_bytes: std::sync::atomic::AtomicU64,
    /// Bodies whose wire form was longer than their raw form.
    pub expanded_bodies: std::sync::atomic::AtomicU64,
    /// Sends every body raw and asks for raw replies.
    pub codec_disabled: std::sync::atomic::AtomicBool,
    pub traffic: std::sync::Mutex<TestTraffic>,
}
#[cfg(test)]
#[derive(Default, Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TestTraffic {
    pub journal_requests: u64,
    /// Wire bytes; each `*_raw_bytes` field counts the same replies decoded.
    pub journal_response_bytes: u64,
    pub journal_response_raw_bytes: u64,
    /// Decoded inline values, not additional wire bytes.
    pub journal_inline_decoded_bytes: u64,
    pub object_part_requests: u64,
    pub object_part_response_bytes: u64,
    pub object_part_response_raw_bytes: u64,
    pub object_transfer_requests: u64,
    pub object_transfer_response_bytes: u64,
    pub object_transfer_response_raw_bytes: u64,
    pub prepared_units: u64,
}
#[cfg(test)]
impl TestTraffic {
    fn response(&mut self, method: &Method, path: &str, wire: u64, bytes: &[u8]) {
        let raw = bytes.len() as u64;
        if *method == Method::GET && path == "changes" {
            self.journal_requests += 1;
            self.journal_response_bytes += wire;
            self.journal_response_raw_bytes += raw;
            if let Ok(page) = risunest_sync_wire::canonical::decode::<risunest_sync_wire::lww::ChangesPage>(bytes, risunest_sync_wire::MAX_METADATA_BYTES) {
                use base64::Engine;
                for entry in page.items {
                    if let risunest_sync_wire::unit::UnitValue::Inline { bytes } = entry.value {
                        if let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(bytes) {
                            self.journal_inline_decoded_bytes += bytes.len() as u64;
                        }
                    }
                }
            }
        } else if *method == Method::GET && path.strip_prefix("objects/").and_then(|rest| rest.strip_suffix("/part")).is_some_and(|hash| risunest_sync_wire::validate_hash(hash).is_ok()) {
            self.object_part_requests += 1;
            self.object_part_response_bytes += wire;
            self.object_part_response_raw_bytes += raw;
        } else if *method == Method::POST && path == "objects/transfer" {
            self.object_transfer_requests += 1;
            self.object_transfer_response_bytes += wire;
            self.object_transfer_response_raw_bytes += raw;
        }
    }
}
#[cfg(test)]
impl TestIoCounters {
    pub(crate) fn reset(&self) { *self.traffic.lock().unwrap() = TestTraffic::default(); for value in [&self.requests,&self.request_body_bytes,&self.response_body_bytes,&self.request_raw_bytes,&self.response_raw_bytes,&self.expanded_bodies] {value.store(0,std::sync::atomic::Ordering::Relaxed);} }
    pub(crate) fn snapshot(&self) -> [u64;3] { [self.requests.load(std::sync::atomic::Ordering::Relaxed),self.request_body_bytes.load(std::sync::atomic::Ordering::Relaxed),self.response_body_bytes.load(std::sync::atomic::Ordering::Relaxed)] }
}
#[cfg(test)]
thread_local! {
    static TEST_IO_SCOPES: std::cell::RefCell<Vec<(Vec<std::path::PathBuf>, Arc<TestIoCounters>)>> = const { std::cell::RefCell::new(Vec::new()) };
}
#[cfg(test)]
pub(crate) fn with_test_io<T>(root: &std::path::Path, counters: Arc<TestIoCounters>, operation: impl FnOnce() -> T) -> T {
    struct Scope;
    impl Drop for Scope { fn drop(&mut self) { TEST_IO_SCOPES.with(|scopes| { scopes.borrow_mut().pop(); }); } }
    let mut roots = vec![root.to_owned()];
    if let Ok(canonical) = std::fs::canonicalize(root) { roots.push(canonical); }
    TEST_IO_SCOPES.with(|scopes| scopes.borrow_mut().push((roots, counters)));
    let _scope = Scope;
    operation()
}
#[cfg(test)]
pub(crate) fn attach_test_io(root: &std::path::Path, client: &mut ServerClient) {
    client.test_io = TEST_IO_SCOPES.with(|scopes| scopes.borrow().iter().rev()
        .find(|(roots, _)| roots.iter().any(|candidate| candidate == root))
        .map(|(_, counters)| counters.clone()));
}
/// The activity id shown while frames or chunks are in flight.
pub(crate) const ACTIVITY_UPLOADING: u8 = 4;
/// Restores the activity the transfer replaced, on every exit path. Nested
/// transfers restore in reverse order, so the outermost one ends the phase.
pub(crate) struct TransferActivity<'a> {
    activity: Option<&'a std::sync::atomic::AtomicU8>,
    previous: u8,
}
impl Drop for TransferActivity<'_> {
    fn drop(&mut self) {
        if let Some(activity) = self.activity {
            activity.store(self.previous, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
pub(crate) struct Reply {
    pub status: u16,
    /// Always raw; an encoded reply is decoded before it is returned.
    pub body: Vec<u8>,
    /// `x-risu-object-range` of a part reply, in raw offsets.
    pub object_range: Option<String>,
    pub retry_after: Option<Duration>,
    pub attempt_duration: Duration,
    /// The level headers, absent from a server that predates format levels.
    pub library_format: Option<super::library_format::LibraryFormat>,
}
/// A request body ready to send: `wire` is what travels, encoded when the
/// marker is set. Every attempt reads the same bytes.
pub(crate) struct PreparedBody {
    wire: Arc<Vec<u8>>,
    log_body: Option<Vec<u8>>,
    encoded: bool,
    #[cfg(test)]
    raw_len: usize,
}
impl PreparedBody {
    /// What one attempt sends. Attempts share the prepared bytes instead of
    /// copying them.
    fn reader(&self) -> std::io::Cursor<SharedWire> {
        std::io::Cursor::new(SharedWire(self.wire.clone()))
    }
}
struct SharedWire(Arc<Vec<u8>>);
impl AsRef<[u8]> for SharedWire {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
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
    protocol_id: String,
}
impl ServerClient {
    /// Resolve only at an identity GET boundary. Never replay a mutation on a new URL.
    pub fn resolve_identity(&self) -> Result<RemoteHead> {
        let verify = |client: &Self| -> Result<RemoteHead> { Ok(client.identity()?.head) };
        let original = match verify(self) {
            Ok(head) => { reset_directory_retry(&self.config()); return Ok(head); },
            Err(error) => error,
        };
        if matches!(original.status, 401 | 403 | 409) || original.code == "cancelled" {
            return Err(original);
        }
        let current = self.config();
        let Some(directory) = &current.directory else {
            return Err(original);
        };
        self.ensure_active()?;
        let record = directory.record_url()?;
        // All lanes and short-lived clients share this admission, including internal HTTP retries.
        if !directory_retries().lock().unwrap_or_else(|error| error.into_inner()).admit(record.as_str(), Instant::now()) {
            return Err(original);
        }
        // A separate unauthenticated request. Device and library headers are never sent.
        let request = self
            .http
            .get(record)
            .header("accept-encoding", "identity")
            .header("cache-control", "no-cache")
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| SyncError::new("directory-unreachable", 503))?;
        let capture = risunest_network_log::begin("sync-directory", request.method().as_str(), request.url().as_str(), request.headers());
        let response = self.http.execute(request).map_err(|_| {
            capture.finish("error"); SyncError::new("directory-unreachable", 503)
        })?;
        capture.response(response.status().as_u16(), response.url().as_str(), response.headers());
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
        let read = response.take(limit as u64 + 1).read_to_end(&mut bytes);
        capture.append(risunest_network_log::Side::Response, &bytes);
        if read.is_err() { capture.finish("error"); }
        read.map_err(|_| SyncError::new("directory-incomplete-response", 503))?;
        capture.finish("complete");
        if bytes.len() > limit {
            return Err(SyncError::new("invalid-directory-envelope", 502));
        }
        let envelope = std::str::from_utf8(&bytes)
            .map_err(|_| SyncError::new("invalid-directory-envelope", 502))?;
        let endpoint =
            risunest_sync_connect::open_endpoint(&directory.uuid, &directory.key, envelope)?;
        if risunest_sync_connect::validate_endpoint(&endpoint)?
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
        candidate.retry_transient = self.retry_transient;
        candidate.verified_bytes = self.verified_bytes.clone();
        candidate.retryable_failure = self.retryable_failure.clone();
        candidate.activity = self.activity.clone();
        let head = verify(&candidate)?;
        reset_directory_retry(&candidate_config);
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
    /// The identity check of a connection being configured. A connection that
    /// fails before any reply, as one through a tunnel that has just started
    /// can, is tried once more after the usual retry delay.
    pub fn resolve_configured_identity(&self) -> Result<RemoteHead> {
        let attempted = (self.retry_budget.clock)();
        match self.resolve_identity() {
            Err(error) if error.code == "server-unreachable" => {
                let failed_for = (self.retry_budget.clock)().saturating_duration_since(attempted);
                self.wait_after_ambiguous(&error, failed_for)?;
                self.resolve_identity()
            }
            other => other,
        }
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
        // Checked before anything else this cycle would attempt, so an
        // incompatible peer is named instead of rejecting a later request.
        if identity.protocol_id != risunest_sync_wire::PROTOCOL_ID {
            return Err(SyncError::new("server-incompatible", 409));
        }
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
        let http = shared_http()?;
        Ok(Self {
            body_download_observer: None,
            #[cfg(test)]
            test_io: None,
            http,
            url: RwLock::new(url),
            config: RwLock::new(config),
            cancelled,
            retry_budget,
            retry_transient: true,
            verified_bytes: None,
            retryable_failure: None,
            activity: None,
            lane: super::progress::current().unwrap_or_default(),
        })
    }
    /// A client for the same server that ignores this one's cancellation, for
    /// releasing what a cancelled operation still holds on the server.
    pub(crate) fn uncancelled(&self) -> Result<Self> {
        let client = Self { lane: self.lane.clone(), ..Self::with_cancellation(self.config(), None)? };
        #[cfg(test)]
        let client = Self { test_io: self.test_io.clone(), ..client };
        Ok(client)
    }
    pub(crate) fn retry_budget(&self) -> Arc<RetryBudget> {
        self.retry_budget.clone()
    }
    pub(crate) fn progress(&self) {
        self.retry_budget.progress();
        self.report_retryable_failure(None);
    }
    pub fn verified(&self, bytes: u64) {
        self.progress();
        self.lane.file_done(bytes);
        if let Some(counter) = &self.verified_bytes {
            counter.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        }
    }
    pub(crate) fn lane(&self) -> &super::progress::ProgressLane {
        &self.lane
    }
    pub(crate) fn transferring(&self) -> TransferActivity<'_> {
        match self.activity.as_deref() {
            Some(activity) => TransferActivity {
                previous: activity
                    .swap(ACTIVITY_UPLOADING, std::sync::atomic::Ordering::Relaxed),
                activity: Some(activity),
            },
            None => TransferActivity {
                activity: None,
                previous: 0,
            },
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
        let body = self.prepare_body(body)?;
        let operation_id = uuid::Uuid::new_v4();
        let download_high_water = std::sync::atomic::AtomicU64::new(0);
        loop {
            let attempted = (self.retry_budget.clock)();
            let result = self.request_once(method.clone(), path, &[], body.as_ref(), headers, limit, operation_id, &download_high_water);
            let elapsed = (self.retry_budget.clock)().saturating_duration_since(attempted);
            match result {
                Ok(mut reply) => {
                    reply.attempt_duration = elapsed;
                    let maintenance = reply.status == 503
                        && response_code(&reply).as_deref() == Some("server-updating");
                    if maintenance {
                        self.report_retryable_failure(Some("server-updating"));
                        self.retry_budget.wait(
                            reply.retry_after,
                            (self.retry_budget.clock)().saturating_duration_since(attempted),
                        )?;
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

    pub(crate) fn frame_request(&self, body: Vec<u8>) -> Result<Reply> {
        self.request_with_policy(
            Method::POST,
            "uploads/frames",
            &[],
            Some(body),
            &[],
            MAX_METADATA_BYTES,
            false,
        )
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
        let body = self.prepare_body(body)?;
        let operation_id = uuid::Uuid::new_v4();
        let download_high_water = std::sync::atomic::AtomicU64::new(0);
        loop {
            let attempted = (self.retry_budget.clock)();
            let result =
                self.request_once(method.clone(), path, query, body.as_ref(), headers, limit, operation_id, &download_high_water);
            match result {
                Ok(mut reply) => {
                    let elapsed = (self.retry_budget.clock)().saturating_duration_since(attempted);
                    reply.attempt_duration = elapsed;
                    let code = response_code(&reply);
                    let maintenance =
                        reply.status == 503 && code.as_deref() == Some("server-updating");
                    let transient = retryable_response(&reply);
                    if self.retry_transient && (maintenance || (retry_generic && replay_safe && transient)) {
                        self.report_retryable_failure(if maintenance {
                            Some("server-updating")
                        } else {
                            Some("server-unreachable")
                        });
                        if !maintenance {
                            let _ = self.resolve_identity();
                        }
                        recovering = true;
                        self.retry_budget.wait(
                            reply.retry_after,
                            (self.retry_budget.clock)().saturating_duration_since(attempted),
                        )?;
                        continue;
                    }
                    if recovering {
                        self.report_retryable_failure(None);
                    }
                    return Ok(reply);
                }
                Err(error) if self.retry_transient && retry_generic && replay_safe && is_ambiguous_transient(&error) => {
                    self.report_retryable_failure(Some(&error.code));
                    let _ = self.resolve_identity();
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

    /// Tests turn the codec off to compare wire and raw traffic.
    fn codec_enabled(&self) -> bool {
        #[cfg(test)]
        if self.test_io.as_ref().is_some_and(|counter| counter.codec_disabled.load(std::sync::atomic::Ordering::Relaxed)) {
            return false;
        }
        true
    }

    /// Checks and encodes a request body once, before any attempt, so a retry
    /// resends the same bytes.
    fn prepare_body(&self, body: Option<Vec<u8>>) -> Result<Option<PreparedBody>> {
        let Some(raw) = body else {
            return Ok(None);
        };
        if raw.len() > 8 * 1024 * 1024 {
            return Err(SyncError::new("request-too-large", 413));
        }
        let encoded = if self.codec_enabled() {
            body_codec::encode(&raw).map_err(|error| SyncError::new(error.0, 500))?
        } else {
            None
        };
        Ok(Some(match encoded {
            Some(wire) => PreparedBody {
                wire: Arc::new(wire),
                encoded: true,
                #[cfg(test)]
                raw_len: raw.len(),
                log_body: risunest_network_log::enabled().then_some(raw),
            },
            None => PreparedBody {
                #[cfg(test)]
                raw_len: raw.len(),
                wire: Arc::new(raw),
                log_body: None,
                encoded: false,
            },
        }))
    }

    fn request_once(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&PreparedBody>,
        headers: &[(&str, String)],
        limit: usize,
        operation_id: uuid::Uuid,
        download_high_water: &std::sync::atomic::AtomicU64,
    ) -> Result<Reply> {
        self.ensure_active()?;
        #[cfg(test)]
        let observed_method = method.clone();
        #[cfg(test)]
        if let Some(counter)=&self.test_io {
            counter.requests.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
            counter.request_body_bytes.fetch_add(body.map_or(0,|b|b.wire.len() as u64),std::sync::atomic::Ordering::Relaxed);
            counter.request_raw_bytes.fetch_add(body.map_or(0,|b|b.raw_len as u64),std::sync::atomic::Ordering::Relaxed);
            if body.is_some_and(|b| b.wire.len() > b.raw_len) { counter.expanded_bodies.fetch_add(1,std::sync::atomic::Ordering::Relaxed); }
        }
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
            .header(risunest_sync_wire::library_format::HEADER, super::library_format::LEVEL.to_string())
            .header("accept-encoding", "identity")
            .timeout(timeout);
        if self.codec_enabled() {
            request = request.header(body_codec::ACCEPT_HEADER, body_codec::ZSTD);
        }
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        if let Some(body) = body {
            if body.encoded {
                request = request.header(body_codec::ENCODING_HEADER, body_codec::ZSTD);
            }
            let length = body.wire.len() as u64;
            let reader = super::progress::WireReader::new(body.reader(), self.lane.clone(), true, Some(length));
            request = request.body(reqwest::blocking::Body::sized(reader, length));
        }
        let request = request.build().map_err(|_| SyncError::new("server-unreachable", 503))?;
        let capture = risunest_network_log::begin_operation("sync", request.method().as_str(), request.url().as_str(), request.headers(), operation_id);
        if capture.is_active() {
            if let Some(body) = body {
                let raw = body.log_body.as_deref().unwrap_or(&body.wire);
                if body.encoded {
                    capture.representation(risunest_network_log::Side::Request, if body.log_body.is_some() { "decoded-sync" } else { "encoded-sync" });
                }
                capture.append(risunest_network_log::Side::Request, raw);
            }
        }
        let response = self.http.execute(request).map_err(|e| {
            capture.finish("error");
            SyncError::new(
                if e.is_timeout() {
                    "server-timeout"
                } else {
                    "server-unreachable"
                },
                503,
            )
        })?;
        capture.response(response.status().as_u16(), response.url().as_str(), response.headers());
        let status = response.status().as_u16();
        let library_format = super::library_format::read(response.headers())?;
        super::lww_client::observe_library_format(&config.endpoint, &config.library_id, status, library_format);
        let object_range = response
            .headers()
            .get("x-risu-object-range")
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
        let encoded = body_codec::marked(
            response
                .headers()
                .get_all(body_codec::ENCODING_HEADER)
                .iter()
                .map(|value| value.as_bytes()),
        )
        .map_err(|_| SyncError::new("invalid-response-encoding", 502))?;
        // A kept encoding is smaller than its raw body, so the raw limit also
        // bounds the encoded one.
        if response.content_length().is_some_and(|v| v > limit as u64) {
            return Err(SyncError::new("response-too-large", 502));
        }
        // The declared length, already within the limit, sizes the buffer so a
        // large reply is not read into one grown by doubling.
        let mut bytes = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
        let wire = super::progress::WireReader::new(response.take(limit as u64 + 1), self.lane.clone(), false, None);
        let observer = if (200..300).contains(&status) && (path == "objects/transfer" || (path.starts_with("objects/") && path.ends_with("/part"))) {
            self.body_download_observer.as_deref()
        } else { None };
        let read = ObservedBodyRead { inner: wire, position: 0, high_water: download_high_water, observer }.read_to_end(&mut bytes);
        if read.is_err() || bytes.len() > limit {
            if encoded { capture.representation(risunest_network_log::Side::Response, "encoded-sync"); }
            capture.append(risunest_network_log::Side::Response, &bytes);
            capture.finish("error");
        }
        read.map_err(|_| SyncError::new("incomplete-response", 503))?;
        if bytes.len() > limit { return Err(SyncError::new("response-too-large", 502)); }
        #[cfg(test)]
        let wire = bytes.len() as u64;
        let bytes = if encoded {
            match body_codec::decode(&bytes, limit) {
                Ok(raw) => { capture.representation(risunest_network_log::Side::Response, "decoded-sync"); raw },
                Err(error) => {
                    capture.representation(risunest_network_log::Side::Response, "encoded-sync");
                    capture.append(risunest_network_log::Side::Response, &bytes);
                    capture.finish("error");
                    return Err(SyncError::new(if error.0 == "body-too-large" { "response-too-large" } else { "invalid-response-encoding" }, 502));
                }
            }
        } else { bytes };
        capture.append(risunest_network_log::Side::Response, &bytes);
        capture.finish("complete");
        #[cfg(test)]
        if let Some(counter)=&self.test_io {
            counter.response_body_bytes.fetch_add(wire,std::sync::atomic::Ordering::Relaxed);
            counter.response_raw_bytes.fetch_add(bytes.len() as u64,std::sync::atomic::Ordering::Relaxed);
            if wire > bytes.len() as u64 { counter.expanded_bodies.fetch_add(1,std::sync::atomic::Ordering::Relaxed); }
            counter.traffic.lock().unwrap().response(&observed_method, path, wire, &bytes);
        }
        Ok(Reply {
            status,
            body: bytes,
            object_range,
            retry_after,
            attempt_duration: Duration::ZERO,
            library_format,
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
        Ok(self.head_with_format()?.0)
    }
    /// The library head and the format level its reply reports.
    pub(crate) fn head_with_format(&self) -> Result<(RemoteHead, Option<super::library_format::LibraryFormat>)> {
        let reply = self.request(Method::GET, "head", &[], None, &[], MAX_METADATA_BYTES)?;
        if !(200..300).contains(&reply.status) {
            return Err(response_error(reply));
        }
        let head: RemoteHead = canonical::decode(&reply.body, MAX_METADATA_BYTES)?;
        head.validate()?;
        if head.library_id != self.config().library_id {
            return Err(SyncError::new("library-mismatch", 409));
        }
        Ok((head, reply.library_format))
    }
}

pub(crate) fn response_code(reply: &Reply) -> Option<String> {
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
                | "ack"
        ) || (path.starts_with("uploads/") && path.ends_with("/complete"))
            || (path.starts_with("staged-changes/") && path.ends_with("/seal")))
}

pub(crate) fn retryable_response(reply: &Reply) -> bool {
    matches!(reply.status, 502 | 503 | 504)
        || (reply.status == 408 && response_code(reply).as_deref() == Some("request-timeout"))
}

/// How long sync clients share one HTTP client.
const SHARED_HTTP_LIFETIME: Duration = Duration::from_secs(60);

type SharedHttp = Option<(reqwest::blocking::Client, Instant)>;

/// The HTTP client of every sync client built within a minute of it. Commands
/// that follow one another reuse its open connections; the next one built
/// after that minute picks up system proxy changes. Idle connections close
/// well before a server or proxy would close them under a request.
fn shared_http() -> Result<reqwest::blocking::Client> {
    #[cfg(test)]
    if let Some(http) = tests::private_shared_http() {
        return http;
    }
    static SHARED: std::sync::Mutex<SharedHttp> = std::sync::Mutex::new(None);
    reuse_or_build_http(&mut SHARED.lock().unwrap_or_else(|error| error.into_inner()), Instant::now())
}
fn reuse_or_build_http(shared: &mut SharedHttp, now: Instant) -> Result<reqwest::blocking::Client> {
    if let Some((http, built)) = shared.as_ref() {
        if now.saturating_duration_since(*built) < SHARED_HTTP_LIFETIME {
            return Ok(http.clone());
        }
    }
    // Other crates enable reqwest's decoders, which would strip a
    // Content-Encoding this client has to see and refuse.
    let http = crate::platform_tls::blocking_client_builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| SyncError::new("http-client-unavailable", 503))?;
    *shared = Some((http.clone(), now));
    Ok(http)
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
    fn http_body_progress_is_visible_before_the_request_and_response_complete() {
        use super::*;
        use std::{io::Write, net::TcpListener, sync::mpsc};
        const UPLOAD: usize = 512 * 1024;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (notice, noticed) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") { let mut byte = [0]; socket.read_exact(&mut byte).unwrap(); header.push(byte[0]); }
            assert!(String::from_utf8(header).unwrap().to_ascii_lowercase().contains(&format!("content-length: {UPLOAD}\r\n")));
            let mut body = vec![0; UPLOAD];
            socket.read_exact(&mut body[..32768]).unwrap();
            notice.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            socket.read_exact(&mut body[32768..]).unwrap();
            assert!(body.iter().all(|byte| *byte == 7));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8192\r\nConnection: close\r\n\r\n").unwrap();
            socket.write_all(&[3; 4096]).unwrap();
            notice.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            socket.write_all(&[3; 4096]).unwrap();
        });
        let lane = Arc::new(super::super::progress::ProgressLane::default());
        let worker_lane = lane.clone();
        let downloaded = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observed = downloaded.clone();
        let worker = std::thread::spawn(move || super::super::progress::within(&worker_lane, || {
            let mut client = ServerClient::new(super::tests::config(&endpoint)).unwrap();
            client.body_download_observer = Some(Arc::new(move |bytes| { observed.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed); }));
            let body = PreparedBody { wire: Arc::new(vec![7; UPLOAD]), log_body: None, encoded: false, raw_len: UPLOAD };
            client.request_once(Method::POST, "objects/transfer", &[], Some(&body), &[], 8192, uuid::Uuid::new_v4(), &std::sync::atomic::AtomicU64::new(0)).unwrap().body
        }));
        noticed.recv_timeout(Duration::from_secs(5)).unwrap();
        let uploading = lane.snapshot("send");
        assert!(uploading.sent_bytes > 0 && uploading.sent_bytes <= UPLOAD as u64);
        assert_eq!((uploading.received_bytes, uploading.bytes_done), (0, 0));
        release.send(()).unwrap();
        noticed.recv_timeout(Duration::from_secs(5)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while downloaded.load(std::sync::atomic::Ordering::Relaxed) < 4096 && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(1)); }
        let downloading = lane.snapshot("send");
        assert_eq!(downloading.received_bytes, 4096);
        assert_eq!(downloaded.load(std::sync::atomic::Ordering::Relaxed), 4096);
        assert!(downloading.receiving);
        assert!(!downloading.sending);
        assert_eq!(downloading.bytes_done, 0);
        release.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), vec![3; 8192]);
        server.join().unwrap();
        let done = lane.snapshot("send");
        assert_eq!((done.sent_bytes, done.received_bytes), (UPLOAD as u64, 8192));
        assert_eq!(downloaded.load(std::sync::atomic::Ordering::Relaxed), 8192);
        assert!(!done.sending && !done.receiving);
    }

    #[test]
    fn an_attempt_reads_the_prepared_body_without_copying_it() {
        use super::*;
        let body = PreparedBody { wire: Arc::new(vec![7; 4096]), log_body: None, encoded: false, raw_len: 4096 };
        let mut first = body.reader();
        assert_eq!(Arc::strong_count(&body.wire), 2);
        let mut sent = Vec::new();
        first.read_to_end(&mut sent).unwrap();
        drop(first);
        // A retry starts again from the first byte of the same body.
        let mut retried = Vec::new();
        body.reader().read_to_end(&mut retried).unwrap();
        assert_eq!((sent, retried), (vec![7; 4096], vec![7; 4096]));
    }

    #[test]
    fn a_reply_is_read_into_a_buffer_of_its_declared_length() {
        use super::*;
        use std::{io::Write, net::TcpListener};
        const LENGTH: usize = 600_000;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") { let mut byte = [0]; socket.read_exact(&mut byte).unwrap(); header.push(byte[0]); }
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {LENGTH}\r\nConnection: close\r\n\r\n").unwrap();
            socket.write_all(&vec![5; LENGTH]).unwrap();
        });
        let client = ServerClient::new(super::tests::config(&endpoint)).unwrap();
        let reply = client.request_once(Method::GET, "objects/synthetic/part", &[], None, &[], LENGTH, uuid::Uuid::new_v4(), &std::sync::atomic::AtomicU64::new(0)).unwrap();
        server.join().unwrap();
        assert_eq!(reply.body, vec![5; LENGTH]);
        assert!(reply.body.capacity() < LENGTH + LENGTH / 8);
    }

    #[test]
    fn body_download_retry_reports_only_new_bytes() {
        use super::*;
        use std::{io::Write, net::TcpListener, sync::mpsc};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (notice, noticed) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let server = std::thread::spawn(move || {
            for attempt in 0..4 {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") { let mut byte = [0]; socket.read_exact(&mut byte).unwrap(); header.push(byte[0]); }
                let header = String::from_utf8(header).unwrap();
                let path = if attempt == 1 { "session" } else { "objects/synthetic/part" };
                assert!(header.starts_with(&format!("GET /{path} HTTP/1.1\r\n")));
                if attempt == 1 || attempt == 2 {
                    let body = if attempt == 1 { r#"{"error":"synthetic-identity"}"# } else { r#"{"error":"server-updating"}"# };
                    write!(socket,"HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                } else {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8192\r\nConnection: close\r\n\r\n").unwrap();
                    socket.write_all(&[3;4096]).unwrap();
                    if attempt == 0 {
                        released.recv_timeout(Duration::from_secs(5)).unwrap();
                        // Closing this declared 8192-byte body after its first half forces the real retry path.
                    } else { socket.write_all(&[3;4096]).unwrap(); }
                }
            }
        });
        let reported = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observed = reported.clone();
        let worker = std::thread::spawn(move || {
            let mut client = ServerClient::new(super::tests::config(&endpoint)).unwrap();
            client.retry_budget = super::tests::no_sleep_budget();
            client.body_download_observer = Some(Arc::new(move |bytes| {
                let total = observed.fetch_add(bytes,std::sync::atomic::Ordering::Relaxed) + bytes;
                if total == 4096 { notice.send(()).unwrap(); }
            }));
            client.request(Method::GET,"objects/synthetic/part",&[],None,&[],8192).unwrap()
        });
        noticed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(reported.load(std::sync::atomic::Ordering::Relaxed),4096);
        assert!(!worker.is_finished());
        release.send(()).unwrap();
        let reply = worker.join().unwrap();
        assert_eq!(reply.status,200);
        assert_eq!(reply.body,vec![3;8192]);
        server.join().unwrap();
        assert_eq!(reported.load(std::sync::atomic::Ordering::Relaxed),8192,"retry prefixes, identity responses and error bodies are not downloaded body progress");
    }

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
#[track_caller]
pub(crate) fn response_error(reply: Reply) -> SyncError {
    let value: Option<serde_json::Value> = serde_json::from_slice(&reply.body).ok();
    let code = value
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|v| v.as_str())
        .filter(|s| s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
        .unwrap_or("server-response-error");
    // A rejection that names a record is the only one worth a log line. The key
    // is a locator and stays in this device's own log.
    if let Some(key) = value.as_ref().and_then(|v| v.get("key")).and_then(|v| v.as_str()) {
        crate::native_log::global_state().record(
            "error",
            "server-sync",
            format!(
                "server rejected a record: status={} code={code} key={}",
                reply.status,
                log_key(key)
            ),
        );
    }
    SyncError::new(code, reply.status)
}
/// Bounds a reported key and drops control characters before it reaches a log line.
pub(crate) fn log_key(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(512)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    pub(super) fn no_sleep_budget() -> Arc<RetryBudget> {
        let current = Arc::new(Mutex::new(Instant::now()));
        let clock_state = current.clone();
        let sleep_state = current;
        Arc::new(RetryBudget::with_driver(
            None,
            Arc::new(move || *clock_state.lock().unwrap()),
            Arc::new(move |duration| *sleep_state.lock().unwrap() += duration),
        ))
    }
    pub(super) fn config(endpoint: &str) -> ServerConfig {
        ServerConfig {
            directory: None,
            endpoint: endpoint.into(),
            library_id: "library".into(),
            device_id: "device".into(),
            token: "a".repeat(64),
        }
    }

    thread_local! {
        /// The shared HTTP client of a `PrivateSharedHttp` scope, and how far its
        /// clock was moved ahead.
        static PRIVATE_SHARED_HTTP: std::cell::RefCell<Option<(SharedHttp, Duration)>> = const { std::cell::RefCell::new(None) };
    }
    pub(super) fn private_shared_http() -> Option<Result<reqwest::blocking::Client>> {
        PRIVATE_SHARED_HTTP.with(|private| {
            let mut private = private.borrow_mut();
            let (shared, ahead) = private.as_mut()?;
            Some(reuse_or_build_http(shared, Instant::now() + *ahead))
        })
    }
    /// Gives clients built on this thread a shared HTTP client of their own,
    /// which no other test reuses or ages, until dropped. Dropping it drops that
    /// client here rather than in the thread-local's destructor, where joining
    /// the client's runtime thread deadlocks on Windows.
    struct PrivateSharedHttp;
    impl PrivateSharedHttp {
        fn new() -> Self {
            PRIVATE_SHARED_HTTP.with(|private| *private.borrow_mut() = Some((None, Duration::ZERO)));
            Self
        }
        fn move_clock_ahead(&self, by: Duration) {
            PRIVATE_SHARED_HTTP.with(|private| {
                if let Some((_, ahead)) = private.borrow_mut().as_mut() {
                    *ahead += by;
                }
            });
        }
    }
    impl Drop for PrivateSharedHttp {
        fn drop(&mut self) {
            let private = PRIVATE_SHARED_HTTP.with(|private| private.borrow_mut().take());
            drop(private);
        }
    }

    /// A server that records each request's connection and credentials, and a
    /// call that stops it.
    fn recording_server() -> (String, Arc<Mutex<Vec<(std::net::SocketAddr, String)>>>, impl FnOnce()) {
        use axum::{extract::ConnectInfo, http::{HeaderMap, StatusCode}, Router};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = seen.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
                let router = Router::new().fallback(move |ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>, headers: HeaderMap| {
                    let captured = captured.clone();
                    async move {
                        captured.lock().unwrap().push((peer, headers["authorization"].to_str().unwrap().to_owned()));
                        StatusCode::NO_CONTENT
                    }
                });
                axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), router.into_make_service_with_connect_info::<std::net::SocketAddr>())
                    .with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
            });
        });
        (endpoint, seen, move || { stop.send(()).unwrap(); server.join().unwrap(); })
    }

    /// Clients built one after another share the process-wide transport.
    #[test]
    fn clients_built_one_after_another_share_the_process_transport() {
        let (endpoint, seen, stop) = recording_server();
        for _ in 0..3 {
            let client = ServerClient::new(config(&endpoint)).unwrap();
            assert_eq!(client.request(Method::GET, "head", &[], None, &[], 1024).unwrap().status, 204);
        }
        stop();
        let seen = seen.lock().unwrap();
        let connections = seen.iter().map(|(peer, _)| *peer).collect::<std::collections::BTreeSet<_>>();
        // The shared client may reach the end of its lifetime between two of them.
        assert!(seen.len() == 3 && connections.len() <= 2, "{} requests on {} connections", seen.len(), connections.len());
    }

    /// Clients built within the shared lifetime reuse its connection, one built
    /// after it refreshes transport configuration, and none shares another's
    /// credentials or cancellation.
    #[test]
    fn clients_share_transport_for_a_minute_without_sharing_credentials_or_cancellation() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (endpoint, seen, stop) = recording_server();
        let shared = PrivateSharedHttp::new();
        let cancelled = Arc::new(AtomicBool::new(false));
        let first = ServerClient::with_cancellation(config(&endpoint), Some(cancelled.clone())).unwrap();
        assert_eq!(first.request(Method::GET, "head", &[], None, &[], 1024).unwrap().status, 204);
        cancelled.store(true, Ordering::Release);
        assert_eq!(first.request(Method::GET, "head", &[], None, &[], 1024).err().unwrap().code, "cancelled");
        drop(first);
        let mut next = config(&endpoint);
        next.token = "b".repeat(64);
        let second = ServerClient::new(next).unwrap();
        assert_eq!(second.request(Method::GET, "head", &[], None, &[], 1024).unwrap().status, 204);
        shared.move_clock_ahead(SHARED_HTTP_LIFETIME);
        let third = ServerClient::new(config(&endpoint)).unwrap();
        assert_eq!(third.request(Method::GET, "head", &[], None, &[], 1024).unwrap().status, 204);
        stop();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].0, seen[1].0, "a client within the lifetime reuses the connection");
        assert_ne!(seen[1].0, seen[2].0, "a client after the lifetime refreshes transport configuration");
        assert_eq!(seen[0].1, format!("Bearer {}", "a".repeat(64)));
        assert_eq!(seen[1].1, format!("Bearer {}", "b".repeat(64)));
        assert_eq!(seen[2].1, format!("Bearer {}", "a".repeat(64)));
    }
    #[test]
    fn verified_progress_renews_more_than_five_minutes_of_transient_failures() {
        let current = Arc::new(Mutex::new(Instant::now()));
        let clock = current.clone();
        let sleeper = current.clone();
        let budget = Arc::new(RetryBudget::with_driver(
            None,
            Arc::new(move || *clock.lock().unwrap()),
            Arc::new(move |d| *sleeper.lock().unwrap() += d),
        ));
        let mut client =
            ServerClient::with_retry_budget(config("http://127.0.0.1:1"), None, budget.clone())
                .unwrap();
        client.retryable_failure = Some(Arc::new(Mutex::new(None)));
        for _ in 0..10 {
            budget.wait(None, Duration::from_secs(60)).unwrap();
            client.report_retryable_failure(Some("server-timeout"));
            client.verified(1024);
            assert_eq!(budget.remaining().unwrap(), RETRY_BUDGET);
            assert!(client
                .retryable_failure
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .is_none());
        }
        budget.charge(RETRY_BUDGET).unwrap();
        assert_eq!(
            budget.wait(None, Duration::ZERO).unwrap_err().code,
            "sync-retry-budget-exhausted"
        );
    }

    #[test]
    fn fixed_endpoint_accepts_http_and_https_and_rejects_embedded_credentials() {
        for url in [
            "https://sync.example/base",
            "http://127.0.0.1:4319",
            "http://[::1]:4319",
            "http://192.168.0.1:14319",
            "http://[fd00::10]:14319",
            "http://sync.internal:14319/base",
            "http://203.0.113.10:14319",
        ] {
            assert!(config(url).validate().is_ok());
        }
        for url in [
            "ftp://sync.example",
            "http://name:secret@sync.example",
            "https://name:secret@sync.example",
            "https://sync.example/?token=x",
            "https://sync.example/#fragment",
            "file:///tmp",
        ] {
            assert!(config(url).validate().is_err());
        }
    }

    #[test]
    fn a_peer_on_another_protocol_is_named_before_any_other_request() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut bytes = Vec::new();
                // The blank line that ends a request head.
                while !bytes.ends_with(&[13, 10, 13, 10]) {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    bytes.push(byte[0]);
                }
                let input = String::from_utf8(bytes).unwrap();
                recorded
                    .lock()
                    .unwrap()
                    .push(input.lines().next().unwrap_or_default().to_owned());
                let head = serde_json::to_value(risunest_sync_wire::RemoteHead {
                    head_id: "a".repeat(64),
                    ..risunest_sync_wire::RemoteHead::genesis("library".into(), "epoch".into())
                        .unwrap()
                })
                .unwrap();
                let body = serde_json::json!({"head":head,"deviceId":"device","protocolId":"risunest-sync/v0"}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                stream.flush().unwrap();
            }
        });
        let client = ServerClient::new(config(&endpoint)).unwrap();
        // The peer is not asked for anything else.
        let error = client.resolve_identity().unwrap_err();
        assert_eq!(error.code, "server-incompatible");
        assert_eq!(error.status, 409);
        assert!(!error.retryable);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen.iter().all(|line| line.starts_with("GET /session ")));
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
        if crate::test_process::run_isolated(concat!(module_path!(), "::explicit_maintenance_retries_an_unadmitted_mutation_with_retry_after")) {
            return;
        }
        let _guard = crate::network_log::TEST_RECORDER_LOCK
            .lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let recorder = risunest_network_log::Recorder::open(
            root.path().join("recent"), root.path().join("retained"),
        ).unwrap();
        recorder.configure(risunest_network_log::Settings {
            enabled: true, ..Default::default()
        }).unwrap();
        risunest_network_log::install(recorder.clone());
        for ambiguous in [false, true] {
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
                let mut request = server.recv().unwrap();
                std::io::copy(request.as_reader(), &mut std::io::sink()).unwrap();
                request.respond(tiny_http::Response::empty(201)).unwrap();
            });
            let mut client = ServerClient::new(config(&endpoint)).unwrap();
            client.retry_budget = no_sleep_budget();
            let request = || {
                if ambiguous {
                    match client.request_ambiguous_mutation(
                        Method::POST, "uploads", Some(b"synthetic".to_vec()), &[], MAX_METADATA_BYTES,
                    ).unwrap() {
                        RequestAttempt::Response(reply) => reply,
                        RequestAttempt::Failure { error, .. } => panic!("{error:?}"),
                    }
                } else {
                    client.request(
                        Method::POST,
                        "uploads",
                        &[],
                        Some(b"synthetic".to_vec()),
                        &[],
                        MAX_METADATA_BYTES,
                    )
                    .unwrap()
                }
            };
            let reply = request();
            assert_eq!(reply.status, 201);
            assert_eq!(*client.retry_budget.spent.lock().unwrap(), MAX_RETRY_DELAY);
            let attempts = recorder.list(risunest_network_log::Query {
                search: endpoint.clone(), ..Default::default()
            }).unwrap().items;
            assert_eq!(attempts.len(), 2);
            let operation = attempts[0].operation_id.clone().expect("retry operation ID");
            assert!(uuid::Uuid::parse_str(&operation).is_ok());
            assert_eq!(attempts[1].operation_id.as_deref(), Some(operation.as_str()));
            assert_ne!(attempts[0].id, attempts[1].id);
            assert_eq!(request().status, 201);
            task.join().unwrap();
            let next = recorder.list(risunest_network_log::Query {
                search: endpoint, ..Default::default()
            }).unwrap().items;
            assert_eq!(next.len(), 3);
            assert_eq!(next.iter().filter(|item| item.operation_id.as_deref() == Some(operation.as_str())).count(), 2);
            assert!(next.iter().all(|item| item.operation_id.is_some()));
        }
        recorder.shutdown();
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

    #[test]
    fn transient_identity_resolution_consumes_the_remaining_retry_budget_before_replay() {
        for status in [502, 503, 504] {
            let current = Arc::new(Mutex::new(Instant::now()));
            let clock = current.clone();
            let sleeper = current.clone();
            let budget = Arc::new(RetryBudget::with_driver(
                None,
                Arc::new(move || *clock.lock().unwrap()),
                Arc::new(move |duration| *sleeper.lock().unwrap() += duration),
            ));
            let identity_duration = Duration::from_secs(30);
            budget.charge(RETRY_BUDGET - identity_duration).unwrap();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", server.server_addr());
            let identity_clock = current.clone();
            let task = std::thread::spawn(move || {
                let request = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                assert_eq!(request.method(), &tiny_http::Method::Get);
                assert_eq!(request.url(), "/head");
                request.respond(tiny_http::Response::empty(status)).unwrap();
                let request = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                assert_eq!(request.method(), &tiny_http::Method::Get);
                assert_eq!(request.url(), "/session");
                *identity_clock.lock().unwrap() += identity_duration;
                request.respond(tiny_http::Response::empty(503)).unwrap();
                assert!(server.recv_timeout(Duration::from_millis(100)).unwrap().is_none());
            });
            let client = ServerClient::with_retry_budget(config(&endpoint), None, budget.clone())
                .unwrap();
            let attempted = *current.lock().unwrap();
            let error = client
                .request(Method::GET, "head", &[], None, &[], MAX_METADATA_BYTES)
                .err()
                .expect("identity resolution must exhaust the budget before replay");
            assert_eq!((error.code.as_str(), error.status), ("sync-retry-budget-exhausted", 503));
            assert!(error.retryable);
            assert_eq!(current.lock().unwrap().duration_since(attempted), identity_duration);
            assert_eq!(budget.remaining().unwrap(), identity_duration);
            task.join().unwrap();
        }
    }

    fn respond_in_order(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, std::thread::JoinHandle<usize>) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.server_addr());
        let task = std::thread::spawn(move || {
            let mut seen = 0;
            for (status, body) in responses {
                let mut request = server.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
                seen += 1;
                std::io::copy(request.as_reader(), &mut std::io::sink()).unwrap();
                request
                    .respond(
                        tiny_http::Response::from_string(body)
                            .with_status_code(status)
                            .with_header(
                                tiny_http::Header::from_bytes("Retry-After", "1").unwrap(),
                            ),
                    )
                    .unwrap();
            }
            if server
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_some()
            {
                seen += 1;
            }
            seen
        });
        (endpoint, task)
    }

    #[test]
    fn initial_receive_returns_the_first_failure_without_spending_the_retry_budget() {
        for identity in [false, true] {
            for (status, body, code) in [
                (503, "", "server-response-error"),
                (521, "<html>origin unavailable</html>", "server-response-error"),
                (530, "<html>error 1033</html>", "server-response-error"),
                (408, r#"{"error":"request-timeout"}"#, "request-timeout"),
                (503, r#"{"error":"server-updating"}"#, "server-updating"),
                (401, r#"{"error":"unauthorized"}"#, "unauthorized"),
            ] {
                let (endpoint, task) = respond_in_order(vec![(status, body)]);
                let mut client = ServerClient::new(config(&endpoint)).unwrap();
                client.retry_budget = no_sleep_budget();
                client.retry_transient = false;
                let error = if identity { client.resolve_identity().unwrap_err() }
                    else { client.json::<serde_json::Value>(Method::GET, "changes", &[], None::<&()>, &[]).unwrap_err() };
                assert_eq!(error.code, code);
                assert_eq!(error.status, status);
                assert_eq!(client.retry_budget.remaining().unwrap(), RETRY_BUDGET);
                assert_eq!(task.join().unwrap(), 1);
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let mut client = ServerClient::new(config(&endpoint)).unwrap();
        client.retry_budget = no_sleep_budget();
        client.retry_transient = false;
        let error = client.json::<serde_json::Value>(Method::GET, "changes", &[], None::<&()>, &[]).unwrap_err();
        assert_eq!(error.code, "server-unreachable");
        assert_eq!(client.retry_budget.remaining().unwrap(), RETRY_BUDGET);

        let silent = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut client = ServerClient::new(config(&format!("http://{}", silent.server_addr()))).unwrap();
        client.retry_transient = false;
        *client.retry_budget.spent.lock().unwrap() = RETRY_BUDGET - Duration::from_millis(50);
        let error = client.resolve_identity().unwrap_err();
        assert_eq!(error.code, "server-timeout");
        assert_eq!(client.retry_budget.remaining().unwrap(), Duration::from_millis(50));
    }

    #[test]
    fn request_timeout_replays_only_recognized_identity_bound_requests() {
        for (path, code, replay) in [
            ("ack", "request-timeout", true),
            ("objects/missing", "request-timeout", true),
            ("objects/missing", "unknown-timeout", false),
            ("uploads", "request-timeout", false),
        ] {
            let body = if code == "request-timeout" { r#"{"error":"request-timeout"}"# }
                else { r#"{"error":"unknown-timeout"}"# };
            let mut responses = vec![(408, body)];
            if replay {
                // Recovery checks the identity, then retries within the same budget.
                responses.extend([(503, ""), (200, "done")]);
            }
            let (endpoint, task) = respond_in_order(responses);
            let mut client = ServerClient::new(config(&endpoint)).unwrap();
            client.retry_budget = no_sleep_budget();
            let reply = client.request(Method::POST, path, &[], Some(b"{}".to_vec()), &[], MAX_METADATA_BYTES).unwrap();
            assert_eq!(reply.status, if replay { 200 } else { 408 });
            assert_eq!(task.join().unwrap(), if replay { 3 } else { 1 });
        }
    }

    #[test]
    fn an_unrecognized_or_permanent_refusal_is_not_replayed() {
        for (status, body) in [
            (429, r#"{"error":"pin-quota"}"#),
            (429, "Too Many Requests"),
            (403, r#"{"error":"forbidden"}"#),
        ] {
            let (endpoint, task) = respond_in_order(vec![(status, body)]);
            let client = ServerClient::new(config(&endpoint)).unwrap();
            let reply = client
                .request(
                    Method::POST,
                    "media/access",
                    &[],
                    Some(b"{}".to_vec()),
                    &[],
                    MAX_METADATA_BYTES,
                )
                .unwrap();
            assert_eq!(reply.status, status);
            assert_eq!(task.join().unwrap(), 1, "{status} {body}");
        }
    }

    /// A loopback endpoint that closes its first `dropped` connections before
    /// any reply and forwards later ones to `target`, if there is one.
    fn closing_endpoint(
        dropped: usize,
        target: Option<std::net::SocketAddr>,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::net::{Shutdown, TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = accepted.clone();
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { return };
                let index = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some(target) = target.filter(|_| index >= dropped) else {
                    drop(client);
                    continue;
                };
                let server = TcpStream::connect(target).unwrap();
                for (mut from, mut to) in [
                    (client.try_clone().unwrap(), server.try_clone().unwrap()),
                    (server, client),
                ] {
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut from, &mut to);
                        let _ = to.shutdown(Shutdown::Write);
                    });
                }
            }
        });
        (endpoint, accepted)
    }

    #[test]
    fn a_configured_connection_closed_before_any_reply_is_tried_once_more() {
        let server = crate::server_sync::lww_tests::LocalServerFixture::new();
        let (_root, store) = crate::server_sync::lww_tests::local();
        let (registered, _) = server.candidate(&store);
        let target = Url::parse(&registered.endpoint).unwrap().socket_addrs(|| None).unwrap()[0];
        let (endpoint, accepted) = closing_endpoint(1, Some(target));
        let mut client = ServerClient::new(ServerConfig { endpoint, ..registered.clone() }).unwrap();
        client.retry_budget = no_sleep_budget();
        let head = client.resolve_configured_identity().unwrap();
        assert_eq!(head.library_id, registered.library_id);
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(*client.retry_budget.spent.lock().unwrap() >= Duration::from_secs(1));

        let (endpoint, accepted) = closing_endpoint(usize::MAX, None);
        let mut client = ServerClient::new(ServerConfig { endpoint, ..registered }).unwrap();
        client.retry_budget = no_sleep_budget();
        let error = client.resolve_configured_identity().unwrap_err();
        assert_eq!((error.code.as_str(), error.status), ("server-unreachable", 503));
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn a_configured_connection_that_replies_is_not_tried_again() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", server.server_addr());
        let task = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            request
                .respond(tiny_http::Response::from_string("{\"error\":\"unavailable\"}").with_status_code(503))
                .unwrap();
            server.recv_timeout(Duration::from_millis(200)).unwrap().is_none()
        });
        let mut client = ServerClient::new(config(&endpoint)).unwrap();
        client.retry_budget = no_sleep_budget();
        let error = client.resolve_configured_identity().unwrap_err();
        assert_eq!(error.status, 503);
        assert!(task.join().unwrap());
        assert_eq!(*client.retry_budget.spent.lock().unwrap(), Duration::ZERO);
    }

}

#[cfg(test)]
#[path = "directory_tests.rs"]
mod directory_tests;

#[cfg(test)]
#[path = "concurrency_tests.rs"]
mod concurrency_tests;
