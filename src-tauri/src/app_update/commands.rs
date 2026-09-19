use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use risunest_release_update::{
    compare_versions, MetadataLoader, PackageFormat, Product, Repository, VerifiedProduct,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

#[cfg(desktop)]
use tauri_plugin_updater::{Update, UpdaterExt};

use super::platform::{current_selection, InstallStrategy};
use super::transport::GithubTransport;

const PUBLIC_KEY: &str = match option_env!("RISUNEST_UPDATE_PUBLIC_KEY") {
    Some(value) => value,
    None => "",
};

pub(crate) struct AppUpdateState {
    loader: Option<MetadataLoader<GithubTransport>>,
    disabled_reason: Option<&'static str>,
    handles: Mutex<HashMap<String, UpdateHandle>>,
    active: Arc<AtomicBool>,
}

impl AppUpdateState {
    pub(crate) fn initialize(agent_build: bool) -> Self {
        let (loader, disabled_reason) = if agent_build {
            (None, Some("agent-build"))
        } else if PUBLIC_KEY.trim().is_empty() {
            (None, Some("not-configured"))
        } else {
            match GithubTransport::new().and_then(|transport| {
                MetadataLoader::new(Repository::risunest(), PUBLIC_KEY, transport)
                    .map_err(|error| error.to_string())
            }) {
                Ok(loader) => (Some(loader), None),
                Err(_) => (None, Some("invalid-configuration")),
            }
        };
        Self {
            loader,
            disabled_reason,
            handles: Mutex::new(HashMap::new()),
            active: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Clone)]
struct UpdateHandle {
    product: VerifiedProduct,
    download: risunest_release_update::Download,
    strategy: InstallStrategy,
    cancelled: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    #[cfg(desktop)]
    native_update: Option<Update>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateEnvironment {
    current_version: String,
    install_strategy: InstallStrategy,
    configured: bool,
    disabled_reason: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateCheckResult {
    status: &'static str,
    current_version: String,
    disabled_reason: Option<&'static str>,
    update: Option<AvailableUpdate>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AvailableUpdate {
    handle_id: String,
    version: String,
    pub_date: String,
    notes: String,
    localized_notes: std::collections::BTreeMap<String, String>,
    release_page: String,
    install_strategy: InstallStrategy,
    download_url: String,
    download_size: u64,
    format: Option<PackageFormat>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateProgress {
    handle_id: String,
    downloaded: u64,
    total: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StagedDeb {
    path: String,
    install_command: String,
}

#[tauri::command]
pub(crate) fn app_update_environment(
    app: AppHandle,
    state: tauri::State<'_, AppUpdateState>,
) -> UpdateEnvironment {
    UpdateEnvironment {
        current_version: app.package_info().version.to_string(),
        install_strategy: current_selection().strategy,
        configured: state.loader.is_some(),
        disabled_reason: state.disabled_reason,
    }
}

#[tauri::command]
pub(crate) async fn app_update_check(
    app: AppHandle,
    state: tauri::State<'_, AppUpdateState>,
) -> Result<UpdateCheckResult, String> {
    tokio::time::timeout(Duration::from_secs(30), run_app_update_check(app, state))
        .await
        .map_err(|_| "update check timed out".to_owned())?
}

async fn run_app_update_check(
    app: AppHandle,
    state: tauri::State<'_, AppUpdateState>,
) -> Result<UpdateCheckResult, String> {
    let _active = ActiveOperation::enter(&state)?;
    let current_version = app.package_info().version.to_string();
    let Some(loader) = state.loader.as_ref() else {
        return Ok(UpdateCheckResult {
            status: "disabled",
            current_version,
            disabled_reason: state.disabled_reason,
            update: None,
        });
    };
    let product = loader
        .load_latest(Product::App)
        .await
        .map_err(|error| error.to_string())?;
    if compare_versions(&product.release.version, &current_version)
        .map_err(|error| error.to_string())?
        != std::cmp::Ordering::Greater
    {
        return Ok(UpdateCheckResult {
            status: "current",
            current_version,
            disabled_reason: None,
            update: None,
        });
    }

    let selection = current_selection();
    let Some(request) = selection.request else {
        return Ok(UpdateCheckResult {
            status: "available",
            current_version,
            disabled_reason: None,
            update: Some(AvailableUpdate {
                handle_id: String::new(),
                version: product.release.version.clone(),
                pub_date: product.release.pub_date.clone(),
                notes: product.release.notes.clone(),
                localized_notes: product.release.localized_notes.clone(),
                release_page: product.release.release_page.clone(),
                install_strategy: InstallStrategy::Disabled,
                download_url: product.release.release_page.clone(),
                download_size: 0,
                format: None,
            }),
        });
    };
    let download = product
        .select_download(&request)
        .map_err(|error| error.to_string())?
        .clone();

    #[cfg(desktop)]
    let native_update = if matches!(
        selection.strategy,
        InstallStrategy::SelfInstall | InstallStrategy::StageDeb
    ) {
        let target = selection
            .target
            .as_deref()
            .ok_or("update target is missing")?;
        let entry = product
            .catalog
            .product(Product::App)
            .map_err(|error| error.to_string())?;
        let endpoint = url::Url::parse(&entry.manifest_url).map_err(|error| error.to_string())?;
        let updater = app
            .updater_builder()
            .pubkey(PUBLIC_KEY)
            .target(target)
            .endpoints(vec![endpoint])
            .map_err(|error| error.to_string())?
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| error.to_string())?;
        let checked = updater
            .check()
            .await
            .map_err(|error| error.to_string())?
            .ok_or("verified update was not returned by the native updater")?;
        verify_native_update(&product, &download, target, &checked)?;
        Some(checked)
    } else {
        None
    };

    let handle_id = uuid::Uuid::new_v4().to_string();
    let available = AvailableUpdate {
        handle_id: handle_id.clone(),
        version: product.release.version.clone(),
        pub_date: product.release.pub_date.clone(),
        notes: product.release.notes.clone(),
        localized_notes: product.release.localized_notes.clone(),
        release_page: product.release.release_page.clone(),
        install_strategy: selection.strategy,
        download_url: download.url.clone(),
        download_size: download.size,
        format: Some(download.format),
    };
    let handle = UpdateHandle {
        product,
        download,
        strategy: selection.strategy,
        cancelled: Arc::new(AtomicBool::new(false)),
        started: Arc::new(AtomicBool::new(false)),
        #[cfg(desktop)]
        native_update,
    };
    let mut handles = state.handles.lock().map_err(|error| error.to_string())?;
    handles.clear();
    handles.insert(handle_id, handle);
    Ok(UpdateCheckResult {
        status: "available",
        current_version,
        disabled_reason: None,
        update: Some(available),
    })
}

#[cfg(desktop)]
fn verify_native_update(
    product: &VerifiedProduct,
    download: &risunest_release_update::Download,
    target: &str,
    update: &Update,
) -> Result<(), String> {
    let platform = product
        .release
        .platform(target)
        .map_err(|error| error.to_string())?;
    let expected_json =
        serde_json::to_value(&product.release).map_err(|error| error.to_string())?;
    if update.version != product.release.version
        || update.target != target
        || update.download_url.as_str() != download.url
        || platform.url != download.url
        || update.signature != platform.signature
        || update.raw_json != expected_json
    {
        return Err("native updater metadata differs from the verified snapshot".to_owned());
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn app_update_cancel(
    state: tauri::State<'_, AppUpdateState>,
    handle_id: String,
) -> Result<(), String> {
    let handles = state.handles.lock().map_err(|error| error.to_string())?;
    let handle = handles
        .get(&handle_id)
        .ok_or("verified update handle is unavailable")?;
    handle.cancelled.store(true, Ordering::Release);
    Ok(())
}

#[tauri::command]
pub(crate) async fn app_update_install(
    app: AppHandle,
    state: tauri::State<'_, AppUpdateState>,
    handle_id: String,
) -> Result<(), String> {
    #[cfg(not(desktop))]
    {
        let _ = (app, state, handle_id);
        return Err("self-install is unavailable on this platform".to_owned());
    }
    #[cfg(desktop)]
    {
        let _active = ActiveOperation::enter(&state)?;
        let handle = clone_handle(&state, &handle_id)?;
        let _started = HandleOperation::enter(&handle)?;
        if handle.strategy != InstallStrategy::SelfInstall {
            return Err("verified update handle is not self-installable".to_owned());
        }
        let update = handle
            .native_update
            .as_ref()
            .ok_or("native update handle is missing")?;
        let bytes = download_native(&app, &handle_id, &handle, update).await?;
        if handle.cancelled.load(Ordering::Acquire) {
            return Err("cancelled".to_owned());
        }
        let _ = app.emit("app-update://applying", &handle_id);
        update.install(&bytes).map_err(|error| error.to_string())
    }
}

#[tauri::command]
pub(crate) async fn app_update_stage_deb(
    app: AppHandle,
    state: tauri::State<'_, AppUpdateState>,
    handle_id: String,
) -> Result<StagedDeb, String> {
    #[cfg(not(desktop))]
    {
        let _ = (app, state, handle_id);
        return Err("DEB staging is unavailable on this platform".to_owned());
    }
    #[cfg(desktop)]
    {
        let _active = ActiveOperation::enter(&state)?;
        let handle = clone_handle(&state, &handle_id)?;
        let _started = HandleOperation::enter(&handle)?;
        if handle.strategy != InstallStrategy::StageDeb {
            return Err("verified update handle is not a DEB package".to_owned());
        }
        let update = handle
            .native_update
            .as_ref()
            .ok_or("native update handle is missing")?;
        let bytes = download_native(&app, &handle_id, &handle, update).await?;
        if handle.cancelled.load(Ordering::Acquire) {
            return Err("cancelled".to_owned());
        }
        let directory = app
            .path()
            .download_dir()
            .map_err(|error| error.to_string())?;
        let file_name = url::Url::parse(&handle.download.url)
            .ok()
            .and_then(|url| url.path_segments()?.next_back().map(str::to_owned))
            .filter(|name| !name.is_empty() && !name.contains('/') && !name.contains('\\'))
            .ok_or("signed DEB filename is invalid")?;
        stage_verified_deb(&directory, &file_name, &bytes)
    }
}

#[cfg(desktop)]
fn stage_verified_deb(directory: &std::path::Path, file_name: &str, bytes: &[u8]) -> Result<StagedDeb, String> {
    let stem = file_name.strip_suffix(".deb")
        .filter(|name| !name.is_empty() && !name.contains('/') && !name.contains('\\'))
        .ok_or("signed DEB filename is invalid")?;
    if !directory.is_absolute() {
        return Err("Downloads directory must be absolute".to_owned());
    }
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!("{stem}-"))
        .suffix(".deb")
        .tempfile_in(directory)
        .map_err(|error| error.to_string())?;
    std::io::Write::write_all(&mut temporary, bytes).map_err(|error| error.to_string())?;
    temporary.as_file().sync_all().map_err(|error| error.to_string())?;
    let (_file, destination) = temporary.keep().map_err(|error| error.error.to_string())?;
    let path = destination.to_string_lossy().into_owned();
    let quoted = format!("'{}'", path.replace('\'', "'\\''"));
    Ok(StagedDeb {
        path,
        install_command: format!("sudo apt install {quoted}"),
    })
}

#[cfg(desktop)]
fn clone_handle(state: &AppUpdateState, handle_id: &str) -> Result<UpdateHandle, String> {
    let handles = state.handles.lock().map_err(|error| error.to_string())?;
    handles
        .get(handle_id)
        .cloned()
        .ok_or_else(|| "verified update handle is unavailable".to_owned())
}

#[cfg(desktop)]
async fn download_native(
    app: &AppHandle,
    handle_id: &str,
    handle: &UpdateHandle,
    update: &Update,
) -> Result<Vec<u8>, String> {
    if handle.cancelled.swap(false, Ordering::AcqRel) {
        return Err("cancelled".to_owned());
    }
    let mut downloaded = 0_u64;
    let app_handle = app.clone();
    let id = handle_id.to_owned();
    let download = update.download(
        move |length, total| {
            downloaded = downloaded.saturating_add(length as u64);
            let _ = app_handle.emit(
                "app-update://progress",
                UpdateProgress {
                    handle_id: id.clone(),
                    downloaded,
                    total,
                },
            );
        },
        || {},
    );
    let bytes = run_cancelable(
        async { download.await.map_err(|error| error.to_string()) },
        &handle.cancelled,
    )
    .await?;
    handle
        .product
        .verify_download_bytes(&handle.download, &bytes)
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

#[cfg(desktop)]
async fn run_cancelable<T>(
    future: impl std::future::Future<Output = Result<T, String>>,
    cancelled: &AtomicBool,
) -> Result<T, String> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if cancelled.load(Ordering::Acquire) {
                    return Err("cancelled".to_owned());
                }
            }
        }
    }
}

struct ActiveOperation(Arc<AtomicBool>);

impl ActiveOperation {
    fn enter(state: &AppUpdateState) -> Result<Self, String> {
        state
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "another update operation is already running".to_owned())?;
        Ok(Self(state.active.clone()))
    }
}

impl Drop for ActiveOperation {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct HandleOperation(Arc<AtomicBool>);

impl HandleOperation {
    fn enter(handle: &UpdateHandle) -> Result<Self, String> {
        Self::enter_flag(&handle.started)
    }

    fn enter_flag(started: &Arc<AtomicBool>) -> Result<Self, String> {
        started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "this update handle is already running".to_owned())?;
        Ok(Self(started.clone()))
    }
}

impl Drop for HandleOperation {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(desktop)]
    #[test]
    fn deb_staging_creates_downloads_and_keeps_unique_verified_packages() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("User's Downloads");
        let first = stage_verified_deb(&directory, "RisuNest_2.0.0.deb", b"first verified package").unwrap();
        let unrelated = directory.join("RisuNest_2.0.0.deb");
        std::fs::write(&unrelated, b"user file").unwrap();
        let second = stage_verified_deb(&directory, "RisuNest_2.0.0.deb", b"second verified package").unwrap();
        assert_ne!(first.path, second.path);
        assert!(first.path.ends_with(".deb"));
        assert!(second.path.ends_with(".deb"));
        assert_eq!(std::fs::read(&first.path).unwrap(), b"first verified package");
        assert_eq!(std::fs::read(&second.path).unwrap(), b"second verified package");
        assert_eq!(std::fs::read(unrelated).unwrap(), b"user file");
        assert!(first.install_command.contains("User'\\''s Downloads"));
    }

    #[cfg(desktop)]
    #[test]
    fn deb_staging_rejects_invalid_paths_without_creating_files() {
        let root = tempfile::tempdir().unwrap();
        for name in ["", "../package.deb", "sub\\package.deb", "package.exe", ".deb"] {
            assert!(stage_verified_deb(root.path(), name, b"verified").is_err());
        }
        assert!(stage_verified_deb(std::path::Path::new("relative"), "package.deb", b"verified").is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let file = root.path().join("not-a-directory");
        std::fs::write(&file, b"keep").unwrap();
        assert!(stage_verified_deb(&file, "package.deb", b"verified").is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    }

    #[test]
    fn native_operation_guard_rejects_concurrent_checks_or_installs() {
        let state = AppUpdateState::initialize(true);
        let first = ActiveOperation::enter(&state).unwrap();
        assert!(ActiveOperation::enter(&state).is_err());
        drop(first);
        assert!(ActiveOperation::enter(&state).is_ok());
    }

    #[test]
    fn handle_guard_rejects_a_second_installer_for_the_same_handle() {
        let started = Arc::new(AtomicBool::new(false));
        let first = HandleOperation::enter_flag(&started).unwrap();
        assert!(HandleOperation::enter_flag(&started).is_err());
        drop(first);
        assert!(HandleOperation::enter_flag(&started).is_ok());
    }

    #[cfg(desktop)]
    #[tokio::test]
    async fn cancellation_interrupts_an_in_progress_download_future() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            signal.store(true, Ordering::Release);
        });
        let pending = std::future::pending::<Result<(), String>>();
        assert_eq!(
            run_cancelable(pending, &cancelled).await,
            Err("cancelled".to_owned())
        );
    }
}
