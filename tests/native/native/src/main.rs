use std::{fs, path::PathBuf};
use tauri::Manager;

fn value(name: &str) -> String { std::env::var(name).expect(name) }

#[tauri::command]
fn boundary_phase(app: tauri::AppHandle) -> serde_json::Value {
    let root = app.path().app_data_dir().unwrap();
    assert_eq!(root, PathBuf::from(value("RISUNEST_BOUNDARY_ROOT")));
    assert_eq!(fs::read_to_string(root.join("boundary-owner")).unwrap(), value("RISUNEST_BOUNDARY_ID"));
    serde_json::json!({"runId": value("RISUNEST_BOUNDARY_ID"), "phase": value("RISUNEST_BOUNDARY_PHASE"),
        "appLockUnavailable": std::env::var("RISUNEST_BOUNDARY_APP_LOCK_UNAVAILABLE").as_deref() == Ok("1"),
        "accountOrigin": std::env::var("RISUNEST_BOUNDARY_ACCOUNT_ORIGIN").ok()})
}

#[tauri::command]
fn boundary_cover_present(app: tauri::AppHandle) -> Result<bool, String> {
    let window = app.get_webview_window("main").ok_or("main window unavailable")?;
    if window.is_visible().map_err(|error| error.to_string())? { return Err("boundary window must stay hidden".into()); }
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        extern "system" {
            fn FindWindowExW(parent: *mut std::ffi::c_void, after: *mut std::ffi::c_void, class: *const u16, title: *const u16) -> *mut std::ffi::c_void;
        }
        let class: Vec<u16> = "RisuNestLockCover\0".encode_utf16().collect();
        let parent = window.hwnd().map_err(|error| error.to_string())?.0;
        Ok(!unsafe { FindWindowExW(parent, std::ptr::null_mut(), class.as_ptr(), std::ptr::null()) }.is_null())
    }
    #[cfg(not(windows))]
    { Ok(true) }
}

fn write_report(report: &serde_json::Value, options: &mut fs::OpenOptions) -> Result<(), String> {
    if report["runId"] != value("RISUNEST_BOUNDARY_ID") || report["phase"] != value("RISUNEST_BOUNDARY_PHASE") {
        return Err("incorrect report identity".into());
    }
    let path = PathBuf::from(value("RISUNEST_BOUNDARY_REPORT"));
    let mut file = options.write(true).open(path).map_err(|e| e.to_string())?;
    use std::io::Write;
    file.write_all(report.to_string().as_bytes()).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

#[tauri::command]
fn boundary_finish(app: tauri::AppHandle, report: serde_json::Value) -> Result<(), String> {
    write_report(&report, fs::OpenOptions::new().create_new(true))?;
    app.exit(if report["success"] == true { 0 } else { 1 });
    Ok(())
}

/// Records the report of a phase that the app's own native exit ends; a failure exits with `exit`.
#[tauri::command]
fn boundary_record(app: tauri::AppHandle, report: serde_json::Value, exit: Option<i32>) -> Result<(), String> {
    write_report(&report, fs::OpenOptions::new().create(true).truncate(true))?;
    if let Some(code) = exit { app.exit(code); }
    Ok(())
}

/// Tells the runner that the document listens for the session end it is about to send.
#[tauri::command]
fn boundary_ready() -> Result<(), String> {
    fs::OpenOptions::new().write(true).create_new(true)
        .open(format!("{}.ready", value("RISUNEST_BOUNDARY_REPORT"))).map(drop).map_err(|e| e.to_string())
}

/// Closes the main window the way a user does, so the close request reaches the app first.
#[tauri::command]
fn boundary_close_main(app: tauri::AppHandle) -> Result<(), String> {
    app.get_webview_window("main").ok_or("main window unavailable")?.close().map_err(|e| e.to_string())
}

fn boundary_handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![boundary_phase, boundary_finish, boundary_record, boundary_ready, boundary_close_main, boundary_cover_present]
}

fn main() {
    let id = value("RISUNEST_BOUNDARY_ID");
    assert!(id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()));
    let identifier = format!("io.github.rsyumi.risunest.boundary.{id}");
    let root = PathBuf::from(value("RISUNEST_BOUNDARY_ROOT"));
    assert!(root.is_absolute());
    assert_eq!(root.file_name().unwrap(), identifier.as_str());
    assert_eq!(fs::read_to_string(root.join("boundary-owner")).unwrap(), id);
    let mut context = tauri::generate_context!();
    assert_eq!(context.config().identifier, "io.github.rsyumi.risunest.boundary");
    assert!(!context.config().app.windows[0].visible, "boundary window must stay hidden");
    assert!(matches!(context.config().build.frontend_dist, Some(tauri::utils::config::FrontendDist::Files(_))));
    context.config_mut().identifier = identifier;
    context.config_mut().app.windows[0].data_directory = Some(root.join("webview"));
    #[cfg(target_os = "macos")]
    { context.config_mut().app.windows[0].incognito = true; }
    let paths = risunest_lib::AppPaths::isolated(root.clone()).expect("isolated boundary roots");
    assert_eq!(paths.data, root);
    let product = risunest_lib::invoke_handler();
    let boundary = boundary_handler();
    // The harness page exposes its failure record once it loads; an error before that keeps the text alone.
    let bootstrap = format!(r#"(() => {{
        const finish = (error, thrown) => {{
            const report = {{runId: {}, phase: {}, success: false, cases: [], error}};
            try {{
                const record = window.__boundaryFailureRecord;
                if (typeof record === 'function' && thrown !== undefined && thrown !== null) report.failure = record(thrown);
            }} catch {{}}
            window.__TAURI_INTERNALS__.invoke('boundary_finish', {{ report }});
        }};
        window.addEventListener('error', event => finish(String(event.message), event.error));
        window.addEventListener('unhandledrejection', event => finish(String(event.reason), event.reason));
    }})();"#, serde_json::json!(id), serde_json::json!(value("RISUNEST_BOUNDARY_PHASE")));
    let builder = risunest_lib::builder().manage(paths).append_invoke_initialization_script(&bootstrap).invoke_handler(move |invoke| {
        if invoke.message.command().starts_with("boundary_") { boundary(invoke) } else { product(invoke) }
    });
    #[cfg(windows)]
    let builder = builder.plugin(tauri::plugin::Builder::<tauri::Wry>::new("windows-appearance").build());
    let app = builder.build(context).expect("build isolated persistence boundary");
    assert_eq!(app.path().app_data_dir().unwrap(), root);
    app.run(risunest_lib::handle_run_event);
}
