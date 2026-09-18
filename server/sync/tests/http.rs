mod common;
use common::changes;
use reqwest::{Client, RequestBuilder, StatusCode};
use risunest_sync_server::{
    http,
    store::{DeviceCredential, Store},
};
use risunest_sync_wire::{
    hash,
    transfer::{self, Frame},
    CommitIntent, Receipt, RemoteHead, TerminalStatus,
};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn full_frames(objects: &[&[u8]]) -> Vec<u8> {
    let frames = objects
        .iter()
        .map(|bytes| Frame::Full(bytes.to_vec()))
        .collect::<Vec<_>>();
    transfer::encode(&frames).expect("synthetic full frames must fit")
}

struct Server {
    base: String,
    store: Arc<Store>,
    client: Client,
    a: DeviceCredential,
    b: DeviceCredential,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

#[tokio::test]
async fn scoped_media_urls_serve_get_head_and_range_without_device_credentials() {
    use risunest_sync_connect::media::{MediaAccess, MediaObject, MediaRequest, MediaSigner};
    let server = Server::start().await;
    let bytes = b"0123456789";
    server.upload(&server.a, bytes).await;
    let head = server.head(&server.a).await;
    let object = MediaObject {
        hash: hash(bytes),
        size: 10.into(),
        mime: "image/custom".into(),
    };
    let refresh = MediaSigner::new(&[3; 32])
        .unwrap()
        .refresh_url("http://127.0.0.1:12345", &object)
        .unwrap();
    let body = serde_json::json!({"epoch":head.epoch,"requests":[MediaRequest {object:object.clone(),refresh_url:refresh}]});
    assert_eq!(
        server
            .client
            .post(format!("{}/media/access", server.base))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let grants: Vec<MediaAccess> = server
        .auth(
            server.client.post(format!("{}/media/access", server.base)),
            &server.a,
        )
        .json(&body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(grants.len(), 1);
    let url = format!("{}/media/{}", server.base, grants[0].token);
    let head = server.client.head(&url).send().await.unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["content-length"], "10");
    assert_eq!(head.headers()["content-type"], "image/custom");
    assert!(head.bytes().await.unwrap().is_empty());
    let get = server
        .client
        .get(&url)
        .header("origin", "http://tauri.localhost")
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(get.headers()["access-control-allow-origin"], "*");
    assert_eq!(get.bytes().await.unwrap().as_ref(), bytes);
    let range = server
        .client
        .get(&url)
        .header("range", "bytes=3-6")
        .send()
        .await
        .unwrap();
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.headers()["content-range"], "bytes 3-6/10");
    assert_eq!(range.bytes().await.unwrap().as_ref(), b"3456");
    assert_eq!(
        server.client.post(&url).send().await.unwrap().status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    server.store.revoke_device(&server.a.device_id).unwrap();
    assert_eq!(
        server.client.get(&url).send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn retention_http_is_authenticated_and_releases_only_its_exact_id() {
    let server = Server::start().await;
    let bytes = b"synthetic custody HTTP";
    server.upload(&server.a, bytes).await;
    let head = server.head(&server.a).await;
    let body = serde_json::json!({"epoch":head.epoch,"objects":[{"hash":hash(bytes),"size":bytes.len().to_string()}]});
    let url = format!("{}/objects/retention", server.base);
    assert_eq!(
        server
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let retained: serde_json::Value = server
        .auth(server.client.post(&url), &server.a)
        .json(&body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let release = serde_json::json!({"epoch":head.epoch,"objects":[{"deviceId":server.a.device_id,"hash":hash(bytes),"retentionId":retained[0]["retentionId"]}]});
    let response = server
        .auth(server.client.post(format!("{}/release", url)), &server.a)
        .json(&release)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let listed: serde_json::Value = server
        .auth(server.client.get(&url), &server.a)
        .query(&[("epoch", head.epoch)])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["objects"], serde_json::json!([]));
}

#[tokio::test]
async fn bounded_bulk_buffers_leave_head_available_and_release_after_completion() {
    let server = Server::start().await;
    let third = server.store.add_device().unwrap();
    let address = server.base.strip_prefix("http://").unwrap();
    let frame = risunest_sync_wire::transfer::encode(&[]).unwrap();
    let mut sockets = Vec::new();
    for credential in [&server.a, &server.a, &server.b, &server.b] {
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(format!("POST /uploads/frames HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nX-Risu-Library: {}\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",credential.token,credential.library_id,frame.len()).as_bytes()).await.unwrap();
        let mut interim = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !interim.ends_with(b"\r\n\r\n") {
                interim.push(socket.read_u8().await.unwrap());
            }
        })
        .await
        .unwrap();
        assert!(interim.starts_with(b"HTTP/1.1 100 Continue"));
        sockets.push(socket);
    }
    let rejected = server
        .auth(
            server
                .client
                .post(format!("{}/uploads/frames", server.base)),
            &third,
        )
        .body(frame.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        rejected.json::<serde_json::Value>().await.unwrap()["error"],
        "transfer-memory-busy"
    );
    assert_eq!(
        server
            .auth(server.client.get(format!("{}/head", server.base)), &third)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let mut first = sockets.remove(0);
    first.write_all(&frame).await.unwrap();
    let mut completed = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), first.read_to_end(&mut completed))
        .await
        .unwrap()
        .unwrap();
    assert!(completed.starts_with(b"HTTP/1.1 204 No Content"));
    assert_eq!(
        server
            .auth(
                server
                    .client
                    .post(format!("{}/uploads/frames", server.base)),
                &third
            )
            .body(frame)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    drop(sockets);
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn session_identity_and_previous_device_status_are_authenticated_and_revocation_aware() {
    let server = Server::start().await;
    let url = format!("{}/session", server.base);
    assert_eq!(
        server.client.get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let session: serde_json::Value = server
        .auth(server.client.get(&url), &server.a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        session,
        serde_json::json!({"head":server.store.head().unwrap(),"deviceId":server.a.device_id,"operationWatermark":"0","operationPending":false})
    );
    let scope: serde_json::Value = server
        .auth(
            server
                .client
                .get(format!("{}/scopes?scope=plugin-storage", server.base)),
            &server.a,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (version, clear) = server.store.scope_state("plugin-storage").unwrap();
    assert_eq!(
        scope,
        serde_json::json!({"head":server.store.head().unwrap(),"scope":"plugin-storage","version":version,"clearVersion":clear})
    );
    let url = format!("{}/devices/{}/status", server.base, server.a.device_id);
    let before: serde_json::Value = server
        .auth(server.client.get(&url), &server.b)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["active"], true);
    server.store.revoke_device(&server.a.device_id).unwrap();
    let after: serde_json::Value = server
        .auth(server.client.get(&url), &server.b)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["active"], false);
    assert_eq!(
        server
            .auth(server.client.get(&url), &server.a)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
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
                self.client.post(format!("{}/uploads/frames", self.base)),
                device,
            )
            .body(full_frames(&[body]))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    async fn download_full(&self, device: &DeviceCredential, expected: &[u8]) -> bool {
        let digest = hash(expected);
        let response = self
            .auth(
                self.client.post(format!("{}/objects/transfer", self.base)),
                device,
            )
            .json(&serde_json::json!([{"target":digest,"bases":[]}]))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let encoded = response.bytes().await.unwrap();
        let mut frames = transfer::decode(&encoded).unwrap();
        assert_eq!(frames.len(), 1);
        let (bytes, fallback) = match frames.pop().unwrap() {
            Frame::Full(bytes) => (bytes, false),
            Frame::FullRequired { hash, size } => {
                assert_eq!(hash, digest);
                assert_eq!(size, expected.len() as u64);
                let response = self
                    .auth(
                        self.client.get(format!("{}/objects/{hash}", self.base)),
                        device,
                    )
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                (response.bytes().await.unwrap().to_vec(), true)
            }
            Frame::Delta(_) => panic!("empty bases must not return a delta"),
        };
        assert_eq!(bytes.len(), expected.len());
        assert_eq!(hash(&bytes), digest);
        assert_eq!(bytes, expected);
        fallback
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
            ("domains", "library"),
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
        b"POST /uploads/frames HTTP/1.1\r\nHost: localhost\r\nContent-Length: 8388608\r\nExpect: 100-continue\r\n\r\n",
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
    let frame = full_frames(&[b"partial synthetic bytes"]);
    let headers=format!("POST /uploads/frames HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nX-Risu-Library: {}\r\nContent-Length: {}\r\n\r\n",s.a.token,s.a.library_id,frame.len());
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
    for _ in 0..2 {
        s.upload(&s.a, b"partial synthetic bytes").await;
    }
    assert_eq!(
        s.store.get_object(&hash(b"partial synthetic bytes")).unwrap(),
        b"partial synthetic bytes"
    );
}
#[tokio::test]
async fn identity_range_and_transfer_retries_use_verified_target_bytes() {
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
        .auth(s.client.post(format!("{}/objects/missing", s.base)), &s.a)
        .json(&serde_json::json!([
            {"hash":hash(b"missing-a"),"size":"9"},
            {"hash":digest,"size":"10"},
            {"hash":hash(b"missing-b"),"size":"9"}
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!({"missing":[hash(b"missing-a"),hash(b"missing-b")]})
    );
    assert!(!s.download_full(&s.b, b"0123456789").await);
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
    let headers=format!("POST /uploads/frames HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nX-Risu-Library: {}\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",s.a.token,s.a.library_id,transfer::MAX_BATCH_BYTES+1);
    oversized.write_all(headers.as_bytes()).await.unwrap();
    let mut response = [0; 1024];
    let length = tokio::time::timeout(Duration::from_secs(2), oversized.read(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&response[..length]).starts_with("HTTP/1.1 413"));
    drop(oversized);
    let mut corrupt = full_frames(&[b"good", b"bad"]);
    let second_hash = 8 + (45 + b"good".len()) + 5;
    corrupt[second_hash] ^= 1;
    let response = s
        .auth(s.client.post(format!("{}/uploads/frames", s.base)), &s.a)
        .body(corrupt)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(s.store.object_size(&hash(b"good")).unwrap().is_none());
    assert!(s.store.object_size(&hash(b"bad")).unwrap().is_none());
    assert_eq!(s.head(&s.a).await, head);
}

#[tokio::test]
async fn removed_batch_endpoints_have_no_post_alias() {
    let s = Server::start().await;
    let head = s.head(&s.a).await;
    let object = b"synthetic obsolete upload";
    for (path, body) in [
        ("/uploads/batch", full_frames(&[object])),
        ("/objects/batch", serde_json::to_vec(&[hash(object)]).unwrap()),
    ] {
        let response = s
            .auth(s.client.post(format!("{}{path}", s.base)), &s.a)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
    assert!(s.store.object_size(&hash(object)).unwrap().is_none());
    assert_eq!(s.head(&s.a).await, head);
}

#[tokio::test]
async fn truncated_frames_never_publish_objects_or_allow_a_commit() {
    let s = Server::start().await;
    let head = s.head(&s.a).await;
    let first = b"synthetic complete first object";
    let second = b"synthetic truncated second object";
    let encoded = full_frames(&[first, second]);
    for end in 0..encoded.len() {
        let response = s
            .auth(s.client.post(format!("{}/uploads/frames", s.base)), &s.a)
            .body(encoded[..end].to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "prefix {end}");
        assert!(s.store.object_size(&hash(first)).unwrap().is_none());
        assert!(s.store.object_size(&hash(second)).unwrap().is_none());
    }
    let invalid_upload = transfer::encode(&[
        Frame::Full(first.to_vec()),
        Frame::FullRequired { hash: hash(second), size: second.len() as u64 },
    ]).unwrap();
    let response = s
        .auth(s.client.post(format!("{}/uploads/frames", s.base)), &s.a)
        .body(invalid_upload)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.json::<serde_json::Value>().await.unwrap()["error"], "invalid-upload-frame");
    assert!(s.store.object_size(&hash(first)).unwrap().is_none());
    let intent = s.stage(&s.a, head.clone(), 1, "incomplete", first).await;
    let response = s
        .auth(s.client.post(format!("{}/commits", s.base)), &s.a)
        .header("if-match", head.etag())
        .json(&intent)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(response.json::<serde_json::Value>().await.unwrap()["error"], "missing-dependency");
    assert_eq!(s.head(&s.a).await, head);
}

#[tokio::test]
async fn streamed_frame_body_limit_rejects_before_publishing() {
    let s = Server::start().await;
    let head = s.head(&s.a).await;
    let object = b"synthetic streamed prefix";
    let prefix = full_frames(&[object]);
    let padding = vec![0; transfer::MAX_BATCH_BYTES + 1 - prefix.len()];
    let body = reqwest::Body::wrap_stream(futures_util::stream::iter([
        Ok::<_, std::io::Error>(prefix),
        Ok(padding),
    ]));
    let request = s
        .auth(s.client.post(format!("{}/uploads/frames", s.base)), &s.a)
        .body(body)
        .build()
        .unwrap();
    assert!(!request.headers().contains_key("content-length"));
    let response = s.client.execute(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(s.store.object_size(&hash(object)).unwrap().is_none());
    assert_eq!(s.head(&s.a).await, head);
}

#[tokio::test]
async fn transfer_and_full_required_get_reject_other_libraries_and_revoked_devices() {
    let s = Server::start().await;
    let foreign = Server::start().await;
    let bytes = vec![b'x'; transfer::PREFERRED_BATCH_BYTES];
    s.upload(&s.a, &bytes).await;
    assert!(s.download_full(&s.b, &bytes).await);
    let digest = hash(&bytes);
    for credential in [None, Some(&foreign.a)] {
        let requests = [
            s.client.post(format!("{}/objects/transfer", s.base))
                .json(&serde_json::json!([{"target":digest,"bases":[]}])),
            s.client.get(format!("{}/objects/{digest}", s.base)),
        ];
        for request in requests {
            let request = match credential {
                Some(credential) => s.auth(request, credential),
                None => request,
            };
            assert_eq!(request.send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
        }
    }
    let denied = b"synthetic foreign upload";
    let response = s.client.post(format!("{}/uploads/frames", s.base))
        .bearer_auth(&s.a.token)
        .header("x-risu-library", &foreign.a.library_id)
        .body(full_frames(&[denied]))
        .send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(s.store.object_size(&hash(denied)).unwrap().is_none());
    s.store.revoke_device(&s.b.device_id).unwrap();
    for request in [
        s.client.post(format!("{}/objects/transfer", s.base))
            .json(&serde_json::json!([{"target":digest,"bases":[]}])),
        s.client.get(format!("{}/objects/{digest}", s.base)),
    ] {
        assert_eq!(s.auth(request, &s.b).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chunk_upload_delta_download_checkpoint_and_durable_job_over_tcp() {
    use risunest_sync_wire::{
        delta,
        transfer::{self, Frame},
    };
    let s = Server::start().await;
    let base = vec![b'a'; 10 * 1024 * 1024];
    let mut target = base.clone();
    target.splice(
        5 * 1024 * 1024..5 * 1024 * 1024,
        b"new-content".iter().copied(),
    );
    let response = s
        .auth(s.client.post(format!("{}/uploads", s.base)), &s.a)
        .json(&serde_json::json!({"hash":hash(&base),"size":base.len().to_string()}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let upload = response.json::<serde_json::Value>().await.unwrap()["uploadId"]
        .as_str()
        .unwrap()
        .to_owned();
    for (index, chunk) in base
        .chunks(risunest_sync_server::store::UPLOAD_CHUNK_BYTES as usize)
        .enumerate()
    {
        let response = s
            .auth(
                s.client
                    .put(format!("{}/uploads/{upload}/chunks/{index}", s.base)),
                &s.a,
            )
            .header("x-content-sha256", hash(chunk))
            .body(chunk.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    s.auth(
        s.client
            .post(format!("{}/uploads/{upload}/complete", s.base)),
        &s.a,
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    let patch =
        transfer::encode(&[Frame::Delta(delta::create(&[&base], &target).unwrap())]).unwrap();
    assert!(patch.len() < 1024);
    s.auth(s.client.post(format!("{}/uploads/frames", s.base)), &s.a)
        .body(patch)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let response = s
        .auth(s.client.post(format!("{}/objects/transfer", s.base)), &s.b)
        .json(&serde_json::json!([{"target":hash(&target),"bases":[hash(&base)]}]))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let received = response.bytes().await.unwrap();
    assert!(received.len() < 1024);
    let mut frames = transfer::decode(&received).unwrap();
    match frames.remove(0) {
        Frame::Delta(recipe) => assert_eq!(recipe.apply(&[&base]).unwrap(), target),
        _ => panic!("warm download must use delta"),
    }
    assert!(s.download_full(&s.b, &target).await);
    let response = s
        .auth(
            s.client
                .get(format!("{}/objects/{}", s.base, hash(&target))),
            &s.b,
        )
        .header("range", "bytes=5242880-5242890")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(&response.bytes().await.unwrap()[..], b"new-content");
    let head = s.head(&s.a).await;
    let response = s
        .auth(
            s.client.post(format!("{}/staged-changes/start", s.base)),
            &s.a,
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let id = response.json::<serde_json::Value>().await.unwrap()["stagedChangesId"]
        .as_str()
        .unwrap()
        .to_owned();
    for index in 0..2 {
        let page = changes(&format!("key-{index}"), &target);
        s.auth(
            s.client
                .put(format!("{}/staged-changes/{id}/pages/{index}", s.base)),
            &s.a,
        )
        .json(&page)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    }
    let sealed = s
        .auth(
            s.client
                .post(format!("{}/staged-changes/{id}/seal", s.base)),
            &s.a,
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let intent = CommitIntent {
        device_operation_seq: 1.into(),
        expected_head: head.clone(),
        staged_changes_id: id,
        changes_digest: sealed["changesDigest"].as_str().unwrap().into(),
    };
    let response = s
        .auth(s.client.post(format!("{}/commits", s.base)), &s.a)
        .header("if-match", head.etag())
        .json(&intent)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let operation = response.json::<serde_json::Value>().await.unwrap()["operationId"]
        .as_str()
        .unwrap()
        .to_owned();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = s
            .auth(
                s.client.get(format!("{}/operations/{operation}", s.base)),
                &s.a,
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        if status["status"] == "committed" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(s.head(&s.a).await.seq.as_str(), "1");
    let checkpoint = s
        .auth(s.client.post(format!("{}/checkpoints", s.base)), &s.b)
        .json(&serde_json::json!({"domains":["library"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let id = checkpoint["checkpointId"].as_str().unwrap();
    let page = s
        .auth(
            s.client.get(format!("{}/checkpoints/{id}?limit=1", s.base)),
            &s.b,
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(page["records"].as_array().unwrap().len(), 1);
    assert_eq!(
        page["checkpoint"]["domains"],
        serde_json::json!(["library"])
    );
    assert_eq!(page["records"][0]["domain"], "library");
    assert_eq!(page["next"]["domain"], "library");
    assert!(page["next"]["key"].is_string());
}
