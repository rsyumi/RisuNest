use super::{
    device_registry::{incoming_source_by_id, DevicePermissions, IncomingSource},
    lan::{authenticated_peer_hello, PeerHello, PeerHelloLane},
    PeerSyncError,
};
use serde::Serialize;
use std::{
    fmt,
    path::{Path, PathBuf},
};
use tauri::{AppHandle, Manager, State};

#[cfg(desktop)]
use super::commands::PeerCloneCommandState;
use super::{
    android_foreground::AndroidForegroundKey,
    bidirectional_commands::{
        peer_bidirectional_resolve_registered_client, peer_bidirectional_sync_registered_client,
        PeerBidirectionalCommandState, PeerBidirectionalConflictWinner,
        PeerBidirectionalSyncResult,
    },
    delta_commands::{
        peer_delta_pull_registered_client, PeerDeltaCommandState, PeerDeltaPullResult,
    },
    lan::LanLogicalDeltaClient,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegisteredLane {
    Clone,
    Delta,
    Bidirectional,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RegisteredSourceConnection {
    source: IncomingSource,
    lane: PeerHelloLane,
    hello: PeerHello,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredLaneDescriptor {
    session_id: String,
    manifest_id: String,
}

impl From<&PeerHelloLane> for RegisteredLaneDescriptor {
    fn from(value: &PeerHelloLane) -> Self {
        Self {
            session_id: value.session_id.clone(),
            manifest_id: value.manifest_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredHelloLanes {
    clone: Option<RegisteredLaneDescriptor>,
    delta: Option<RegisteredLaneDescriptor>,
    bidirectional: Option<RegisteredLaneDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredSourceHello {
    source_device_id: String,
    name: String,
    permissions: DevicePermissions,
    lanes: RegisteredHelloLanes,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg(desktop)]
pub struct RegisteredCloneTarget {
    source_device_id: String,
    endpoint: String,
    session_id: String,
    manifest_id: String,
}

impl RegisteredSourceConnection {
    fn safe_hello(&self) -> RegisteredSourceHello {
        safe_hello(&self.hello)
    }

    #[cfg(desktop)]
    fn clone_target(&self) -> RegisteredCloneTarget {
        RegisteredCloneTarget {
            source_device_id: self.hello.device_id.clone(),
            endpoint: self.source.endpoint.clone(),
            session_id: self.lane.session_id.clone(),
            manifest_id: self.lane.manifest_id.clone(),
        }
    }
}

fn safe_hello(hello: &PeerHello) -> RegisteredSourceHello {
    RegisteredSourceHello {
        source_device_id: hello.device_id.clone(),
        name: hello.name.clone(),
        permissions: hello.permissions.clone(),
        lanes: RegisteredHelloLanes {
            clone: hello.lanes.clone.as_ref().map(Into::into),
            delta: hello.lanes.delta.as_ref().map(Into::into),
            bidirectional: hello.lanes.bidirectional.as_ref().map(Into::into),
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegisteredTargetError {
    SourceMissing,
    AuthorizationExpired,
    PermissionDenied,
    LaneUnavailable,
    IdentityMismatch,
    TransportUnavailable,
}

impl RegisteredTargetError {
    fn code(self) -> &'static str {
        match self {
            Self::SourceMissing => "sourceMissing",
            Self::AuthorizationExpired => "authorizationExpired",
            Self::PermissionDenied => "permissionDenied",
            Self::LaneUnavailable => "laneUnavailable",
            Self::IdentityMismatch => "identityMismatch",
            Self::TransportUnavailable => "transportUnavailable",
        }
    }
}

impl fmt::Display for RegisteredTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

fn resolve_registered_source(
    app_root: &Path,
    device_id: &str,
    lane: RegisteredLane,
) -> Result<RegisteredSourceConnection, RegisteredTargetError> {
    resolve_registered_source_with(app_root, device_id, lane, authenticated_peer_hello)
}

fn app_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| safe_command_failure("registered source app root", error))
}

fn safe_command_failure(context: &str, error: impl fmt::Display) -> String {
    crate::nlog!("warn", "{context} failed: {error}");
    "transportUnavailable".to_owned()
}

fn registered_hello(
    app_root: &Path,
    device_id: &str,
) -> Result<RegisteredSourceHello, RegisteredTargetError> {
    let source = incoming_source_by_id(app_root, device_id)
        .map_err(|error| safe_failure("registered source lookup", error))?
        .ok_or(RegisteredTargetError::SourceMissing)?;
    let current = authenticated_peer_hello(&source.endpoint, &source.bearer)
        .map_err(|error| safe_failure("registered source hello", error))?;
    if current.device_id != source.device_id {
        crate::nlog!(
            "warn",
            "registered source hello returned a different device identity"
        );
        return Err(RegisteredTargetError::IdentityMismatch);
    }
    Ok(safe_hello(&current))
}

#[tauri::command]
pub async fn peer_sync_registered_hello(
    app: AppHandle,
    device_id: String,
) -> Result<RegisteredSourceHello, String> {
    let root = app_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || registered_hello(&root, &device_id))
        .await
        .map_err(|error| safe_command_failure("registered source hello worker", error))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
#[cfg(desktop)]
pub async fn peer_clone_claim_registered_client(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
    device_id: String,
) -> Result<RegisteredCloneTarget, String> {
    let root = app_root(&app)?;
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let connection = resolve_registered_source(&root, &device_id, RegisteredLane::Clone)?;
        state
            .connect_registered_target(
                &root.join("peer-clone"),
                &connection.source.endpoint,
                &connection.lane.session_id,
                &connection.lane.manifest_id,
                &connection.source.device_id,
                &connection.source.bearer,
            )
            .map_err(|error| safe_failure("registered clone target", error))?;
        Ok(connection.clone_target())
    })
    .await
    .map_err(|error| safe_command_failure("registered clone target worker", error))?
    .map_err(|error: RegisteredTargetError| error.to_string())
}

#[tauri::command]
pub async fn peer_delta_pull_registered(
    app: AppHandle,
    state: State<'_, PeerDeltaCommandState>,
    device_id: String,
    expected_revision: i64,
) -> Result<PeerDeltaPullResult, String> {
    let root = app_root(&app)?;
    let connection = tauri::async_runtime::spawn_blocking(move || {
        resolve_registered_source(&root, &device_id, RegisteredLane::Delta)
    })
    .await
    .map_err(|error| safe_command_failure("registered delta hello worker", error))?
    .map_err(|error| error.to_string())?;
    let local_device_id = super::device_registry::load_or_create_device_id(&app_root(&app)?)
        .map_err(|error| safe_failure("registered delta local identity", error).to_string())?;
    let client = LanLogicalDeltaClient::from_registered(
        &connection.source.endpoint,
        &connection.lane.session_id,
        &connection.lane.manifest_id,
        &local_device_id,
        &connection.source.device_id,
        &connection.source.bearer,
    )
    .map_err(|error| safe_failure("registered delta client", error).to_string())?;
    peer_delta_pull_registered_client(app, state.inner().clone(), client, expected_revision)
        .await
        .map_err(|error| {
            crate::nlog!("warn", "registered delta pull failed: {error}");
            "transportUnavailable".to_owned()
        })
}

#[tauri::command]
pub async fn peer_bidirectional_sync_registered(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    device_id: String,
    expected_revision: i64,
    foreground: Option<AndroidForegroundKey>,
) -> Result<PeerBidirectionalSyncResult, String> {
    let root = app_root(&app)?;
    let connection = tauri::async_runtime::spawn_blocking(move || {
        resolve_registered_source(&root, &device_id, RegisteredLane::Bidirectional)
    })
    .await
    .map_err(|error| safe_command_failure("registered bidirectional hello worker", error))?
    .map_err(|error| error.to_string())?;
    peer_bidirectional_sync_registered_client(
        app,
        state,
        connection.source.endpoint,
        connection.lane.session_id,
        connection.lane.manifest_id,
        connection.source.device_id,
        connection.source.bearer,
        expected_revision,
        foreground,
    )
    .await
    .map_err(|error| {
        crate::nlog!("warn", "registered bidirectional sync failed: {error}");
        "transportUnavailable".to_owned()
    })
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn peer_bidirectional_resolve_registered(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    device_id: String,
    operation_id: String,
    winner: PeerBidirectionalConflictWinner,
    expected_revision: i64,
    foreground: Option<AndroidForegroundKey>,
) -> Result<PeerBidirectionalSyncResult, String> {
    let root = app_root(&app)?;
    let connection = tauri::async_runtime::spawn_blocking(move || {
        resolve_registered_source(&root, &device_id, RegisteredLane::Bidirectional)
    })
    .await
    .map_err(|error| safe_command_failure("registered resolution hello worker", error))?
    .map_err(|error| error.to_string())?;
    peer_bidirectional_resolve_registered_client(
        app,
        state,
        operation_id,
        winner,
        connection.source.endpoint,
        connection.lane.session_id,
        connection.lane.manifest_id,
        connection.source.device_id,
        connection.source.bearer,
        expected_revision,
        foreground,
    )
    .await
    .map_err(|error| {
        crate::nlog!(
            "warn",
            "registered bidirectional resolution failed: {error}"
        );
        "transportUnavailable".to_owned()
    })
}

fn resolve_registered_source_with(
    app_root: &Path,
    device_id: &str,
    lane: RegisteredLane,
    hello: impl FnOnce(&str, &str) -> Result<PeerHello, PeerSyncError>,
) -> Result<RegisteredSourceConnection, RegisteredTargetError> {
    let source = incoming_source_by_id(app_root, device_id)
        .map_err(|error| safe_failure("registered source lookup", error))?
        .ok_or(RegisteredTargetError::SourceMissing)?;
    let current = hello(&source.endpoint, &source.bearer)
        .map_err(|error| safe_failure("registered source hello", error))?;
    if current.device_id != source.device_id {
        crate::nlog!(
            "warn",
            "registered source hello returned a different device identity"
        );
        return Err(RegisteredTargetError::IdentityMismatch);
    }
    let granted = match lane {
        RegisteredLane::Clone | RegisteredLane::Delta => current.permissions.allows_read(),
        RegisteredLane::Bidirectional => current.permissions.allows_bidirectional(),
    };
    if !granted {
        return Err(RegisteredTargetError::PermissionDenied);
    }
    let descriptor = match lane {
        RegisteredLane::Clone => current.lanes.clone.clone(),
        RegisteredLane::Delta => current.lanes.delta.clone(),
        RegisteredLane::Bidirectional => current.lanes.bidirectional.clone(),
    }
    .ok_or(RegisteredTargetError::LaneUnavailable)?;
    Ok(RegisteredSourceConnection {
        source,
        lane: descriptor,
        hello: current,
    })
}

fn safe_failure(context: &str, error: PeerSyncError) -> RegisteredTargetError {
    crate::nlog!("warn", "{context} failed: {error}");
    match error {
        PeerSyncError::Transport(message) if message.starts_with("HTTP 401") => {
            RegisteredTargetError::AuthorizationExpired
        }
        _ => RegisteredTargetError::TransportUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer_sync::{
        device_registry::{
            register_incoming_source, remove_incoming_source, DevicePermissions, IncomingSource,
        },
        lan::{PeerHello, PeerHelloLane, PeerHelloLanes},
        PeerSyncError,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SOURCE_ID: &str = "00000000-0000-4000-8000-000000000101";
    const OTHER_SOURCE_ID: &str = "00000000-0000-4000-8000-000000000102";
    const SESSION_ID: &str = "00000000-0000-4000-8000-000000000103";
    const MANIFEST_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BEARER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn source(permissions: DevicePermissions) -> IncomingSource {
        IncomingSource {
            device_id: SOURCE_ID.to_owned(),
            name: "Windows".to_owned(),
            endpoint: "http://127.0.0.1:32145".to_owned(),
            bearer: BEARER.to_owned(),
            permissions,
            last_seen_ms: 7,
            total_bytes: 11,
        }
    }

    fn hello(device_id: &str, permissions: DevicePermissions) -> PeerHello {
        PeerHello {
            device_id: device_id.to_owned(),
            name: "Windows".to_owned(),
            permissions,
            lanes: PeerHelloLanes {
                clone: Some(PeerHelloLane {
                    session_id: SESSION_ID.to_owned(),
                    manifest_id: MANIFEST_ID.to_owned(),
                }),
                delta: Some(PeerHelloLane {
                    session_id: SESSION_ID.to_owned(),
                    manifest_id: MANIFEST_ID.to_owned(),
                }),
                bidirectional: Some(PeerHelloLane {
                    session_id: SESSION_ID.to_owned(),
                    manifest_id: MANIFEST_ID.to_owned(),
                }),
            },
        }
    }

    #[test]
    fn registered_source_lookup_rejects_missing_and_revoked_sources_before_transport() {
        let root = tempfile::tempdir().unwrap();
        let calls = AtomicUsize::new(0);
        let missing = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Clone,
            |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(hello(SOURCE_ID, DevicePermissions::read()))
            },
        )
        .unwrap_err();
        assert_eq!(missing.code(), "sourceMissing");
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        register_incoming_source(root.path(), source(DevicePermissions::read())).unwrap();
        remove_incoming_source(root.path(), SOURCE_ID).unwrap();
        let revoked = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Clone,
            |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(hello(SOURCE_ID, DevicePermissions::read()))
            },
        )
        .unwrap_err();
        assert_eq!(revoked.code(), "sourceMissing");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn registered_source_uses_authenticated_hello_current_lane_without_reclaiming() {
        let root = tempfile::tempdir().unwrap();
        register_incoming_source(root.path(), source(DevicePermissions::read())).unwrap();
        let calls = AtomicUsize::new(0);
        let connection = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Delta,
            |endpoint, bearer| {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(endpoint, "http://127.0.0.1:32145");
                assert_eq!(bearer, BEARER);
                Ok(hello(SOURCE_ID, DevicePermissions::read()))
            },
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(connection.lane.session_id, SESSION_ID);
        assert_eq!(connection.lane.manifest_id, MANIFEST_ID);
    }

    #[test]
    fn registered_source_rejects_identity_permission_and_lane_mismatches() {
        let root = tempfile::tempdir().unwrap();
        register_incoming_source(root.path(), source(DevicePermissions::read())).unwrap();
        let mismatch = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Clone,
            |_, _| Ok(hello(OTHER_SOURCE_ID, DevicePermissions::read())),
        )
        .unwrap_err();
        assert_eq!(mismatch.code(), "identityMismatch");

        let permission = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Bidirectional,
            |_, _| Ok(hello(SOURCE_ID, DevicePermissions::read())),
        )
        .unwrap_err();
        assert_eq!(permission.code(), "permissionDenied");

        let mut no_delta = hello(SOURCE_ID, DevicePermissions::read());
        no_delta.lanes.delta = None;
        let lane = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Delta,
            |_, _| Ok(no_delta),
        )
        .unwrap_err();
        assert_eq!(lane.code(), "laneUnavailable");
    }

    #[test]
    fn registered_source_maps_authorization_and_transport_to_bounded_codes() {
        let root = tempfile::tempdir().unwrap();
        register_incoming_source(root.path(), source(DevicePermissions::read())).unwrap();
        let unauthorized = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Clone,
            |_, _| Err(PeerSyncError::Transport("HTTP 401 Unauthorized".to_owned())),
        )
        .unwrap_err();
        assert_eq!(unauthorized.code(), "authorizationExpired");

        let transport = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Clone,
            |_, _| Err(PeerSyncError::Transport("socket detail secret".to_owned())),
        )
        .unwrap_err();
        assert_eq!(transport.code(), "transportUnavailable");
        assert!(!transport.to_string().contains("socket detail secret"));
    }

    #[test]
    fn registered_hello_and_clone_target_never_serialize_the_bearer() {
        let root = tempfile::tempdir().unwrap();
        register_incoming_source(root.path(), source(DevicePermissions::read())).unwrap();
        let connection = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Clone,
            |_, _| Ok(hello(SOURCE_ID, DevicePermissions::read())),
        )
        .unwrap();
        let hello_json = serde_json::to_string(&connection.safe_hello()).unwrap();
        let target_json = serde_json::to_string(&connection.clone_target()).unwrap();
        assert!(!hello_json.contains(BEARER));
        assert!(!target_json.contains(BEARER));
        assert!(!hello_json.contains("endpoint"));
        assert!(target_json.contains("http://127.0.0.1:32145"));
    }

    #[test]
    fn registered_clients_reuse_the_stored_bearer_without_a_claim_request() {
        let root = tempfile::tempdir().unwrap();
        let target_id = "00000000-0000-4000-8000-000000000104";
        let clone_credential = root.path().join("clone-credential.json");
        let clone = crate::peer_sync::lan::LanCloneClient::from_registered_and_persist(
            &clone_credential,
            "http://127.0.0.1:32145",
            SESSION_ID,
            MANIFEST_ID,
            target_id,
            SOURCE_ID,
            BEARER,
        )
        .unwrap();
        assert_eq!(clone.target_identity().unwrap().1, SESSION_ID);
        assert!(std::fs::read_to_string(clone_credential)
            .unwrap()
            .contains(BEARER));

        let delta = crate::peer_sync::lan::LanLogicalDeltaClient::from_registered(
            "http://127.0.0.1:32145",
            SESSION_ID,
            MANIFEST_ID,
            target_id,
            SOURCE_ID,
            BEARER,
        )
        .unwrap();
        assert_eq!(delta.source_device_id(), SOURCE_ID);
        assert!(delta.is_v2_registered());

        let bidirectional = crate::peer_sync::lan::LanBidirectionalLogicalClient::from_registered(
            "http://127.0.0.1:32145",
            SESSION_ID,
            MANIFEST_ID,
            target_id,
            SOURCE_ID,
            BEARER,
        )
        .unwrap();
        assert_eq!(bidirectional.source_device_id(), SOURCE_ID);
        assert_eq!(bidirectional.credential().bearer, BEARER);
    }

    #[test]
    fn registered_clone_primes_the_existing_target_runtime() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::peer_sync::commands::PeerCloneCommandState::default();
        let target = state
            .connect_registered_target(
                &root.path().join("peer-clone"),
                "http://127.0.0.1:32145",
                SESSION_ID,
                MANIFEST_ID,
                SOURCE_ID,
                BEARER,
            )
            .unwrap();
        assert_eq!(target.source_device_id.as_deref(), Some(SOURCE_ID));
        assert!(
            serde_json::to_string(&state.target_status_current().unwrap())
                .unwrap()
                .contains("\"phase\":\"idle\"")
        );
        assert!(root.path().join("peer-clone/targets").try_exists().unwrap());
    }
}
