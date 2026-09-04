#[cfg(any(target_os = "android", test))]
use super::android_client::{AndroidCloneJobPhase, AndroidCloneJobStatus};
use super::{
    command_codes::{code_for, PeerCommandCode},
    device_registry::{incoming_source_by_id, DevicePermissions, IncomingSource},
    lan::{
        authenticated_peer_hello_status, authenticated_peer_hello_with_capabilities,
        AuthenticatedPeerHelloOutcome, PeerCompletionCapability, PeerHello, PeerHelloLane,
    },
    PeerSyncError,
};
use serde::Serialize;
use std::{
    fmt,
    path::{Path, PathBuf},
};
use tauri::{AppHandle, Manager, State};

#[cfg(target_os = "android")]
use super::android_commands::AndroidPeerCloneCommandState;
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
    completion: PeerCompletionCapability,
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

#[cfg(any(target_os = "android", test))]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AndroidRegisteredCloneStatus {
    source_device_id: String,
    job_id: String,
    phase: AndroidCloneJobPhase,
    completed_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    committed_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup_path: Option<PathBuf>,
}

#[cfg(any(target_os = "android", test))]
pub(crate) fn safe_android_clone_status(
    source_device_id: &str,
    status: &AndroidCloneJobStatus,
) -> AndroidRegisteredCloneStatus {
    let backup_path = status
        .backup_path
        .as_ref()
        .map(|_| PathBuf::from(format!("pre-clone-{}.lossless", status.job_id)));
    AndroidRegisteredCloneStatus {
        source_device_id: source_device_id.to_owned(),
        job_id: status.job_id.clone(),
        phase: status.phase,
        completed_bytes: status.completed_bytes,
        total_bytes: status.total_bytes,
        error: status.error.as_ref().map(|_| "transferFailed".to_owned()),
        committed_revision: status.committed_revision,
        backup_path,
    }
}

fn validate_android_registered_source_endpoint(endpoint: &str) -> Result<String, PeerSyncError> {
    let endpoint = super::lan::validate_lan_endpoint(endpoint)?;
    let parsed =
        url::Url::parse(&endpoint).map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    let allowed = parsed.scheme() == "http"
        && matches!(
            parsed.host(),
            Some(url::Host::Ipv4(address))
                if address.is_private() || address.is_link_local()
        );
    if !allowed {
        return Err(PeerSyncError::Validation(
            "Android registered source must use a private LAN IPv4 endpoint".to_owned(),
        ));
    }
    Ok(endpoint)
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

#[cfg(target_os = "android")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AndroidPeerCloneClaimResult {
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

fn resolve_registered_source(
    app_root: &Path,
    device_id: &str,
    lane: RegisteredLane,
) -> Result<RegisteredSourceConnection, PeerCommandCode> {
    let mut connection =
        resolve_registered_source_with(app_root, device_id, lane, authenticated_peer_hello_status)?;
    if lane == RegisteredLane::Clone {
        let observed = authenticated_peer_hello_with_capabilities(
            &connection.source.endpoint,
            &connection.source.bearer,
        )
        .map_err(|error| safe_failure("registered clone capabilities", error))?;
        if observed.hello != connection.hello {
            crate::nlog!(
                "warn",
                "registered clone capability hello changed its authenticated identity"
            );
            return Err(PeerCommandCode::IdentityMismatch);
        }
        connection.completion = observed.completion;
    }
    Ok(connection)
}

fn app_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| safe_command_failure("registered source app root", error))
}

fn safe_command_failure(context: &str, error: impl fmt::Display) -> String {
    crate::nlog!("warn", "{context} failed: {error}");
    PeerCommandCode::TransportUnavailable.code().to_owned()
}

fn registered_operation_failure(context: &str, error: impl fmt::Display) -> String {
    crate::nlog!("warn", "{context} failed: {error}");
    PeerCommandCode::OperationFailed.code().to_owned()
}

/// A failure past the hello round trip goes through the same mapping table as
/// every other command boundary, so a refusal such as a rotated registration or
/// a blocked new registration keeps its own code instead of collapsing to the
/// generic one.
fn registered_target_operation_failure(context: &str, error: PeerSyncError) -> PeerCommandCode {
    crate::nlog!("warn", "{context} failed: {error}");
    code_for(&error)
}

fn registered_hello(
    app_root: &Path,
    device_id: &str,
) -> Result<RegisteredSourceHello, PeerCommandCode> {
    registered_hello_with(
        app_root,
        device_id,
        cfg!(target_os = "android"),
        authenticated_peer_hello_status,
    )
}

fn registered_hello_with<F>(
    app_root: &Path,
    device_id: &str,
    require_android_lan: bool,
    transport: F,
) -> Result<RegisteredSourceHello, PeerCommandCode>
where
    F: FnOnce(&str, &str) -> Result<AuthenticatedPeerHelloOutcome, PeerSyncError>,
{
    let source = incoming_source_by_id(app_root, device_id)
        .map_err(|error| safe_failure("registered source lookup", error))?
        .ok_or(PeerCommandCode::SourceMissing)?;
    if require_android_lan {
        validate_android_registered_source_endpoint(&source.endpoint)
            .map_err(|error| safe_failure("registered Android hello endpoint", error))?;
    }
    let current = registered_hello_outcome(
        "registered source hello",
        transport(&source.endpoint, &source.bearer),
    )?;
    if current.device_id != source.device_id {
        crate::nlog!(
            "warn",
            "registered source hello returned a different device identity"
        );
        return Err(PeerCommandCode::IdentityMismatch);
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
        .map_err(|code: PeerCommandCode| code.to_string())
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
                connection.completion,
            )
            .map_err(|error| {
                registered_target_operation_failure("registered clone target", error)
            })?;
        Ok(connection.clone_target())
    })
    .await
    .map_err(|error| registered_operation_failure("registered clone target worker", error))?
    .map_err(|code: PeerCommandCode| code.to_string())
}

#[tauri::command]
#[cfg(target_os = "android")]
pub async fn peer_clone_claim_client(
    app: AppHandle,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    claim: String,
) -> Result<AndroidPeerCloneClaimResult, String> {
    let root = app_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let endpoint = validate_android_registered_source_endpoint(&endpoint)
            .map_err(|error| safe_failure("Android clone registration endpoint", error))?;
        let credential_root = root.join("peer-clone").join("android-registration");
        std::fs::create_dir_all(&credential_root).map_err(|error| {
            registered_target_operation_failure(
                "Android clone registration storage",
                PeerSyncError::from(error),
            )
        })?;
        let client = super::lan::LanCloneClient::claim_strict_v2_and_persist_and_register(
            &root,
            super::device_registry::platform_device_name(),
            &credential_root.join("credential.json"),
            &endpoint,
            &session_id,
            &manifest_id,
            &claim,
        )
        .map_err(|error| {
            registered_target_operation_failure("Android clone registration", error)
        })?;
        let source_device_id = client
            .registered_source_device_id()
            .ok_or(PeerCommandCode::OperationFailed)?
            .to_owned();
        Ok(AndroidPeerCloneClaimResult {
            source_device_id,
            endpoint,
            session_id,
            manifest_id,
        })
    })
    .await
    .map_err(|error| registered_operation_failure("Android clone registration worker", error))?
    .map_err(|code: PeerCommandCode| code.to_string())
}

#[tauri::command]
#[cfg(target_os = "android")]
pub async fn peer_clone_claim_v2_client(
    app: AppHandle,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    claim: String,
) -> Result<AndroidPeerCloneClaimResult, String> {
    peer_clone_claim_client(app, endpoint, session_id, manifest_id, claim).await
}

#[tauri::command]
#[cfg(target_os = "android")]
pub async fn peer_clone_claim_registered_client(
    app: AppHandle,
    state: State<'_, AndroidPeerCloneCommandState>,
    device_id: String,
) -> Result<AndroidRegisteredCloneStatus, String> {
    let root = app_root(&app)?;
    let registry = state.registry()?;
    tauri::async_runtime::spawn_blocking(move || {
        let connection = resolve_registered_source(&root, &device_id, RegisteredLane::Clone)?;
        let local_device_id =
            super::device_registry::load_or_create_device_id(&root).map_err(|error| {
                registered_target_operation_failure("registered Android local identity", error)
            })?;
        let status = registry
            .connect_registered(
                &connection.source.endpoint,
                &connection.lane.session_id,
                &connection.lane.manifest_id,
                &local_device_id,
                &connection.source.device_id,
                &connection.source.bearer,
                connection.completion,
            )
            .map_err(|error| {
                registered_target_operation_failure("registered Android clone target", error)
            })?;
        Ok(safe_android_clone_status(
            &connection.source.device_id,
            &status,
        ))
    })
    .await
    .map_err(|error| registered_operation_failure("registered Android clone worker", error))?
    .map_err(|code: PeerCommandCode| code.to_string())
}

#[tauri::command]
#[cfg(desktop)]
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
    .map_err(|code: PeerCommandCode| code.to_string())?;
    let local_device_id = super::device_registry::load_or_create_device_id(&app_root(&app)?)
        .map_err(|error| {
            registered_target_operation_failure("registered delta local identity", error)
                .to_string()
        })?;
    let client = LanLogicalDeltaClient::from_registered(
        &connection.source.endpoint,
        &connection.lane.session_id,
        &connection.lane.manifest_id,
        &local_device_id,
        &connection.source.device_id,
        &connection.source.bearer,
    )
    .map_err(|error| {
        registered_target_operation_failure("registered delta client", error).to_string()
    })?;
    peer_delta_pull_registered_client(app, state.inner().clone(), client, expected_revision).await
}

#[tauri::command]
#[cfg(target_os = "android")]
pub async fn peer_delta_pull_registered(
    app: AppHandle,
    state: State<'_, PeerDeltaCommandState>,
    device_id: String,
    expected_revision: i64,
    foreground: AndroidForegroundKey,
) -> Result<PeerDeltaPullResult, String> {
    let root = app_root(&app)?;
    let connection = tauri::async_runtime::spawn_blocking(move || {
        resolve_registered_source(&root, &device_id, RegisteredLane::Delta)
    })
    .await
    .map_err(|error| safe_command_failure("registered delta hello worker", error))?
    .map_err(|code: PeerCommandCode| code.to_string())?;
    let local_device_id = super::device_registry::load_or_create_device_id(&app_root(&app)?)
        .map_err(|error| {
            registered_target_operation_failure("registered delta local identity", error)
                .to_string()
        })?;
    let client = LanLogicalDeltaClient::from_registered(
        &connection.source.endpoint,
        &connection.lane.session_id,
        &connection.lane.manifest_id,
        &local_device_id,
        &connection.source.device_id,
        &connection.source.bearer,
    )
    .map_err(|error| {
        registered_target_operation_failure("registered delta client", error).to_string()
    })?;
    peer_delta_pull_registered_client(
        app,
        state.inner().clone(),
        client,
        expected_revision,
        foreground,
    )
    .await
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
    .map_err(|code: PeerCommandCode| code.to_string())?;
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
    .map_err(|code: PeerCommandCode| code.to_string())?;
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
}

fn resolve_registered_source_with(
    app_root: &Path,
    device_id: &str,
    lane: RegisteredLane,
    hello: impl FnOnce(&str, &str) -> Result<AuthenticatedPeerHelloOutcome, PeerSyncError>,
) -> Result<RegisteredSourceConnection, PeerCommandCode> {
    let source = incoming_source_by_id(app_root, device_id)
        .map_err(|error| safe_failure("registered source lookup", error))?
        .ok_or(PeerCommandCode::SourceMissing)?;
    #[cfg(target_os = "android")]
    validate_android_registered_source_endpoint(&source.endpoint)
        .map_err(|error| safe_failure("registered Android LAN endpoint", error))?;
    let current = registered_hello_outcome(
        "registered source hello",
        hello(&source.endpoint, &source.bearer),
    )?;
    if current.device_id != source.device_id {
        crate::nlog!(
            "warn",
            "registered source hello returned a different device identity"
        );
        return Err(PeerCommandCode::IdentityMismatch);
    }
    let granted = match lane {
        RegisteredLane::Clone | RegisteredLane::Delta => current.permissions.allows_read(),
        RegisteredLane::Bidirectional => current.permissions.allows_bidirectional(),
    };
    if !granted {
        return Err(PeerCommandCode::PermissionDenied);
    }
    let descriptor = match lane {
        RegisteredLane::Clone => current.lanes.clone.clone(),
        RegisteredLane::Delta => current.lanes.delta.clone(),
        RegisteredLane::Bidirectional => current.lanes.bidirectional.clone(),
    }
    .ok_or(PeerCommandCode::LaneUnavailable)?;
    Ok(RegisteredSourceConnection {
        source,
        lane: descriptor,
        hello: current,
        completion: PeerCompletionCapability::Unsupported,
    })
}

fn safe_failure(context: &str, error: PeerSyncError) -> PeerCommandCode {
    crate::nlog!("warn", "{context} failed: {error}");
    PeerCommandCode::TransportUnavailable
}

fn registered_hello_outcome(
    context: &str,
    outcome: Result<AuthenticatedPeerHelloOutcome, PeerSyncError>,
) -> Result<PeerHello, PeerCommandCode> {
    match outcome {
        Ok(AuthenticatedPeerHelloOutcome::Hello(hello)) => Ok(hello),
        Ok(AuthenticatedPeerHelloOutcome::AuthorizationExpired) => {
            crate::nlog!("warn", "{context} failed: HTTP 401 Unauthorized");
            Err(PeerCommandCode::AuthorizationExpired)
        }
        Err(error) => Err(safe_failure(context, error)),
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
                Ok(AuthenticatedPeerHelloOutcome::Hello(hello(
                    SOURCE_ID,
                    DevicePermissions::read(),
                )))
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
                Ok(AuthenticatedPeerHelloOutcome::Hello(hello(
                    SOURCE_ID,
                    DevicePermissions::read(),
                )))
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
                Ok(AuthenticatedPeerHelloOutcome::Hello(hello(
                    SOURCE_ID,
                    DevicePermissions::read(),
                )))
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
            |_, _| {
                Ok(AuthenticatedPeerHelloOutcome::Hello(hello(
                    OTHER_SOURCE_ID,
                    DevicePermissions::read(),
                )))
            },
        )
        .unwrap_err();
        assert_eq!(mismatch.code(), "identityMismatch");

        let permission = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Bidirectional,
            |_, _| {
                Ok(AuthenticatedPeerHelloOutcome::Hello(hello(
                    SOURCE_ID,
                    DevicePermissions::read(),
                )))
            },
        )
        .unwrap_err();
        assert_eq!(permission.code(), "permissionDenied");

        let mut no_delta = hello(SOURCE_ID, DevicePermissions::read());
        no_delta.lanes.delta = None;
        let lane = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Delta,
            |_, _| Ok(AuthenticatedPeerHelloOutcome::Hello(no_delta)),
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
            |_, _| Ok(AuthenticatedPeerHelloOutcome::AuthorizationExpired),
        )
        .unwrap_err();
        assert_eq!(unauthorized.code(), "authorizationExpired");

        let string_unauthorized = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Clone,
            |_, _| Err(PeerSyncError::Transport("HTTP 401 Unauthorized".to_owned())),
        )
        .unwrap_err();
        assert_eq!(string_unauthorized.code(), "transportUnavailable");

        let transport = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Clone,
            |_, _| Err(PeerSyncError::Transport("socket detail secret".to_owned())),
        )
        .unwrap_err();
        assert_eq!(transport.code(), "transportUnavailable");
        assert!(!transport.to_string().contains("socket detail secret"));

        let false_unauthorized = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Clone,
            |_, _| Err(PeerSyncError::Transport("HTTP 4010".to_owned())),
        )
        .unwrap_err();
        assert_eq!(false_unauthorized.code(), "transportUnavailable");
    }

    #[test]
    fn registered_local_failures_are_not_reported_as_connection_failures() {
        let target = registered_target_operation_failure(
            "registered clone target",
            PeerSyncError::Storage("local target state is unavailable".to_owned()),
        );
        assert_eq!(target.code(), "operationFailed");
        assert_ne!(target.code(), "transportUnavailable");

        let claim_storage = registered_target_operation_failure(
            "registered clone credential",
            PeerSyncError::Storage("local credential write failed".to_owned()),
        );
        assert_eq!(claim_storage.code(), "operationFailed");
        let claim_validation = registered_target_operation_failure(
            "registered incoming registry",
            PeerSyncError::Validation("local registry is invalid".to_owned()),
        );
        assert_eq!(claim_validation.code(), "operationFailed");
        let claim_transport = registered_target_operation_failure(
            "registered clone claim",
            PeerSyncError::Transport("source disconnected".to_owned()),
        );
        assert_eq!(claim_transport.code(), "transportUnavailable");

        let code = registered_operation_failure(
            "registered delta pull",
            "local expected revision is stale",
        );
        assert_eq!(code, "operationFailed");
        assert_ne!(code, "transportUnavailable");
        assert!(!code.contains("revision"));
    }

    #[test]
    fn a_registered_target_failure_keeps_the_code_its_error_already_carries() {
        for (context, error, expected) in [
            (
                "registered clone target",
                PeerSyncError::Validation(
                    crate::peer_sync::registry_commands::REGISTERED_SOURCE_CHANGED.to_owned(),
                ),
                "sourceChanged",
            ),
            (
                "registered clone registration",
                PeerSyncError::Validation(
                    crate::peer_sync::registry_commands::REGISTRATION_BLOCKED_BY_ACTIVE_WORK
                        .to_owned(),
                ),
                "registrationBlockedByActiveWork",
            ),
            (
                "registered clone target",
                PeerSyncError::Transport("source disconnected".to_owned()),
                "transportUnavailable",
            ),
            (
                "registered clone credential",
                PeerSyncError::Storage("local credential write failed".to_owned()),
                "operationFailed",
            ),
        ] {
            assert_eq!(
                registered_target_operation_failure(context, error).code(),
                expected
            );
        }
    }

    #[test]
    fn registered_hello_and_clone_target_never_serialize_the_bearer() {
        let root = tempfile::tempdir().unwrap();
        register_incoming_source(root.path(), source(DevicePermissions::read())).unwrap();
        let connection = resolve_registered_source_with(
            root.path(),
            SOURCE_ID,
            RegisteredLane::Clone,
            |_, _| {
                Ok(AuthenticatedPeerHelloOutcome::Hello(hello(
                    SOURCE_ID,
                    DevicePermissions::read(),
                )))
            },
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
    fn android_registered_sources_reject_public_or_https_endpoints_before_transport() {
        assert!(validate_android_registered_source_endpoint("http://192.168.4.8:32145").is_ok());
        assert!(validate_android_registered_source_endpoint("http://10.0.0.7:32145").is_ok());
        assert!(validate_android_registered_source_endpoint("https://sync.example.com").is_err());
        assert!(validate_android_registered_source_endpoint("http://8.8.8.8:32145").is_err());
        assert!(validate_android_registered_source_endpoint("http://127.0.0.1:32145").is_err());
    }

    #[test]
    fn android_registered_hello_rejects_public_endpoint_before_transport() {
        let root = tempfile::tempdir().unwrap();
        let mut public = source(DevicePermissions::read());
        public.endpoint = "https://sync.example.com".to_owned();
        register_incoming_source(root.path(), public).unwrap();
        let transport_called = std::cell::Cell::new(false);

        let error = registered_hello_with(root.path(), SOURCE_ID, true, |_, _| {
            transport_called.set(true);
            unreachable!("public Android endpoints must be rejected before transport")
        })
        .unwrap_err();

        assert_eq!(error.code(), "transportUnavailable");
        assert!(!transport_called.get());
    }

    #[test]
    fn android_registered_clone_status_omits_native_connection_secrets() {
        let job_id = "00000000-0000-4000-8000-000000000199";
        let status = crate::peer_sync::android_client::AndroidCloneJobStatus {
            job_id: job_id.to_owned(),
            endpoint: "http://192.168.4.8:32145".to_owned(),
            session_id: SESSION_ID.to_owned(),
            manifest_id: MANIFEST_ID.to_owned(),
            phase: crate::peer_sync::android_client::AndroidCloneJobPhase::Ready,
            completed_bytes: 0,
            total_bytes: None,
            error: Some(format!(
                "transfer failed at {} with {BEARER}",
                "http://192.168.4.8:32145"
            )),
            committed_revision: None,
            backup_path: Some(std::path::PathBuf::from(format!(
                "/data/user/0/app/peer-clone-activation/backups/pre-clone-{MANIFEST_ID}-{job_id}.lossless"
            ))),
        };
        let json = serde_json::to_string(&safe_android_clone_status(SOURCE_ID, &status)).unwrap();
        assert!(json.contains(SOURCE_ID));
        assert!(json.contains("ready"));
        assert!(!json.contains("192.168.4.8"));
        assert!(!json.contains(SESSION_ID));
        assert!(!json.contains(MANIFEST_ID));
        assert!(!json.contains(BEARER));
        assert!(json.contains("transferFailed"));
        assert!(json.contains("backupPath"));
        assert!(json.contains(&format!("pre-clone-{job_id}.lossless")));
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
    #[cfg(desktop)]
    fn registered_clone_primes_the_existing_target_runtime() {
        let root = tempfile::tempdir().unwrap();
        register_incoming_source(root.path(), source(DevicePermissions::read())).unwrap();
        let state = crate::peer_sync::commands::PeerCloneCommandState::default();
        let target = state
            .connect_registered_target(
                &root.path().join("peer-clone"),
                "http://127.0.0.1:32145",
                SESSION_ID,
                MANIFEST_ID,
                SOURCE_ID,
                BEARER,
                PeerCompletionCapability::Unsupported,
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
