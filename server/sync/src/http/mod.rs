use crate::{
    store::{CommitSubmission, Device, Store},
    workload::{WorkKind, Workload},
    Error, Result,
};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post, put},
    Extension, Json, Router,
};
use risunest_sync_wire::{
    canonical, transfer, ChangeSet, CommitIntent, Domain, Receipt, Sequence, TerminalStatus,
    MAX_METADATA_BYTES,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;

/// How long a held notification stream waits before confirming the head on its
/// own. It bounds the delay of a change no writer announced.
const HEAD_NOTICE_INTERVAL: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct App {
    store: Arc<Store>,
    slots: Arc<Semaphore>,
    wait_slots: Arc<Semaphore>,
    buffers: Arc<Semaphore>,
    materializers: Arc<Semaphore>,
    media_slots: Arc<Semaphore>,
    notice_slots: Arc<Semaphore>,
    devices: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    workload: Workload,
    _lifetime: Arc<()>,
}
#[derive(Clone)]
struct BufferedRequest {
    _permit: Arc<tokio::sync::OwnedSemaphorePermit>,
}
pub fn router(store: Arc<Store>) -> Router {
    router_with_workload(store, Workload::new())
}

pub fn router_with_workload(store: Arc<Store>, workload: Workload) -> Router {
    let lifetime = Arc::new(());
    let maintenance_alive = Arc::downgrade(&lifetime);
    let maintenance_store = Arc::downgrade(&store);
    let maintenance_workload = workload.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if maintenance_alive.strong_count() == 0 {
                break;
            }
            let Some(store) = maintenance_store.upgrade() else {
                break;
            };
            let Ok(mut work) = maintenance_workload.begin(WorkKind::Background) else {
                continue;
            };
            let _ = blocking(move || {
                let result = store.maintain();
                work.set_performed_work(true);
                result
            })
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
    let alive = Arc::downgrade(&lifetime);
    let jobs = Arc::downgrade(&store);
    let commit_workload = workload.clone();
    tokio::spawn(async move {
        while alive.strong_count() > 0 {
            let Some(store) = jobs.upgrade() else { break };
            let Ok(mut work) = commit_workload.begin(WorkKind::Background) else {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            };
            let result = blocking(move || {
                let result = store.run_pending_commit();
                work.set_performed_work(!matches!(&result, Ok(false)));
                result
            })
            .await;
            if !matches!(result, Ok(true)) {
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
        slots: Arc::new(Semaphore::new(8)),
        wait_slots: Arc::new(Semaphore::new(8)),
        buffers: Arc::new(Semaphore::new(4)),
        materializers: Arc::new(Semaphore::new(1)),
        media_slots: Arc::new(Semaphore::new(16)),
        notice_slots: Arc::new(Semaphore::new(32)),
        devices: Arc::new(Mutex::new(HashMap::new())),
        workload,
        _lifetime: lifetime,
    };
    Router::new()
        .route("/session", get(session))
        .route("/devices/{id}/status", get(device_status))
        .route("/head", get(head))
        .route("/changes", get(changes))
        .route("/read-pins", post(pin_changes))
        .route("/read-pins/{id}", get(pinned_changes).delete(release_pin))
        .route("/checkpoints", post(create_checkpoint))
        .route(
            "/checkpoints/{id}",
            get(checkpoint_page).delete(release_checkpoint),
        )
        .route("/objects/pins", post(pin_objects))
        .route(
            "/objects/retention",
            post(retain_objects).get(retained_objects),
        )
        .route("/objects/retention/release", post(release_retained_objects))
        .route("/media/access", post(media_access))
        .route("/scopes", get(scope))
        .route("/objects/missing", post(missing))
        .route("/objects/{hash}", get(object))
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
        .route("/staged-changes", post(stage))
        .route("/staged-changes/start", post(begin_stage))
        .route(
            "/staged-changes/{id}",
            get(stage_progress).delete(cancel_stage),
        )
        .route("/staged-changes/{id}/pages/{index}", put(stage_page))
        .route("/staged-changes/{id}/seal", post(seal_stage))
        .route("/commits", post(commit))
        .route("/operations/{id}", get(receipt))
        .route("/acks", post(ack))
        .layer(DefaultBodyLimit::max(transfer::MAX_BATCH_BYTES))
        .route_layer(middleware::from_fn_with_state(app.clone(), authorize))
        // A held notification stream must not occupy an admission slot or a
        // device slot for its whole life, so it authenticates on its own.
        .route("/events", get(events))
        .route("/media/{token}", get(media))
        .with_state(app)
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let mut response = (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({"error":self.code})),
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
async fn authorize(State(app): State<App>, request: Request, next: Next) -> Response {
    let result = async {
        let token = header(request.headers(), "authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(Error::new("unauthorized", 401))?
            .to_owned();
        let library = header(request.headers(), "x-risu-library")
            .ok_or(Error::new("unauthorized", 401))?
            .to_owned();
        let store = app.store.clone();
        let device = blocking(move || store.authenticate(&library, &token)).await?;
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
        let work = app.workload.begin(WorkKind::Request)?;
        let semaphore = {
            let mut devices = app
                .devices
                .lock()
                .map_err(|_| Error::new("worker-unavailable", 503))?;
            devices
                .entry(device.id.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(2)))
                .clone()
        };
        let _device_permit = semaphore
            .try_acquire_owned()
            .map_err(|_| Error::new("device-busy", 429))?;
        // Waiting for a durable worker does not consume a transfer/head slot.
        let is_wait = request.method() == axum::http::Method::GET
            && (request.uri().path().starts_with("/object-deltas/")
                || request.uri().path().starts_with("/uploads/"));
        let slots = if is_wait { app.wait_slots } else { app.slots };
        let _global_permit = slots
            .try_acquire_owned()
            .map_err(|_| Error::new("server-busy", 429))?;
        // Reserve bounded body/response memory before consuming any bulk body.
        // A worker clone retains the reservation even if its HTTP future times out.
        let path = request.uri().path();
        let buffered = if matches!(
            path,
            "/uploads/frames" | "/objects/transfer"
        ) || path.contains("/chunks/")
            || (path.starts_with("/uploads/") && path.ends_with("/delta"))
        {
            Some(BufferedRequest {
                _permit: Arc::new(
                    app.buffers
                        .clone()
                        .try_acquire_owned()
                        .map_err(|_| Error::new("transfer-memory-busy", 429))?,
                ),
            })
        } else {
            None
        };
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
        let task = tokio::spawn(async move {
            let response = next.run(request).await;
            (response, (_device_permit, _global_permit, buffered, work))
        });
        let (mut response, permits) = tokio::time::timeout(Duration::from_secs(deadline), task)
            .await
            .map_err(|_| Error::new("request-timeout", 408))?
            .map_err(|_| Error::new("worker-unavailable", 503))?;
        response
            .headers_mut()
            .insert("cache-control", "no-store".parse().unwrap());
        // A slow response must retain its slot until the stream is consumed or
        // disconnected, not just until response headers are ready.
        let (parts, body) = response.into_parts();
        let response = Response::from_parts(parts, retain_guard_until_body_eof(body, permits));
        Ok::<_, Error>(response)
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
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
/// Notifies a held client that the ledger head may have moved. The change
/// itself still travels over the ordinary endpoints, so a client that never
/// connects here, or whose connection drops, reaches the same state by asking.
async fn events(State(app): State<App>, headers: HeaderMap) -> Result<Response> {
    let token = header(&headers, "authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(Error::new("unauthorized", 401))?
        .to_owned();
    let library = header(&headers, "x-risu-library")
        .ok_or(Error::new("unauthorized", 401))?
        .to_owned();
    let store = app.store.clone();
    blocking(move || store.authenticate(&library, &token)).await?;
    let permit = app
        .notice_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::new("server-busy", 429))?;
    let announced = app.store.head_announcements();
    let stream = futures_util::stream::unfold(
        (app, announced, None::<String>, permit),
        |(app, mut announced, last, permit)| async move {
            loop {
                // A stream outlives the drain a maintenance owner waits for, so
                // it closes as soon as admission does.
                if !app
                    .workload
                    .status()
                    .is_ok_and(|status| status.state == "open")
                {
                    return None;
                }
                let store = app.store.clone();
                let head = blocking(move || store.head()).await.ok()?;
                if last.as_deref() != Some(head.head_id.as_str()) {
                    let event = Event::default().event("head").data(&head.head_id);
                    return Some((
                        Ok::<_, std::convert::Infallible>(event),
                        (app, announced, Some(head.head_id), permit),
                    ));
                }
                let _ =
                    tokio::time::timeout(HEAD_NOTICE_INTERVAL, announced.changed()).await;
            }
        },
    );
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChangesQuery {
    epoch: String,
    after_seq: Sequence,
    after_ordinal: Sequence,
    through_seq: Sequence,
    domains: String,
    limit: Option<usize>,
}
/// Sections are named explicitly. There is no implicit all-sections read.
fn query_domains(value: &str) -> Result<Vec<Domain>> {
    value
        .split(',')
        .map(|name| Ok(Domain::try_from(name)?))
        .collect()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeQuery {
    scope: String,
}
async fn scope(State(app): State<App>, Query(query): Query<ScopeQuery>) -> Result<Response> {
    blocking(move || {
        let (head, version, clear_version) = app.store.scope_snapshot(&query.scope)?;
        Ok(Json(
            serde_json::json!({"head":head,"scope":query.scope,"version":version,"clearVersion":clear_version}),
        )
        .into_response())
    })
    .await
}
async fn changes(State(app): State<App>, Query(query): Query<ChangesQuery>) -> Result<Response> {
    let cursor = crate::store::ChangeCursor {
        seq: query.after_seq,
        ordinal: query.after_ordinal,
    };
    blocking(move || {
        Ok(Json(app.store.changes(
            &query.epoch,
            &cursor,
            &query.through_seq,
            &query_domains(&query.domains)?,
            query.limit.unwrap_or(128),
        )?)
        .into_response())
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
        let mut absent = Vec::new();
        for candidate in candidates {
            match app.store.object_size(&candidate.hash)? {
                None => absent.push(candidate.hash),
                Some(size) if candidate.size != size.into() => {
                    return Err(Error::new("object-size-mismatch", 409))
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
    let (file, total) = blocking(move || app.store.open_object(&digest)).await?;
    object_response(
        file,
        total,
        &tag_digest,
        "application/octet-stream",
        headers,
    )
    .await
}

async fn object_response(
    file: std::fs::File,
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
            let mut file = tokio::fs::File::from_std(file);
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            file.seek(std::io::SeekFrom::Start(start)).await?;
            response = axum::body::Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
                file.take(length),
                64 * 1024,
            ))
            .into_response();
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
    headers: HeaderMap,
) -> Result<Response> {
    use crate::store::MediaResponse;
    use futures_util::StreamExt;
    let permit = app
        .media_slots
        .try_acquire_owned()
        .map_err(|_| Error::new("media-busy", 429))?;
    let resolved = blocking(move || app.store.resolve_media(&token)).await?;
    let mut response = match resolved {
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
            let (parts, body) = response.into_parts();
            Response::from_parts(
                parts,
                axum::body::Body::from_stream(body.into_data_stream().map(move |item| {
                    let _ = &permit;
                    item
                })),
            )
        }
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
async fn stage(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Result<Response> {
    let changes: ChangeSet = canonical::decode(&body, MAX_METADATA_BYTES)?;
    blocking(move || {
        Ok((
            StatusCode::CREATED,
            Json(app.store.stage_changes(&device, &changes)?),
        )
            .into_response())
    })
    .await
}
async fn cancel_stage(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    blocking(move || app.store.cancel_staged_changes(&device, &id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn begin_stage(
    State(app): State<App>,
    Extension(device): Extension<Device>,
) -> Result<Response> {
    blocking(move || {
        Ok((
            StatusCode::CREATED,
            Json(serde_json::json!({"stagedChangesId":app.store.begin_changes(&device)?})),
        )
            .into_response())
    })
    .await
}
async fn stage_progress(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
) -> Result<Response> {
    blocking(move || Ok(Json(app.store.changes_progress(&device, &id)?).into_response())).await
}
async fn stage_page(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path((id, index)): Path<(String, u64)>,
    body: Bytes,
) -> Result<StatusCode> {
    let page: ChangeSet = canonical::decode(&body, MAX_METADATA_BYTES)?;
    blocking(move || app.store.put_changes_page(&device, &id, index, &page)).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn seal_stage(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
) -> Result<Response> {
    blocking(move || Ok(Json(app.store.seal_changes(&device, &id)?).into_response())).await
}
fn receipt_response(receipt: Receipt) -> Response {
    let status = match receipt.status {
        TerminalStatus::Committed => StatusCode::OK,
        TerminalStatus::Stale => StatusCode::PRECONDITION_FAILED,
        TerminalStatus::Failed => StatusCode::CONFLICT,
    };
    (status, Json(receipt)).into_response()
}
async fn commit(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let intent: CommitIntent = canonical::decode(&body, MAX_METADATA_BYTES)?;
    let if_match = header(&headers, "if-match")
        .ok_or(Error::new("if-match-required", 428))?
        .to_owned();
    blocking(
        move || match app.store.submit_commit(&device, &intent, &if_match)? {
            CommitSubmission::Terminal(receipt) => Ok(receipt_response(receipt)),
            pending => {
                let small = app
                    .store
                    .changes_progress(&device, &intent.staged_changes_id)
                    .is_ok_and(|p| p.next_page == 1.into());
                if small {
                    Ok(receipt_response(
                        app.store.commit(&device, &intent, &if_match)?,
                    ))
                } else {
                    Ok((StatusCode::ACCEPTED, Json(pending)).into_response())
                }
            }
        },
    )
    .await
}
async fn receipt(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
) -> Result<Response> {
    blocking(move || Ok(Json(app.store.operation_status(&device, &id)?).into_response())).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    epoch: String,
    sections: std::collections::BTreeMap<Domain, Sequence>,
}
async fn ack(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Result<StatusCode> {
    let ack: Ack = canonical::decode(&body, MAX_METADATA_BYTES)?;
    blocking(move || app.store.acknowledge(&device, &ack.epoch, &ack.sections)).await?;
    Ok(StatusCode::NO_CONTENT)
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
struct PinRequest {
    epoch: String,
    after_seq: Sequence,
    domains: Vec<Domain>,
}
async fn pin_changes(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Result<Response> {
    let request: PinRequest = canonical::decode(&body, MAX_METADATA_BYTES)?;
    blocking(move || {
        Ok((
            StatusCode::CREATED,
            Json(app.store.pin_changes(
                &device,
                &request.epoch,
                &request.after_seq,
                &request.domains,
            )?),
        )
            .into_response())
    })
    .await
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PinQuery {
    after_seq: Sequence,
    after_ordinal: Sequence,
    limit: Option<usize>,
}
async fn pinned_changes(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
    Query(query): Query<PinQuery>,
) -> Result<Response> {
    blocking(move || {
        Ok(Json(app.store.pinned_changes(
            &device,
            &id,
            &crate::store::ChangeCursor {
                seq: query.after_seq,
                ordinal: query.after_ordinal,
            },
            query.limit.unwrap_or(128),
        )?)
        .into_response())
    })
    .await
}
async fn release_pin(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    blocking(move || app.store.release_pin(&device, &id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointRequest {
    domains: Vec<Domain>,
}
async fn create_checkpoint(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Result<Response> {
    let request: CheckpointRequest = canonical::decode(&body, MAX_METADATA_BYTES)?;
    blocking(move || {
        Ok((
            StatusCode::CREATED,
            Json(app.store.create_checkpoint(&device, &request.domains)?),
        )
            .into_response())
    })
    .await
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CheckpointQuery {
    after_domain: Option<Domain>,
    after_key: Option<String>,
    limit: Option<usize>,
}
async fn checkpoint_page(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
    Query(query): Query<CheckpointQuery>,
) -> Result<Response> {
    let after = match (query.after_domain, query.after_key) {
        (Some(domain), Some(key)) => Some(crate::store::CheckpointCursor { domain, key }),
        (None, None) => None,
        _ => return Err(Error::new("invalid-cursor", 400)),
    };
    blocking(move || {
        Ok(Json(app.store.checkpoint_page(
            &device,
            &id,
            after.as_ref(),
            query.limit.unwrap_or(128),
        )?)
        .into_response())
    })
    .await
}
async fn release_checkpoint(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    blocking(move || app.store.release_checkpoint(&device, &id)).await?;
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
    // Tokio's FIFO semaphore queues CPU materialization, never the library writer
    // or another device's network/Range transfer. Keep it inside the worker lifetime.
    let permit = app
        .materializers
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| Error::new("worker-unavailable", 503))?;
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
    let permit = app
        .materializers
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| Error::new("worker-unavailable", 503))?;
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
}
