use std::io::Write;
use tauri::Manager;
mod network_probe;

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
        let authorization_url = "https://httpbin.org/redirect-to?url=risunestoauthtest%3A%2F%2Foauth%3Fcode%3Dsynthetic%26state%3Ddevice";
        return Ok(match app
            .ios_native()
            .authenticate(authorization_url, "risunestoauthtest", true)
            .await
        {
            WebAuthenticationOutcome::Callback(callback_url) => {
                serde_json::json!({ "status": "succeeded", "callbackUrl": callback_url })
            }
            WebAuthenticationOutcome::Cancelled => serde_json::json!({ "status": "cancelled" }),
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

fn benchmark_handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        ios_bench_phase,
        ios_bench_report,
        ios_bench_stream_url,
        ios_bench_cloud_key,
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
        let app = risunest_lib::builder()
            .invoke_handler(move |invoke| {
                if invoke.message.command().starts_with("ios_bench_") {
                    benchmark(invoke)
                } else {
                    product(invoke)
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
