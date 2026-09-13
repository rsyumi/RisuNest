pub mod discovery;
pub mod storage;

use crate::{
    connection::ConnectionOptions, runtime::ConnectionRuntime, store::Store, Error, Result,
};
use axum::{
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{watch, Mutex, RwLock},
    task::JoinHandle,
};

struct RuntimeState {
    connection: Option<ConnectionRuntime>,
    revision: u64,
}
struct Context {
    store: Arc<Store>,
    origin: SocketAddr,
    locator: discovery::Discovery,
    session: String,
    runtime: Mutex<RuntimeState>,
    usage: RwLock<storage::StorageUsage>,
    started: Instant,
    stop: watch::Sender<bool>,
}

pub struct Management {
    context: Arc<Context>,
    server: JoinHandle<()>,
    scanner: JoinHandle<()>,
}

impl Management {
    pub async fn start(store: Arc<Store>, origin: SocketAddr) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let locator = discovery::Discovery {
            address: listener.local_addr()?,
            token: discovery::request_id()?,
        };
        let (stop, _) = watch::channel(false);
        let runtime = ConnectionRuntime::start(store.clone(), origin)?;
        let context = Arc::new(Context {
            store,
            origin,
            locator,
            session: discovery::request_id()?,
            runtime: Mutex::new(RuntimeState {
                connection: Some(runtime),
                revision: 0,
            }),
            usage: RwLock::new(storage::StorageUsage::default()),
            started: Instant::now(),
            stop,
        });
        if let Err(error) = context.locator.save(context.store.data_path()) {
            if let Some(runtime) = context.runtime.lock().await.connection.take() {
                runtime.shutdown().await;
            }
            return Err(error);
        }
        let app = Router::new()
            .route("/status", get(status))
            .route("/connection", post(configure))
            .route("/devices", post(issue))
            .route("/devices/{id}/revoke", post(revoke))
            .route("/tunnel/{action}", post(tunnel))
            .route("/registry/repost", post(repost))
            .route("/shutdown", post(shutdown))
            .layer(DefaultBodyLimit::max(16 * 1024))
            .layer(middleware::from_fn_with_state(context.clone(), authorize))
            .with_state(context.clone());
        let mut stopped = context.stop.subscribe();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stopped.changed().await;
                })
                .await;
        });
        let shared = context.clone();
        let mut stopped = context.stop.subscribe();
        let scanner = tokio::spawn(async move {
            loop {
                let root = shared.store.data_path().to_owned();
                let measured = tokio::task::spawn_blocking(move || storage::measure(&root)).await;
                let mut usage = shared.usage.write().await;
                match measured {
                    Ok(Ok(value)) => *usage = value,
                    Ok(Err(error)) => usage.error = Some(error.code),
                    Err(_) => usage.error = Some("storage-measurement-failed"),
                }
                drop(usage);
                if *stopped.borrow() {
                    break;
                }
                tokio::select! { _ = stopped.changed() => break, _ = tokio::time::sleep(Duration::from_secs(60)) => () }
            }
        });
        Ok(Self {
            context,
            server,
            scanner,
        })
    }
    pub fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.context.stop.subscribe()
    }
    pub fn address(&self) -> SocketAddr {
        self.context.locator.address
    }
    pub async fn close(self) {
        self.context.stop.send_replace(true);
        if let Some(runtime) = self.context.runtime.lock().await.connection.take() {
            runtime.shutdown().await;
        }
        let _ = self.server.await;
        let _ = self.scanner.await;
        let _ = std::fs::remove_file(self.context.store.data_path().join("management-session"));
    }
}

async fn authorize(State(ctx): State<Arc<Context>>, request: Request, next: Next) -> Response {
    let h = request.headers();
    let expected_host = ctx.locator.address.to_string();
    let valid_host = h.get_all(header::HOST).iter().count() == 1
        && h.get(header::HOST).and_then(|v| v.to_str().ok()) == Some(expected_host.as_str());
    let token = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let valid_token = token.is_some_and(|token| {
        token.len() == ctx.locator.token.len()
            && token
                .bytes()
                .zip(ctx.locator.token.bytes())
                .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                == 0
    });
    if !valid_host || h.contains_key(header::ORIGIN) || h.contains_key("sec-fetch-site") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"management-origin-denied"})),
        )
            .into_response();
    }
    if !valid_token || h.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"management-unauthorized"})),
        )
            .into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| Error::new("management-operation-failed", 503))?
}

async fn status(State(ctx): State<Arc<Context>>) -> Result<Json<Value>> {
    let rt = ctx.runtime.lock().await;
    let store = ctx.store.clone();
    let (connection, persisted, devices) = blocking(move || {
        Ok((
            store.management_connection()?,
            store.connection_status()?,
            store.managed_devices()?,
        ))
    })
    .await?;
    let (tunnel, publication) = match &rt.connection {
        Some(runtime) => (
            json!(*runtime.tunnel.borrow()),
            json!(*runtime.publication.borrow()),
        ),
        None => (
            json!({"phase":"stopped","endpoint":null,"error":null}),
            json!({"phase":"stopped","error":null}),
        ),
    };
    Ok(Json(json!({
        "revision":format!("{}:{}",ctx.session,rt.revision), "uptimeSeconds":ctx.started.elapsed().as_secs(),
        "connection":connection, "connectionState":persisted, "tunnel":tunnel, "publication":publication,
        "storage":*ctx.usage.read().await, "devices":devices,
        "defaultRegistryUrl":option_env!("RISUNEST_DEFAULT_REGISTRY_URL"),
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Mutation {
    revision: String,
    options: Option<ConnectionOptions>,
    name: Option<String>,
    request_id: Option<String>,
}
fn check(ctx: &Context, rt: &mut RuntimeState, input: &Mutation) -> Result<()> {
    if input.revision != format!("{}:{}", ctx.session, rt.revision) {
        return Err(Error::new("management-stale-state", 409));
    }
    // Increment even if a later action fails after an external side effect.
    rt.revision += 1;
    Ok(())
}

async fn configure(
    State(ctx): State<Arc<Context>>,
    Json(input): Json<Mutation>,
) -> Result<Json<Value>> {
    let options = input
        .options
        .clone()
        .ok_or(Error::new("connection-options-required", 400))?;
    options.validate()?;
    let mut rt = ctx.runtime.lock().await;
    check(&ctx, &mut rt, &input)?;
    if let Some(runtime) = rt.connection.take() {
        runtime.shutdown().await;
    }
    let store = ctx.store.clone();
    let result = blocking(move || store.configure_connection(options)).await;
    rt.connection = Some(ConnectionRuntime::start(ctx.store.clone(), ctx.origin)?);
    result?;
    drop(rt);
    status(State(ctx)).await
}
async fn issue(
    State(ctx): State<Arc<Context>>,
    Json(input): Json<Mutation>,
) -> Result<Json<Value>> {
    let mut rt = ctx.runtime.lock().await;
    check(&ctx, &mut rt, &input)?;
    let state = ctx.store.connection_status()?;
    if state.mode == "managed"
        && rt
            .connection
            .as_ref()
            .is_none_or(|r| r.tunnel.borrow().phase != "connected")
    {
        return Err(Error::new("public-endpoint-not-ready", 409));
    }
    let name = input.name.ok_or(Error::new("device-name-required", 400))?;
    let request = input
        .request_id
        .ok_or(Error::new("registration-request-required", 400))?;
    let store = ctx.store.clone();
    let uri = blocking(move || store.issue_named_registration(&name, &request)).await?;
    Ok(Json(json!({"uri":uri})))
}
async fn revoke(
    State(ctx): State<Arc<Context>>,
    Path(id): Path<String>,
    Json(input): Json<Mutation>,
) -> Result<Json<Value>> {
    let mut rt = ctx.runtime.lock().await;
    check(&ctx, &mut rt, &input)?;
    let store = ctx.store.clone();
    blocking(move || store.revoke_device(&id)).await?;
    drop(rt);
    status(State(ctx)).await
}
async fn tunnel(
    State(ctx): State<Arc<Context>>,
    Path(action): Path<String>,
    Json(input): Json<Mutation>,
) -> Result<Json<Value>> {
    if !["start", "stop", "restart"].contains(&action.as_str()) {
        return Err(Error::new("invalid-tunnel-action", 400));
    }
    let mut rt = ctx.runtime.lock().await;
    check(&ctx, &mut rt, &input)?;
    if ctx.store.managed_cloudflared()?.is_none() {
        return Err(Error::new("managed-tunnel-not-configured", 409));
    }
    if let Some(runtime) = rt.connection.take() {
        runtime.shutdown().await;
    }
    if action != "stop" {
        rt.connection = Some(ConnectionRuntime::start(ctx.store.clone(), ctx.origin)?);
    }
    drop(rt);
    status(State(ctx)).await
}
async fn repost(
    State(ctx): State<Arc<Context>>,
    Json(input): Json<Mutation>,
) -> Result<Json<Value>> {
    let mut rt = ctx.runtime.lock().await;
    check(&ctx, &mut rt, &input)?;
    let runtime = rt
        .connection
        .as_ref()
        .ok_or(Error::new("connection-runtime-stopped", 409))?;
    let store = ctx.store.clone();
    blocking(move || store.request_republication()).await?;
    runtime.publication_changed();
    drop(rt);
    status(State(ctx)).await
}
async fn shutdown(
    State(ctx): State<Arc<Context>>,
    Json(input): Json<Mutation>,
) -> Result<Json<Value>> {
    let mut rt = ctx.runtime.lock().await;
    check(&ctx, &mut rt, &input)?;
    ctx.stop.send_replace(true);
    Ok(Json(json!({"stopping":true})))
}
