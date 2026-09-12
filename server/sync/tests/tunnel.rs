use risunest_sync_server::{
    connection::ConnectionOptions, runtime::ConnectionRuntime, store::Store,
};
use std::{sync::Arc, time::Duration};

// Compiled only by this integration test. No fixture entry or switch is shipped.
fn fixture(root: &std::path::Path) -> std::path::PathBuf {
    let source = root.join("synthetic.rs");
    std::fs::write(&source, r#"
use std::{io::Write, time::Duration};
fn main() {
    let root = std::env::current_exe().unwrap().parent().unwrap().to_owned();
    let first = !root.join("started").exists();
    std::fs::write(root.join("started"), b"started").unwrap();
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(root.join("pids")).unwrap();
    writeln!(log, "{}", std::process::id()).unwrap();
    println!("{{\"message\":\"https://synthetic.trycloudflare.com\"}}");
    // Endpoint alone must not be considered ready.
    std::thread::sleep(Duration::from_millis(250));
    println!("{{\"message\":\"Registered tunnel connection\"}}");
    if first { std::thread::sleep(Duration::from_millis(250)); return; }
    loop { std::thread::sleep(Duration::from_secs(1)); }
}
"#).unwrap();
    let binary = root.join(if cfg!(windows) {
        "synthetic.exe"
    } else {
        "synthetic"
    });
    let output = std::process::Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture compiler: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    binary
}

#[tokio::test]
async fn managed_child_restarts_and_shutdown_owns_its_lifetime() {
    let binaries = tempfile::tempdir().unwrap();
    let executable = fixture(binaries.path());
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::init(root.path()).unwrap());
    store
        .configure_connection(ConnectionOptions {
            endpoint: None,
            cloudflared: Some(executable),
            registry_url: None,
        })
        .unwrap();
    let runtime =
        ConnectionRuntime::start(store.clone(), "127.0.0.1:4319".parse().unwrap()).unwrap();
    let mut view = runtime.tunnel.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut connected = 0;
        loop {
            view.changed().await.unwrap();
            if view.borrow().phase == "connected" {
                connected += 1;
            }
            if connected == 2 {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        store.connection_status().unwrap().endpoint.as_deref(),
        Some("https://synthetic.trycloudflare.com")
    );
    // Closing a GUI subscription does not terminate the daemon runtime.
    drop(view);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(runtime.tunnel.borrow().phase, "connected");
    runtime.shutdown().await;
    let pids = std::fs::read_to_string(binaries.path().join("pids")).unwrap();
    assert_eq!(pids.lines().count(), 2);
    #[cfg(windows)]
    for pid in pids.lines() {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        };
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid.parse().unwrap()) };
        if !handle.is_null() {
            assert_eq!(unsafe { WaitForSingleObject(handle, 1000) }, 0);
            unsafe {
                CloseHandle(handle);
            }
        }
    }
}

#[tokio::test]
async fn fixed_endpoint_mode_does_not_start_a_tunnel() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::init(root.path()).unwrap());
    store
        .configure_connection(ConnectionOptions {
            endpoint: Some("https://sync.example".into()),
            cloudflared: None,
            registry_url: None,
        })
        .unwrap();
    let runtime = ConnectionRuntime::start(store, "127.0.0.1:4319".parse().unwrap()).unwrap();
    let mut view = runtime.tunnel.clone();
    tokio::time::timeout(Duration::from_secs(2), view.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(view.borrow().phase, "external");
    runtime.shutdown().await;
}
