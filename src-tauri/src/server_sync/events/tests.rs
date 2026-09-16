use super::*;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio::time::Instant;

const HEADERS: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
const REFUSED: &[u8] = b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n";

#[derive(Default)]
struct Counter(AtomicUsize);
impl HintSink for Arc<Counter> {
    fn hint(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
impl Counter {
    fn count(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

struct Fake {
    endpoint: String,
    address: std::net::SocketAddr,
    connections: Arc<AtomicUsize>,
    closing: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            // Release a worker still waiting for a connection the holder,
            // having stopped, will never make.
            let _ = TcpStream::connect(self.address);
            let _ = worker.join();
        }
    }
}

/// One scripted response per connection, closed after the paired delay. Zero
/// closes at once; a longer hold models a peer that stays quiet but connected.
fn once(response: &[u8]) -> (Vec<u8>, Duration) {
    (response.to_vec(), Duration::ZERO)
}

fn fake(script: Vec<(Vec<u8>, Duration)>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a notification peer");
    let address = listener.local_addr().unwrap();
    let endpoint = format!("http://{address}");
    let connections = Arc::new(AtomicUsize::new(0));
    let closing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let served = connections.clone();
    let stopping = closing.clone();
    let worker = std::thread::spawn(move || {
        for (response, held) in script {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            if stopping.load(Ordering::Acquire) {
                return;
            }
            served.fetch_add(1, Ordering::Relaxed);
            read_request(&mut socket);
            let _ = socket.write_all(&response);
            let _ = socket.flush();
            // Keeping the socket elsewhere lets the peer accept the next
            // connection while this one stays open.
            std::thread::spawn(move || {
                std::thread::sleep(held);
                drop(socket);
            });
        }
    });
    Fake {
        endpoint,
        address,
        connections,
        closing,
        worker: Some(worker),
    }
}

fn read_request(socket: &mut TcpStream) {
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        match socket.read(&mut byte) {
            Ok(0) | Err(_) => return,
            Ok(_) => request.push(byte[0]),
        }
    }
}

fn binding(endpoint: &str) -> ServerConfig {
    ServerConfig {
        directory: None,
        endpoint: endpoint.into(),
        library_id: "synthetic-library".into(),
        device_id: "synthetic-device".into(),
        token: "0".repeat(64),
    }
}

struct Outcome {
    hints: usize,
    connections: usize,
    /// Whether the holder gave up by itself rather than being told to stop.
    released_itself: bool,
}

fn quick() -> Timing {
    Timing {
        first_retry: Duration::from_millis(10),
        idle: Duration::from_secs(30),
    }
}

/// Holds until `expected` notifications have been reported, then settles and
/// stops. Waiting on the count rather than on a clock keeps a loaded machine
/// from reading a slow start as a lost contract, and the settle still catches a
/// holder that reports more than it should.
fn run_until(peer: &Fake, expected: usize) -> Outcome {
    run_with(peer, Some(expected), quick())
}

/// Holds until the holder gives up, or until the deadline says it will not.
fn run_to_release(peer: &Fake, timing: Timing) -> Outcome {
    run_with(peer, None, timing)
}

fn run_with(peer: &Fake, expected: Option<usize>, timing: Timing) -> Outcome {
    let sink = Arc::new(Counter::default());
    let observed = sink.clone();
    let resolve: Resolve = {
        let config = binding(&peer.endpoint);
        Arc::new(move || Some(config.clone()))
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let counted = observed.clone();
    let released_itself = runtime.block_on(async move {
        let stop = Stop::new();
        let holder = tokio::spawn(hold(resolve, Arc::new(sink), stop.clone(), timing));
        let released = match expected {
            Some(expected) => {
                let deadline = Instant::now() + Duration::from_secs(20);
                while counted.count() < expected && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                holder.is_finished()
            }
            None => tokio::time::timeout(Duration::from_secs(20), holder)
                .await
                .is_ok(),
        };
        stop.stop();
        released
    });
    runtime.shutdown_timeout(Duration::from_secs(5));
    Outcome {
        hints: observed.count(),
        connections: peer.connections.load(Ordering::Relaxed),
        released_itself,
    }
}

/// Contract 3. A held connection is never assumed to have carried everything,
/// so establishing one is itself a reason to confirm the head.
#[test]
fn every_reconnection_confirms_the_head_even_when_nothing_was_announced() {
    let peer = fake(vec![once(HEADERS), once(HEADERS), once(HEADERS)]);
    let outcome = run_until(&peer, 3);
    assert_eq!(outcome.connections, 3);
    assert_eq!(outcome.hints, 3);
}

/// Stopping before the holder has run once still stops it. Nothing connects.
#[test]
fn a_stop_that_arrives_before_the_first_attempt_is_still_observed() {
    let peer = fake(vec![once(HEADERS)]);
    let sink = Arc::new(Counter::default());
    let observed = sink.clone();
    let resolve: Resolve = {
        let config = binding(&peer.endpoint);
        Arc::new(move || Some(config.clone()))
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let released = runtime.block_on(async move {
        let stop = Stop::new();
        let holder = tokio::spawn(hold(resolve, Arc::new(sink), stop.clone(), quick()));
        stop.stop();
        tokio::time::timeout(Duration::from_secs(10), holder)
            .await
            .is_ok()
    });
    runtime.shutdown_timeout(Duration::from_secs(5));
    assert!(released);
    assert_eq!(peer.connections.load(Ordering::Relaxed), 0);
    assert_eq!(observed.count(), 0);
}

/// A binding the server does not accept is not retried on a timer. The user
/// sees the same refusal from the ordinary synchronisation attempt.
#[test]
fn a_refused_binding_stops_holding_instead_of_reconnecting() {
    let peer = fake(vec![once(REFUSED), once(HEADERS)]);
    let outcome = run_to_release(&peer, quick());
    assert!(outcome.released_itself);
    assert_eq!(outcome.connections, 1);
    assert_eq!(outcome.hints, 0);
}

/// Keep-alive traffic holds the connection open; it is not a change.
#[test]
fn a_keep_alive_comment_does_not_wake_synchronization() {
    let mut announced = HEADERS.to_vec();
    announced.extend_from_slice(b":

:

event: head
data: 0123abcd

:

");
    let peer = fake(vec![once(&announced)]);
    let outcome = run_until(&peer, 2);
    assert_eq!(outcome.connections, 1);
    assert_eq!(outcome.hints, 2);
}

/// A connection that stays open but silent is not evidence of anything. It is
/// replaced, and the replacement confirms the head.
#[test]
fn a_connection_that_falls_silent_is_replaced_instead_of_trusted() {
    let peer = fake(vec![
        (HEADERS.to_vec(), Duration::from_secs(5)),
        once(REFUSED),
    ]);
    let outcome = run_to_release(
        &peer,
        Timing {
            first_retry: Duration::from_millis(10),
            idle: Duration::from_millis(200),
        },
    );
    assert!(outcome.released_itself);
    assert_eq!(outcome.connections, 2);
    assert_eq!(outcome.hints, 1);
}

/// Invariant 22 at the notification boundary. The renderer is told that
/// something moved and nothing else: no endpoint, device or token.
#[test]
fn a_change_notification_reaches_the_renderer_carrying_no_credential() {
    use tauri::Listener;
    let app = tauri::test::mock_builder()
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .expect("build a renderer host");
    let seen = Arc::new(Mutex::new(Vec::new()));
    for name in [DEVICE_CHANGED_EVENT, REMOTE_HINT_EVENT] {
        let seen = seen.clone();
        app.listen(name, move |event| {
            seen.lock()
                .unwrap()
                .push((name, event.payload().to_owned()));
        });
    }
    notify_device_changed(app.handle());
    notify_remote_hint(app.handle());
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![
            (DEVICE_CHANGED_EVENT, "null".to_owned()),
            (REMOTE_HINT_EVENT, "null".to_owned()),
        ]
    );
}
