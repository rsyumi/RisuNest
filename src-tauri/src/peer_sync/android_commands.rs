use super::{
    android_client::AndroidCloneJobRegistry,
    android_jni,
    command_codes::{finish_peer_command, finish_peer_worker},
    registered_target_commands::AndroidRegisteredCloneStatus,
    PeerSyncError,
};
use crate::{
    asset_repository::PayloadCas,
    local_backup::NeverCancelled,
    persistent_store::{self, StoreError},
};
use serde::Serialize;
use std::{path::PathBuf, sync::Arc};
use tauri::{AppHandle, Manager, State};

pub(crate) struct AndroidPeerCloneCommandState {
    app_data_root: PathBuf,
    registry: Result<Arc<AndroidCloneJobRegistry>, String>,
}

impl AndroidPeerCloneCommandState {
    pub(crate) fn initialize(app_data_root: PathBuf) -> Self {
        let registry = finish_peer_command(
            "Android clone registry recovery",
            AndroidCloneJobRegistry::initialize(&app_data_root).map(Arc::new),
        );
        Self {
            app_data_root,
            registry,
        }
    }

    pub(crate) fn registry(&self) -> Result<Arc<AndroidCloneJobRegistry>, String> {
        self.registry.clone()
    }

    fn capabilities(&self) -> AndroidPeerCloneCapabilities {
        AndroidPeerCloneCapabilities {
            android_client: true,
            atomic_activation_ready: true,
            lossless_backup_ready: true,
            http_transport_ready: true,
            production_enabled: self.registry.is_ok(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AndroidPeerCloneCapabilities {
    android_client: bool,
    atomic_activation_ready: bool,
    lossless_backup_ready: bool,
    http_transport_ready: bool,
    production_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AndroidPeerCloneFinalizeResult {
    revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup_path: Option<PathBuf>,
}

#[tauri::command]
pub(crate) fn peer_clone_android_capabilities(
    state: State<'_, AndroidPeerCloneCommandState>,
) -> AndroidPeerCloneCapabilities {
    state.capabilities()
}

#[tauri::command]
pub(crate) async fn peer_clone_android_current(
    state: State<'_, AndroidPeerCloneCommandState>,
) -> Result<Option<AndroidRegisteredCloneStatus>, String> {
    let registry = state.registry()?;
    finish_peer_worker(
        "Android peer clone status",
        tauri::async_runtime::spawn_blocking(move || registry.current_for_command()).await,
    )
}

#[tauri::command]
pub(crate) async fn peer_clone_android_download(
    state: State<'_, AndroidPeerCloneCommandState>,
    job_id: String,
) -> Result<(), String> {
    let registry = state.registry()?;
    let app_data_root = app_data_root_string(&state.app_data_root)?;
    let downloaded = tauri::async_runtime::spawn_blocking(move || {
        let current = registry.current()?.ok_or_else(|| {
            PeerSyncError::Validation("Android clone job is unavailable".to_owned())
        })?;
        if current.job_id != job_id {
            return Err(PeerSyncError::Validation(
                "Android clone job is not the current owned job".to_owned(),
            ));
        }
        match android_jni::resume_foreground_job(&job_id, &app_data_root, |_| {}) {
            android_jni::RESULT_TERMINAL_FAILURE => Err(PeerSyncError::Storage(
                "Android peer clone foreground transfer failed".to_owned(),
            )),
            _ => Ok(()),
        }
    })
    .await;
    finish_peer_worker("Android peer clone foreground transfer", downloaded)
}

#[tauri::command]
pub(crate) async fn peer_clone_android_request_cancel(
    state: State<'_, AndroidPeerCloneCommandState>,
    job_id: String,
) -> Result<(), String> {
    let registry = state.registry()?;
    finish_peer_worker(
        "Android peer clone cancellation",
        tauri::async_runtime::spawn_blocking(move || registry.request_cancel(&job_id)).await,
    )
}

#[tauri::command]
pub(crate) async fn peer_clone_android_cancel_foreground(
    state: State<'_, AndroidPeerCloneCommandState>,
    job_id: String,
) -> Result<(), String> {
    state.registry()?;
    let app_data_root = app_data_root_string(&state.app_data_root)?;
    let cancelled = tauri::async_runtime::spawn_blocking(move || {
        android_jni::cancel_and_cleanup_job(&job_id, &app_data_root)
            .then_some(())
            .ok_or_else(|| {
                PeerSyncError::Storage(
                    "Android peer clone foreground cancellation failed".to_owned(),
                )
            })
    })
    .await;
    finish_peer_worker("Android peer clone foreground cancellation", cancelled)
}

#[tauri::command]
pub(crate) async fn peer_clone_android_finalize(
    app: AppHandle,
    state: State<'_, AndroidPeerCloneCommandState>,
    job_id: String,
    expected_revision: i64,
) -> Result<AndroidPeerCloneFinalizeResult, String> {
    let registry = state.registry()?;
    let app_data_root = state.app_data_root.clone();
    let finalized = tauri::async_runtime::spawn_blocking(move || {
        let cas = PayloadCas::new(&app_data_root)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        let activation_root = app_data_root.join("peer-clone-activation");
        persistent_store::commands::with_store_mut(app.state(), |store| {
            registry
                .finalize(
                    &job_id,
                    store,
                    &cas,
                    &activation_root,
                    expected_revision,
                    &NeverCancelled,
                )
                .map(|receipt| AndroidPeerCloneFinalizeResult {
                    revision: receipt.revision,
                    backup_path: receipt.backup_path,
                })
                .map_err(as_store_error)
        })
        .map_err(|error| PeerSyncError::Storage(error.to_string()))
    })
    .await;
    finish_peer_worker("Android peer clone finalization", finalized)
}

#[tauri::command]
pub(crate) async fn peer_clone_android_release(
    app: AppHandle,
    state: State<'_, AndroidPeerCloneCommandState>,
    job_id: String,
) -> Result<(), String> {
    let registry = state.registry()?;
    let released = tauri::async_runtime::spawn_blocking(move || {
        persistent_store::commands::with_store_mut(app.state(), |store| {
            registry.release(&job_id, store).map_err(as_store_error)
        })
        .map_err(|error| PeerSyncError::Storage(error.to_string()))
    })
    .await;
    finish_peer_worker("Android peer clone release", released)
}

fn app_data_root_string(root: &std::path::Path) -> Result<String, String> {
    finish_peer_command(
        "Android app data path",
        root.to_str().map(str::to_owned).ok_or_else(|| {
            PeerSyncError::Storage("Android app data path is not valid UTF-8".to_owned())
        }),
    )
}

fn as_store_error(error: PeerSyncError) -> StoreError {
    StoreError::Store {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_capability_requires_a_recovered_registry() {
        let root = tempfile::tempdir().unwrap();
        let state = AndroidPeerCloneCommandState::initialize(root.path().to_owned());

        assert_eq!(
            state.capabilities(),
            AndroidPeerCloneCapabilities {
                android_client: true,
                atomic_activation_ready: true,
                lossless_backup_ready: true,
                http_transport_ready: true,
                production_enabled: true,
            }
        );
    }

    #[test]
    fn a_registry_that_never_recovered_reports_only_a_bounded_code() {
        let root = tempfile::tempdir().unwrap();
        // A file where the app data directory belongs fails the recovery in a
        // way no command may describe to the interface.
        let occupied = root.path().join("occupied");
        std::fs::write(&occupied, b"not a directory").unwrap();
        let state = AndroidPeerCloneCommandState::initialize(occupied);

        assert_eq!(state.registry().err(), Some("operationFailed".to_owned()));
        assert!(!state.capabilities().production_enabled);
    }

    #[cfg(windows)]
    fn path_the_worker_cannot_carry() -> PathBuf {
        use std::os::windows::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_wide(&[0x64, 0xd800]))
    }

    #[cfg(not(windows))]
    fn path_the_worker_cannot_carry() -> PathBuf {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(vec![0x64, 0x80]))
    }

    #[test]
    fn an_app_data_path_the_worker_cannot_carry_reports_only_a_bounded_code() {
        assert_eq!(
            app_data_root_string(&path_the_worker_cannot_carry()).unwrap_err(),
            "operationFailed"
        );

        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            app_data_root_string(root.path()).unwrap(),
            root.path().to_str().unwrap()
        );
    }

    #[test]
    fn finalize_result_serializes_the_optional_backup_path_in_camel_case() {
        let result = AndroidPeerCloneFinalizeResult {
            revision: 8,
            backup_path: Some(PathBuf::from("/data/user/0/app/pre-clone.lossless")),
        };

        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::json!({
                "revision": 8,
                "backupPath": "/data/user/0/app/pre-clone.lossless",
            })
        );
        assert_eq!(
            serde_json::to_value(AndroidPeerCloneFinalizeResult {
                revision: 8,
                backup_path: None,
            })
            .unwrap(),
            serde_json::json!({ "revision": 8 })
        );
    }
}
