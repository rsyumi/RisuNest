//! Change notifications for the synchronisation scheduler. The device
//! credential stays in this process: the renderer is only told that something
//! may have moved and confirms the remote head itself.
//!
//! A held connection lowers the delay before a remote change is noticed. It is
//! not the path a change travels, and it is not kept while the app is not
//! running: the scheduler still polls, and polling reaches the same state.
use super::client::ServerConfig;
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager, Runtime};

/// Emitted after a local write advanced the device revision in a section that
/// can be synchronised.
pub(crate) const DEVICE_CHANGED_EVENT: &str = "risu-server-sync-device-changed";

/// Emitted when the remote may have moved. Carries no payload at all, so the
/// renderer cannot mistake a notification for a confirmed head.
pub(crate) const REMOTE_HINT_EVENT: &str = "risu-server-sync-remote-hint";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RETRY: Duration = Duration::from_secs(60);
/// A connection that stood this long is treated as healthy, so the next drop
/// retries promptly instead of inheriting the previous delay.
const SETTLED_CONNECTION: Duration = Duration::from_secs(30);
/// A peer that never completes a frame is not speaking this protocol.
const MAX_PENDING_BYTES: usize = 64 * 1024;

/// How long a connection may go quiet before it is replaced. A live peer keeps
/// it open, so silence past this is a connection that only looks alive.
#[derive(Clone, Copy)]
pub(crate) struct Timing {
    pub first_retry: Duration,
    pub idle: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            first_retry: Duration::from_secs(1),
            idle: Duration::from_secs(90),
        }
    }
}

pub(crate) fn notify_device_changed<R: Runtime>(app: &AppHandle<R>) {
    // A lost notification costs latency; the scheduler still polls.
    let _ = app.emit(DEVICE_CHANGED_EVENT, ());
}

pub(crate) fn notify_remote_hint<R: Runtime>(app: &AppHandle<R>) {
    let _ = app.emit(REMOTE_HINT_EVENT, ());
}

/// What a held connection reports. It takes no argument, so a notification can
/// carry nothing beyond its own occurrence.
pub(crate) trait HintSink: Send + Sync + 'static {
    fn hint(&self);
}

struct RendererSink<R: Runtime>(AppHandle<R>);
impl<R: Runtime> HintSink for RendererSink<R> {
    fn hint(&self) {
        notify_remote_hint(&self.0);
    }
}

#[derive(Clone)]
pub(crate) struct Stop(Arc<tokio::sync::watch::Sender<bool>>);
impl Stop {
    pub(crate) fn new() -> Self {
        Self(Arc::new(tokio::sync::watch::Sender::new(false)))
    }
    pub(crate) fn stop(&self) {
        // Stored rather than sent: a holder between two waits has no receiver
        // yet, and a stop it never observes would keep the connection.
        self.0.send_replace(true);
    }
    fn stopped(&self) -> bool {
        *self.0.borrow()
    }
    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    async fn settle(&self) {
        let mut observer = self.0.subscribe();
        let _ = observer.wait_for(|stopped| *stopped).await;
    }
    /// Waits out a delay, returning as soon as the holder is stopped.
    async fn wait(&self, delay: Duration) {
        let _ = tokio::time::timeout(delay, self.settle()).await;
    }
    /// Abandons an operation the moment the holder is stopped.
    async fn race<T>(&self, operation: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            value = operation => Some(value),
            _ = self.settle() => None,
        }
    }
}

enum Attempt {
    /// The connection ended. Another one may succeed.
    Ended,
    /// This binding cannot hold a connection at all.
    Refused,
}

type Resolve = Arc<dyn Fn() -> Option<ServerConfig> + Send + Sync>;

/// Holds one connection at a time, reconnecting with a bounded delay. Both the
/// credential and the retry policy stay on this side.
pub(crate) async fn hold(resolve: Resolve, sink: Arc<dyn HintSink>, stop: Stop, timing: Timing) {
    let mut delay = timing.first_retry;
    while !stop.stopped() {
        let read = resolve.clone();
        let config = match tokio::task::spawn_blocking(move || read()).await {
            Ok(config) => config,
            Err(_) => None,
        };
        let Some(config) = config else {
            // Nothing is bound yet. Look again later rather than treating an
            // unbound install as a failing connection.
            stop.wait(MAX_RETRY).await;
            continue;
        };
        let started = Instant::now();
        match attempt(&config, sink.as_ref(), &stop, timing.idle).await {
            Attempt::Refused => return,
            Attempt::Ended if started.elapsed() >= SETTLED_CONNECTION => {
                delay = timing.first_retry
            }
            Attempt::Ended => (),
        }
        if stop.stopped() {
            return;
        }
        stop.wait(delay).await;
        delay = (delay * 2).min(MAX_RETRY);
    }
}

async fn attempt(
    config: &ServerConfig,
    sink: &dyn HintSink,
    stop: &Stop,
    idle: Duration,
) -> Attempt {
    let Ok(base) = config.validate() else {
        return Attempt::Refused;
    };
    let Ok(url) = base.join("events") else {
        return Attempt::Refused;
    };
    if url.origin() != base.origin() {
        return Attempt::Refused;
    }
    let Ok(http) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
    else {
        return Attempt::Ended;
    };
    let Some(Ok(mut response)) = stop
        .race(
            http.get(url)
                .bearer_auth(&config.token)
                .header("x-risu-library", &config.library_id)
                .header("accept", "text/event-stream")
                .header("accept-encoding", "identity")
                .send(),
        )
        .await
    else {
        return Attempt::Ended;
    };
    match response.status().as_u16() {
        200 => (),
        401 | 403 | 404 => return Attempt::Refused,
        _ => return Attempt::Ended,
    }
    // A fresh connection confirms the head on its own. This side never assumes
    // it saw every announcement the one before it might have carried.
    sink.hint();
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let Some(Ok(Ok(Some(chunk)))) =
            stop.race(tokio::time::timeout(idle, response.chunk())).await
        else {
            return Attempt::Ended;
        };
        pending.extend(chunk.iter().copied().filter(|byte| *byte != b'\r'));
        while let Some(end) = frame_end(&pending) {
            let frame: Vec<u8> = pending.drain(..end).collect();
            if carries_a_field(&frame) {
                sink.hint();
            }
        }
        if pending.len() > MAX_PENDING_BYTES {
            return Attempt::Ended;
        }
    }
}

fn frame_end(pending: &[u8]) -> Option<usize> {
    pending
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .map(|index| index + 2)
}

/// A keep-alive comment is not a change. Only a frame with a field is.
fn carries_a_field(frame: &[u8]) -> bool {
    frame
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"data:") || line.starts_with(b"event:"))
}

#[derive(Default)]
pub(crate) struct ServerSyncEventsState {
    held: Mutex<Option<Stop>>,
}

/// Holds notifications while the app can act on them. Starting twice keeps the
/// one connection already held.
pub(crate) fn start<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<ServerSyncEventsState>();
    let Ok(mut held) = state.held.lock() else {
        return;
    };
    if held.is_some() {
        return;
    }
    let stop = Stop::new();
    *held = Some(stop.clone());
    let reader = app.clone();
    let resolve: Resolve = Arc::new(move || {
        crate::persistent_store::commands::with_store(reader.state(), |store| {
            Ok(store.server_config().ok().flatten())
        })
        .ok()
        .flatten()
    });
    let sink = Arc::new(RendererSink(app.clone()));
    let host = app.clone();
    tauri::async_runtime::spawn(async move {
        hold(resolve, sink, stop.clone(), Timing::default()).await;
        // A holder that gave up frees its place, so a later binding or a
        // renewed lifecycle can hold a connection again.
        release_held(&host, &stop);
    });
}

/// Clears the slot only if it still holds this connection, so a holder that
/// ended long ago cannot cancel the one that replaced it.
fn release_held<R: Runtime>(app: &AppHandle<R>, stop: &Stop) {
    let state = app.state::<ServerSyncEventsState>();
    let Ok(mut held) = state.held.lock() else {
        return;
    };
    if held.as_ref().is_some_and(|current| current.same(stop)) {
        *held = None;
    }
}

pub(crate) fn release<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<ServerSyncEventsState>();
    let Ok(mut held) = state.held.lock() else {
        return;
    };
    if let Some(stop) = held.take() {
        stop.stop();
    }
}

#[tauri::command]
pub(crate) fn server_sync_events_start(app: AppHandle) {
    start(&app);
}

#[tauri::command]
pub(crate) fn server_sync_events_stop(app: AppHandle) {
    release(&app);
}

#[cfg(test)]
mod tests;
