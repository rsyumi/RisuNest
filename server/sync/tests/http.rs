mod common;
use common::changes;
use reqwest::{Client, RequestBuilder, StatusCode};
use risunest_sync_server::{
    http,
    store::{DeviceCredential, Store},
};
use risunest_sync_wire::{batch, hash, CommitIntent, Receipt, RemoteHead, TerminalStatus};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Server {
    base: String,
    store: Arc<Store>,
    client: Client,
    a: DeviceCredential,
    b: DeviceCredential,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::init(dir.path()).unwrap());
        let a = store.add_device().unwrap();
        let b = store.add_device().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = http::router(store.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            base,
            store,
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            a,
            b,
            task,
            _dir: dir,
        }
    }
    fn auth(&self, req: RequestBuilder, device: &DeviceCredential) -> RequestBuilder {
        req.bearer_auth(&device.token)
            .header("x-risu-library", &device.library_id)
    }
    async fn head(&self, device: &DeviceCredential) -> RemoteHead {
        self.auth(self.client.get(format!("{}/head", self.base)), device)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn upload(&self, device: &DeviceCredential, body: &[u8]) {
        let response = self
            .auth(
                self.client.post(format!("{}/uploads/batch", self.base)),
                device,
            )
            .body(batch::encode(&[body]).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    async fn stage(
        &self,
        device: &DeviceCredential,
        head: RemoteHead,
        seq: u64,
        key: &str,
        body: &[u8],
    ) -> CommitIntent {
        let response = self
            .auth(
                self.client.post(format!("{}/staged-changes", self.base)),
                device,
            )
            .json(&changes(key, body))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let value: serde_json::Value = response.json().await.unwrap();
        CommitIntent {
            device_operation_seq: seq.into(),
            expected_head: head,
            changes_digest: value["changesDigest"].as_str().unwrap().into(),
            staged_changes_id: value["stagedChangesId"].as_str().unwrap().into(),
        }
    }
    async fn commit(
        &self,
        device: &DeviceCredential,
        intent: &CommitIntent,
    ) -> (StatusCode, Receipt) {
        let response = self
            .auth(self.client.post(format!("{}/commits", self.base)), device)
            .header("if-match", intent.expected_head.etag())
            .json(intent)
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }
}

#[tokio::test]
async fn tcp_vertical_slice_conditional_head_exact_bytes_receipt_and_revoke() {
    let s = Server::start().await;
    let head = s.head(&s.a).await;
    let response = s
        .auth(s.client.get(format!("{}/head", s.base)), &s.a)
        .header("if-none-match", head.etag())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(response.bytes().await.unwrap().is_empty());
    assert!(serde_json::to_vec(&head).unwrap().len() <= 1024);
    let body = br#"{ "opaque":1.0, "other":9007199254740993 }"#;
    s.upload(&s.a, body).await;
    assert_eq!(s.head(&s.a).await, head);
    let intent = s
        .stage(&s.a, head.clone(), 1, "character/synthetic", body)
        .await;
    let (status, receipt) = s.commit(&s.a, &intent).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(s.commit(&s.a, &intent).await.1, receipt);
    let response = s
        .auth(s.client.get(format!("{}/changes", s.base)), &s.b)
        .query(&[
            ("epoch", head.epoch.as_str()),
            ("afterSeq", "0"),
            ("afterOrdinal", "1024"),
            ("throughSeq", "1"),
            ("limit", "1"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let page: risunest_sync_server::store::ChangePage = response.json().await.unwrap();
    assert_eq!(page.through, receipt.head);
    assert_eq!(page.entries[0].change.key, "character/synthetic");
    let downloaded = s
        .auth(
            s.client.get(format!("{}/objects/{}", s.base, hash(body))),
            &s.b,
        )
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(downloaded.as_ref(), body);
    let response = s
        .auth(
            s.client
                .get(format!("{}/operations/{}", s.base, receipt.operation_id)),
            &s.b,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    s.store.revoke_device(&s.a.device_id).unwrap();
    let response = s
        .auth(s.client.get(format!("{}/head", s.base)), &s.a)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(s.head(&s.b).await, receipt.head);
}
#[tokio::test]
async fn two_tcp_clients_race_then_reconcile_with_new_operation() {
    let s = Server::start().await;
    let head = s.head(&s.a).await;
    tokio::join!(s.upload(&s.a, b"a"), s.upload(&s.b, b"b"));
    let ia = s.stage(&s.a, head.clone(), 1, "a", b"a").await;
    let ib = s.stage(&s.b, head, 1, "b", b"b").await;
    let (ra, rb) = tokio::join!(s.commit(&s.a, &ia), s.commit(&s.b, &ib));
    assert_eq!(
        [ra.0, rb.0]
            .iter()
            .filter(|&&v| v == StatusCode::OK)
            .count(),
        1
    );
    assert_eq!(
        [ra.0, rb.0]
            .iter()
            .filter(|&&v| v == StatusCode::PRECONDITION_FAILED)
            .count(),
        1
    );
    let (device, key, body) = if ra.0 == StatusCode::PRECONDITION_FAILED {
        (&s.a, "a", b"a")
    } else {
        (&s.b, "b", b"b")
    };
    let next = s.stage(device, s.head(device).await, 2, key, body).await;
    assert_eq!(
        s.commit(device, &next).await.1.status,
        TerminalStatus::Committed
    );
    assert_eq!(s.head(device).await.seq.as_str(), "2");
}
#[tokio::test]
async fn unauthorized_large_unfinished_body_is_rejected_before_reading_it() {
    let s = Server::start().await;
    let mut tcp = tokio::net::TcpStream::connect(s.base.strip_prefix("http://").unwrap())
        .await
        .unwrap();
    tcp.write_all(
        b"POST /uploads/batch HTTP/1.1\r\nHost: localhost\r\nContent-Length: 8388608\r\n\r\n",
    )
    .await
    .unwrap();
    let mut bytes = [0; 1024];
    let count = tokio::time::timeout(Duration::from_secs(2), tcp.read(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes[..count]).starts_with("HTTP/1.1 401"));
    let response = s
        .client
        .get(format!("{}/admin", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = s
        .client
        .get(format!("{}/head", s.base))
        .bearer_auth(&s.a.token)
        .header("x-risu-library", "different")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
#[tokio::test]
async fn stalled_upload_does_not_hold_library_writer_or_another_device_slot() {
    let s = Server::start().await;
    let mut tcp = tokio::net::TcpStream::connect(s.base.strip_prefix("http://").unwrap())
        .await
        .unwrap();
    let frame = batch::encode(&[b"partial synthetic bytes"]).unwrap();
    let headers=format!("POST /uploads/batch HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nX-Risu-Library: {}\r\nContent-Length: {}\r\n\r\n",s.a.token,s.a.library_id,frame.len());
    tcp.write_all(headers.as_bytes()).await.unwrap();
    tcp.write_all(&frame[..10]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        s.upload(&s.b, b"concurrent").await;
        let intent = s
            .stage(&s.b, s.head(&s.b).await, 1, "b", b"concurrent")
            .await;
        assert_eq!(s.commit(&s.b, &intent).await.0, StatusCode::OK);
    })
    .await
    .unwrap();
    // Disconnect A mid-frame; no verified object can be published for that frame.
    drop(tcp);
    assert!(s
        .store
        .object_size(&hash(b"partial synthetic bytes"))
        .unwrap()
        .is_none());
}
#[tokio::test]
async fn identity_range_and_batch_retries_use_verified_target_bytes() {
    let s = Server::start().await;
    s.upload(&s.a, b"0123456789").await;
    let digest = hash(b"0123456789");
    let url = format!("{}/objects/{digest}", s.base);
    for (range, expected) in [
        ("bytes=2-5", b"2345".as_slice()),
        ("bytes=-3", b"789"),
        ("bytes=8-", b"89"),
    ] {
        let response = s
            .auth(s.client.get(&url), &s.b)
            .header("range", range)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert!(response.headers().get("content-encoding").is_none());
        assert_eq!(response.bytes().await.unwrap().as_ref(), expected);
    }
    let response = s
        .auth(s.client.get(&url), &s.b)
        .header("range", "bytes=30-40")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()["content-range"], "bytes */10");
    let response = s
        .auth(s.client.get(&url), &s.b)
        .header("range", "bytes=2-5")
        .header("if-range", "\"wrong\"")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"0123456789");
    let response = s
        .auth(s.client.post(format!("{}/objects/missing", s.base)), &s.a)
        .json(&serde_json::json!([{"hash":digest,"size":"10"}]))
        .send()
        .await
        .unwrap();
    let missing: serde_json::Value = response.json().await.unwrap();
    assert_eq!(missing["missing"], serde_json::json!([]));
    let response = s
        .auth(s.client.post(format!("{}/objects/batch", s.base)), &s.b)
        .json(&[digest])
        .send()
        .await
        .unwrap();
    let bytes = response.bytes().await.unwrap();
    assert_eq!(batch::decode(&bytes).unwrap()[0].bytes, b"0123456789");
}
#[tokio::test]
async fn malformed_metadata_and_frame_fail_without_mutation() {
    let s = Server::start().await;
    let head = s.head(&s.a).await;
    let response = s
        .auth(s.client.post(format!("{}/staged-changes", s.base)), &s.a)
        .body(r#"{"changes":[],"changes":[],"readFences":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let mut oversized = tokio::net::TcpStream::connect(s.base.strip_prefix("http://").unwrap())
        .await
        .unwrap();
    let headers=format!("POST /uploads/batch HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nX-Risu-Library: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",s.a.token,s.a.library_id,batch::MAX_BATCH_BYTES+1);
    oversized.write_all(headers.as_bytes()).await.unwrap();
    let mut response = [0; 1024];
    let length = tokio::time::timeout(Duration::from_secs(2), oversized.read(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&response[..length]).starts_with("HTTP/1.1 413"));
    drop(oversized);
    let mut corrupt = batch::encode(&[b"good", b"bad"]).unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    let response = s
        .auth(s.client.post(format!("{}/uploads/batch", s.base)), &s.a)
        .body(corrupt)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(s.store.object_size(&hash(b"good")).unwrap().is_none());
    assert_eq!(s.head(&s.a).await, head);
}
