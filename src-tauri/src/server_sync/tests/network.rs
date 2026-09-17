use super::*;
use axum::{
    body::Body,
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU16, AtomicU64, Ordering},
        Mutex,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

struct Rate {
    timer: Pin<Box<tokio::time::Sleep>>,
    bytes_per_second: u64,
    next: tokio::time::Instant,
}
impl Rate {
    fn new(bytes_per_second: u64) -> Self {
        Self {
            timer: Box::pin(tokio::time::sleep(Duration::ZERO)),
            bytes_per_second,
            next: tokio::time::Instant::now(),
        }
    }
    fn ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.bytes_per_second == 0 {
            Poll::Ready(())
        } else {
            self.timer.as_mut().poll(cx)
        }
    }
    fn used(&mut self, bytes: usize) {
        if self.bytes_per_second > 0 {
            let interval = Duration::from_secs_f64(bytes as f64 / self.bytes_per_second as f64);
            // Keep an absolute byte clock, with at most one packet of credit,
            // so timer quantization does not accumulate on every read.
            self.next = self.next.max(tokio::time::Instant::now() - interval) + interval;
            self.timer.as_mut().reset(self.next);
        }
    }
}
struct Socket {
    socket: tokio::net::TcpStream,
    read: Rate,
    write: Rate,
    bytes: Arc<AtomicU64>,
}
impl AsyncRead for Socket {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.read.ready(cx).is_pending() {
            return Poll::Pending;
        }
        let limit = output.remaining().min(64 * 1024);
        let mut buffer = ReadBuf::new(output.initialize_unfilled_to(limit));
        let result = Pin::new(&mut this.socket).poll_read(cx, &mut buffer);
        let count = buffer.filled().len();
        output.advance(count);
        this.read.used(count);
        this.bytes.fetch_add(count as u64, Ordering::Relaxed);
        result
    }
}
impl AsyncWrite for Socket {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.write.ready(cx).is_pending() {
            return Poll::Pending;
        }
        let result =
            Pin::new(&mut this.socket).poll_write(cx, &bytes[..bytes.len().min(64 * 1024)]);
        if let Poll::Ready(Ok(count)) = result {
            this.write.used(count);
            this.bytes.fetch_add(count as u64, Ordering::Relaxed);
        }
        result
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_shutdown(cx)
    }
}
struct Listener {
    listener: tokio::net::TcpListener,
    rate: u64,
    bytes: Arc<AtomicU64>,
}
impl axum::serve::Listener for Listener {
    type Io = Socket;
    type Addr = std::net::SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (socket, address) = self.listener.accept().await.unwrap();
        socket.set_nodelay(true).unwrap();
        (
            Socket {
                socket,
                read: Rate::new(self.rate),
                write: Rate::new(self.rate),
                bytes: self.bytes.clone(),
            },
            address,
        )
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}
#[derive(Default)]
struct Faults {
    code: AtomicU16,
    after_accept: bool,
    latency: Duration,
    chunks: Mutex<BTreeMap<String, usize>>,
    requests: AtomicU64,
    uploads_active: AtomicU64,
    downloads_active: AtomicU64,
    uploads_peak: AtomicU64,
    downloads_peak: AtomicU64,
    range_code: AtomicU16,
    ranges: Mutex<BTreeMap<String, usize>>,
}
struct InFlight<'a>(&'a AtomicU64);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
async fn proxy(State(state): State<Arc<Faults>>, request: Request, next: Next) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let counters = if request.uri().path().contains("/chunks/") {
        Some((&state.uploads_active, &state.uploads_peak))
    } else if request.method().as_str() == "GET" && request.uri().path().starts_with("/objects/") {
        Some((&state.downloads_active, &state.downloads_peak))
    } else {
        None
    };
    let _inflight = counters.map(|(active, peak)| {
        let count = active.fetch_add(1, Ordering::Relaxed) + 1;
        peak.fetch_max(count, Ordering::Relaxed);
        InFlight(active)
    });
    tokio::time::sleep(state.latency).await;
    let path = request.uri().path();
    if path.starts_with("/objects/") && request.method().as_str() == "GET" {
        if let Some(range) = request.headers().get("range") {
            let range = range.to_str().unwrap().to_owned();
            *state
                .ranges
                .lock()
                .unwrap()
                .entry(range.clone())
                .or_default() += 1;
            if range.starts_with("bytes=0-") {
                let code = state.range_code.swap(0, Ordering::Relaxed);
                if code > 0 {
                    return Response::builder()
                        .status(code)
                        .header("retry-after", "1")
                        .body(Body::empty())
                        .unwrap();
                }
            }
        }
    }
    if path.contains("/chunks/") {
        let index = path.rsplit('/').next().unwrap().to_owned();
        *state
            .chunks
            .lock()
            .unwrap()
            .entry(index.clone())
            .or_default() += 1;
        if index == "1" {
            let code = state.code.swap(0, Ordering::Relaxed);
            if code > 0 {
                if state.after_accept {
                    let _ = next.run(request).await;
                } else {
                    // Model a received HTTP error. Dropping the request while
                    // reqwest is writing produces a TCP reset instead (503),
                    // which is a separate transport failure.
                    axum::body::to_bytes(
                        request.into_body(),
                        risunest_sync_wire::transfer::MAX_BATCH_BYTES,
                    )
                    .await
                    .unwrap();
                }
                return Response::builder()
                    .status(code)
                    .header("retry-after", "1")
                    .body(Body::empty())
                    .unwrap();
            }
        }
    }
    next.run(request).await
}
fn run_case(
    rate: u64,
    latency: Duration,
    failure: u16,
    after_accept: bool,
    download_failure: u16,
) -> (u64, u64, u128, u128, u64, u64) {
    let root = tempfile::tempdir().unwrap();
    let server = Arc::new(Store::init(root.path()).unwrap());
    let device = server.add_device().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let bytes = Arc::new(AtomicU64::new(0));
    let faults = Arc::new(Faults {
        code: AtomicU16::new(failure),
        after_accept,
        latency,
        range_code: AtomicU16::new(download_failure),
        ..Default::default()
    });
    let faults_for_router = faults.clone();
    let counted = bytes.clone();
    let task = runtime.spawn(async move {
        let router = http::router(server).layer(axum::middleware::from_fn_with_state(
            faults_for_router,
            proxy,
        ));
        axum::serve(
            Listener {
                listener,
                rate,
                bytes: counted,
            },
            router,
        )
        .await
        .unwrap();
    });
    let client = ServerClient::new(ServerConfig {
        directory: None,
        endpoint,
        library_id: device.library_id,
        device_id: device.device_id,
        token: device.token,
    })
    .unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let source = Cache::open(source_dir.path()).unwrap();
    let target = Cache::open(target_dir.path()).unwrap();
    let payload: Vec<_> = (0..2 * risunest_sync_wire::transfer::UPLOAD_CHUNK_BYTES + 17)
        .map(|i| (i.wrapping_mul(137) % 251) as u8)
        .collect();
    let hash = source.put(&payload).unwrap();
    let started = Instant::now();
    {
        let transfer = Transfer::new(&client, &source).unwrap();
        if failure > 0 {
            let error = transfer
                .upload(std::slice::from_ref(&hash), &[])
                .unwrap_err();
            assert_eq!(
                error.status,
                failure,
                "{error:?}, attempts={:?}",
                faults.chunks.lock().unwrap()
            );
        } else {
            transfer.upload(std::slice::from_ref(&hash), &[]).unwrap();
        }
    }
    if failure > 0 {
        // Reopen the durable native journal, exactly as after process restart.
        Transfer::new(&client, &source)
            .unwrap()
            .upload(std::slice::from_ref(&hash), &[])
            .unwrap();
        let attempts = faults.chunks.lock().unwrap();
        assert_eq!(attempts.get("0"), Some(&1));
        assert_eq!(attempts.get("1"), Some(&if after_accept { 1 } else { 2 }));
    }
    let upload_ms = started.elapsed().as_millis();
    let started = Instant::now();
    if download_failure > 0 {
        let error = Transfer::new(&client, &target)
            .unwrap()
            .download(std::slice::from_ref(&hash), &[])
            .unwrap_err();
        assert_eq!(error.status, download_failure);
    }
    Transfer::new(&client, &target)
        .unwrap()
        .download(std::slice::from_ref(&hash), &[])
        .unwrap();
    let download_ms = started.elapsed().as_millis();
    assert_eq!(target.read(&hash, payload.len()).unwrap(), payload);
    if download_failure > 0 {
        let ranges = faults.ranges.lock().unwrap();
        assert_eq!(ranges.get("bytes=0-1048575"), Some(&2));
        // The second parallel response was persisted even though the first
        // request failed, and is reused after reopening the native journal.
        assert_eq!(ranges.get("bytes=1048576-2097151"), Some(&1));
        assert_eq!(ranges.get("bytes=2097152-2097168"), Some(&1));
    }
    let result = (
        bytes.load(Ordering::Relaxed),
        faults.requests.load(Ordering::Relaxed),
        upload_ms,
        download_ms,
        faults.uploads_peak.load(Ordering::Relaxed),
        faults.downloads_peak.load(Ordering::Relaxed),
    );
    task.abort();
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}
#[test]
fn full_chunk_upload_and_download_overlap_exactly_two_requests() {
    let result = run_case(0, Duration::from_millis(50), 0, false, 0);
    assert_eq!(result.4, 2, "upload concurrency");
    assert_eq!(result.5, 2, "download concurrency");
}
#[test]
fn proxy_413_429_524_resume_only_unverified_chunks_and_lost_accept_is_not_resent() {
    for code in [413, 429, 524] {
        run_case(0, Duration::ZERO, code, false, 0);
    }
    run_case(0, Duration::ZERO, 524, true, 0);
}
#[test]
fn failed_range_keeps_the_verified_parallel_response_across_reopen() {
    for code in [413, 429, 524] {
        run_case(0, Duration::from_millis(20), 0, false, code);
    }
}
#[test]
#[ignore = "Explicit synthetic bandwidth and request latency measurement"]
fn slow_network_full_transfer_gate() {
    for (bits, millis) in [(10_000_000, 80), (1_000_000, 200)] {
        let result = run_case(bits / 8, Duration::from_millis(millis), 0, false, 0);
        eprintln!("synthetic bits/sec={bits}, request delay={millis}ms: (HTTP bytes, requests, upload ms, download ms, upload concurrency, download concurrency)={result:?}");
        assert!(result.0 < 4 * 1024 * 1024 + 32 * 1024);
    }
}
