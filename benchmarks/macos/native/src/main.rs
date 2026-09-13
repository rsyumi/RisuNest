//! This executable alone owns the verification commands. Product builds never import it.
use std::{io::Write, sync::Mutex};
use tauri::Manager;

#[derive(Default)]
struct Events(Mutex<Vec<&'static str>>);

#[tauri::command]
fn macos_bench_phase() -> String {
    std::env::var("RISUNEST_MACOS_PHASE").expect("controller phase")
}

#[tauri::command]
fn macos_bench_report(stage: String, result: serde_json::Value) -> Result<(), String> {
    let path = std::env::var("RISUNEST_MACOS_REPORT").map_err(|error| error.to_string())?;
    let record = serde_json::json!({ "stage": stage, "result": result });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(format!("{record}\n").as_bytes())
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())
}

#[tauri::command]
fn macos_bench_events(state: tauri::State<'_, Events>) -> Vec<&'static str> {
    state.0.lock().unwrap().clone()
}

#[tauri::command]
fn macos_bench_quit(app: tauri::AppHandle) {
    app.exit(0);
}

fn main() {
    let product = risunest_lib::invoke_handler();
    let benchmark = tauri::generate_handler![
        macos_bench_phase,
        macos_bench_report,
        macos_bench_events,
        macos_bench_quit,
    ];
    let app = risunest_lib::builder()
        .manage(Events::default())
        .invoke_handler(move |invoke| {
            if invoke.message.command().starts_with("macos_bench_") {
                benchmark(invoke)
            } else {
                product(invoke)
            }
        })
        .build(tauri::generate_context!())
        .expect("build isolated Mac harness");
    assert_eq!(
        app.config().identifier,
        "io.github.rsyumi.risunest.macos.bench"
    );
    app.run(|app, event| {
        let name = match &event {
            tauri::RunEvent::Opened { .. } => Some("opened"),
            tauri::RunEvent::Reopen { .. } => Some("reopen"),
            tauri::RunEvent::ExitRequested { .. } => Some("quit"),
            tauri::RunEvent::WindowEvent {
                event: tauri::WindowEvent::CloseRequested { .. },
                ..
            } => Some("close"),
            _ => None,
        };
        if let Some(name) = name {
            app.state::<Events>().0.lock().unwrap().push(name);
        }
        risunest_lib::handle_run_event(app, event);
    });
}
