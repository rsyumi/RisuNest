use crate::{
    store::{Device, Store},
    Error, Result,
};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Extension, Json, Router,
};
use risunest_sync_wire::{
    batch, canonical, ChangeSet, CommitIntent, Receipt, Sequence, TerminalStatus,
    MAX_METADATA_BYTES,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;

#[derive(Clone)]
struct App {
    store: Arc<Store>,
    slots: Arc<Semaphore>,
    devices: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
}
pub fn router(store: Arc<Store>) -> Router {
    let app = App {
        store,
        slots: Arc::new(Semaphore::new(8)),
        devices: Arc::new(Mutex::new(HashMap::new())),
    };
    Router::new()
        .route("/head", get(head))
        .route("/changes", get(changes))
        .route("/objects/missing", post(missing))
        .route("/objects/batch", post(download_batch))
        .route("/objects/{hash}", get(object))
        .route("/uploads/batch", post(upload_batch))
        .route("/staged-changes", post(stage))
        .route("/staged-changes/{id}", delete(cancel_stage))
        .route("/commits", post(commit))
        .route("/operations/{id}", get(receipt))
        .route("/acks", post(ack))
        .layer(DefaultBodyLimit::max(batch::MAX_BATCH_BYTES))
        .route_layer(middleware::from_fn_with_state(app.clone(), authorize))
        .with_state(app)
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let mut response = (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({"error":self.code})),
        )
            .into_response();
        if self.status == 429 {
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
            let limit = if request.uri().path() == "/uploads/batch" {
                batch::MAX_BATCH_BYTES
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
        let _global_permit = app
            .slots
            .try_acquire_owned()
            .map_err(|_| Error::new("server-busy", 429))?;
        let mut request = request;
        request.extensions_mut().insert(device);
        let mut response = tokio::time::timeout(Duration::from_secs(60), next.run(request))
            .await
            .map_err(|_| Error::new("request-timeout", 408))?;
        response
            .headers_mut()
            .insert("cache-control", "no-store".parse().unwrap());
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
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChangesQuery {
    epoch: String,
    after_seq: Sequence,
    after_ordinal: Sequence,
    through_seq: Sequence,
    limit: Option<usize>,
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
            query.limit.unwrap_or(128),
        )?)
        .into_response())
    })
    .await
}
async fn upload_batch(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Result<Response> {
    blocking(move || {
        // Decode and validate the entire batch before publishing any frame.
        let frames = batch::decode(&body)?;
        let mut hashes = Vec::new();
        for frame in frames {
            app.store.put_object(&device, &frame.hash, frame.bytes)?;
            hashes.push(frame.hash);
        }
        Ok(Json(serde_json::json!({"verified":hashes})).into_response())
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
async fn download_batch(State(app): State<App>, body: Bytes) -> Result<Response> {
    let hashes: Vec<String> = canonical::decode(&body, MAX_METADATA_BYTES)?;
    if hashes.len() > batch::MAX_BATCH_OBJECTS {
        return Err(Error::new("too-many-candidates", 400));
    }
    blocking(move || {
        let mut budget = 8u64;
        // Preflight without allocating payloads.
        for digest in &hashes {
            let size = app
                .store
                .object_size(digest)?
                .ok_or(Error::new("object-not-found", 404))?;
            budget = budget
                .checked_add(size)
                .and_then(|v| v.checked_add(41))
                .ok_or(Error::new("batch-too-large", 413))?;
            if budget > batch::MAX_BATCH_BYTES as u64 {
                return Err(Error::new("batch-too-large", 413));
            }
        }
        let objects: Vec<Vec<u8>> = hashes
            .iter()
            .map(|h| app.store.get_object(h))
            .collect::<Result<_>>()?;
        let bytes = batch::encode(&objects.iter().map(Vec::as_slice).collect::<Vec<_>>())?;
        Ok(([("content-type", "application/octet-stream")], bytes).into_response())
    })
    .await
}
async fn object(
    State(app): State<App>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    blocking(move || {
        let bytes = app.store.get_object(&digest)?;
        let etag = format!("\"{digest}\"");
        let total = bytes.len();
        let mut response;
        if header(&headers, "if-none-match") == Some(&etag) {
            response = StatusCode::NOT_MODIFIED.into_response();
        } else if let Some(range) = header(&headers, "range")
            .filter(|_| header(&headers, "if-range").is_none_or(|v| v == etag))
        {
            if let Some((start, end)) = parse_range(range, total) {
                response =
                    (StatusCode::PARTIAL_CONTENT, bytes[start..=end].to_vec()).into_response();
                response.headers_mut().insert(
                    "content-range",
                    format!("bytes {start}-{end}/{total}").parse().unwrap(),
                );
            } else {
                response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                response
                    .headers_mut()
                    .insert("content-range", format!("bytes */{total}").parse().unwrap());
            }
        } else {
            response = bytes.into_response();
        }
        response.headers_mut().insert("etag", etag.parse().unwrap());
        response
            .headers_mut()
            .insert("accept-ranges", "bytes".parse().unwrap());
        response
            .headers_mut()
            .insert("content-type", "application/octet-stream".parse().unwrap());
        Ok(response)
    })
    .await
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
    blocking(move || {
        Ok(receipt_response(
            app.store.commit(&device, &intent, &if_match)?,
        ))
    })
    .await
}
async fn receipt(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    Path(id): Path<String>,
) -> Result<Response> {
    blocking(move || Ok(Json(app.store.receipt(&device, &id)?).into_response())).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    epoch: String,
    seq: Sequence,
}
async fn ack(
    State(app): State<App>,
    Extension(device): Extension<Device>,
    body: Bytes,
) -> Result<StatusCode> {
    let ack: Ack = canonical::decode(&body, MAX_METADATA_BYTES)?;
    blocking(move || app.store.acknowledge(&device, &ack.epoch, &ack.seq)).await?;
    Ok(StatusCode::NO_CONTENT)
}
