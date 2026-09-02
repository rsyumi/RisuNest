#[cfg(target_os = "android")]
use super::android_source_commands::AndroidPeerCloneSourceState;
#[cfg(target_os = "android")]
use super::shared_session::AndroidDeviceSyncSourceState;
use super::{
    bidirectional_commands::PeerBidirectionalCommandState,
    delta_commands::PeerDeltaCommandState,
    device_registry::{
        incoming_source_summaries, outgoing_device_summaries, remove_incoming_source,
        revoke_outgoing_device, IncomingSourceSummary, OutgoingDeviceSummary,
    },
};
#[cfg(desktop)]
use super::{commands::PeerCloneCommandState, shared_session::DeviceSyncSourceState};
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};

trait RegistryAppRootResolver {
    fn resolve_registry_app_root(&self) -> Result<PathBuf, String>;
}

impl RegistryAppRootResolver for AppHandle {
    fn resolve_registry_app_root(&self) -> Result<PathBuf, String> {
        self.path()
            .app_data_dir()
            .map_err(|error| error.to_string())
    }
}

fn app_root(app: &impl RegistryAppRootResolver) -> Result<PathBuf, String> {
    finish_registry_command(
        "application data directory",
        app.resolve_registry_app_root(),
    )
}

fn finish_registry_command<T, E>(operation: &str, result: Result<T, E>) -> Result<T, String>
where
    E: std::fmt::Display,
{
    result.map_err(|error| {
        crate::nlog!("error", "peer sync registry {operation} failed: {error}");
        error.to_string()
    })
}

#[tauri::command]
pub fn peer_sync_outgoing_devices(app: AppHandle) -> Result<Vec<OutgoingDeviceSummary>, String> {
    finish_registry_command(
        "outgoing device list",
        outgoing_device_summaries(&app_root(&app)?),
    )
}
#[tauri::command]
pub fn peer_sync_incoming_sources(app: AppHandle) -> Result<Vec<IncomingSourceSummary>, String> {
    finish_registry_command(
        "incoming source list",
        incoming_source_summaries(&app_root(&app)?),
    )
}
#[tauri::command]
pub fn peer_sync_remove_incoming_source(app: AppHandle, device_id: String) -> Result<(), String> {
    finish_registry_command(
        "incoming source removal",
        remove_incoming_source(&app_root(&app)?, &device_id),
    )
}

#[cfg(desktop)]
#[tauri::command]
pub fn peer_sync_revoke_outgoing_device(
    app: AppHandle,
    clone: State<'_, PeerCloneCommandState>,
    delta: State<'_, PeerDeltaCommandState>,
    bidirectional: State<'_, PeerBidirectionalCommandState>,
    shared: State<'_, DeviceSyncSourceState>,
    device_id: String,
) -> Result<(), String> {
    let clone = clone.inner().clone();
    let delta = delta.inner().clone();
    let bidirectional = bidirectional.inner().clone();
    let shared = shared.inner().clone();
    finish_registry_command(
        "outgoing device revocation",
        revoke_outgoing_device(&app_root(&app)?, &device_id, move |id| {
            clone.revoke_registered_device(id);
            delta.revoke_registered_device(id);
            bidirectional.revoke_registered_device(id);
            shared.revoke_registered_device(id);
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn latest_registry_log_containing(marker: &str) -> crate::native_log::LogEntry {
        crate::native_log::global_state()
            .tail(None)
            .into_iter()
            .rev()
            .find(|entry| {
                entry.target.ends_with("peer_sync::registry_commands")
                    && entry.message.contains(marker)
            })
            .expect("registry failure log")
    }

    #[test]
    fn underlying_registry_failure_is_logged_without_changing_the_returned_string() {
        let directory = tempfile::tempdir().unwrap();
        let invalid_root = directory.path().join("not-a-directory");
        fs::write(&invalid_root, b"file").unwrap();
        let raw = outgoing_device_summaries(&invalid_root)
            .unwrap_err()
            .to_string();

        let error = finish_registry_command::<(), _>(
            "outgoing device list registry-fixture-underlying",
            outgoing_device_summaries(&invalid_root).map(|_| ()),
        )
        .unwrap_err();

        assert_eq!(error, raw);
        let entry = latest_registry_log_containing("registry-fixture-underlying");
        assert!(entry.message.contains("outgoing device list"));
        assert!(entry.message.contains(&raw));
    }

    #[test]
    fn registry_failure_detail_is_masked_only_in_the_native_log() {
        let raw =
            "registry-fixture-masking Authorization: Bearer fixture-registry-secret".to_owned();

        let error = finish_registry_command::<(), _>(
            "outgoing device list registry-fixture-masking",
            Err(raw.clone()),
        )
        .unwrap_err();

        assert_eq!(error, raw);
        let entry = latest_registry_log_containing("registry-fixture-masking");
        assert!(entry.message.contains("Authorization: ***"));
        assert!(!entry.message.contains("fixture-registry-secret"));
    }

    #[test]
    fn app_root_failure_is_logged_without_changing_the_returned_string() {
        struct FailingResolver;

        impl RegistryAppRootResolver for FailingResolver {
            fn resolve_registry_app_root(&self) -> Result<PathBuf, String> {
                Err("registry-fixture-app-root path resolver unavailable".to_owned())
            }
        }

        let raw = "registry-fixture-app-root path resolver unavailable".to_owned();

        let error = app_root(&FailingResolver).unwrap_err();

        assert_eq!(error, raw);
        let entry = latest_registry_log_containing("registry-fixture-app-root");
        assert!(entry.message.contains("path resolver unavailable"));
    }
}
#[cfg(target_os = "android")]
#[tauri::command]
pub fn peer_sync_revoke_outgoing_device(
    app: AppHandle,
    clone: State<'_, AndroidPeerCloneSourceState>,
    delta: State<'_, PeerDeltaCommandState>,
    bidirectional: State<'_, PeerBidirectionalCommandState>,
    shared: State<'_, AndroidDeviceSyncSourceState>,
    device_id: String,
) -> Result<(), String> {
    let clone = clone.inner().clone();
    let delta = delta.inner().clone();
    let bidirectional = bidirectional.inner().clone();
    let shared = shared.inner().clone();
    finish_registry_command(
        "outgoing device revocation",
        revoke_outgoing_device(&app_root(&app)?, &device_id, move |id| {
            clone.revoke_registered_device(id);
            delta.revoke_registered_device(id);
            bidirectional.revoke_registered_device(id);
            shared.revoke_registered_device(id);
        }),
    )
}
