mod account_credential;
#[cfg(any(test, target_os = "android"))]
mod android_commit_transport;
mod app_data_root;
mod app_update;
#[cfg(desktop)]
mod appimage_integration;
mod asset_repository;
mod boot_marker;
mod cold_payload_codec;
mod data_health;
pub(crate) mod device_backup;
mod external_storage;
pub mod import_export_jobs;
#[cfg(target_os = "ios")]
mod ios_lifecycle;
#[allow(dead_code)]
mod local_backup;
mod logical_records;
#[allow(dead_code)]
mod lossless_f0;
#[cfg(any(test, target_os = "macos"))]
mod macos_lifecycle;
pub mod native_file_jobs;
pub(crate) mod native_log;
mod native_media;
mod native_tokenizer;
#[cfg(desktop)]
mod opened_files;
#[cfg(any(windows, target_os = "linux", target_os = "ios", target_os = "macos"))]
mod persistent_commit_raw;
#[cfg(windows)]
mod persistent_commit_transport;
mod persistent_store;
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
mod server_sync;
mod trust_boundary;
#[cfg(test)]
mod test_memory;
#[cfg(windows)]
mod windows_appearance;

use base64::{engine::general_purpose, Engine as _};
use oauth2::basic::{BasicClient, BasicErrorResponseType, BasicTokenType};
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EmptyExtraTokenFields,
    EndpointNotSet, EndpointSet, PkceCodeChallenge, RedirectUrl, RevocationErrorResponseType,
    Scope, StandardErrorResponse, StandardRevocableToken, StandardTokenIntrospectionResponse,
    StandardTokenResponse, TokenResponse, TokenUrl,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Listener, Manager};

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

    pub(crate) fn ensure_ready(&self) -> Result<(), String> {
        let failure = self
            .0
            .lock()
            .map_err(|error| format!("native startup status lock failed: {error}"))?;
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
    app_data_root::resolve(app).map_err(|error| {
        format!("failed to resolve application data directory: {error}")
    })
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
    let version = app.package_info().version.to_string();
    let root = boot_marker_root(&app)?;
    boot_marker::begin(&root, &version, boot_marker_now())
        .map_err(|error| format!("failed to record the start attempt: {error}"))
}

/// Records that the start finished, so the next one begins from zero.
#[tauri::command(async)]
fn boot_attempt_complete(app: tauri::AppHandle) -> Result<(), String> {
    let root = boot_marker_root(&app)?;
    boot_marker::complete(&root)
        .map_err(|error| format!("failed to clear the start attempt: {error}"))
}

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[tauri::command]
async fn native_request(url: String, body: String, header: String, method: String) -> String {
    let headers_json: Value = match serde_json::from_str(&header) {
        Ok(h) => h,
        Err(e) => return format!(r#"{{"success":false,"body":"{}"}}"#, e.to_string()),
    };

    let mut headers = HeaderMap::new();
    let method = method.to_string();
    if let Some(obj) = headers_json.as_object() {
        for (key, value) in obj {
            let header_name = match HeaderName::from_bytes(key.as_bytes()) {
                Ok(name) => name,
                Err(e) => return format!(r#"{{"success":false,"body":"{}"}}"#, e.to_string()),
            };
            let header_value = match HeaderValue::from_str(value.as_str().unwrap_or("")) {
                Ok(value) => value,
                Err(e) => return format!(r#"{{"success":false,"body":"{}"}}"#, e.to_string()),
            };
            headers.insert(header_name, header_value);
        }
    } else {
        return format!(r#"{{"success":false,"body":"Invalid header JSON"}}"#);
    }

    let client = reqwest::Client::new();
    let response: Result<reqwest::Response, reqwest::Error>;

    if method == "POST" {
        response = client
            .post(&url)
            .headers(headers)
            .timeout(Duration::from_secs(120))
            .body(body)
            .send()
            .await;
    } else {
        response = client
            .get(&url)
            .headers(headers)
            .timeout(Duration::from_secs(120))
            .send()
            .await;
    }

    match response {
        Ok(resp) => {
            let headers = resp.headers();
            let header_json = header_map_to_json(headers);
            let status = resp.status().as_u16().to_string();
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => return format!(r#"{{"success":false,"body":"{}"}}"#, e.to_string()),
            };
            let encoded = general_purpose::STANDARD.encode(&bytes);

            format!(
                // r#"{{"success":true,"body":"{}","headers":{}}}"#,
                r#"{{"success":true,"body":"{}","headers":{},"status":{}}}"#,
                encoded, header_json, status
            )
        }
        Err(e) => format!(
            r#"{{"success":false,"body":"{}","status":400}}"#,
            e.to_string()
        ),
    }
}

fn get_oauth_client() -> oauth2::Client<
    StandardErrorResponse<BasicErrorResponseType>,
    StandardTokenResponse<EmptyExtraTokenFields, BasicTokenType>,
    StandardTokenIntrospectionResponse<EmptyExtraTokenFields, BasicTokenType>,
    StandardRevocableToken,
    StandardErrorResponse<RevocationErrorResponseType>,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointSet,
> {
    let auth_url = AuthUrl::new("http://authorize".to_string()).unwrap();
    let token_url = TokenUrl::new("http://token".to_string()).unwrap();
    let redirection_url = RedirectUrl::new("http://redirect".to_string()).unwrap();
    let client = BasicClient::new(ClientId::new("client_id".to_string()))
        .set_client_secret(ClientSecret::new("client_secret".to_string()))
        .set_auth_uri(auth_url)
        .set_token_uri(token_url)
        // Set the URL the user will be redirected to after the authorization process.
        .set_redirect_uri(redirection_url);

    return client;
}

#[tauri::command]
async fn oauth_login(app: AppHandle) -> Result<String, String> {
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let client = get_oauth_client();
    // Write pkce_verifier to a file or session for later use.
    std::fs::write("pkce_verifier.txt", pkce_verifier.secret()).unwrap();

    let (auth_url, _csrf_token) = client
        .authorize_url(CsrfToken::new_random)
        // Set the desired scopes.
        .add_scope(Scope::new("read".to_string()))
        .add_scope(Scope::new("write".to_string()))
        // Set the PKCE code challenge.
        .set_pkce_challenge(pkce_challenge)
        .url();

    let http_client = oauth2::reqwest::ClientBuilder::new()
        // Following redirects opens the client up to SSRF vulnerabilities.
        .redirect(oauth2::reqwest::redirect::Policy::none())
        .build()
        .expect("Client should build");

    app.emit("oauth_open_url", auth_url.to_string()).unwrap();

    let auth_code = Arc::new(Mutex::new(String::new()));
    let auth_code_clone = Arc::clone(&auth_code);

    let handle = app.app_handle().clone();
    //promise
    app.listen("oauth_callback_event", move |event| {
        // Handle the event
        let mut code = auth_code_clone.lock().unwrap();
        *code = event.payload().to_string();
        handle.unlisten(event.id());
    });

    //wait for auth_code to be set
    loop {
        {
            let code = auth_code.lock().unwrap();
            if !code.is_empty() {
                break;
            }
        } // MutexGuard is dropped here before await
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // Now you can trade it for an access token.
    let auth_code_value = auth_code.lock().unwrap().clone();
    let token_result = client
        .exchange_code(AuthorizationCode::new(auth_code_value))
        // Set the PKCE code verifier.
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http_client)
        .await;

    return Ok(token_result.unwrap().access_token().secret().to_string());
}

#[tauri::command]
fn check_auth(fpath: String, auth: String) -> bool {
    //check file exists
    let path = Path::new(&fpath);
    if !path.exists() {
        crate::nlog!("warn", "authentication file does not exist");
        return false;
    }

    // check file is a file
    if !path.is_file() {
        crate::nlog!("warn", "authentication path is not a file");
        return false;
    }

    // check file size
    let size = std::fs::metadata(&fpath).unwrap().len();

    //check file size is less than 1000 bytes
    if size > 1000 {
        crate::nlog!("warn", "authentication file is too large");
        return false;
    }

    // read file, return false when error
    let got_auth = std::fs::read_to_string(&path);

    // check read error
    if got_auth.is_err() {
        crate::nlog!("warn", "authentication file could not be read");
        return false;
    } else {
        // check auth
        if got_auth.unwrap() != auth {
            crate::nlog!("warn", "authentication did not match");
            return false;
        }
        crate::nlog!("info", "authentication matched");
        return true;
    }
}

/// Product initialization shared by native entry points.
pub fn builder() -> tauri::Builder<tauri::Wry> {
    builder_with_main_window(None)
}

fn builder_with_main_window(
    main_window: Option<(tauri::utils::config::WindowConfig, std::path::PathBuf)>,
) -> tauri::Builder<tauri::Wry> {
    native_log::install_panic_hook();
    let native_log_state = native_log::global_state();
    let setup_native_log_state = native_log_state.clone();
    let native_startup_state = NativeStartupState::default();
    let setup_native_startup_state = native_startup_state.clone();
    let mut builder = tauri::Builder::default().manage(native_startup_state);
    #[cfg(desktop)]
    {
        // Reject a second process before plugins with startup side effects run.
        builder = builder
            .manage(opened_files::OpenedFilesState::from_launch_arguments())
            .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
                opened_files::deliver_single_instance_arguments(app, &args, &cwd);
            }));
    }
    #[cfg(target_os = "ios")]
    {
        builder = builder
            .append_invoke_initialization_script(include_str!("ios_ipc.js"))
            .manage(ios_lifecycle::RestartState::default())
            .plugin(tauri_plugin_opener::init());
    }
    #[cfg(not(target_os = "ios"))]
    {
        builder = builder.plugin(tauri_plugin_shell::init());
    }
    #[cfg(windows)]
    {
        use tauri_plugin_window_state::StateFlags;
        builder = builder
            .plugin(
                tauri_plugin_window_state::Builder::default()
                    .with_state_flags(
                        StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED,
                    )
                    .with_filter(|label| label == "main")
                    .build(),
            )
            .plugin(windows_appearance::init());
    }
    #[cfg(target_os = "macos")]
    {
        builder = builder.manage(macos_lifecycle::ExitState::default());
    }
    #[cfg(target_os = "android")]
    {
        builder = builder
            .manage(android_commit_transport::native_state().clone())
            .on_page_load(|webview, payload| {
                if webview.label() == "main"
                    && matches!(payload.event(), tauri::webview::PageLoadEvent::Started)
                {
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
                        if let Err(error) = state.reset_renderer_session() {
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
                    if let Err(error) = state.reset_renderer_session() {
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
                #[cfg(target_os = "ios")]
                ios_lifecycle::main_document_started(webview.app_handle());
                #[cfg(target_os = "macos")]
                macos_lifecycle::document_started(webview.app_handle());
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
                    if let Err(error) = state.reset_renderer_session() {
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
            }
        });
    }

    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    }

    builder
        .setup(move |app| {
            if let Some((config, data_directory)) = &main_window {
                tauri::WebviewWindowBuilder::from_config(app, config)?
                    .data_directory(data_directory.clone())
                    .build()?;
            }
            let setup_result = (|| -> Result<(), String> {
                #[cfg(target_os = "macos")]
                macos_lifecycle::install_native_quit(app.handle())?;
                #[cfg(any(target_os = "android", target_os = "ios"))]
                app.handle()
                    .plugin(tauri_plugin_barcode_scanner::init())
                    .map_err(|error| format!("barcode scanner initialization failed: {error}"))?;
                #[cfg(target_os = "ios")]
                app.handle()
                    .plugin(tauri_plugin_ios_native::init())
                    .map_err(|error| format!("iOS native initialization failed: {error}"))?;
                let app_data_dir = app_data_root::resolve(app)
                    .map_err(|error| format!("application data root unavailable: {error}"))?;
                app.state::<external_storage::job_store::JobCommandState>()
                    .root
                    .set(app_data_dir.clone())
                    .map_err(|_| "external storage root is already configured".to_string())?;
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
                setup_native_log_state.configure_file_path(&app_data_dir);
                app.manage(setup_native_log_state.clone());
                let state = native_file_jobs::NativeFileJobState::initialize(
                    app_data_dir.join("native-file-jobs"),
                );
                // Manage recovery state before acquiring either fence. If a later
                // startup step fails, an attached fence remains live while the
                // renderer shows the failure panel.
                app.manage(device_backup);
                app.manage(state);
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
                native_media::recover_inlay_writes(&app_data_dir)
                    .map_err(|error| format!("Inlay recovery failed: {error}"))?;
                app.manage(native_media::streaming::MediaServerState::initialize(
                    app_data_dir.clone(),
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
        .manage(server_sync::events::ServerSyncEventsState::default())
        .manage(external_storage::connection_commands::ConnectionCommandState::default())
        .manage(external_storage::job_store::JobCommandState::default())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(invoke_handler())
}

/// The product command router, reusable by alternative native entries.
pub fn invoke_handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        external_storage::connection_commands::external_storage_list_providers,
        external_storage::connection_commands::external_storage_prepare_connection,
        external_storage::connection_commands::external_storage_commit_connection,
        external_storage::connection_commands::external_storage_begin_authorization,
        external_storage::connection_commands::external_storage_complete_authorization,
        external_storage::connection_commands::external_storage_cancel_authorization,
        external_storage::connection_commands::external_storage_set_capture_policy,
        external_storage::connection_commands::external_storage_set_retention_policy,
        external_storage::connection_commands::external_storage_remove_connection,
        external_storage::connection_commands::external_storage_begin_connection_settings_export,
        external_storage::connection_commands::external_storage_save_connection_settings_file,
        external_storage::connection_commands::external_storage_prepare_connection_settings_import,
        external_storage::runtime::external_storage_get_state,
        external_storage::runtime::external_storage_capture_exit_target,
        external_storage::runtime::external_storage_set_execution_session,
        external_storage::runtime::external_storage_set_sync_target,
        external_storage::runtime::external_storage_start_job,
        external_storage::runtime::external_storage_get_job,
        external_storage::runtime::external_storage_cancel_job,
        external_storage::runtime::external_storage_get_quota,
        external_storage::history::external_storage_list_history,
        external_storage::history_deletion::external_storage_prepare_history_delete,
        external_storage::sync_engine::external_storage_list_conflicts,
        external_storage::sync_engine::external_storage_delete_conflict,
        external_storage::sync_engine::external_storage_recheck_conflict,
        native_file_jobs::reference_source::external_storage_open_conflict_source,
        native_file_jobs::reference_source::external_storage_release_conflict_source,
        external_storage::sync_engine::external_storage_apply_received,
        external_storage::snapshot_export_commands::external_storage_export_snapshot,
        #[cfg(target_os = "ios")]
        ios_lifecycle::ios_prepare_restart,
        #[cfg(target_os = "macos")]
        macos_lifecycle::macos_lifecycle_ready,
        #[cfg(target_os = "macos")]
        macos_lifecycle::macos_exit_response,
        native_startup_status,
        app_update::commands::app_update_environment,
        app_update::commands::app_update_check,
        app_update::commands::app_update_cancel,
        app_update::commands::app_update_install,
        app_update::commands::app_update_stage_deb,
        native_media::streaming::native_media_base_url,
        server_sync::commands::server_sync_status,
        server_sync::commands::server_sync_asset_status,
        server_sync::commands::server_sync_asset_policy,
        server_sync::commands::server_sync_asset_evict,
        server_sync::commands::server_sync_verified_bytes,
        server_sync::commands::server_sync_progress_counts,
        server_sync::commands::server_sync_retryable_failure,
        server_sync::commands::server_sync_backups,
        server_sync::commands::server_sync_backup_inventory,
        server_sync::commands::server_sync_backup_delete,
        server_sync::commands::server_sync_backup_cleanup,
        server_sync::commands::server_sync_backup_release,
        server_sync::commands::server_sync_cache_usage,
        server_sync::commands::server_sync_cache_cleanup,
        server_sync::commands::server_sync_backup_source,
        server_sync::commands::server_sync_bind,
        server_sync::commands::server_sync_reregister,
        server_sync::commands::server_sync_reconcile,
        server_sync::commands::server_sync_unbind,
        server_sync::commands::server_sync_prepare,
        server_sync::commands::server_sync_activate,
        server_sync::commands::server_sync_publish,
        server_sync::commands::server_sync_cancel,
        greet,
        boot_attempt_begin,
        boot_attempt_complete,
        native_request,
        check_auth,
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
        persistent_store::commands::data_health::pds_data_health_journals,
        persistent_store::commands::data_health::pds_data_health_undo,
        native_log::native_log_tail,
        native_log::native_log_error,
        native_log::native_log_file_path,
        native_log::native_log_set_file_enabled,
        oauth_login,
        native_tokenizer::tokenize_batch,
        native_media::native_media_write_inlay_image,
        native_media::native_media_encode_inlay_image,
        native_media::ipc::native_media_inlay_input_open,
        native_media::ipc::native_media_inlay_input_chunk,
        native_media::ipc::native_media_inlay_input_cancel,
        native_media::ipc::native_media_encode_inlay_finish,
        native_media::ipc::native_media_write_inlay_finish,
        native_media::ipc::native_media_inlay_output_read,
        native_media::ipc::native_media_inlay_output_cancel,
        asset_repository::commands::asset_cas_read_object_range,
        asset_repository::commands::asset_cas_stat_object,
        asset_repository::commands::asset_remote_stat_object,
        asset_repository::commands::asset_remote_read_object,
        asset_repository::commands::asset_cas_job_begin,
        asset_repository::commands::asset_cas_job_prepare,
        asset_repository::commands::asset_cas_job_upload_open,
        asset_repository::commands::asset_cas_job_upload_chunk,
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
        native_file_jobs::native_file_job_list,
        native_file_jobs::native_file_job_finalize,
        native_file_jobs::native_file_job_cancel,
        native_file_jobs::native_file_job_official_publication_retry,
        native_file_jobs::native_file_job_forget,
        native_file_jobs::native_portable_handoff_cleanup,
        native_file_jobs::native_raw_recovery_handoff_cleanup,
        native_file_jobs::native_portable_select_sections,
        native_file_jobs::native_plugin_values_assign,
        native_file_jobs::native_backup_source_format,
        native_file_jobs::native_legacy_backup_handoff_cleanup,
        native_file_jobs::native_character_charx_handoff_cleanup,
        native_file_jobs::native_character_card_handoff_cleanup,
        native_file_jobs::native_risu_module_handoff_cleanup,
        native_file_jobs::screenshot_output::native_file_job_screenshot_output_start,
        native_file_jobs::screenshot_output::native_file_job_screenshot_output_append,
        native_file_jobs::screenshot_output::native_file_job_screenshot_output_publish,
        native_file_jobs::screenshot_output::native_file_job_screenshot_output_cancel,
        native_file_jobs::screenshot_output::native_file_job_screenshot_output_release,
        persistent_store::commands::pds_open,
        persistent_store::commands::pds_asset_gc_maintenance,
        persistent_store::commands::pds_read_root,
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
        persistent_store::commands::pds_read_plugin_storage,
        persistent_store::commands::hypa::pds_read_hypa_embeddings,
        persistent_store::commands::hypa::pds_write_hypa_embeddings,
        persistent_store::commands::pds_read_asset_alias,
        persistent_store::commands::pds_read_asset_aliases_by_keys,
        persistent_store::commands::pds_list_asset_aliases,
        persistent_store::commands::pds_read_asset_repository_authority,
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
        #[cfg(any(windows, target_os = "linux", target_os = "ios", target_os = "macos"))]
        persistent_commit_raw::pds_commit_raw,
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
        persistent_store::commands::pds_replace_add_characters,
        persistent_store::commands::pds_replace_put_asset_aliases,
        persistent_store::commands::pds_replace_put_asset_owner_heads,
        persistent_store::commands::pds_replace_put_asset_repository_authority,
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
        persistent_store::commands::pds_snapshot_restore_request,
        persistent_store::commands::pds_get_device_setting,
        persistent_store::commands::pds_set_device_setting,
        persistent_store::commands::pds_patch_device_setting,
        persistent_store::commands::pds_read_device_settings,
        persistent_store::commands::pds_read_plugin_permissions,
        persistent_store::commands::pds_write_plugin_permission,
        persistent_store::commands::pds_write_plugin_permission_grant,
        persistent_store::commands::pds_clear_plugin_permissions,
        persistent_store::commands::pds_hydrate_plugin_device_storage,
        persistent_store::commands::pds_read_plugin_device_value,
        persistent_store::commands::pds_list_plugin_device_keys,
        persistent_store::commands::pds_list_plugin_device_storage,
        persistent_store::commands::pds_write_plugin_device_values,
        persistent_store::commands::pds_begin_plugin_claim_session,
        persistent_store::commands::pds_claim_plugin_storage_value,
        persistent_store::commands::pds_close_plugin_claim_session,
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
        persistent_store::commands::pds_read_section_participation,
        persistent_store::commands::pds_set_section_participating,
        persistent_store::commands::pds_read_character_summary,
        persistent_store::commands::pds_working_set_change_window,
        persistent_store::commands::pds_working_set_change_page,
        persistent_store::commands::pds_commit_working_set_change_cursor,
        server_sync::events::server_sync_events_start,
        server_sync::events::server_sync_events_stop,
    ]
}

pub fn handle_run_event(_app: &tauri::AppHandle, _event: tauri::RunEvent) {
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut context = tauri::generate_context!();
    #[cfg(desktop)]
    let main_window = app_data_root::take_main_window_with_webview_root(&mut context)
        .expect("desktop WebView data directory unavailable");
    #[cfg(not(desktop))]
    let main_window = None;
    builder_with_main_window(main_window)
        .build(context)
        .expect("error while building tauri application")
        .run(handle_run_event);
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
