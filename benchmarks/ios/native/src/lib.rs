use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::Manager;
mod network_probe;
mod legacy_restore_memory;

#[tauri::command]
async fn ios_bench_network_probe() -> Result<serde_json::Value, &'static str> {
    if ios_bench_phase() != "network" {
        return Err("Network verification phase required");
    }
    Ok(network_probe::probe().await)
}

#[tauri::command]
async fn ios_bench_authenticate(
    app: tauri::AppHandle,
) -> Result<serde_json::Value, &'static str> {
    if ios_bench_phase() != "oauth" {
        return Err("OAuth verification phase required");
    }
    #[cfg(target_os = "ios")]
    {
        use tauri_plugin_ios_native::{IosNativeExt, WebAuthenticationOutcome};

        let callback = "risunestoauthtest://oauth?code=synthetic&state=device";
        // The redirector must pass a non-HTTP callback target through unchanged.
        let authorization_url = "https://postman-echo.com/redirect-to?url=risunestoauthtest%3A%2F%2Foauth%3Fcode%3Dsynthetic%26state%3Ddevice";
        return Ok(match app
            .ios_native()
            .authenticate(authorization_url, "risunestoauthtest", true)
            .await
        {
            WebAuthenticationOutcome::Callback(callback_url) => {
                serde_json::json!({ "status": "succeeded", "callbackUrl": callback_url })
            }
            WebAuthenticationOutcome::Cancelled | WebAuthenticationOutcome::Unavailable => serde_json::json!({ "status": "cancelled" }),
            WebAuthenticationOutcome::Failed => serde_json::json!({ "status": "failed" }),
        });
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = app;
        Err("iOS runtime required")
    }
}

#[tauri::command]
fn ios_bench_phase() -> String {
    std::env::var("RISUNEST_IOS_PHASE").expect("verification phase")
}

#[tauri::command]
fn ios_bench_stream_url() -> String {
    std::env::var("RISUNEST_IOS_STREAM_URL").expect("synthetic stream endpoint")
}

#[tauri::command]
fn ios_bench_cloud_key() -> Result<String, &'static str> {
    let phase = ios_bench_phase();
    if phase != "cloud" && phase != "cloud-cancel" {
        return Err("Live verification phase required");
    }
    std::env::var("RISUNEST_IOS_CLOUD_KEY").map_err(|_| "Live credential unavailable")
}

#[tauri::command]
fn ios_bench_sync_input() -> Result<serde_json::Value, &'static str> {
    let read = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or("Sync inputs unavailable")
    };
    match ios_bench_phase().as_str() {
        "sync" => Ok(serde_json::json!({
            "registration": read("RISUNEST_IOS_SYNC_REGISTRATION")?,
            "expect": read("RISUNEST_IOS_SYNC_EXPECT")?,
            "marker": read("RISUNEST_IOS_SYNC_MARKER")?,
        })),
        "sync-publish" => Ok(serde_json::json!({
            "registration": read("RISUNEST_IOS_SYNC_REGISTRATION")?,
            "marker": read("RISUNEST_IOS_SYNC_PUBLISH")?,
        })),
        // A relaunch reconnects from the stored credential, never from a registration.
        "sync-restart" => Ok(serde_json::json!({ "marker": read("RISUNEST_IOS_SYNC_MARKER")? })),
        _ => Err("Sync verification phase required"),
    }
}

#[tauri::command]
fn ios_bench_external_input() -> Result<serde_json::Value, &'static str> {
    let read = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or("External storage inputs unavailable")
    };
    match ios_bench_phase().as_str() {
        "external-storage" => Ok(serde_json::json!({
            "webdav": {
                "endpoint": read("RISUNEST_IOS_WEBDAV_URL")?,
                "accountId": read("RISUNEST_IOS_WEBDAV_USER")?,
                "password": read("RISUNEST_IOS_WEBDAV_PASSWORD")?,
                "root": read("RISUNEST_IOS_WEBDAV_ROOT")?,
            },
            "before": read("RISUNEST_IOS_EXTERNAL_BEFORE")?,
            "after": read("RISUNEST_IOS_EXTERNAL_AFTER")?,
        })),
        // A relaunch uses the stored connection and secret, never fresh credentials.
        "external-storage-restart" => Ok(serde_json::json!({ "before": read("RISUNEST_IOS_EXTERNAL_BEFORE")? })),
        "external-sync" => Ok(serde_json::json!({
            "webdav": {
                "endpoint": read("RISUNEST_IOS_WEBDAV_URL")?,
                "accountId": read("RISUNEST_IOS_WEBDAV_USER")?,
                "password": read("RISUNEST_IOS_WEBDAV_PASSWORD")?,
                "root": read("RISUNEST_IOS_WEBDAV_SYNC_ROOT")?,
            },
            "before": read("RISUNEST_IOS_EXTERNAL_SYNC_FIRST")?,
            "after": read("RISUNEST_IOS_EXTERNAL_SYNC_SECOND")?,
        })),
        "external-sync-restart" => Ok(serde_json::json!({
            "before": read("RISUNEST_IOS_EXTERNAL_SYNC_FIRST")?,
            "after": read("RISUNEST_IOS_EXTERNAL_SYNC_SECOND")?,
        })),
        "backup-file" | "backup-file-restart" => Ok(serde_json::json!({
            "before": read("RISUNEST_IOS_BACKUP_FILE_BEFORE")?,
            "after": read("RISUNEST_IOS_BACKUP_FILE_AFTER")?,
        })),
        _ => Err("External storage verification phase required"),
    }
}

fn benchmark_handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        ios_bench_phase,
        legacy_restore_memory::ios_bench_peak_rss,
        ios_bench_report,
        ios_bench_stream_url,
        ios_bench_cloud_key,
        ios_bench_sync_input,
        ios_bench_external_input,
        ios_bench_network_probe,
        ios_bench_authenticate
    ]
}

#[tauri::command]
fn ios_bench_report(
    app: tauri::AppHandle,
    stage: String,
    result: serde_json::Value,
) -> Result<(), String> {
    let directory = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let path = directory.join(format!("verification-{}.jsonl", ios_bench_phase()));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    writeln!(
        file,
        "{}",
        serde_json::json!({ "stage": stage, "result": result })
    )
    .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

/// The generated benchmark main.mm calls this symbol, never the product entry.
#[no_mangle]
pub extern "C" fn ios_bench_start() {
    let result = std::panic::catch_unwind(|| {
        let product = risunest_lib::invoke_handler();
        let benchmark = benchmark_handler();
        let readback_received = AtomicU64::new(0);
        let app = risunest_lib::builder()
            .invoke_handler(move |invoke| {
                if invoke.message.command().starts_with("ios_bench_") {
                    benchmark(invoke)
                } else {
                    let readback = invoke.message.command() == "pds_read_conversation"
                        && ios_bench_phase().starts_with("legacy-restore-");
                    let received = if readback {
                        let count = readback_received.fetch_add(1, Ordering::Relaxed) + 1;
                        println!("RISUNEST_CR228_NATIVE stage=received count={count}");
                        Some(count)
                    } else {
                        None
                    };
                    let handled = product(invoke);
                    if let Some(count) = received {
                        println!("RISUNEST_CR228_NATIVE stage=dispatched count={count} handled={}", u8::from(handled));
                    }
                    handled
                }
            })
            .build(tauri::generate_context!())
            .expect("build iOS verification app");
        assert_eq!(
            app.config().identifier,
            "io.github.rsyumi.risunest.ios.bench"
        );
        app.run(risunest_lib::handle_run_event);
    });
    if result.is_err() {
        std::process::abort();
    }
}
