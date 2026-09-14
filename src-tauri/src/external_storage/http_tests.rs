use super::{
    contract::*,
    http::*,
    wire_fixture::{Reply, WireServer},
};
use std::{collections::BTreeMap, time::Duration};
use tokio::io::AsyncReadExt;
fn request(server: &WireServer, body: Option<Vec<u8>>) -> HttpRequest {
    let length = body.as_ref().map(|body| body.len() as u64);
    HttpRequest {
        method: if body.is_some() {
            reqwest::Method::PUT
        } else {
            reqwest::Method::GET
        },
        url: server.url.clone(),
        headers: BTreeMap::new(),
        body: body.map(|body| Box::pin(std::io::Cursor::new(body)) as _),
        content_length: length,
        operation: ProviderOperation::ReplaceHead,
        costs: vec![],
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
#[test]
fn wire_streams_exact_bytes_and_exposes_status_headers_without_redirects_or_retries() {
    runtime().block_on(async {
        let data = vec![42; 1024 * 1024];
        let server = WireServer::start(vec![
            Reply::Http {
                status: 200,
                headers: vec![
                    ("ETag".into(), "\"version-1\"".into()),
                    ("Content-Encoding".into(), "gzip".into()),
                ],
                body: data.clone(),
            },
            Reply::Http {
                status: 307,
                headers: vec![("Location".into(), "http://127.0.0.1:1/do-not-follow".into())],
                body: vec![],
            },
            Reply::Http {
                status: 429,
                headers: vec![("Retry-After".into(), "60".into())],
                body: vec![],
            },
            Reply::Lost,
        ]);
        let transport = NativeHttpTransport::for_loopback_tests();
        let cancel = Cancellation::default();
        let mut response = transport
            .send(request(&server, Some(data.clone())), &cancel)
            .await
            .unwrap();
        assert_eq!(response.headers["etag"], "\"version-1\"");
        let mut output = Vec::new();
        response.body.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, data);
        let response = transport
            .send(request(&server, None), &cancel)
            .await
            .unwrap();
        assert_eq!(response.status, 307);
        let response = transport
            .send(request(&server, None), &cancel)
            .await
            .unwrap();
        assert_eq!(response.status, 429);
        assert_eq!(response.headers["retry-after"], "60");
        assert!(transport
            .send(request(&server, Some(vec![9; 10])), &cancel)
            .await
            .is_err());
        let records = server.requests.lock().unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].body, data);
        assert!(records[0].headers.starts_with("PUT /synthetic HTTP/1.1"));
    });
}
#[test]
fn cancellation_wakes_header_and_body_waits_and_all_listeners() {
    runtime().block_on(async {
        let shared = Cancellation::default();
        let listeners = futures::future::join(shared.cancelled(), shared.cancelled());
        let trigger = async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            shared.cancel();
        };
        tokio::time::timeout(
            Duration::from_millis(200),
            futures::future::join(listeners, trigger),
        )
        .await
        .unwrap();
        for reply in [Reply::DelayedHeaders, Reply::DelayedBody] {
            let server = WireServer::start(vec![reply]);
            let transport = NativeHttpTransport::for_loopback_tests();
            let cancel = Cancellation::default();
            let perform = async {
                match transport.send(request(&server, None), &cancel).await {
                    Ok(mut response) => {
                        let mut byte = [0];
                        assert_eq!(
                            response.body.read(&mut byte).await.unwrap_err().kind(),
                            std::io::ErrorKind::Other
                        );
                        assert_eq!(
                            response.body.read(&mut byte).await.unwrap_err().kind(),
                            std::io::ErrorKind::Other
                        );
                    }
                    Err(error) => assert_eq!(error.kind, ErrorKind::Cancelled),
                }
            };
            let trigger = async {
                while server.requests.lock().unwrap().is_empty() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                cancel.cancel();
            };
            tokio::time::timeout(
                Duration::from_millis(200),
                futures::future::join(perform, trigger),
            )
            .await
            .unwrap();
            let listeners = futures::future::join(cancel.cancelled(), cancel.cancelled());
            tokio::time::timeout(Duration::from_millis(50), listeners)
                .await
                .unwrap();
        }
    });
}
#[test]
fn durable_quota_denies_wire_dispatch_after_reopen() {
    use super::{durable_quota::DurableBudget, quota::Bucket};
    struct Time;
    impl Clock for Time {
        fn now_ms(&self) -> u64 {
            10
        }
    }
    runtime().block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quota.sqlite");
        let budget = DurableBudget::open(&path).unwrap();
        budget
            .configure(
                "account",
                "download",
                Bucket {
                    limit: 1,
                    used: 0,
                    reset: QuotaReset::Unknown,
                    blocked_until_ms: None,
                    last_reset_ms: None,
                },
            )
            .unwrap();
        let server = WireServer::start(vec![Reply::Lost]);
        let transport = NativeHttpTransport::for_loopback_tests();
        let cancel = Cancellation::default();
        let make = || {
            let mut request = request(&server, Some(vec![1]));
            request.costs.push(RequestCost {
                bucket: "download".into(),
                shared_account: "account".into(),
                units: 1,
                reset: QuotaReset::Unknown,
            });
            request
        };
        assert_eq!(
            send(&transport, &budget, &Time, make(), &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Transient
        );
        drop(budget);
        let reopened = DurableBudget::open(&path).unwrap();
        assert_eq!(
            send(&transport, &reopened, &Time, make(), &cancel)
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::DailyQuotaExhausted
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    });
}
