mod account_credential;
#[cfg(any(test, target_os = "android"))]
mod android_commit_transport;
mod app_paths;
#[cfg(desktop)]
mod desktop_notifications;
#[cfg(desktop)]
mod startup_appearance;
mod cleanup_secrets;
mod device_secrets;
mod cleanup_webview;
mod app_cleanup;
mod app_update;
#[cfg(desktop)]
mod appimage_integration;
mod asset_repository;
mod asset_byte_transport;
mod boot_marker;
#[cfg(windows)]
mod windows_session;
mod cold_payload_codec;
mod data_health;
#[cfg(any(windows, target_os = "linux"))]
mod desktop_session;
pub(crate) mod device_backup;
mod external_storage;
pub mod import_export_jobs;
#[cfg(target_os = "ios")]
mod ios_lifecycle;
#[cfg_attr(not(desktop), allow(dead_code))]
mod launch_profile;
#[cfg(target_os = "linux")]
mod linux_session;
#[cfg(target_os = "linux")]
mod linux_webview;
#[allow(dead_code)]
mod local_backup;
mod logical_records;
mod lossless_f0;
#[cfg(any(test, target_os = "macos"))]
mod macos_lifecycle;
pub mod native_file_jobs;
pub(crate) mod native_log;
mod diagnostics;
mod generation_diagnostics;
mod network_log;
mod native_media;
mod native_tokenizer;
#[cfg(desktop)]
mod opened_files;
#[cfg(any(windows, target_os = "linux", target_os = "ios", target_os = "macos", target_os = "android"))]
mod persistent_commit_raw;
#[cfg(windows)]
mod persistent_commit_transport;
mod persistent_store;
mod platform_tls;
mod portable_backup;
#[cfg(feature = "official-publication-upload-pilot")]
mod publication_upload;
#[cfg(any(
    test,
    target_os = "windows",
    target_os = "android",
    target_os = "linux",
    target_os = "ios",
    target_os = "macos"
))]
mod regex_shadow;
#[cfg(not(target_os = "android"))]
mod renderer_recovery;
mod server_sync;
mod sqlite_open;
mod trust_boundary;
#[cfg(test)]
mod command_future_size;
#[cfg(test)]
mod test_memory;
#[cfg(test)]
mod test_process;
#[cfg(windows)]
mod windows_appearance;
#[cfg(windows)]
mod windows_webview;
mod webview_navigation;
mod webview_navigation_policy;

use risunest_network_log::reqwest::RequestBuilderExt;
use base64::{engine::general_purpose, Engine as _};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;

const MAX_NATIVE_STARTUP_ERROR_CHARS: usize = 4096;

#[derive(Default)]
struct NativeStartupFailure {
    message: Option<String>,
    _persistent_gate: Option<persistent_store::commands::DeviceMaintenanceGuard>,
    _native_file_gate: Option<native_file_jobs::admission::Permit>,
}

#[derive(Clone, Default)]
pub(crate) struct NativeStartupState(Arc<Mutex<NativeStartupFailure>>);

impl NativeStartupState {
    fn record_failure(
        &self,
        error: impl std::fmt::Display,
        persistent_gate: Option<persistent_store::commands::DeviceMaintenanceGuard>,
        native_file_gate: Option<native_file_jobs::admission::Permit>,
    ) {
        let message = error
            .to_string()
            .chars()
            .take(MAX_NATIVE_STARTUP_ERROR_CHARS)
            .collect::<String>();
        let mut failure = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if failure.message.is_none() {
            failure.message = Some(message);
            failure._persistent_gate = persistent_gate;
            failure._native_file_gate = native_file_gate;
        }
    }

    pub(crate) fn release_cleanup_gates(&self) -> Result<(), String> {
        let mut failure = self.0.lock().map_err(|_| "cleanup-startup-state-unavailable")?;
        failure._persistent_gate.take();
        failure._native_file_gate.take();
        Ok(())
    }

    pub(crate) fn finish_cleanup(&self) -> Result<(), String> {
        let mut failure = self.0.lock().map_err(|_| "cleanup-startup-state-unavailable")?;
        failure.message = None;
        Ok(())
    }

    pub(crate) fn ensure_ready(&self) -> Result<(), String> {
        let failure = self
            .0
            .lock()
            .map_err(|error| {
                let _ = native_log::logged_without_detail::<(), _>(
                    "native_startup_status",
                    Err("startup-state-unavailable".to_owned()),
                );
                format!("native startup status lock failed: {error}")
            })?;
        match failure.message.as_ref() {
            Some(error) => Err(format!("Native setup failed: {error}")),
            None => Ok(()),
        }
    }
}

#[tauri::command]
fn native_startup_status(state: tauri::State<'_, NativeStartupState>) -> Result<(), String> {
    state.ensure_ready()
}

fn boot_marker_root(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    app_paths::data_root(app)
}

fn boot_marker_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// Records that a start has begun and reports what the last one left. The renderer calls it
/// before anything it could fail in, so a start that never reaches the app still counts.
#[tauri::command(async)]
fn boot_attempt_begin(app: tauri::AppHandle) -> Result<boot_marker::BootDecision, String> {
    crate::native_log::logged_without_detail("boot_attempt_begin", (|| {
        let version = app.package_info().version.to_string();
        let root = boot_marker_root(&app)?;
        boot_marker::begin(&root, &version, boot_marker_now())
            .map_err(|error| format!("failed to record the start attempt: {error}"))
    })())
}

/// Records that the start finished, so the next one begins from zero.
#[tauri::command(async)]
fn boot_attempt_complete(app: tauri::AppHandle) -> Result<(), String> {
    crate::native_log::logged_without_detail("boot_attempt_complete", (|| {
        let root = boot_marker_root(&app)?;
        boot_marker::complete(&root)
            .map_err(|error| format!("failed to clear the start attempt: {error}"))
    })())
}

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[track_caller]
fn log_native_request_failure(code: &'static str) {
    let _ = native_log::logged_without_detail::<(), _>("native_request", Err(code.to_owned()));
}

#[tauri::command]
async fn native_request(url: String, body: String, header: String, method: String) -> String {
    let headers_json: Value = match serde_json::from_str(&header) {
        Ok(h) => h,
        Err(e) => {
            log_native_request_failure("invalid-header-json");
            return format!(r#"{{"success":false,"body":"{}"}}"#, e.to_string());
        }
    };

    let mut headers = HeaderMap::new();
    let method = method.to_string();
    if let Some(obj) = headers_json.as_object() {
        for (key, value) in obj {
            let header_name = match HeaderName::from_bytes(key.as_bytes()) {
                Ok(name) => name,
                Err(e) => {
                    log_native_request_failure("invalid-header-name");
                    return format!(r#"{{"success":false,"body":"{}"}}"#, e.to_string());
                }
            };
            let header_value = match HeaderValue::from_str(value.as_str().unwrap_or("")) {
                Ok(value) => value,
                Err(e) => {
                    log_native_request_failure("invalid-header-value");
                    return format!(r#"{{"success":false,"body":"{}"}}"#, e.to_string());
                }
            };
            headers.insert(header_name, header_value);
        }
    } else {
        log_native_request_failure("header-json-shape");
        return format!(r#"{{"success":false,"body":"Invalid header JSON"}}"#);
    }

    let client = match platform_tls::client_builder().build() {
        Ok(client) => client,
        Err(e) => {
            log_native_request_failure("client-build");
            return format!(r#"{{"success":false,"body":"{}"}}"#, e.to_string());
        }
    };
    let response: Result<reqwest::Response, reqwest::Error>;

    if method == "POST" {
        response = client
            .post(&url)
            .headers(headers)
            .timeout(Duration::from_secs(120))
            .body(body)
            .send_logged("native-http")
            .await;
    } else {
        response = client
            .get(&url)
            .headers(headers)
            .timeout(Duration::from_secs(120))
            .send_logged("native-http")
            .await;
    }

    match response {
        Ok(resp) => {
            let headers = resp.headers();
            let header_json = header_map_to_json(headers);
            let status = resp.status().as_u16().to_string();
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    log_native_request_failure("response-body");
                    return format!(r#"{{"success":false,"body":"{}"}}"#, e.to_string());
                }
            };
            let encoded = general_purpose::STANDARD.encode(&bytes);

            format!(
                // r#"{{"success":true,"body":"{}","headers":{}}}"#,
                r#"{{"success":true,"body":"{}","headers":{},"status":{}}}"#,
                encoded, header_json, status
            )
        }
        Err(e) => {
            log_native_request_failure("transport");
            format!(
                r#"{{"success":false,"body":"{}","status":400}}"#,
                e.to_string()
            )
        }
    }
}

pub use app_paths::{AppPaths, Integration};

/// Remembers the main window geometry beside the store. The plugin resolves its
/// directory once at registration, so it is wired where the manifest is known.
#[cfg(desktop)]
fn window_state_plugin(directory: &std::path::Path) -> tauri::plugin::TauriPlugin<tauri::Wry> {
    use tauri_plugin_window_state::StateFlags;
    tauri_plugin_window_state::Builder::default()
        .with_state_flags(StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED)
        .with_filter(|label| label == "main")
        .with_directory(directory.to_path_buf())
        .with_default_maximized(true)
        .with_show_after_restore(!cfg!(windows))
        .build()
}

/// Keeps IPC responses on the custom protocol after a failed body read.
fn ipc_read_guard_script() -> String {
    include_str!("ipc_read_guard.js").replace(
        "__RISUNEST_ANDROID__",
        if cfg!(target_os = "android") { "true" } else { "false" },
    )
}

/// Product initialization shared by native entry points. An alternative entry
/// manages its own [`AppPaths`] before building.
pub fn builder() -> tauri::Builder<tauri::Wry> {
    builder_with_main_window(None, None)
}

fn builder_with_main_window(
    main_window: Option<app_paths::MainWindow>,
    #[cfg_attr(mobile, allow(unused_variables))] instance_id: Option<String>,
) -> tauri::Builder<tauri::Wry> {
    native_log::install_panic_hook();
    let native_log_state = native_log::global_state();
    let setup_native_log_state = native_log_state.clone();
    let native_startup_state = NativeStartupState::default();
    let setup_native_startup_state = native_startup_state.clone();
    let mut builder = tauri::Builder::default()
        .manage(native_startup_state)
        .plugin(tauri_plugin_opener::init())
        .plugin(webview_navigation::init())
        .append_invoke_initialization_script(ipc_read_guard_script());
    #[cfg(not(target_os = "android"))]
    {
        builder = builder.manage(renderer_recovery::RendererRecovery::default());
    }
    #[cfg(desktop)]
    {
        // Reject a second process before plugins with startup side effects run.
        let mut single_instance = tauri_plugin_single_instance::Builder::new().callback(|app, args, cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
            opened_files::deliver_single_instance_arguments(app, &args, &cwd);
        });
        if let Some(instance_id) = instance_id {
            single_instance = single_instance.instance_id(instance_id);
        }
        builder = builder
            .manage(opened_files::OpenedFilesState::from_launch_arguments())
            .plugin(single_instance.build());
    }
    #[cfg(target_os = "ios")]
    {
        builder = builder
            .append_invoke_initialization_script(include_str!("ios_ipc.js"))
            .manage(ios_lifecycle::RestartState::default());
    }
    #[cfg(not(target_os = "ios"))]
    {
        builder = builder.plugin(tauri_plugin_shell::init());
    }
    #[cfg(windows)]
    {
        builder = builder
            .plugin(windows_appearance::init())
            .plugin(windows_webview::init());
    }
    #[cfg(target_os = "linux")]
    {
        builder = builder.plugin(linux_webview::init());
    }
    #[cfg(target_os = "macos")]
    {
        builder = builder.manage(macos_lifecycle::ExitState::default());
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        // Replaces Tauri's default handler, which only reloads.
        builder = builder.on_web_content_process_terminate(|webview| {
            if webview.label() == "main" {
                crate::nlog!("error", "WebKit web content process ended");
                generation_diagnostics::renderer_exit(generation_diagnostics::RendererExit::Apple { reason: "unknown" });
                generation_diagnostics::reload_result("requested");
                renderer_recovery::renderer_exited(webview.app_handle());
                // A quit that arrives before the reloaded document is ready no longer waits for it.
                #[cfg(target_os = "macos")]
                macos_lifecycle::document_started(webview.app_handle());
            }
            if let Err(error) = webview.reload() {
                if webview.label() == "main" { generation_diagnostics::reload_result("failed"); }
                crate::nlog!("error", "Could not reload after the web content process ended: {error}");
            }
        });
    }
    #[cfg(target_os = "android")]
    {
        builder = builder
            .manage(android_commit_transport::native_state().clone())
            .on_page_load(|webview, payload| {
                if webview.label() == "main"
                    && matches!(payload.event(), tauri::webview::PageLoadEvent::Started)
                {
                    generation_diagnostics::document_started();
                    if let Some(state) =
                        webview.try_state::<persistent_store::PersistentStoreState>()
                    {
                        if let Err(error) = state.reset_renderer_session() {
                            crate::nlog!(
                                "error",
                                "failed to reset persistent renderer session: {error}"
                            );
                        }
                    }
                    if let Some(state) =
                        webview.try_state::<asset_repository::commands::DurableCasJobState>()
                    {
                        if let Err(error) = state.reset_renderer_session(webview.app_handle()) {
                            crate::nlog!(
                                "error",
                                "failed to reset durable CAS renderer session: {error}"
                            );
                        }
                    }
                    if let Some(state) =
                        webview.try_state::<native_media::ipc::NativeMediaIpcState>()
                    {
                        if let Err(error) = state.reset_renderer_session() {
                            crate::nlog!(
                                "error",
                                "failed to reset native media renderer session: {error}"
                            );
                        }
                    }
                    if let Some(state) =
                        webview.try_state::<server_sync::commands::ServerSyncCommandState>()
                    {
                        if let Err(error) = state.reset_renderer_session() {
                            crate::nlog!(
                                "error",
                                "failed to reset server sync renderer session: {}",
                                error.code
                            );
                        }
                    }
                    webview
                        .state::<android_commit_transport::AndroidCommitState>()
                        .reset();
                }
            });
    }
    #[cfg(windows)]
    {
        builder = builder.on_page_load(|webview, payload| {
            if webview.label() == "main"
                && matches!(payload.event(), tauri::webview::PageLoadEvent::Started)
            {
                generation_diagnostics::document_started();
                renderer_recovery::document_started(webview.app_handle());
                if let Some(state) = webview.try_state::<persistent_store::PersistentStoreState>() {
                    if let Err(error) = state.reset_renderer_session() {
                        crate::nlog!(
                            "error",
                            "failed to reset persistent renderer session: {error}"
                        );
                    }
                }
                if let Some(state) =
                    webview.try_state::<asset_repository::commands::DurableCasJobState>()
                {
                    if let Err(error) = state.reset_renderer_session(webview.app_handle()) {
                        crate::nlog!(
                            "error",
                            "failed to reset durable CAS renderer session: {error}"
                        );
                    }
                }
                if let Some(state) = webview.try_state::<native_media::ipc::NativeMediaIpcState>() {
                    if let Err(error) = state.reset_renderer_session() {
                        crate::nlog!(
                            "error",
                            "failed to reset native media renderer session: {error}"
                        );
                    }
                }
                if let Some(state) =
                    webview.try_state::<server_sync::commands::ServerSyncCommandState>()
                {
                    if let Err(error) = state.reset_renderer_session() {
                        crate::nlog!(
                            "error",
                            "failed to reset server sync renderer session: {}",
                            error.code
                        );
                    }
                }
                let _ = webview.with_webview(|_| persistent_commit_transport::reset());
            }
        });
    }

    #[cfg(not(any(windows, target_os = "android")))]
    {
        builder = builder.on_page_load(|webview, payload| {
            if webview.label() == "main"
                && matches!(payload.event(), tauri::webview::PageLoadEvent::Started)
            {
                generation_diagnostics::document_started();
                #[cfg(target_os = "ios")]
                ios_lifecycle::main_document_started(webview.app_handle());
                #[cfg(target_os = "macos")]
                macos_lifecycle::document_started(webview.app_handle());
                #[cfg(target_os = "linux")]
                renderer_recovery::document_started(webview.app_handle());
                if let Some(state) = webview.try_state::<persistent_store::PersistentStoreState>() {
                    if let Err(error) = state.reset_renderer_session() {
                        crate::nlog!(
                            "error",
                            "failed to reset persistent renderer session: {error}"
                        );
                    }
                }
                if let Some(state) =
                    webview.try_state::<asset_repository::commands::DurableCasJobState>()
                {
                    if let Err(error) = state.reset_renderer_session(webview.app_handle()) {
                        crate::nlog!(
                            "error",
                            "failed to reset durable CAS renderer session: {error}"
                        );
                    }
                }
                if let Some(state) = webview.try_state::<native_media::ipc::NativeMediaIpcState>() {
                    if let Err(error) = state.reset_renderer_session() {
                        crate::nlog!(
                            "error",
                            "failed to reset native media renderer session: {error}"
                        );
                    }
                }
                if let Some(state) =
                    webview.try_state::<server_sync::commands::ServerSyncCommandState>()
                {
                    if let Err(error) = state.reset_renderer_session() {
                        crate::nlog!(
                            "error",
                            "failed to reset server sync renderer session: {}",
                            error.code
                        );
                    }
                }
            }
        });
    }

    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_notification::init())
            .plugin(tauri_plugin_updater::Builder::new()
            .relaunch_args(opened_files::relaunch_arguments(std::env::args_os(), std::env::current_dir().ok().as_deref()))
            .build());
    }
    #[cfg(any(windows, target_os = "linux"))]
    {
        builder = builder.manage(desktop_session::SessionState::default())
            .append_invoke_initialization_script(include_str!("desktop_lifecycle.js"));
    }

    builder
        .setup(move |app| {
            #[cfg(mobile)]
            app.manage(app_paths::AppPaths::resolve(app)?);
            // `run` resolves the desktop manifest before the builder, because the
            // uninstall CLI and the WebView profile need it first. An entry that
            // builds without it still gets one here, from the same configuration.
            #[cfg(desktop)]
            if app.try_state::<app_paths::AppPaths>().is_none() {
                app.manage(app_paths::AppPaths::desktop(app.config())?.prepared()?);
            }
            let app_data_dir = app_paths::data_root(app)?;
            let _ = native_log::logged_without_detail(
                "network_log_initialize",
                network_log::initialize(&app.state::<app_paths::AppPaths>()),
            );
            setup_native_log_state.configure_file_path(&app_data_dir);
            app.manage(setup_native_log_state.clone());
            app.manage(app_cleanup::CleanupState::initialize(app.handle())?);
            app.manage(native_file_jobs::NativeFileJobState::initialize(
                app_data_dir.join("native-file-jobs"),
            ));
            // Before the WebView exists, so no renderer call can precede it.
            let renderer_access = app_paths::permit_renderer_access(app.handle());
            if let Err(error) = &renderer_access {
                let native_file_gate = app.state::<native_file_jobs::NativeFileJobState>().admission.file(true).ok();
                let persistent_gate = app.state::<persistent_store::PersistentStoreState>().acquire_device_maintenance().ok();
                setup_native_startup_state.record_failure(error, persistent_gate, native_file_gate);
            }
            if let Some(main_window) = &main_window {
                let mut window_builder = tauri::WebviewWindowBuilder::from_config(app, &main_window.config)?;
                if let Some(data_directory) = &main_window.data_directory {
                    window_builder = window_builder.data_directory(data_directory.clone());
                }
                #[cfg(target_os = "macos")]
                if let Some(data_store) = main_window.data_store {
                    window_builder = window_builder.data_store_identifier(data_store);
                }
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                let window_builder = startup_appearance::configure(app.handle(), &main_window.config, window_builder);
                window_builder.build()?;
            }
            let setup_result = (|| -> Result<(), String> {
                renderer_access?;
                #[cfg(windows)]
                windows_session::install(app.handle())?;
                #[cfg(target_os = "linux")]
                linux_session::install(app.handle());
                #[cfg(target_os = "macos")]
                macos_lifecycle::install_native_quit(app.handle())?;
                #[cfg(any(target_os = "android", target_os = "ios"))]
                app.handle()
                    .plugin(tauri_plugin_barcode_scanner::init())
                    .map_err(|error| format!("barcode scanner initialization failed: {error}"))?;
                #[cfg(target_os = "ios")]
                app.handle()
                    .plugin(tauri_plugin_ios_native::init(
                        app_paths::data_root(app)?
                            .to_str()
                            .ok_or_else(|| {
                                "application data root is not valid UTF-8".to_owned()
                            })?
                            .to_owned(),
                    ))
                    .map_err(|error| format!("iOS native initialization failed: {error}"))?;
                let app_data_dir = app_paths::data_root(app)?;
                app.state::<external_storage::job_store::JobCommandState>()
                    .root
                    .set(app_data_dir.clone())
                    .map_err(|_| "external storage root is already configured".to_string())?;
                app.state::<external_storage::job_store::JobCommandState>()
                    .open_lease_ledger(&app.state::<app_paths::AppPaths>().cache)
                    .map_err(|error| format!("external lease ownership unavailable: {error}"))?;
                external_storage::leftovers::remove_at_startup(&app_data_dir);
                let agent_build = app
                    .config()
                    .plugins
                    .0
                    .get("risunest")
                    .and_then(|value| value.get("agent"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                app.manage(app_update::AppUpdateState::initialize(agent_build));
                let device_backup = device_backup::DeviceBackupState::initialize(
                    app_data_dir.join("device-backup"),
                );
                // Manage recovery state before acquiring either fence. If a later
                // startup step fails, an attached fence remains live while the
                // renderer shows the failure panel.
                app.manage(device_backup);
                let device_backup = app.state::<device_backup::DeviceBackupState>();
                let state = app.state::<native_file_jobs::NativeFileJobState>();
                // Recover device state before opening the PDS or permitting other
                // native writers. Initialization failures also keep these gates shut.
                let device_recovery_pending = !matches!(device_backup.is_blocking(), Ok(false));
                if device_recovery_pending {
                    let admission = state
                        .admission
                        .file(true)
                        .map_err(|error| format!("native file admission failed: {error}"))?;
                    device_backup
                        .attach_startup_admission(admission)
                        .map_err(|error| format!("device startup admission failed: {error}"))?;
                    let guard = app
                        .state::<persistent_store::PersistentStoreState>()
                        .acquire_device_maintenance()
                        .map_err(|error| {
                            format!("persistent device maintenance gate failed: {error}")
                        })?;
                    device_backup
                        .attach_maintenance_guard(guard)
                        .map_err(|error| format!("device maintenance guard failed: {error}"))?;
                }
                app.state::<native_media::ipc::NativeMediaIpcState>()
                    .configure(app_data_dir.join("native-media-ipc"))?;
                server_sync::residency::arm_anchor(&app_data_dir);
                app.manage(native_media::streaming::MediaServerState::initialize(
                    app_data_dir.clone(),
                    app.state::<app_paths::AppPaths>().cache.clone(),
                ));
                app.manage(
                    native_file_jobs::screenshot_output::ScreenshotOutputState::initialize(
                        app_data_dir
                            .join("native-file-jobs")
                            .join("screenshot-output"),
                    ),
                );
                #[cfg(any(
                    target_os = "windows",
                    target_os = "android",
                    target_os = "linux",
                    target_os = "ios",
                    target_os = "macos"
                ))]
                app.manage(regex_shadow::RegexCancellationRegistry::default());
                Ok(())
            })();
            if let Err(error) = setup_result {
                crate::nlog!("error", "native startup initialization failed: {error}");
                // Retain both library-wide gates for the rest of the process.
                // A gate that cannot be acquired is already held or unavailable,
                // either of which also rejects later operations.
                let native_file_gate = app
                    .try_state::<native_file_jobs::NativeFileJobState>()
                    .and_then(|state| state.admission.file(true).ok());
                let persistent_gate = app
                    .state::<persistent_store::PersistentStoreState>()
                    .acquire_device_maintenance()
                    .ok();
                setup_native_startup_state.record_failure(error, persistent_gate, native_file_gate);
            }
            Ok(())
        })
        .manage(asset_repository::commands::DurableCasJobState::default())
        .manage(native_media::ipc::NativeMediaIpcState::default())
        .manage(persistent_store::PersistentStoreState::default())
        .manage(persistent_store::commands::data_health::DataHealthState::default())
        .manage(server_sync::commands::ServerSyncCommandState::default())
        .manage(external_storage::connection_commands::ConnectionCommandState::default())
        .manage(external_storage::job_store::JobCommandState::default())
        .plugin(tauri_plugin_http::init_with_client_builder(platform_tls::apply))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(invoke_handler())
}

/// The product command router, reusable by alternative native entries.
pub fn invoke_handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    let handler: Box<dyn Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync> = Box::new(tauri::generate_handler![
        app_paths::app_paths_roots,
        app_cleanup::app_cleanup_status,
        app_cleanup::app_cleanup_request,
        app_cleanup::app_cleanup_resume,
        app_cleanup::app_cleanup_cancel,
        external_storage::connection_commands::external_storage_list_providers,
        external_storage::connection_commands::external_storage_prepare_connection,
        external_storage::connection_commands::external_storage_prepare_renewal,
        external_storage::connection_commands::external_storage_change_endpoint,
        external_storage::connection_commands::external_storage_unlock_connection,
        external_storage::connection_commands::external_storage_commit_connection,
        external_storage::connection_commands::external_storage_begin_authorization,
        external_storage::connection_commands::external_storage_complete_authorization,
        external_storage::connection_commands::external_storage_cancel_authorization,
        external_storage::connection_commands::external_storage_list_folders,
        external_storage::connection_commands::external_storage_select_folder,
        external_storage::connection_commands::external_storage_cancel_folder_selection,
        external_storage::connection_commands::external_storage_validate_sync_root,
        external_storage::lww_commands::external_lww_inspect,
        external_storage::lww_commands::external_lww_stage_binding,
        external_storage::lww_commands::external_lww_publish,
        external_storage::lww_commands::external_lww_receive,
        external_storage::lww_commands::external_lww_maintenance,
        external_storage::lww_commands::external_lww_fence,
        external_storage::lww_commands::external_lww_resume,
        external_storage::lww_commands::external_lww_prepare_new_device,
        external_storage::connection_commands::external_storage_set_retention_policy,
        external_storage::connection_commands::external_storage_set_transfer_concurrency,
        external_storage::connection_commands::external_storage_remove_connection,
        external_storage::connection_commands::external_storage_begin_connection_settings_export,
        external_storage::connection_commands::external_storage_save_connection_settings_file,
        external_storage::connection_commands::external_storage_prepare_connection_settings_import,
        external_storage::runtime::external_storage_get_state,
        external_storage::runtime::external_storage_capture_exit_target,
        external_storage::runtime::external_storage_set_execution_session,
        external_storage::snapshot_export_commands::external_storage_cancel_export,
        external_storage::runtime::external_storage_set_sync_target,
        external_storage::runtime::external_storage_set_sync_paused,
        external_storage::runtime::external_storage_set_automatic_backup_paused,
        external_storage::runtime::external_storage_start_job,
        external_storage::runtime::external_storage_get_job,
        external_storage::runtime_restore::external_storage_confirm_restore_adoption,
        external_storage::runtime::external_storage_cancel_job,
        external_storage::runtime::external_storage_stop_restore,
        external_storage::runtime::external_storage_get_quota,
        external_storage::history::external_storage_list_history,
        external_storage::history_deletion::external_storage_prepare_history_delete,
        external_storage::snapshot_export_commands::external_storage_export_snapshot,
        #[cfg(target_os = "ios")]
        ios_lifecycle::ios_prepare_restart,
        #[cfg(target_os = "macos")]
        macos_lifecycle::macos_lifecycle_ready,
        #[cfg(target_os = "macos")]
        macos_lifecycle::macos_exit_response,
        #[cfg(desktop)]
        opened_files::desktop_relaunch,
        #[cfg(any(windows, target_os = "linux"))]
        desktop_session::desktop_flush_complete,
        #[cfg(any(windows, target_os = "linux"))]
        renderer_recovery::desktop_close_ack,
        #[cfg(not(target_os = "android"))]
        renderer_recovery::renderer_recovery_take,
        native_startup_status,
        app_update::commands::app_update_environment,
        app_update::commands::app_update_check,
        app_update::commands::app_update_cancel,
        app_update::commands::app_update_install,
        app_update::commands::app_update_stage_deb,
        native_media::streaming::native_media_base_url,
        native_media::streaming::native_media_ensure,
        server_sync::commands::server_sync_asset_status,
        server_sync::commands::asset_residency_connection_objects,
        server_sync::commands::server_sync_status,
        server_sync::commands::server_sync_configure,
        server_sync::commands::server_sync_lww_push,
        server_sync::commands::server_sync_lww_pull,
        server_sync::commands::server_sync_lww_ack,
        server_sync::commands::server_sync_lww_fence,
        server_sync::commands::server_sync_lww_activate,
        server_sync::commands::server_sync_lww_hydrate,
        server_sync::commands::server_sync_lww_pending_count,
        server_sync::commands::server_sync_progress,
        server_sync::commands::server_sync_lww_inspect,
        server_sync::commands::server_sync_lww_pending_binding,
        server_sync::commands::server_sync_lww_stage_target,
        server_sync::commands::server_sync_lww_prepare_new_device,
        server_sync::commands::server_sync_lww_prepare_fresh_writer,
        server_sync::commands::server_sync_lww_activate_new_device,
        server_sync::commands::server_sync_lww_retry,
        server_sync::commands::server_sync_lww_drain,
        server_sync::commands::server_sync_cancel,
        server_sync::commands::server_sync_claim_offline_notice,
        server_sync::commands::server_sync_notify_start,
        server_sync::commands::server_sync_notify_stop,
        server_sync::commands::server_sync_asset_policy,
            server_sync::commands::asset_residency_download_remote,
            server_sync::commands::asset_residency_prepare_download,
        server_sync::commands::server_sync_asset_evict,
        server_sync::commands::server_sync_cache_usage,
        server_sync::commands::server_sync_cache_cleanup,
        greet,
        boot_attempt_begin,
        boot_attempt_complete,
        native_request,
        #[cfg(desktop)]
        opened_files::opened_files_take,
        #[cfg(desktop)]
        appimage_integration::appimage_integration_state,
        #[cfg(desktop)]
        appimage_integration::appimage_integration_register,
        persistent_store::commands::pds_storage_stats,
        persistent_store::commands::pds_snapshot_delete,
        persistent_store::commands::pds_asset_gc_preview,
        persistent_store::commands::pds_asset_gc_execute,
        persistent_store::commands::data_health::pds_data_health_scan,
        persistent_store::commands::data_health::pds_data_health_deep_scan,
        persistent_store::commands::data_health::pds_data_health_result,
        persistent_store::commands::data_health::pds_data_health_cancel,
        persistent_store::commands::data_health::pds_data_health_repair_plan,
        persistent_store::commands::data_health::pds_data_health_repair_preview,
        persistent_store::commands::data_health::pds_data_health_repair_apply,
        persistent_store::commands::data_health::pds_data_health_discard_intent,
        persistent_store::commands::data_health::pds_data_health_complete_intent,
        persistent_store::commands::data_health::pds_data_health_journals,
        persistent_store::commands::data_health::pds_data_health_undo,
        network_log::network_log_command,
        native_log::native_log_tail,
        diagnostics::diagnostic_device_info,
        native_log::native_log_error,
        generation_diagnostics::native_generation_diagnostic,
        native_log::native_log_file_path,
        native_log::native_log_set_file_enabled,
        native_tokenizer::tokenize_batch,
        native_media::native_media_encode_inlay_image,
        native_media::ipc::native_media_inlay_input_open,
        native_media::ipc::native_media_inlay_input_chunk,
        native_media::ipc::native_media_inlay_input_chunk_raw,
        native_media::ipc::native_media_inlay_input_cancel,
        native_media::ipc::native_media_encode_inlay_finish,
        native_media::ipc::native_media_inlay_output_read,
        native_media::ipc::native_media_inlay_output_cancel,
        asset_repository::commands::asset_cas_read_bytes,
        asset_repository::commands::asset_cas_stat_object,
            asset_repository::commands::asset_cas_stat_objects,
            asset_repository::commands::asset_remote_stat_objects,
        asset_repository::commands::asset_remote_stat_object,
        asset_repository::commands::asset_remote_hydrate_object,
        asset_repository::commands::asset_cas_job_begin,
        asset_repository::commands::asset_cas_job_prepare,
        asset_repository::commands::asset_cas_job_upload_open,
        asset_repository::commands::asset_cas_job_upload_chunk,
        asset_repository::commands::asset_cas_job_upload_chunk_raw,
        asset_byte_transport::native_asset_bytes_probe,
        asset_repository::commands::asset_cas_job_upload_finish,
        asset_repository::commands::asset_cas_job_upload_cancel,
        asset_repository::commands::asset_cas_job_pin_existing,
        asset_repository::commands::asset_cas_job_seal,
        asset_repository::commands::asset_cas_job_finalize_content,
        asset_repository::commands::asset_cas_job_seal_prepared_content,
        asset_repository::commands::asset_cas_job_release,
        native_file_jobs::native_file_job_start,
        device_backup::native_device_backup_bootstrap,
        device_backup::native_device_backup_recovery_complete,
        native_file_jobs::native_content_source_metadata,
        native_file_jobs::native_file_job_status,
        native_file_jobs::native_file_job_prepared_content,
        native_file_jobs::native_file_job_stage_inline_asset,
        native_file_jobs::native_file_job_list,
        native_file_jobs::native_file_job_finalize,
        native_file_jobs::native_file_job_cancel,
        native_file_jobs::native_file_job_official_publication_retry,
        native_file_jobs::native_file_job_forget,
        native_file_jobs::official_assets::native_official_asset_restore_enqueue,
        native_file_jobs::official_assets::native_official_asset_restore_settled,
        native_file_jobs::native_portable_handoff_cleanup,
        native_file_jobs::native_raw_recovery_handoff_cleanup,
        native_file_jobs::native_portable_select_sections,
        native_file_jobs::native_portable_confirm_restore_adoption,
        native_file_jobs::native_portable_retry_restore_bodies,
        native_file_jobs::native_snapshot_restore_bodies_start,
        native_file_jobs::native_snapshot_restore_bodies_status,
        native_file_jobs::native_plugin_values_assign,
        native_file_jobs::native_backup_source_format,
        native_file_jobs::native_conflict_backup_source,
        native_file_jobs::native_portable_source_discard,
        native_file_jobs::native_portable_source_cleanup_orphans,
        native_file_jobs::native_legacy_backup_handoff_cleanup,
        native_file_jobs::native_character_charx_handoff_cleanup,
        native_file_jobs::native_character_card_handoff_cleanup,
        native_file_jobs::native_risu_module_handoff_cleanup,
        native_file_jobs::native_dataset_export_handoff_cleanup,
        native_file_jobs::download_handoff::native_download_handoff_create,
        native_file_jobs::download_handoff::native_download_handoff_append,
        native_file_jobs::download_handoff::native_download_handoff_cleanup,
        native_file_jobs::screenshot_output::native_file_job_screenshot_output_start,
        native_file_jobs::screenshot_output::native_file_job_screenshot_output_append,
        native_file_jobs::screenshot_output::native_file_job_screenshot_output_publish,
        native_file_jobs::screenshot_output::native_file_job_screenshot_output_cancel,
        native_file_jobs::screenshot_output::native_file_job_screenshot_output_release,
        persistent_store::sync_selection::pds_lww_binding_content,
        persistent_store::sync_selection::pds_lww_binding_state,
        persistent_store::sync_selection::pds_lww_switch_target,
        persistent_store::sync_selection::pds_lww_replace_from_target,
        persistent_store::commands::pds_open,
        persistent_store::commands::pds_asset_gc_maintenance,
        persistent_store::commands::pds_message_object_sweep,
        persistent_store::commands::pds_read_root,
        persistent_store::commands::pds_read_revision,
            persistent_store::commands::pds_read_library_identity,
        persistent_store::commands::pds_query_presets,
        persistent_store::commands::pds_read_preset,
        persistent_store::commands::pds_query_characters,
        persistent_store::commands::pds_read_character,
        persistent_store::commands::pds_query_conversations,
        persistent_store::commands::pds_read_conversation,
        persistent_store::commands::pds_read_conversation_metadata,
        persistent_store::commands::pds_read_conversation_window,
        persistent_store::commands::pds_read_conversation_message_metadata_window,
        persistent_store::commands::pds_query_plugin_storage,
        persistent_store::commands::pds_list_plugin_storage,
        persistent_store::commands::pds_read_plugin_storage_page,
        persistent_store::commands::pds_read_plugin_storage,
        persistent_store::commands::hypa::pds_read_hypa_embeddings,
        persistent_store::commands::hypa::pds_write_hypa_embeddings,
        persistent_store::commands::hypa::pds_hypa_embedding_usage,
        persistent_store::commands::hypa::pds_clear_hypa_embeddings,
        persistent_store::commands::pds_read_asset_alias,
        persistent_store::commands::pds_read_asset_aliases_by_keys,
        persistent_store::commands::pds_list_asset_aliases,
        persistent_store::commands::gallery::pds_gallery_page,
        persistent_store::commands::gallery::pds_gallery_keys,
        persistent_store::commands::gallery::pds_gallery_index,
        persistent_store::commands::gallery::pds_gallery_missing_keys,
        persistent_store::commands::gallery::pds_gallery_delete,
        persistent_store::commands::gallery::pds_gallery_scan_start,
        persistent_store::commands::gallery::pds_gallery_scan_status,
        persistent_store::commands::gallery::pds_gallery_scan_result,
        persistent_store::commands::gallery::pds_gallery_scan_cancel,
        persistent_store::commands::gallery::pds_gallery_locations,
        persistent_store::commands::gallery::pds_gallery_usage_count,
        persistent_store::commands::pds_read_asset_owner_head,
        persistent_store::commands::pds_commit_asset_alias,
        persistent_store::commands::pds_delete_asset_alias,
        persistent_store::commands::pds_commit,
        #[cfg(target_os = "android")]
        android_commit_transport::pds_commit_android_open,
        #[cfg(target_os = "android")]
        android_commit_transport::pds_commit_android_chunk,
        #[cfg(target_os = "android")]
        android_commit_transport::pds_commit_android_finish,
        #[cfg(target_os = "android")]
        android_commit_transport::pds_commit_android_cancel,
        #[cfg(target_os = "android")]
        android_commit_transport::pds_replace_android_finish,
        #[cfg(target_os = "android")]
        android_commit_transport::pds_plugin_part_android_finish,
        #[cfg(target_os = "android")]
        android_commit_transport::pds_plugin_write_android_finish,
        #[cfg(any(windows, target_os = "linux", target_os = "ios", target_os = "macos"))]
        persistent_commit_raw::pds_commit_raw,
        persistent_store::lww_commands::pds_lww_read_outbox,
        persistent_store::lww_commands::pds_lww_finish_initial_publication,
        persistent_store::lww_commands::pds_lww_stage_receive,
        persistent_store::lww_commands::pds_lww_apply_receive,
        persistent_store::lww_commands::pds_lww_finish_receive,
        persistent_store::lww_commands::pds_lww_drain_deferred,
        persistent_store::lww_commands::pds_lww_commit_replacement,
        persistent_store::lww_commands::pds_lww_replace_target_as_new_device,
        #[cfg(windows)]
        persistent_commit_transport::pds_commit_shared_open,
        #[cfg(windows)]
        persistent_commit_transport::pds_commit_shared_chunk,
        #[cfg(windows)]
        persistent_commit_transport::pds_commit_shared_finish,
        #[cfg(windows)]
        persistent_commit_transport::pds_commit_shared_cancel,
        persistent_store::commands::pds_archive_preview,
        persistent_store::commands::pds_archive_character,
        persistent_store::commands::pds_restore_character,
        persistent_store::commands::pds_cancel_character_archive_operation,
        persistent_store::commands::pds_replace_begin,
        persistent_store::commands::pds_replace_put_root,
        persistent_store::commands::pds_replace_put_presets,
        persistent_store::commands::pds_replace_add_presets,
        persistent_store::commands::pds_replace_add_plugin_storage_values,
        persistent_store::commands::pds_replace_add_plugin_storage,
        persistent_store::commands::pds_replace_put_character_detail,
        persistent_store::commands::pds_replace_put_conversation,
        persistent_store::commands::pds_replace_add_conversation_messages,
        persistent_store::commands::pds_replace_add_characters,
        persistent_store::commands::pds_replace_put_asset_aliases,
        persistent_store::commands::pds_replace_put_asset_owner_heads,
        persistent_store::commands::pds_replace_preserve_repositories,
        persistent_store::commands::pds_replace_commit,
        persistent_store::commands::pds_replace_abort,
        persistent_store::commands::pds_materialize,
        persistent_store::commands::pds_acquire_revision,
        persistent_store::commands::pds_release_revision,
        persistent_store::commands::pds_export_risu_save,
        persistent_store::commands::pds_export_risu_save_cleanup,
        #[cfg(feature = "official-publication-upload-pilot")]
        persistent_store::commands::official_publication_upload_file,
        #[cfg(feature = "native-kei-upload-pilot")]
        persistent_store::commands::pds_kei_backup_upload,
        persistent_store::commands::pds_checkpoint,
        persistent_store::commands::pds_snapshot_create,
        persistent_store::commands::pds_snapshot_list,
        persistent_store::commands::pds_get_device_setting,
        persistent_store::commands::pds_set_device_setting,
        persistent_store::commands::pds_patch_device_setting,
            persistent_store::commands::pds_read_device_settings,
            persistent_store::commands::image_geometry::pds_read_image_geometry,
            persistent_store::commands::image_geometry::pds_write_image_geometry,
            persistent_store::commands::image_geometry::pds_compute_image_geometry,
        persistent_store::commands::pds_read_plugin_permissions,
        persistent_store::commands::pds_write_plugin_permission,
        persistent_store::commands::pds_write_plugin_permission_grant,
        persistent_store::commands::pds_clear_plugin_permissions,
        persistent_store::commands::pds_hydrate_plugin_device_storage,
        persistent_store::commands::pds_read_plugin_device_value,
        persistent_store::commands::pds_read_plugin_device_range,
        persistent_store::commands::pds_open_plugin_device_write,
        persistent_store::commands::pds_open_plugin_device_binary_write,
        persistent_store::commands::pds_append_plugin_device_bytes,
        persistent_store::commands::pds_append_plugin_device_bytes_raw,
        persistent_store::commands::pds_read_plugin_device_bytes,
        persistent_store::commands::pds_append_plugin_device_write,
        persistent_store::commands::pds_abort_plugin_device_write,
        persistent_store::commands::pds_list_plugin_device_keys,
        persistent_store::commands::pds_list_plugin_device_storage,
        persistent_store::commands::pds_write_plugin_device_values,
        persistent_store::commands::pds_plugin_device_sequence,
        persistent_store::commands::pds_begin_plugin_claim_session,
        persistent_store::commands::pds_claim_plugin_storage_value,
        persistent_store::commands::pds_close_plugin_claim_session,
        persistent_store::commands::pds_close_plugin_claim_eligibility,
        persistent_store::commands::pds_colliding_plugin_storage_keys,
        persistent_store::commands::pds_assign_plugin_storage,
        account_credential::account_credential_read,
        account_credential::account_credential_write,
        account_credential::account_credential_clear,
        #[cfg(any(
            target_os = "windows",
            target_os = "android",
            target_os = "linux",
            target_os = "ios",
            target_os = "macos"
        ))]
        regex_shadow::regex_execute_batch,
        #[cfg(any(
            target_os = "windows",
            target_os = "android",
            target_os = "linux",
            target_os = "ios",
            target_os = "macos"
        ))]
        regex_shadow::regex_cancel_batch,
        #[cfg(windows)]
        windows_appearance::windows_set_appearance,
        #[cfg(desktop)]
        desktop_notifications::desktop_notify,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        startup_appearance::desktop_cache_appearance,
        persistent_store::commands::pds_read_section_participation,
        persistent_store::commands::pds_set_section_participating,
        persistent_store::commands::pds_read_character_summary,
        persistent_store::commands::pds_working_set_change_window,
        persistent_store::commands::pds_working_set_change_page,
        persistent_store::commands::pds_commit_working_set_change_cursor,
    ]);
    move |invoke: tauri::ipc::Invoke<tauri::Wry>| {
        if app_cleanup::pending(invoke.message.webview_ref().app_handle())
            && !invoke.message.command().starts_with("app_cleanup_") {
            invoke.resolver.reject("cleanup-pending");
            return true;
        }
        handler(invoke)
    }
}

pub fn handle_run_event(_app: &tauri::AppHandle, _event: tauri::RunEvent) {
    if matches!(&_event, tauri::RunEvent::Exit) {
        if let Some(recorder) = risunest_network_log::global() { recorder.shutdown(); }
    }
    if app_cleanup::closing(_app) { return; }
    #[cfg(desktop)]
    if matches!(&_event, tauri::RunEvent::Exit) { opened_files::restart_on_exit(_app); }
    #[cfg(any(windows, target_os = "linux"))]
    if let tauri::RunEvent::WindowEvent { label, event: tauri::WindowEvent::CloseRequested { api, .. }, .. } = &_event {
        if label == "main" {
            cancel_incomplete_boot(_app);
            renderer_recovery::on_main_close_requested(_app, api);
        }
    }
    #[cfg(target_os = "ios")]
    if let tauri::RunEvent::Opened { urls } = &_event {
        use tauri_plugin_ios_native::IosNativeExt;
        let app = _app.clone();
        let files = urls.iter().filter(|url| url.scheme() == "file")
            .map(ToString::to_string).collect::<Vec<_>>();
        if !files.is_empty() {
            tauri::async_runtime::spawn(async move {
                app.ios_native().receive_opened_files(files).await;
            });
        }
    }
    #[cfg(target_os = "macos")]
    macos_lifecycle::handle_run_event(_app, _event);
}

fn cancel_incomplete_boot(app: &tauri::AppHandle) {
    if let Ok(root) = boot_marker_root(app) {
        if let Err(error) = boot_marker::cancel_if_incomplete(&root) {
            crate::nlog!("warn", "startup cancellation failed: {error}");
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    native_log::install_panic_hook();
    // The profile decides every root and identity, so it is settled before any
    // of them is resolved. A malformed option never falls back to the default.
    #[cfg(desktop)]
    let (profile, arguments) = match launch_profile::split(std::env::args_os().skip(1)) {
        Ok(selection) => selection,
        Err(error) => {
            launch_profile::report_rejected_option(&error);
            std::process::exit(2);
        }
    };
    let mut context = tauri::generate_context!();
    #[cfg(desktop)]
    let profiles = app_paths::DesktopProfiles::new(context.config());
    #[cfg(desktop)]
    if let Some(window) = context.config_mut().app.windows.iter_mut().find(|window| window.label == "main") {
        window.visible = false;
        if let Some(profile) = &profile {
            window.title = launch_profile::window_title(&window.title, profile);
        }
    }
    #[cfg(desktop)]
    let paths = profiles.manifest(profile.as_ref())
        .expect("application data roots unavailable");
    #[cfg(desktop)]
    if let Some(code) = app_cleanup::run_cli(&arguments, &profiles, profile.as_ref()) {
        std::process::exit(code);
    }
    #[cfg(target_os = "macos")]
    let data_store = profile.as_ref().map(|profile| launch_profile::webview_store(profiles.agent(), profile));
    #[cfg(all(desktop, not(target_os = "macos")))]
    let data_store = None;
    #[cfg(desktop)]
    let main_window = app_paths::take_main_window(&mut context, &paths, data_store)
        .expect("desktop WebView data directory unavailable");
    #[cfg(not(desktop))]
    let main_window = None;
    #[cfg(desktop)]
    let instance_id = launch_profile::instance_id(profiles.identifier(), profiles.agent(), profile.as_ref());
    #[cfg(not(desktop))]
    let instance_id = None;
    #[allow(unused_mut)]
    let mut builder = builder_with_main_window(main_window, instance_id);
    #[cfg(desktop)]
    {
        builder = builder.plugin(window_state_plugin(&paths.data));
    }
    #[cfg(desktop)]
    {
        builder = builder
            .manage(paths.prepared().expect("application data root unavailable"))
            .manage(launch_profile::LaunchProfile(profile));
    }
    match builder.build(context) {
        Ok(app) => app.run(handle_run_event),
        Err(error) => {
            // Cleanup lock contention is an expected refusal while its owner runs.
            // Other prerequisite failures still terminate, with a native diagnostic.
            native_log::record_startup_failure(&error.to_string());
            std::process::exit(startup_failure_exit_code(&error));
        }
    }
}

fn startup_failure_exit_code(error: &tauri::Error) -> i32 {
    match error {
        tauri::Error::Setup(error) if error.to_string() == "cleanup-app-still-running" => 0,
        _ => 1,
    }
}

fn header_map_to_json(header_map: &HeaderMap) -> serde_json::Value {
    let mut map = HashMap::new();
    for (key, value) in header_map {
        map.insert(
            key.as_str().to_string(),
            String::from_utf8_lossy(value.as_bytes()).into_owned(),
        );
    }
    json!(map)
}

#[cfg(test)]
mod header_map_tests {
    use super::*;

    #[test]
    fn cleanup_owner_refusal_is_a_logged_exit_not_a_panic() {
        let setup = |message: &str| tauri::Error::Setup(Box::<dyn std::error::Error>::from(message).into());
        assert_eq!(startup_failure_exit_code(&setup("cleanup-app-still-running")), 0);
        assert_eq!(startup_failure_exit_code(&setup("cleanup-lock-unavailable")), 1);
        assert_eq!(startup_failure_exit_code(&setup("invalid native path")), 1);
    }

    #[test]
    fn ipc_read_guard_script_carries_its_target_flag() {
        let script = ipc_read_guard_script();
        assert!(!script.contains("__RISUNEST_ANDROID__"));
        let flag = format!("const android = {};", cfg!(target_os = "android"));
        assert!(script.contains(&flag));
    }
    use reqwest::header::{HeaderName, HeaderValue};

    #[test]
    fn header_map_to_json_does_not_panic_on_non_ascii_header_values() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("content-disposition"),
            HeaderValue::from_bytes("attachment; filename=\"캐릭터.png\"".as_bytes()).unwrap(),
        );
        headers.insert(
            HeaderName::from_static("x-latin1"),
            HeaderValue::from_bytes(&[0xE9, 0x74, 0xE9]).unwrap(),
        );
        let json = header_map_to_json(&headers);
        assert_eq!(
            json["content-disposition"],
            "attachment; filename=\"캐릭터.png\""
        );
        // Invalid UTF-8 bytes degrade to replacement characters instead of panicking.
        assert_eq!(json["x-latin1"], "\u{FFFD}t\u{FFFD}");
    }

    #[test]
    fn native_startup_state_keeps_the_first_bounded_failure() {
        let state = NativeStartupState::default();
        assert_eq!(state.ensure_ready(), Ok(()));

        state.record_failure("synthetic setup failure", None, None);
        state.record_failure("later failure", None, None);

        assert_eq!(
            state.ensure_ready(),
            Err("Native setup failed: synthetic setup failure".to_owned())
        );

        let bounded = NativeStartupState::default();
        bounded.record_failure("x".repeat(MAX_NATIVE_STARTUP_ERROR_CHARS + 10), None, None);
        let error = bounded.ensure_ready().unwrap_err();
        assert_eq!(
            error.chars().count(),
            "Native setup failed: ".chars().count() + MAX_NATIVE_STARTUP_ERROR_CHARS
        );
    }

    #[test]
    fn native_startup_failure_retains_persistent_and_file_admission_gates() {
        let directory = tempfile::tempdir().unwrap();
        let persistent = persistent_store::PersistentStoreState::default();
        let native_files =
            native_file_jobs::NativeFileJobState::initialize(directory.path().into());
        let native_file_gate = native_files.admission.file(true).unwrap();
        let persistent_gate = persistent.acquire_device_maintenance().unwrap();
        let startup = NativeStartupState::default();

        startup.record_failure(
            "synthetic gated failure",
            Some(persistent_gate),
            Some(native_file_gate),
        );

        assert!(persistent.admit_renderer_operation().is_err());
        assert!(native_files.admission.file(false).is_err());
        assert!(native_files.admission.server().is_err());
        assert_eq!(
            startup.ensure_ready(),
            Err("Native setup failed: synthetic gated failure".to_owned())
        );
    }
}
