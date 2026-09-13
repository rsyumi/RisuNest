//! Injected HTTP boundary. Every SDK subrequest and retry reserves durable
//! account budget here. There is deliberately no automatic retry loop.
use super::contract::*;
use std::{collections::BTreeMap, pin::Pin};
use tokio::io::AsyncRead;

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
