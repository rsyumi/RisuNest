#[cfg(test)]
mod admission_tests;
mod lww;
use crate::{
    store::{admit_format, Device, Store},
    workload::{WorkKind, Workload},
    Error, Result,
};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Version},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Extension, Json, Router,
};
use http_body::Body as _;
use risunest_sync_wire::{body, canonical, library_format, transfer, Sequence, MAX_METADATA_BYTES};
use serde::Deserialize;
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::Semaphore;

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
struct App {
    store: Arc<Store>,
    buffers: Arc<Semaphore>,
    materializers: Arc<Semaphore>,
    media_slots: Arc<Semaphore>,
    /// Bounds concurrent zstd work, with its input and output buffers.
    codecs: Arc<Semaphore>,
    workload: Workload,
    shutdown: tokio::sync::watch::Receiver<bool>,
    _lifetime: Arc<()>,
}

impl App {
    async fn authenticate(&self, library: &str, token: &str) -> Result<Device> {
        let store = self.store.clone();
        let library = library.to_owned();
        let token = token.to_owned();
        blocking(move || store.authenticate(&library, &token)).await
    }

    async fn wait_slot(&self, slots: Arc<Semaphore>) -> Result<tokio::sync::OwnedSemaphorePermit> {
        let mut shutdown = self.shutdown.clone();
        tokio::select! {
            biased;
            _ = async {
                if shutdown.wait_for(|stopping| *stopping).await.is_err() {
                    std::future::pending::<()>().await;
                }
            } => Err(Error::new("server-updating", 503)),
            permit = slots.acquire_owned() => permit.map_err(|_| Error::new("worker-unavailable", 503)),
        }
    }
}
#[derive(Clone)]
struct BufferedRequest {
    _permit: Arc<tokio::sync::OwnedSemaphorePermit>,
}
/// Set by routes whose successful replies are bounded and already in memory,
/// so `authorize` may encode them.
#[derive(Clone)]
struct Compressible;
async fn compressible(mut response: Response) -> Response {
    response.extensions_mut().insert(Compressible);
    response
}
pub fn router(store: Arc<Store>) -> Router {
    router_with_workload(store, Workload::new())
}

/// Accepts connections with Nagle's algorithm off, so the last segment of a
/// reply is not held until the client acknowledges the ones before it.
pub fn listener(
    listener: tokio::net::TcpListener,
) -> impl axum::serve::Listener<Io = tokio::net::TcpStream, Addr = std::net::SocketAddr> {
    use axum::serve::ListenerExt;
    listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    })
}

pub fn router_with_workload(store: Arc<Store>, workload: Workload) -> Router {
    router_with_shutdown(
        store,
        workload,
        tokio::sync::watch::Sender::new(false).subscribe(),
    )
}

async fn drain_trash_backlog(
    store: &std::sync::Weak<Store>,
    alive: &std::sync::Weak<()>,
    workload: &Workload,
) {
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if alive.strong_count() == 0 {
            break;
        }
        let Some(store) = store.upgrade() else { break };
        let Ok(mut work) = workload.begin(WorkKind::Background) else {
            break;
        };
        let pass = blocking(move || {
            let pass = store.drain_trash();
            work.set_performed_work(matches!(&pass, Ok(value) if value.removed > 0));
            pass
        })
        .await;
        match pass {
            Ok(pass) => {
                if pass.failed > 0 {
                    eprintln!("sync trash failed={} backlog={}", pass.failed, pass.backlog);
                }
                if pass.selected < 1024 || pass.removed == 0 || pass.backlog == 0 {
                    break;
                }
            }
            Err(error) => {
                eprintln!("sync trash failed: {}", error.code);
                break;
            }
        }
    }
}

pub fn router_with_shutdown(
    store: Arc<Store>,
    workload: Workload,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Router {
    let lifetime = Arc::new(());
    let maintenance_alive = Arc::downgrade(&lifetime);
    let maintenance_store = Arc::downgrade(&store);
    let maintenance_workload = workload.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(MAINTENANCE_INTERVAL).await;
            if maintenance_alive.strong_count() == 0 {
                break;
            }
            let Some(store) = maintenance_store.upgrade() else {
                break;
            };
            let Ok(mut work) = maintenance_workload.begin(WorkKind::Background) else {
                continue;
            };
            let result = blocking(move || {
                let result = store.maintain();
                work.set_performed_work(matches!(&result, Ok(value) if value.objects_removed > 0));
                result
            })
            .await;
            let continue_trash = match result {
                Err(error) => {
                    eprintln!("sync maintenance failed: {}", error.code);
                    false
                }
                Ok(result) => {
                    if result.wal_checkpoint.incomplete() {
                        eprintln!("sync maintenance wal checkpoint incomplete: busy={} log={} checkpointed={}", result.wal_checkpoint.busy, result.wal_checkpoint.log_frames, result.wal_checkpoint.checkpointed_frames);
                    }
                    if result.trash_failed > 0 {
                        eprintln!(
                            "sync trash failed={} backlog={}",
                            result.trash_failed, result.trash_backlog
                        );
                    }
                    result.trash_backlog > 0
                }
            };
            if !continue_trash {
                continue;
            }
            drain_trash_backlog(
                &maintenance_store,
                &maintenance_alive,
                &maintenance_workload,
            )
            .await;
        }
    });
    let uploads_alive = Arc::downgrade(&lifetime);
    let uploads = Arc::downgrade(&store);
    let upload_workload = workload.clone();
    tokio::spawn(async move {
        while uploads_alive.strong_count() > 0 {
            let Some(store) = uploads.upgrade() else {
                break;
            };
            let Ok(mut work) = upload_workload.begin(WorkKind::Background) else {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            };
            if !matches!(
                blocking(move || {
                    let result = store.run_pending_upload();
                    work.set_performed_work(matches!(&result, Ok(true)));
                    result
                })
                .await,
                Ok(true)
            ) {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    });
    let delta_alive = Arc::downgrade(&lifetime);
    let delta_store = Arc::downgrade(&store);
    let delta_workload = workload.clone();
    tokio::spawn(async move {
        while delta_alive.strong_count() > 0 {
            let Some(store) = delta_store.upgrade() else {
                break;
            };
            let Ok(mut work) = delta_workload.begin(WorkKind::Background) else {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            };
            if !matches!(
                blocking(move || {
                    let result = store.run_pending_download_delta();
                    work.set_performed_work(!matches!(&result, Ok(false)));
                    result
                })
                .await,
                Ok(true)
            ) {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    });
    let app = App {
        store,
        buffers: Arc::new(Semaphore::new(4)),
        materializers: Arc::new(Semaphore::new(1)),
        media_slots: Arc::new(Semaphore::new(16)),
        codecs: Arc::new(Semaphore::new(4)),
        workload,
        shutdown,
        _lifetime: lifetime,
    };
    // Object reads stream from storage; their part route encodes for itself.
    let objects = Router::new()
        .route("/objects/{hash}", get(object))
        .route("/objects/{hash}/part", get(object_part));
    Router::new()
        .route("/session", get(session))
        .route(
            "/session/claim-writer",
            post(lww::claim_writer).get(lww::writer_claim),
        )
        .route("/session/writer", post(lww::bind_writer))
        .route("/devices/{id}/status", get(device_status))
        .route("/head", get(head))
        .route("/time", get(lww::time))
        .route("/push", post(lww::push))
        .route("/changes", get(lww::changes))
        .route("/state/pins", post(lww::create_pin))
        .route("/state/pins/{id}", axum::routing::delete(lww::release_pin))
        .route("/state", get(lww::state))
        .route("/objects/pins", post(pin_objects))
        .route(
            "/objects/retention",
            post(retain_objects).get(retained_objects),
        )
        .route("/objects/retention/release", post(release_retained_objects))
        .route("/media/access", post(media_access))
        .route("/objects/missing", post(missing))
        .route("/uploads/frames", post(upload_frames))
        .route("/objects/transfer", post(transfer_objects))
        .route("/uploads", post(begin_upload))
        .route("/uploads/{id}", get(upload_progress).delete(cancel_upload))
        .route("/uploads/{id}/chunks/{index}", put(upload_chunk))
        .route("/uploads/{id}/complete", post(finish_upload))
        .route("/uploads/{id}/delta", put(upload_delta))
        .route("/objects/delta", post(begin_download_delta))
        .route(
            "/object-deltas/{id}",
            get(download_delta_progress).delete(release_download_delta),
        )
        .route("/operations/{id}", get(lww::operation))
        .route("/operations/{id}/cancel", post(lww::cancel_operation))
        .route("/ack", post(lww::ack))
        .route_layer(middleware::map_response(compressible))
        .merge(objects)
        .layer(DefaultBodyLimit::max(transfer::MAX_BATCH_BYTES))
        .route_layer(middleware::from_fn_with_state(app.clone(), authorize))
        // Notification streams authenticate without entering request maintenance work.
        .route("/notify", get(lww::notify))
        .route("/media/{token}", get(media))
        .layer(middleware::from_fn(close_after_unread_body))
        .with_state(app)
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let body = if let Some((floor, latest)) = self.journal_floor {
            serde_json::json!({"error":self.code,"journalFloor":floor,"latestSeq":latest})
        } else {
            match &self.key {
                Some(key) => serde_json::json!({"error":self.code,"key":key}),
                None => serde_json::json!({"error":self.code}),
            }
        };
        let mut response = (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(body),
        )
            .into_response();
        if self.status == 429 || self.code == "server-updating" {
            response
                .headers_mut()
                .insert("retry-after", "1".parse().unwrap());
        }
        response
    }
}
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    #[cfg(all(test, feature = "source-observer"))]
    let worker = crate::source_observer::worker();
    #[cfg(all(test, feature = "source-observer"))]
    let f = move || {
        let _entered = worker.enter();
        let result = f();
        drop(_entered);
        drop(worker);
        result
    };
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| Error::new("worker-unavailable", 503))?
}
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    if headers.get_all(name).iter().count() != 1 {
        return None;
    }
    headers.get(name)?.to_str().ok()
}
/// The format level a request declares; released clients declare none.
fn declared_format(headers: &HeaderMap) -> Result<Option<u64>> {
    if !headers.contains_key(library_format::HEADER) {
        return Ok(None);
    }
    header(headers, library_format::HEADER)
        .and_then(|value| library_format::parse_header(value).ok())
        .map(Some)
        .ok_or(Error::new("invalid-library-format", 400))
}
/// Every authenticated reply names the library level and the highest level
/// this server accepts, so a client can tell a server with the gate apart.
fn insert_format_headers(store: &Store, headers: &mut HeaderMap) {
    headers.insert(library_format::HEADER, store.library_format().into());
    headers.insert(
        library_format::MAX_HEADER,
        crate::LIBRARY_FORMAT_MAX_LEVEL.into(),
    );
}
fn values<'a>(headers: &'a HeaderMap, name: &str) -> impl Iterator<Item = &'a [u8]> {
    headers.get_all(name).iter().map(|value| value.as_bytes())
}
/// Runs zstd work on the blocking pool under a codec permit. The job owns the
/// permit, so a request whose deadline expires still counts until it returns.
async fn codec<T: Send + 'static>(
    app: &App,
    job: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let permit = app.wait_slot(app.codecs.clone()).await?;
    blocking(move || {
        let _permit = permit;
        job()
    })
    .await
}
fn mark_encoded(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(body::ENCODING_HEADER, body::ZSTD.parse().unwrap());
    headers.insert("content-type", "application/octet-stream".parse().unwrap());
    headers.remove("content-length");
}
/// The decoded size a request body may reach on its path.
fn raw_body_limit(path: &str) -> usize {
    if path.contains("/chunks/") {
        transfer::UPLOAD_CHUNK_BYTES
    } else if path == "/uploads/frames"
        || (path.starts_with("/uploads/") && path.ends_with("/delta"))
    {
        transfer::MAX_BATCH_BYTES
    } else {
        MAX_METADATA_BYTES
    }
}
async fn read_body(stream: axum::body::Body, limit: usize) -> Result<Vec<u8>> {
    use futures_util::StreamExt;
    let mut stream = stream.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Error::new("incomplete-body", 400))?;
        if bytes.len() + chunk.len() > limit {
            return Err(Error::new("body-too-large", 413));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
/// Hands the route a raw body. An encoded one is read under the raw limit of
/// its path and decoded under a codec permit.
async fn decode_request(app: &App, request: Request) -> Result<Request> {
    if !body::marked(values(request.headers(), body::ENCODING_HEADER))
        .map_err(|_| Error::new("invalid-body-encoding", 400))?
    {
        return Ok(request);
    }
    let limit = raw_body_limit(request.uri().path());
    let device = request
        .extensions()
        .get::<Device>()
        .cloned()
        .ok_or(Error::new("unauthorized", 401))?;
    let (mut parts, stream) = request.into_parts();
    let encoded = read_body(stream, limit).await?;
    let store = app.store.clone();
    let raw = codec(app, move || {
        // Registration can be revoked while a request waits for a codec permit.
        store.require_registered(&device)?;
        body::decode(&encoded, limit).map_err(|error| match error.0 {
            "body-too-large" => Error::new("body-too-large", 413),
            _ => Error::new("invalid-body-encoding", 400),
        })
    })
    .await?;
    parts.headers.remove(body::ENCODING_HEADER);
    parts.headers.insert("content-length", raw.len().into());
    Ok(Request::from_parts(parts, axum::body::Body::from(raw)))
}
/// Encodes a successful reply its route marked compressible, when the client
/// accepts encoded replies and encoding saves enough.
async fn encode_response(app: &App, accepts: bool, response: Response) -> Result<Response> {
    if !accepts
        || response.status() != StatusCode::OK
        || response.extensions().get::<Compressible>().is_none()
    {
        return Ok(response);
    }
    let (parts, reply) = response.into_parts();
    let raw = axum::body::to_bytes(reply, transfer::MAX_BATCH_BYTES)
        .await
        .map_err(|_| Error::new("response-too-large", 500))?;
    let (bytes, encoded) = if raw.len() < body::MIN_ENCODED_BYTES {
        (raw, false)
    } else {
        // The raw reply is dropped inside the job once it is encoded.
        codec(app, move || {
            Ok(
                match body::encode(&raw).map_err(|_| Error::new("body-encoding-failed", 500))? {
                    Some(encoded) => (Bytes::from(encoded), true),
                    None => (raw, false),
                },
            )
        })
        .await?
    };
    let mut response = Response::from_parts(parts, axum::body::Body::from(bytes));
    if encoded {
        mark_encoded(&mut response);
    }
    Ok(response)
}
fn retain_guard_until_body_eof<T: Send + 'static>(
    body: axum::body::Body,
    guard: T,
) -> axum::body::Body {
    use futures_util::Stream;
    let mut body = Box::pin(body.into_data_stream());
    let mut guard = Some(guard);
    let stream = futures_util::stream::poll_fn(move |context| {
        let next = body.as_mut().poll_next(context);
        if matches!(next, std::task::Poll::Ready(None)) {
            drop(guard.take());
        }
        next
    });
    axum::body::Body::from_stream(stream)
}
/// Records when a request body has been read to its end.
struct BodyEndTracker {
    body: axum::body::Body,
    read: Arc<AtomicBool>,
}
impl http_body::Body for BodyEndTracker {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<http_body::Frame<Bytes>, axum::Error>>> {
        let frame = Pin::new(&mut self.body).poll_frame(context);
        if matches!(frame, Poll::Ready(None)) {
            self.read.store(true, Ordering::Release);
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}
/// Closes the connection after a reply sent before its request body was read
/// to the end. Hyper cannot drain the rest and would otherwise close a
/// connection whose reply offered it for reuse.
pub(crate) async fn close_after_unread_body(request: Request, next: Next) -> Response {
    let read = Arc::new(AtomicBool::new(request.body().is_end_stream()));
    let http1 = request.version() < Version::HTTP_2;
    let tracked = read.clone();
    let request = request.map(|body| {
        axum::body::Body::new(BodyEndTracker {
            body,
            read: tracked,
        })
    });
    let mut response = next.run(request).await;
    if http1 && !read.load(Ordering::Acquire) {
        response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    response
}
async fn authorize(State(app): State<App>, request: Request, next: Next) -> Response {
    let mut authenticated = false;
    let result = async {
        let token = header(request.headers(), "authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(Error::new("unauthorized", 401))?
            .to_owned();
        let library = header(request.headers(), "x-risu-library")
            .ok_or(Error::new("unauthorized", 401))?
            .to_owned();
        app.authenticate(&library, &token).await?;
        authenticated = true;
        // An early refusal; store operations check the level again themselves.
        let declared = declared_format(request.headers())?;
        admit_format(app.store.library_format(), declared)?;
        // DefaultBodyLimit caps consumed bytes but does not preflight a declared
        // oversized body. Reject it after authentication, before waiting for bytes.
        if request.headers().contains_key("content-length") {
            let length = header(request.headers(), "content-length")
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or(Error::new("invalid-content-length", 400))?;
            let limit = if request.uri().path() == "/uploads/frames"
                || request.uri().path().contains("/chunks/")
                || (request.uri().path().starts_with("/uploads/")
                    && request.uri().path().ends_with("/delta"))
            {
                transfer::MAX_BATCH_BYTES
            } else {
                MAX_METADATA_BYTES
            };
            if length > limit as u64 {
                return Err(Error::new("body-too-large", 413));
            }
        }
        if header(request.headers(), "content-encoding").is_some_and(|v| v != "identity") {
            return Err(Error::new("unsupported-content-encoding", 415));
        }
        if app.workload.status()?.state != "open" {
            return Err(Error::new("server-updating", 503));
        }
        let accepts = body::accepted(values(request.headers(), body::ACCEPT_HEADER));
        // Reserve bounded body/response memory before consuming any bulk body.
        // A worker clone retains the reservation even if its HTTP future times out.
        let path = request.uri().path();
        let buffered = if matches!(path, "/uploads/frames" | "/objects/transfer")
            || path.contains("/chunks/")
            || (path.starts_with("/uploads/") && path.ends_with("/delta"))
        {
            Some(BufferedRequest {
                _permit: Arc::new(app.wait_slot(app.buffers.clone()).await?),
            })
        } else {
            None
        };
        // Queued requests do not prevent maintenance from draining active work.
        let work = app.workload.begin(WorkKind::Request)?;
        // Registration can be revoked while a request waits for capacity.
        let mut device = app.authenticate(&library, &token).await?;
        device.format_level = declared;
        let mut request = request;
        request.extensions_mut().insert(device);
        if let Some(buffered) = &buffered {
            request.extensions_mut().insert(buffered.clone());
        }
        let deadline = if request.uri().path() == "/uploads/frames"
            || (request.uri().path().starts_with("/uploads/")
                && request.uri().path().ends_with("/delta"))
        {
            110
        } else {
            60
        };
        // The task owns every admission permit. If the HTTP deadline expires,
        // blocking work continues to hold admission until it actually returns.
        let codecs = app.clone();
        let task = tokio::spawn(async move {
            let response = match decode_request(&codecs, request).await {
                Ok(request) => encode_response(&codecs, accepts, next.run(request).await)
                    .await
                    .unwrap_or_else(IntoResponse::into_response),
                Err(error) => error.into_response(),
            };
            (response, (buffered, work))
        });
        let (mut response, permits) = tokio::time::timeout(Duration::from_secs(deadline), task)
            .await
            .map_err(|_| Error::new("request-timeout", 408))?
            .map_err(|_| Error::new("worker-unavailable", 503))?;
        let cache = if response.headers().contains_key(body::ENCODING_HEADER) {
            "no-store, no-transform"
        } else {
            "no-store"
        };
        response
            .headers_mut()
            .insert("cache-control", cache.parse().unwrap());
        // A slow response must retain its slot until the stream is consumed or
        // disconnected, not just until response headers are ready.
        let (parts, body) = response.into_parts();
        let response = Response::from_parts(parts, retain_guard_until_body_eof(body, permits));
        Ok::<_, Error>(response)
    }
    .await;
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    if authenticated {
        insert_format_headers(&app.store, response.headers_mut());
    }
    response
}
async fn head(State(app): State<App>, headers: HeaderMap) -> Result<Response> {
    let head = blocking(move || app.store.head()).await?;
    let etag = head.etag();
    let mut response = if header(&headers, "if-none-match") == Some(&etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Json(head).into_response()
    };
    response.headers_mut().insert("etag", etag.parse().unwrap());
    Ok(response)
}
async fn session(State(app): State<App>, Extension(device): Extension<Device>) -> Result<Response> {
    blocking(move || Ok(Json(app.store.device_session(&device)?).into_response())).await
}
async fn device_status(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
) -> Result<Response> {
    blocking(move || {
        Ok(
            Json(serde_json::json!({"deviceId":id,"active":app.store.device_active(&device,&id)?}))
                .into_response(),
        )
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectRequest {
    hash: String,
    size: Sequence,
}
async fn missing(State(app): State<App>, body: Bytes) -> Result<Response> {
    let candidates: Vec<ObjectRequest> = canonical::decode(&body, MAX_METADATA_BYTES)?;
    if candidates.len() > 1024 {
        return Err(Error::new("too-many-candidates", 400));
    }
    blocking(move || {
        let paths = app.store.object_paths();
        let mut absent = Vec::new();
        for candidate in candidates {
            match app.store.object_size(&candidate.hash)? {
                None => absent.push(candidate.hash),
                Some(size) if candidate.size != size.into() => {
                    return Err(Error::new("object-size-mismatch", 409))
                }
                Some(_) if !app.store.object_presence_in(&paths, &candidate.hash)? => {
                    absent.push(candidate.hash)
                }
                _ => (),
            }
        }
        Ok(Json(serde_json::json!({"missing":absent})).into_response())
    })
    .await
}
async fn object(
    State(app): State<App>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let tag_digest = digest.clone();
    let (body, total) = blocking(move || app.store.open_object(&digest)).await?;
    object_response(
        body,
        total,
        &tag_digest,
        "application/octet-stream",
        headers,
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PartQuery {
    offset: u64,
    length: u64,
}
/// One part of an object in raw offsets. Unlike an HTTP range, the reply may
/// be encoded, so its position travels in `x-risu-object-range`.
async fn object_part(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(digest): Path<String>,
    Query(PartQuery { offset, length }): Query<PartQuery>,
    headers: HeaderMap,
) -> Result<Response> {
    if length == 0 || length > transfer::UPLOAD_CHUNK_BYTES as u64 {
        return Err(Error::new("invalid-object-range", 400));
    }
    let accepts = body::accepted(values(&headers, body::ACCEPT_HEADER));
    let store = app.store.clone();
    // The raw part is in memory only under a codec permit.
    let (bytes, total, encoded) = codec(&app, move || {
        use std::io::{Read, Seek};
        store.require_registered(&device)?;
        let (mut object, total) = store.open_object(&digest)?;
        if offset.checked_add(length).is_none_or(|end| end > total) {
            return Err(Error::new("invalid-object-range", 400));
        }
        object.seek(std::io::SeekFrom::Start(offset))?;
        let mut raw = Vec::with_capacity(length as usize);
        object.take(length).read_to_end(&mut raw)?;
        if raw.len() as u64 != length {
            return Err(Error::new("corrupt-object", 503));
        }
        let encoded = if accepts {
            body::encode(&raw).map_err(|_| Error::new("body-encoding-failed", 500))?
        } else {
            None
        };
        Ok(match encoded {
            Some(encoded) => (encoded, total, true),
            None => (raw, total, false),
        })
    })
    .await?;
    let mut response = (
        [("content-type", "application/octet-stream")],
        Bytes::from(bytes),
    )
        .into_response();
    response.headers_mut().insert(
        "x-risu-object-range",
        format!("{offset}-{}/{total}", offset + length - 1)
            .parse()
            .unwrap(),
    );
    if encoded {
        mark_encoded(&mut response);
    }
    Ok(response)
}

async fn object_response(
    body: crate::store::Body,
    total: u64,
    digest: &str,
    mime: &str,
    headers: HeaderMap,
) -> Result<Response> {
    let tag = format!("\"{digest}\"");
    let mut response;
    if header(&headers, "if-none-match") == Some(&tag) {
        response = StatusCode::NOT_MODIFIED.into_response();
    } else {
        let range = header(&headers, "range")
            .filter(|_| header(&headers, "if-range").is_none_or(|v| v == tag));
        let selection = range.map(|r| parse_range(r, total as usize));
        if selection == Some(None) {
            response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            response
                .headers_mut()
                .insert("content-range", format!("bytes */{total}").parse().unwrap());
        } else {
            let (start, length) = match selection.flatten() {
                Some((start, end)) => (start as u64, (end - start + 1) as u64),
                None => (0, total),
            };
            response = match body {
                crate::store::Body::File(file) => {
                    #[cfg(not(all(test, feature = "source-observer")))]
                    let mut file = tokio::fs::File::from_std(file);
                    #[cfg(all(test, feature = "source-observer"))]
                    let mut file = {
                        let file = file.into_async();
                        file.selected(start, length);
                        file
                    };
                    use tokio::io::{AsyncReadExt, AsyncSeekExt};
                    file.seek(std::io::SeekFrom::Start(start)).await?;
                    axum::body::Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
                        file.take(length),
                        64 * 1024,
                    ))
                    .into_response()
                }
                // Already in memory and bounded by the inline threshold, so a
                // range is a slice rather than a seek.
                crate::store::Body::Bytes(bytes) => {
                    #[cfg(all(test, feature = "source-observer"))]
                    crate::source_observer::inline_selection(digest, start, length);
                    let bytes = bytes.into_inner();
                    let end = (start + length).min(bytes.len() as u64) as usize;
                    axum::body::Body::from(bytes[start as usize..end].to_vec()).into_response()
                }
            };
            response
                .headers_mut()
                .insert("content-length", length.to_string().parse().unwrap());
            if selection.is_some() {
                *response.status_mut() = StatusCode::PARTIAL_CONTENT;
                response.headers_mut().insert(
                    "content-range",
                    format!("bytes {start}-{}/{total}", start + length - 1)
                        .parse()
                        .unwrap(),
                );
            }
        }
    }
    response.headers_mut().insert("etag", tag.parse().unwrap());
    response
        .headers_mut()
        .insert("accept-ranges", "bytes".parse().unwrap());
    response.headers_mut().insert(
        "content-type",
        mime.parse()
            .map_err(|_| Error::new("invalid-media-mime", 400))?,
    );
    Ok(response)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MediaAccessRequest {
    epoch: String,
    requests: Vec<risunest_sync_connect::media::MediaRequest>,
}
async fn media_access(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Result<Response> {
    let request: MediaAccessRequest = canonical::decode(&body, MAX_METADATA_BYTES)?;
    blocking(move || {
        Ok(Json(
            app.store
                .media_access(&device, &request.epoch, &request.requests)?,
        )
        .into_response())
    })
    .await
}

async fn media(
    State(app): State<App>,
    Path(token): Path<String>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Result<Response> {
    let response =
        resolve_media_response(app.store.clone(), token.clone(), headers.clone()).await?;
    let mut response = if method != axum::http::Method::HEAD && media_has_body(&response) {
        if *app.shutdown.borrow() {
            return Err(Error::new("server-updating", 503));
        }
        let (response, permit) = match app.media_slots.clone().try_acquire_owned() {
            Ok(permit) => (response, permit),
            Err(_) => {
                // Do not retain an open file or inline body while waiting. Recheck
                // the capability after admission for expiry and revocation.
                drop(response);
                let permit = app.wait_slot(app.media_slots.clone()).await?;
                (
                    resolve_media_response(app.store.clone(), token, headers).await?,
                    permit,
                )
            }
        };
        if media_has_body(&response) {
            let (parts, body) = response.into_parts();
            Response::from_parts(parts, retain_guard_until_body_eof(body, permit))
        } else {
            response
        }
    } else {
        response
    };
    response
        .headers_mut()
        .insert("access-control-allow-origin", "*".parse().unwrap());
    response.headers_mut().insert(
        "access-control-expose-headers",
        "Accept-Ranges, Content-Length, Content-Range, Content-Type, ETag"
            .parse()
            .unwrap(),
    );
    response
        .headers_mut()
        .insert("x-content-type-options", "nosniff".parse().unwrap());
    response
        .headers_mut()
        .insert("referrer-policy", "no-referrer".parse().unwrap());
    Ok(response)
}

fn media_has_body(response: &Response) -> bool {
    header(response.headers(), "content-length")
        .and_then(|length| length.parse::<u64>().ok())
        .is_some_and(|length| length > 0)
}

async fn resolve_media_response(
    store: Arc<Store>,
    token: String,
    headers: HeaderMap,
) -> Result<Response> {
    use crate::store::MediaResponse;
    let resolved = blocking(move || store.resolve_media(&token)).await?;
    Ok(match resolved {
        MediaResponse::Refresh(url) => {
            let mut response = StatusCode::TEMPORARY_REDIRECT.into_response();
            response.headers_mut().insert(
                "location",
                url.parse()
                    .map_err(|_| Error::new("invalid-media-refresh", 503))?,
            );
            response
                .headers_mut()
                .insert("cache-control", "no-store".parse().unwrap());
            response
        }
        MediaResponse::File {
            file,
            size,
            hash,
            mime,
            max_age,
        } => {
            let mut response = object_response(file, size, &hash, &mime, headers).await?;
            response.headers_mut().insert(
                "cache-control",
                format!("private, max-age={max_age}").parse().unwrap(),
            );
            response
        }
    })
}

fn parse_range(range: &str, total: usize) -> Option<(usize, usize)> {
    let (start, end) = range.strip_prefix("bytes=")?.split_once('-')?;
    if total == 0 {
        return None;
    }
    if start.is_empty() {
        let length = end.parse::<usize>().ok()?;
        return (length > 0).then_some((total.saturating_sub(length), total - 1));
    }
    let start = start.parse::<usize>().ok()?;
    let end = if end.is_empty() {
        total - 1
    } else {
        end.parse::<usize>().ok()?.min(total - 1)
    };
    (start <= end && start < total).then_some((start, end))
}
async fn begin_upload(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Result<Response> {
    let manifest: crate::store::UploadManifest = canonical::decode(&body, MAX_METADATA_BYTES)?;
    blocking(move || {
        Ok((
            StatusCode::CREATED,
            Json(serde_json::json!({"uploadId":app.store.begin_upload(&device,&manifest)?})),
        )
            .into_response())
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadQuery {
    after: Option<u64>,
    wait: Option<bool>,
}
async fn upload_progress(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
    Query(query): Query<UploadQuery>,
) -> Result<Response> {
    let until = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let store = app.store.clone();
        let device = device.clone();
        let id = id.clone();
        let progress = blocking(move || store.upload_progress(&device, &id, query.after)).await?;
        if query.wait != Some(true) || !progress.finishing || tokio::time::Instant::now() >= until {
            return Ok(Json(progress).into_response());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn upload_chunk(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Extension(buffer): Extension<BufferedRequest>,
    Path((id, index)): Path<(String, u64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode> {
    let hash = header(&headers, "x-content-sha256")
        .ok_or(Error::new("chunk-hash-required", 400))?
        .to_owned();
    blocking(move || {
        let _buffer = buffer;
        app.store
            .put_upload_chunk(&device, &id, index, &hash, &body)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn finish_upload(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
) -> Result<Response> {
    blocking(move || {
        Ok(match app.store.submit_upload(&device, &id)? {
            Some(hash) => Json(serde_json::json!({"hash":hash})).into_response(),
            None => (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({"uploadId":id,"status":"pending"})),
            )
                .into_response(),
        })
    })
    .await
}
async fn cancel_upload(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    blocking(move || app.store.cancel_upload(&device, &id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetainRequest {
    epoch: String,
    objects: Vec<crate::store::ObjectIdentity>,
}
async fn retain_objects(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Result<Response> {
    let request: RetainRequest = canonical::decode(&body, MAX_METADATA_BYTES)?;
    blocking(move || {
        Ok(Json(
            app.store
                .retain_objects(&device, &request.epoch, &request.objects)?,
        )
        .into_response())
    })
    .await
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetentionQuery {
    epoch: String,
    after: Option<String>,
}
async fn retained_objects(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Query(query): Query<RetentionQuery>,
) -> Result<Response> {
    blocking(move || {
        Ok(Json(
            app.store
                .retained_objects(&device, &query.epoch, query.after.as_deref())?,
        )
        .into_response())
    })
    .await
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReleaseRetentionRequest {
    epoch: String,
    objects: Vec<crate::store::RetentionRelease>,
}
async fn release_retained_objects(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Result<StatusCode> {
    let request: ReleaseRetentionRequest = canonical::decode(&body, MAX_METADATA_BYTES)?;
    blocking(move || {
        app.store
            .release_retained_objects(&device, &request.epoch, &request.objects)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn pin_objects(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Result<StatusCode> {
    let hashes: Vec<String> = canonical::decode(&body, MAX_METADATA_BYTES)?;
    blocking(move || app.store.pin_objects(&device, &hashes)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn upload_frames(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Extension(buffer): Extension<BufferedRequest>,
    body: Bytes,
) -> Result<Response> {
    // Keep CPU admission inside the worker lifetime, including after an HTTP timeout.
    let permit = app.wait_slot(app.materializers.clone()).await?;
    blocking(move || {
        let _permit = permit;
        let _buffer = buffer;
        app.store.receive_frames(&device, &body)?;
        // Successful completion verifies every submitted target. Repeating the
        // complete hash list adds no information; failures still return errors.
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await
}
async fn transfer_objects(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Extension(buffer): Extension<BufferedRequest>,
    body: Bytes,
) -> Result<Response> {
    let requests: Vec<crate::store::TransferRequest> =
        canonical::decode(&body, MAX_METADATA_BYTES)?;
    let permit = app.wait_slot(app.materializers.clone()).await?;
    blocking(move || {
        let _permit = permit;
        let _buffer = buffer;
        Ok((
            [("content-type", "application/octet-stream")],
            app.store.transfer_objects(&device, &requests)?,
        )
            .into_response())
    })
    .await
}

async fn upload_delta(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Extension(buffer): Extension<BufferedRequest>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode> {
    blocking(move || {
        let _buffer = buffer;
        app.store.attach_upload_delta(&device, &id, &body)
    })
    .await?;
    Ok(StatusCode::ACCEPTED)
}
async fn begin_download_delta(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Result<Response> {
    let request: crate::store::TransferRequest = canonical::decode(&body, MAX_METADATA_BYTES)?;
    blocking(move || {
        Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"jobId":app.store.begin_download_delta(&device,&request)?})),
        )
            .into_response())
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitQuery {
    wait: Option<bool>,
}
async fn download_delta_progress(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
    Query(query): Query<WaitQuery>,
) -> Result<Response> {
    use crate::store::DeltaProgress;
    let until = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let store = app.store.clone();
        let device = device.clone();
        let id = id.clone();
        let progress = blocking(move || store.download_delta_progress(&device, &id)).await?;
        match progress {
            DeltaProgress::Ready(bytes) => {
                return Ok(([("content-type", "application/octet-stream")], bytes).into_response())
            }
            DeltaProgress::FullRequired => return Ok(StatusCode::NO_CONTENT.into_response()),
            DeltaProgress::Pending
                if query.wait != Some(true) || tokio::time::Instant::now() >= until =>
            {
                return Ok(StatusCode::ACCEPTED.into_response())
            }
            DeltaProgress::Pending => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}
async fn release_download_delta(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    blocking(move || app.store.release_download_delta(&device, &id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::retain_guard_until_body_eof;
    use axum::body::Body;
    use futures_util::StreamExt;
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    #[tokio::test]
    async fn accepted_connections_send_without_waiting_for_acknowledgements() {
        use axum::serve::Listener;
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = tcp.local_addr().unwrap();
        let mut listener = super::listener(tcp);
        let _client = tokio::net::TcpStream::connect(address).await.unwrap();
        let (accepted, _) = listener.accept().await;
        assert!(accepted.nodelay().unwrap());
    }

    #[tokio::test]
    async fn exhausted_body_releases_its_guard_before_the_stream_is_dropped() {
        let semaphore = Arc::new(Semaphore::new(1));
        let guard = semaphore.clone().try_acquire_owned().unwrap();
        let body = retain_guard_until_body_eof(Body::from("synthetic body"), guard);
        let mut stream = Box::pin(body.into_data_stream());

        assert!(semaphore.clone().try_acquire_owned().is_err());
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.as_ref(), b"synthetic body");
        assert!(semaphore.clone().try_acquire_owned().is_err());

        assert!(stream.next().await.is_none());
        let available = semaphore.clone().try_acquire_owned().unwrap();
        assert!(stream.next().await.is_none());
        drop(available);
    }

    #[tokio::test]
    async fn trash_continuation_yields_and_rechecks_maintenance_admission() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::Store::init(dir.path()).unwrap());
        let alive = Arc::new(());
        let workload = crate::workload::Workload::with_clock(
            std::time::Duration::ZERO,
            std::time::Duration::from_secs(45),
            Arc::new(std::time::Instant::now),
        );
        let db = rusqlite::Connection::open(dir.path().join("metadata.sqlite")).unwrap();
        db.execute_batch("BEGIN").unwrap();
        for index in 0..2049 {
            db.execute(
                "INSERT INTO object_trash(hash) VALUES(?1)",
                [format!("{index:064x}")],
            )
            .unwrap();
        }
        db.execute_batch("COMMIT").unwrap();
        let weak_store = Arc::downgrade(&store);
        let weak_alive = Arc::downgrade(&alive);
        let pending = super::drain_trash_backlog(&weak_store, &weak_alive, &workload);
        tokio::pin!(pending);
        tokio::select! {
            _ = &mut pending => panic!("cleanup must yield before its first batch"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => (),
        }
        assert_eq!(workload.status().unwrap().active_background_jobs, 0);
        // Observe the first bounded batch and acquire maintenance during its yield.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let remaining: i64 = db
                    .query_row("SELECT count(*) FROM object_trash", [], |row| row.get(0))
                    .unwrap();
                if remaining == 1025 && workload.status().unwrap().drained {
                    break;
                }
                tokio::select! {
                    _ = &mut pending => panic!("cleanup finished before the maintenance window"),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => (),
                }
            }
        })
        .await
        .unwrap();
        let (token, _) = workload.acquire().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut pending)
            .await
            .unwrap();
        let remaining: i64 = db
            .query_row("SELECT count(*) FROM object_trash", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1025);
        assert!(workload.status().unwrap().drained);
        workload.release(&token).unwrap();
    }

    #[tokio::test]
    async fn trash_continuation_stops_when_a_batch_makes_no_progress() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::Store::init(dir.path()).unwrap());
        let alive = Arc::new(());
        let workload = crate::workload::Workload::new();
        let db = rusqlite::Connection::open(dir.path().join("metadata.sqlite")).unwrap();
        db.execute_batch("BEGIN").unwrap();
        for index in 0..1024 {
            let hash = format!("{index:064x}");
            std::fs::create_dir_all(dir.path().join("objects").join(&hash[..2]).join(&hash))
                .unwrap();
            db.execute("INSERT INTO object_trash(hash) VALUES(?1)", [&hash])
                .unwrap();
        }
        let sentinel_hash = format!("{:064x}", 1024);
        let sentinel = dir
            .path()
            .join("objects")
            .join(&sentinel_hash[..2])
            .join(&sentinel_hash);
        std::fs::write(&sentinel, b"synthetic trash").unwrap();
        db.execute(
            "INSERT INTO object_trash(hash) VALUES(?1)",
            [&sentinel_hash],
        )
        .unwrap();
        db.execute_batch("COMMIT").unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            super::drain_trash_backlog(&Arc::downgrade(&store), &Arc::downgrade(&alive), &workload),
        )
        .await
        .expect("an undeletable full batch must not spin");
        let remaining: i64 = db
            .query_row("SELECT count(*) FROM object_trash", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1025);
        assert!(sentinel.is_file());
        assert!(workload.status().unwrap().drained);
    }
}
