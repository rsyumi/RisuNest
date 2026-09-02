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
    logical_completion::invalidate_outgoing_logical_completion_proofs,
    PeerSyncError,
};
#[cfg(desktop)]
use super::{commands::PeerCloneCommandState, shared_session::DeviceSyncSourceState};
use std::path::{Path, PathBuf};
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

fn revoke_outgoing_device_with_logical_cleanup(
    app_root: &Path,
    device_id: &str,
    revoke_live: impl FnOnce(&str),
) -> Result<(), PeerSyncError> {
    // Revoke the credential and completion offers first. If proof cleanup then
    // fails, the stale proof remains unusable and the caller still sees error.
    revoke_outgoing_device(app_root, device_id, revoke_live)?;
    invalidate_outgoing_logical_completion_proofs(app_root, device_id)
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
        revoke_outgoing_device_with_logical_cleanup(&app_root(&app)?, &device_id, move |id| {
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
    use crate::peer_sync::{
        device_registry::{
            issue_outgoing_unmeasured_completion_offer, CompletionLane, DevicePermissions,
            OutgoingDevice, OutgoingDeviceRegistry,
        },
        logical_completion::{
            prepare_outgoing_logical_completion_proof, LogicalCompletionManifest,
        },
    };
    use std::{collections::BTreeMap, fs};

    const DEVICE_ID: &str = "00000000-0000-4000-8000-000000000205";
    const MANIFEST_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn root_with_logical_proof() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let mut registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
        registry
            .upsert(OutgoingDevice {
                device_id: DEVICE_ID.to_owned(),
                name: "Target".to_owned(),
                bearer_digest: "b".repeat(64),
                permissions: DevicePermissions::read(),
                created_at_ms: 1,
                last_seen_ms: 1,
                total_bytes: 0,
            })
            .unwrap();
        registry.save().unwrap();
        let lease = issue_outgoing_unmeasured_completion_offer(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            MANIFEST_ID,
            None,
        )
        .unwrap();
        prepare_outgoing_logical_completion_proof(
            root.path(),
            DEVICE_ID,
            CompletionLane::Delta,
            lease.as_str(),
            MANIFEST_ID,
            &LogicalCompletionManifest::new(BTreeMap::from([("1".repeat(64), 5)])).unwrap(),
        )
        .unwrap();
        root
    }

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

    #[test]
    fn revoke_removes_core_state_and_logical_proof_in_one_request() {
        let root = root_with_logical_proof();
        let proof = root
            .path()
            .join("peer-sync/logical-completion-proofs")
            .join(format!("{DEVICE_ID}-delta.json"));
        let mut revoked_live = false;

        revoke_outgoing_device_with_logical_cleanup(root.path(), DEVICE_ID, |_| {
            revoked_live = true
        })
        .unwrap();

        assert!(revoked_live);
        assert!(OutgoingDeviceRegistry::load(root.path())
            .unwrap()
            .devices()
            .is_empty());
        assert!(!proof.exists());
    }

    #[test]
    fn cleanup_failure_still_leaves_the_core_credential_revoked() {
        let root = root_with_logical_proof();
        let proof = root
            .path()
            .join("peer-sync/logical-completion-proofs")
            .join(format!("{DEVICE_ID}-delta.json"));
        fs::remove_file(&proof).unwrap();
        fs::create_dir(&proof).unwrap();

        assert!(
            revoke_outgoing_device_with_logical_cleanup(root.path(), DEVICE_ID, |_| {}).is_err()
        );
        assert!(OutgoingDeviceRegistry::load(root.path())
            .unwrap()
            .devices()
            .is_empty());
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
        revoke_outgoing_device_with_logical_cleanup(&app_root(&app)?, &device_id, move |id| {
            clone.revoke_registered_device(id);
            delta.revoke_registered_device(id);
            bidirectional.revoke_registered_device(id);
            shared.revoke_registered_device(id);
        }),
    )
}
