use super::command_codes::{finish_peer_command, finish_peer_worker};
use super::lan::{
    validate_device_sync_endpoint, validate_lan_endpoint, LanCloneHostControl,
    PeerCompletionCapability, NAMED_TUNNEL_ORIGIN_UNAVAILABLE,
};
use super::production::prepare_unified_clone_source;
use super::registry_commands::REGISTERED_SOURCE_CHANGED;
use super::{
    activate_downloaded_clone, CloneTargetAdapter, LanCloneClient, LanCloneHost,
    LoopbackCloneClient, LosslessCloneTargetAdapter, PeerSyncError, TransferCancellation,
};
use crate::{
    asset_repository::PayloadCas,
    local_backup::NeverCancelled,
    persistent_store::{self, PersistentStore, StoreError},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
};
use tauri::{AppHandle, Manager, State};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneCapabilities {
    desktop: bool,
    source_ready: bool,
    atomic_activation_ready: bool,
    lossless_backup_ready: bool,
    http_transport_ready: bool,
    large_fixture_passed: bool,
    production_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerCloneGateState {
    desktop: bool,
    source_ready: bool,
    atomic_activation_ready: bool,
    lossless_backup_ready: bool,
    http_transport_ready: bool,
    large_fixture_passed: bool,
}

impl PeerCloneCapabilities {
    fn current() -> Self {
        Self::from_gates(PeerCloneGateState {
            desktop: true,
            source_ready: true,
            atomic_activation_ready: true,
            lossless_backup_ready: true,
            http_transport_ready: true,
            large_fixture_passed: false,
        })
    }

    fn from_gates(gates: PeerCloneGateState) -> Self {
        Self {
            desktop: gates.desktop,
            source_ready: gates.source_ready,
            atomic_activation_ready: gates.atomic_activation_ready,
            lossless_backup_ready: gates.lossless_backup_ready,
            http_transport_ready: gates.http_transport_ready,
            large_fixture_passed: gates.large_fixture_passed,
            production_enabled: gates.desktop
                && gates.source_ready
                && gates.atomic_activation_ready
                && gates.lossless_backup_ready
                && gates.http_transport_ready,
        }
    }
}

#[tauri::command]
pub fn peer_clone_capabilities() -> PeerCloneCapabilities {
    PeerCloneCapabilities::current()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerCloneSourcePhase {
    Idle,
    Prepared,
    Starting,
    Running,
    Stopping,
    Stopped,
}

pub use super::tunnel_lifecycle::{
    PeerTunnelMetadata as PeerCloneTunnelMetadata, PeerTunnelPhase as PeerCloneTunnelPhase,
    PeerTunnelStart as PeerCloneTunnelStart,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneTunnelStatus {
    session_id: Option<String>,
    phase: PeerCloneTunnelPhase,
    tunnel: Option<PeerCloneTunnelMetadata>,
}

impl PeerCloneTunnelStatus {
    fn idle() -> Self {
        Self {
            session_id: None,
            phase: PeerCloneTunnelPhase::Idle,
            tunnel: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneSourceDevice {
    device_id: String,
    verified_bytes: u64,
    current_object: Option<String>,
    last_seen_at: u64,
    revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneSourceStatus {
    session_id: Option<String>,
    manifest_id: Option<String>,
    pairing_uri: Option<String>,
    phase: PeerCloneSourcePhase,
    devices: Vec<PeerCloneSourceDevice>,
    tunnel: Option<PeerCloneTunnelMetadata>,
}

impl PeerCloneSourceStatus {
    fn idle() -> Self {
        Self {
            session_id: None,
            manifest_id: None,
            pairing_uri: None,
            phase: PeerCloneSourcePhase::Idle,
            devices: Vec::new(),
            tunnel: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCloneTargetRequest {
    endpoint: String,
    session_id: String,
    manifest_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneClaimResult {
    pub(crate) source_device_id: String,
    endpoint: String,
    session_id: String,
    manifest_id: String,
}

fn clone_claim_result(
    request: &PeerCloneTargetRequest,
    source_device_id: &str,
) -> PeerCloneClaimResult {
    PeerCloneClaimResult {
        source_device_id: source_device_id.to_owned(),
        endpoint: request.endpoint.clone(),
        session_id: request.session_id.clone(),
        manifest_id: request.manifest_id.clone(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerCloneTargetPhase {
    Idle,
    Downloading,
    Cancelling,
    AwaitingActivation,
    Activating,
    Cancelled,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneTargetStatus {
    phase: PeerCloneTargetPhase,
    completed_bytes: u64,
    total_bytes: Option<u64>,
    error: Option<String>,
}

impl PeerCloneTargetStatus {
    fn idle() -> Self {
        Self {
            phase: PeerCloneTargetPhase::Idle,
            completed_bytes: 0,
            total_bytes: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneFinalizeResult {
    revision: i64,
    warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup_path: Option<PathBuf>,
}

struct VerifiedCloneValidator;

impl<S> super::CloneValidator<S> for VerifiedCloneValidator {
    fn validate(
        &mut self,
        _manifest: &super::CloneManifest,
        _stage: &S,
    ) -> Result<(), PeerSyncError> {
        Ok(())
    }
}

use super::lan::discover_lan_ipv4;
use super::tunnel_lifecycle::{
    FailedPeerTunnel as FailedSourceTunnel, PeerTunnel as SourceTunnel,
    PeerTunnelKind as PeerCloneTunnelKind, PeerTunnelLauncher as SourceTunnelLauncher,
    PeerTunnelLifecycle as SourceTunnelLifecycle, SystemPeerTunnelLauncher,
};

const ACTIVE_SOURCE_MARKER_SCHEMA: &str = "risunest.peer-clone-active-source/v1";
const ACTIVE_SOURCE_MARKER_FILE: &str = "active-source.json";
const MAX_ACTIVE_SOURCE_MARKER_BYTES: u64 = 4 * 1024;
const TARGET_OPERATION_MARKER_SCHEMA: &str = "risunest.peer-clone-target-operation/v1";
const TARGET_OPERATION_MARKER_FILE: &str = "activation-operation.json";
const MAX_TARGET_OPERATION_MARKER_BYTES: u64 = 4 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActiveSourceMarker {
    schema: String,
    directory_id: String,
    session_id: String,
    manifest_id: String,
}

impl ActiveSourceMarker {
    fn new(directory_id: String, session_id: String, manifest_id: String) -> Self {
        Self {
            schema: ACTIVE_SOURCE_MARKER_SCHEMA.to_owned(),
            directory_id,
            session_id,
            manifest_id,
        }
    }

    fn validate(&self) -> Result<(), PeerSyncError> {
        if self.schema != ACTIVE_SOURCE_MARKER_SCHEMA
            || !is_canonical_uuid(&self.directory_id)
            || !is_canonical_uuid(&self.session_id)
            || super::protocol::validate_hash(&self.manifest_id).is_err()
        {
            return Err(PeerSyncError::Validation(
                "invalid prepared clone source marker".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TargetOperationMarker {
    schema: String,
    session_id: String,
    manifest_id: String,
    operation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backup_path: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    completion_acknowledged: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion_capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion_lease_id: Option<String>,
}

impl TargetOperationMarker {
    fn new(request: &PeerCloneTargetRequest) -> Self {
        Self {
            schema: TARGET_OPERATION_MARKER_SCHEMA.to_owned(),
            session_id: request.session_id.clone(),
            manifest_id: request.manifest_id.clone(),
            operation_id: uuid::Uuid::new_v4().to_string(),
            backup_path: None,
            completion_acknowledged: false,
            completion_capability: None,
            completion_lease_id: None,
        }
    }

    fn validate_for(&self, request: &PeerCloneTargetRequest) -> Result<(), PeerSyncError> {
        if self.schema != TARGET_OPERATION_MARKER_SCHEMA
            || self.session_id != request.session_id
            || self.manifest_id != request.manifest_id
            || !is_canonical_uuid(&self.session_id)
            || super::protocol::validate_hash(&self.manifest_id).is_err()
            || !is_canonical_v4_uuid(&self.operation_id)
            || self.operation_id == self.session_id
            || self
                .backup_path
                .as_deref()
                .is_some_and(|path| path != target_backup_relative_path(&self.operation_id))
            || (self.completion_acknowledged && self.backup_path.is_none())
            || match (
                self.completion_capability.as_deref(),
                self.completion_lease_id.as_deref(),
            ) {
                (None, None) => false,
                (Some(super::lan::PEER_COMPLETION_CAPABILITY_V1), Some(lease_id)) => {
                    !is_canonical_v4_uuid(lease_id) || lease_id == self.operation_id
                }
                _ => true,
            }
        {
            return Err(PeerSyncError::Validation(
                "invalid peer clone target operation marker".to_owned(),
            ));
        }
        Ok(())
    }
}

struct SourceRuntime {
    session_id: String,
    manifest_id: String,
    session_root: PathBuf,
    marker_path: PathBuf,
    marker: ActiveSourceMarker,
    host: Option<LanCloneHost>,
    control: LanCloneHostControl,
    tunnel: Option<Box<dyn SourceTunnel>>,
    failed_tunnel: Option<Box<dyn FailedSourceTunnel>>,
    tunnel_metadata: Option<PeerCloneTunnelMetadata>,
    phase: PeerCloneSourcePhase,
    stop_in_progress: bool,
    terminal_cleanup_pending: bool,
    #[cfg(test)]
    stop_pause: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    fail_cleanup_once: bool,
}

/// Hostless clone preparation.  The shared source lifecycle owns this until
/// its `PreparedCloneSession` is moved into the one common listener.
pub(crate) struct PreparedSharedCloneSource {
    prepared: Option<super::PreparedCloneSession>,
    session_root: PathBuf,
    marker_path: PathBuf,
    marker: ActiveSourceMarker,
}

impl PreparedSharedCloneSource {
    pub(crate) fn session_id(&self) -> &str {
        &self.marker.session_id
    }

    pub(crate) fn manifest_id(&self) -> &str {
        &self.marker.manifest_id
    }

    pub(crate) fn take_session(&mut self) -> Result<super::PreparedCloneSession, PeerSyncError> {
        self.prepared.take().ok_or_else(|| {
            PeerSyncError::Protocol("shared clone source session is unavailable".to_owned())
        })
    }

    pub(crate) fn cleanup(&mut self) -> Result<(), PeerSyncError> {
        // Dropping an unhosted session releases its package ownership.  When
        // it has moved into SharedSessionHost, that host is stopped first.
        self.prepared.take();
        cleanup_source_artifacts(&self.session_root, &self.marker_path, &self.marker).and_then(
            |_| {
                sweep_canonical_owned_directories(self.session_root.parent().ok_or_else(|| {
                    PeerSyncError::Storage(
                        "peer clone source session has no owned parent".to_owned(),
                    )
                })?)
            },
        )
    }
}

pub(crate) fn prepare_shared_clone_source(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    peer_root: &Path,
    cancellation: &dyn crate::local_backup::CancellationProbe,
) -> Result<PreparedSharedCloneSource, PeerSyncError> {
    let mut unified = prepare_unified_clone_source(store, cas, peer_root, cancellation)?;
    let operation_id = unified.directory_id()?.to_owned();
    let session_root = unified.session_root().to_path_buf();
    let marker = ActiveSourceMarker::new(
        operation_id,
        unified.session_id()?.to_owned(),
        unified.manifest_id()?.to_owned(),
    );
    let marker_path = peer_root.join(ACTIVE_SOURCE_MARKER_FILE);
    Ok(PreparedSharedCloneSource {
        prepared: Some(unified.take_session()?),
        session_root,
        marker_path,
        marker,
    })
}

impl Drop for SourceRuntime {
    fn drop(&mut self) {
        if let Some(tunnel) = self.tunnel.as_mut() {
            let _ = tunnel.stop();
        }
        if let Some(failure) = self.failed_tunnel.as_mut() {
            let _ = failure.stop();
        }
        if let Some(host) = self.host.as_mut() {
            let _ = host.stop();
        }
    }
}

struct TargetRuntime {
    request: PeerCloneTargetRequest,
    job_root: PathBuf,
    operation_id: String,
    source_device_id: String,
    client: Option<LoopbackCloneClient>,
    cancellation: Option<TransferCancellation>,
    worker: Option<JoinHandle<()>>,
    status: PeerCloneTargetStatus,
    #[cfg(test)]
    fail_finalize_cleanup_once: bool,
    #[cfg(test)]
    fail_finalize_backup_receipt_once: bool,
    #[cfg(test)]
    fail_release_cleanup_once: bool,
    #[cfg(test)]
    fail_finalize_completion_ack_once: bool,
    #[cfg(test)]
    fail_after_remote_completion_once: bool,
}

#[derive(Default)]
struct PeerCloneRuntime {
    source_preparing: bool,
    source_recovery_error: Option<String>,
    source: Option<SourceRuntime>,
    target_claiming: bool,
    #[cfg(test)]
    target_claim_pause: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    registered_target_publish_barriers: Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>,
    target: Option<TargetRuntime>,
}

#[derive(Clone)]
pub struct PeerCloneCommandState {
    runtime: Arc<Mutex<PeerCloneRuntime>>,
    tunnel_launcher: Arc<dyn SourceTunnelLauncher>,
}

// Clears source_preparing on drop so a panic (or any early return) in the
// prepare path cannot wedge every later prepare with "already prepared"
// until app restart. Disarm on the paths that consume the flag under the
// runtime lock.
struct SourcePrepareGuard<'a> {
    runtime: &'a Mutex<PeerCloneRuntime>,
    armed: bool,
}

impl SourcePrepareGuard<'_> {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for SourcePrepareGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Recover a poisoned lock: resetting the plain bool is always safe,
        // and skipping it would wedge the prepare lane permanently.
        self.runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .source_preparing = false;
    }
}

impl Default for PeerCloneCommandState {
    fn default() -> Self {
        Self {
            runtime: Arc::new(Mutex::new(PeerCloneRuntime::default())),
            tunnel_launcher: Arc::new(SystemPeerTunnelLauncher { lane: "clone" }),
        }
    }
}

fn recover_source(peer_root: &Path) -> Result<Option<SourceRuntime>, PeerSyncError> {
    let marker_path = peer_root.join(ACTIVE_SOURCE_MARKER_FILE);
    let recovered = (|| {
        let Some(marker) = read_active_source_marker(&marker_path)? else {
            return Ok(None);
        };
        let session_root = peer_root.join("source-sessions").join(&marker.directory_id);
        let prepared = super::PreparedCloneSession::reopen_lossless(
            &session_root,
            &marker.session_id,
            &marker.manifest_id,
        )?;
        Ok(Some((marker, session_root, prepared)))
    })();
    let Some((marker, session_root, prepared)) = (match recovered {
        Ok(recovered) => recovered,
        Err(PeerSyncError::Validation(_)) => {
            remove_file_if_exists(&marker_path)?;
            return Ok(None);
        }
        Err(error) => return Err(error),
    }) else {
        return Ok(None);
    };
    let session_id = prepared.manifest().session_id.clone();
    let manifest_id = prepared.manifest_id().to_owned();
    let host = LanCloneHost::prepare(prepared);
    let control = host.control();
    Ok(Some(SourceRuntime {
        session_id,
        manifest_id,
        session_root,
        marker_path,
        marker,
        host: Some(host),
        control,
        tunnel: None,
        failed_tunnel: None,
        tunnel_metadata: None,
        phase: PeerCloneSourcePhase::Prepared,
        stop_in_progress: false,
        terminal_cleanup_pending: false,
        #[cfg(test)]
        stop_pause: None,
        #[cfg(test)]
        fail_cleanup_once: false,
    }))
}

impl PeerCloneCommandState {
    #[cfg(test)]
    fn with_tunnel_launcher(tunnel_launcher: Arc<dyn SourceTunnelLauncher>) -> Self {
        Self {
            runtime: Arc::new(Mutex::new(PeerCloneRuntime::default())),
            tunnel_launcher,
        }
    }

    pub(crate) fn initialize(peer_root: &Path) -> Self {
        let state = Self::default();
        if let Err(error) = sweep_unpublished_registered_target_orphans(peer_root) {
            crate::nlog!(
                "warn",
                "failed to sweep unpublished registered peer clone targets: {error}"
            );
        }
        if let Err(error) = sweep_activated_target_residues(peer_root) {
            crate::nlog!(
                "warn",
                "failed to sweep activated peer clone target residues: {error}"
            );
        }
        let source = match recover_source(peer_root) {
            Ok(source) => source,
            Err(error) => {
                state
                    .runtime
                    .lock()
                    .expect("new peer clone runtime")
                    .source_recovery_error = Some(error.to_string());
                return state;
            }
        };
        let Some(source) = source else {
            return state;
        };
        state.runtime.lock().expect("new peer clone runtime").source = Some(source);
        state
    }

    fn lock_runtime(&self) -> Result<MutexGuard<'_, PeerCloneRuntime>, PeerSyncError> {
        self.runtime.lock().map_err(|error| {
            PeerSyncError::Storage(format!("peer clone command state mutex poisoned: {error}"))
        })
    }

    pub(crate) fn shutdown_for_exit(&self) {
        let Some((mut host, mut tunnel, mut failed_tunnel)) =
            self.runtime.lock().ok().and_then(|mut runtime| {
                runtime.source.as_mut().map(|source| {
                    (
                        source.host.take(),
                        source.tunnel.take(),
                        source.failed_tunnel.take(),
                    )
                })
            })
        else {
            return;
        };
        if let Some(tunnel) = tunnel.as_mut() {
            let _ = tunnel.stop();
        }
        if let Some(failed_tunnel) = failed_tunnel.as_mut() {
            let _ = failed_tunnel.stop();
        }
        if let Some(host) = host.as_mut() {
            let _ = host.stop();
        }
    }

    pub(crate) fn revoke_registered_device(&self, device_id: &str) {
        if let Ok(runtime) = self.lock_runtime() {
            if let Some(source) = runtime.source.as_ref() {
                let _ = source.control.revoke(device_id);
            }
        }
    }

    pub(crate) fn configure_v2_registry(
        &self,
        app_root: &Path,
        name: &str,
        permissions: super::device_registry::DevicePermissions,
    ) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let source = runtime.source.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone source is not prepared".to_owned())
        })?;
        if source.phase != PeerCloneSourcePhase::Prepared {
            return Err(PeerSyncError::Protocol(
                "peer clone source is not prepared".to_owned(),
            ));
        }
        source
            .host
            .as_mut()
            .ok_or_else(|| {
                PeerSyncError::Protocol("peer clone source host is unavailable".to_owned())
            })?
            .enable_v2_registry(app_root, name, permissions)
    }

    pub fn prepare_source(
        &self,
        store: &mut PersistentStore,
        cas: &PayloadCas,
        peer_root: &Path,
        cancellation: &dyn crate::local_backup::CancellationProbe,
    ) -> Result<PeerCloneSourceStatus, PeerSyncError> {
        let retry_recovery = {
            let mut runtime = self.lock_runtime()?;
            if runtime.source_preparing
                || runtime
                    .source
                    .as_ref()
                    .is_some_and(|source| source.phase != PeerCloneSourcePhase::Stopped)
            {
                return Err(PeerSyncError::Protocol(
                    "peer clone source is already prepared or running".to_owned(),
                ));
            }
            runtime.source_preparing = true;
            runtime.source_recovery_error.is_some()
        };
        let prepare_guard = SourcePrepareGuard {
            runtime: &self.runtime,
            armed: true,
        };

        if retry_recovery {
            match recover_source(peer_root) {
                Ok(Some(source)) => {
                    let mut runtime = self.lock_runtime()?;
                    runtime.source_preparing = false;
                    runtime.source_recovery_error = None;
                    runtime.source = Some(source);
                    let status = source_status(&mut runtime);
                    drop(runtime);
                    prepare_guard.disarm();
                    return status;
                }
                Ok(None) => {
                    self.lock_runtime()?.source_recovery_error = None;
                }
                Err(error) => {
                    self.lock_runtime()?.source_recovery_error = Some(error.to_string());
                    return Err(error);
                }
            }
        }

        let mut prepared = prepare_shared_clone_source(store, cas, peer_root, cancellation)?;
        let session_id = prepared.session_id().to_owned();
        let manifest_id = prepared.manifest_id().to_owned();
        let session_root = prepared.session_root.clone();
        let marker_path = prepared.marker_path.clone();
        let marker = prepared.marker.clone();
        let host = LanCloneHost::prepare(prepared.take_session()?);
        let control = host.control();
        let mut runtime = self.lock_runtime()?;
        runtime.source_preparing = false;
        runtime.source = Some(SourceRuntime {
            session_id,
            manifest_id,
            session_root,
            marker_path,
            marker,
            host: Some(host),
            control,
            tunnel: None,
            failed_tunnel: None,
            tunnel_metadata: None,
            phase: PeerCloneSourcePhase::Prepared,
            stop_in_progress: false,
            terminal_cleanup_pending: false,
            #[cfg(test)]
            stop_pause: None,
            #[cfg(test)]
            fail_cleanup_once: false,
        });
        let status = source_status(&mut runtime);
        drop(runtime);
        prepare_guard.disarm();
        status
    }

    pub fn start_source(
        &self,
        session_id: &str,
        advertised_ip: Ipv4Addr,
    ) -> Result<PeerCloneSourceStatus, PeerSyncError> {
        if !(advertised_ip.is_private() || advertised_ip.is_link_local()) {
            return Err(PeerSyncError::Validation(
                "peer clone source requires a private or link-local IPv4 address".to_owned(),
            ));
        }
        let mut runtime = self.lock_runtime()?;
        let source = require_source_mut(&mut runtime, session_id)?;
        if source.phase != PeerCloneSourcePhase::Prepared {
            return Err(PeerSyncError::Protocol(
                "peer clone source is not prepared".to_owned(),
            ));
        }
        let host = source.host.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone source host is unavailable".to_owned())
        })?;
        let pairing = host.start()?;
        let address = host.address().ok_or_else(|| {
            PeerSyncError::Transport("peer clone source address is unavailable".to_owned())
        })?;
        let endpoint = format!("http://{advertised_ip}:{}", address.port());
        let pairing_uri = match build_pairing_uri(&endpoint, &pairing) {
            Ok(pairing_uri) => pairing_uri,
            Err(error) => {
                let _ = host.stop();
                return Err(error);
            }
        };
        source.phase = PeerCloneSourcePhase::Running;
        let mut status = source_status(&mut runtime)?;
        status.pairing_uri = Some(pairing_uri);
        Ok(status)
    }

    pub fn start_tunnel(
        &self,
        session_id: &str,
        tunnel_request: PeerCloneTunnelStart,
    ) -> Result<PeerCloneSourceStatus, PeerSyncError> {
        let metadata = match &tunnel_request {
            PeerCloneTunnelStart::Quick => PeerCloneTunnelMetadata::quick(),
            PeerCloneTunnelStart::Named { .. } => PeerCloneTunnelMetadata::named(),
        };
        let mut host = {
            let mut runtime = self.lock_runtime()?;
            let source = require_source_mut(&mut runtime, session_id)?;
            if source.phase != PeerCloneSourcePhase::Prepared {
                return Err(PeerSyncError::Protocol(
                    "peer clone source is not prepared".to_owned(),
                ));
            }
            let host = source.host.take().ok_or_else(|| {
                PeerSyncError::Protocol("peer clone source host is unavailable".to_owned())
            })?;
            source.phase = PeerCloneSourcePhase::Starting;
            source.tunnel_metadata = Some(metadata);
            host
        };

        let origin = match metadata.kind {
            PeerCloneTunnelKind::Quick => host.start_quick_tunnel_origin(),
            PeerCloneTunnelKind::Named => host.start_named_tunnel_origin(),
        };
        let pairing = match origin {
            Ok(pairing) => pairing,
            Err(error) => {
                let mut runtime = self.lock_runtime()?;
                let source = require_source_mut(&mut runtime, session_id)?;
                source.host = Some(host);
                source.phase = PeerCloneSourcePhase::Prepared;
                source.tunnel_metadata = None;
                return Err(
                    if metadata.kind == PeerCloneTunnelKind::Named
                        && error
                            == PeerSyncError::Transport(NAMED_TUNNEL_ORIGIN_UNAVAILABLE.to_owned())
                    {
                        error
                    } else {
                        tunnel_start_error()
                    },
                );
            }
        };

        let mut tunnel = match self.tunnel_launcher.start(tunnel_request, host) {
            Ok(tunnel) => tunnel,
            Err(mut failure) => {
                match failure.recover_host() {
                    Ok(Some(mut host)) => {
                        let stopped = host.stop().is_ok();
                        let mut runtime = self.lock_runtime()?;
                        let source = require_source_mut(&mut runtime, session_id)?;
                        source.host = Some(host);
                        if stopped {
                            source.phase = PeerCloneSourcePhase::Prepared;
                            source.tunnel_metadata = None;
                        } else {
                            source.phase = PeerCloneSourcePhase::Stopping;
                        }
                    }
                    Ok(None) | Err(_) => {
                        let mut runtime = self.lock_runtime()?;
                        let source = require_source_mut(&mut runtime, session_id)?;
                        source.failed_tunnel = Some(failure);
                        source.phase = PeerCloneSourcePhase::Stopping;
                    }
                }
                return Err(tunnel_start_error());
            }
        };

        let endpoint = match validate_lan_endpoint(tunnel.transport_url().as_str()) {
            Ok(endpoint) if endpoint.starts_with("https://") => endpoint,
            _ => {
                return Err(self.abort_started_tunnel(session_id, tunnel));
            }
        };
        let pairing_uri = match build_pairing_uri(&endpoint, &pairing) {
            Ok(pairing_uri) => pairing_uri,
            Err(_) => return Err(self.abort_started_tunnel(session_id, tunnel)),
        };

        let mut runtime = self.lock_runtime()?;
        let source = require_source_mut(&mut runtime, session_id)?;
        if source.phase != PeerCloneSourcePhase::Starting {
            let _ = tunnel.stop();
            return Err(tunnel_start_error());
        }
        source.tunnel = Some(tunnel);
        source.phase = PeerCloneSourcePhase::Running;
        let mut status = source_status(&mut runtime)?;
        if status.phase != PeerCloneSourcePhase::Running {
            return Err(tunnel_start_error());
        }
        status.pairing_uri = Some(pairing_uri);
        Ok(status)
    }

    fn abort_started_tunnel(
        &self,
        session_id: &str,
        mut tunnel: Box<dyn SourceTunnel>,
    ) -> PeerSyncError {
        let stopped = tunnel.stop().is_ok();
        if let Ok(mut runtime) = self.lock_runtime() {
            if let Ok(source) = require_source_mut(&mut runtime, session_id) {
                if stopped {
                    source.phase = match cleanup_source_artifacts(
                        &source.session_root,
                        &source.marker_path,
                        &source.marker,
                    ) {
                        Ok(()) => PeerCloneSourcePhase::Stopped,
                        Err(_) => PeerCloneSourcePhase::Stopping,
                    };
                } else {
                    source.tunnel = Some(tunnel);
                    source.phase = PeerCloneSourcePhase::Stopping;
                }
            }
        }
        tunnel_start_error()
    }

    pub fn source_status(&self) -> Result<PeerCloneSourceStatus, PeerSyncError> {
        let (status, terminal_cleanup) = {
            let mut runtime = self.lock_runtime()?;
            let status = source_status(&mut runtime)?;
            let terminal_cleanup = runtime.source.as_mut().and_then(|source| {
                if source.terminal_cleanup_pending && !source.stop_in_progress {
                    source.terminal_cleanup_pending = false;
                    Some(source.session_id.clone())
                } else {
                    None
                }
            });
            (status, terminal_cleanup)
        };
        let Some(session_id) = terminal_cleanup else {
            return Ok(status);
        };
        let _ = self.stop_source(&session_id);
        let mut runtime = self.lock_runtime()?;
        source_status(&mut runtime)
    }

    pub fn tunnel_status(&self) -> Result<PeerCloneTunnelStatus, PeerSyncError> {
        let _ = self.source_status()?;
        let mut runtime = self.lock_runtime()?;
        let _ = source_status(&mut runtime)?;
        Ok(tunnel_status(&runtime))
    }

    // Test-facing status accessors.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn source_bind_address(&self) -> Result<Option<SocketAddr>, PeerSyncError> {
        Ok(self
            .lock_runtime()?
            .source
            .as_ref()
            .and_then(|source| source.host.as_ref())
            .and_then(LanCloneHost::address))
    }

    pub fn revoke_source_device(
        &self,
        session_id: &str,
        device_id: &str,
    ) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let source = require_source_mut(&mut runtime, session_id)?;
        if source.phase != PeerCloneSourcePhase::Running || !source.control.revoke(device_id) {
            return Err(PeerSyncError::Protocol(
                "peer clone source device is unavailable".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn stop_source(&self, session_id: &str) -> Result<(), PeerSyncError> {
        let (
            mut host,
            mut tunnel,
            mut failed_tunnel,
            session_root,
            marker_path,
            marker,
            stop_pause,
            fail_cleanup,
        ) = {
            let mut runtime = self.lock_runtime()?;
            let source = require_source_mut(&mut runtime, session_id)?;
            if source.phase == PeerCloneSourcePhase::Stopped {
                return Ok(());
            }
            if source.phase == PeerCloneSourcePhase::Starting {
                return Err(PeerSyncError::Protocol(
                    "peer clone tunnel start is in progress".to_owned(),
                ));
            }
            if source.stop_in_progress {
                return Err(PeerSyncError::Protocol(
                    "peer clone source stop is already in progress".to_owned(),
                ));
            }
            source.stop_in_progress = true;
            source.terminal_cleanup_pending = false;
            let host = source.host.take();
            let tunnel = source.tunnel.take();
            let failed_tunnel = source.failed_tunnel.take();
            source.phase = PeerCloneSourcePhase::Stopping;
            (
                host,
                tunnel,
                failed_tunnel,
                source.session_root.clone(),
                source.marker_path.clone(),
                source.marker.clone(),
                #[cfg(test)]
                source.stop_pause.take(),
                #[cfg(not(test))]
                (),
                #[cfg(test)]
                std::mem::take(&mut source.fail_cleanup_once),
                #[cfg(not(test))]
                false,
            )
        };
        #[cfg(test)]
        if let Some(stop_pause) = stop_pause {
            stop_pause.wait();
            stop_pause.wait();
        }
        #[cfg(not(test))]
        let _ = stop_pause;
        let mut stop_error = None;
        if let Some(active_tunnel) = tunnel.as_mut() {
            if active_tunnel.stop().is_ok() {
                tunnel = None;
            } else {
                stop_error = Some(tunnel_stop_error());
            }
        }
        if let Some(start_failure) = failed_tunnel.as_mut() {
            if start_failure.stop().is_ok() {
                failed_tunnel = None;
            } else if stop_error.is_none() {
                stop_error = Some(tunnel_stop_error());
            }
        }
        if let Some(active_host) = host.as_mut() {
            match active_host.stop() {
                Ok(()) => host = None,
                Err(error) if stop_error.is_none() => stop_error = Some(error),
                Err(_) => {}
            }
        }
        let result = if let Some(error) = stop_error {
            Err(error)
        } else if fail_cleanup {
            Err(PeerSyncError::Storage(
                "injected peer clone source cleanup failure".to_owned(),
            ))
        } else {
            cleanup_source_artifacts(&session_root, &marker_path, &marker).and_then(|()| {
                sweep_canonical_owned_directories(session_root.parent().ok_or_else(|| {
                    PeerSyncError::Storage(
                        "peer clone source session has no owned parent".to_owned(),
                    )
                })?)
            })
        };
        let mut runtime = self.lock_runtime()?;
        let source = require_source_mut(&mut runtime, session_id)?;
        source.stop_in_progress = false;
        source.host = host;
        source.tunnel = tunnel;
        source.failed_tunnel = failed_tunnel;
        if result.is_ok() {
            source.phase = PeerCloneSourcePhase::Stopped;
        }
        result
    }

    #[cfg(test)]
    fn pause_source_stop_before_cleanup_for_test(
        &self,
        pause: Arc<std::sync::Barrier>,
    ) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let source = runtime.source.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone source session is unavailable".to_owned())
        })?;
        source.stop_pause = Some(pause);
        Ok(())
    }

    #[cfg(test)]
    fn fail_source_cleanup_once_for_test(&self) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let source = runtime.source.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone source session is unavailable".to_owned())
        })?;
        source.fail_cleanup_once = true;
        Ok(())
    }

    #[cfg(test)]
    fn source_session_root_for_test(&self) -> Result<PathBuf, PeerSyncError> {
        self.lock_runtime()?
            .source
            .as_ref()
            .map(|source| source.session_root.clone())
            .ok_or_else(|| {
                PeerSyncError::Protocol("peer clone source session is unavailable".to_owned())
            })
    }

    pub(crate) fn claim_v2_target(
        &self,
        peer_root: &Path,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
    ) -> Result<PeerCloneClaimResult, PeerSyncError> {
        self.reap_finished_target_worker()?;
        let request = PeerCloneTargetRequest {
            endpoint: endpoint.to_owned(),
            session_id: session_id.to_owned(),
            manifest_id: manifest_id.to_owned(),
        };
        let app_root = peer_root.parent().ok_or_else(|| {
            PeerSyncError::Storage("peer clone target root has no app root".to_owned())
        })?;
        let paths = target_paths(peer_root, &request)?;
        if self.lock_runtime()?.target.is_none() {
            ensure_no_other_persisted_target_job(peer_root, &paths.job_root)?;
        }
        let new_job = !paths.job_root.try_exists()?;
        let rotate_credential = {
            let mut runtime = self.lock_runtime()?;
            if let Some(target) = &runtime.target {
                if !same_target_identity(&target.request, &request)
                    || target.job_root != paths.job_root
                {
                    return Err(PeerSyncError::Protocol(
                        "another peer clone target job is already owned".to_owned(),
                    ));
                }
                if matches!(
                    target.status.phase,
                    PeerCloneTargetPhase::AwaitingActivation
                        | PeerCloneTargetPhase::Activating
                        | PeerCloneTargetPhase::Completed
                ) {
                    return Err(PeerSyncError::Protocol(
                        "peer clone target cannot rotate credentials during activation".to_owned(),
                    ));
                }
                if !matches!(
                    target.status.phase,
                    PeerCloneTargetPhase::Idle
                        | PeerCloneTargetPhase::Failed
                        | PeerCloneTargetPhase::Cancelled
                ) {
                    return Err(PeerSyncError::Protocol(
                        "peer clone target is not ready to repair its pairing".to_owned(),
                    ));
                }
                if target.worker.is_some() {
                    return Err(PeerSyncError::Protocol(
                        "peer clone target worker is still active".to_owned(),
                    ));
                }
            }
            if runtime.target_claiming {
                return Err(PeerSyncError::Protocol(
                    "peer clone target claim is already in progress".to_owned(),
                ));
            }
            runtime.target_claiming = true;
            runtime.target.is_some()
        };
        #[cfg(test)]
        let target_claim_pause = { self.lock_runtime()?.target_claim_pause.take() };
        #[cfg(test)]
        if let Some(pause) = target_claim_pause {
            pause.wait();
            pause.wait();
        }
        let claimed = (|| {
            let persisted_marker = read_target_operation_marker(
                &paths.job_root.join(TARGET_OPERATION_MARKER_FILE),
                &request,
            )?;
            let marker_was_missing = persisted_marker.is_none();
            let pending_marker =
                persisted_marker.unwrap_or_else(|| TargetOperationMarker::new(&request));
            let operation_id = pending_marker.operation_id.clone();
            let lan = LanCloneClient::claim_v2_and_persist_and_register(
                app_root,
                super::device_registry::platform_device_name(),
                &paths.credential,
                endpoint,
                session_id,
                manifest_id,
                claim,
            )?;
            if !lan.matches_target(endpoint, session_id, manifest_id)? {
                return Err(PeerSyncError::Protocol(
                    "persisted peer clone target identity does not match the request".to_owned(),
                ));
            }
            let completion_capability = if new_job {
                registered_clone_completion_capability(&lan)?
            } else {
                marker_completion_capability(&pending_marker)?
            };
            let source_device_id = lan.registered_source_device_id().to_owned();
            let mut client = LoopbackCloneClient::from_lan_with_completion(
                &paths.transfer,
                lan,
                manifest_id,
                completion_capability,
                pending_marker.completion_lease_id.as_deref(),
            )?;
            let completion_lease_id = if completion_capability == PeerCompletionCapability::V1 {
                client.prepare_completion_manifest()?
            } else {
                None
            };
            #[cfg(test)]
            let publish_barriers = {
                self.lock_runtime()?
                    .registered_target_publish_barriers
                    .take()
            };
            {
                let _source_lifecycle =
                    super::registry_commands::lock_registered_source_lifecycle()?;
                require_registered_clone_source_binding(app_root, &paths.credential)?;
                #[cfg(test)]
                if let Some((reached_publish, resume_publish)) = publish_barriers {
                    reached_publish.wait();
                    resume_publish.wait();
                }
                if marker_was_missing {
                    publish_new_target_completion_marker(
                        &paths.job_root,
                        &request,
                        &operation_id,
                        completion_capability,
                        completion_lease_id.as_deref(),
                    )?;
                } else {
                    persist_target_completion_lease(
                        &paths.job_root,
                        &request,
                        &operation_id,
                        completion_capability,
                        completion_lease_id.as_deref(),
                    )?;
                }
            }
            Ok((client, operation_id, source_device_id))
        })();

        let mut runtime = self.lock_runtime()?;
        runtime.target_claiming = false;
        let (client, operation_id, source_device_id) = match claimed {
            Ok(claimed) => claimed,
            Err(error) => {
                if new_job {
                    remove_directory_if_exists(&paths.job_root).map_err(|cleanup| {
                        PeerSyncError::Storage(format!(
                            "{error}; failed to discard unavailable peer clone: {cleanup}"
                        ))
                    })?;
                }
                return Err(error);
            }
        };
        if rotate_credential {
            let target = runtime.target.as_mut().ok_or_else(|| {
                PeerSyncError::Protocol("peer clone target job is unavailable".to_owned())
            })?;
            if !same_target_identity(&target.request, &request)
                || target.job_root != paths.job_root
                || !matches!(
                    target.status.phase,
                    PeerCloneTargetPhase::Idle
                        | PeerCloneTargetPhase::Failed
                        | PeerCloneTargetPhase::Cancelled
                )
                || target.worker.is_some()
            {
                return Err(PeerSyncError::Protocol(
                    "peer clone target changed while repairing its pairing".to_owned(),
                ));
            }
            let result = clone_claim_result(&request, &source_device_id);
            target.request = request;
            target.source_device_id = source_device_id;
            target.client = Some(client);
            target.cancellation = None;
            target.status = PeerCloneTargetStatus::idle();
            return Ok(result);
        }
        let result = clone_claim_result(&request, &source_device_id);
        runtime.target = Some(TargetRuntime {
            request,
            job_root: paths.job_root,
            operation_id,
            source_device_id,
            client: Some(client),
            cancellation: None,
            worker: None,
            status: PeerCloneTargetStatus::idle(),
            #[cfg(test)]
            fail_finalize_cleanup_once: false,
            #[cfg(test)]
            fail_finalize_backup_receipt_once: false,
            #[cfg(test)]
            fail_release_cleanup_once: false,
            #[cfg(test)]
            fail_finalize_completion_ack_once: false,
            #[cfg(test)]
            fail_after_remote_completion_once: false,
        });
        Ok(result)
    }

    pub(crate) fn connect_registered_target(
        &self,
        peer_root: &Path,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        source_device_id: &str,
        bearer: &str,
        completion_capability: PeerCompletionCapability,
    ) -> Result<PeerCloneClaimResult, PeerSyncError> {
        self.reap_finished_target_worker()?;
        let request = PeerCloneTargetRequest {
            endpoint: endpoint.to_owned(),
            session_id: session_id.to_owned(),
            manifest_id: manifest_id.to_owned(),
        };
        let app_root = peer_root.parent().ok_or_else(|| {
            PeerSyncError::Storage("peer clone target root has no app root".to_owned())
        })?;
        let paths = target_paths(peer_root, &request)?;
        if self.lock_runtime()?.target.is_none() {
            ensure_no_other_persisted_target_job(peer_root, &paths.job_root)?;
        }
        let new_job = !paths.job_root.try_exists()?;
        {
            let mut runtime = self.lock_runtime()?;
            if runtime.target_claiming {
                return Err(PeerSyncError::Protocol(
                    "peer clone target claim is already in progress".to_owned(),
                ));
            }
            if let Some(target) = runtime.target.as_mut() {
                if !matches!(
                    target.status.phase,
                    PeerCloneTargetPhase::Failed | PeerCloneTargetPhase::Cancelled
                ) || target.worker.is_some()
                    || target.cancellation.is_some()
                {
                    return Err(PeerSyncError::Protocol(
                        "another peer clone target job is already owned".to_owned(),
                    ));
                }
                #[cfg(test)]
                if std::mem::take(&mut target.fail_release_cleanup_once) {
                    return Err(PeerSyncError::Storage(
                        "injected peer clone target release cleanup failure".to_owned(),
                    ));
                }
                remove_directory_if_exists(&target.job_root)?;
                runtime.target = None;
            }
            runtime.target_claiming = true;
        }
        #[cfg(test)]
        let target_claim_pause = { self.lock_runtime()?.target_claim_pause.take() };
        #[cfg(test)]
        if let Some(pause) = target_claim_pause {
            pause.wait();
            pause.wait();
        }
        let connected = (|| {
            let persisted_marker = read_target_operation_marker(
                &paths.job_root.join(TARGET_OPERATION_MARKER_FILE),
                &request,
            )?;
            let marker_was_missing = persisted_marker.is_none();
            let pending_marker =
                persisted_marker.unwrap_or_else(|| TargetOperationMarker::new(&request));
            let operation_id = pending_marker.operation_id.clone();
            let completion_capability = if new_job {
                completion_capability
            } else {
                marker_completion_capability(&pending_marker)?
            };
            let local_device_id = super::device_registry::load_or_create_device_id(app_root)?;
            let lan = LanCloneClient::from_registered_and_persist(
                &paths.credential,
                endpoint,
                session_id,
                manifest_id,
                &local_device_id,
                source_device_id,
                bearer,
            )?;
            if !lan.matches_registered_credential(
                endpoint,
                session_id,
                manifest_id,
                &local_device_id,
                source_device_id,
                bearer,
            )? {
                return Err(PeerSyncError::Validation(
                    "registered peer clone credential changed while connecting".to_owned(),
                ));
            }
            let mut client = LoopbackCloneClient::from_lan_with_completion(
                &paths.transfer,
                lan,
                manifest_id,
                completion_capability,
                pending_marker.completion_lease_id.as_deref(),
            )?;
            let completion_lease_id = client.prepare_completion_manifest()?;
            #[cfg(test)]
            let publish_barriers = {
                self.lock_runtime()?
                    .registered_target_publish_barriers
                    .take()
            };
            {
                let _source_lifecycle =
                    super::registry_commands::lock_registered_source_lifecycle()?;
                require_registered_clone_source_binding(app_root, &paths.credential)?;
                let source =
                    super::device_registry::incoming_source_by_id(app_root, source_device_id)?;
                if !source.as_ref().is_some_and(|source| {
                    source.device_id == source_device_id
                        && source.endpoint == endpoint
                        && source.bearer == bearer
                        && source.permissions.allows_read()
                }) {
                    crate::nlog!("warn", "registered clone source changed before publication");
                    return Err(PeerSyncError::Validation(
                        REGISTERED_SOURCE_CHANGED.to_owned(),
                    ));
                }
                #[cfg(test)]
                if let Some((reached_publish, resume_publish)) = publish_barriers {
                    reached_publish.wait();
                    resume_publish.wait();
                }
                if marker_was_missing {
                    publish_new_target_completion_marker(
                        &paths.job_root,
                        &request,
                        &operation_id,
                        completion_capability,
                        completion_lease_id.as_deref(),
                    )?;
                } else {
                    persist_target_completion_lease(
                        &paths.job_root,
                        &request,
                        &operation_id,
                        completion_capability,
                        completion_lease_id.as_deref(),
                    )?;
                }
            }
            Ok((client, operation_id))
        })();
        let mut runtime = self.lock_runtime()?;
        runtime.target_claiming = false;
        let (client, operation_id) = match connected {
            Ok(connected) => connected,
            Err(error) => {
                if new_job {
                    remove_directory_if_exists(&paths.job_root).map_err(|cleanup| {
                        PeerSyncError::Storage(format!(
                            "{error}; failed to discard unavailable registered peer clone: {cleanup}"
                        ))
                    })?;
                }
                return Err(error);
            }
        };
        let result = clone_claim_result(&request, source_device_id);
        runtime.target = Some(TargetRuntime {
            request,
            job_root: paths.job_root,
            operation_id,
            source_device_id: source_device_id.to_owned(),
            client: Some(client),
            cancellation: None,
            worker: None,
            status: PeerCloneTargetStatus::idle(),
            #[cfg(test)]
            fail_finalize_cleanup_once: false,
            #[cfg(test)]
            fail_finalize_backup_receipt_once: false,
            #[cfg(test)]
            fail_release_cleanup_once: false,
            #[cfg(test)]
            fail_finalize_completion_ack_once: false,
            #[cfg(test)]
            fail_after_remote_completion_once: false,
        });
        Ok(result)
    }

    pub fn start_target_download(
        &self,
        peer_root: &Path,
        request: PeerCloneTargetRequest,
    ) -> Result<(), PeerSyncError> {
        self.start_target_worker(peer_root, request, false)
    }

    #[cfg(test)]
    fn pause_target_after_verified_chunk_for_test(
        &self,
        pause: Arc<std::sync::Barrier>,
    ) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let target = runtime.target.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target job is unavailable".to_owned())
        })?;
        let client = target.client.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target client is unavailable".to_owned())
        })?;
        client.pause_after_verified_chunk_for_test(pause);
        Ok(())
    }

    #[cfg(test)]
    fn fail_target_after_cas_promotion_once_for_test(&self) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let target = runtime.target.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target job is unavailable".to_owned())
        })?;
        let client = target.client.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target client is unavailable".to_owned())
        })?;
        client.fail_after_cas_promotion_once_for_test();
        Ok(())
    }

    #[cfg(test)]
    fn fail_target_finalize_cleanup_once_for_test(&self) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let target = runtime.target.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target job is unavailable".to_owned())
        })?;
        target.fail_finalize_cleanup_once = true;
        Ok(())
    }

    #[cfg(test)]
    fn fail_target_finalize_backup_receipt_once_for_test(&self) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let target = runtime.target.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target job is unavailable".to_owned())
        })?;
        target.fail_finalize_backup_receipt_once = true;
        Ok(())
    }

    #[cfg(test)]
    fn fail_target_finalize_completion_ack_once_for_test(&self) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let target = runtime.target.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target job is unavailable".to_owned())
        })?;
        target.fail_finalize_completion_ack_once = true;
        Ok(())
    }

    #[cfg(test)]
    fn fail_target_after_remote_completion_once_for_test(&self) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let target = runtime.target.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target job is unavailable".to_owned())
        })?;
        target.fail_after_remote_completion_once = true;
        Ok(())
    }

    #[cfg(test)]
    fn fail_target_release_cleanup_once_for_test(&self) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let target = runtime.target.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target job is unavailable".to_owned())
        })?;
        target.fail_release_cleanup_once = true;
        Ok(())
    }

    #[cfg(test)]
    fn fail_target_activation_ledger_once_for_test(&self) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let target = runtime.target.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target job is unavailable".to_owned())
        })?;
        let client = target.client.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target client is unavailable".to_owned())
        })?;
        client.fail_record_activation_once_for_test();
        Ok(())
    }

    #[cfg(test)]
    fn pause_target_claim_before_network_for_test(
        &self,
        pause: Arc<std::sync::Barrier>,
    ) -> Result<(), PeerSyncError> {
        self.lock_runtime()?.target_claim_pause = Some(pause);
        Ok(())
    }

    #[cfg(test)]
    fn pause_registered_target_publish_once_for_test(
        &self,
        reached_publish: Arc<std::sync::Barrier>,
        resume_publish: Arc<std::sync::Barrier>,
    ) -> Result<(), PeerSyncError> {
        self.lock_runtime()?.registered_target_publish_barriers =
            Some((reached_publish, resume_publish));
        Ok(())
    }

    pub fn resume_target_download(
        &self,
        peer_root: &Path,
        request: PeerCloneTargetRequest,
    ) -> Result<(), PeerSyncError> {
        self.reap_finished_target_worker()?;
        let paths = target_paths(peer_root, &request)?;
        let needs_restore = {
            let mut runtime = self.lock_runtime()?;
            if let Some(target) = &runtime.target {
                if target.request != request || target.job_root != paths.job_root {
                    return Err(PeerSyncError::Protocol(
                        "peer clone target request does not own the active job".to_owned(),
                    ));
                }
                false
            } else {
                if runtime.target_claiming {
                    return Err(PeerSyncError::Protocol(
                        "peer clone target ownership is already being restored".to_owned(),
                    ));
                }
                runtime.target_claiming = true;
                true
            }
        };
        if needs_restore {
            let restored = (|| {
                let lan = LanCloneClient::open_persisted(&paths.credential)?;
                if !lan.matches_target(
                    &request.endpoint,
                    &request.session_id,
                    &request.manifest_id,
                )? {
                    return Err(PeerSyncError::Protocol(
                        "persisted peer clone target identity does not match the request"
                            .to_owned(),
                    ));
                }
                let marker = read_target_operation_marker(
                    &paths.job_root.join(TARGET_OPERATION_MARKER_FILE),
                    &request,
                )?
                .ok_or_else(|| {
                    PeerSyncError::Storage(
                        "peer clone target operation marker is missing".to_owned(),
                    )
                })?;
                let source_device_id = lan.registered_source_device_id().to_owned();
                let client = LoopbackCloneClient::from_lan_with_completion(
                    &paths.transfer,
                    lan,
                    &request.manifest_id,
                    marker_completion_capability(&marker)?,
                    marker.completion_lease_id.as_deref(),
                )?;
                Ok((client, source_device_id))
            })();
            let mut runtime = self.lock_runtime()?;
            runtime.target_claiming = false;
            let (client, source_device_id) = restored?;
            if runtime.target.is_some() {
                return Err(PeerSyncError::Protocol(
                    "another peer clone target job acquired ownership".to_owned(),
                ));
            }
            runtime.target = Some(TargetRuntime {
                request: request.clone(),
                operation_id: load_or_create_target_operation_id(&paths.job_root, &request)?,
                job_root: paths.job_root,
                source_device_id,
                client: Some(client),
                cancellation: None,
                worker: None,
                status: PeerCloneTargetStatus::idle(),
                #[cfg(test)]
                fail_finalize_cleanup_once: false,
                #[cfg(test)]
                fail_finalize_backup_receipt_once: false,
                #[cfg(test)]
                fail_release_cleanup_once: false,
                #[cfg(test)]
                fail_finalize_completion_ack_once: false,
                #[cfg(test)]
                fail_after_remote_completion_once: false,
            });
        }
        self.start_target_worker(peer_root, request, true)
    }

    fn start_target_worker(
        &self,
        peer_root: &Path,
        request: PeerCloneTargetRequest,
        resume: bool,
    ) -> Result<(), PeerSyncError> {
        self.reap_finished_target_worker()?;
        let paths = target_paths(peer_root, &request)?;
        let mut command_runtime = self.lock_runtime()?;
        if command_runtime.target_claiming {
            return Err(PeerSyncError::Protocol(
                "peer clone target claim is already in progress".to_owned(),
            ));
        }
        let (mut client, cancellation) = {
            let target = require_target_mut(&mut command_runtime, &request, &paths.job_root)?;
            let allowed = target.status.phase == PeerCloneTargetPhase::Idle
                || (resume
                    && matches!(
                        target.status.phase,
                        PeerCloneTargetPhase::Cancelled | PeerCloneTargetPhase::Failed
                    ));
            if !allowed {
                return Err(PeerSyncError::Protocol(
                    "peer clone target is not ready to download".to_owned(),
                ));
            }
            let client = target.client.take().ok_or_else(|| {
                PeerSyncError::Protocol("peer clone target client is unavailable".to_owned())
            })?;
            let cancellation = TransferCancellation::new();
            target.cancellation = Some(cancellation.clone());
            target.status = PeerCloneTargetStatus {
                phase: PeerCloneTargetPhase::Downloading,
                completed_bytes: target.status.completed_bytes,
                total_bytes: target.status.total_bytes,
                error: None,
            };
            (client, cancellation)
        };

        let runtime = Arc::clone(&self.runtime);
        let worker_request = request.clone();
        let worker_cancellation = cancellation.clone();
        let handle = thread::spawn(move || {
            let all_objects_verified = match client.all_objects_verified(&worker_cancellation) {
                Ok(verified) => verified,
                Err(error) => {
                    finish_target_worker(&runtime, &worker_request, client, Err(error), None);
                    return;
                }
            };
            let (verified_bytes, total_bytes) = match client.transfer_progress() {
                Ok(progress) => progress,
                Err(error) => {
                    finish_target_worker(&runtime, &worker_request, client, Err(error), None);
                    return;
                }
            };
            update_target_progress(&runtime, &worker_request, verified_bytes, total_bytes);
            if all_objects_verified {
                if let Err(error) = client.report_terminal_progress() {
                    finish_target_worker(&runtime, &worker_request, client, Err(error), None);
                    return;
                }
                finish_target_worker(
                    &runtime,
                    &worker_request,
                    client,
                    Ok(()),
                    Some((verified_bytes, total_bytes)),
                );
                return;
            }
            let progress_runtime = Arc::clone(&runtime);
            let progress_request = worker_request.clone();
            let result = client.download_with_progress(&worker_cancellation, move |transferred| {
                update_target_progress(
                    &progress_runtime,
                    &progress_request,
                    verified_bytes.saturating_add(transferred).min(total_bytes),
                    total_bytes,
                );
            });
            let final_progress = client.transfer_progress().ok();
            let result = if worker_cancellation.is_cancelled() {
                Err(PeerSyncError::Cancelled)
            } else {
                result.map(|_| ())
            };
            finish_target_worker(&runtime, &worker_request, client, result, final_progress);
        });
        let target = require_target_mut(&mut command_runtime, &request, &paths.job_root)?;
        target.worker = Some(handle);
        Ok(())
    }

    pub fn cancel_target(&self, request: &PeerCloneTargetRequest) -> Result<(), PeerSyncError> {
        let worker = {
            let mut runtime = self.lock_runtime()?;
            let target = require_target_request_mut(&mut runtime, request)?;
            if target.status.phase != PeerCloneTargetPhase::Downloading {
                return Err(PeerSyncError::Protocol(
                    "peer clone target is not downloading".to_owned(),
                ));
            }
            target
                .cancellation
                .as_ref()
                .ok_or_else(|| {
                    PeerSyncError::Protocol(
                        "peer clone target cancellation is unavailable".to_owned(),
                    )
                })?
                .cancel();
            target.status.phase = PeerCloneTargetPhase::Cancelling;
            target.worker.take()
        };
        if let Err(error) = join_target_worker(worker) {
            let mut runtime = self.lock_runtime()?;
            let target = require_target_request_mut(&mut runtime, request)?;
            target.cancellation = None;
            target.status.phase = PeerCloneTargetPhase::Failed;
            target.status.error = Some(error.to_string());
            return Err(error);
        }
        let mut runtime = self.lock_runtime()?;
        let target = require_target_request_mut(&mut runtime, request)?;
        target.cancellation = None;
        target.status.phase = PeerCloneTargetPhase::Cancelled;
        target.status.error = None;
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn target_status_current(&self) -> Result<PeerCloneTargetStatus, PeerSyncError> {
        Ok(self
            .lock_runtime()?
            .target
            .as_ref()
            .map(|target| target.status.clone())
            .unwrap_or_else(PeerCloneTargetStatus::idle))
    }

    pub fn target_status(
        &self,
        request: &PeerCloneTargetRequest,
    ) -> Result<PeerCloneTargetStatus, PeerSyncError> {
        let runtime = self.lock_runtime()?;
        let target = runtime.target.as_ref().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target job is unavailable".to_owned())
        })?;
        if &target.request != request {
            return Err(PeerSyncError::Protocol(
                "peer clone target request does not own the active job".to_owned(),
            ));
        }
        Ok(target.status.clone())
    }

    pub fn release_target(&self, request: &PeerCloneTargetRequest) -> Result<(), PeerSyncError> {
        self.reap_finished_target_worker()?;
        let mut runtime = self.lock_runtime()?;
        let Some(target) = runtime.target.as_mut() else {
            return Ok(());
        };
        if &target.request != request {
            return Err(PeerSyncError::Protocol(
                "peer clone target request does not own the active job".to_owned(),
            ));
        }
        if target.status.phase != PeerCloneTargetPhase::Completed {
            return Err(PeerSyncError::Protocol(
                "peer clone target is not ready for release".to_owned(),
            ));
        }
        if target.worker.is_some() {
            return Err(PeerSyncError::Protocol(
                "peer clone target worker is still active".to_owned(),
            ));
        }
        let peer_root = target
            .job_root
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| {
                PeerSyncError::Storage("peer clone target job has no peer root".to_owned())
            })?;
        let app_root = peer_root.parent().ok_or_else(|| {
            PeerSyncError::Storage("peer clone target root has no app root".to_owned())
        })?;
        if target.client.is_none() {
            return Err(PeerSyncError::Protocol(
                "peer clone target client is unavailable".to_owned(),
            ));
        }
        if !target_completion_is_durable(
            app_root,
            &target.job_root,
            request,
            &target.operation_id,
            &target.source_device_id,
        )? {
            return Err(PeerSyncError::Protocol(
                "peer clone target completion is not durably accounted".to_owned(),
            ));
        }
        #[cfg(test)]
        if std::mem::take(&mut target.fail_release_cleanup_once) {
            return Err(PeerSyncError::Storage(
                "injected peer clone target release cleanup failure".to_owned(),
            ));
        }
        remove_directory_if_exists(&target.job_root)?;
        runtime.target = None;
        Ok(())
    }

    pub fn finalize_target(
        &self,
        store: &mut PersistentStore,
        cas: &PayloadCas,
        peer_root: &Path,
        request: &PeerCloneTargetRequest,
    ) -> Result<PeerCloneFinalizeResult, PeerSyncError> {
        let paths = target_paths(peer_root, request)?;
        let (worker, operation_id, fail_cleanup_after_commit, fail_backup_receipt) = {
            let mut runtime = self.lock_runtime()?;
            let target = require_target_mut(&mut runtime, request, &paths.job_root)?;
            if target.status.phase != PeerCloneTargetPhase::AwaitingActivation {
                return Err(PeerSyncError::Protocol(
                    "peer clone target is not awaiting activation".to_owned(),
                ));
            }
            target.status.phase = PeerCloneTargetPhase::Activating;
            (
                target.worker.take(),
                target.operation_id.clone(),
                #[cfg(test)]
                std::mem::take(&mut target.fail_finalize_cleanup_once),
                #[cfg(not(test))]
                false,
                #[cfg(test)]
                std::mem::take(&mut target.fail_finalize_backup_receipt_once),
                #[cfg(not(test))]
                false,
            )
        };
        if let Err(error) = join_target_worker(worker) {
            let mut runtime = self.lock_runtime()?;
            let target = require_target_mut(&mut runtime, request, &paths.job_root)?;
            target.status.phase = PeerCloneTargetPhase::Failed;
            target.status.error = Some(error.to_string());
            return Err(error);
        }
        let mut client = {
            let mut runtime = self.lock_runtime()?;
            let target = require_target_mut(&mut runtime, request, &paths.job_root)?;
            match target.client.take() {
                Some(client) => client,
                None => {
                    let error = PeerSyncError::Protocol(
                        "peer clone target client is unavailable".to_owned(),
                    );
                    target.status.phase = PeerCloneTargetPhase::Failed;
                    target.status.error = Some(error.to_string());
                    return Err(error);
                }
            }
        };
        let activation_root = peer_root.join("activation");
        let result = (|| {
            let expected_revision = store.revision().map_err(store_error)?;
            let mut backup_observer = |backup_path: &Path| {
                if fail_backup_receipt {
                    Err(PeerSyncError::Storage(
                        "injected peer clone backup receipt failure".to_owned(),
                    ))
                } else {
                    persist_target_backup_receipt(
                        &paths.job_root,
                        peer_root,
                        request,
                        &operation_id,
                        backup_path,
                    )
                }
            };
            let mut target = LosslessCloneTargetAdapter::new_with_owned_backup_observer(
                store,
                cas,
                &activation_root,
                expected_revision,
                &NeverCancelled,
                &request.manifest_id,
                &operation_id,
                &mut backup_observer,
            )?;
            if let Some(backup_path) =
                recover_target_backup_receipt(&paths.job_root, peer_root, request, &operation_id)?
            {
                target.recover_owned_committed_backup(
                    &request.manifest_id,
                    &operation_id,
                    &backup_path,
                )?;
            }
            #[cfg(test)]
            if fail_cleanup_after_commit {
                target.fail_cleanup_after_commit_once_for_test();
            }
            #[cfg(not(test))]
            let _ = fail_cleanup_after_commit;
            let mut validator = VerifiedCloneValidator;
            let activation = activate_downloaded_clone(&mut client, &mut target, &mut validator);
            let outcome = match activation {
                Ok(()) | Err(PeerSyncError::AlreadyActivated) => Ok(None),
                Err(error) => match target.active_manifest_id() {
                    Ok(Some(active))
                        if active == request.manifest_id
                            && target.committed_backup_path().is_some() =>
                    {
                        Ok(Some(bounded_finalize_warning(&error)))
                    }
                    Ok(_) => Err(error),
                    Err(reconcile) => Err(PeerSyncError::Storage(format!(
                        "{error}; failed to reconcile peer clone activation: {reconcile}"
                    ))),
                },
            };
            let backup_path = target.committed_backup_path().map(Path::to_path_buf);
            drop(target);
            match outcome {
                Ok(warning) => Ok(PeerCloneFinalizeResult {
                    revision: store.revision().map_err(store_error)?,
                    warning,
                    backup_path,
                }),
                Err(error) => Err(error),
            }
        })();
        let (completion, fail_after_remote_completion, fail_completion_ack) = {
            let mut runtime = self.lock_runtime()?;
            let target = require_target_mut(&mut runtime, request, &paths.job_root)?;
            let completion = if result.is_ok() {
                (|| {
                    let marker = read_target_operation_marker(
                        &paths.job_root.join(TARGET_OPERATION_MARKER_FILE),
                        request,
                    )?
                    .ok_or_else(|| {
                        PeerSyncError::Storage(
                            "peer clone target operation marker is missing".to_owned(),
                        )
                    })?;
                    if recover_target_backup_receipt(
                        &paths.job_root,
                        peer_root,
                        request,
                        &operation_id,
                    )?
                    .is_none()
                    {
                        return Err(PeerSyncError::Validation(
                            "registered peer clone completion backup receipt is missing".to_owned(),
                        ));
                    }
                    let (ledger_completed, ledger_total) = client.transfer_progress()?;
                    if ledger_completed != ledger_total
                        || target.status.completed_bytes != ledger_completed
                        || target.status.total_bytes != Some(ledger_total)
                    {
                        return Err(PeerSyncError::Storage(
                            "peer clone completion bytes differ from its verified transfer ledger"
                                .to_owned(),
                        ));
                    }
                    Ok(Some((
                        marker,
                        target.source_device_id.clone(),
                        ledger_total,
                    )))
                })()
            } else {
                Ok(None)
            };
            target.client = Some(client);
            target.cancellation = None;
            (
                completion,
                #[cfg(test)]
                std::mem::take(&mut target.fail_after_remote_completion_once),
                #[cfg(not(test))]
                false,
                #[cfg(test)]
                std::mem::take(&mut target.fail_finalize_completion_ack_once),
                #[cfg(not(test))]
                false,
            )
        };
        let activation_succeeded = result.is_ok();
        let result = result.and_then(|finalized| {
            let app_root = peer_root.parent().ok_or_else(|| {
                PeerSyncError::Storage("peer clone target root has no app root".to_owned())
            })?;
            if let Some((marker, source_device_id, useful_bytes)) = completion? {
                complete_target_accounting(
                    app_root,
                    &paths.credential,
                    request,
                    &operation_id,
                    &marker,
                    &source_device_id,
                    useful_bytes,
                    fail_after_remote_completion,
                )?;
            }
            if fail_completion_ack {
                return Err(PeerSyncError::Storage(
                    "injected peer clone completion acknowledgement failure".to_owned(),
                ));
            }
            persist_target_completion_acknowledgement(&paths.job_root, request, &operation_id)?;
            Ok(finalized)
        });
        {
            let mut runtime = self.lock_runtime()?;
            let target = require_target_mut(&mut runtime, request, &paths.job_root)?;
            match &result {
                Ok(_) => {
                    target.status.phase = PeerCloneTargetPhase::Completed;
                    target.status.error = None;
                }
                Err(error) if activation_succeeded => {
                    target.status.phase = PeerCloneTargetPhase::AwaitingActivation;
                    target.status.error = Some(error.to_string());
                }
                Err(error) => {
                    target.status.phase = if matches!(error, PeerSyncError::Cancelled) {
                        PeerCloneTargetPhase::Cancelled
                    } else {
                        PeerCloneTargetPhase::Failed
                    };
                    target.status.error = Some(error.to_string());
                }
            }
        }
        result
    }

    fn reap_finished_target_worker(&self) -> Result<(), PeerSyncError> {
        let worker = {
            let mut runtime = self.lock_runtime()?;
            runtime.target.as_mut().and_then(|target| {
                if target.worker.as_ref().is_some_and(JoinHandle::is_finished) {
                    target.worker.take()
                } else {
                    None
                }
            })
        };
        join_target_worker(worker)
    }
}

struct TargetPaths {
    job_root: PathBuf,
    credential: PathBuf,
    transfer: PathBuf,
}

fn same_target_identity(left: &PeerCloneTargetRequest, right: &PeerCloneTargetRequest) -> bool {
    left.session_id == right.session_id && left.manifest_id == right.manifest_id
}

fn target_paths(
    peer_root: &Path,
    request: &PeerCloneTargetRequest,
) -> Result<TargetPaths, PeerSyncError> {
    let session = uuid::Uuid::parse_str(&request.session_id).map_err(|_| {
        PeerSyncError::Validation("peer clone target session identity is invalid".to_owned())
    })?;
    if session.to_string() != request.session_id {
        return Err(PeerSyncError::Validation(
            "peer clone target session identity is not canonical".to_owned(),
        ));
    }
    super::protocol::validate_hash(&request.manifest_id)?;
    let job_root = peer_root.join("targets").join(&request.session_id);
    Ok(TargetPaths {
        credential: job_root.join("credential.json"),
        transfer: job_root.join("transfer"),
        job_root,
    })
}

fn load_or_create_target_operation_id(
    job_root: &Path,
    request: &PeerCloneTargetRequest,
) -> Result<String, PeerSyncError> {
    super::maintenance::with_backup_reference_lifecycle(|| {
        let path = job_root.join(TARGET_OPERATION_MARKER_FILE);
        if let Some(bytes) = read_target_operation_marker_bytes(&path)? {
            if let Ok(marker) = serde_json::from_slice::<TargetOperationMarker>(&bytes) {
                if marker.validate_for(request).is_ok() {
                    return Ok(marker.operation_id);
                }
            }
        }
        let marker = TargetOperationMarker::new(request);
        write_target_operation_marker(&path, &marker, request)?;
        Ok(marker.operation_id)
    })
}

fn target_completion_state(
    capability: PeerCompletionCapability,
    completion_lease_id: Option<&str>,
) -> Result<(Option<String>, Option<String>), PeerSyncError> {
    match (capability, completion_lease_id) {
        (PeerCompletionCapability::Unsupported, None) => Ok((None, None)),
        (PeerCompletionCapability::V1, Some(lease_id)) if is_canonical_v4_uuid(lease_id) => Ok((
            Some(super::lan::PEER_COMPLETION_CAPABILITY_V1.to_owned()),
            Some(lease_id.to_owned()),
        )),
        _ => Err(PeerSyncError::Protocol(
            "peer clone completion capability did not provide an exact lease".to_owned(),
        )),
    }
}

fn publish_new_target_completion_marker(
    job_root: &Path,
    request: &PeerCloneTargetRequest,
    operation_id: &str,
    capability: PeerCompletionCapability,
    completion_lease_id: Option<&str>,
) -> Result<(), PeerSyncError> {
    let marker_path = job_root.join(TARGET_OPERATION_MARKER_FILE);
    if read_target_operation_marker(&marker_path, request)?.is_some() {
        return Err(PeerSyncError::Validation(
            "peer clone target operation was published concurrently".to_owned(),
        ));
    }
    let (completion_capability, completion_lease_id) =
        target_completion_state(capability, completion_lease_id)?;
    let mut marker = TargetOperationMarker::new(request);
    marker.operation_id = operation_id.to_owned();
    marker.completion_capability = completion_capability;
    marker.completion_lease_id = completion_lease_id;
    write_target_operation_marker(&marker_path, &marker, request)
}

fn persist_target_completion_lease(
    job_root: &Path,
    request: &PeerCloneTargetRequest,
    operation_id: &str,
    capability: PeerCompletionCapability,
    completion_lease_id: Option<&str>,
) -> Result<(), PeerSyncError> {
    let marker_path = job_root.join(TARGET_OPERATION_MARKER_FILE);
    let mut marker = read_target_operation_marker(&marker_path, request)?.ok_or_else(|| {
        PeerSyncError::Storage("peer clone target operation marker is missing".to_owned())
    })?;
    if marker.operation_id != operation_id {
        return Err(PeerSyncError::Validation(
            "peer clone completion lease belongs to another local operation".to_owned(),
        ));
    }
    let proposed = target_completion_state(capability, completion_lease_id)?;
    if marker.completion_capability.is_some() || marker.completion_lease_id.is_some() {
        if (
            marker.completion_capability.as_ref(),
            marker.completion_lease_id.as_ref(),
        ) != (proposed.0.as_ref(), proposed.1.as_ref())
        {
            return Err(PeerSyncError::Validation(
                "peer clone completion lease changed while resuming".to_owned(),
            ));
        }
        return Ok(());
    }
    marker.completion_capability = proposed.0;
    marker.completion_lease_id = proposed.1;
    write_target_operation_marker(&marker_path, &marker, request)
}

fn require_registered_clone_source_binding(
    app_root: &Path,
    credential_path: &Path,
) -> Result<(), PeerSyncError> {
    let credential = LanCloneClient::open_persisted(credential_path)?;
    let source_device_id = credential.registered_source_device_id();
    let (endpoint, session_id, manifest_id) = credential.target_identity()?;
    let local_device_id = super::device_registry::load_or_create_device_id(app_root)?;
    let Some(source) = super::device_registry::incoming_source_by_id(app_root, source_device_id)?
    else {
        crate::nlog!(
            "warn",
            "registered clone credential no longer binds a registered source"
        );
        return Err(PeerSyncError::Validation(
            REGISTERED_SOURCE_CHANGED.to_owned(),
        ));
    };
    if source.device_id != source_device_id
        || source.endpoint != endpoint
        || !source.permissions.allows_read()
        || !credential.matches_registered_credential(
            endpoint,
            session_id,
            manifest_id,
            &local_device_id,
            source_device_id,
            &source.bearer,
        )?
    {
        crate::nlog!(
            "warn",
            "registered clone credential no longer matches the registered source"
        );
        return Err(PeerSyncError::Validation(
            REGISTERED_SOURCE_CHANGED.to_owned(),
        ));
    }
    Ok(())
}

fn marker_completion_capability(
    marker: &TargetOperationMarker,
) -> Result<PeerCompletionCapability, PeerSyncError> {
    match marker.completion_capability.as_deref() {
        None if marker.completion_lease_id.is_none() => Ok(PeerCompletionCapability::Unsupported),
        Some(super::lan::PEER_COMPLETION_CAPABILITY_V1) if marker.completion_lease_id.is_some() => {
            Ok(PeerCompletionCapability::V1)
        }
        _ => Err(PeerSyncError::Validation(
            "peer clone target completion state is inconsistent".to_owned(),
        )),
    }
}

fn target_completion_delivery(
    source_device_id: &str,
    completion_lease_id: &str,
    manifest_id: &str,
    useful_bytes: u64,
) -> super::device_registry::PendingCompletionDelivery {
    super::device_registry::PendingCompletionDelivery {
        source_device_id: source_device_id.to_owned(),
        lane: super::device_registry::CompletionLane::Clone
            .as_str()
            .to_owned(),
        completion_lease_id: completion_lease_id.to_owned(),
        manifest_id: manifest_id.to_owned(),
        useful_bytes,
        receipt_id: super::device_registry::completion_receipt_id(
            "clone",
            completion_lease_id,
            manifest_id,
        ),
    }
}

fn complete_target_accounting(
    app_root: &Path,
    credential_path: &Path,
    request: &PeerCloneTargetRequest,
    operation_id: &str,
    marker: &TargetOperationMarker,
    source_device_id: &str,
    useful_bytes: u64,
    fail_after_remote_completion: bool,
) -> Result<(), PeerSyncError> {
    match marker_completion_capability(marker)? {
        PeerCompletionCapability::Unsupported => {
            let receipt_id = super::device_registry::completion_receipt_id(
                "clone",
                operation_id,
                &request.manifest_id,
            );
            super::device_registry::record_incoming_completed_operation_once(
                app_root,
                source_device_id,
                &receipt_id,
                useful_bytes,
            )?;
            Ok(())
        }
        PeerCompletionCapability::V1 => {
            let completion_lease_id = marker.completion_lease_id.as_deref().ok_or_else(|| {
                PeerSyncError::Validation(
                    "peer clone target completion lease is missing".to_owned(),
                )
            })?;
            let delivery = target_completion_delivery(
                source_device_id,
                completion_lease_id,
                &request.manifest_id,
                useful_bytes,
            );
            let delivered = super::client::prepare_and_deliver_registered_clone_completion(
                app_root,
                credential_path,
                &delivery,
            )?;
            if delivered && fail_after_remote_completion {
                return Err(PeerSyncError::Storage(
                    "injected failure after remote clone completion".to_owned(),
                ));
            }
            if delivered {
                super::device_registry::finalize_incoming_completion_delivery(app_root, &delivery)?;
            }
            if !super::device_registry::incoming_completion_is_durable(app_root, &delivery)? {
                return Err(PeerSyncError::Validation(
                    "peer clone completion is not durable".to_owned(),
                ));
            }
            Ok(())
        }
    }
}

fn registered_clone_completion_capability(
    lan: &LanCloneClient,
) -> Result<PeerCompletionCapability, PeerSyncError> {
    let source_device_id = lan.registered_source_device_id();
    let (_, session_id, manifest_id) = lan.target_identity()?;
    let observed = lan.hello_with_capabilities()?;
    if observed.hello.device_id != source_device_id
        || !observed.hello.permissions.allows_read()
        || !observed
            .hello
            .lanes
            .clone
            .as_ref()
            .is_some_and(|lane| lane.session_id == session_id && lane.manifest_id == manifest_id)
    {
        return Err(PeerSyncError::Protocol(
            "registered peer clone capability hello changed its target identity".to_owned(),
        ));
    }
    Ok(observed.completion)
}

fn read_target_operation_marker(
    path: &Path,
    request: &PeerCloneTargetRequest,
) -> Result<Option<TargetOperationMarker>, PeerSyncError> {
    let Some(bytes) = read_target_operation_marker_bytes(path)? else {
        return Ok(None);
    };
    let marker: TargetOperationMarker = serde_json::from_slice(&bytes)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    marker.validate_for(request)?;
    Ok(Some(marker))
}

fn read_target_operation_marker_bytes(path: &Path) -> Result<Option<Vec<u8>>, PeerSyncError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file()
        || metadata_is_symlink_or_reparse(&metadata)
        || metadata.len() > MAX_TARGET_OPERATION_MARKER_BYTES
    {
        return Err(PeerSyncError::Validation(
            "invalid peer clone target operation marker file".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take(MAX_TARGET_OPERATION_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TARGET_OPERATION_MARKER_BYTES {
        return Err(PeerSyncError::Validation(
            "peer clone target operation marker exceeds its bound".to_owned(),
        ));
    }
    Ok(Some(bytes))
}

fn target_backup_relative_path(operation_id: &str) -> String {
    format!("activation/backups/pre-clone-{operation_id}.lossless")
}

fn target_backup_path(peer_root: &Path, operation_id: &str) -> PathBuf {
    peer_root.join(target_backup_relative_path(operation_id))
}

fn ensure_no_other_persisted_target_job(
    peer_root: &Path,
    candidate_job_root: &Path,
) -> Result<(), PeerSyncError> {
    super::maintenance::with_backup_reference_lifecycle(|| {
        let targets_root = peer_root.join("targets");
        if !ordinary_directory(peer_root)? || !ordinary_directory(&targets_root)? {
            return Ok(());
        }
        for entry in fs::read_dir(&targets_root)? {
            let entry = entry?;
            if entry.path() == candidate_job_root {
                continue;
            }
            let Some(session_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_canonical_uuid(&session_id) || !ordinary_directory(&entry.path())? {
                continue;
            }
            let Some(bytes) = read_target_operation_marker_bytes(
                &entry.path().join(TARGET_OPERATION_MARKER_FILE),
            )?
            else {
                continue;
            };
            let marker: TargetOperationMarker = serde_json::from_slice(&bytes)
                .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
            marker.validate_for(&PeerCloneTargetRequest {
                endpoint: String::new(),
                session_id,
                manifest_id: marker.manifest_id.clone(),
            })?;
            return Err(PeerSyncError::Protocol(
                "another persisted peer clone target job is unresolved".to_owned(),
            ));
        }
        Ok(())
    })
}

pub(crate) fn retryable_target_operation_references_backup(
    peer_root: &Path,
    backup: &Path,
) -> Result<bool, PeerSyncError> {
    let targets_root = peer_root.join("targets");
    let metadata = match fs::symlink_metadata(&targets_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || metadata_is_symlink_or_reparse(&metadata) {
        return Err(PeerSyncError::Validation(
            "peer clone target root is not a plain directory".to_owned(),
        ));
    }
    let backup = fs::canonicalize(backup)?;
    for entry in fs::read_dir(targets_root)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() || metadata_is_symlink_or_reparse(&metadata) {
            return Err(PeerSyncError::Validation(
                "peer clone target job root is not a plain directory".to_owned(),
            ));
        }
        let marker_path = entry.path().join(TARGET_OPERATION_MARKER_FILE);
        let Some(bytes) = read_target_operation_marker_bytes(&marker_path)? else {
            continue;
        };
        let marker: TargetOperationMarker = serde_json::from_slice(&bytes)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        let request = PeerCloneTargetRequest {
            endpoint: String::new(),
            session_id: marker.session_id.clone(),
            manifest_id: marker.manifest_id.clone(),
        };
        marker.validate_for(&request)?;
        if entry.file_name() != marker.session_id.as_str() {
            return Err(PeerSyncError::Validation(
                "peer clone target operation is outside its session root".to_owned(),
            ));
        }
        let expected = target_backup_path(peer_root, &marker.operation_id);
        let expected = match fs::canonicalize(expected) {
            Ok(expected) => expected,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if expected == backup {
            let exact_receipt = marker.backup_path.as_deref()
                == Some(target_backup_relative_path(&marker.operation_id).as_str());
            return Ok(!exact_receipt || !marker.completion_acknowledged);
        }
    }
    Ok(false)
}

/// `source_device_id` of `None` asks the same question about any registered source.
pub(crate) fn registered_clone_source_is_active(
    app_root: &Path,
    source_device_id: Option<&str>,
) -> Result<bool, PeerSyncError> {
    let targets_root = app_root.join("peer-clone").join("targets");
    let metadata = match fs::symlink_metadata(&targets_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || metadata_is_symlink_or_reparse(&metadata) {
        return Err(PeerSyncError::Validation(
            "peer clone target root is not a plain directory".to_owned(),
        ));
    }
    for entry in fs::read_dir(&targets_root)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() || metadata_is_symlink_or_reparse(&metadata) {
            return Err(PeerSyncError::Validation(
                "peer clone target job root is not a plain directory".to_owned(),
            ));
        }
        let Some(session_id) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(PeerSyncError::Validation(
                "peer clone target session identity is invalid".to_owned(),
            ));
        };
        let marker_path = entry.path().join(TARGET_OPERATION_MARKER_FILE);
        let Some(bytes) = read_target_operation_marker_bytes(&marker_path)? else {
            continue;
        };
        let marker: TargetOperationMarker = serde_json::from_slice(&bytes)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        let request = PeerCloneTargetRequest {
            endpoint: String::new(),
            session_id: session_id.clone(),
            manifest_id: marker.manifest_id.clone(),
        };
        marker.validate_for(&request)?;
        let credential = LanCloneClient::open_persisted(&entry.path().join("credential.json"))?;
        let (_, credential_session_id, credential_manifest_id) = credential.target_identity()?;
        if credential_session_id != session_id || credential_manifest_id != marker.manifest_id {
            return Err(PeerSyncError::Validation(
                "peer clone target identity is inconsistent".to_owned(),
            ));
        }
        if source_device_id.is_none_or(|wanted| credential.registered_source_device_id() == wanted)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn sweep_unpublished_registered_target_orphans(peer_root: &Path) -> Result<(), PeerSyncError> {
    let _source_lifecycle = super::registry_commands::lock_registered_source_lifecycle()?;
    let Some(app_root) = peer_root.parent() else {
        return Ok(());
    };
    let targets_root = peer_root.join("targets");
    if !ordinary_directory(peer_root)? || !ordinary_directory(&targets_root)? {
        return Ok(());
    }
    let local_device_id = super::device_registry::load_or_create_device_id(app_root)?;
    let active_manifest_id = active_target_manifest_evidence(peer_root)?;
    for entry in fs::read_dir(&targets_root)? {
        let entry = entry?;
        let Some(session_id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_canonical_uuid(&session_id) || !ordinary_directory(&entry.path())? {
            continue;
        }
        let Some(bytes) =
            read_target_operation_marker_bytes(&entry.path().join(TARGET_OPERATION_MARKER_FILE))?
        else {
            // Unlike Android recovery, which releases the owned job because its
            // credential is corrupt, this sweep deletes only on positive evidence
            // that the directory is an orphan, and a credential it can open is
            // that evidence; a credential it cannot read is never evidence, so it
            // is left alone instead of aborting the whole sweep.
            if LanCloneClient::open_persisted(&entry.path().join("credential.json")).is_ok() {
                remove_directory_if_exists(&entry.path())?;
            }
            continue;
        };
        let marker: TargetOperationMarker = match serde_json::from_slice(&bytes) {
            Ok(marker) => marker,
            Err(_) => continue,
        };
        let request = PeerCloneTargetRequest {
            endpoint: String::new(),
            session_id: session_id.clone(),
            manifest_id: marker.manifest_id.clone(),
        };
        if marker.validate_for(&request).is_err() || marker.backup_path.is_some() {
            continue;
        }
        if active_manifest_id.as_deref() == Some(marker.manifest_id.as_str())
            && super::client::persisted_activation_matches(
                &entry.path().join("transfer"),
                &marker.manifest_id,
            )?
        {
            continue;
        }
        // The deletion below is decided by comparing this credential against the
        // registered source, so a credential the sweep cannot read decides
        // nothing. Skipping keeps the sweep best-effort over the other targets;
        // corruption is not the release trigger here as it is on Android.
        let credential = match LanCloneClient::open_persisted(&entry.path().join("credential.json"))
        {
            Ok(credential) => credential,
            Err(_) => continue,
        };
        let source_device_id = credential.registered_source_device_id();
        let Some(source) =
            super::device_registry::incoming_source_by_id(app_root, source_device_id)?
        else {
            remove_directory_if_exists(&entry.path())?;
            continue;
        };
        if !source.permissions.allows_read()
            || !credential.matches_registered_credential(
                &source.endpoint,
                &session_id,
                &marker.manifest_id,
                &local_device_id,
                source_device_id,
                &source.bearer,
            )?
        {
            remove_directory_if_exists(&entry.path())?;
        }
    }
    Ok(())
}

fn persist_target_backup_receipt(
    job_root: &Path,
    peer_root: &Path,
    request: &PeerCloneTargetRequest,
    operation_id: &str,
    backup_path: &Path,
) -> Result<(), PeerSyncError> {
    super::maintenance::with_backup_reference_lifecycle(|| {
        let marker_path = job_root.join(TARGET_OPERATION_MARKER_FILE);
        let mut marker = read_target_operation_marker(&marker_path, request)?.ok_or_else(|| {
            PeerSyncError::Storage("peer clone target operation marker is missing".to_owned())
        })?;
        if marker.operation_id != operation_id {
            return Err(PeerSyncError::Validation(
                "peer clone backup receipt belongs to another operation".to_owned(),
            ));
        }
        let expected = fs::canonicalize(target_backup_path(peer_root, operation_id))?;
        let actual = fs::canonicalize(backup_path)?;
        if actual != expected {
            return Err(PeerSyncError::Validation(
                "peer clone backup receipt path is not operation-owned".to_owned(),
            ));
        }
        marker.backup_path = Some(target_backup_relative_path(operation_id));
        write_target_operation_marker(&marker_path, &marker, request)
    })
}

fn recover_target_backup_receipt(
    job_root: &Path,
    peer_root: &Path,
    request: &PeerCloneTargetRequest,
    operation_id: &str,
) -> Result<Option<PathBuf>, PeerSyncError> {
    let marker_path = job_root.join(TARGET_OPERATION_MARKER_FILE);
    let Some(marker) = read_target_operation_marker(&marker_path, request)? else {
        return Ok(None);
    };
    if marker.operation_id != operation_id {
        return Err(PeerSyncError::Validation(
            "peer clone backup receipt belongs to another operation".to_owned(),
        ));
    }
    let Some(relative) = marker.backup_path else {
        return Ok(None);
    };
    if relative != target_backup_relative_path(operation_id) {
        return Err(PeerSyncError::Validation(
            "peer clone backup receipt path is invalid".to_owned(),
        ));
    }
    let backup_root_path = peer_root.join("activation").join("backups");
    let backup_root_metadata = fs::symlink_metadata(&backup_root_path)?;
    if !backup_root_metadata.is_dir() || metadata_is_symlink_or_reparse(&backup_root_metadata) {
        return Err(PeerSyncError::Validation(
            "peer clone backup root is not a plain directory".to_owned(),
        ));
    }
    let path = target_backup_path(peer_root, operation_id);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata_is_symlink_or_reparse(&metadata) {
        return Err(PeerSyncError::Validation(
            "peer clone backup receipt is not a regular file".to_owned(),
        ));
    }
    let path = fs::canonicalize(path)?;
    let backup_root = fs::canonicalize(backup_root_path)?;
    if path.parent() != Some(backup_root.as_path()) {
        return Err(PeerSyncError::Validation(
            "peer clone backup receipt escapes its owned directory".to_owned(),
        ));
    }
    Ok(Some(path))
}

fn persist_target_completion_acknowledgement(
    job_root: &Path,
    request: &PeerCloneTargetRequest,
    operation_id: &str,
) -> Result<(), PeerSyncError> {
    let marker_path = job_root.join(TARGET_OPERATION_MARKER_FILE);
    let mut marker = read_target_operation_marker(&marker_path, request)?.ok_or_else(|| {
        PeerSyncError::Storage("peer clone target operation marker is missing".to_owned())
    })?;
    if marker.operation_id != operation_id || marker.backup_path.is_none() {
        return Err(PeerSyncError::Validation(
            "peer clone completion acknowledgement is not operation-bound".to_owned(),
        ));
    }
    marker.completion_acknowledged = true;
    write_target_operation_marker(&marker_path, &marker, request)
}

fn target_completion_is_durable(
    app_root: &Path,
    job_root: &Path,
    request: &PeerCloneTargetRequest,
    operation_id: &str,
    source_device_id: &str,
) -> Result<bool, PeerSyncError> {
    let Some(marker) =
        read_target_operation_marker(&job_root.join(TARGET_OPERATION_MARKER_FILE), request)?
    else {
        return Ok(false);
    };
    if marker.operation_id != operation_id || !marker.completion_acknowledged {
        return Ok(false);
    }
    match marker_completion_capability(&marker)? {
        PeerCompletionCapability::Unsupported => {
            let receipt_id = super::device_registry::completion_receipt_id(
                "clone",
                operation_id,
                &request.manifest_id,
            );
            super::device_registry::incoming_completed_operation_recorded(
                app_root,
                source_device_id,
                &receipt_id,
            )
        }
        PeerCompletionCapability::V1 => {
            let completion_lease_id = marker.completion_lease_id.as_deref().ok_or_else(|| {
                PeerSyncError::Validation(
                    "peer clone target completion lease is missing".to_owned(),
                )
            })?;
            let useful_bytes = super::client::persisted_clone_manifest_total(
                &job_root.join("transfer"),
                &request.session_id,
                &request.manifest_id,
            )?;
            let delivery = target_completion_delivery(
                source_device_id,
                completion_lease_id,
                &request.manifest_id,
                useful_bytes,
            );
            super::device_registry::incoming_completion_is_durable(app_root, &delivery)
        }
    }
}

fn write_target_operation_marker(
    path: &Path,
    marker: &TargetOperationMarker,
    request: &PeerCloneTargetRequest,
) -> Result<(), PeerSyncError> {
    marker.validate_for(request)?;
    let bytes =
        serde_json::to_vec(marker).map_err(|error| PeerSyncError::Storage(error.to_string()))?;
    if bytes.len() as u64 > MAX_TARGET_OPERATION_MARKER_BYTES {
        return Err(PeerSyncError::Storage(
            "peer clone target operation marker exceeds its bound".to_owned(),
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        PeerSyncError::Storage("peer clone target operation marker has no parent".to_owned())
    })?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".activation-operation-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        replace_file_atomic(&temporary, path)?;
        sync_parent_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn finish_target_worker(
    runtime: &Arc<Mutex<PeerCloneRuntime>>,
    request: &PeerCloneTargetRequest,
    client: LoopbackCloneClient,
    result: Result<(), PeerSyncError>,
    progress: Option<(u64, u64)>,
) {
    let Ok(mut runtime) = runtime.lock() else {
        return;
    };
    let Some(target) = runtime
        .target
        .as_mut()
        .filter(|target| target.request == *request)
    else {
        return;
    };
    target.client = Some(client);
    target.cancellation = None;
    if let Some((completed_bytes, total_bytes)) = progress {
        target.status.completed_bytes = completed_bytes;
        target.status.total_bytes = Some(total_bytes);
    }
    if target.status.phase == PeerCloneTargetPhase::Cancelling {
        return;
    }
    match result {
        Ok(()) => {
            target.status.phase = PeerCloneTargetPhase::AwaitingActivation;
            if let Some(total_bytes) = target.status.total_bytes {
                target.status.completed_bytes = total_bytes;
            }
            target.status.error = None;
        }
        Err(PeerSyncError::Cancelled) => {
            target.status.phase = PeerCloneTargetPhase::Cancelled;
            target.status.error = None;
        }
        Err(error) => {
            target.status.phase = PeerCloneTargetPhase::Failed;
            target.status.error = Some(error.to_string());
        }
    }
}

fn update_target_progress(
    runtime: &Arc<Mutex<PeerCloneRuntime>>,
    request: &PeerCloneTargetRequest,
    completed_bytes: u64,
    total_bytes: u64,
) {
    if let Ok(mut runtime) = runtime.lock() {
        if let Some(target) = runtime
            .target
            .as_mut()
            .filter(|target| target.request == *request)
        {
            target.status.completed_bytes = completed_bytes;
            target.status.total_bytes = Some(total_bytes);
        }
    }
}

fn join_target_worker(worker: Option<JoinHandle<()>>) -> Result<(), PeerSyncError> {
    worker
        .map(|worker| {
            worker.join().map_err(|_| {
                PeerSyncError::Transport("peer clone target worker panicked".to_owned())
            })
        })
        .transpose()
        .map(|_| ())
}

fn source_status(runtime: &mut PeerCloneRuntime) -> Result<PeerCloneSourceStatus, PeerSyncError> {
    if let Some(error) = &runtime.source_recovery_error {
        return Err(PeerSyncError::Storage(error.clone()));
    }
    let Some(source) = runtime.source.as_mut() else {
        return Ok(PeerCloneSourceStatus::idle());
    };
    if let Some(tunnel) = source.tunnel.as_mut() {
        match tunnel.lifecycle() {
            Ok(SourceTunnelLifecycle::Running) => {}
            Ok(SourceTunnelLifecycle::CleanupPending) | Err(_) => {
                source.phase = PeerCloneSourcePhase::Stopping;
            }
            Ok(SourceTunnelLifecycle::Stopped) => {
                source.tunnel = None;
                source.phase = PeerCloneSourcePhase::Stopping;
                source.terminal_cleanup_pending = true;
            }
        }
    }
    let devices = source
        .control
        .devices()
        .into_iter()
        .map(|device| PeerCloneSourceDevice {
            device_id: device.device_id,
            verified_bytes: device.verified_bytes,
            current_object: device.current_object,
            last_seen_at: u64::try_from(device.last_seen_unix_ms).unwrap_or(u64::MAX),
            revoked: device.revoked,
        })
        .collect();
    Ok(PeerCloneSourceStatus {
        session_id: Some(source.session_id.clone()),
        manifest_id: Some(source.manifest_id.clone()),
        pairing_uri: None,
        phase: source.phase,
        devices,
        tunnel: source.tunnel_metadata,
    })
}

fn tunnel_status(runtime: &PeerCloneRuntime) -> PeerCloneTunnelStatus {
    let Some(source) = runtime
        .source
        .as_ref()
        .filter(|source| source.tunnel_metadata.is_some())
    else {
        return PeerCloneTunnelStatus::idle();
    };
    let phase = match source.phase {
        PeerCloneSourcePhase::Starting => PeerCloneTunnelPhase::Starting,
        PeerCloneSourcePhase::Running => PeerCloneTunnelPhase::Running,
        PeerCloneSourcePhase::Stopping => PeerCloneTunnelPhase::Stopping,
        PeerCloneSourcePhase::Stopped => PeerCloneTunnelPhase::Stopped,
        PeerCloneSourcePhase::Idle | PeerCloneSourcePhase::Prepared => PeerCloneTunnelPhase::Idle,
    };
    PeerCloneTunnelStatus {
        session_id: Some(source.session_id.clone()),
        phase,
        tunnel: source.tunnel_metadata,
    }
}

fn require_source_mut<'a>(
    runtime: &'a mut PeerCloneRuntime,
    session_id: &str,
) -> Result<&'a mut SourceRuntime, PeerSyncError> {
    runtime
        .source
        .as_mut()
        .filter(|source| source.session_id == session_id)
        .ok_or_else(|| {
            PeerSyncError::Protocol("peer clone source session is unavailable".to_owned())
        })
}

fn require_target_request_mut<'a>(
    runtime: &'a mut PeerCloneRuntime,
    request: &PeerCloneTargetRequest,
) -> Result<&'a mut TargetRuntime, PeerSyncError> {
    runtime
        .target
        .as_mut()
        .filter(|target| target.request == *request)
        .ok_or_else(|| {
            PeerSyncError::Protocol(
                "peer clone target request does not own the active job".to_owned(),
            )
        })
}

fn require_target_mut<'a>(
    runtime: &'a mut PeerCloneRuntime,
    request: &PeerCloneTargetRequest,
    job_root: &Path,
) -> Result<&'a mut TargetRuntime, PeerSyncError> {
    require_target_request_mut(runtime, request).and_then(|target| {
        if target.job_root == job_root {
            Ok(target)
        } else {
            Err(PeerSyncError::Protocol(
                "peer clone target storage root does not own the active job".to_owned(),
            ))
        }
    })
}

fn remove_directory_if_exists(path: &Path) -> Result<(), PeerSyncError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || metadata_is_symlink_or_reparse(&metadata) {
        return Err(PeerSyncError::Validation(
            "peer clone cleanup target is not an ordinary directory".to_owned(),
        ));
    }
    fs::remove_dir_all(path)?;
    Ok(())
}

fn metadata_is_symlink_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn ordinary_directory(path: &Path) -> Result<bool, PeerSyncError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir() && !metadata_is_symlink_or_reparse(&metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn sweep_canonical_owned_directories(parent: &Path) -> Result<(), PeerSyncError> {
    if !ordinary_directory(parent)? {
        return Ok(());
    }
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_canonical_uuid(&name) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && !metadata_is_symlink_or_reparse(&metadata) {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn active_target_manifest_evidence(peer_root: &Path) -> Result<Option<String>, PeerSyncError> {
    let Some(repository_root) = peer_root.parent() else {
        return Ok(None);
    };
    let store = match PersistentStore::open(repository_root) {
        Ok(store) => store,
        Err(_) => return Ok(None),
    };
    let revision = store.revision().map_err(store_error)?;
    let Some(value) = store
        .get_app_kv("peerCloneActiveManifest")
        .map_err(store_error)?
    else {
        return Ok(None);
    };
    let Some(marker) = value.as_object() else {
        return Ok(None);
    };
    if marker.len() != 2 || marker.get("revision").and_then(Value::as_i64) != Some(revision) {
        return Ok(None);
    }
    let Some(manifest_id) = marker.get("manifestId").and_then(Value::as_str) else {
        return Ok(None);
    };
    if super::protocol::validate_hash(manifest_id).is_err() {
        return Ok(None);
    }
    Ok(Some(manifest_id.to_owned()))
}

fn sweep_activated_target_residues(peer_root: &Path) -> Result<(), PeerSyncError> {
    let Some(active_manifest_id) = active_target_manifest_evidence(peer_root)? else {
        return Ok(());
    };
    let targets_root = peer_root.join("targets");
    if !ordinary_directory(peer_root)? || !ordinary_directory(&targets_root)? {
        return Ok(());
    }
    for entry in fs::read_dir(&targets_root)? {
        let entry = entry?;
        let Some(session_id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_canonical_uuid(&session_id) {
            continue;
        }
        let job_root = entry.path();
        if !ordinary_directory(&job_root)? {
            continue;
        }
        let transfer_root = job_root.join("transfer");
        if !ordinary_directory(&transfer_root)? {
            continue;
        }
        let manifest_path = transfer_root.join("manifest.json");
        let metadata = match fs::symlink_metadata(&manifest_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file()
            || metadata_is_symlink_or_reparse(&metadata)
            || metadata.len() > super::protocol::MAX_MANIFEST_BYTES as u64
        {
            continue;
        }
        let bytes = fs::read(&manifest_path)?;
        let manifest: super::CloneManifest = match serde_json::from_slice(&bytes) {
            Ok(manifest) => manifest,
            Err(_) => continue,
        };
        if manifest.validate().is_err()
            || manifest.session_id != session_id
            || manifest.canonical_bytes().ok().as_deref() != Some(bytes.as_slice())
            || manifest.identity().ok().as_deref() != Some(active_manifest_id.as_str())
        {
            continue;
        }
        if !super::client::persisted_activation_matches(&transfer_root, &active_manifest_id)? {
            continue;
        }
        let request = PeerCloneTargetRequest {
            endpoint: String::new(),
            session_id: session_id.clone(),
            manifest_id: active_manifest_id.clone(),
        };
        let marker = match read_target_operation_marker(
            &job_root.join(TARGET_OPERATION_MARKER_FILE),
            &request,
        ) {
            Ok(Some(marker)) => marker,
            Ok(None) | Err(_) => continue,
        };
        let credential = match LanCloneClient::open_persisted(&job_root.join("credential.json")) {
            Ok(credential) => credential,
            Err(_) => continue,
        };
        let Ok((_, credential_session_id, credential_manifest_id)) = credential.target_identity()
        else {
            continue;
        };
        if credential_session_id != session_id || credential_manifest_id != active_manifest_id {
            continue;
        }
        let Some(app_root) = peer_root.parent() else {
            continue;
        };
        if !target_completion_is_durable(
            app_root,
            &job_root,
            &request,
            &marker.operation_id,
            credential.registered_source_device_id(),
        )? {
            continue;
        }
        remove_directory_if_exists(&job_root)?;
    }
    Ok(())
}

fn read_active_source_marker(path: &Path) -> Result<Option<ActiveSourceMarker>, PeerSyncError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file()
        || metadata_is_symlink_or_reparse(&metadata)
        || metadata.len() > MAX_ACTIVE_SOURCE_MARKER_BYTES
    {
        return Err(PeerSyncError::Validation(
            "invalid prepared clone source marker file".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take(MAX_ACTIVE_SOURCE_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ACTIVE_SOURCE_MARKER_BYTES {
        return Err(PeerSyncError::Validation(
            "prepared clone source marker exceeds its bound".to_owned(),
        ));
    }
    let marker: ActiveSourceMarker = serde_json::from_slice(&bytes)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    marker.validate()?;
    Ok(Some(marker))
}

fn write_active_source_marker(
    path: &Path,
    marker: &ActiveSourceMarker,
) -> Result<(), PeerSyncError> {
    marker.validate()?;
    let bytes =
        serde_json::to_vec(marker).map_err(|error| PeerSyncError::Storage(error.to_string()))?;
    if bytes.len() as u64 > MAX_ACTIVE_SOURCE_MARKER_BYTES {
        return Err(PeerSyncError::Storage(
            "prepared clone source marker exceeds its bound".to_owned(),
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        PeerSyncError::Storage("prepared clone source marker has no parent".to_owned())
    })?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".active-source-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        replace_file_atomic(&temporary, path)?;
        sync_parent_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn cleanup_source_artifacts(
    session_root: &Path,
    marker_path: &Path,
    marker: &ActiveSourceMarker,
) -> Result<(), PeerSyncError> {
    let peer_root = marker_path.parent().ok_or_else(|| {
        PeerSyncError::Storage("prepared clone source marker has no owned parent".to_owned())
    })?;
    let sessions_root = session_root.parent().ok_or_else(|| {
        PeerSyncError::Storage("prepared clone source session has no owned parent".to_owned())
    })?;
    if !ordinary_directory(peer_root)? || !ordinary_directory(sessions_root)? {
        return Err(PeerSyncError::Validation(
            "peer clone source cleanup root is not an ordinary directory".to_owned(),
        ));
    }
    remove_directory_if_exists(session_root)?;
    match read_active_source_marker(marker_path)? {
        None => Ok(()),
        Some(actual) if actual == *marker => remove_file_if_exists(marker_path),
        Some(_) => Err(PeerSyncError::Validation(
            "prepared clone source marker belongs to another session".to_owned(),
        )),
    }
}

fn remove_file_if_exists(path: &Path) -> Result<(), PeerSyncError> {
    match fs::remove_file(path) {
        Ok(()) => path
            .parent()
            .map(sync_parent_directory)
            .transpose()
            .map(|_| ()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn replace_file_atomic(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file_atomic(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), PeerSyncError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<(), PeerSyncError> {
    Ok(())
}

fn is_canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
        .unwrap_or(false)
}

fn is_canonical_v4_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .map(|parsed| {
            parsed.to_string() == value && parsed.get_version() == Some(uuid::Version::Random)
        })
        .unwrap_or(false)
}

fn build_pairing_uri(endpoint: &str, pairing: &super::LanPairing) -> Result<String, PeerSyncError> {
    super::tunnel_lifecycle::build_lane_pairing_uri("peer-clone", endpoint, pairing)
}

fn tunnel_start_error() -> PeerSyncError {
    PeerSyncError::Transport("peer clone tunnel failed to start".to_owned())
}

fn tunnel_stop_error() -> PeerSyncError {
    PeerSyncError::Transport("peer clone tunnel failed to stop".to_owned())
}

fn public_tunnel_start_error(error: PeerSyncError) -> String {
    match error {
        PeerSyncError::Transport(message) if message == NAMED_TUNNEL_ORIGIN_UNAVAILABLE => message,
        _ => "peer clone tunnel failed to start".to_owned(),
    }
}

fn store_error(error: StoreError) -> PeerSyncError {
    PeerSyncError::Storage(error.to_string())
}

fn bounded_finalize_warning(error: &PeerSyncError) -> String {
    error.to_string().chars().take(1_024).collect()
}

fn as_store_error(error: PeerSyncError) -> StoreError {
    StoreError::Store {
        message: error.to_string(),
    }
}

/// The application data directory and the peer clone root inside it. Both
/// finishers below project this one resolution, so the directory lookup and the
/// `peer-clone` name each live in a single place. The failure stays unwrapped so
/// each finisher keeps the wording its own callers already report.
fn resolve_peer_roots(app: &AppHandle) -> Result<(PathBuf, PathBuf), tauri::Error> {
    let app_root = app.path().app_data_dir()?;
    let peer_root = app_root.join("peer-clone");
    Ok((app_root, peer_root))
}

fn app_peer_root(app: &AppHandle) -> Result<(PathBuf, PathBuf), String> {
    resolve_peer_roots(app)
        .map_err(|error| format!("failed to resolve application data directory: {error}"))
}

/// The bounded-code twin of `app_peer_root`, for the clone target commands the
/// device sync page reaches. Source lane commands keep their diagnostic text.
fn bounded_app_peer_root(app: &AppHandle) -> Result<(PathBuf, PathBuf), String> {
    finish_peer_command(
        "peer clone application data directory",
        resolve_peer_roots(app).map_err(|error| PeerSyncError::Storage(error.to_string())),
    )
}

fn target_request(
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> PeerCloneTargetRequest {
    PeerCloneTargetRequest {
        endpoint,
        session_id,
        manifest_id,
    }
}

#[tauri::command(async)]
pub async fn peer_clone_prepare(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
) -> Result<PeerCloneSourceStatus, String> {
    let state = state.inner().clone();
    let (app_root, peer_root) = app_peer_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let cas = PayloadCas::new(&app_root).map_err(|error| error.to_string())?;
        persistent_store::commands::with_store_mut(app.state(), |store| {
            state
                .prepare_source(store, &cas, &peer_root, &NeverCancelled)
                .map_err(as_store_error)
        })
        .map_err(|error| error.to_string())?;
        state
            .configure_v2_registry(
                &app_root,
                super::device_registry::platform_device_name(),
                super::device_registry::DevicePermissions::read(),
            )
            .map_err(|error| error.to_string())?;
        state.source_status().map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("peer clone source preparation worker failed: {error}"))?
}

#[tauri::command(async)]
pub fn peer_clone_start(
    state: State<'_, PeerCloneCommandState>,
    session_id: String,
) -> Result<PeerCloneSourceStatus, String> {
    let advertised_ip = discover_lan_ipv4().map_err(|error| error.to_string())?;
    state
        .start_source(&session_id, advertised_ip)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn peer_clone_tunnel_start(
    state: State<'_, PeerCloneCommandState>,
    session_id: String,
    tunnel: PeerCloneTunnelStart,
) -> Result<PeerCloneSourceStatus, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.start_tunnel(&session_id, tunnel))
        .await
        .map_err(|_| "peer clone tunnel start worker failed".to_owned())?
        .map_err(public_tunnel_start_error)
}

#[tauri::command]
pub async fn peer_clone_status(
    state: State<'_, PeerCloneCommandState>,
) -> Result<PeerCloneSourceStatus, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.source_status())
        .await
        .map_err(|error| format!("peer clone source status worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn peer_clone_tunnel_status(
    state: State<'_, PeerCloneCommandState>,
) -> Result<PeerCloneTunnelStatus, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.tunnel_status())
        .await
        .map_err(|_| "peer clone tunnel status worker failed".to_owned())?
        .map_err(|_| "peer clone tunnel status is unavailable".to_owned())
}

#[tauri::command]
pub async fn peer_clone_stop(
    state: State<'_, PeerCloneCommandState>,
    session_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.stop_source(&session_id))
        .await
        .map_err(|error| format!("peer clone source stop worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn peer_clone_tunnel_stop(
    state: State<'_, PeerCloneCommandState>,
    session_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.stop_source(&session_id))
        .await
        .map_err(|_| "peer clone tunnel stop worker failed".to_owned())?
        .map_err(|_| "peer clone tunnel failed to stop".to_owned())
}

#[tauri::command(async)]
pub fn peer_clone_revoke(
    state: State<'_, PeerCloneCommandState>,
    session_id: String,
    device_id: String,
) -> Result<(), String> {
    state
        .revoke_source_device(&session_id, &device_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn peer_clone_claim_v2_client(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    claim: String,
) -> Result<PeerCloneClaimResult, String> {
    let endpoint = finish_peer_command(
        "peer clone v2 target endpoint",
        validate_device_sync_endpoint(&endpoint),
    )?;
    let state = state.inner().clone();
    let (_, peer_root) = bounded_app_peer_root(&app)?;
    let claimed = tauri::async_runtime::spawn_blocking(move || {
        state.claim_v2_target(&peer_root, &endpoint, &session_id, &manifest_id, &claim)
    })
    .await;
    finish_peer_worker("peer clone v2 target claim", claimed)
}

#[tauri::command(async)]
pub fn peer_clone_download(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<(), String> {
    let (_, peer_root) = bounded_app_peer_root(&app)?;
    finish_peer_command(
        "peer clone target download",
        state.start_target_download(
            &peer_root,
            target_request(endpoint, session_id, manifest_id),
        ),
    )
}

#[tauri::command]
pub async fn peer_clone_resume(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<(), String> {
    let (_, peer_root) = bounded_app_peer_root(&app)?;
    let state = state.inner().clone();
    let request = target_request(endpoint, session_id, manifest_id);
    let resumed = tauri::async_runtime::spawn_blocking(move || {
        state.resume_target_download(&peer_root, request)
    })
    .await;
    finish_peer_worker("peer clone target resume", resumed)
}

#[tauri::command]
pub async fn peer_clone_cancel(
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    let request = target_request(endpoint, session_id, manifest_id);
    finish_peer_worker(
        "peer clone target cancellation",
        tauri::async_runtime::spawn_blocking(move || state.cancel_target(&request)).await,
    )
}

#[tauri::command(async)]
pub fn peer_clone_target_status(
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<PeerCloneTargetStatus, String> {
    finish_peer_command(
        "peer clone target status",
        state.target_status(&target_request(endpoint, session_id, manifest_id)),
    )
}

#[tauri::command(async)]
pub fn peer_clone_release_target(
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<(), String> {
    finish_peer_command(
        "peer clone target release",
        state.release_target(&target_request(endpoint, session_id, manifest_id)),
    )
}

#[tauri::command]
pub async fn peer_clone_finalize(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<PeerCloneFinalizeResult, String> {
    let state = state.inner().clone();
    let request = target_request(endpoint, session_id, manifest_id);
    let (app_root, peer_root) = bounded_app_peer_root(&app)?;
    let finalized = tauri::async_runtime::spawn_blocking(move || {
        let cas = PayloadCas::new(&app_root)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        persistent_store::commands::with_store_mut(app.state(), |store| {
            state
                .finalize_target(store, &cas, &peer_root, &request)
                .map_err(as_store_error)
        })
        .map_err(|error| PeerSyncError::Storage(error.to_string()))
    })
    .await;
    finish_peer_worker("peer clone target finalization", finalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistent_store::WorkingSetCommit;
    use serde_json::json;
    use std::{
        io::Cursor,
        net::{Ipv4Addr, TcpListener, TcpStream},
        sync::{mpsc, Arc, Barrier},
        thread,
        time::{Duration, Instant},
    };

    #[derive(Debug)]
    struct RecordedClaimRequest {
        request_line: String,
        body: Option<serde_json::Value>,
    }

    fn read_recorded_claim_request(stream: &mut TcpStream) -> RecordedClaimRequest {
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        let (header_end, content_length) = loop {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "HTTP request ended before its headers");
            request.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let header_end = header_end + 4;
                let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                break (header_end, content_length);
            }
        };
        while request.len() < header_end + content_length {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "HTTP request ended before its body");
            request.extend_from_slice(&buffer[..read]);
        }
        let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
        let body = (content_length > 0).then(|| {
            serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap()
        });
        RecordedClaimRequest {
            request_line: headers.lines().next().unwrap().to_owned(),
            body,
        }
    }

    fn rejecting_claim_server() -> (String, thread::JoinHandle<Vec<RecordedClaimRequest>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(750);
            let mut requests = Vec::new();
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        requests.push(read_recorded_claim_request(&mut stream));
                        stream
                            .write_all(
                                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .unwrap();
                        stream.flush().unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept rejecting claim request: {error}"),
                }
            }
            requests
        });
        (endpoint, server)
    }

    fn write_clone_credential(path: &Path, request: &PeerCloneTargetRequest) -> Vec<u8> {
        let bytes = serde_json::to_vec(&json!({
            "schema": super::super::lan::PERSISTED_CREDENTIAL_SCHEMA,
            "endpoint": request.endpoint,
            "sessionId": request.session_id,
            "manifestId": request.manifest_id,
            "deviceId": "00000000-0000-4000-8000-000000000201",
            "bearer": "b".repeat(64),
            "sourceDeviceId": "00000000-0000-4000-8000-000000000202",
            "permission": "clone-read"
        }))
        .unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &bytes).unwrap();
        bytes
    }

    fn assert_single_strict_v2_claim(requests: Vec<RecordedClaimRequest>, session_id: &str) {
        assert_eq!(requests.len(), 1, "strict v2 must make exactly one request");
        assert_eq!(
            requests[0].request_line,
            format!("POST /v1/sessions/{session_id}/claim HTTP/1.1")
        );
        assert_eq!(requests[0].body.as_ref().unwrap()["protocolVersion"], 2);
    }

    #[cfg(unix)]
    fn create_directory_link(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("create directory symlink");
    }

    #[cfg(windows)]
    fn create_directory_link(target: &Path, link: &Path) {
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("invoke junction creation");
        assert!(
            output.status.success(),
            "create directory junction: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn public_lan_clone_exposes_verified_production_gates() {
        assert_eq!(
            peer_clone_capabilities(),
            PeerCloneCapabilities {
                desktop: true,
                source_ready: true,
                atomic_activation_ready: true,
                lossless_backup_ready: true,
                http_transport_ready: true,
                large_fixture_passed: false,
                production_enabled: true,
            }
        );
    }

    #[test]
    fn production_gate_requires_lossless_backup_and_qualified_http_transport() {
        let otherwise_ready = PeerCloneGateState {
            desktop: true,
            source_ready: true,
            atomic_activation_ready: true,
            lossless_backup_ready: true,
            http_transport_ready: true,
            large_fixture_passed: true,
        };

        assert!(PeerCloneCapabilities::from_gates(otherwise_ready).production_enabled);
        assert!(
            !PeerCloneCapabilities::from_gates(PeerCloneGateState {
                lossless_backup_ready: false,
                ..otherwise_ready
            })
            .production_enabled
        );
        assert!(
            !PeerCloneCapabilities::from_gates(PeerCloneGateState {
                http_transport_ready: false,
                ..otherwise_ready
            })
            .production_enabled
        );
    }

    #[test]
    fn large_fixture_is_release_evidence_not_a_runtime_gate() {
        let capabilities = PeerCloneCapabilities::from_gates(PeerCloneGateState {
            desktop: true,
            source_ready: true,
            atomic_activation_ready: true,
            lossless_backup_ready: true,
            http_transport_ready: true,
            large_fixture_passed: false,
        });

        assert!(capabilities.production_enabled);
        assert!(!capabilities.large_fixture_passed);
    }

    #[test]
    fn target_operation_marker_round_trips_absent_completion_state_by_omitting_it() {
        let request = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000300".to_owned(),
            manifest_id: "a".repeat(64),
        };
        let without_completion = serde_json::json!({
            "schema": TARGET_OPERATION_MARKER_SCHEMA,
            "sessionId": request.session_id,
            "manifestId": request.manifest_id,
            "operationId": "00000000-0000-4000-8000-000000000301"
        });

        let marker: TargetOperationMarker = serde_json::from_value(without_completion).unwrap();
        marker.validate_for(&request).unwrap();
        assert_eq!(marker.completion_capability.as_deref(), None);
        assert_eq!(marker.completion_lease_id.as_deref(), None);
        let serialized = serde_json::to_value(marker).unwrap();
        assert!(serialized.get("completionCapability").is_none());
        assert!(serialized.get("completionLeaseId").is_none());
    }

    #[test]
    fn target_operation_marker_replaces_inherited_or_non_v4_operation_ids() {
        let root = tempfile::tempdir().unwrap();
        let request = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000301".to_owned(),
            manifest_id: "a".repeat(64),
        };

        for invalid_operation_id in [
            request.session_id.as_str(),
            "00000000-0000-1000-8000-000000000302",
        ] {
            let job_root = root.path().join(invalid_operation_id);
            fs::create_dir_all(&job_root).unwrap();
            fs::write(
                job_root.join(TARGET_OPERATION_MARKER_FILE),
                serde_json::to_vec(&TargetOperationMarker {
                    schema: TARGET_OPERATION_MARKER_SCHEMA.to_owned(),
                    session_id: request.session_id.clone(),
                    manifest_id: request.manifest_id.clone(),
                    operation_id: invalid_operation_id.to_owned(),
                    backup_path: None,
                    completion_acknowledged: false,
                    completion_capability: None,
                    completion_lease_id: None,
                })
                .unwrap(),
            )
            .unwrap();

            let replacement = load_or_create_target_operation_id(&job_root, &request).unwrap();
            let parsed = uuid::Uuid::parse_str(&replacement).unwrap();
            assert_eq!(parsed.get_version(), Some(uuid::Version::Random));
            assert_ne!(replacement, request.session_id);
            assert_ne!(replacement, invalid_operation_id);
            assert_eq!(
                read_target_operation_marker(
                    &job_root.join(TARGET_OPERATION_MARKER_FILE),
                    &request,
                )
                .unwrap()
                .unwrap()
                .operation_id,
                replacement
            );
        }

        let malformed_root = root.path().join("malformed");
        fs::create_dir_all(&malformed_root).unwrap();
        fs::write(
            malformed_root.join(TARGET_OPERATION_MARKER_FILE),
            br#"{"operationId":"truncated""#,
        )
        .unwrap();
        let replacement = load_or_create_target_operation_id(&malformed_root, &request).unwrap();
        let parsed = uuid::Uuid::parse_str(&replacement).unwrap();
        assert_eq!(parsed.get_version(), Some(uuid::Version::Random));
        assert_ne!(replacement, request.session_id);
    }

    #[test]
    fn target_backup_receipt_rejects_a_linked_backup_root() {
        let root = tempfile::tempdir().unwrap();
        let peer_root = root.path().join("peer-clone");
        let job_root = peer_root.join("targets/job");
        let external = root.path().join("external-backups");
        fs::create_dir_all(peer_root.join("activation")).unwrap();
        fs::create_dir_all(&job_root).unwrap();
        fs::create_dir_all(&external).unwrap();
        create_directory_link(&external, &peer_root.join("activation").join("backups"));
        let request = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000311".to_owned(),
            manifest_id: "b".repeat(64),
        };
        let operation_id = "00000000-0000-4000-8000-000000000312";
        fs::write(
            external.join(format!("pre-clone-{operation_id}.lossless")),
            b"backup",
        )
        .unwrap();
        write_target_operation_marker(
            &job_root.join(TARGET_OPERATION_MARKER_FILE),
            &TargetOperationMarker {
                schema: TARGET_OPERATION_MARKER_SCHEMA.to_owned(),
                session_id: request.session_id.clone(),
                manifest_id: request.manifest_id.clone(),
                operation_id: operation_id.to_owned(),
                backup_path: Some(target_backup_relative_path(operation_id)),
                completion_acknowledged: false,
                completion_capability: None,
                completion_lease_id: None,
            },
            &request,
        )
        .unwrap();

        assert!(matches!(
            recover_target_backup_receipt(&job_root, &peer_root, &request, operation_id),
            Err(PeerSyncError::Validation(message))
                if message == "peer clone backup root is not a plain directory"
        ));
    }

    #[test]
    fn tunnel_command_contract_deserializes_secrets_but_never_serializes_them() {
        let secret = "eyJ-remotely-managed-tunnel-token";
        let request: PeerCloneTunnelStart = serde_json::from_value(json!({
            "kind": "named",
            "token": secret,
            "expectedPublicBaseUrl": "https://sync.example.com"
        }))
        .unwrap();
        match request {
            PeerCloneTunnelStart::Named {
                token,
                expected_public_base_url,
            } => {
                assert_eq!(token, secret);
                assert_eq!(expected_public_base_url, "https://sync.example.com");
            }
            PeerCloneTunnelStart::Quick => panic!("named request became quick"),
        }

        let status = PeerCloneTunnelStatus {
            session_id: Some("session-id".to_owned()),
            phase: PeerCloneTunnelPhase::Running,
            tunnel: Some(PeerCloneTunnelMetadata::named()),
        };
        let serialized = serde_json::to_string(&status).unwrap();

        assert_eq!(
            serde_json::to_value(status).unwrap(),
            json!({
                "sessionId": "session-id",
                "phase": "running",
                "tunnel": {
                    "kind": "named",
                    "experimental": false,
                    "oneShot": false
                }
            })
        );
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains("sync.example.com"));
        assert!(!serialized.contains("tunnel-check"));
    }

    #[test]
    fn tunnel_start_command_exposes_only_the_fixed_port_recovery_message() {
        assert_eq!(
            public_tunnel_start_error(PeerSyncError::Transport(
                super::super::lan::NAMED_TUNNEL_ORIGIN_UNAVAILABLE.to_owned()
            )),
            super::super::lan::NAMED_TUNNEL_ORIGIN_UNAVAILABLE
        );

        for reflected in [
            "eyJ-reflected-remotely-managed-token",
            "https://sync.example.com/tunnel-check?probe=reflected-secret",
            "reflected internal launch detail",
        ] {
            assert_eq!(
                public_tunnel_start_error(PeerSyncError::Transport(reflected.to_owned())),
                "peer clone tunnel failed to start"
            );
        }
    }

    #[test]
    fn product_quick_tunnel_uses_loopback_and_returns_pairing_only_once() {
        let fixture = TunnelSourceFixture::prepare("https://quick-id.trycloudflare.com/");
        let running = fixture
            .source
            .start_tunnel(&fixture.session_id, PeerCloneTunnelStart::Quick)
            .unwrap();
        let pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());

        assert_eq!(running.phase, PeerCloneSourcePhase::Running);
        assert_eq!(
            serde_json::to_value(running.tunnel.as_ref().unwrap()).unwrap(),
            json!({"kind": "quick", "experimental": true, "oneShot": true})
        );
        assert_eq!(
            pairing.advertised_endpoint,
            "https://quick-id.trycloudflare.com"
        );
        assert_eq!(
            fixture
                .launcher_state
                .lock()
                .unwrap()
                .seen_origin
                .unwrap()
                .ip(),
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        let source_status = fixture.source.source_status().unwrap();
        let tunnel_status = fixture.source.tunnel_status().unwrap();
        let status_json = serde_json::to_string(&(source_status, tunnel_status)).unwrap();
        assert!(!status_json.contains("quick-id.trycloudflare.com"));
        assert!(!status_json.contains(&pairing.claim));

        fixture.source.stop_source(&fixture.session_id).unwrap();
    }

    #[test]
    fn product_named_tunnel_failure_is_sanitized_and_restores_lan_fallback() {
        // Reserve an ephemeral port for the named origin so the test never binds the
        // machine-global fixed port.
        let reserved = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let fixed_port = reserved.local_addr().unwrap().port();
        drop(reserved);
        let _override = super::super::lan::override_named_tunnel_origin_port_for_test(fixed_port);
        let fixture = TunnelSourceFixture::prepare("https://sync.example.com/");
        fixture.launcher_state.lock().unwrap().fail_start = true;
        let secret = "eyJ-reflected-remotely-managed-token";

        let error = fixture
            .source
            .start_tunnel(
                &fixture.session_id,
                PeerCloneTunnelStart::Named {
                    token: secret.to_owned(),
                    expected_public_base_url: "https://sync.example.com".to_owned(),
                },
            )
            .unwrap_err()
            .to_string();

        assert_eq!(error, "Transport(\"peer clone tunnel failed to start\")");
        assert!(!error.contains(secret));
        assert!(!error.contains("sync.example.com"));
        assert!(!error.contains("tunnel-check"));
        assert_eq!(
            fixture.launcher_state.lock().unwrap().seen_token.as_deref(),
            Some(secret)
        );
        assert_eq!(
            fixture.source.source_status().unwrap().phase,
            PeerCloneSourcePhase::Prepared
        );
        assert!(fixture.source.tunnel_status().unwrap().tunnel.is_none());
        assert!(fixture.source.source_bind_address().unwrap().is_none());

        let lan = fixture
            .source
            .start_source(&fixture.session_id, Ipv4Addr::new(192, 168, 1, 8))
            .unwrap();
        assert_eq!(lan.phase, PeerCloneSourcePhase::Running);
        fixture.source.stop_source(&fixture.session_id).unwrap();
    }

    #[test]
    fn product_named_tunnel_reports_only_the_actionable_fixed_port_conflict() {
        // The occupied ephemeral reservation stands in for the fixed port so the test
        // never contends on the machine-global 32145.
        let occupied = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let _override = super::super::lan::override_named_tunnel_origin_port_for_test(
            occupied.local_addr().unwrap().port(),
        );
        let fixture = TunnelSourceFixture::prepare("https://sync.example.com/");

        let error = fixture
            .source
            .start_tunnel(
                &fixture.session_id,
                PeerCloneTunnelStart::Named {
                    token: "eyJ-remotely-managed-tunnel-token".to_owned(),
                    expected_public_base_url: "https://sync.example.com".to_owned(),
                },
            )
            .unwrap_err();

        assert_eq!(
            error,
            PeerSyncError::Transport(super::super::lan::NAMED_TUNNEL_ORIGIN_UNAVAILABLE.to_owned())
        );
        assert_eq!(
            fixture.source.source_status().unwrap().phase,
            PeerCloneSourcePhase::Prepared
        );
        drop(occupied);
        fixture.source.stop_source(&fixture.session_id).unwrap();
    }

    #[test]
    fn product_tunnel_start_does_not_lock_status_and_blocks_competing_stop() {
        let fixture = TunnelSourceFixture::prepare("https://quick-id.trycloudflare.com/");
        let pause = Arc::new(Barrier::new(2));
        fixture.launcher_state.lock().unwrap().launch_pause = Some(Arc::clone(&pause));
        let source = fixture.source.clone();
        let session_id = fixture.session_id.clone();
        let start =
            thread::spawn(move || source.start_tunnel(&session_id, PeerCloneTunnelStart::Quick));
        pause.wait();

        assert_eq!(
            fixture.source.source_status().unwrap().phase,
            PeerCloneSourcePhase::Starting
        );
        assert_eq!(
            fixture.source.tunnel_status().unwrap().phase,
            PeerCloneTunnelPhase::Starting
        );
        assert!(fixture.source.stop_source(&fixture.session_id).is_err());

        pause.wait();
        start.join().unwrap().unwrap();
        fixture.source.stop_source(&fixture.session_id).unwrap();
    }

    #[test]
    fn product_tunnel_stop_failure_keeps_cleanup_owned_and_retryable() {
        let fixture = TunnelSourceFixture::prepare("https://quick-id.trycloudflare.com/");
        fixture
            .source
            .start_tunnel(&fixture.session_id, PeerCloneTunnelStart::Quick)
            .unwrap();
        fixture.launcher_state.lock().unwrap().stop_failures = 1;
        let session_root = fixture.source.source_session_root_for_test().unwrap();
        let marker_path = session_root
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("active-source.json");

        let error = fixture.source.stop_source(&fixture.session_id).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Transport(\"peer clone tunnel failed to stop\")"
        );
        assert_eq!(
            fixture.source.source_status().unwrap().phase,
            PeerCloneSourcePhase::Stopping
        );
        assert!(session_root.exists());
        assert!(marker_path.exists());

        fixture.source.stop_source(&fixture.session_id).unwrap();
        assert_eq!(
            fixture.source.source_status().unwrap().phase,
            PeerCloneSourcePhase::Stopped
        );
        assert!(!session_root.exists());
        assert!(!marker_path.exists());
    }

    #[test]
    fn app_exit_shutdown_releases_runtime_before_stopping_the_tunnel() {
        let fixture = TunnelSourceFixture::prepare("https://quick-id.trycloudflare.com/");
        fixture
            .source
            .start_tunnel(&fixture.session_id, PeerCloneTunnelStart::Quick)
            .unwrap();
        let session_root = fixture.source.source_session_root_for_test().unwrap();
        let marker_path = session_root
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("active-source.json");
        let stop_pause = Arc::new(Barrier::new(2));
        fixture.launcher_state.lock().unwrap().stop_pause = Some(Arc::clone(&stop_pause));

        let source = fixture.source.clone();
        let shutdown = thread::spawn(move || source.shutdown_for_exit());
        stop_pause.wait();

        let status_source = fixture.source.clone();
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        thread::spawn(move || status_tx.send(status_source.source_status()).unwrap());
        assert_eq!(
            status_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap()
                .phase,
            PeerCloneSourcePhase::Running
        );
        assert!(session_root.exists());

        stop_pause.wait();
        shutdown.join().unwrap();
        assert_eq!(fixture.launcher_state.lock().unwrap().stop_calls, 1);
        assert!(session_root.exists());
        assert!(marker_path.exists());
    }

    #[test]
    fn product_tunnel_natural_exit_reaps_the_source_session() {
        let fixture = TunnelSourceFixture::prepare("https://quick-id.trycloudflare.com/");
        fixture
            .source
            .start_tunnel(&fixture.session_id, PeerCloneTunnelStart::Quick)
            .unwrap();
        let session_root = fixture.source.source_session_root_for_test().unwrap();
        let marker_path = session_root
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("active-source.json");
        let cleanup_pause = Arc::new(Barrier::new(2));
        fixture
            .source
            .pause_source_stop_before_cleanup_for_test(Arc::clone(&cleanup_pause))
            .unwrap();
        fixture.launcher_state.lock().unwrap().lifecycle = SourceTunnelLifecycle::Stopped;

        let source = fixture.source.clone();
        let status = thread::spawn(move || source.source_status());
        cleanup_pause.wait();

        assert!(session_root.exists());
        assert_eq!(
            fixture.source.source_status().unwrap().phase,
            PeerCloneSourcePhase::Stopping
        );

        cleanup_pause.wait();

        assert_eq!(
            status.join().unwrap().unwrap().phase,
            PeerCloneSourcePhase::Stopped
        );
        assert!(!session_root.exists());
        assert!(!marker_path.exists());
    }

    #[test]
    fn strict_v2_claim_bypasses_a_matching_runtime_on_http_400() {
        let target_root = tempfile::tempdir().unwrap();
        let peer_root = target_root.path().join("peer-sync");
        let (endpoint, server) = rejecting_claim_server();
        let request = PeerCloneTargetRequest {
            endpoint,
            session_id: "00000000-0000-4000-8000-000000000202".to_owned(),
            manifest_id: "a".repeat(64),
        };
        let paths = target_paths(&peer_root, &request).unwrap();
        let original_credential = write_clone_credential(&paths.credential, &request);
        let client = LoopbackCloneClient::from_lan_with_completion(
            &paths.transfer,
            LanCloneClient::open_persisted(&paths.credential).unwrap(),
            &request.manifest_id,
            PeerCompletionCapability::Unsupported,
            None,
        )
        .unwrap();
        let state = PeerCloneCommandState::default();
        state.lock_runtime().unwrap().target = Some(TargetRuntime {
            request: request.clone(),
            job_root: paths.job_root,
            operation_id: "00000000-0000-4000-8000-000000000203".to_owned(),
            source_device_id: "00000000-0000-4000-8000-000000000202".to_owned(),
            client: Some(client),
            cancellation: None,
            worker: None,
            status: PeerCloneTargetStatus::idle(),
            fail_finalize_cleanup_once: false,
            fail_finalize_backup_receipt_once: false,
            fail_release_cleanup_once: false,
            fail_finalize_completion_ack_once: false,
            fail_after_remote_completion_once: false,
        });

        assert!(state
            .claim_v2_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &"c".repeat(64),
            )
            .is_err());

        assert_single_strict_v2_claim(server.join().unwrap(), &request.session_id);
        assert_eq!(fs::read(&paths.credential).unwrap(), original_credential);
        assert!(
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()
                .is_empty()
        );
    }

    #[test]
    fn strict_v2_claim_bypasses_a_persisted_credential_on_http_400() {
        let target_root = tempfile::tempdir().unwrap();
        let peer_root = target_root.path().join("peer-sync");
        let (endpoint, server) = rejecting_claim_server();
        let request = PeerCloneTargetRequest {
            endpoint,
            session_id: "00000000-0000-4000-8000-000000000204".to_owned(),
            manifest_id: "a".repeat(64),
        };
        let paths = target_paths(&peer_root, &request).unwrap();
        let original_credential = write_clone_credential(&paths.credential, &request);
        let state = PeerCloneCommandState::default();

        assert!(state
            .claim_v2_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &"c".repeat(64),
            )
            .is_err());

        assert_single_strict_v2_claim(server.join().unwrap(), &request.session_id);
        assert_eq!(fs::read(&paths.credential).unwrap(), original_credential);
        assert!(
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()
                .is_empty()
        );
    }

    #[test]
    fn strict_v2_claim_replaces_a_matching_runtime_and_consumes_the_claim() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let verifier_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        let source_peer_root = source_root.path().join("peer-sync");
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_peer_root,
                &NeverCancelled,
            )
            .unwrap();
        source
            .configure_v2_registry(
                source_root.path(),
                "Windows source",
                super::super::device_registry::DevicePermissions::read(),
            )
            .unwrap();
        let running = source
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());
        let request = PeerCloneTargetRequest {
            endpoint: format!(
                "http://127.0.0.1:{}",
                source.source_bind_address().unwrap().unwrap().port()
            ),
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
        };
        let target_peer_root = target_root.path().join("peer-sync");
        let paths = target_paths(&target_peer_root, &request).unwrap();
        let original_credential = write_clone_credential(&paths.credential, &request);
        let client = LoopbackCloneClient::from_lan_with_completion(
            &paths.transfer,
            LanCloneClient::open_persisted(&paths.credential).unwrap(),
            &request.manifest_id,
            PeerCompletionCapability::Unsupported,
            None,
        )
        .unwrap();
        let target = PeerCloneCommandState::default();
        target.lock_runtime().unwrap().target = Some(TargetRuntime {
            request: request.clone(),
            job_root: paths.job_root,
            operation_id: "00000000-0000-4000-8000-000000000205".to_owned(),
            source_device_id: "00000000-0000-4000-8000-000000000202".to_owned(),
            client: Some(client),
            cancellation: None,
            worker: None,
            status: PeerCloneTargetStatus::idle(),
            fail_finalize_cleanup_once: false,
            fail_finalize_backup_receipt_once: false,
            fail_release_cleanup_once: false,
            fail_finalize_completion_ack_once: false,
            fail_after_remote_completion_once: false,
        });

        let result = target
            .claim_v2_target(
                &target_peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        let source_device_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        assert_eq!(result.source_device_id, source_device_id);
        assert_ne!(fs::read(&paths.credential).unwrap(), original_credential);
        assert_eq!(
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()
                .len(),
            1
        );
        assert!(LanCloneClient::claim_v2_and_persist_and_register(
            verifier_root.path(),
            "Verifier",
            &verifier_root.path().join("credential.json"),
            &request.endpoint,
            &request.session_id,
            &request.manifest_id,
            &pairing.claim,
        )
        .is_err());

        source.stop_source(&request.session_id).unwrap();
    }

    #[test]
    fn product_full_lossless_clone_waits_for_renderer_finalize() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let target_cas = PayloadCas::new(target_root.path()).unwrap();
        let authoritative = target_cas
            .prepare_reader(&mut Cursor::new(b"authoritative-target-object"))
            .unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        let mut target_store = PersistentStore::open(target_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        seed_product_store(&mut target_store, "Target", 0);
        let source = PeerCloneCommandState::default();

        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_root.path().join("peer-sync"),
                &NeverCancelled,
            )
            .unwrap();

        assert_eq!(prepared.phase, PeerCloneSourcePhase::Prepared);
        assert!(prepared.pairing_uri.is_none());
        assert!(source.source_bind_address().unwrap().is_none());
        source
            .configure_v2_registry(
                source_root.path(),
                "Windows",
                super::super::device_registry::DevicePermissions::read(),
            )
            .unwrap();

        let running = source
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let bound = source.source_bind_address().unwrap().unwrap();
        assert!(bound.ip().is_unspecified());
        let pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());
        assert!(source.source_status().unwrap().pairing_uri.is_none());
        assert_eq!(
            pairing.advertised_endpoint,
            format!("http://192.168.1.4:{}", bound.port())
        );

        let request = PeerCloneTargetRequest {
            endpoint: format!("http://127.0.0.1:{}", bound.port()),
            session_id: pairing.session_id.clone(),
            manifest_id: pairing.manifest_id.clone(),
        };
        let target = PeerCloneCommandState::default();
        let claim_result = target
            .claim_v2_target(
                &target_root.path().join("peer-sync"),
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        let source_device_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        assert_eq!(claim_result.source_device_id, source_device_id);
        let serialized = serde_json::to_string(&claim_result).unwrap();
        assert!(!serialized.contains("bearer"));
        let incoming =
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap();
        assert!(!serialized.contains(&incoming.sources()[0].bearer));
        assert_eq!(incoming.sources().len(), 1);
        assert_eq!(incoming.sources()[0].name, "Windows");
        assert_eq!(
            incoming.sources()[0].device_id,
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap()
        );
        let verifier_root = tempfile::tempdir().unwrap();
        assert!(LanCloneClient::claim_v2_and_persist_and_register(
            verifier_root.path(),
            "Verifier",
            &verifier_root.path().join("credential.json"),
            &request.endpoint,
            &request.session_id,
            &request.manifest_id,
            &pairing.claim,
        )
        .is_err());

        target
            .start_target_download(&target_root.path().join("peer-sync"), request.clone())
            .unwrap();
        let downloaded = wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation);
        assert_eq!(downloaded.completed_bytes, downloaded.total_bytes.unwrap());
        assert!(target.release_target(&request).is_err());
        assert_eq!(target_store.revision().unwrap(), 1);
        assert_eq!(
            target_store.read_root(None).unwrap().value["username"],
            "Target"
        );
        {
            let mut runtime = source.lock_runtime().unwrap();
            runtime
                .source
                .as_mut()
                .unwrap()
                .host
                .as_mut()
                .unwrap()
                .stop()
                .unwrap();
        }
        drop(target);
        let target = PeerCloneCommandState::default();
        target
            .resume_target_download(&target_root.path().join("peer-sync"), request.clone())
            .unwrap();
        wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation);
        {
            let mut runtime = source.lock_runtime().unwrap();
            runtime
                .source
                .as_mut()
                .unwrap()
                .host
                .as_mut()
                .unwrap()
                .start_fixed_on(Ipv4Addr::UNSPECIFIED, bound.port())
                .unwrap();
        }
        let operation_id = target
            .lock_runtime()
            .unwrap()
            .target
            .as_ref()
            .unwrap()
            .operation_id
            .clone();
        target.fail_target_finalize_cleanup_once_for_test().unwrap();
        let accounting =
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap();
        let remaining = u64::MAX - accounting.sources()[0].total_bytes;
        super::super::device_registry::record_incoming_completed_operation(
            target_root.path(),
            &source_device_id,
            remaining,
        )
        .unwrap();
        let accounting_error = target
            .finalize_target(
                &mut target_store,
                &target_cas,
                &target_root.path().join("peer-sync"),
                &request,
            )
            .unwrap_err();

        assert!(matches!(
            accounting_error,
            PeerSyncError::Validation(message) if message == "peer total bytes overflow"
        ));
        assert_eq!(target_store.revision().unwrap(), 2);
        let backup_path = fs::canonicalize(target_backup_path(
            &target_root.path().join("peer-sync"),
            &operation_id,
        ))
        .unwrap();
        assert!(backup_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&operation_id));
        assert!(!backup_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&request.manifest_id));
        assert_eq!(
            backup_path.parent(),
            Some(
                fs::canonicalize(target_root.path().join("peer-sync/activation/backups"))
                    .unwrap()
                    .as_path()
            )
        );
        assert_eq!(
            target.target_status(&request).unwrap().phase,
            PeerCloneTargetPhase::AwaitingActivation
        );
        assert_eq!(
            target_store.read_root(None).unwrap().value["username"],
            "Source"
        );
        let accounted =
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap();
        assert_eq!(accounted.sources()[0].total_bytes, u64::MAX);
        let pending_accounting: Value = serde_json::from_slice(
            &fs::read(target_root.path().join("peer-sync/sources.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            pending_accounting["pendingCompletionDeliveries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(target.release_target(&request).is_err());
        assert_eq!(
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()[0]
                .total_bytes,
            u64::MAX
        );
        let target_job_root = target_root
            .path()
            .join("peer-sync")
            .join("targets")
            .join(&request.session_id);
        assert!(target_job_root.join("credential.json").is_file());
        assert!(target_job_root.join("transfer/manifest.json").is_file());
        assert!(target_job_root.join("transfer/ledger.jsonl").is_file());
        assert!(target_job_root.join("transfer/assets-v2/objects").is_dir());
        drop(target);

        let recovered = PeerCloneCommandState::initialize(&target_root.path().join("peer-sync"));
        assert!(target_job_root.exists());
        let sources_path = target_root.path().join("peer-sync/sources.json");
        let mut accounting: Value =
            serde_json::from_slice(&fs::read(&sources_path).unwrap()).unwrap();
        accounting["sources"][0]["totalBytes"] = Value::from(0);
        fs::write(&sources_path, serde_json::to_vec(&accounting).unwrap()).unwrap();
        recovered
            .resume_target_download(&target_root.path().join("peer-sync"), request.clone())
            .unwrap();
        wait_for_target_phase(&recovered, PeerCloneTargetPhase::AwaitingActivation);

        let finalized = recovered
            .finalize_target(
                &mut target_store,
                &target_cas,
                &target_root.path().join("peer-sync"),
                &request,
            )
            .unwrap();

        assert_eq!(finalized.revision, 2);
        assert!(finalized.warning.is_none());
        assert_eq!(finalized.backup_path.as_ref(), Some(&backup_path));
        assert_eq!(
            recovered.target_status(&request).unwrap().phase,
            PeerCloneTargetPhase::Completed
        );
        assert_eq!(
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()[0]
                .total_bytes,
            downloaded.total_bytes.unwrap()
        );
        assert!(target_cas
            .open_object(&authoritative.content_hash)
            .unwrap()
            .is_some());
        assert_eq!(target_store.revision().unwrap(), finalized.revision);
        recovered.release_target(&request).unwrap();
        assert!(!target_job_root.exists());
        assert!(target_cas
            .open_object(&authoritative.content_hash)
            .unwrap()
            .is_some());
        assert_eq!(target_store.revision().unwrap(), finalized.revision);
        recovered.release_target(&request).unwrap();
        assert!(recovered.target_status(&request).is_err());
        source.stop_source(pairing.session_id.as_str()).unwrap();
    }

    #[test]
    fn product_remote_completion_response_and_ack_loss_replay_after_restart() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let target_cas = PayloadCas::new(target_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        let mut target_store = PersistentStore::open(target_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        seed_product_store(&mut target_store, "Target", 0);
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_root.path().join("peer-sync"),
                &NeverCancelled,
            )
            .unwrap();
        source
            .configure_v2_registry(
                source_root.path(),
                "Windows",
                super::super::device_registry::DevicePermissions::read(),
            )
            .unwrap();
        let running = source
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());
        let request = PeerCloneTargetRequest {
            endpoint: format!(
                "http://127.0.0.1:{}",
                source.source_bind_address().unwrap().unwrap().port()
            ),
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
        };
        let peer_root = target_root.path().join("peer-sync");
        let target = PeerCloneCommandState::default();
        let claim_result = target
            .claim_v2_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        let source_device_id = claim_result.source_device_id;
        let operation_id = target
            .lock_runtime()
            .unwrap()
            .target
            .as_ref()
            .unwrap()
            .operation_id
            .clone();
        let marker_path = peer_root
            .join("targets")
            .join(&request.session_id)
            .join(TARGET_OPERATION_MARKER_FILE);
        let completion_marker: Value =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        assert_eq!(completion_marker["completionCapability"], "v1");
        let completion_lease_id = completion_marker["completionLeaseId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(is_canonical_v4_uuid(&completion_lease_id));
        assert_ne!(completion_lease_id, operation_id);
        target
            .start_target_download(&peer_root, request.clone())
            .unwrap();
        let downloaded = wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation);
        target
            .fail_target_activation_ledger_once_for_test()
            .unwrap();
        target
            .fail_target_after_remote_completion_once_for_test()
            .unwrap();

        assert_eq!(
            target
                .finalize_target(&mut target_store, &target_cas, &peer_root, &request)
                .unwrap_err(),
            PeerSyncError::Storage("injected failure after remote clone completion".to_owned())
        );

        assert_eq!(target_store.revision().unwrap(), 2);
        let backup_path = fs::canonicalize(target_backup_path(&peer_root, &operation_id)).unwrap();
        assert!(backup_path.is_file());
        assert!(retryable_target_operation_references_backup(&peer_root, &backup_path).unwrap());
        assert_eq!(
            backup_path.parent(),
            Some(
                fs::canonicalize(peer_root.join("activation/backups"))
                    .unwrap()
                    .as_path()
            )
        );
        assert_eq!(
            target.target_status(&request).unwrap().phase,
            PeerCloneTargetPhase::AwaitingActivation
        );
        assert_eq!(
            target_store.read_root(None).unwrap().value["username"],
            "Source"
        );
        let accounted_once =
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()
                .iter()
                .find(|source| source.device_id == source_device_id)
                .unwrap()
                .total_bytes;
        assert_eq!(accounted_once, 0);
        let remotely_accounted_once = downloaded.total_bytes.unwrap();
        assert_eq!(
            super::super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            remotely_accounted_once
        );
        let incoming_file: Value = serde_json::from_slice(
            &fs::read(target_root.path().join("peer-sync/sources.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            incoming_file["pendingCompletionDeliveries"][0]["receiptId"],
            super::super::device_registry::completion_receipt_id(
                "clone",
                &completion_lease_id,
                &request.manifest_id,
            )
        );
        assert_eq!(
            incoming_file["completedReceipts"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0),
            0
        );
        assert_eq!(
            incoming_file["pendingCompletionDeliveries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            super::super::registry_commands::remove_incoming_source_if_inactive(
                target_root.path(),
                &source_device_id,
            )
            .is_err()
        );
        let marker_before_restart: Value = serde_json::from_slice(
            &fs::read(
                peer_root
                    .join("targets")
                    .join(&request.session_id)
                    .join(TARGET_OPERATION_MARKER_FILE),
            )
            .unwrap(),
        )
        .unwrap();
        assert_ne!(marker_before_restart["completionAcknowledged"], true);
        drop(target);
        let recovered = PeerCloneCommandState::initialize(&peer_root);
        assert!(peer_root.join("targets").join(&request.session_id).exists());
        recovered
            .resume_target_download(&peer_root, request.clone())
            .unwrap();
        wait_for_target_phase(&recovered, PeerCloneTargetPhase::AwaitingActivation);
        recovered
            .fail_target_finalize_completion_ack_once_for_test()
            .unwrap();
        assert_eq!(
            recovered
                .finalize_target(&mut target_store, &target_cas, &peer_root, &request)
                .unwrap_err(),
            PeerSyncError::Storage(
                "injected peer clone completion acknowledgement failure".to_owned()
            )
        );
        assert_eq!(
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()
                .iter()
                .find(|source| source.device_id == source_device_id)
                .unwrap()
                .total_bytes,
            remotely_accounted_once
        );
        let after_ack_loss: Value = serde_json::from_slice(
            &fs::read(target_root.path().join("peer-sync/sources.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            after_ack_loss["pendingCompletionDeliveries"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0),
            0
        );
        assert_ne!(
            serde_json::from_slice::<Value>(&fs::read(&marker_path).unwrap()).unwrap()
                ["completionAcknowledged"],
            true
        );
        drop(recovered);

        let recovered = PeerCloneCommandState::initialize(&peer_root);
        let blocked_request = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:9".to_owned(),
            session_id: uuid::Uuid::new_v4().to_string(),
            manifest_id: "e".repeat(64),
        };
        assert_eq!(
            recovered
                .claim_v2_target(
                    &peer_root,
                    &blocked_request.endpoint,
                    &blocked_request.session_id,
                    &blocked_request.manifest_id,
                    "unused-claim",
                )
                .unwrap_err(),
            PeerSyncError::Protocol(
                "another persisted peer clone target job is unresolved".to_owned()
            )
        );
        recovered
            .resume_target_download(&peer_root, request.clone())
            .unwrap();
        wait_for_target_phase(&recovered, PeerCloneTargetPhase::AwaitingActivation);
        source.stop_source(&request.session_id).unwrap();
        let recovered_receipt = recovered
            .finalize_target(&mut target_store, &target_cas, &peer_root, &request)
            .unwrap();
        assert_eq!(recovered_receipt.backup_path.as_ref(), Some(&backup_path));
        assert_eq!(
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()
                .iter()
                .find(|source| source.device_id == source_device_id)
                .unwrap()
                .total_bytes,
            remotely_accounted_once
        );
        assert_eq!(
            super::super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            remotely_accounted_once
        );
        let marker_after_retry: Value = serde_json::from_slice(
            &fs::read(
                peer_root
                    .join("targets")
                    .join(&request.session_id)
                    .join(TARGET_OPERATION_MARKER_FILE),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(marker_after_retry["completionLeaseId"], completion_lease_id);
        assert_eq!(marker_after_retry["completionAcknowledged"], true);
        assert!(!retryable_target_operation_references_backup(&peer_root, &backup_path).unwrap());
        recovered.release_target(&request).unwrap();
        assert!(!peer_root.join("targets").join(&request.session_id).exists());
        assert_eq!(
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()
                .iter()
                .find(|source| source.device_id == source_device_id)
                .unwrap()
                .total_bytes,
            remotely_accounted_once
        );
    }

    #[test]
    fn product_backup_receipt_failure_stays_retryable_after_commit() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let target_cas = PayloadCas::new(target_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        let mut target_store = PersistentStore::open(target_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        seed_product_store(&mut target_store, "Target", 0);
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_root.path().join("peer-sync"),
                &NeverCancelled,
            )
            .unwrap();
        source
            .configure_v2_registry(
                source_root.path(),
                "Windows",
                super::super::device_registry::DevicePermissions::read(),
            )
            .unwrap();
        let running = source
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());
        let request = PeerCloneTargetRequest {
            endpoint: format!(
                "http://127.0.0.1:{}",
                source.source_bind_address().unwrap().unwrap().port()
            ),
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
        };
        let peer_root = target_root.path().join("peer-sync");
        let target = PeerCloneCommandState::default();
        target
            .claim_v2_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        let operation_id = target
            .lock_runtime()
            .unwrap()
            .target
            .as_ref()
            .unwrap()
            .operation_id
            .clone();
        target
            .start_target_download(&peer_root, request.clone())
            .unwrap();
        wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation);
        target
            .fail_target_finalize_backup_receipt_once_for_test()
            .unwrap();

        assert!(target
            .finalize_target(&mut target_store, &target_cas, &peer_root, &request)
            .is_err());
        assert_eq!(target_store.revision().unwrap(), 2);
        assert_eq!(
            target.target_status(&request).unwrap().phase,
            PeerCloneTargetPhase::Failed
        );

        drop(target);
        let recovered = PeerCloneCommandState::initialize(&peer_root);
        recovered
            .resume_target_download(&peer_root, request.clone())
            .unwrap();
        assert_eq!(
            recovered
                .lock_runtime()
                .unwrap()
                .target
                .as_ref()
                .unwrap()
                .operation_id,
            operation_id
        );
        wait_for_target_phase(&recovered, PeerCloneTargetPhase::AwaitingActivation);
        let receipt = recovered
            .finalize_target(&mut target_store, &target_cas, &peer_root, &request)
            .unwrap();
        assert_eq!(receipt.revision, 2);
        assert!(receipt.backup_path.as_ref().unwrap().is_file());
        assert_eq!(
            fs::read_dir(peer_root.join("activation/backups"))
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            super::super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()
                .len(),
            1
        );
        assert_eq!(
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()
                .len(),
            1
        );
        source.stop_source(&request.session_id).unwrap();
    }

    #[test]
    fn registered_target_two_clones_from_one_source_session_get_distinct_backup_receipts() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let target_cas = PayloadCas::new(target_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        let mut target_store = PersistentStore::open(target_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        seed_product_store(&mut target_store, "Target", 0);
        let source = PeerCloneCommandState::default();
        let source_peer_root = source_root.path().join("peer-sync");
        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_peer_root,
                &NeverCancelled,
            )
            .unwrap();
        source
            .configure_v2_registry(
                source_root.path(),
                "Windows",
                super::super::device_registry::DevicePermissions::read(),
            )
            .unwrap();
        let running = source
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());
        let request = PeerCloneTargetRequest {
            endpoint: format!(
                "http://127.0.0.1:{}",
                source.source_bind_address().unwrap().unwrap().port()
            ),
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
        };
        let peer_root = target_root.path().join("peer-sync");
        let target = PeerCloneCommandState::default();
        let claim = target
            .claim_v2_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        let source_device_id = claim.source_device_id;
        let registered_source =
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()[0]
                .clone();
        target
            .start_target_download(&peer_root, request.clone())
            .unwrap();
        let first_total = wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation)
            .total_bytes
            .unwrap();
        let first_operation_id = target
            .lock_runtime()
            .unwrap()
            .target
            .as_ref()
            .unwrap()
            .operation_id
            .clone();
        let first_completion_lease = read_target_operation_marker(
            &target_paths(&peer_root, &request)
                .unwrap()
                .job_root
                .join(TARGET_OPERATION_MARKER_FILE),
            &request,
        )
        .unwrap()
        .unwrap()
        .completion_lease_id
        .unwrap();
        let first = target
            .finalize_target(&mut target_store, &target_cas, &peer_root, &request)
            .unwrap();
        assert_eq!(
            super::super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            first_total
        );
        assert_eq!(
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()[0]
                .total_bytes,
            first_total
        );
        target.release_target(&request).unwrap();

        let mut local_root = target_store.read_root(None).unwrap().value;
        local_root["username"] = Value::String("Local edit".to_owned());
        let local_revision = target_store.revision().unwrap();
        target_store
            .commit(&WorkingSetCommit {
                expected_revision: local_revision,
                root: Some(local_root),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();

        target
            .connect_registered_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &source_device_id,
                &registered_source.bearer,
                PeerCompletionCapability::V1,
            )
            .unwrap();
        let second_operation_id = target
            .lock_runtime()
            .unwrap()
            .target
            .as_ref()
            .unwrap()
            .operation_id
            .clone();
        assert_ne!(second_operation_id, first_operation_id);
        let second_completion_lease = read_target_operation_marker(
            &target_paths(&peer_root, &request)
                .unwrap()
                .job_root
                .join(TARGET_OPERATION_MARKER_FILE),
            &request,
        )
        .unwrap()
        .unwrap()
        .completion_lease_id
        .unwrap();
        assert_ne!(second_completion_lease, first_completion_lease);
        target
            .start_target_download(&peer_root, request.clone())
            .unwrap();
        let second_total = wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation)
            .total_bytes
            .unwrap();
        let second = target
            .finalize_target(&mut target_store, &target_cas, &peer_root, &request)
            .unwrap();

        assert_ne!(second.backup_path, first.backup_path);
        assert_eq!(
            fs::read_dir(peer_root.join("activation/backups"))
                .unwrap()
                .count(),
            2
        );
        assert_eq!(
            target_store.read_root(None).unwrap().value["username"],
            "Source"
        );
        assert_eq!(
            super::super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            first_total + second_total
        );
        assert_eq!(
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap()
                .sources()[0]
                .total_bytes,
            first_total + second_total
        );

        target.release_target(&request).unwrap();
        target
            .connect_registered_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &source_device_id,
                &registered_source.bearer,
                PeerCompletionCapability::V1,
            )
            .unwrap();
        target
            .start_target_download(&peer_root, request.clone())
            .unwrap();
        wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation);
        assert!(target
            .finalize_target(&mut target_store, &target_cas, &peer_root, &request)
            .is_err());
        assert_eq!(
            super::super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            first_total + second_total
        );
        let incoming =
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap();
        assert_eq!(
            incoming.sources()[0].total_bytes,
            first_total + second_total
        );
        let marker = read_target_operation_marker(
            &target_paths(&peer_root, &request)
                .unwrap()
                .job_root
                .join(TARGET_OPERATION_MARKER_FILE),
            &request,
        )
        .unwrap()
        .unwrap();
        assert!(marker.backup_path.is_none());
        assert!(!marker.completion_acknowledged);
        source.stop_source(&request.session_id).unwrap();
    }

    #[test]
    fn product_resume_repairs_promoted_but_unrecorded_object_before_awaiting_activation() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_root.path().join("peer-sync"),
                &NeverCancelled,
            )
            .unwrap();
        source
            .configure_v2_registry(
                source_root.path(),
                "Windows",
                super::super::device_registry::DevicePermissions::read(),
            )
            .unwrap();
        let running = source
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());
        let request = PeerCloneTargetRequest {
            endpoint: format!(
                "http://127.0.0.1:{}",
                source.source_bind_address().unwrap().unwrap().port()
            ),
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
        };
        let peer_root = target_root.path().join("peer-sync");
        let target = PeerCloneCommandState::default();
        target
            .claim_v2_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        target
            .fail_target_after_cas_promotion_once_for_test()
            .unwrap();
        target
            .start_target_download(&peer_root, request.clone())
            .unwrap();
        wait_for_target_phase(&target, PeerCloneTargetPhase::Failed);

        target
            .resume_target_download(&peer_root, request.clone())
            .unwrap();
        wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation);

        source.stop_source(&request.session_id).unwrap();
    }

    #[test]
    fn product_cancel_preserves_persisted_claim_and_verified_resume_state() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 9 * 1024 * 1024);
        let source_peer_root = source_root.path().join("peer-sync");
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_peer_root,
                &NeverCancelled,
            )
            .unwrap();
        source
            .configure_v2_registry(
                source_root.path(),
                "Windows",
                super::super::device_registry::DevicePermissions::read(),
            )
            .unwrap();
        let running = source
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());
        let bound = source.source_bind_address().unwrap().unwrap();
        let request = PeerCloneTargetRequest {
            endpoint: format!("http://127.0.0.1:{}", bound.port()),
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
        };
        let target = PeerCloneCommandState::default();
        let peer_root = target_root.path().join("peer-sync");
        target
            .claim_v2_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        let transfer_pause = Arc::new(Barrier::new(2));
        target
            .pause_target_after_verified_chunk_for_test(Arc::clone(&transfer_pause))
            .unwrap();
        target
            .start_target_download(&peer_root, request.clone())
            .unwrap();
        transfer_pause.wait();
        let cancel_state = target.clone();
        let cancel_request = request.clone();
        let cancel = thread::spawn(move || cancel_state.cancel_target(&cancel_request));
        let cancelling = wait_for_target_phase(&target, PeerCloneTargetPhase::Cancelling);
        assert!(cancelling.completed_bytes > 0);
        assert!(target
            .resume_target_download(&peer_root, request.clone())
            .is_err());
        let mut target_store = PersistentStore::open(target_root.path()).unwrap();
        let target_cas = PayloadCas::new(target_root.path()).unwrap();
        assert!(target
            .finalize_target(&mut target_store, &target_cas, &peer_root, &request)
            .is_err());
        transfer_pause.wait();
        cancel.join().unwrap().unwrap();
        wait_for_target_phase(&target, PeerCloneTargetPhase::Cancelled);
        let credential = peer_root
            .join("targets")
            .join(&request.session_id)
            .join("credential.json");
        let ledger = peer_root
            .join("targets")
            .join(&request.session_id)
            .join("transfer")
            .join("ledger.jsonl");
        assert!(credential.is_file());
        let persisted_chunk = first_persisted_chunk_event(&ledger);
        let different_session = PeerCloneTargetRequest {
            session_id: "223e4567-e89b-42d3-a456-426614174000".to_owned(),
            ..request.clone()
        };
        let different_manifest = PeerCloneTargetRequest {
            manifest_id: "0".repeat(64),
            ..request.clone()
        };
        assert!(target
            .claim_v2_target(
                &peer_root,
                &different_session.endpoint,
                &different_session.session_id,
                &different_session.manifest_id,
                &pairing.claim,
            )
            .is_err());
        assert!(target
            .claim_v2_target(
                &peer_root,
                &different_manifest.endpoint,
                &different_manifest.session_id,
                &different_manifest.manifest_id,
                &pairing.claim,
            )
            .is_err());
        target
            .resume_target_download(&peer_root, request.clone())
            .unwrap();
        wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation);
        assert_eq!(
            persisted_chunk_event_count(&ledger, &persisted_chunk),
            1,
            "a cancelled transfer must not redownload an already verified chunk"
        );

        assert_eq!(source.source_status().unwrap().devices.len(), 1);
        source.stop_source(&request.session_id).unwrap();
    }

    #[test]
    fn product_target_repair_rejects_activation_and_completed_phases_before_claim() {
        for phase in [
            PeerCloneTargetPhase::AwaitingActivation,
            PeerCloneTargetPhase::Activating,
            PeerCloneTargetPhase::Completed,
        ] {
            let source_root = tempfile::tempdir().unwrap();
            let target_root = tempfile::tempdir().unwrap();
            let verifier_root = tempfile::tempdir().unwrap();
            let source_cas = PayloadCas::new(source_root.path()).unwrap();
            let mut source_store = PersistentStore::open(source_root.path()).unwrap();
            seed_product_store(&mut source_store, "Source", 0);
            let source_peer_root = source_root.path().join("peer-sync");
            let source = PeerCloneCommandState::default();
            let prepared = source
                .prepare_source(
                    &mut source_store,
                    &source_cas,
                    &source_peer_root,
                    &NeverCancelled,
                )
                .unwrap();
            source
                .configure_v2_registry(
                    source_root.path(),
                    "Windows",
                    super::super::device_registry::DevicePermissions::read(),
                )
                .unwrap();
            let running = source
                .start_source(
                    prepared.session_id.as_deref().unwrap(),
                    Ipv4Addr::new(192, 168, 1, 4),
                )
                .unwrap();
            let pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());
            let target_peer_root = target_root.path().join("peer-sync");
            let target = PeerCloneCommandState::default();
            target
                .claim_v2_target(
                    &target_peer_root,
                    &format!(
                        "http://127.0.0.1:{}",
                        source.source_bind_address().unwrap().unwrap().port()
                    ),
                    &pairing.session_id,
                    &pairing.manifest_id,
                    &pairing.claim,
                )
                .unwrap();
            target
                .lock_runtime()
                .unwrap()
                .target
                .as_mut()
                .unwrap()
                .status
                .phase = phase;

            source.shutdown_for_exit();
            drop(source);
            let reopened = PeerCloneCommandState::initialize(&source_peer_root);
            reopened
                .configure_v2_registry(
                    source_root.path(),
                    "Windows",
                    super::super::device_registry::DevicePermissions::read(),
                )
                .unwrap();
            let restarted = reopened
                .start_source(&pairing.session_id, Ipv4Addr::new(192, 168, 1, 4))
                .unwrap();
            let repaired = parse_product_pairing(restarted.pairing_uri.as_deref().unwrap());
            let repaired_endpoint = format!(
                "http://127.0.0.1:{}",
                reopened.source_bind_address().unwrap().unwrap().port()
            );

            assert!(target
                .claim_v2_target(
                    &target_peer_root,
                    &repaired_endpoint,
                    &repaired.session_id,
                    &repaired.manifest_id,
                    &repaired.claim,
                )
                .is_err());
            PeerCloneCommandState::default()
                .claim_v2_target(
                    &verifier_root.path().join("peer-sync"),
                    &repaired_endpoint,
                    &repaired.session_id,
                    &repaired.manifest_id,
                    &repaired.claim,
                )
                .unwrap();
            reopened.stop_source(&repaired.session_id).unwrap();
        }
    }

    #[test]
    fn product_target_repair_reservation_blocks_old_worker_until_the_claim_finishes() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        let source_peer_root = source_root.path().join("peer-sync");
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_peer_root,
                &NeverCancelled,
            )
            .unwrap();
        source
            .configure_v2_registry(
                source_root.path(),
                "Windows",
                super::super::device_registry::DevicePermissions::read(),
            )
            .unwrap();
        let running = source
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let first_pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());
        let first_request = PeerCloneTargetRequest {
            endpoint: format!(
                "http://127.0.0.1:{}",
                source.source_bind_address().unwrap().unwrap().port()
            ),
            session_id: first_pairing.session_id.clone(),
            manifest_id: first_pairing.manifest_id.clone(),
        };
        let target_peer_root = target_root.path().join("peer-sync");
        let target = PeerCloneCommandState::default();
        target
            .claim_v2_target(
                &target_peer_root,
                &first_request.endpoint,
                &first_request.session_id,
                &first_request.manifest_id,
                &first_pairing.claim,
            )
            .unwrap();
        target
            .lock_runtime()
            .unwrap()
            .target
            .as_mut()
            .unwrap()
            .status
            .phase = PeerCloneTargetPhase::Failed;
        let credential_path = target_paths(&target_peer_root, &first_request)
            .unwrap()
            .credential;
        let first_credential = fs::read(&credential_path).unwrap();

        source.shutdown_for_exit();
        drop(source);
        let reopened = PeerCloneCommandState::initialize(&source_peer_root);
        reopened
            .configure_v2_registry(
                source_root.path(),
                "Windows",
                super::super::device_registry::DevicePermissions::read(),
            )
            .unwrap();
        let restarted = reopened
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let repaired_pairing = parse_product_pairing(restarted.pairing_uri.as_deref().unwrap());
        let repaired_request = PeerCloneTargetRequest {
            endpoint: format!(
                "http://127.0.0.1:{}",
                reopened.source_bind_address().unwrap().unwrap().port()
            ),
            session_id: repaired_pairing.session_id.clone(),
            manifest_id: repaired_pairing.manifest_id.clone(),
        };
        let claim_pause = Arc::new(Barrier::new(2));
        target
            .pause_target_claim_before_network_for_test(Arc::clone(&claim_pause))
            .unwrap();
        let claiming_target = target.clone();
        let claiming_root = target_peer_root.clone();
        let claiming_request = repaired_request.clone();
        let repaired_claim = repaired_pairing.claim.clone();
        let claim = thread::spawn(move || {
            claiming_target.claim_v2_target(
                &claiming_root,
                &claiming_request.endpoint,
                &claiming_request.session_id,
                &claiming_request.manifest_id,
                &repaired_claim,
            )
        });

        claim_pause.wait();
        let devices_before_claim = reopened.source_status().unwrap().devices.len();
        let credential_before_claim = fs::read(&credential_path).unwrap();
        let resume_while_claiming =
            target.resume_target_download(&target_peer_root, first_request.clone());
        let (phase_while_claiming, worker_started, request_while_claiming) = {
            let runtime = target.lock_runtime().unwrap();
            let owned = runtime.target.as_ref().unwrap();
            (
                owned.status.phase,
                owned.worker.is_some(),
                owned.request.clone(),
            )
        };

        claim_pause.wait();
        let claim_result = claim.join().unwrap();
        assert_eq!(devices_before_claim, 1);
        assert_eq!(credential_before_claim, first_credential);
        assert!(resume_while_claiming.is_err());
        assert_eq!(phase_while_claiming, PeerCloneTargetPhase::Failed);
        assert!(!worker_started);
        assert_eq!(request_while_claiming, first_request);
        // A registered source refuses to rotate a bearer while this target still
        // holds completion state for the job, so the repair claim fails and the
        // reservation has to leave the persisted credential exactly as it was.
        assert!(claim_result.is_err());
        assert_eq!(fs::read(&credential_path).unwrap(), first_credential);
        assert_eq!(reopened.source_status().unwrap().devices.len(), 1);
        assert_eq!(
            target.target_status(&first_request).unwrap().phase,
            PeerCloneTargetPhase::Failed
        );
        reopened.stop_source(&repaired_request.session_id).unwrap();
    }

    #[test]
    fn registered_target_reconnect_replaces_only_a_terminal_owner_after_source_restart() {
        let root = tempfile::tempdir().unwrap();
        let peer_root = root.path().join("peer-clone");
        let state = PeerCloneCommandState::default();
        let first = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000201".to_owned(),
            manifest_id: "a".repeat(64),
        };
        let restarted = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32146".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000202".to_owned(),
            manifest_id: "c".repeat(64),
        };
        let source_device_id = "00000000-0000-4000-8000-000000000203";
        let bearer = "b".repeat(64);
        let register_source = |endpoint: &str| {
            super::super::device_registry::register_incoming_source(
                root.path(),
                super::super::device_registry::IncomingSource {
                    device_id: source_device_id.to_owned(),
                    name: "Desktop source".to_owned(),
                    endpoint: endpoint.to_owned(),
                    bearer: bearer.clone(),
                    permissions: super::super::device_registry::DevicePermissions::read(),
                    last_seen_ms: 0,
                    total_bytes: 0,
                },
            )
            .unwrap();
        };
        register_source(&first.endpoint);

        state
            .connect_registered_target(
                &peer_root,
                &first.endpoint,
                &first.session_id,
                &first.manifest_id,
                source_device_id,
                &bearer,
                PeerCompletionCapability::Unsupported,
            )
            .unwrap();
        assert!(registered_clone_source_is_active(root.path(), Some(source_device_id)).unwrap());
        assert!(
            super::super::registry_commands::remove_incoming_source_if_inactive(
                root.path(),
                source_device_id,
            )
            .is_err()
        );
        assert!(state
            .connect_registered_target(
                &peer_root,
                &restarted.endpoint,
                &restarted.session_id,
                &restarted.manifest_id,
                source_device_id,
                &bearer,
                PeerCompletionCapability::Unsupported,
            )
            .is_err());
        let first_root = target_paths(&peer_root, &first).unwrap().job_root;
        let restarted_root = target_paths(&peer_root, &restarted).unwrap().job_root;
        state
            .lock_runtime()
            .unwrap()
            .target
            .as_mut()
            .unwrap()
            .status
            .phase = PeerCloneTargetPhase::AwaitingActivation;
        assert!(state
            .connect_registered_target(
                &peer_root,
                &restarted.endpoint,
                &restarted.session_id,
                &restarted.manifest_id,
                source_device_id,
                &bearer,
                PeerCompletionCapability::Unsupported,
            )
            .is_err());
        state
            .lock_runtime()
            .unwrap()
            .target
            .as_mut()
            .unwrap()
            .status
            .phase = PeerCloneTargetPhase::Failed;
        register_source(&restarted.endpoint);

        let result = state
            .connect_registered_target(
                &peer_root,
                &restarted.endpoint,
                &restarted.session_id,
                &restarted.manifest_id,
                source_device_id,
                &bearer,
                PeerCompletionCapability::Unsupported,
            )
            .unwrap();

        assert_eq!(result.source_device_id, source_device_id);
        assert!(!first_root.try_exists().unwrap());
        assert!(restarted_root.try_exists().unwrap());
        let runtime = state.lock_runtime().unwrap();
        let target = runtime.target.as_ref().unwrap();
        assert_eq!(target.request, restarted);
        assert_eq!(target.status.phase, PeerCloneTargetPhase::Idle);
    }

    #[test]
    fn registered_target_reconnect_cleanup_failure_preserves_terminal_ownership() {
        let root = tempfile::tempdir().unwrap();
        let peer_root = root.path().join("peer-clone");
        let state = PeerCloneCommandState::default();
        let first = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000211".to_owned(),
            manifest_id: "d".repeat(64),
        };
        let restarted = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32146".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000212".to_owned(),
            manifest_id: "e".repeat(64),
        };
        let source_device_id = "00000000-0000-4000-8000-000000000213";
        let bearer = "f".repeat(64);
        let register_source = |endpoint: &str| {
            super::super::device_registry::register_incoming_source(
                root.path(),
                super::super::device_registry::IncomingSource {
                    device_id: source_device_id.to_owned(),
                    name: "Desktop source".to_owned(),
                    endpoint: endpoint.to_owned(),
                    bearer: bearer.clone(),
                    permissions: super::super::device_registry::DevicePermissions::read(),
                    last_seen_ms: 0,
                    total_bytes: 0,
                },
            )
            .unwrap();
        };
        register_source(&first.endpoint);
        state
            .connect_registered_target(
                &peer_root,
                &first.endpoint,
                &first.session_id,
                &first.manifest_id,
                source_device_id,
                &bearer,
                PeerCompletionCapability::Unsupported,
            )
            .unwrap();
        state
            .lock_runtime()
            .unwrap()
            .target
            .as_mut()
            .unwrap()
            .status
            .phase = PeerCloneTargetPhase::Cancelled;
        state.fail_target_release_cleanup_once_for_test().unwrap();
        let first_root = target_paths(&peer_root, &first).unwrap().job_root;
        let restarted_root = target_paths(&peer_root, &restarted).unwrap().job_root;

        assert!(matches!(
            state.connect_registered_target(
                &peer_root,
                &restarted.endpoint,
                &restarted.session_id,
                &restarted.manifest_id,
                source_device_id,
                &bearer,
                PeerCompletionCapability::Unsupported,
            ),
            Err(PeerSyncError::Storage(message))
                if message == "injected peer clone target release cleanup failure"
        ));
        assert!(first_root.try_exists().unwrap());
        assert!(!restarted_root.try_exists().unwrap());
        {
            let runtime = state.lock_runtime().unwrap();
            let target = runtime.target.as_ref().unwrap();
            assert_eq!(target.request, first);
            assert_eq!(target.status.phase, PeerCloneTargetPhase::Cancelled);
            assert!(!runtime.target_claiming);
        }

        register_source(&restarted.endpoint);
        state
            .connect_registered_target(
                &peer_root,
                &restarted.endpoint,
                &restarted.session_id,
                &restarted.manifest_id,
                source_device_id,
                &bearer,
                PeerCompletionCapability::Unsupported,
            )
            .unwrap();
        assert!(!first_root.try_exists().unwrap());
        assert!(restarted_root.try_exists().unwrap());
    }

    #[test]
    fn registered_target_publish_revalidates_a_concurrently_removed_source() {
        let root = tempfile::tempdir().unwrap();
        let peer_root = root.path().join("peer-clone");
        let state = PeerCloneCommandState::default();
        let source_device_id = "00000000-0000-4000-8000-000000000223";
        let bearer = "a".repeat(64);
        let request = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000224".to_owned(),
            manifest_id: "b".repeat(64),
        };
        super::super::device_registry::register_incoming_source(
            root.path(),
            super::super::device_registry::IncomingSource {
                device_id: source_device_id.to_owned(),
                name: "Desktop source".to_owned(),
                endpoint: request.endpoint.clone(),
                bearer: bearer.clone(),
                permissions: super::super::device_registry::DevicePermissions::read(),
                last_seen_ms: 0,
                total_bytes: 0,
            },
        )
        .unwrap();
        let pause = Arc::new(Barrier::new(2));
        state
            .pause_target_claim_before_network_for_test(Arc::clone(&pause))
            .unwrap();
        let worker_state = state.clone();
        let worker_root = peer_root.clone();
        let worker_request = request.clone();
        let worker_bearer = bearer.clone();
        let worker = thread::spawn(move || {
            worker_state.connect_registered_target(
                &worker_root,
                &worker_request.endpoint,
                &worker_request.session_id,
                &worker_request.manifest_id,
                source_device_id,
                &worker_bearer,
                PeerCompletionCapability::Unsupported,
            )
        });

        pause.wait();
        super::super::registry_commands::remove_incoming_source_if_inactive(
            root.path(),
            source_device_id,
        )
        .unwrap();
        pause.wait();

        assert_eq!(
            worker.join().unwrap().unwrap_err(),
            PeerSyncError::Validation(REGISTERED_SOURCE_CHANGED.to_owned())
        );
        assert!(state.lock_runtime().unwrap().target.is_none());
        assert!(!target_paths(&peer_root, &request)
            .unwrap()
            .job_root
            .try_exists()
            .unwrap());
    }

    #[test]
    fn registered_target_publication_serializes_source_removal_after_revalidation() {
        let root = tempfile::tempdir().unwrap();
        let peer_root = root.path().join("peer-clone");
        let state = PeerCloneCommandState::default();
        let source_device_id = "00000000-0000-4000-8000-000000000229";
        let bearer = "5".repeat(64);
        let request = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000230".to_owned(),
            manifest_id: "6".repeat(64),
        };
        super::super::device_registry::register_incoming_source(
            root.path(),
            super::super::device_registry::IncomingSource {
                device_id: source_device_id.to_owned(),
                name: "Desktop source".to_owned(),
                endpoint: request.endpoint.clone(),
                bearer: bearer.clone(),
                permissions: super::super::device_registry::DevicePermissions::read(),
                last_seen_ms: 0,
                total_bytes: 0,
            },
        )
        .unwrap();
        let reached_publish = Arc::new(Barrier::new(2));
        let resume_publish = Arc::new(Barrier::new(2));
        state
            .pause_registered_target_publish_once_for_test(
                Arc::clone(&reached_publish),
                Arc::clone(&resume_publish),
            )
            .unwrap();
        let worker_state = state.clone();
        let worker_root = peer_root.clone();
        let worker_request = request.clone();
        let worker_bearer = bearer.clone();
        let worker = thread::spawn(move || {
            worker_state.connect_registered_target(
                &worker_root,
                &worker_request.endpoint,
                &worker_request.session_id,
                &worker_request.manifest_id,
                source_device_id,
                &worker_bearer,
                PeerCompletionCapability::Unsupported,
            )
        });

        reached_publish.wait();
        let marker_path = target_paths(&peer_root, &request)
            .unwrap()
            .job_root
            .join(TARGET_OPERATION_MARKER_FILE);
        assert!(!marker_path.try_exists().unwrap());
        let (removal_tx, removal_rx) = mpsc::channel();
        let removal_root = root.path().to_owned();
        let removal = thread::spawn(move || {
            removal_tx
                .send(
                    super::super::registry_commands::remove_incoming_source_if_inactive(
                        &removal_root,
                        source_device_id,
                    ),
                )
                .unwrap();
        });
        assert!(removal_rx.recv_timeout(Duration::from_millis(100)).is_err());
        resume_publish.wait();

        worker.join().unwrap().unwrap();
        assert!(matches!(
            removal_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(PeerSyncError::Validation(message))
                if message == super::super::registry_commands::SOURCE_IN_USE
        ));
        removal.join().unwrap();
        assert!(marker_path.try_exists().unwrap());
        assert!(registered_clone_source_is_active(root.path(), Some(source_device_id)).unwrap());
    }

    #[test]
    fn registered_target_publication_wins_before_bearer_rotation() {
        let root = tempfile::tempdir().unwrap();
        let peer_root = root.path().join("peer-clone");
        let state = PeerCloneCommandState::default();
        let source_device_id = "00000000-0000-4000-8000-000000000233";
        let bearer = "7".repeat(64);
        let request = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000234".to_owned(),
            manifest_id: "8".repeat(64),
        };
        super::super::device_registry::register_incoming_source(
            root.path(),
            super::super::device_registry::IncomingSource {
                device_id: source_device_id.to_owned(),
                name: "Desktop source".to_owned(),
                endpoint: request.endpoint.clone(),
                bearer: bearer.clone(),
                permissions: super::super::device_registry::DevicePermissions::read(),
                last_seen_ms: 0,
                total_bytes: 0,
            },
        )
        .unwrap();
        let reached_publish = Arc::new(Barrier::new(2));
        let resume_publish = Arc::new(Barrier::new(2));
        state
            .pause_registered_target_publish_once_for_test(
                Arc::clone(&reached_publish),
                Arc::clone(&resume_publish),
            )
            .unwrap();
        let worker_state = state.clone();
        let worker_root = peer_root.clone();
        let worker_request = request.clone();
        let worker_bearer = bearer.clone();
        let worker = thread::spawn(move || {
            worker_state.connect_registered_target(
                &worker_root,
                &worker_request.endpoint,
                &worker_request.session_id,
                &worker_request.manifest_id,
                source_device_id,
                &worker_bearer,
                PeerCompletionCapability::Unsupported,
            )
        });

        reached_publish.wait();
        let sources_path = root.path().join("peer-sync/sources.json");
        let before = fs::read(&sources_path).unwrap();
        let (rotation_tx, rotation_rx) = mpsc::channel();
        let rotation_root = root.path().to_owned();
        let rotation = thread::spawn(move || {
            rotation_tx
                .send(
                    super::super::registry_commands::register_incoming_source_if_compatible(
                        &rotation_root,
                        super::super::device_registry::IncomingSource {
                            device_id: source_device_id.to_owned(),
                            name: "Rotated source".to_owned(),
                            endpoint: "http://127.0.0.1:32146".to_owned(),
                            bearer: "9".repeat(64),
                            permissions: super::super::device_registry::DevicePermissions::read(),
                            last_seen_ms: 1,
                            total_bytes: 0,
                        },
                    ),
                )
                .unwrap();
        });
        assert!(rotation_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        resume_publish.wait();

        worker.join().unwrap().unwrap();
        assert!(matches!(
            rotation_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(PeerSyncError::Validation(message))
                if message == super::super::registry_commands::SOURCE_IN_USE
        ));
        rotation.join().unwrap();
        assert_eq!(fs::read(&sources_path).unwrap(), before);
        assert!(registered_clone_source_is_active(root.path(), Some(source_device_id)).unwrap());
    }

    #[test]
    fn bearer_rotation_wins_before_registered_target_publication() {
        let root = tempfile::tempdir().unwrap();
        let peer_root = root.path().join("peer-clone");
        let state = PeerCloneCommandState::default();
        let source_device_id = "00000000-0000-4000-8000-000000000235";
        let bearer = "a".repeat(64);
        let request = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000236".to_owned(),
            manifest_id: "b".repeat(64),
        };
        super::super::device_registry::register_incoming_source(
            root.path(),
            super::super::device_registry::IncomingSource {
                device_id: source_device_id.to_owned(),
                name: "Desktop source".to_owned(),
                endpoint: request.endpoint.clone(),
                bearer: bearer.clone(),
                permissions: super::super::device_registry::DevicePermissions::read(),
                last_seen_ms: 0,
                total_bytes: 0,
            },
        )
        .unwrap();
        let pause = Arc::new(Barrier::new(2));
        state
            .pause_target_claim_before_network_for_test(Arc::clone(&pause))
            .unwrap();
        let worker_state = state.clone();
        let worker_root = peer_root.clone();
        let worker_request = request.clone();
        let worker_bearer = bearer.clone();
        let worker = thread::spawn(move || {
            worker_state.connect_registered_target(
                &worker_root,
                &worker_request.endpoint,
                &worker_request.session_id,
                &worker_request.manifest_id,
                source_device_id,
                &worker_bearer,
                PeerCompletionCapability::Unsupported,
            )
        });

        pause.wait();
        super::super::registry_commands::register_incoming_source_if_compatible(
            root.path(),
            super::super::device_registry::IncomingSource {
                device_id: source_device_id.to_owned(),
                name: "Rotated source".to_owned(),
                endpoint: request.endpoint.clone(),
                bearer: "c".repeat(64),
                permissions: super::super::device_registry::DevicePermissions::read(),
                last_seen_ms: 1,
                total_bytes: 0,
            },
        )
        .unwrap();
        pause.wait();

        let refusal = worker.join().unwrap().unwrap_err();
        assert_eq!(
            refusal,
            PeerSyncError::Validation(REGISTERED_SOURCE_CHANGED.to_owned())
        );
        assert_eq!(
            super::super::command_codes::code_for(&refusal).code(),
            "sourceChanged"
        );
        let marker_path = target_paths(&peer_root, &request)
            .unwrap()
            .job_root
            .join(TARGET_OPERATION_MARKER_FILE);
        assert!(!marker_path.try_exists().unwrap());
    }

    #[test]
    fn initialize_discards_an_unpublished_registered_target_orphan() {
        let root = tempfile::tempdir().unwrap();
        let peer_root = root.path().join("peer-clone");
        let source_device_id = "00000000-0000-4000-8000-000000000225";
        let bearer = "c".repeat(64);
        let request = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000226".to_owned(),
            manifest_id: "d".repeat(64),
        };
        super::super::device_registry::register_incoming_source(
            root.path(),
            super::super::device_registry::IncomingSource {
                device_id: source_device_id.to_owned(),
                name: "Desktop source".to_owned(),
                endpoint: request.endpoint.clone(),
                bearer: bearer.clone(),
                permissions: super::super::device_registry::DevicePermissions::read(),
                last_seen_ms: 0,
                total_bytes: 0,
            },
        )
        .unwrap();
        let state = PeerCloneCommandState::default();
        state
            .connect_registered_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                source_device_id,
                &bearer,
                PeerCompletionCapability::Unsupported,
            )
            .unwrap();
        let job_root = target_paths(&peer_root, &request).unwrap().job_root;
        assert!(job_root.exists());
        drop(state);

        let sources_path = root.path().join("peer-sync/sources.json");
        let mut sources: serde_json::Value =
            serde_json::from_slice(&fs::read(&sources_path).unwrap()).unwrap();
        sources["sources"] = serde_json::json!([]);
        fs::write(&sources_path, serde_json::to_vec(&sources).unwrap()).unwrap();

        let _recovered = PeerCloneCommandState::initialize(&peer_root);
        assert!(!job_root.exists());
    }

    #[test]
    fn initialize_discards_a_registered_target_crashed_before_marker_publication() {
        let root = tempfile::tempdir().unwrap();
        let peer_root = root.path().join("peer-clone");
        let source_device_id = "00000000-0000-4000-8000-000000000231";
        let bearer = "7".repeat(64);
        let request = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000232".to_owned(),
            manifest_id: "8".repeat(64),
        };
        super::super::device_registry::register_incoming_source(
            root.path(),
            super::super::device_registry::IncomingSource {
                device_id: source_device_id.to_owned(),
                name: "Desktop source".to_owned(),
                endpoint: request.endpoint.clone(),
                bearer: bearer.clone(),
                permissions: super::super::device_registry::DevicePermissions::read(),
                last_seen_ms: 0,
                total_bytes: 0,
            },
        )
        .unwrap();
        let state = PeerCloneCommandState::default();
        state
            .connect_registered_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                source_device_id,
                &bearer,
                PeerCompletionCapability::Unsupported,
            )
            .unwrap();
        let job_root = target_paths(&peer_root, &request).unwrap().job_root;
        fs::remove_file(job_root.join(TARGET_OPERATION_MARKER_FILE)).unwrap();
        drop(state);

        let _recovered = PeerCloneCommandState::initialize(&peer_root);

        assert!(!job_root.exists());
        assert!(super::super::device_registry::incoming_source_by_id(
            root.path(),
            source_device_id
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn legacy_registered_target_resume_does_not_opt_into_completion_v1() {
        let root = tempfile::tempdir().unwrap();
        let peer_root = root.path().join("peer-clone");
        let source_device_id = "00000000-0000-4000-8000-000000000227";
        let bearer = "e".repeat(64);
        let request = PeerCloneTargetRequest {
            endpoint: "http://127.0.0.1:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000228".to_owned(),
            manifest_id: "f".repeat(64),
        };
        super::super::device_registry::register_incoming_source(
            root.path(),
            super::super::device_registry::IncomingSource {
                device_id: source_device_id.to_owned(),
                name: "Desktop source".to_owned(),
                endpoint: request.endpoint.clone(),
                bearer: bearer.clone(),
                permissions: super::super::device_registry::DevicePermissions::read(),
                last_seen_ms: 0,
                total_bytes: 0,
            },
        )
        .unwrap();
        let state = PeerCloneCommandState::default();
        state
            .connect_registered_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                source_device_id,
                &bearer,
                PeerCompletionCapability::Unsupported,
            )
            .unwrap();
        drop(state);

        let recovered = PeerCloneCommandState::initialize(&peer_root);
        recovered
            .connect_registered_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                source_device_id,
                &bearer,
                PeerCompletionCapability::V1,
            )
            .unwrap();
        let marker: Value = serde_json::from_slice(
            &fs::read(
                target_paths(&peer_root, &request)
                    .unwrap()
                    .job_root
                    .join(TARGET_OPERATION_MARKER_FILE),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(marker.get("completionCapability").is_none());
        assert!(marker.get("completionLeaseId").is_none());
    }

    #[test]
    fn product_source_stop_owns_runtime_until_shutdown_and_cleanup_finish() {
        let source_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_root.path().join("peer-sync"),
                &NeverCancelled,
            )
            .unwrap();
        let session_id = prepared.session_id.unwrap();
        source
            .start_source(&session_id, Ipv4Addr::new(192, 168, 1, 4))
            .unwrap();
        let stop_pause = Arc::new(Barrier::new(2));
        source
            .pause_source_stop_before_cleanup_for_test(Arc::clone(&stop_pause))
            .unwrap();

        let stop_state = source.clone();
        let stop_session = session_id.clone();
        let stop = thread::spawn(move || stop_state.stop_source(&stop_session));
        stop_pause.wait();
        assert_eq!(
            source.source_status().unwrap().phase,
            PeerCloneSourcePhase::Stopping
        );
        assert!(source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_root.path().join("peer-sync"),
                &NeverCancelled,
            )
            .is_err());
        stop_pause.wait();
        stop.join().unwrap().unwrap();
        assert_eq!(
            source.source_status().unwrap().phase,
            PeerCloneSourcePhase::Stopped
        );
        assert!(!source.source_session_root_for_test().unwrap().exists());
    }

    #[test]
    fn product_source_stop_cleanup_failure_is_retryable() {
        let source_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_root.path().join("peer-sync"),
                &NeverCancelled,
            )
            .unwrap();
        let session_id = prepared.session_id.unwrap();
        let session_root = source.source_session_root_for_test().unwrap();
        source.fail_source_cleanup_once_for_test().unwrap();

        assert!(source.stop_source(&session_id).is_err());
        assert_eq!(
            source.source_status().unwrap().phase,
            PeerCloneSourcePhase::Stopping
        );
        assert!(session_root.exists());
        source.stop_source(&session_id).unwrap();
        assert_eq!(
            source.source_status().unwrap().phase,
            PeerCloneSourcePhase::Stopped
        );
        assert!(!session_root.exists());
    }

    #[test]
    fn successful_source_stop_sweeps_only_canonical_markerless_session_directories() {
        let source_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        let peer_root = source_root.path().join("peer-sync");
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(&mut source_store, &source_cas, &peer_root, &NeverCancelled)
            .unwrap();
        let sessions_root = peer_root.join("source-sessions");
        let markerless = sessions_root.join("123e4567-e89b-42d3-a456-426614174000");
        let uppercase = sessions_root.join("223E4567-E89B-42D3-A456-426614174000");
        let unrelated = sessions_root.join("unrelated");
        let canonical_file = sessions_root.join("323e4567-e89b-42d3-a456-426614174000");
        let link_target = source_root.path().join("source-link-target");
        let canonical_link = sessions_root.join("423e4567-e89b-42d3-a456-426614174000");
        fs::create_dir(&markerless).unwrap();
        fs::create_dir(&uppercase).unwrap();
        fs::create_dir(&unrelated).unwrap();
        fs::write(&canonical_file, b"preserve file").unwrap();
        fs::create_dir(&link_target).unwrap();
        fs::write(link_target.join("sentinel"), b"preserve link target").unwrap();
        create_directory_link(&link_target, &canonical_link);

        source
            .stop_source(prepared.session_id.as_deref().unwrap())
            .unwrap();

        assert!(!markerless.exists());
        assert!(uppercase.is_dir());
        assert!(unrelated.is_dir());
        assert!(canonical_file.is_file());
        assert!(fs::symlink_metadata(&canonical_link).is_ok());
        assert_eq!(
            fs::read(link_target.join("sentinel")).unwrap(),
            b"preserve link target"
        );
    }

    #[test]
    fn activated_target_residue_is_reclaimed_after_process_restart() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let target_cas = PayloadCas::new(target_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        let mut target_store = PersistentStore::open(target_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        seed_product_store(&mut target_store, "Target", 0);
        let source = PeerCloneCommandState::default();
        let source_peer_root = source_root.path().join("peer-sync");
        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_peer_root,
                &NeverCancelled,
            )
            .unwrap();
        source
            .configure_v2_registry(
                source_root.path(),
                "Windows",
                super::super::device_registry::DevicePermissions::read(),
            )
            .unwrap();
        let running = source
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());
        let request = PeerCloneTargetRequest {
            endpoint: format!(
                "http://127.0.0.1:{}",
                source.source_bind_address().unwrap().unwrap().port()
            ),
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
        };
        let target_peer_root = target_root.path().join("peer-sync");
        let target = PeerCloneCommandState::default();
        target
            .claim_v2_target(
                &target_peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        target
            .start_target_download(&target_peer_root, request.clone())
            .unwrap();
        wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation);
        target
            .finalize_target(&mut target_store, &target_cas, &target_peer_root, &request)
            .unwrap();
        let target_job_root = target_peer_root.join("targets").join(&request.session_id);
        assert!(target_job_root.is_dir());
        let targets_root = target_peer_root.join("targets");
        let mismatched = targets_root.join("523e4567-e89b-42d3-a456-426614174000");
        fs::create_dir_all(mismatched.join("transfer")).unwrap();
        fs::copy(
            target_job_root.join("transfer/manifest.json"),
            mismatched.join("transfer/manifest.json"),
        )
        .unwrap();
        let noncanonical = targets_root.join("823E4567-E89B-42D3-A456-426614174000");
        fs::create_dir_all(noncanonical.join("transfer")).unwrap();
        fs::copy(
            target_job_root.join("transfer/manifest.json"),
            noncanonical.join("transfer/manifest.json"),
        )
        .unwrap();
        let canonical_file = targets_root.join("623e4567-e89b-42d3-a456-426614174000");
        fs::write(&canonical_file, b"preserve target file").unwrap();
        let external = target_root.path().join("target-link-external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("sentinel"), b"preserve linked target").unwrap();
        let canonical_link = targets_root.join("723e4567-e89b-42d3-a456-426614174000");
        create_directory_link(&external, &canonical_link);
        drop(target);
        drop(target_store);

        let restarted = PeerCloneCommandState::initialize(&target_peer_root);

        assert!(!target_job_root.exists());
        assert!(mismatched.is_dir());
        assert!(noncanonical.is_dir());
        assert!(canonical_file.is_file());
        assert!(fs::symlink_metadata(&canonical_link).is_ok());
        assert_eq!(
            fs::read(external.join("sentinel")).unwrap(),
            b"preserve linked target"
        );
        assert_eq!(
            restarted.target_status_current().unwrap().phase,
            PeerCloneTargetPhase::Idle
        );
        source.stop_source(&request.session_id).unwrap();
    }

    #[test]
    fn product_source_runtime_reopens_prepared_package_after_process_loss() {
        let source_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        let peer_root = source_root.path().join("peer-sync");
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(&mut source_store, &source_cas, &peer_root, &NeverCancelled)
            .unwrap();
        let session_root = source.source_session_root_for_test().unwrap();
        let marker_path = peer_root.join("active-source.json");
        let marker: serde_json::Value =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        assert_eq!(
            marker,
            json!({
                "schema": "risunest.peer-clone-active-source/v1",
                "directoryId": session_root.file_name().unwrap().to_str().unwrap(),
                "sessionId": prepared.session_id.as_deref().unwrap(),
                "manifestId": prepared.manifest_id.as_deref().unwrap(),
            })
        );

        source
            .configure_v2_registry(
                source_root.path(),
                "Windows",
                super::super::device_registry::DevicePermissions::read(),
            )
            .unwrap();
        let first_running = source
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let first_pairing = parse_product_pairing(first_running.pairing_uri.as_deref().unwrap());
        let first_endpoint = format!(
            "http://127.0.0.1:{}",
            source.source_bind_address().unwrap().unwrap().port()
        );
        let target_app_root = tempfile::tempdir().unwrap();
        let credential_path = target_app_root.path().join("first-credential.json");
        LanCloneClient::claim_v2_and_persist_and_register(
            target_app_root.path(),
            "Registered target",
            &credential_path,
            &first_endpoint,
            &first_pairing.session_id,
            &first_pairing.manifest_id,
            &first_pairing.claim,
        )
        .unwrap();
        let first_credential: serde_json::Value =
            serde_json::from_slice(&fs::read(&credential_path).unwrap()).unwrap();

        source.shutdown_for_exit();

        drop(source);

        assert!(session_root.exists());
        assert!(marker_path.is_file());
        let reopened = PeerCloneCommandState::initialize(&peer_root);
        let recovered = reopened.source_status().unwrap();
        assert_eq!(recovered.phase, PeerCloneSourcePhase::Prepared);
        assert_eq!(recovered.session_id, prepared.session_id);
        assert_eq!(recovered.manifest_id, prepared.manifest_id);
        assert!(reopened.source_bind_address().unwrap().is_none());

        reopened
            .configure_v2_registry(
                source_root.path(),
                "Windows",
                super::super::device_registry::DevicePermissions::read(),
            )
            .unwrap();
        let second_running = reopened
            .start_source(
                recovered.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let second_pairing = parse_product_pairing(second_running.pairing_uri.as_deref().unwrap());
        let second_endpoint = format!(
            "http://127.0.0.1:{}",
            reopened.source_bind_address().unwrap().unwrap().port()
        );
        let second_credential_path = target_app_root.path().join("second-credential.json");
        LanCloneClient::claim_v2_and_persist_and_register(
            target_app_root.path(),
            "Registered target",
            &second_credential_path,
            &second_endpoint,
            &second_pairing.session_id,
            &second_pairing.manifest_id,
            &second_pairing.claim,
        )
        .unwrap();
        let second_credential: serde_json::Value =
            serde_json::from_slice(&fs::read(&second_credential_path).unwrap()).unwrap();
        assert_eq!(second_pairing.session_id, first_pairing.session_id);
        assert_eq!(second_pairing.manifest_id, first_pairing.manifest_id);
        assert_ne!(second_pairing.claim, first_pairing.claim);
        assert_ne!(second_credential["bearer"], first_credential["bearer"]);
        assert_eq!(second_credential["deviceId"], first_credential["deviceId"]);
        let stale = reqwest::blocking::Client::new()
            .get(format!(
                "{second_endpoint}/v1/sessions/{}/manifest",
                second_pairing.session_id
            ))
            .bearer_auth(first_credential["bearer"].as_str().unwrap())
            .send()
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::UNAUTHORIZED);

        reopened.stop_source(&second_pairing.session_id).unwrap();
        assert!(!session_root.exists());
        assert!(!marker_path.exists());
    }

    #[test]
    fn product_source_recovery_fails_closed_for_corrupt_marker_manifest_and_cas() {
        for corruption in [
            "marker-json",
            "manifest-identity",
            "noncanonical-manifest",
            "missing-object",
            "wrong-object-size",
        ] {
            let source_root = tempfile::tempdir().unwrap();
            let source_cas = PayloadCas::new(source_root.path()).unwrap();
            let mut source_store = PersistentStore::open(source_root.path()).unwrap();
            seed_product_store(&mut source_store, "Source", 0);
            let peer_root = source_root.path().join("peer-sync");
            let source = PeerCloneCommandState::default();
            source
                .prepare_source(&mut source_store, &source_cas, &peer_root, &NeverCancelled)
                .unwrap();
            let marker_path = peer_root.join("active-source.json");
            let session_root = source.source_session_root_for_test().unwrap();
            let manifest_path = session_root.join("manifest.json");
            let manifest: super::super::CloneManifest =
                serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
            let session_cas = PayloadCas::new(&session_root).unwrap();
            let object_path = session_cas
                .object_path(&manifest.database.object)
                .unwrap()
                .unwrap();

            match corruption {
                "marker-json" => fs::write(&marker_path, b"{").unwrap(),
                "manifest-identity" => {
                    let mut marker: serde_json::Value =
                        serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
                    marker["manifestId"] = serde_json::Value::String("0".repeat(64));
                    fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
                }
                "noncanonical-manifest" => {
                    let mut bytes = fs::read(&manifest_path).unwrap();
                    bytes.push(b'\n');
                    fs::write(&manifest_path, bytes).unwrap();
                }
                "missing-object" => fs::remove_file(object_path).unwrap(),
                "wrong-object-size" => File::options()
                    .write(true)
                    .open(object_path)
                    .unwrap()
                    .set_len(0)
                    .unwrap(),
                _ => unreachable!(),
            }

            let recovered = PeerCloneCommandState::initialize(&peer_root);
            assert_eq!(
                recovered.source_status().unwrap().phase,
                PeerCloneSourcePhase::Idle,
                "{corruption}"
            );
            assert!(!marker_path.exists(), "{corruption}");
            assert!(
                recovered.source_bind_address().unwrap().is_none(),
                "{corruption}"
            );
            assert!(session_root.exists(), "{corruption}");
        }

        let root = tempfile::tempdir().unwrap();
        let peer_root = root.path().join("peer-sync");
        let orphan = peer_root
            .join("source-sessions")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&orphan).unwrap();
        let idle = PeerCloneCommandState::initialize(&peer_root);
        assert_eq!(
            idle.source_status().unwrap().phase,
            PeerCloneSourcePhase::Idle
        );
        assert!(
            orphan.exists(),
            "markerless legacy sessions are not auto-adopted"
        );
    }

    #[cfg(windows)]
    #[test]
    fn product_source_prepare_retries_retained_recovery_after_io_failure() {
        use std::os::windows::fs::OpenOptionsExt;

        let root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(root.path()).unwrap();
        let mut source_store = PersistentStore::open(root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        let peer_root = root.path().join("peer-sync");
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(&mut source_store, &source_cas, &peer_root, &NeverCancelled)
            .unwrap();
        let marker_path = peer_root.join(ACTIVE_SOURCE_MARKER_FILE);
        let marker_lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&marker_path)
            .unwrap();

        let recovered = PeerCloneCommandState::initialize(&peer_root);
        assert!(matches!(
            recovered.source_status(),
            Err(PeerSyncError::Storage(_))
        ));
        assert!(matches!(
            recovered.prepare_source(&mut source_store, &source_cas, &peer_root, &NeverCancelled,),
            Err(PeerSyncError::Storage(_))
        ));
        assert!(marker_path.is_file());

        drop(marker_lock);

        let retried = recovered
            .prepare_source(&mut source_store, &source_cas, &peer_root, &NeverCancelled)
            .unwrap();
        assert_eq!(retried.phase, PeerCloneSourcePhase::Prepared);
        assert_eq!(retried.session_id, prepared.session_id);
        assert_eq!(retried.manifest_id, prepared.manifest_id);
    }

    struct TunnelSourceFixture {
        _root: tempfile::TempDir,
        source: PeerCloneCommandState,
        session_id: String,
        launcher_state: Arc<Mutex<FakeTunnelLauncherState>>,
    }

    impl TunnelSourceFixture {
        fn prepare(public_url: &str) -> Self {
            let root = tempfile::tempdir().unwrap();
            let cas = PayloadCas::new(root.path()).unwrap();
            let mut store = PersistentStore::open(root.path()).unwrap();
            seed_product_store(&mut store, "Source", 0);
            let launcher_state = Arc::new(Mutex::new(FakeTunnelLauncherState {
                public_url: url::Url::parse(public_url).unwrap(),
                lifecycle: SourceTunnelLifecycle::Running,
                ..FakeTunnelLauncherState::default()
            }));
            let source = PeerCloneCommandState::with_tunnel_launcher(Arc::new(FakeTunnelLauncher(
                Arc::clone(&launcher_state),
            )));
            let prepared = source
                .prepare_source(
                    &mut store,
                    &cas,
                    &root.path().join("peer-sync"),
                    &NeverCancelled,
                )
                .unwrap();
            Self {
                _root: root,
                source,
                session_id: prepared.session_id.unwrap(),
                launcher_state,
            }
        }
    }

    struct FakeTunnelLauncher(Arc<Mutex<FakeTunnelLauncherState>>);

    struct FakeTunnelLauncherState {
        public_url: url::Url,
        fail_start: bool,
        stop_failures: usize,
        lifecycle: SourceTunnelLifecycle,
        launch_pause: Option<Arc<Barrier>>,
        stop_pause: Option<Arc<Barrier>>,
        stop_calls: usize,
        seen_origin: Option<SocketAddr>,
        seen_token: Option<String>,
    }

    impl Default for FakeTunnelLauncherState {
        fn default() -> Self {
            Self {
                public_url: url::Url::parse("https://unused.example.com/").unwrap(),
                fail_start: false,
                stop_failures: 0,
                lifecycle: SourceTunnelLifecycle::Running,
                launch_pause: None,
                stop_pause: None,
                stop_calls: 0,
                seen_origin: None,
                seen_token: None,
            }
        }
    }

    impl SourceTunnelLauncher for FakeTunnelLauncher {
        fn start(
            &self,
            tunnel: PeerCloneTunnelStart,
            host: LanCloneHost,
        ) -> Result<Box<dyn SourceTunnel>, Box<dyn FailedSourceTunnel>> {
            let pause = {
                let mut state = self.0.lock().unwrap();
                state.seen_origin = host.address();
                if let PeerCloneTunnelStart::Named { token, .. } = tunnel {
                    state.seen_token = Some(token);
                }
                state.launch_pause.clone()
            };
            if let Some(pause) = pause {
                pause.wait();
                pause.wait();
            }
            if self.0.lock().unwrap().fail_start {
                return Err(Box::new(FakeFailedSourceTunnel { host: Some(host) }));
            }
            Ok(Box::new(FakeSourceTunnel {
                host: Some(host),
                state: Arc::clone(&self.0),
            }))
        }
    }

    struct FakeFailedSourceTunnel {
        host: Option<LanCloneHost>,
    }

    impl FailedSourceTunnel for FakeFailedSourceTunnel {
        fn recover_host(&mut self) -> Result<Option<LanCloneHost>, String> {
            Ok(self.host.take())
        }

        fn stop(&mut self) -> Result<(), String> {
            self.host
                .as_mut()
                .map(LanCloneHost::stop)
                .transpose()
                .map_err(|_| "reflected tunnel-check/probe-secret".to_owned())?;
            self.host = None;
            Ok(())
        }
    }

    struct FakeSourceTunnel {
        host: Option<LanCloneHost>,
        state: Arc<Mutex<FakeTunnelLauncherState>>,
    }

    impl SourceTunnel for FakeSourceTunnel {
        fn transport_url(&self) -> url::Url {
            self.state.lock().unwrap().public_url.clone()
        }

        fn lifecycle(&mut self) -> Result<SourceTunnelLifecycle, String> {
            let lifecycle = self.state.lock().unwrap().lifecycle;
            if lifecycle == SourceTunnelLifecycle::Stopped {
                if let Some(host) = self.host.as_mut() {
                    host.stop().map_err(|_| "reflected public URL".to_owned())?;
                }
                self.host = None;
            }
            Ok(lifecycle)
        }

        fn stop(&mut self) -> Result<(), String> {
            let pause = {
                let mut state = self.state.lock().unwrap();
                state.stop_calls += 1;
                if state.stop_failures != 0 {
                    state.stop_failures -= 1;
                    return Err(
                        "https://quick-id.trycloudflare.com/tunnel-check/probe-secret".to_owned(),
                    );
                }
                state.stop_pause.clone()
            };
            if let Some(pause) = pause {
                pause.wait();
                pause.wait();
            }
            if let Some(host) = self.host.as_mut() {
                host.stop().map_err(|_| "reflected public URL".to_owned())?;
            }
            self.host = None;
            self.state.lock().unwrap().lifecycle = SourceTunnelLifecycle::Stopped;
            Ok(())
        }
    }

    fn wait_for_target_phase(
        state: &PeerCloneCommandState,
        expected: PeerCloneTargetPhase,
    ) -> PeerCloneTargetStatus {
        for _ in 0..2_000 {
            let status = state.target_status_current().unwrap();
            if status.phase == expected {
                return status;
            }
            if status.phase == PeerCloneTargetPhase::Failed {
                panic!("target failed before {expected:?}: {:?}", status.error);
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("target did not reach {expected:?}");
    }

    struct ProductPairing {
        advertised_endpoint: String,
        session_id: String,
        manifest_id: String,
        claim: String,
    }

    fn parse_product_pairing(value: &str) -> ProductPairing {
        let url = url::Url::parse(value).unwrap();
        ProductPairing {
            advertised_endpoint: url
                .query_pairs()
                .find(|(key, _)| key == "endpoint")
                .unwrap()
                .1
                .into_owned(),
            session_id: url
                .query_pairs()
                .find(|(key, _)| key == "session")
                .unwrap()
                .1
                .into_owned(),
            manifest_id: url
                .query_pairs()
                .find(|(key, _)| key == "manifest")
                .unwrap()
                .1
                .into_owned(),
            claim: url
                .fragment()
                .unwrap()
                .strip_prefix("claim=")
                .unwrap()
                .to_owned(),
        }
    }

    fn seed_product_store(store: &mut PersistentStore, username: &str, filler_bytes: usize) {
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(
                &staging,
                &json!({
                    "username": username,
                    "botPresetsId": 0,
                    "personas": [{ "id": "persona" }],
                    "selectedPersona": 0,
                    "enabledModules": [],
                    "characterOrder": [],
                    "modules": [],
                    "loadouts": [],
                    "plugins": [],
                    "pluginCustomStorage": {},
                    "productCloneFiller": product_filler(filler_bytes),
                }),
            )
            .unwrap();
        store
            .replace_put_presets(&staging, &[json!({ "name": "preset" })])
            .unwrap();
        store
            .replace_put_asset_repository_authority(
                &staging,
                &crate::persistent_store::AssetRepositoryAuthorityState::V2 {
                    migration_id: "peer-clone-test-assets".to_owned(),
                    compatibility_hash: "ab".repeat(32),
                },
            )
            .unwrap();
        store
            .replace_put_cold_payload_authority(
                &staging,
                &crate::persistent_store::ColdPayloadAuthorityState::V2 {
                    migration_id: "peer-clone-test-cold".to_owned(),
                    compatibility_hash: "cd".repeat(32),
                },
            )
            .unwrap();
        store.replace_commit(&staging, Some(0)).unwrap();
    }

    fn product_filler(bytes: usize) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let mut state = 0x9e37_79b9_u32;
        let mut filler = String::with_capacity(bytes);
        for _ in 0..bytes {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            filler.push(ALPHABET[(state as usize) % ALPHABET.len()] as char);
        }
        filler
    }

    fn first_persisted_chunk_event(ledger: &Path) -> (String, u64) {
        fs::read_to_string(ledger)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find_map(|event| {
                let next_chunk = event.get("next_chunk")?.as_u64()?;
                (!event.get("verified")?.as_bool()? && next_chunk > 0).then(|| {
                    (
                        event.get("object").unwrap().as_str().unwrap().to_owned(),
                        next_chunk,
                    )
                })
            })
            .expect("cancelled clone must retain a nonzero verified chunk")
    }

    fn persisted_chunk_event_count(ledger: &Path, expected: &(String, u64)) -> usize {
        fs::read_to_string(ledger)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|event| {
                event.get("object").and_then(serde_json::Value::as_str) == Some(expected.0.as_str())
                    && event.get("next_chunk").and_then(serde_json::Value::as_u64)
                        == Some(expected.1)
                    && event.get("verified").and_then(serde_json::Value::as_bool) == Some(false)
            })
            .count()
    }
}
