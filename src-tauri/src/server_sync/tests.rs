use super::{
    cache::Cache,
    client::{ServerClient, ServerConfig},
    transfer::Transfer,
};
use risunest_sync_server::{http, store::Store};
use std::sync::Arc;
mod network;

#[test]
fn native_client_roundtrips_large_objects_and_exact_delta_against_daemon() {
    let server_dir = tempfile::tempdir().unwrap();
    let server = Arc::new(Store::init(server_dir.path()).unwrap());
    let credential = server.add_device().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server_clone = server.clone();
    let task = runtime.spawn(async move {
        axum::serve(listener, http::router(server_clone))
            .await
            .unwrap();
    });
    let mut client = ServerClient::new(ServerConfig {
        endpoint,
        library_id: credential.library_id.clone(),
        device_id: credential.device_id.clone(),
        token: credential.token.clone(),
    })
    .unwrap();
    let verified = Arc::new(std::sync::atomic::AtomicU64::new(0));
    client.verified_bytes = Some(verified.clone());
    assert_eq!(client.head().unwrap(), server.head().unwrap());
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let first = Cache::open(first_dir.path()).unwrap();
    let second = Cache::open(second_dir.path()).unwrap();
    let base = (0..10 * 1024 * 1024)
        .map(|n| ((n * 37 + n / 127) % 251) as u8)
        .collect::<Vec<_>>();
    let base_hash = first.put(&base).unwrap();
    let mut changed = base.clone();
    changed.splice(
        5 * 1024 * 1024..5 * 1024 * 1024,
        b"synthetic insertion".iter().copied(),
    );
    let changed_hash = first.put(&changed).unwrap();
    {
        let upload = Transfer::new(&client, &first).unwrap();
        upload
            .upload(std::slice::from_ref(&base_hash), &[])
            .unwrap();
        upload
            .upload(
                std::slice::from_ref(&changed_hash),
                std::slice::from_ref(&base_hash),
            )
            .unwrap();
    }
    let download = Transfer::new(&client, &second).unwrap();
    assert_eq!(
        verified.load(std::sync::atomic::Ordering::Relaxed),
        (base.len() + changed.len()) as u64
    );
    download
        .download(std::slice::from_ref(&base_hash), &[])
        .unwrap();
    download
        .download(
            std::slice::from_ref(&changed_hash),
            std::slice::from_ref(&base_hash),
        )
        .unwrap();
    assert_eq!(second.read(&base_hash, 16 * 1024 * 1024).unwrap(), base);
    assert_eq!(
        second.read(&changed_hash, 16 * 1024 * 1024).unwrap(),
        changed
    );
    assert_eq!(server.get_object(&changed_hash).unwrap(), changed);
    let completed = 2 * (base.len() + changed.len()) as u64;
    assert_eq!(
        verified.load(std::sync::atomic::Ordering::Relaxed),
        completed
    );
    // Cache and transfer database reopen without retransferring verified targets.
    drop(download);
    let reopened = Cache::open(second_dir.path()).unwrap();
    Transfer::new(&client, &reopened)
        .unwrap()
        .download(&[base_hash, changed_hash], &[])
        .unwrap();
    assert_eq!(
        verified.load(std::sync::atomic::Ordering::Relaxed),
        completed
    );
    task.abort();
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
}
