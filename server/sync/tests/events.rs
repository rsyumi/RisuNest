mod common;
use common::{changes, stage};
use reqwest::{Client, RequestBuilder, StatusCode};
use risunest_sync_server::{
    http,
    store::{DeviceCredential, Store},
    workload::Workload,
};
use risunest_sync_wire::{canonical, transfer::{self, Frame}, RemoteHead};
use std::{sync::Arc, time::Duration};

struct Server {
    base: String,
    store: Arc<Store>,
    client: Client,
    workload: Workload,
    device: DeviceCredential,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Server {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::init(dir.path()).unwrap());
        let device = store.add_device().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let workload = Workload::new();
        let router = http::router_with_workload(store.clone(), workload.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            base,
            store,
            // No total deadline: a held stream is expected to outlive a request.
            client: Client::builder().no_proxy().build().unwrap(),
            workload,
            device,
            task,
            _dir: dir,
        }
    }
    fn auth(&self, request: RequestBuilder) -> RequestBuilder {
        request
            .bearer_auth(&self.device.token)
            .header("x-risu-library", &self.device.library_id)
    }
    async fn events(&self) -> reqwest::Response {
        self.auth(self.client.get(format!("{}/events", self.base)))
            .send()
            .await
            .unwrap()
    }
    async fn head(&self) -> RemoteHead {
        self.auth(self.client.get(format!("{}/head", self.base)))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    /// One small library change, committed through the ordinary endpoints.
    async fn commit(&self, key: &str, body: &[u8]) {
        let response = self
            .auth(self.client.post(format!("{}/uploads/frames", self.base)))
            .body(transfer::encode(&[Frame::Full(body.to_vec())]).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let head = self.head().await;
        let device = self
            .store
            .authenticate(&self.device.library_id, &self.device.token)
            .unwrap();
        let intent = stage(&self.store, &device, &head, 1, &changes(key, body));
        let response = self
            .auth(self.client.post(format!("{}/commits", self.base)))
            .header("if-match", head.etag())
            .body(canonical::encode(&intent).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

/// Reads the next announcement, failing rather than hanging if none arrives.
async fn next_announcement(buffer: &mut String, stream: &mut reqwest::Response) -> String {
    loop {
        if let Some(index) = buffer.find("\n\n") {
            let frame = buffer[..index].to_owned();
            buffer.drain(..index + 2);
            if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data:")) {
                return data.trim().to_owned();
            }
            continue;
        }
        let chunk = tokio::time::timeout(Duration::from_secs(10), stream.chunk())
            .await
            .expect("an announcement must arrive")
            .expect("the stream must stay readable")
            .expect("the stream must not end");
        buffer.push_str(std::str::from_utf8(&chunk).unwrap());
    }
}

#[tokio::test]
async fn a_committed_head_reaches_a_held_stream_and_an_unknown_client_is_refused() {
    let server = Server::start().await;
    assert_eq!(
        server
            .client
            .get(format!("{}/events", server.base))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let mut stream = server.events().await;
    assert_eq!(stream.status(), StatusCode::OK);
    assert_eq!(
        stream.headers()["content-type"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap(),
        "text/event-stream"
    );
    let mut buffer = String::new();
    // Connecting is itself a head confirmation, so the current head arrives
    // before anything moves.
    let opened = next_announcement(&mut buffer, &mut stream).await;
    assert_eq!(opened, server.head().await.head_id);

    server.commit("r1:character:synthetic", b"synthetic body").await;
    let announced = next_announcement(&mut buffer, &mut stream).await;
    let head = server.head().await;
    assert_eq!(announced, head.head_id);
    assert_ne!(announced, opened);
    server.task.abort();
}

#[tokio::test]
async fn held_streams_occupy_no_admission_slot_and_leave_the_server_drainable() {
    let server = Server::start().await;
    let mut streams = Vec::new();
    for _ in 0..4 {
        let mut stream = server.events().await;
        assert_eq!(stream.status(), StatusCode::OK);
        let mut buffer = String::new();
        // Wait for the first announcement so the stream is really established.
        next_announcement(&mut buffer, &mut stream).await;
        streams.push(stream);
    }
    // Four streams exceed the two concurrent requests one device is admitted,
    // yet ordinary requests still pass and maintenance still sees a drain.
    server.head().await;
    let status = server.workload.status().unwrap();
    assert_eq!(status.active_requests, 0);
    assert!(status.drained);
    drop(streams);
    server.task.abort();
}
