use super::{
    android_client::{AndroidCloneJobRegistry, AndroidCloneJobStatus},
    android_jni, PeerSyncError,
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
        let registry = AndroidCloneJobRegistry::initialize(&app_data_root)
            .map(Arc::new)
            .map_err(|error| error.to_string());
        Self {
            app_data_root,
            registry,
        }
    }

    fn registry(&self) -> Result<Arc<AndroidCloneJobRegistry>, String> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct AndroidPeerCloneFinalizeResult {
    revision: u64,
}

#[tauri::command]
pub(crate) fn peer_clone_android_capabilities(
    state: State<'_, AndroidPeerCloneCommandState>,
) -> AndroidPeerCloneCapabilities {
    state.capabilities()
}

#[tauri::command]
pub(crate) async fn peer_clone_android_claim(
    state: State<'_, AndroidPeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    claim: String,
) -> Result<AndroidCloneJobStatus, String> {
    let registry = state.registry()?;
    tauri::async_runtime::spawn_blocking(move || {
        registry.claim(&endpoint, &session_id, &manifest_id, &claim)
    })
    .await
    .map_err(|error| format!("Android peer clone claim worker failed: {error}"))?
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn peer_clone_android_current(
    state: State<'_, AndroidPeerCloneCommandState>,
) -> Result<Option<AndroidCloneJobStatus>, String> {
    let registry = state.registry()?;
    tauri::async_runtime::spawn_blocking(move || registry.current())
        .await
        .map_err(|error| format!("Android peer clone status worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn peer_clone_android_download(
    state: State<'_, AndroidPeerCloneCommandState>,
    job_id: String,
) -> Result<(), String> {
    let registry = state.registry()?;
    let app_data_root = app_data_root_string(&state.app_data_root)?;
    tauri::async_runtime::spawn_blocking(move || {
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
    .await
    .map_err(|error| format!("Android peer clone foreground worker failed: {error}"))?
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn peer_clone_android_request_cancel(
    state: State<'_, AndroidPeerCloneCommandState>,
    job_id: String,
) -> Result<(), String> {
    let registry = state.registry()?;
    tauri::async_runtime::spawn_blocking(move || registry.request_cancel(&job_id))
        .await
        .map_err(|error| format!("Android peer clone cancellation worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn peer_clone_android_cancel_foreground(
    state: State<'_, AndroidPeerCloneCommandState>,
    job_id: String,
) -> Result<(), String> {
    state.registry()?;
    let app_data_root = app_data_root_string(&state.app_data_root)?;
    tauri::async_runtime::spawn_blocking(move || {
        android_jni::cancel_and_cleanup_job(&job_id, &app_data_root)
            .then_some(())
            .ok_or_else(|| "Android peer clone foreground cancellation failed".to_owned())
    })
    .await
    .map_err(|error| format!("Android peer clone cancellation worker failed: {error}"))?
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
    tauri::async_runtime::spawn_blocking(move || {
        let cas = PayloadCas::new(&app_data_root).map_err(|error| error.to_string())?;
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
                .map(|revision| AndroidPeerCloneFinalizeResult { revision })
                .map_err(as_store_error)
        })
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("Android peer clone finalization worker failed: {error}"))?
}

#[tauri::command]
pub(crate) async fn peer_clone_android_release(
    state: State<'_, AndroidPeerCloneCommandState>,
    job_id: String,
) -> Result<(), String> {
    let registry = state.registry()?;
    tauri::async_runtime::spawn_blocking(move || registry.release(&job_id))
        .await
        .map_err(|error| format!("Android peer clone release worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

fn app_data_root_string(root: &std::path::Path) -> Result<String, String> {
    root.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "Android app data path is not valid UTF-8".to_owned())
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
}
