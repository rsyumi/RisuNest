//! One-dispatch HTTP boundary: account waits, actual MYBOX reservations,
//! cancellation, bounded control requests and observed server-clock facts.
use risunest_network_log::reqwest::RequestBuilderExt;
use super::{
    contract::*,
    leases::{observe_time_sample, TimeSample},
    quota::{AccountBackoff, AccountKey},
    quota_profiles::MyboxCharge,
};
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    pin::Pin,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};
use tokio::io::AsyncRead;

pub(crate) const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const TRANSFER_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(120);

/// Native HTTP streams both ways and does not follow redirects or retry status
/// codes. Production adapters use `send` below, not the transport directly.
pub(crate) struct NativeHttpTransport {
    client: reqwest::Client,
    user_http_origin: Option<String>,
    webdav_http: bool,
    #[cfg(test)]
    loopback_http: bool,
}
impl NativeHttpTransport {
    pub fn new() -> Result<Self> {
        let client = crate::platform_tls::client_builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_gzip().no_brotli().no_deflate()
            .connect_timeout(Duration::from_secs(30))
            .build().map_err(network_error)?;
        Ok(Self {
            client,
            user_http_origin: None,
            webdav_http: false,
            #[cfg(test)]
            loopback_http: false,
        })
    }
    pub fn with_user_endpoint(mut self, config: &ConnectionConfig) -> Result<Self> {
        self.webdav_http = config.provider == "webdav";
        if accepts_user_http(&config.provider) {
            let endpoint = url::Url::parse(&config.endpoint).map_err(|error| ProviderError::new(ErrorKind::Unsupported).caused(&error))?;
            if !user_endpoint_allowed(&endpoint) {
                return Err(ProviderError::new(ErrorKind::Unsupported).caused("The storage URL must use HTTP or HTTPS without embedded credentials or a fragment"));
            }
            if endpoint.scheme() == "http" {
                let target = if config.provider == "s3" {
                    super::providers::s3::configured_origin(config)?
                } else { endpoint };
                self.user_http_origin = Some(target.origin().ascii_serialization());
            }
        }
        Ok(self)
    }
    fn permits_url(&self, url: &url::Url) -> bool {
        url.scheme() == "https" || (url.scheme() == "http"
            && (self.webdav_http || self.user_http_origin.as_deref() == Some(url.origin().ascii_serialization().as_str())))
    }
    #[cfg(test)]
    pub fn for_loopback_tests() -> Self {
        Self {
            client: reqwest::Client::builder().no_proxy()
                .no_gzip().no_brotli().no_deflate()
                .redirect(reqwest::redirect::Policy::none()).build().unwrap(),
            user_http_origin: None,
            webdav_http: false,
            loopback_http: true,
        }
    }
}
/// Providers whose endpoint the user types, and so may be a plain `http` server.
pub(crate) fn accepts_user_http(provider: &str) -> bool {
    matches!(provider, "webdav" | "s3" | "gitlab_packages")
}
pub(crate) fn user_endpoint_allowed(url: &url::Url) -> bool {
    matches!(url.scheme(), "https" | "http") && url.host_str().is_some()
        && url.username().is_empty() && url.password().is_none() && url.fragment().is_none()
        && !url.cannot_be_a_base()
}

fn network_error(error: reqwest::Error) -> ProviderError {
    let mut kind = if error.is_builder() { ErrorKind::Unsupported } else { ErrorKind::Transient };
    let mut details = Vec::new();
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(error) = source {
        let detail = error.to_string();
        if !details.contains(&detail) { details.push(detail); }
        #[cfg(not(target_os = "android"))]
        if error.is::<native_tls::Error>() {
            kind = ErrorKind::EndpointRejected;
        }
        #[cfg(target_os = "android")]
        if error.is::<rustls::Error>() || error.downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref).is_some_and(|inner| inner.is::<rustls::Error>()) {
            kind = ErrorKind::EndpointRejected;
        }
        source = error.source();
    }
    if kind == ErrorKind::EndpointRejected { details.insert(0, "TLS handshake failed".into()); }
    ProviderError::new(kind).caused(&details.join(": "))
}

struct UploadProgress {
    last_progress: tokio::time::Instant,
    remaining: u64,
}
struct UploadReader {
    reader: Pin<Box<dyn AsyncRead + Send>>,
    progress: Arc<Mutex<UploadProgress>>,
}
impl AsyncRead for UploadReader {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = self.reader.as_mut().poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &result {
            let read = buf.filled().len() - before;
            if read > 0 {
                let mut progress = self.progress.lock().unwrap_or_else(|error| error.into_inner());
                progress.last_progress = tokio::time::Instant::now();
                progress.remaining = progress.remaining.saturating_sub(read as u64);
            }
        }
        result
    }
}
async fn upload_watchdog(progress: Arc<Mutex<UploadProgress>>) -> String {
    loop {
        let (deadline, awaiting_headers) = {
            let progress = progress.lock().unwrap_or_else(|error| error.into_inner());
            (progress.last_progress + if progress.remaining == 0 { RESPONSE_HEADERS_TIMEOUT } else { TRANSFER_IDLE_TIMEOUT }, progress.remaining == 0)
        };
        if tokio::time::Instant::now() >= deadline {
            return if awaiting_headers { format!("The server sent no response headers for {} seconds", RESPONSE_HEADERS_TIMEOUT.as_secs()) }
                else { format!("Upload made no progress for {} seconds", TRANSFER_IDLE_TIMEOUT.as_secs()) };
        }
        tokio::time::sleep_until(deadline).await;
    }
}

impl HttpTransport for NativeHttpTransport {
    fn send<'a>(&'a self, request: HttpRequest, cancel: &'a Cancellation) -> ProviderFuture<'a, HttpResponse> {
        Box::pin(async move {
            use futures::TryStreamExt;
            use tokio::io::AsyncReadExt;
            cancel.check()?;
            let allowed = self.permits_url(&request.url);
            #[cfg(test)]
            let allowed = allowed || (self.loopback_http && request.url.scheme() == "http"
                && request.url.host_str() == Some("127.0.0.1"));
            if !allowed || !request.url.username().is_empty() || request.url.password().is_some()
                || request.url.fragment().is_some()
            { return Err(ProviderError::new(ErrorKind::Unsupported)); }
            let mut builder = self.client.request(request.method, request.url);
            for (name, value) in request.headers {
                if name.eq_ignore_ascii_case("transfer-encoding")
                    || (name.eq_ignore_ascii_case("content-length") && value.parse::<u64>().ok() != request.content_length)
                { return Err(ProviderError::new(ErrorKind::Corrupt)); }
                builder = builder.header(name, value);
            }
            let progress = Arc::new(Mutex::new(UploadProgress {
                last_progress: tokio::time::Instant::now(),
                remaining: request.content_length.unwrap_or(0),
            }));
            match (request.body, request.content_length) {
                (Some(body), Some(length)) => {
                    let reader = UploadReader { reader: super::progress::network_reader(body, true), progress: progress.clone() };
                    builder = builder.header(reqwest::header::CONTENT_LENGTH, length)
                        .body(reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::with_capacity(
                            reader.take(length), 64 * 1024,
                        )));
                }
                (None, Some(0)) => { builder = builder.header(reqwest::header::CONTENT_LENGTH, 0); }
                (None, None) => {}
                _ => return Err(ProviderError::new(ErrorKind::Corrupt)),
            }
            let response = tokio::select! {
                _ = cancel.cancelled() => return Err(ProviderError::new(ErrorKind::Cancelled)),
                detail = upload_watchdog(progress) => return Err(ProviderError::new(ErrorKind::Transient).caused(&detail)),
                result = builder.send_logged("external-storage") => result.map_err(network_error)?,
            };
            let status = response.status().as_u16();
            let mut headers: BTreeMap<String, String> = BTreeMap::new();
            for (name, value) in response.headers() {
                let value = value.to_str().map_err(|_| super::providers::common::error(ErrorKind::Corrupt, status))?;
                if let Some(existing) = headers.get_mut(name.as_str()) {
                    if matches!(name.as_str(), "etag" | "location" | "content-length" | "content-range") {
                        return Err(super::providers::common::error(ErrorKind::Corrupt, status));
                    }
                    existing.push_str(", ");
                    existing.push_str(value);
                } else { headers.insert(name.as_str().to_owned(), value.to_owned()); }
            }
            let reader: Pin<Box<dyn AsyncRead + Send>> = Box::pin(
                tokio_util::io::StreamReader::new(
                    response
                        .bytes_stream()
                        .map_err(|error| std::io::Error::other(network_error(error).cause.0.unwrap_or_else(|| "Response body read failed".into()))),
                ),
            );
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(ResponseBody::new(super::progress::network_reader(reader, false), cancel, None)),
            })
        })
    }
}

/// The cancellation future registers a waker even while a response body is
/// stalled. A plain flag check in poll_read would not wake that stalled read.
struct ResponseBody {
    reader: Pin<Box<dyn AsyncRead + Send>>,
    cancel: Cancellation,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
    deadline: Option<Instant>,
    deadline_wait: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    idle_wait: Pin<Box<tokio::time::Sleep>>,
}
impl ResponseBody {
    fn new(reader: Pin<Box<dyn AsyncRead + Send>>, cancel: &Cancellation, deadline: Option<Instant>) -> Self {
        let owned = cancel.clone();
        Self {
            reader, cancel: cancel.clone(),
            cancelled: Box::pin(async move { owned.cancelled().await }),
            deadline,
            idle_wait: Box::pin(tokio::time::sleep(TRANSFER_IDLE_TIMEOUT)),
            deadline_wait: deadline.map(|deadline| Box::pin(async move {
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            }) as Pin<Box<dyn Future<Output = ()> + Send>>),
        }
    }
    fn stopped(&self) -> Option<std::io::Error> {
        if self.cancel.check().is_err() {
            Some(std::io::Error::other("cancelled"))
        } else if self.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            Some(std::io::Error::new(std::io::ErrorKind::TimedOut, "The storage control request exceeded its deadline"))
        } else { None }
    }
}
impl AsyncRead for ResponseBody {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;
        if let Some(error) = self.stopped() { return Poll::Ready(Err(error)); }
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(std::io::Error::other("cancelled")));
        }
        if self.idle_wait.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("Response body made no progress for {} seconds", TRANSFER_IDLE_TIMEOUT.as_secs()))));
        }
        let before = buf.filled().len();
        let result = self.reader.as_mut().poll_read(cx, buf);
        if matches!(&result, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.idle_wait.as_mut().reset(tokio::time::Instant::now() + TRANSFER_IDLE_TIMEOUT);
        }
        match result {
            Poll::Ready(value) => match self.stopped() {
                Some(error) => Poll::Ready(Err(error)),
                None => Poll::Ready(value),
            },
            Poll::Pending => {
                if let Some(wait) = self.deadline_wait.as_mut() {
                    if wait.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "The storage control request exceeded its deadline")));
                    }
                }
                Poll::Pending
            }
        }
    }
}

pub(crate) struct HttpRequest {
    pub method: reqwest::Method,
    pub url: url::Url,
    // Authentication and transfer URLs must never be Debug or Serialize.
    pub headers: BTreeMap<String, String>,
    pub body: Option<Pin<Box<dyn AsyncRead + Send>>>,
    pub content_length: Option<u64>,
    pub operation: ProviderOperation,
    pub account: AccountKey,
    /// Set only by the adapter for its validated API host, never a transfer
    /// redirect. Virtual-hosted S3 buckets still belong to the same API account.
    pub api_request: bool,
    pub mybox_charge: Option<MyboxCharge>,
    /// Head, lease and other control I/O, including its streamed response body.
    pub control: bool,
}
pub(crate) struct HttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Pin<Box<dyn AsyncRead + Send>>,
}
pub(crate) trait HttpTransport: Send + Sync {
    /// No automatic authenticated redirects, retries or body buffering.
    fn send<'a>(&'a self, request: HttpRequest, cancel: &'a Cancellation) -> ProviderFuture<'a, HttpResponse>;
}
pub(crate) trait MyboxRequestBudget: Send + Sync {
    /// Only a real MYBOX allowance is persisted, before the one dispatch.
    fn reserve_mybox<'a>(&'a self, account: &'a AccountKey, charge: &'a MyboxCharge, now_ms: u64) -> ProviderFuture<'a, ()>;
}
pub(crate) trait Clock: Send + Sync { fn now_ms(&self) -> u64; }
pub(crate) struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
            .as_millis().min(u64::MAX as u128) as u64
    }
}

pub(crate) fn control_operation(operation: ProviderOperation) -> bool {
    matches!(operation, ProviderOperation::Metadata | ProviderOperation::List
        | ProviderOperation::DownloadUrl | ProviderOperation::UploadSession
        | ProviderOperation::CompleteUpload | ProviderOperation::CompareExchangeHead
        | ProviderOperation::ReplaceHead | ProviderOperation::Delete | ProviderOperation::Authenticate)
}

/// Call while constructing the provider request, before signing it. The HTTP
/// execution wrapper never adds or changes signed request headers.
pub(crate) fn bypass_cache(headers: &mut BTreeMap<String, String>) {
    headers.retain(|name, _| !name.eq_ignore_ascii_case("cache-control") && !name.eq_ignore_ascii_case("pragma"));
    headers.insert("cache-control".into(), "no-cache, no-store".into());
    headers.insert("pragma".into(), "no-cache".into());
}
fn header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}
fn has_cache_bypass(headers: &BTreeMap<String, String>) -> bool {
    header(headers, "cache-control").is_some_and(|value| value.split(',').any(|part| part.trim().eq_ignore_ascii_case("no-cache")))
}

#[derive(Clone, Debug)]
pub(crate) struct HttpClockSample {
    pub date_ms: Option<u64>,
    pub date_present: bool,
    pub local_before_ms: u64,
    pub local_after_ms: u64,
    pub round_trip_ms: u64,
    pub cache_bypass: bool,
    pub age_seconds: Option<u64>,
    pub cache_hit: bool,
    pub http_status: u16,
    /// Monotonic freshness belongs to this actual response, not to a later read.
    pub observed_at: Instant,
}
impl HttpClockSample {
    pub(crate) fn usable(&self) -> bool {
        self.validate().is_ok()
    }
    pub(crate) fn validate(&self) -> Result<()> {
        let reason = if self.date_ms.is_none() {
            Some(if self.date_present { "date-invalid" } else { "date-missing" })
        } else if self.cache_hit || self.age_seconds.is_some_and(|age| age > 0) {
            Some("sample-cached")
        } else if !self.cache_bypass {
            Some("cache-policy-invalid")
        } else if !(200..300).contains(&self.http_status) {
            Some("sample-status")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(super::clock::failure("sample", reason, format_args!(
                "http_status={} cache_bypass={} cache_hit={} age_seconds={} rtt_ms={}",
                self.http_status, self.cache_bypass, self.cache_hit,
                self.age_seconds.map_or_else(|| "none".into(), |age| age.to_string()), self.round_trip_ms)));
        }
        super::clock::validate_sample(self.date_ms.unwrap(), self.local_before_ms,
            self.local_after_ms, self.round_trip_ms)
    }
}

#[cfg(test)]
fn clock_sample_within_five_minutes(date_ms: u64, local_before_ms: u64, local_after_ms: u64, rtt_ms: u64) -> bool {
    super::clock::validate_sample(date_ms, local_before_ms, local_after_ms, rtt_ms).is_ok()
}

#[derive(Default)]
pub(crate) struct RequestState {
    pub backoff: AccountBackoff,
    samples: Mutex<HashMap<AccountKey, HttpClockSample>>,
}
static REQUEST_STATE: LazyLock<Arc<RequestState>> = LazyLock::new(|| Arc::new(RequestState::default()));
pub(crate) fn shared_request_state() -> Arc<RequestState> { REQUEST_STATE.clone() }

impl RequestState {
    /// A caller supplies the instant at which its control work resumed. Missing,
    /// cached, skewed and pre-resume observations never become a fresh proof.
    pub fn clock_sample_after(&self, account: &AccountKey, not_before: Instant) -> Result<Option<HttpClockSample>> {
        let samples = self.samples.lock().map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        Ok(samples.get(account).filter(|sample| sample.observed_at >= not_before && sample.usable()).cloned())
    }
    pub fn require_clock_sample_after(&self, account: &AccountKey, not_before: Instant) -> Result<HttpClockSample> {
        let samples = self.samples.lock().map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        let sample = samples.get(account)
            .ok_or_else(|| super::clock::failure("sample", "sample-missing", ""))?;
        if sample.observed_at < not_before {
            return Err(super::clock::failure("sample", "sample-expired",
                format_args!("sample_age_ms={}", sample.observed_at.elapsed().as_millis())));
        }
        sample.validate()?;
        Ok(sample.clone())
    }
    pub fn resolve_pending(&self, pending: &AccountKey, account: &AccountKey) -> Result<()> {
        self.backoff.resolve_pending(pending, account)?;
        let mut samples = self.samples.lock().map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        if let Some(source) = samples.remove(pending) {
            if samples.get(account).is_none_or(|target| target.observed_at < source.observed_at) {
                samples.insert(account.clone(), source);
            }
        }
        Ok(())
    }
    /// Provider decoders call this after narrowing an overloaded status from
    /// its documented response fields. Statuses already recorded by `send`
    /// are ignored so one response advances the failure streak only once.
    pub fn observe_classified_error(
        &self,
        account: &AccountKey,
        error: &ProviderError,
        headers: &BTreeMap<String, String>,
        now_ms: u64,
    ) -> Result<()> {
        if !matches!(
            error.kind,
            ErrorKind::Transient | ErrorKind::RateLimited | ErrorKind::DailyQuotaExhausted
        ) {
            return Ok(());
        }
        let status = error.http_status.unwrap_or_default();
        let retry_after = super::providers::common::retry_after_ms(headers, now_ms);
        if status == 429
            || (500..=599).contains(&status)
            || (status == 403 && retry_after.is_some())
        {
            if let Some(until) = error.retry_at_ms {
                self.backoff.defer_for(
                    account,
                    Duration::from_millis(until.saturating_sub(now_ms)),
                    Instant::now(),
                )?;
            }
            return Ok(());
        }
        self.backoff.failure(
            account,
            error.kind,
            error
                .retry_at_ms
                .map(|until| Duration::from_millis(until.saturating_sub(now_ms))),
            Instant::now(),
        )
    }
    fn observe(&self, account: &AccountKey, response: &HttpResponse, bypass: bool, before: u64, after: u64, started: Instant, observed_at: Instant) -> Result<()> {
        let age_header = header(&response.headers, "age");
        let age_seconds = age_header.and_then(|value| value.trim().parse::<u64>().ok());
        let cache_hit = ["x-cache", "x-cache-status", "cf-cache-status", "x-proxy-cache"].into_iter()
            .filter_map(|name| header(&response.headers, name)).any(|value| {
                let value = value.to_ascii_lowercase();
                value.contains("hit") || value.contains("stale") || value.contains("revalidated")
                    || value.contains("updating")
                    || !(value.contains("miss") || value.contains("bypass") || value.contains("dynamic"))
            });
        let date_ms = header(&response.headers, "date")
            .and_then(|value| httpdate::parse_http_date(value).ok())
            .and_then(|date| date.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| u64::try_from(duration.as_millis()).ok());
        let round_trip_ms = u64::try_from(observed_at.saturating_duration_since(started).as_millis()).unwrap_or(u64::MAX);
        let cache_bypass = bypass && (age_header.is_none() || age_seconds.is_some());
        let sample = HttpClockSample {
            date_ms, date_present: header(&response.headers, "date").is_some(),
            local_before_ms: before, local_after_ms: after,
            round_trip_ms, cache_bypass,
            age_seconds, cache_hit, http_status: response.status, observed_at,
        };
        if sample.usable() {
            observe_time_sample(TimeSample {
                date_ms,
                local_before_ms: before,
                local_after_ms: after,
                round_trip_ms,
                cache_bypassed: cache_bypass,
                cache_hit,
                age_ms: age_seconds.map(|age| age.saturating_mul(1_000)),
                status: response.status,
            });
        }
        self.samples.lock().map_err(|_| ProviderError::new(ErrorKind::Transient))?.insert(account.clone(), sample);
        Ok(())
    }
}

async fn within_deadline<T>(operation: impl Future<Output = Result<T>>, cancel: &Cancellation, deadline: Option<Instant>) -> Result<T> {
    use futures::future::{select, Either};
    cancel.check()?;
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(ProviderError::new(ErrorKind::Transient));
    }
    let stopped = async {
        match deadline {
            Some(deadline) => {
                match select(Box::pin(cancel.cancelled()), Box::pin(async {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                })).await {
                    Either::Left(_) => ProviderError::new(ErrorKind::Cancelled),
                    Either::Right(_) => ProviderError::new(ErrorKind::Transient),
                }
            }
            None => { cancel.cancelled().await; ProviderError::new(ErrorKind::Cancelled) }
        }
    };
    match select(Box::pin(operation), Box::pin(stopped)).await {
        Either::Left((result, _)) => result,
        Either::Right((error, _)) => Err(error),
    }
}

pub(crate) async fn wait_until_ready(
    clock: &dyn Clock,
    state: &RequestState,
    account: &AccountKey,
    control: bool,
    cancel: &Cancellation,
) -> Result<Option<Instant>> {
    let deadline = control.then(|| Instant::now() + CONTROL_REQUEST_TIMEOUT);
    if let Err(mut error) = state.backoff.wait(account, cancel, deadline).await {
        if error.kind == ErrorKind::RateLimited {
            let remaining = state.backoff.remaining(account, Instant::now())?;
            error.retry_at_ms = Some(
                clock.now_ms().saturating_add(
                    u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
                ),
            );
        }
        return Err(error);
    }
    Ok(deadline)
}

pub(crate) async fn send(
    transport: &dyn HttpTransport,
    budget: &dyn MyboxRequestBudget,
    clock: &dyn Clock,
    state: &RequestState,
    request: HttpRequest,
    cancel: &Cancellation,
) -> Result<HttpResponse> {
    cancel.check()?;
    let deadline = request.control.then(|| Instant::now() + CONTROL_REQUEST_TIMEOUT);
    let account = request.account.clone();
    wait_until_ready(clock, state, &account, request.control, cancel).await?;
    if let Some(charge) = &request.mybox_charge {
        if account.provider() != "mybox" || account.is_pending() { return Err(ProviderError::new(ErrorKind::Corrupt)); }
        if let Err(error) = within_deadline(budget.reserve_mybox(&account, charge, clock.now_ms()), cancel, deadline).await {
            if let Some(until) = error.retry_at_ms {
                state.backoff.failure(&account, error.kind, Some(Duration::from_millis(until.saturating_sub(clock.now_ms()))), Instant::now())?;
            }
            return Err(error);
        }
    }
    // A different connection can extend the wait while a MYBOX reservation is
    // persisted. Recheck without refunding that durable reservation.
    state.backoff.wait(&account, cancel, deadline).await?;
    dispatch_ready(transport, clock, state, request, cancel, deadline).await
}

/// A provider signed this request after its normal account wait. If another
/// in-flight response imposed a new pause while it was signing, return without
/// dispatch so the provider can construct a fresh signature on its next run.
pub(crate) async fn send_signed(
    transport: &dyn HttpTransport,
    clock: &dyn Clock,
    state: &RequestState,
    request: HttpRequest,
    cancel: &Cancellation,
    deadline: Option<Instant>,
) -> Result<HttpResponse> {
    cancel.check()?;
    if request.mybox_charge.is_some() {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let remaining = state.backoff.remaining(&request.account, Instant::now())?;
    if !remaining.is_zero() {
        return Err(ProviderError {
            kind: ErrorKind::RateLimited,
            http_status: None,
            retry_at_ms: Some(
                clock.now_ms().saturating_add(
                    u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
                ),
            ),
            oauth_error: None,
            oauth_error_description: None,
            cause: Default::default(),
            connection_id: None,
        });
    }
    dispatch_ready(transport, clock, state, request, cancel, deadline).await
}

async fn dispatch_ready(
    transport: &dyn HttpTransport,
    clock: &dyn Clock,
    state: &RequestState,
    request: HttpRequest,
    cancel: &Cancellation,
    deadline: Option<Instant>,
) -> Result<HttpResponse> {
    cancel.check()?;
    let account = request.account.clone();
    let propfind = request.method.as_str() == "PROPFIND";
    // HTTP caches never store a PROPFIND response, so it is always the origin's own.
    let bypass = propfind || has_cache_bypass(&request.headers);
    let body_deadline = if request.operation == ProviderOperation::DownloadUrl { None } else { deadline };
    let api_response = request.api_request
        && (propfind || matches!(request.method, reqwest::Method::GET | reqwest::Method::HEAD));
    let before = clock.now_ms();
    let started = Instant::now();
    let result = within_deadline(transport.send(request, cancel), cancel, deadline).await;
    let observed_at = Instant::now();
    let after = clock.now_ms();
    let mut response = match result {
        Ok(response) => response,
        Err(error) => {
            if error.kind != ErrorKind::Cancelled {
                state.backoff.failure(&account, error.kind,
                    error.retry_at_ms.map(|until| Duration::from_millis(until.saturating_sub(after))), observed_at)?;
            }
            return Err(error);
        }
    };
    let retry_after = super::providers::common::retry_after_ms(&response.headers, after)
        .map(|until| Duration::from_millis(until.saturating_sub(after)));
    // Creation acknowledgements may contain HTML; 204 responses have no content.
    if !matches!(response.status, 201 | 204) && (200..300).contains(&response.status) && response.headers.get("content-type")
        .is_some_and(|value| value.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("text/html")) {
        return Err(super::providers::common::error(ErrorKind::EndpointRejected, response.status)
            .caused("The server returned an HTML page instead of storage data (Content-Type: text/html)"));
    }
    match response.status {
        200..=299 => state.backoff.success(&account)?,
        429 => state.backoff.failure(&account, ErrorKind::RateLimited, retry_after, observed_at)?,
        500..=599 => {
            let kind = super::providers::common::classify_status(response.status, &response.headers, after).kind;
            state.backoff.failure(&account, kind, retry_after, observed_at)?;
        }
        403 if retry_after.is_some() => state.backoff.failure(&account, ErrorKind::RateLimited, retry_after, observed_at)?,
        _ => {}
    }
    // A storage CDN's Date is not evidence about the configured API authority.
    if api_response { state.observe(&account, &response, bypass, before, after, started, observed_at)?; }
    cancel.check()?;
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) { return Err(ProviderError::new(ErrorKind::Transient)); }
    response.body = Box::pin(ResponseBody::new(response.body, cancel, body_deadline));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::quota_profiles::{MyboxCounter, MyboxPlan};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;

    #[test]
    fn webdav_can_follow_http_redirects_and_other_providers_keep_their_configured_origin() {
        let parse = |value: &str| url::Url::parse(value).unwrap();
        // A connection being committed has no account yet.
        let config = |provider: &str, endpoint: &str, addressing: &str| serde_json::from_value::<ConnectionConfig>(serde_json::json!({
            "provider":provider,"endpoint":endpoint,"accountId":"","profile":"generic",
            "location":{"bucket":"chosen","region":"us-east-1","addressing":addressing}
        })).unwrap();
        let default = NativeHttpTransport::new().unwrap();
        assert!(!default.permits_url(&parse("http://storage.invalid/data")));
        for provider in ["webdav","gitlab_packages"] {
            let transport = NativeHttpTransport::new().unwrap().with_user_endpoint(&config(provider,"http://storage.invalid:8080/root","path")).unwrap();
            assert!(transport.permits_url(&parse("http://storage.invalid:8080/root/item")));
            for other in ["http://storage.invalid/root", "http://other.invalid:8080/root"] {
                assert_eq!(transport.permits_url(&parse(other)), provider == "webdav");
            }
        }
        for (addressing, expected, rejected) in [
            ("path","http://storage.invalid:8080/chosen/a","http://chosen.storage.invalid:8080/a"),
            ("virtual","http://chosen.storage.invalid:8080/a","http://storage.invalid:8080/chosen/a")
        ] {
            let transport=NativeHttpTransport::new().unwrap().with_user_endpoint(&config("s3","http://storage.invalid:8080",addressing)).unwrap();
            assert!(transport.permits_url(&parse(expected)));
            assert!(!transport.permits_url(&parse(rejected)));
            assert!(!transport.permits_url(&parse("http://other.storage.invalid:8080/a")));
        }
        let secure=NativeHttpTransport::new().unwrap().with_user_endpoint(&config("webdav","https://storage.invalid","path")).unwrap();
        assert!(secure.permits_url(&parse("http://storage.invalid/a")));
        let fixed=NativeHttpTransport::new().unwrap().with_user_endpoint(&config("google_drive","http://storage.invalid","path")).unwrap();
        assert!(!fixed.permits_url(&parse("http://storage.invalid/a")));
    }

    #[derive(Default)]
    struct Boundary { deny: AtomicBool, reservations: AtomicUsize, sends: AtomicUsize }
    impl Clock for Boundary { fn now_ms(&self) -> u64 { 1_700_000_000_000 } }
    impl MyboxRequestBudget for Boundary {
        fn reserve_mybox<'a>(&'a self, _: &'a AccountKey, _: &'a MyboxCharge, _: u64) -> ProviderFuture<'a, ()> {
            Box::pin(async move {
                self.reservations.fetch_add(1, Ordering::SeqCst);
                if self.deny.load(Ordering::SeqCst) { Err(ProviderError::new(ErrorKind::DailyQuotaExhausted)) } else { Ok(()) }
            })
        }
    }
    impl HttpTransport for Boundary {
        fn send<'a>(&'a self, _: HttpRequest, _: &'a Cancellation) -> ProviderFuture<'a, HttpResponse> {
            Box::pin(async move {
                self.sends.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::new(ErrorKind::Transient))
            })
        }
    }
    fn request(provider: &str, principal: &str) -> HttpRequest {
        HttpRequest {
            method: reqwest::Method::PUT, url: url::Url::parse("https://synthetic.invalid/head").unwrap(),
            headers: BTreeMap::new(), body: None, content_length: Some(0),
            operation: ProviderOperation::ReplaceHead,
            account: AccountKey::new(provider, &url::Url::parse("https://synthetic.invalid").unwrap(), principal).unwrap(),
            api_request: true, mybox_charge: None, control: true,
        }
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn non_mybox_head_loss_dispatches_once_without_a_durable_reservation() {
        let boundary = Boundary::default();
        boundary.deny.store(true, Ordering::SeqCst);
        assert!(futures::executor::block_on(send(&boundary, &boundary, &boundary,
            &RequestState::default(), request("s3", "a"), &Cancellation::default())).is_err());
        assert_eq!(boundary.sends.load(Ordering::SeqCst), 1);
        assert_eq!(boundary.reservations.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn actual_mybox_denial_precedes_any_dispatch() {
        let boundary = Boundary::default();
        boundary.deny.store(true, Ordering::SeqCst);
        let mut request = request("mybox", "a");
        request.mybox_charge = Some(MyboxCharge { plan: MyboxPlan::Plan30gb, counters: vec![MyboxCounter::DownloadDay] });
        let error = futures::executor::block_on(send(&boundary, &boundary, &boundary,
            &RequestState::default(), request, &Cancellation::default())).err().unwrap();
        assert_eq!(error.kind, ErrorKind::DailyQuotaExhausted);
        assert_eq!(boundary.sends.load(Ordering::SeqCst), 0);
        assert_eq!(boundary.reservations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retry_after_is_shared_before_the_body_is_returned_but_not_by_another_account() {
        struct Throttled(AtomicUsize);
        impl HttpTransport for Throttled {
            fn send<'a>(&'a self, _: HttpRequest, _: &'a Cancellation) -> ProviderFuture<'a, HttpResponse> {
                Box::pin(async move {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Ok(HttpResponse { status: 429,
                        headers: BTreeMap::from([("retry-after".into(), "3600".into())]),
                        body: Box::pin(std::io::Cursor::new(Vec::new())), })
                })
            }
        }
        runtime().block_on(async {
            let transport = Throttled(AtomicUsize::new(0));
            let boundary = Boundary::default();
            let state = RequestState::default();
            let cancel = Cancellation::default();
            assert_eq!(send(&transport, &boundary, &boundary, &state, request("s3", "a"), &cancel).await.unwrap().status, 429);
            let error = send(&transport, &boundary, &boundary, &state, request("s3", "a"), &cancel).await.err().unwrap();
            assert_eq!(error.kind, ErrorKind::RateLimited);
            assert!(error.retry_at_ms.unwrap() > boundary.now_ms() + 3_590_000);
            assert_eq!(transport.0.load(Ordering::SeqCst), 1);
            assert_eq!(send(&transport, &boundary, &boundary, &state, request("s3", "b"), &cancel).await.unwrap().status, 429);
            assert_eq!(transport.0.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn provider_classification_records_new_failures_and_extends_already_counted_waits() {
        let state = RequestState::default();
        let account = request("github_releases", "a").account;
        let now_ms = 1_700_000_000_000;
        let before = Instant::now();
        state
            .observe_classified_error(
                &account,
                &ProviderError {
                    kind: ErrorKind::RateLimited,
                    http_status: Some(403),
                    retry_at_ms: Some(now_ms + 3_600_000),
                    oauth_error: None,
                    oauth_error_description: None,
                    cause: Default::default(),
                    connection_id: None,
                },
                &BTreeMap::from([
                    ("x-ratelimit-remaining".into(), "0".into()),
                    ("x-ratelimit-reset".into(), "1700003600".into()),
                ]),
                now_ms,
            )
            .unwrap();
        assert!(state.backoff.remaining(&account, before).unwrap() >= Duration::from_secs(3_599));

        let already_observed = request("github_releases", "b").account;
        state
            .observe_classified_error(
                &already_observed,
                &ProviderError {
                    kind: ErrorKind::RateLimited,
                    http_status: Some(429),
                    retry_at_ms: Some(now_ms + 3_600_000),
                    oauth_error: None,
                    oauth_error_description: None,
                    cause: Default::default(),
                    connection_id: None,
                },
                &BTreeMap::from([("retry-after".into(), "3600".into())]),
                now_ms,
            )
            .unwrap();
        assert!(
            state
                .backoff
                .remaining(&already_observed, Instant::now())
                .unwrap()
                >= Duration::from_secs(3_599)
        );
    }

    #[test]
    fn a_new_pause_after_signing_returns_without_dispatch() {
        let boundary = Boundary::default();
        let state = RequestState::default();
        let request = request("s3", "signed-account");
        state
            .backoff
            .failure(
                &request.account,
                ErrorKind::RateLimited,
                Some(Duration::from_secs(3_600)),
                Instant::now(),
            )
            .unwrap();
        let error = futures::executor::block_on(send_signed(
            &boundary,
            &boundary,
            &state,
            request,
            &Cancellation::default(),
            None,
        ))
        .err()
        .unwrap();
        assert_eq!(error.kind, ErrorKind::RateLimited);
        assert_eq!(boundary.sends.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn clock_bounds_include_truncation_rtt_wall_jumps_and_integer_limits() {
        assert!(clock_sample_within_five_minutes(1_000_000, 1_000_000, 1_000_100, 100));
        assert!(clock_sample_within_five_minutes(299_000, 0, 0, 0));
        assert!(!clock_sample_within_five_minutes(299_001, 0, 0, 0));
        assert!(clock_sample_within_five_minutes(0, 10, 9, 0));
        assert!(clock_sample_within_five_minutes(0, 0, 1001, 0));
        assert!(!clock_sample_within_five_minutes(300_000, 0, 600_001, 600_001));
        assert!(clock_sample_within_five_minutes(u64::MAX, u64::MAX, u64::MAX, 0));
        let base = 1_700_000_000_000_u64;
        for offset in [-539_i64, 539] {
            for phase in [0, 499, 999] {
                for rtt in [0, 100, 800] {
                    let before = (base as i64 + phase as i64 + offset) as u64;
                    let date = base + (phase + rtt / 2) / 1000 * 1000;
                    assert!(clock_sample_within_five_minutes(date, before, before + rtt, rtt));
                }
            }
        }
        for delta in [-5001_i64, -5000, -4999, -539, 539, 4999, 5000, 5001] {
            let before = base + 20_000;
            let after = (before as i64 + 100 + delta) as u64;
            let result = super::super::clock::validate_sample(base, before, after, 100);
            assert_eq!(result.is_ok(), delta.abs() <= 5000, "{delta}");
            if let Err(error) = result {
                let detail = error.cause.0.unwrap();
                assert!(detail.contains("reason=wall-clock-jump"));
                assert!(detail.contains(&format!("delta_ms={delta}")));
                assert!(detail.contains("limit_ms=5000"));
            }
        }
        for (before, after) in [(299_000, 300_000), (300_000, 299_000)] {
            assert!(clock_sample_within_five_minutes(base, base + before, base + after, 0));
            let error = super::super::clock::validate_sample(base, base + before + 1, base + after + 1, 0).unwrap_err();
            assert!(error.cause.0.unwrap().contains("reason=server-offset"));
        }
    }

    #[test]
    fn missing_cached_invalid_and_stale_samples_never_validate_a_clock() {
        let state = RequestState::default();
        let account = request("s3", "a").account;
        let started = Instant::now();
        let now = 1_700_000_000_000;
        let date = httpdate::fmt_http_date(std::time::UNIX_EPOCH + Duration::from_millis(now));
        for (status, extra, bypass, reason) in [
            (200, Some(("age", "1")), true, "sample-cached"), (200, Some(("age", "invalid")), true, "cache-policy-invalid"),
            (200, Some(("x-cache", "HIT")), true, "sample-cached"), (200, Some(("cf-cache-status", "REVALIDATED")), true, "sample-cached"),
            (304, None, true, "sample-status"), (403, None, true, "sample-status"), (200, None, false, "cache-policy-invalid"),
        ] {
            let mut headers = BTreeMap::from([("date".into(), date.clone())]);
            if let Some((key, value)) = extra { headers.insert(key.into(), value.into()); }
            let response = HttpResponse { status, headers, body: Box::pin(std::io::Cursor::new(Vec::new())) };
            state.observe(&account, &response, bypass, now, now, started, Instant::now()).unwrap();
            assert!(state.clock_sample_after(&account, started).unwrap().is_none());
            let error = state.require_clock_sample_after(&account, started).unwrap_err();
            assert_eq!(error.kind, ErrorKind::ClockSkew);
            assert!(error.cause.0.unwrap().contains(&format!("reason={reason}")));
        }
        let mut response = HttpResponse { status: 200, headers: BTreeMap::new(), body: Box::pin(std::io::Cursor::new(Vec::new())) };
        state.observe(&account, &response, true, now, now, started, Instant::now()).unwrap();
        assert!(state.clock_sample_after(&account, started).unwrap().is_none());
        assert!(state.require_clock_sample_after(&account, started).unwrap_err().cause.0.unwrap().contains("reason=date-missing"));
        response.headers.insert("date".into(), "synthetic-private-invalid-date".into());
        state.observe(&account, &response, true, now, now, started, Instant::now()).unwrap();
        let error = state.require_clock_sample_after(&account, started).unwrap_err();
        let detail = error.cause.0.unwrap();
        assert!(detail.contains("reason=date-invalid"));
        assert!(!detail.contains("synthetic-private"));
        response.headers.insert("date".into(), date);
        state.observe(&account, &response, true, now, now, started, Instant::now()).unwrap();
        assert!(state.clock_sample_after(&account, started).unwrap().is_some());
        assert!(state.clock_sample_after(&account, Instant::now() + Duration::from_millis(1)).unwrap().is_none());
        assert!(state.require_clock_sample_after(&account, Instant::now() + Duration::from_millis(1))
            .unwrap_err().cause.0.unwrap().contains("reason=sample-expired"));
    }

    #[test]
    fn propfind_responses_sample_the_clock_without_a_cache_directive_but_keep_the_cache_guards() {
        struct Dated(BTreeMap<String, String>);
        impl HttpTransport for Dated {
            fn send<'a>(&'a self, _: HttpRequest, _: &'a Cancellation) -> ProviderFuture<'a, HttpResponse> {
                Box::pin(async move { Ok(HttpResponse { status: 207, headers: self.0.clone(),
                    body: Box::pin(std::io::Cursor::new(Vec::new())) }) })
            }
        }
        let boundary = Boundary::default();
        let date = httpdate::fmt_http_date(std::time::UNIX_EPOCH + Duration::from_millis(boundary.now_ms()));
        let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();
        let dated = |extra: Option<(&str, &str)>| {
            let mut headers = BTreeMap::from([("date".to_owned(), date.clone())]);
            if let Some((name, value)) = extra { headers.insert(name.into(), value.into()); }
            Dated(headers)
        };
        runtime().block_on(async {
            for (method, extra, api, usable) in [
                (propfind.clone(), None, true, true),
                (propfind.clone(), Some(("age", "3")), true, false),
                (propfind.clone(), Some(("x-cache", "HIT")), true, false),
                (propfind.clone(), None, false, false),
                (reqwest::Method::GET, None, true, false),
                (reqwest::Method::PUT, None, true, false),
            ] {
                let state = RequestState::default();
                let mut sent = request("webdav", "synthetic");
                sent.method = method.clone();
                sent.api_request = api;
                let account = sent.account.clone();
                let started = Instant::now();
                dispatch_ready(&dated(extra), &boundary, &state, sent, &Cancellation::default(), None).await.unwrap();
                assert_eq!(state.clock_sample_after(&account, started).unwrap().is_some(), usable, "{method} {extra:?} api={api}");
            }
        });
    }

    #[test]
    fn clock_samples_are_account_scoped_and_the_latest_response_wins() {
        let state = RequestState::default();
        let first = request("s3", "first").account;
        let second = request("s3", "second").account;
        let started = Instant::now();
        let now = 1_700_000_000_000;
        let date = httpdate::fmt_http_date(
            std::time::UNIX_EPOCH + Duration::from_millis(now),
        );
        let usable = HttpResponse {
            status: 200,
            headers: BTreeMap::from([("date".into(), date)]),
            body: Box::pin(std::io::Cursor::new(Vec::new())),
        };
        state
            .observe(&first, &usable, true, now, now, started, Instant::now())
            .unwrap();
        let unusable = HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: Box::pin(std::io::Cursor::new(Vec::new())),
        };
        state
            .observe(&second, &unusable, true, now, now, started, Instant::now())
            .unwrap();
        assert!(state.clock_sample_after(&first, started).unwrap().is_some());
        assert!(state.clock_sample_after(&second, started).unwrap().is_none());
        state
            .observe(&first, &unusable, true, now, now, started, Instant::now())
            .unwrap();
        assert!(state.clock_sample_after(&first, started).unwrap().is_none());
    }

    struct Stalled;
    impl AsyncRead for Stalled {
        fn poll_read(self: Pin<&mut Self>, _: &mut std::task::Context<'_>, _: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }
    #[test]
    fn untrusted_tls_is_endpoint_rejection_while_refusal_is_transient() {
        use std::io::Read;
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions().unwrap().with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into()).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut stream = rustls::StreamOwned::new(rustls::ServerConnection::new(Arc::new(config)).unwrap(), socket);
            let _ = stream.read(&mut [0]);
        });
        runtime().block_on(async {
            let transport = NativeHttpTransport::for_loopback_tests();
            let mut tls = request("webdav", "synthetic");
            tls.url = url::Url::parse(&format!("https://localhost:{}/synthetic?secret=must-not-leak", address.port())).unwrap();
            let error = transport.send(tls, &Cancellation::default()).await.err().unwrap();
            assert_eq!(error.kind, ErrorKind::EndpointRejected);
            assert_eq!(error.http_status, None);
            assert_eq!(error.oauth_error_description, None);
            let detail = error.cause.0.as_deref().unwrap();
            assert!(detail.contains("TLS handshake failed"));
            assert!(!detail.contains("must-not-leak"));
            server.join().unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            drop(listener);
            let mut refused = request("webdav", "synthetic");
            refused.url = url::Url::parse(&format!("http://{address}/synthetic")).unwrap();
            let error = transport.send(refused, &Cancellation::default()).await.err().unwrap();
            assert_eq!(error.kind, ErrorKind::Transient);
            assert!(error.cause.0.as_deref().is_some_and(|detail| detail.len() > "network failure".len()));
        });
    }
    #[test]
    fn transfer_idle_deadlines_reset_only_on_progress_and_headers_have_a_separate_bound() {
        runtime().block_on(async {
            tokio::time::pause();
            let progress = Arc::new(Mutex::new(UploadProgress { last_progress: tokio::time::Instant::now(), remaining: 6 }));
            let mut reader = UploadReader { reader: Box::pin(std::io::Cursor::new(vec![7; 6])), progress: progress.clone() };
            let watchdog = upload_watchdog(progress.clone());
            tokio::pin!(watchdog);
            for _ in 0..6 {
                tokio::time::advance(Duration::from_secs(50)).await;
                assert!(futures::poll!(&mut watchdog).is_pending());
                assert_eq!(reader.read(&mut [0]).await.unwrap(), 1);
            }
            tokio::time::advance(Duration::from_secs(119)).await;
            assert!(futures::poll!(&mut watchdog).is_pending());
            tokio::time::advance(Duration::from_secs(1)).await;
            let detail = tokio::time::timeout(Duration::from_millis(2), &mut watchdog).await
                .expect("headers must time out within one timer tick of the deadline");
            assert_eq!(detail, "The server sent no response headers for 120 seconds");

            let cancel = Cancellation::default();
            let mut body = ResponseBody::new(Box::pin(std::io::Cursor::new(vec![1; 7])), &cancel, None);
            for _ in 0..6 {
                tokio::time::advance(Duration::from_secs(50)).await;
                assert_eq!(body.read(&mut [0]).await.unwrap(), 1);
            }
            tokio::time::advance(TRANSFER_IDLE_TIMEOUT + Duration::from_millis(2)).await;
            assert_eq!(body.read(&mut [0]).await.unwrap_err().kind(), std::io::ErrorKind::TimedOut);

            let progress = Arc::new(Mutex::new(UploadProgress { last_progress: tokio::time::Instant::now(), remaining: 1 }));
            let watchdog = upload_watchdog(progress);
            tokio::pin!(watchdog);
            assert!(futures::poll!(&mut watchdog).is_pending());
            tokio::time::advance(TRANSFER_IDLE_TIMEOUT - Duration::from_secs(1)).await;
            assert!(futures::poll!(&mut watchdog).is_pending());
            tokio::time::advance(Duration::from_secs(1)).await;
            let detail = tokio::time::timeout(Duration::from_millis(2), &mut watchdog).await
                .expect("uploads must time out within one timer tick of the idle deadline");
            assert_eq!(detail, "Upload made no progress for 90 seconds");
        });
    }

    struct SlowResponseBody { wait: Pin<Box<tokio::time::Sleep>> }
    impl AsyncRead for SlowResponseBody {
        fn poll_read(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
            if self.wait.as_mut().poll(cx).is_pending() { return std::task::Poll::Pending; }
            buf.put_slice(&[7]);
            self.wait.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(50));
            std::task::Poll::Ready(Ok(()))
        }
    }
    struct SyntheticResponse { status: u16, html: bool }
    impl HttpTransport for SyntheticResponse {
        fn send<'a>(&'a self, _: HttpRequest, _: &'a Cancellation) -> ProviderFuture<'a, HttpResponse> {
            Box::pin(async move { Ok(HttpResponse {
                status: self.status,
                headers: if self.html { BTreeMap::from([("content-type".into(), "text/html; charset=utf-8".into())]) } else { BTreeMap::new() },
                body: Box::pin(SlowResponseBody { wait: Box::pin(tokio::time::sleep(Duration::from_secs(50))) }),
            }) })
        }
    }
    #[test]
    fn download_url_body_outlives_control_deadline_and_html_is_endpoint_rejection() {
        runtime().block_on(async {
            tokio::time::pause();
            let boundary = Boundary::default();
            let cancel = Cancellation::default();
            let mut download = request("onedrive", "synthetic");
            download.operation = ProviderOperation::DownloadUrl;
            let mut response = dispatch_ready(&SyntheticResponse { status: 200, html: false }, &boundary, &RequestState::default(), download, &cancel, Some(Instant::now() + CONTROL_REQUEST_TIMEOUT)).await.unwrap();
            for _ in 0..6 {
                assert_eq!(response.body.read(&mut [0]).await.unwrap(), 1);
            }
            for status in [200, 206] {
                let error = dispatch_ready(&SyntheticResponse { status, html: true }, &boundary, &RequestState::default(), request("webdav", "synthetic"), &cancel, None).await.err().unwrap();
                assert_eq!(error.kind, ErrorKind::EndpointRejected);
                assert_eq!(error.http_status, Some(status));
            }
        });
    }
    #[test]
    fn control_deadlines_cover_headers_and_stalled_bodies_and_cancellation_wakes_reads() {
        runtime().block_on(async {
            let cancel = Cancellation::default();
            let error = within_deadline(futures::future::pending::<Result<()>>(), &cancel,
                Some(Instant::now() + Duration::from_millis(1))).await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::Transient);
            let mut body = ResponseBody::new(Box::pin(Stalled), &cancel, Some(Instant::now() + Duration::from_millis(1)));
            assert_eq!(body.read(&mut [0]).await.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
            let mut body = ResponseBody::new(Box::pin(Stalled), &cancel, None);
            let mut buffer = [0];
            let read = body.read(&mut buffer);
            tokio::pin!(read);
            assert!(futures::poll!(&mut read).is_pending());
            cancel.cancel();
            assert_eq!(tokio::time::timeout(Duration::from_millis(100), read).await.unwrap().unwrap_err().kind(), std::io::ErrorKind::Other);
        });
    }
}
