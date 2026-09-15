use super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
};

fn response(stream: &mut impl Write, body: &str, status: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    stream.flush().unwrap();
}
fn request(stream: &mut impl Read) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 8192);
    }
    String::from_utf8(bytes).unwrap()
}
fn tls_identity(device: &str) -> (String, reqwest::Certificate, std::thread::JoinHandle<()>) {
    tls_identity_requests(device, 1)
}

fn tls_identity_requests(
    device: &str,
    requests: usize,
) -> (String, reqwest::Certificate, std::thread::JoinHandle<()>) {
    let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let certificate = reqwest::Certificate::from_pem(key.cert.pem().as_bytes()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![key.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(key.signing_key.serialize_der()).into(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let device = device.to_owned();
    let task = std::thread::spawn(move || {
        let config = Arc::new(config);
        for _ in 0..requests {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(config.clone()).unwrap(),
                socket,
            );
            let input = request(&mut stream);
            assert!(input.starts_with("GET /session ") || input.starts_with("GET /head "));
            assert!(input.to_lowercase().contains("authorization: bearer "));
            let head = serde_json::json!({"libraryId":"library","epoch":"epoch","seq":"0","headId":"a".repeat(64),"minRetainedSeq":"0"});
            let body = if input.starts_with("GET /session ") {
                serde_json::json!({"head":head,"deviceId":device,"operationWatermark":"0","operationPending":false}).to_string()
            } else {
                head.to_string()
            };
            response(&mut stream, &body, "200 OK");
        }
    });
    (endpoint, certificate, task)
}

#[test]
fn mid_cycle_failure_rediscovers_and_verifies_the_changed_endpoint() {
    let (old_endpoint, old_certificate, old_server) = tls_identity("device");
    let (new_endpoint, new_certificate, new_server) = tls_identity_requests("device", 2);
    let mut directory = directory();
    let envelope =
        risunest_sync_connect::seal_endpoint(&directory.uuid, &directory.key, &new_endpoint)
            .unwrap();
    let (registry, registry_task) = directory_server(envelope);
    directory.base_url = registry;
    let config = ServerConfig {
        endpoint: old_endpoint,
        library_id: "library".into(),
        device_id: "device".into(),
        token: "a".repeat(64),
        directory: Some(directory),
    };
    let mut client = ServerClient::new(config).unwrap();
    client.http = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .add_root_certificate(old_certificate)
        .add_root_certificate(new_certificate)
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert!(client.resolve_identity(false).is_ok());
    old_server.join().unwrap();
    let head = client.head().unwrap();
    assert_eq!(head.library_id, "library");
    assert_eq!(client.config().endpoint, new_endpoint);
    registry_task.join().unwrap();
    new_server.join().unwrap();
}

#[test]
fn authenticated_identity_rejection_does_not_try_registry_fallback() {
    let identity = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", identity.server_addr());
    let identity_task = std::thread::spawn(move || {
        let request = identity.recv().unwrap();
        assert_eq!(request.url(), "/session");
        request
            .respond(
                tiny_http::Response::from_string(r#"{"error":"unauthorized"}"#)
                    .with_status_code(401),
            )
            .unwrap();
    });
    let registry = TcpListener::bind("127.0.0.1:0").unwrap();
    registry.set_nonblocking(true).unwrap();
    let mut directory = directory();
    directory.base_url = format!("http://{}", registry.local_addr().unwrap());
    let client = ServerClient::new(ServerConfig {
        endpoint,
        library_id: "library".into(),
        device_id: "device".into(),
        token: "a".repeat(64),
        directory: Some(directory),
    })
    .unwrap();
    let error = client.resolve_identity(false).unwrap_err();
    assert_eq!(error.status, 401);
    assert!(matches!(
        registry.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    identity_task.join().unwrap();
}
fn directory_server(body: String) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let input = request(&mut stream).to_lowercase();
        assert!(input.starts_with("get /endpoints/"));
        assert!(!input.contains("authorization"));
        assert!(!input.contains("x-risu-library"));
        response(&mut stream, &body, "200 OK");
    });
    (endpoint, task)
}
fn directory() -> risunest_sync_connect::Directory {
    risunest_sync_connect::Directory {
        base_url: "https://registry.example".into(),
        uuid: "12345678-1234-4234-9234-123456789abc".into(),
        key: "A".repeat(43),
    }
}
#[test]
fn native_directory_recovery_verifies_identity_before_changing_endpoint() {
    for device in ["device", "wrong-device"] {
        let (endpoint, certificate, server) = tls_identity(device);
        let mut directory = directory();
        let envelope =
            risunest_sync_connect::seal_endpoint(&directory.uuid, &directory.key, &endpoint)
                .unwrap();
        let (registry, registry_task) = directory_server(envelope);
        directory.base_url = registry;
        let config = ServerConfig {
            endpoint: "http://127.0.0.1:1".into(),
            library_id: "library".into(),
            device_id: "device".into(),
            token: "a".repeat(64),
            directory: Some(directory),
        };
        let mut client = ServerClient::new(config).unwrap();
        client.http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .add_root_certificate(certificate)
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let result = client.resolve_identity(false);
        if device == "device" {
            assert!(result.is_ok(), "{}", result.unwrap_err().code);
            assert_eq!(client.config().endpoint, endpoint);
        } else {
            assert_eq!(result.unwrap_err().code, "device-identity-mismatch");
            assert_eq!(client.config().endpoint, "http://127.0.0.1:1");
        }
        registry_task.join().unwrap();
        server.join().unwrap();
    }
}
#[test]
fn native_directory_rejects_tampered_and_oversized_envelopes_without_mutation() {
    for body in [
        "A".repeat(100),
        "A".repeat(risunest_sync_connect::MAX_ENVELOPE_TEXT + 1),
    ] {
        let (registry, task) = directory_server(body);
        let mut directory = directory();
        directory.base_url = registry;
        let config = ServerConfig {
            endpoint: "http://127.0.0.1:1".into(),
            library_id: "library".into(),
            device_id: "device".into(),
            token: "a".repeat(64),
            directory: Some(directory),
        };
        let client = ServerClient::new(config).unwrap();
        assert!(client.resolve_identity(false).is_err());
        assert_eq!(client.config().endpoint, "http://127.0.0.1:1");
        task.join().unwrap();
    }
}

#[test]
fn synthetic_https_fixture_accepts_the_native_client() {
    let (endpoint, certificate, server) = tls_identity("device");
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .add_root_certificate(certificate)
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let reply = client
        .get(format!("{endpoint}/session"))
        .bearer_auth("a".repeat(64))
        .send();
    assert!(reply.is_ok(), "synthetic fixture TLS: {:?}", reply.err());
    server.join().unwrap();
}

#[test]
fn native_verified_cached_endpoint_does_not_read_directory() {
    let (endpoint, certificate, server) = tls_identity("device");
    let mut directory = directory();
    directory.base_url = "http://127.0.0.1:1".into();
    let config = ServerConfig {
        endpoint: endpoint.clone(),
        library_id: "library".into(),
        device_id: "device".into(),
        token: "a".repeat(64),
        directory: Some(directory),
    };
    let mut client = ServerClient::new(config).unwrap();
    client.http = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .add_root_certificate(certificate)
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert!(client.resolve_identity(false).is_ok());
    assert_eq!(client.config().endpoint, endpoint);
    server.join().unwrap();
}
