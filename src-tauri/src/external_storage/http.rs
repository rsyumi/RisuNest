//! Injected HTTP boundary. Every SDK subrequest and retry reserves durable
//! account budget here. There is deliberately no automatic retry loop.
use super::contract::*;
use std::{collections::BTreeMap, pin::Pin};
use tokio::io::AsyncRead;

/// Native provider HTTP, streamed in both directions. The injected `send` below
/// remains the quota boundary, including redirects explicitly followed by adapters.
pub(crate) struct NativeHttpTransport {
    client: reqwest::Client,
    #[cfg(test)]
    loopback_http: bool,
}
impl NativeHttpTransport {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|_| ProviderError::new(ErrorKind::Unsupported))?;
        Ok(Self {
            client,
            #[cfg(test)]
            loopback_http: false,
        })
    }
    #[cfg(test)]
    pub fn for_loopback_tests() -> Self {
        Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            loopback_http: true,
        }
    }
}
fn network_error(_: impl std::fmt::Display) -> ProviderError {
    // reqwest errors can contain signed URLs. Never propagate their text.
    ProviderError::new(ErrorKind::Transient)
}

impl HttpTransport for NativeHttpTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HttpResponse> {
        Box::pin(async move {
            use futures::{
                future::{select, Either},
                TryStreamExt,
            };
            use tokio::io::AsyncReadExt;
            cancel.check()?;
            let allowed = request.url.scheme() == "https";
            #[cfg(test)]
            let allowed = allowed
                || (self.loopback_http
                    && request.url.scheme() == "http"
                    && request.url.host_str() == Some("127.0.0.1"));
            if !allowed
                || !request.url.username().is_empty()
                || request.url.password().is_some()
                || request.url.fragment().is_some()
            {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            let mut builder = self.client.request(request.method, request.url);
            for (name, value) in request.headers {
                if name.eq_ignore_ascii_case("transfer-encoding")
                    || (name.eq_ignore_ascii_case("content-length")
                        && value.parse::<u64>().ok() != request.content_length)
                {
                    return Err(ProviderError::new(ErrorKind::Corrupt));
                }
                builder = builder.header(name, value);
            }
            match (request.body, request.content_length) {
                (Some(body), Some(length)) => {
                    builder = builder
                        .header(reqwest::header::CONTENT_LENGTH, length)
                        .body(reqwest::Body::wrap_stream(
                            tokio_util::io::ReaderStream::with_capacity(
                                body.take(length),
                                64 * 1024,
                            ),
                        ));
                }
                (None, None | Some(0)) => {}
                _ => return Err(ProviderError::new(ErrorKind::Corrupt)),
            }
            let response =
                match select(Box::pin(cancel.cancelled()), Box::pin(builder.send())).await {
                    Either::Left(_) => return Err(ProviderError::new(ErrorKind::Cancelled)),
                    Either::Right((result, _)) => result.map_err(network_error)?,
                };
            let status = response.status().as_u16();
            let mut headers: BTreeMap<String, String> = BTreeMap::new();
            for (name, value) in response.headers() {
                let value = value
                    .to_str()
                    .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
                // Never collapse repeated version/location headers into an arbitrary token.
                if let Some(existing) = headers.get_mut(name.as_str()) {
                    if matches!(
                        name.as_str(),
                        "etag" | "location" | "content-length" | "content-range"
                    ) {
                        return Err(ProviderError::new(ErrorKind::Corrupt));
                    }
                    existing.push_str(", ");
                    existing.push_str(value);
                } else {
                    headers.insert(name.as_str().to_owned(), value.to_owned());
                }
            }
            let reader = tokio_util::io::StreamReader::new(
                response
                    .bytes_stream()
                    .map_err(|_| std::io::Error::other("external-response-io")),
            );
            let owned_cancel = cancel.clone();
            Ok(HttpResponse {
                status,
                headers,
                body: Box::pin(ResponseBody {
                    reader: Box::pin(reader),
                    cancelled: Box::pin(async move { owned_cancel.cancelled().await }),
                }),
            })
        })
    }
}
struct ResponseBody {
    reader: Pin<Box<dyn AsyncRead + Send>>,
    cancelled: Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
}
impl AsyncRead for ResponseBody {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "cancelled",
            )));
        }
        self.reader.as_mut().poll_read(cx, buf)
    }
}

pub(crate) struct HttpRequest {
    pub method: reqwest::Method,
    pub url: url::Url,
    // May contain authentication and session URLs. Not Debug or Serialize.
    pub headers: BTreeMap<String, String>,
    pub body: Option<Pin<Box<dyn AsyncRead + Send>>>,
    pub content_length: Option<u64>,
    pub operation: ProviderOperation,
    pub costs: Vec<RequestCost>,
}
pub(crate) struct HttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Pin<Box<dyn AsyncRead + Send>>,
}
pub(crate) trait HttpTransport: Send + Sync {
    /// Implementations must disable automatic redirects carrying authentication,
    /// status retries and body buffering. Redirect URL requests get their own budget.
    fn send<'a>(
        &'a self,
        request: HttpRequest,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HttpResponse>;
}
pub(crate) trait RequestBudget: Send + Sync {
    /// Atomically persist consumption before the actual request is dispatched.
    fn reserve<'a>(&'a self, costs: &'a [RequestCost], now_ms: u64) -> ProviderFuture<'a, ()>;
}
pub(crate) trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}
pub(crate) struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64
    }
}
pub(crate) async fn send(
    transport: &dyn HttpTransport,
    budget: &dyn RequestBudget,
    clock: &dyn Clock,
    request: HttpRequest,
    cancel: &Cancellation,
) -> Result<HttpResponse> {
    cancel.check()?;
    budget.reserve(&request.costs, clock.now_ms()).await?;
    cancel.check()?;
    transport.send(request, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Boundary {
        deny: AtomicBool,
        reservations: AtomicUsize,
        sends: AtomicUsize,
    }
    impl Clock for Boundary {
        fn now_ms(&self) -> u64 {
            100
        }
    }
    impl RequestBudget for Boundary {
        fn reserve<'a>(&'a self, _: &'a [RequestCost], _: u64) -> ProviderFuture<'a, ()> {
            Box::pin(async move {
                self.reservations.fetch_add(1, Ordering::SeqCst);
                if self.deny.load(Ordering::SeqCst) {
                    Err(ProviderError::new(ErrorKind::DailyQuotaExhausted))
                } else {
                    Ok(())
                }
            })
        }
    }
    impl HttpTransport for Boundary {
        fn send<'a>(
            &'a self,
            _: HttpRequest,
            _: &'a Cancellation,
        ) -> ProviderFuture<'a, HttpResponse> {
            Box::pin(async move {
                self.sends.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::new(ErrorKind::Transient))
            })
        }
    }
    fn request() -> HttpRequest {
        HttpRequest {
            method: reqwest::Method::PUT,
            url: url::Url::parse("https://synthetic.invalid/head").unwrap(),
            headers: BTreeMap::new(),
            body: None,
            content_length: Some(0),
            operation: ProviderOperation::ReplaceHead,
            costs: Vec::new(),
        }
    }
    #[test]
    fn quota_denial_precedes_transport_and_head_response_loss_has_no_hidden_retry() {
        let boundary = Boundary {
            deny: AtomicBool::new(true),
            reservations: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
        };
        let cancel = Cancellation::default();
        assert!(futures::executor::block_on(send(
            &boundary,
            &boundary,
            &boundary,
            request(),
            &cancel
        ))
        .is_err());
        assert_eq!(boundary.sends.load(Ordering::SeqCst), 0);
        boundary.deny.store(false, Ordering::SeqCst);
        assert!(futures::executor::block_on(send(
            &boundary,
            &boundary,
            &boundary,
            request(),
            &cancel
        ))
        .is_err());
        assert_eq!(boundary.sends.load(Ordering::SeqCst), 1);
        assert_eq!(boundary.reservations.load(Ordering::SeqCst), 2);
    }
}
