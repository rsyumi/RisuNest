//! File-backed loopback transport. Neither the response planner nor this
//! transport collects a complete media body in native or JavaScript memory.
use crate::server_sync::media::MediaProvider;
use axum::{
    body::Body,
    extract::State,
    http::{header, Request, Response, StatusCode},
    Router,
};
use risunest_sync_connect::media::{MediaObject, REFRESH_PATH};
use std::{io, net::Ipv4Addr, path::PathBuf, sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;

const CHUNK_BYTES: usize = 64 * 1024;
const MAX_TRANSFERS: usize = 8;

#[derive(Clone)]
struct Files {
    root: PathBuf,
    authority: String,
    prefix: String,
    slots: Arc<Semaphore>,
    remote: Arc<MediaProvider>,
}

pub(crate) struct MediaServer {
    base_url: String,
    task: tauri::async_runtime::JoinHandle<()>,
}

pub(crate) struct MediaServerState(Result<MediaServer, String>);

impl MediaServerState {
    pub(crate) fn initialize(root: PathBuf) -> Self {
        Self(MediaServer::start(root).map_err(|_| "native-media-unavailable".to_owned()))
    }
}

impl Drop for MediaServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MediaServer {
    #[cfg(test)]
    pub(crate) fn test_base_url(&self) -> &str {
        &self.base_url
    }
    pub(crate) fn start(root: PathBuf) -> io::Result<Self> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let authority = listener.local_addr()?.to_string();
        let prefix = format!("/{}/", uuid::Uuid::new_v4().simple());
        let base_url = format!("http://{authority}{prefix}");
        let remote = Arc::new(
            MediaProvider::new(root.clone(), format!("http://{authority}"))
                .map_err(|_| io::Error::other("media capability unavailable"))?,
        );
        let state = Files {
            root,
            authority,
            prefix,
            slots: Arc::new(Semaphore::new(MAX_TRANSFERS)),
            remote,
        };
        let task = tauri::async_runtime::spawn(async move {
            let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                return;
            };
            let _ = axum::serve(listener, Router::new().fallback(serve).with_state(state)).await;
        });
        Ok(Self { base_url, task })
    }
}

#[tauri::command]
pub(crate) fn native_media_base_url(
    state: tauri::State<'_, MediaServerState>,
) -> Result<String, String> {
    state
        .0
        .as_ref()
        .map(|server| server.base_url.clone())
        .map_err(Clone::clone)
}

fn empty(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, "0")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::empty())
        .unwrap()
}

async fn serve(State(state): State<Files>, request: Request<Body>) -> Response<Body> {
    // Exact authority prevents DNS rebinding; the per-process capability is
    // required even for local callers. Never expose a filesystem path or token
    // for the synchronization server here.
    if request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        != Some(&state.authority)
    {
        return empty(StatusCode::NOT_FOUND);
    }
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("");
    if path.len() > 16384 {
        return empty(StatusCode::NOT_FOUND);
    }
    let (uri, remote, refresh) = if let Some(token) = path.strip_prefix(REFRESH_PATH) {
        let Ok(object) = state.remote.verify_refresh(token) else {
            return empty(StatusCode::NOT_FOUND);
        };
        let key = format!(
            "assets-v2/objects/{}/{}",
            &object.hash[..2],
            &object.hash[2..]
        );
        let mut url =
            url::Url::parse(&format!("http://risuasset.localhost/{}", hex::encode(key))).unwrap();
        url.query_pairs_mut()
            .append_pair("mime", &object.mime)
            .append_pair("size", object.size.as_str());
        (url.to_string(), Some(object), true)
    } else if let Some(path) = path.strip_prefix(&state.prefix) {
        let uri = format!("http://risuasset.localhost/{path}");
        let remote = super::decode_physical_key(&uri)
            .and_then(|key| super::cas_content_hash(&key))
            .and_then(|hash| {
                super::cas_descriptor(&uri).map(|(mime, size)| MediaObject {
                    hash,
                    mime,
                    size: size.into(),
                })
            });
        (uri, remote, false)
    } else {
        return empty(StatusCode::NOT_FOUND);
    };
    let Ok(uri) = uri.parse() else {
        return empty(StatusCode::NOT_FOUND);
    };
    let permit =
        match tokio::time::timeout(Duration::from_secs(30), state.slots.acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            _ => return empty(StatusCode::SERVICE_UNAVAILABLE),
        };
    let mut request = request.map(|_| ());
    *request.uri_mut() = uri;
    let response = match tokio::task::spawn_blocking(move || {
        let response = super::prepare_response(&state.root, request);
        if response.status() == StatusCode::NOT_FOUND {
            if let Some(object) = remote {
                return match state.remote.url(&object, refresh) {
                    Ok(url) => Response::builder()
                        .status(StatusCode::TEMPORARY_REDIRECT)
                        .header(header::LOCATION, url)
                        .header(header::CACHE_CONTROL, "no-store")
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(None)
                        .unwrap(),
                    Err(error) => Response::builder()
                        .status(
                            StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_GATEWAY),
                        )
                        .header(header::CACHE_CONTROL, "no-store")
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(None)
                        .unwrap(),
                };
            }
        }
        response
    })
    .await
    {
        Ok(response) => response,
        Err(_) => return empty(StatusCode::INTERNAL_SERVER_ERROR),
    };
    let (mut parts, reader) = response.into_parts();
    parts
        .headers
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    parts
        .headers
        .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    let body = match reader {
        None => Body::empty(),
        Some(reader) => {
            let remaining = reader.limit();
            let reader = tokio::fs::File::from_std(reader.into_inner()).take(remaining);
            // The permit and file are released when the client disconnects or
            // the body finishes. Each poll reads at most one fixed-size chunk.
            let stream =
                futures::stream::try_unfold((reader, permit), |(mut reader, permit)| async move {
                    if reader.limit() == 0 {
                        return Ok::<_, io::Error>(None);
                    }
                    let mut bytes = vec![0; (CHUNK_BYTES as u64).min(reader.limit()) as usize];
                    let count = reader.read(&mut bytes).await?;
                    if count == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "media truncated",
                        ));
                    }
                    bytes.truncate(count);
                    Ok(Some((bytes, (reader, permit))))
                });
            Body::from_stream(stream)
        }
    };
    Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests;
