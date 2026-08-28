use super::{
    activate_downloaded_clone, prepare_lossless_clone_session, CloneTargetAdapter, LanCloneClient,
    LanCloneHost, LoopbackCloneClient, LosslessCloneTargetAdapter, PeerSyncError,
    TransferCancellation,
};
use super::{
    lan::{validate_lan_endpoint, LanCloneHostControl, NAMED_TUNNEL_ORIGIN_UNAVAILABLE},
    tunnel::{self, RunningTunnelLifecycle, SystemTunnelProcess, TunnelStartFailure},
};
use crate::{
    asset_repository::PayloadCas,
    local_backup::NeverCancelled,
    persistent_store::{self, PersistentStore, StoreError},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
    time::Duration,
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

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PeerCloneTunnelStart {
    Quick,
    Named {
        token: String,
        #[serde(rename = "expectedPublicBaseUrl")]
        expected_public_base_url: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerCloneTunnelKind {
    Quick,
    Named,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneTunnelMetadata {
    kind: PeerCloneTunnelKind,
    experimental: bool,
    one_shot: bool,
}

impl PeerCloneTunnelMetadata {
    fn quick() -> Self {
        Self {
            kind: PeerCloneTunnelKind::Quick,
            experimental: true,
            one_shot: true,
        }
    }

    fn named() -> Self {
        Self {
            kind: PeerCloneTunnelKind::Named,
            experimental: false,
            one_shot: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerCloneTunnelPhase {
    Idle,
    Starting,
    Running,
    Stopping,
    Stopped,
}

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceTunnelLifecycle {
    Running,
    CleanupPending,
    Stopped,
}

trait SourceTunnel: Send {
    fn transport_url(&self) -> url::Url;
    fn lifecycle(&mut self) -> Result<SourceTunnelLifecycle, String>;
    fn stop(&mut self) -> Result<(), String>;
}

trait FailedSourceTunnel: Send {
    fn recover_host(&mut self) -> Result<Option<LanCloneHost>, String>;
    fn stop(&mut self) -> Result<(), String>;
}

trait SourceTunnelLauncher: Send + Sync {
    fn start(
        &self,
        tunnel: PeerCloneTunnelStart,
        host: LanCloneHost,
    ) -> Result<Box<dyn SourceTunnel>, Box<dyn FailedSourceTunnel>>;
}

struct SystemSourceTunnel(tunnel::RunningTunnel);

impl SourceTunnel for SystemSourceTunnel {
    fn transport_url(&self) -> url::Url {
        self.0.transport_url().clone()
    }

    fn lifecycle(&mut self) -> Result<SourceTunnelLifecycle, String> {
        self.0
            .poll_lifecycle()
            .map(|lifecycle| match lifecycle {
                RunningTunnelLifecycle::Running => SourceTunnelLifecycle::Running,
                RunningTunnelLifecycle::CleanupPending => SourceTunnelLifecycle::CleanupPending,
                RunningTunnelLifecycle::Stopped => SourceTunnelLifecycle::Stopped,
            })
            .map_err(|_| "peer clone tunnel status is unavailable".to_owned())
    }

    fn stop(&mut self) -> Result<(), String> {
        self.0
            .stop(Duration::from_secs(2))
            .map_err(|_| "peer clone tunnel failed to stop".to_owned())
    }
}

type SystemTunnelStartFailure = TunnelStartFailure<SystemTunnelProcess, LanCloneHost>;

struct SystemFailedSourceTunnel {
    failure: Option<SystemTunnelStartFailure>,
}

impl FailedSourceTunnel for SystemFailedSourceTunnel {
    fn recover_host(&mut self) -> Result<Option<LanCloneHost>, String> {
        let Some(failure) = self.failure.take() else {
            return Ok(None);
        };
        match failure.retry_into_peer_session() {
            Ok(host) => Ok(Some(host)),
            Err(failure) => {
                self.failure = Some(failure);
                Err("peer clone tunnel cleanup is pending".to_owned())
            }
        }
    }

    fn stop(&mut self) -> Result<(), String> {
        let Some(failure) = self.failure.as_mut() else {
            return Ok(());
        };
        failure
            .retry_cleanup()
            .map_err(|_| "peer clone tunnel failed to stop".to_owned())?;
        self.failure = None;
        Ok(())
    }
}

struct SystemSourceTunnelLauncher;

impl SourceTunnelLauncher for SystemSourceTunnelLauncher {
    fn start(
        &self,
        request: PeerCloneTunnelStart,
        host: LanCloneHost,
    ) -> Result<Box<dyn SourceTunnel>, Box<dyn FailedSourceTunnel>> {
        let started = match request {
            PeerCloneTunnelStart::Quick => tunnel::start_quick_desktop_tunnel(host),
            PeerCloneTunnelStart::Named {
                token,
                expected_public_base_url,
            } => tunnel::start_named_desktop_tunnel(host, token, &expected_public_base_url),
        };
        started
            .map(|running| Box::new(SystemSourceTunnel(running)) as Box<dyn SourceTunnel>)
            .map_err(|failure| {
                Box::new(SystemFailedSourceTunnel {
                    failure: Some(failure),
                }) as Box<dyn FailedSourceTunnel>
            })
    }
}

struct SourceRuntime {
    session_id: String,
    manifest_id: String,
    session_root: PathBuf,
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
        let _ = remove_directory_if_exists(&self.session_root);
    }
}

struct TargetRuntime {
    request: PeerCloneTargetRequest,
    job_root: PathBuf,
    client: Option<LoopbackCloneClient>,
    cancellation: Option<TransferCancellation>,
    worker: Option<JoinHandle<()>>,
    status: PeerCloneTargetStatus,
    #[cfg(test)]
    fail_finalize_cleanup_once: bool,
    #[cfg(test)]
    fail_release_cleanup_once: bool,
}

#[derive(Default)]
struct PeerCloneRuntime {
    source_preparing: bool,
    source: Option<SourceRuntime>,
    target_claiming: bool,
    target: Option<TargetRuntime>,
}

#[derive(Clone)]
pub struct PeerCloneCommandState {
    runtime: Arc<Mutex<PeerCloneRuntime>>,
    tunnel_launcher: Arc<dyn SourceTunnelLauncher>,
}

impl Default for PeerCloneCommandState {
    fn default() -> Self {
        Self {
            runtime: Arc::new(Mutex::new(PeerCloneRuntime::default())),
            tunnel_launcher: Arc::new(SystemSourceTunnelLauncher),
        }
    }
}

impl PeerCloneCommandState {
    #[cfg(test)]
    fn with_tunnel_launcher(tunnel_launcher: Arc<dyn SourceTunnelLauncher>) -> Self {
        Self {
            runtime: Arc::new(Mutex::new(PeerCloneRuntime::default())),
            tunnel_launcher,
        }
    }

    fn lock_runtime(&self) -> Result<MutexGuard<'_, PeerCloneRuntime>, PeerSyncError> {
        self.runtime.lock().map_err(|error| {
            PeerSyncError::Storage(format!("peer clone command state mutex poisoned: {error}"))
        })
    }

    pub fn prepare_source(
        &self,
        store: &mut PersistentStore,
        cas: &PayloadCas,
        peer_root: &Path,
        cancellation: &dyn crate::local_backup::CancellationProbe,
    ) -> Result<PeerCloneSourceStatus, PeerSyncError> {
        {
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
        }

        let operation_id = uuid::Uuid::new_v4().to_string();
        let preparation_parent = peer_root.join("source-preparation");
        let sessions_parent = peer_root.join("source-sessions");
        let preparation_root = preparation_parent.join(&operation_id);
        let session_root = sessions_parent.join(&operation_id);
        let prepared = (|| {
            fs::create_dir_all(&preparation_parent)?;
            fs::create_dir_all(&sessions_parent)?;
            let expected_revision = store.revision().map_err(store_error)?;
            prepare_lossless_clone_session(
                store,
                cas,
                expected_revision,
                &preparation_root,
                &session_root,
                cancellation,
            )
        })();

        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let cleanup = remove_directory_if_exists(&session_root);
                let mut runtime = self.lock_runtime()?;
                runtime.source_preparing = false;
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(PeerSyncError::Storage(format!(
                        "{error}; peer clone source session cleanup failed: {cleanup}"
                    ))),
                };
            }
        };
        let session_id = prepared.manifest().session_id.clone();
        let manifest_id = prepared.manifest_id().to_owned();
        let host = LanCloneHost::prepare(prepared);
        let control = host.control();
        let mut runtime = self.lock_runtime()?;
        runtime.source_preparing = false;
        runtime.source = Some(SourceRuntime {
            session_id,
            manifest_id,
            session_root,
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
        source_status(&mut runtime)
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
                    source.phase = match remove_directory_if_exists(&source.session_root) {
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
        let (mut host, mut tunnel, mut failed_tunnel, session_root, stop_pause, fail_cleanup) = {
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
            remove_directory_if_exists(&session_root)
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

    pub fn claim_target(
        &self,
        peer_root: &Path,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
    ) -> Result<(), PeerSyncError> {
        let request = PeerCloneTargetRequest {
            endpoint: endpoint.to_owned(),
            session_id: session_id.to_owned(),
            manifest_id: manifest_id.to_owned(),
        };
        let paths = target_paths(peer_root, &request)?;
        {
            let mut runtime = self.lock_runtime()?;
            if let Some(target) = &runtime.target {
                return if target.request == request && target.job_root == paths.job_root {
                    Ok(())
                } else {
                    Err(PeerSyncError::Protocol(
                        "another peer clone target job is already owned".to_owned(),
                    ))
                };
            }
            if runtime.target_claiming {
                return Err(PeerSyncError::Protocol(
                    "peer clone target claim is already in progress".to_owned(),
                ));
            }
            runtime.target_claiming = true;
        }

        let claimed = (|| {
            let lan = match fs::symlink_metadata(&paths.credential) {
                Ok(_) => LanCloneClient::open_persisted(&paths.credential)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    LanCloneClient::claim_and_persist(
                        &paths.credential,
                        endpoint,
                        session_id,
                        manifest_id,
                        claim,
                    )?
                }
                Err(error) => return Err(error.into()),
            };
            if !lan.matches_target(endpoint, session_id, manifest_id)? {
                return Err(PeerSyncError::Protocol(
                    "persisted peer clone target identity does not match the request".to_owned(),
                ));
            }
            LoopbackCloneClient::from_lan(&paths.transfer, lan, manifest_id)
        })();

        let mut runtime = self.lock_runtime()?;
        runtime.target_claiming = false;
        let client = claimed?;
        runtime.target = Some(TargetRuntime {
            request,
            job_root: paths.job_root,
            client: Some(client),
            cancellation: None,
            worker: None,
            status: PeerCloneTargetStatus::idle(),
            #[cfg(test)]
            fail_finalize_cleanup_once: false,
            #[cfg(test)]
            fail_release_cleanup_once: false,
        });
        Ok(())
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
                LoopbackCloneClient::from_lan(&paths.transfer, lan, &request.manifest_id)
            })();
            let mut runtime = self.lock_runtime()?;
            runtime.target_claiming = false;
            let client = restored?;
            if runtime.target.is_some() {
                return Err(PeerSyncError::Protocol(
                    "another peer clone target job acquired ownership".to_owned(),
                ));
            }
            runtime.target = Some(TargetRuntime {
                request: request.clone(),
                job_root: paths.job_root,
                client: Some(client),
                cancellation: None,
                worker: None,
                status: PeerCloneTargetStatus::idle(),
                #[cfg(test)]
                fail_finalize_cleanup_once: false,
                #[cfg(test)]
                fail_release_cleanup_once: false,
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
        let (worker, fail_cleanup_after_commit) = {
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
                #[cfg(test)]
                std::mem::take(&mut target.fail_finalize_cleanup_once),
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
            let mut target = LosslessCloneTargetAdapter::new(
                store,
                cas,
                &activation_root,
                expected_revision,
                &NeverCancelled,
            )?;
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
                    Ok(Some(active)) if active == request.manifest_id => {
                        Ok(Some(bounded_finalize_warning(&error)))
                    }
                    Ok(_) => Err(error),
                    Err(reconcile) => Err(PeerSyncError::Storage(format!(
                        "{error}; failed to reconcile peer clone activation: {reconcile}"
                    ))),
                },
            };
            drop(target);
            match outcome {
                Ok(warning) => Ok(PeerCloneFinalizeResult {
                    revision: store.revision().map_err(store_error)?,
                    warning,
                }),
                Err(error) => Err(error),
            }
        })();
        let mut runtime = self.lock_runtime()?;
        let target = require_target_mut(&mut runtime, request, &paths.job_root)?;
        target.client = Some(client);
        target.cancellation = None;
        match &result {
            Ok(_) => {
                target.status.phase = PeerCloneTargetPhase::Completed;
                target.status.error = None;
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
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn build_pairing_uri(endpoint: &str, pairing: &super::LanPairing) -> Result<String, PeerSyncError> {
    let mut uri = url::Url::parse("risuailocal://peer-clone/v1")
        .map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
    uri.query_pairs_mut()
        .append_pair("endpoint", endpoint)
        .append_pair("session", &pairing.session_id)
        .append_pair("manifest", &pairing.manifest_id);
    uri.set_fragment(Some(&format!("claim={}", pairing.claim)));
    Ok(uri.to_string())
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

fn discover_lan_ipv4() -> Result<Ipv4Addr, PeerSyncError> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
    match socket.local_addr()?.ip() {
        std::net::IpAddr::V4(address) if address.is_private() || address.is_link_local() => {
            Ok(address)
        }
        _ => Err(PeerSyncError::Validation(
            "no private IPv4 LAN address is available".to_owned(),
        )),
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

fn app_peer_root(app: &AppHandle) -> Result<(PathBuf, PathBuf), String> {
    let app_root = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("failed to resolve application data directory: {error}"))?;
    Ok((app_root.clone(), app_root.join("peer-clone")))
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
        .map_err(|error| error.to_string())
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
pub async fn peer_clone_claim_client(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    claim: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    let (_, peer_root) = app_peer_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        state.claim_target(&peer_root, &endpoint, &session_id, &manifest_id, &claim)
    })
    .await
    .map_err(|error| format!("peer clone target claim worker failed: {error}"))?
    .map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub fn peer_clone_download(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<(), String> {
    let (_, peer_root) = app_peer_root(&app)?;
    state
        .start_target_download(
            &peer_root,
            target_request(endpoint, session_id, manifest_id),
        )
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn peer_clone_resume(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<(), String> {
    let (_, peer_root) = app_peer_root(&app)?;
    let state = state.inner().clone();
    let request = target_request(endpoint, session_id, manifest_id);
    tauri::async_runtime::spawn_blocking(move || state.resume_target_download(&peer_root, request))
        .await
        .map_err(|error| format!("peer clone target resume worker failed: {error}"))?
        .map_err(|error| error.to_string())
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
    tauri::async_runtime::spawn_blocking(move || state.cancel_target(&request))
        .await
        .map_err(|error| format!("peer clone target cancellation worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub fn peer_clone_target_status(
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<PeerCloneTargetStatus, String> {
    state
        .target_status(&target_request(endpoint, session_id, manifest_id))
        .map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub fn peer_clone_release_target(
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<(), String> {
    state
        .release_target(&target_request(endpoint, session_id, manifest_id))
        .map_err(|error| error.to_string())
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
    let (app_root, peer_root) = app_peer_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let cas = PayloadCas::new(&app_root).map_err(|error| error.to_string())?;
        persistent_store::commands::with_store_mut(app.state(), |store| {
            state
                .finalize_target(store, &cas, &peer_root, &request)
                .map_err(as_store_error)
        })
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("peer clone target finalization worker failed: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::Cursor,
        net::Ipv4Addr,
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };

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
        let _port = super::super::lan::NAMED_TUNNEL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let _port = super::super::lan::NAMED_TUNNEL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fixture = TunnelSourceFixture::prepare("https://sync.example.com/");
        let occupied = std::net::TcpListener::bind((
            Ipv4Addr::LOCALHOST,
            super::super::lan::NAMED_TUNNEL_ORIGIN_PORT,
        ))
        .unwrap();

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

        fixture.source.stop_source(&fixture.session_id).unwrap();
        assert_eq!(
            fixture.source.source_status().unwrap().phase,
            PeerCloneSourcePhase::Stopped
        );
        assert!(!session_root.exists());
    }

    #[test]
    fn product_tunnel_natural_exit_reaps_the_source_session() {
        let fixture = TunnelSourceFixture::prepare("https://quick-id.trycloudflare.com/");
        fixture
            .source
            .start_tunnel(&fixture.session_id, PeerCloneTunnelStart::Quick)
            .unwrap();
        let session_root = fixture.source.source_session_root_for_test().unwrap();
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
        target
            .claim_target(
                &target_root.path().join("peer-sync"),
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        PeerCloneCommandState::default()
            .claim_target(
                &target_root.path().join("peer-sync"),
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        assert!(super::super::LanCloneClient::claim(
            &request.endpoint,
            &request.session_id,
            &pairing.claim,
        )
        .is_err());

        target
            .start_target_download(&target_root.path().join("peer-sync"), request.clone())
            .unwrap();
        let downloaded = wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation);
        assert_eq!(downloaded.completed_bytes, downloaded.total_bytes.unwrap());
        assert!(target.release_target(&request).is_err());
        let device_id = source.source_status().unwrap().devices[0].device_id.clone();
        source
            .revoke_source_device(&request.session_id, &device_id)
            .unwrap();
        assert!(source.source_status().unwrap().devices[0].revoked);
        assert_eq!(target_store.revision().unwrap(), 1);
        assert_eq!(
            target_store.read_root(None).unwrap().value["username"],
            "Target"
        );
        drop(target);
        source.stop_source(pairing.session_id.as_str()).unwrap();
        let target = PeerCloneCommandState::default();
        target
            .resume_target_download(&target_root.path().join("peer-sync"), request.clone())
            .unwrap();
        wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation);
        target.fail_target_finalize_cleanup_once_for_test().unwrap();

        let finalized = target
            .finalize_target(
                &mut target_store,
                &target_cas,
                &target_root.path().join("peer-sync"),
                &request,
            )
            .unwrap();

        assert_eq!(finalized.revision, 2);
        assert!(finalized.warning.is_some());
        assert_eq!(
            target.target_status(&request).unwrap().phase,
            PeerCloneTargetPhase::Completed
        );
        assert_eq!(
            target_store.read_root(None).unwrap().value["username"],
            "Source"
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
        target.fail_target_release_cleanup_once_for_test().unwrap();
        assert_eq!(
            target.release_target(&request).unwrap_err(),
            PeerSyncError::Storage("injected peer clone target release cleanup failure".to_owned())
        );
        assert_eq!(
            target.target_status(&request).unwrap().phase,
            PeerCloneTargetPhase::Completed
        );
        assert!(target_job_root.exists());
        assert!(target_cas
            .open_object(&authoritative.content_hash)
            .unwrap()
            .is_some());
        assert_eq!(target_store.revision().unwrap(), finalized.revision);
        target.release_target(&request).unwrap();
        assert!(!target_job_root.exists());
        assert!(target_cas
            .open_object(&authoritative.content_hash)
            .unwrap()
            .is_some());
        assert_eq!(target_store.revision().unwrap(), finalized.revision);
        target.release_target(&request).unwrap();
        assert!(target.target_status(&request).is_err());
    }

    #[test]
    fn product_post_commit_ledger_error_returns_committed_revision() {
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
            .claim_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        target
            .start_target_download(&peer_root, request.clone())
            .unwrap();
        wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation);
        target
            .fail_target_activation_ledger_once_for_test()
            .unwrap();

        let finalized = target
            .finalize_target(&mut target_store, &target_cas, &peer_root, &request)
            .unwrap();

        assert_eq!(finalized.revision, 2);
        assert!(finalized.warning.is_some());
        assert_eq!(
            target.target_status(&request).unwrap().phase,
            PeerCloneTargetPhase::Completed
        );
        assert_eq!(
            target_store.read_root(None).unwrap().value["username"],
            "Source"
        );
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
            .claim_target(
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
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_root.path().join("peer-sync"),
                &NeverCancelled,
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
            .claim_target(
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
        drop(target);

        let resumed = PeerCloneCommandState::default();
        resumed
            .resume_target_download(&peer_root, request.clone())
            .unwrap();
        wait_for_target_phase(&resumed, PeerCloneTargetPhase::AwaitingActivation);
        assert_eq!(
            persisted_chunk_event_count(&ledger, &persisted_chunk),
            1,
            "resume must not redownload an already verified chunk"
        );

        assert_eq!(source.source_status().unwrap().devices.len(), 1);
        source.stop_source(&request.session_id).unwrap();
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
    fn product_owned_source_runtime_cleans_up_on_drop() {
        let source_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        let source = PeerCloneCommandState::default();
        source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_root.path().join("peer-sync"),
                &NeverCancelled,
            )
            .unwrap();
        let session_root = source.source_session_root_for_test().unwrap();

        drop(source);

        assert!(!session_root.exists());
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
            {
                let mut state = self.state.lock().unwrap();
                if state.stop_failures != 0 {
                    state.stop_failures -= 1;
                    return Err(
                        "https://quick-id.trycloudflare.com/tunnel-check/probe-secret".to_owned(),
                    );
                }
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
