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
        ios_bench_network_probe
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
