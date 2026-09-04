#[cfg(target_os = "android")]
use super::android_source_commands::AndroidPeerCloneSourceState;
#[cfg(target_os = "android")]
use super::shared_session::AndroidDeviceSyncSourceState;
use super::{
    bidirectional_commands::PeerBidirectionalCommandState,
    command_codes::finish_peer_command,
    delta_commands::PeerDeltaCommandState,
    device_registry::{
        incoming_source_by_id, incoming_source_summaries, outgoing_device_summaries,
        register_incoming_source, remove_incoming_source, revoke_outgoing_device, IncomingSource,
        IncomingSourceSummary, OutgoingDeviceSummary,
    },
    logical_completion::invalidate_outgoing_logical_completion_proofs,
    PeerSyncError,
};
#[cfg(desktop)]
use super::{commands::PeerCloneCommandState, shared_session::DeviceSyncSourceState};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, State};

fn registered_source_lifecycle_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Proof that the caller holds the registered-source lifecycle hold. Only
/// `lock_registered_source_lifecycle` can make one, so a lifecycle-guarded
/// function cannot be handed an unrelated guard by mistake.
pub(crate) struct RegisteredSourceLifecycleGuard(
    // Held only for its Drop: releasing the lifecycle hold is the whole value.
    #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
);

pub(crate) fn lock_registered_source_lifecycle(
) -> Result<RegisteredSourceLifecycleGuard, super::PeerSyncError> {
    registered_source_lifecycle_lock()
        .lock()
        .map(RegisteredSourceLifecycleGuard)
        .map_err(|_| {
            super::PeerSyncError::Storage("registered source lifecycle lock failed".to_owned())
        })
}

#[cfg(test)]
pub(crate) fn registered_source_lifecycle_is_locked() -> bool {
    matches!(
        registered_source_lifecycle_lock().try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    )
}

/// Stable codes the interface maps to its own wording; never shown as native text.
pub(crate) const REGISTRATION_BLOCKED_BY_ACTIVE_WORK: &str =
    "peer-registration-blocked-by-active-work";
pub(crate) const SOURCE_IN_USE: &str = "peer-source-in-use";
/// A registered clone target publishes only while the incoming source it bound
/// to is still the registered one. Both the desktop and the Android target
/// raise this, so it lives beside the other lifecycle codes.
pub(crate) const REGISTERED_SOURCE_CHANGED: &str = "peer-registered-source-changed";

trait RegistryAppRootResolver {
    fn resolve_registry_app_root(&self) -> Result<PathBuf, PeerSyncError>;
}

impl RegistryAppRootResolver for AppHandle {
    fn resolve_registry_app_root(&self) -> Result<PathBuf, PeerSyncError> {
        self.path()
            .app_data_dir()
            .map_err(|error| PeerSyncError::Storage(error.to_string()))
    }
}

fn app_root(app: &impl RegistryAppRootResolver) -> Result<PathBuf, String> {
    finish_peer_command(
        "peer sync registry application data directory",
        app.resolve_registry_app_root(),
    )
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
    finish_peer_command(
        "peer sync registry outgoing device list",
        outgoing_device_summaries(&app_root(&app)?),
    )
}
#[tauri::command]
pub fn peer_sync_incoming_sources(app: AppHandle) -> Result<Vec<IncomingSourceSummary>, String> {
    finish_peer_command(
        "peer sync registry incoming source list",
        incoming_source_summaries(&app_root(&app)?),
    )
}
// The lifecycle hold this waits on can span a registration round trip, so it
// must not run on the main thread.
#[tauri::command(async)]
pub fn peer_sync_remove_incoming_source(app: AppHandle, device_id: String) -> Result<(), String> {
    finish_peer_command(
        "peer sync registry incoming source removal",
        remove_incoming_source_if_inactive(&app_root(&app)?, &device_id),
    )
}

/// Names the lane still depending on a registered source, or `None` when no lane
/// does. `source_device_id` of `None` looks at every registered source at once.
fn active_registered_source_lane(
    app_root: &Path,
    source_device_id: Option<&str>,
) -> Result<Option<&'static str>, PeerSyncError> {
    #[cfg(desktop)]
    if super::commands::registered_clone_source_is_active(app_root, source_device_id)? {
        return Ok(Some("registered clone source is used by an active target"));
    }
    #[cfg(any(target_os = "android", test))]
    if super::android_client::registered_clone_source_is_active(app_root, source_device_id)? {
        return Ok(Some(
            "registered Android clone source is used by an active job",
        ));
    }
    if super::delta_completion::registered_delta_source_is_active(app_root, source_device_id)? {
        return Ok(Some("registered delta source is used by an active target"));
    }
    if super::bidirectional_commands::registered_bidirectional_source_is_active(
        app_root,
        source_device_id,
    )? {
        return Ok(Some(
            "registered bidirectional source is used by an active target",
        ));
    }
    Ok(None)
}

/// Refuses a new registration while any lane still depends on a registered source.
/// The remote peer replaces its authorization as soon as it answers a new
/// registration link, and it has no way to take that back once this device
/// refuses, so the refusal has to happen before the request leaves.
///
/// The caller must already hold the registered-source lifecycle guard, so that no
/// lane can start between this check and the registration that follows it.
pub(crate) fn ensure_no_active_registered_source_work(
    _lifecycle: &RegisteredSourceLifecycleGuard,
    app_root: &Path,
) -> Result<(), PeerSyncError> {
    if let Some(lane) = active_registered_source_lane(app_root, None)? {
        crate::nlog!("warn", "new registration refused: {lane}");
        return Err(PeerSyncError::Validation(
            REGISTRATION_BLOCKED_BY_ACTIVE_WORK.to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn register_incoming_source_if_compatible(
    app_root: &Path,
    source: IncomingSource,
) -> Result<(), PeerSyncError> {
    let lifecycle = lock_registered_source_lifecycle()?;
    register_incoming_source_if_compatible_locked(&lifecycle, app_root, source)
}

/// The lock-held half of the guard, for callers that already hold the lifecycle
/// guard across a wider span than a single registration.
pub(crate) fn register_incoming_source_if_compatible_locked(
    _lifecycle: &RegisteredSourceLifecycleGuard,
    app_root: &Path,
    source: IncomingSource,
) -> Result<(), PeerSyncError> {
    let bearer_changed = incoming_source_by_id(app_root, &source.device_id)?
        .is_some_and(|current| current.bearer != source.bearer);
    if !bearer_changed {
        return register_incoming_source(app_root, source);
    }
    if let Some(lane) = active_registered_source_lane(app_root, Some(&source.device_id))? {
        crate::nlog!("warn", "registered source rotation refused: {lane}");
        return Err(PeerSyncError::Validation(SOURCE_IN_USE.to_owned()));
    }
    register_incoming_source(app_root, source)
}

pub(crate) fn remove_incoming_source_if_inactive(
    app_root: &std::path::Path,
    device_id: &str,
) -> Result<(), super::PeerSyncError> {
    let _lifecycle = lock_registered_source_lifecycle()?;
    if let Some(lane) = active_registered_source_lane(app_root, Some(device_id))? {
        crate::nlog!("warn", "registered source removal refused: {lane}");
        return Err(super::PeerSyncError::Validation(SOURCE_IN_USE.to_owned()));
    }
    remove_incoming_source(app_root, device_id)
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
    finish_peer_command(
        "peer sync registry outgoing device revocation",
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
    const OTHER_SOURCE_ID: &str = "00000000-0000-4000-8000-000000000206";

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
            .expect("registry refusal log")
    }

    fn latest_command_code_log_containing(marker: &str) -> crate::native_log::LogEntry {
        crate::native_log::global_state()
            .tail(None)
            .into_iter()
            .rev()
            .find(|entry| {
                entry.target.ends_with("peer_sync::command_codes") && entry.message.contains(marker)
            })
            .expect("registry failure log")
    }

    #[test]
    fn an_underlying_registry_failure_reaches_the_interface_as_a_bounded_code() {
        let directory = tempfile::tempdir().unwrap();
        let invalid_root = directory.path().join("not-a-directory");
        fs::write(&invalid_root, b"file").unwrap();
        let raw = outgoing_device_summaries(&invalid_root)
            .unwrap_err()
            .to_string();

        let error = finish_peer_command(
            "peer sync registry outgoing device list registry-fixture-underlying",
            outgoing_device_summaries(&invalid_root).map(|_| ()),
        )
        .unwrap_err();

        assert_eq!(error, "operationFailed");
        let entry = latest_command_code_log_containing("registry-fixture-underlying");
        assert!(entry.message.contains("outgoing device list"));
        assert!(entry.message.contains(&raw));
    }

    #[test]
    fn app_root_failure_is_logged_and_returns_only_the_bounded_code() {
        struct FailingResolver;

        impl RegistryAppRootResolver for FailingResolver {
            fn resolve_registry_app_root(&self) -> Result<PathBuf, PeerSyncError> {
                Err(PeerSyncError::Storage(
                    "registry-fixture-app-root path resolver unavailable".to_owned(),
                ))
            }
        }

        let error = app_root(&FailingResolver).unwrap_err();

        assert_eq!(error, "operationFailed");
        let entry = latest_command_code_log_containing("registry-fixture-app-root");
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

    fn store_active_delta_intent(root: &std::path::Path, source_device_id: &str) {
        crate::peer_sync::delta_completion::PeerDeltaCompletionJournal::new(root)
            .store_activation_intent(
                &crate::peer_sync::delta_completion::DeltaCompletionContext {
                    operation_id: "00000000-0000-4000-8000-000000000207".to_owned(),
                    source_device_id: source_device_id.to_owned(),
                    manifest_id: MANIFEST_ID.to_owned(),
                    mode: crate::peer_sync::delta_completion::DeltaCompletionMode::CompletionV1,
                    pre_revision: 1,
                    pre_common_base: None,
                    post_revision: 2,
                    post_common_base: crate::persistent_store::SyncGenerationIdentity {
                        generation_id: "generation-2".to_owned(),
                        manifest_hash: MANIFEST_ID.to_owned(),
                        generation_sequence: "2".to_owned(),
                    },
                    transferred_objects: 1,
                    transferred_bytes: 3,
                },
            )
            .unwrap();
    }

    fn registered_source(bearer: &str) -> IncomingSource {
        IncomingSource {
            device_id: DEVICE_ID.to_owned(),
            name: "Source".to_owned(),
            endpoint: "http://192.168.0.21:32145".to_owned(),
            bearer: bearer.to_owned(),
            permissions: DevicePermissions::read(),
            last_seen_ms: 3,
            total_bytes: 5,
        }
    }

    #[test]
    fn a_new_registration_is_refused_by_work_that_belongs_to_a_different_source() {
        let root = tempfile::tempdir().unwrap();
        let lifecycle = lock_registered_source_lifecycle().unwrap();

        ensure_no_active_registered_source_work(&lifecycle, root.path()).unwrap();

        store_active_delta_intent(root.path(), OTHER_SOURCE_ID);

        assert_eq!(
            ensure_no_active_registered_source_work(&lifecycle, root.path()).unwrap_err(),
            PeerSyncError::Validation(REGISTRATION_BLOCKED_BY_ACTIVE_WORK.to_owned())
        );
    }

    #[test]
    fn removal_and_source_rotation_refuse_with_one_code_and_log_only_the_lane() {
        let root = tempfile::tempdir().unwrap();
        register_incoming_source(root.path(), registered_source(&"b".repeat(64))).unwrap();
        store_active_delta_intent(root.path(), DEVICE_ID);

        let removal = remove_incoming_source_if_inactive(root.path(), DEVICE_ID).unwrap_err();
        let rotation =
            register_incoming_source_if_compatible(root.path(), registered_source(&"c".repeat(64)))
                .unwrap_err();

        assert_eq!(removal, PeerSyncError::Validation(SOURCE_IN_USE.to_owned()));
        assert_eq!(
            rotation,
            PeerSyncError::Validation(SOURCE_IN_USE.to_owned())
        );
        for message in [removal.to_string(), rotation.to_string()] {
            assert!(!message.contains("active target"), "{message}");
        }
        for marker in ["removal refused", "rotation refused"] {
            let entry = latest_registry_log_containing(marker);
            assert!(entry
                .message
                .contains("registered delta source is used by an active target"));
        }
        assert_eq!(
            incoming_source_by_id(root.path(), DEVICE_ID)
                .unwrap()
                .unwrap()
                .bearer,
            "b".repeat(64)
        );
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
    finish_peer_command(
        "peer sync registry outgoing device revocation",
        revoke_outgoing_device_with_logical_cleanup(&app_root(&app)?, &device_id, move |id| {
            clone.revoke_registered_device(id);
            delta.revoke_registered_device(id);
            bidirectional.revoke_registered_device(id);
            shared.revoke_registered_device(id);
        }),
    )
}
