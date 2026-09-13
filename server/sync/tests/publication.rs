use axum::{extract::State, routing::post, Router};
use risunest_sync_server::{connection::ConnectionOptions, publication::Publisher, store::Store};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn posts_only_ciphertext_and_keeps_uncertain_request_for_retry() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let app = Router::new()
        .route(
            "/endpoints/{uuid}",
            post(
                |State(seen): State<Arc<Mutex<Vec<String>>>>, body: String| async move {
                    let mut seen = seen.lock().unwrap();
                    seen.push(body);
                    if seen.len() == 1 {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        axum::http::StatusCode::NO_CONTENT
                    }
                },
            ),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    store
        .configure_connection(ConnectionOptions {
            endpoint: Some("https://sync.example/library".into()),
            cloudflared: None,
            registry_url: Some(url),
        })
        .unwrap();
    let planned = store.plan_publication().unwrap().unwrap();
    let publisher = Publisher::new().unwrap();
    assert!(publisher.publish_once(&store).await.is_err());
    assert_eq!(
        store.plan_publication().unwrap().unwrap().envelope,
        planned.envelope
    );
    assert!(publisher.publish_once(&store).await.unwrap());
    assert!(!publisher.publish_once(&store).await.unwrap());
    let values = seen.lock().unwrap();
    assert_eq!(values.len(), 2);
    assert_eq!(values[0], values[1]);
    assert!(!values[0].contains("sync.example"));
    assert!(!values[0].contains(&planned.directory.key));
    assert_eq!(
        risunest_sync_connect::open_endpoint(
            &planned.directory.uuid,
            &planned.directory.key,
            &values[0]
        )
        .unwrap(),
        "https://sync.example/library"
    );
    server.abort();
}

#[tokio::test]
async fn publication_redirect_is_not_followed_or_confirmed() {
    let app = Router::new()
        .route(
            "/endpoints/{uuid}",
            post(|| async { axum::response::Redirect::temporary("/must-not-post") }),
        )
        .route(
            "/must-not-post",
            post(|| async {
                panic!("redirect followed");
                #[allow(unreachable_code)]
                ""
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    store
        .configure_connection(ConnectionOptions {
            endpoint: Some("https://sync.example".into()),
            cloudflared: None,
            registry_url: Some(url),
        })
        .unwrap();
    assert!(Publisher::new()
        .unwrap()
        .publish_once(&store)
        .await
        .is_err());
    assert_eq!(store.connection_status().unwrap().publication, "pending");
    server.abort();
}

#[tokio::test]
async fn stalled_registry_does_not_block_the_sync_listener_or_shutdown() {
    use risunest_sync_server::{http, runtime::ConnectionRuntime};
    let root = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::init(root.path()).unwrap());
    let device = store.add_device().unwrap();
    let registry_hit = std::sync::Arc::new(tokio::sync::Notify::new());
    let entered = registry_hit.clone();
    let registry_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry_url = format!("http://{}", registry_listener.local_addr().unwrap());
    let registry = tokio::spawn(async move {
        axum::serve(
            registry_listener,
            axum::Router::new().route(
                "/endpoints/{uuid}",
                axum::routing::post(move || {
                    let entered = entered.clone();
                    async move {
                        entered.notify_one();
                        std::future::pending::<axum::http::StatusCode>().await
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    store
        .configure_connection(ConnectionOptions {
            endpoint: Some("https://sync.example".into()),
            cloudflared: None,
            registry_url: Some(registry_url),
        })
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let runtime = ConnectionRuntime::start(store.clone(), address).unwrap();
    let sync =
        tokio::spawn(async move { axum::serve(listener, http::router(store)).await.unwrap() });
    tokio::time::timeout(std::time::Duration::from_secs(2), registry_hit.notified())
        .await
        .unwrap();
    let response = reqwest::Client::new()
        .get(format!("http://{address}/session"))
        .header("x-risu-library", device.library_id)
        .bearer_auth(device.token)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    tokio::time::timeout(std::time::Duration::from_secs(2), runtime.shutdown())
        .await
        .unwrap();
    sync.abort();
    registry.abort();
}
