#[cfg(test)]
use super::android_foreground::test_registry_guard;
#[cfg(any(target_os = "android", test))]
use super::android_foreground::{registry, AndroidForegroundKey, AndroidForegroundLane};
#[cfg(desktop)]
use super::lan::discover_lan_ipv4;
#[cfg(desktop)]
use super::lan::{validate_lan_endpoint, NAMED_TUNNEL_ORIGIN_UNAVAILABLE};
use super::logical_delta_transfer::execute_logical_delta_pull_with_pre_activation;
#[cfg(test)]
use super::target_foreground_transition::AndroidTargetForegroundTransitionPhase as AndroidTargetForegroundPhase;
#[cfg(any(target_os = "android", test))]
use super::target_foreground_transition::{
    AndroidTargetForegroundTransition, AndroidTargetForegroundTransitionError,
};
use super::{
    lan::{LanCloneHostControl, LanLogicalDeltaClient, PreparedLogicalLanSession},
    logical_delta::decode_logical_manifest,
    LanCloneHost, LogicalDeltaActivation, LogicalDeltaObject, LogicalDeltaObjectSource,
    PeerSyncError, ReadyLogicalDeltaPlan,
};
use crate::{
    asset_repository::{
        job_pins::{
            reclaim_abandoned_durable_cas_jobs, CasJobKind, CasReleaseOutcome, DurableCasJob,
        },
        PayloadCas,
    },
    local_backup::{CancellationProbe, NeverCancelled},
    persistent_store::{
        self, establish_logical_common_base, logical_delta_source::LogicalDeltaSourceSession,
        PersistentLogicalDeltaTarget, PersistentStore, StoreError, PRODUCT_LOGICAL_LIBRARY_ID,
    },
};
use serde::Serialize;
#[cfg(test)]
use std::fs;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    io::{self, Read},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager, State};

#[cfg(test)]
const SOURCE_DEVICE_ID_FILE: &str = "source-device-id";
const P4_DELTA_TARGET_JOB_PREFIX: &str = "p4-delta-target-";
const P4_SOURCE_PIN_PREFIX: &str = "logical-session-p4-source-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerDeltaCapabilities {
    desktop: bool,
    source_ready: bool,
    atomic_activation_ready: bool,
    authenticated_transport_ready: bool,
    production_enabled: bool,
    tunnel_ready: bool,
}

#[tauri::command]
pub fn peer_delta_capabilities() -> PeerDeltaCapabilities {
    PeerDeltaCapabilities {
        desktop: cfg!(desktop),
        source_ready: true,
        atomic_activation_ready: true,
        authenticated_transport_ready: true,
        production_enabled: true,
        tunnel_ready: cfg!(desktop),
    }
}

#[cfg(target_os = "android")]
use super::android_foreground::acquire_foreground_lane as acquire_foreground;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum PeerDeltaSourcePhase {
    Idle,
    Prepared,
    Starting,
    Running,
    Stopping,
    Stopped,
}

#[cfg(desktop)]
pub use super::tunnel_lifecycle::PeerTunnelStart as PeerDeltaTunnelStart;
#[cfg(desktop)]
use super::tunnel_lifecycle::{
    PeerTunnelKind as PeerDeltaTunnelKind, PeerTunnelMetadata as PeerDeltaTunnelMetadata,
    PeerTunnelPhase as PeerDeltaTunnelPhase,
};

#[cfg(desktop)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerDeltaTunnelStatus {
    session_id: Option<String>,
    phase: PeerDeltaTunnelPhase,
    tunnel: Option<PeerDeltaTunnelMetadata>,
}

#[cfg(desktop)]
use super::tunnel_lifecycle::{
    FailedPeerTunnel as FailedDeltaTunnel, PeerTunnel as DeltaTunnel,
    PeerTunnelLauncher as DeltaTunnelLauncher, PeerTunnelLifecycle as DeltaTunnelLifecycle,
    SystemPeerTunnelLauncher,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PeerDeltaSourceDevice {
    device_id: String,
    transferred_bytes: u64,
    current_object: Option<String>,
    last_seen_at: u64,
    revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerDeltaSourceStatus {
    session_id: Option<String>,
    manifest_id: Option<String>,
    pairing_uri: Option<String>,
    phase: PeerDeltaSourcePhase,
    devices: Vec<PeerDeltaSourceDevice>,
    #[cfg(desktop)]
    tunnel: Option<PeerDeltaTunnelMetadata>,
}

impl PeerDeltaSourceStatus {
    fn idle(phase: PeerDeltaSourcePhase) -> Self {
        Self {
            session_id: None,
            manifest_id: None,
            pairing_uri: None,
            phase,
            devices: Vec::new(),
            #[cfg(desktop)]
            tunnel: None,
        }
    }
}

struct DeltaSourceRuntime {
    session_id: String,
    manifest_id: String,
    host: Option<LanCloneHost>,
    control: LanCloneHostControl,
    phase: PeerDeltaSourcePhase,
    #[cfg(desktop)]
    tunnel: Option<Box<dyn DeltaTunnel>>,
    #[cfg(desktop)]
    failed_tunnel: Option<Box<dyn FailedDeltaTunnel>>,
    #[cfg(desktop)]
    tunnel_metadata: Option<PeerDeltaTunnelMetadata>,
    pairing_uri: Option<String>,
    // Read by the desktop stop/status interlock.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    stop_in_progress: bool,
    #[cfg(any(target_os = "android", test))]
    foreground: Option<AndroidForegroundKey>,
}

#[derive(Default)]
struct PeerDeltaRuntime {
    source_preparing: bool,
    source: Option<DeltaSourceRuntime>,
    stopped: bool,
    pull_in_progress: bool,
    #[cfg(any(target_os = "android", test))]
    target_foreground: Option<AndroidTargetForegroundStatus>,
}

/// Hostless P4 source preparation.  Keeping the logical session inside the
/// prepared transport makes its generation pin live exactly until shared
/// source cleanup drops the transport.
pub(crate) struct PreparedSharedDeltaSource {
    session: Option<PreparedLogicalLanSession>,
    session_id: String,
    manifest_id: String,
}

impl PreparedSharedDeltaSource {
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn manifest_id(&self) -> &str {
        &self.manifest_id
    }

    pub(crate) fn take_session(&mut self) -> Result<PreparedLogicalLanSession, PeerSyncError> {
        self.session.take().ok_or_else(|| {
            PeerSyncError::Protocol("shared delta source session is unavailable".to_owned())
        })
    }

    pub(crate) fn cleanup(&mut self) {
        self.session.take();
    }
}

pub(crate) fn prepare_shared_delta_source(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
) -> Result<PreparedSharedDeltaSource, PeerSyncError> {
    store
        .reclaim_logical_generation_pins(P4_SOURCE_PIN_PREFIX)
        .map_err(store_error)?;
    let built = store
        .seal_or_initialize_active_logical_generation(cas)
        .map_err(store_error)?;
    let session_store = store.open_native_job_store().map_err(store_error)?;
    let session = LogicalDeltaSourceSession::open_owned(
        session_store,
        app_root,
        &built.manifest.library_id,
        &built.manifest.generation,
        P4_SOURCE_PIN_PREFIX,
    )?;
    let source_device_id = canonical_source_device_id(app_root)?;
    let transport_session_id = uuid::Uuid::new_v4().to_string();
    let manifest_id = session.manifest_hash().to_owned();
    let objects = session.objects().to_vec();
    let prepared = PreparedLogicalLanSession::new(
        &transport_session_id,
        &source_device_id,
        manifest_id.clone(),
        built.manifest_bytes,
        objects,
        Box::new(session),
    )?;
    Ok(PreparedSharedDeltaSource {
        session: Some(prepared),
        session_id: transport_session_id,
        manifest_id,
    })
}

#[cfg(any(target_os = "android", test))]
type AndroidTargetForegroundStatus = AndroidTargetForegroundTransition<PeerDeltaPullResult>;

#[derive(Clone)]
pub struct PeerDeltaCommandState {
    runtime: Arc<Mutex<PeerDeltaRuntime>>,
    lifecycle_operation: Arc<Mutex<()>>,
    #[cfg(desktop)]
    tunnel_launcher: Arc<dyn DeltaTunnelLauncher>,
}

impl Default for PeerDeltaCommandState {
    fn default() -> Self {
        Self {
            runtime: Arc::new(Mutex::new(PeerDeltaRuntime::default())),
            lifecycle_operation: Arc::new(Mutex::new(())),
            #[cfg(desktop)]
            tunnel_launcher: Arc::new(SystemPeerTunnelLauncher { lane: "delta" }),
        }
    }
}

impl PeerDeltaCommandState {
    #[cfg(all(test, desktop))]
    fn with_tunnel_launcher(tunnel_launcher: Arc<dyn DeltaTunnelLauncher>) -> Self {
        Self {
            runtime: Arc::new(Mutex::new(PeerDeltaRuntime::default())),
            lifecycle_operation: Arc::new(Mutex::new(())),
            tunnel_launcher,
        }
    }

    fn lock_lifecycle_operation(&self) -> Result<MutexGuard<'_, ()>, PeerSyncError> {
        self.lifecycle_operation.lock().map_err(|error| {
            PeerSyncError::Storage(format!("peer delta lifecycle mutex poisoned: {error}"))
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, PeerDeltaRuntime>, PeerSyncError> {
        self.runtime.lock().map_err(|error| {
            PeerSyncError::Storage(format!("peer delta command state mutex poisoned: {error}"))
        })
    }

    #[cfg(any(target_os = "android", test))]
    fn reserve_target_foreground(&self) -> Result<AndroidForegroundKey, PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        let mut runtime = self.lock()?;
        if runtime.target_foreground.is_some() {
            return Err(PeerSyncError::Protocol(
                "Android peer delta target foreground cleanup is pending".to_owned(),
            ));
        }
        let foreground = registry()
            .reserve(AndroidForegroundLane::P4Target)
            .map_err(PeerSyncError::Protocol)?;
        runtime.target_foreground =
            Some(AndroidTargetForegroundStatus::reserved(foreground.clone()));
        Ok(foreground)
    }

    #[cfg(any(target_os = "android", test))]
    fn mark_target_running_exact(
        &self,
        foreground: &AndroidForegroundKey,
    ) -> Result<(), PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        let mut runtime = self.lock()?;
        let target = runtime.target_foreground.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("Android peer delta target foreground is absent".to_owned())
        })?;
        target
            .require_exact_reserved(foreground)
            .map_err(|error| match error {
                AndroidTargetForegroundTransitionError::Stale => PeerSyncError::Protocol(
                    "Android peer delta target foreground identity is stale".to_owned(),
                ),
                AndroidTargetForegroundTransitionError::NotReserved => PeerSyncError::Protocol(
                    "Android peer delta target foreground is not reserved".to_owned(),
                ),
                AndroidTargetForegroundTransitionError::NotRunning => unreachable!(),
            })?;
        if !registry().retain_target_exact(foreground) {
            return Err(PeerSyncError::Protocol(
                "Android peer delta target foreground could not be retained".to_owned(),
            ));
        }
        target.mark_running();
        Ok(())
    }

    #[cfg(any(target_os = "android", test))]
    fn publish_target_terminal_exact(
        &self,
        foreground: &AndroidForegroundKey,
        outcome: Result<PeerDeltaPullResult, String>,
    ) -> Result<(), PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        let mut runtime = self.lock()?;
        let target = runtime.target_foreground.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("Android peer delta target foreground is absent".to_owned())
        })?;
        target
            .require_exact_running(foreground)
            .map_err(|error| match error {
                AndroidTargetForegroundTransitionError::Stale => PeerSyncError::Protocol(
                    "Android peer delta target foreground identity is stale".to_owned(),
                ),
                AndroidTargetForegroundTransitionError::NotRunning => PeerSyncError::Protocol(
                    "Android peer delta target foreground is not running".to_owned(),
                ),
                AndroidTargetForegroundTransitionError::NotReserved => unreachable!(),
            })?;
        target.publish_terminal(outcome);
        Ok(())
    }

    #[cfg(any(target_os = "android", test))]
    fn cancel_target_foreground_exact(
        &self,
        foreground: &AndroidForegroundKey,
    ) -> Result<bool, PeerSyncError> {
        {
            let runtime = self.lock()?;
            let Some(target) = runtime.target_foreground.as_ref() else {
                return Ok(false);
            };
            if !target.matches(foreground) {
                return Ok(false);
            }
        }
        // The runtime guard is dropped before cancel_exact: a registered
        // source_stop callback runs synchronously on this thread and
        // re-locks this non-reentrant mutex. cancel_exact keeps the
        // exact-generation semantics through its own key equality check.
        Ok(registry().cancel_exact(foreground))
    }

    #[cfg(any(target_os = "android", test))]
    fn target_foreground_status(
        &self,
    ) -> Result<Option<AndroidTargetForegroundStatus>, PeerSyncError> {
        Ok(self.lock()?.target_foreground.clone())
    }

    #[cfg(any(target_os = "android", test))]
    fn release_target_foreground_exact(
        &self,
        foreground: &AndroidForegroundKey,
    ) -> Result<bool, PeerSyncError> {
        let operation = self.lock_lifecycle_operation()?;
        let mut runtime = self.lock()?;
        let Some(target) = runtime.target_foreground.as_ref() else {
            return Ok(registry().release_target_exact(foreground));
        };
        if !target.matches(foreground) {
            return Ok(false);
        }
        if target.is_running() {
            // Both guards are dropped before cancel_exact: a registered
            // source_stop callback runs synchronously on this thread and
            // re-locks these non-reentrant mutexes. cancel_exact keeps the
            // exact-generation semantics through its own key equality check.
            drop(runtime);
            drop(operation);
            let _ = registry().cancel_exact(foreground);
            return Ok(false);
        }
        if !registry().release_target_exact(foreground) {
            return Ok(false);
        }
        runtime.target_foreground = None;
        Ok(true)
    }

    fn install_source(
        &self,
        session: LogicalDeltaSourceSession,
        source_device_id: &str,
        manifest_bytes: Vec<u8>,
    ) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        self.install_source_inner(session, source_device_id, manifest_bytes)
    }

    fn install_source_inner(
        &self,
        session: LogicalDeltaSourceSession,
        source_device_id: &str,
        manifest_bytes: Vec<u8>,
    ) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        let transport_session_id = uuid::Uuid::new_v4().to_string();
        let manifest_id = session.manifest_hash().to_owned();
        let objects = session.objects().to_vec();
        let prepared = PreparedLogicalLanSession::new(
            &transport_session_id,
            source_device_id,
            manifest_id.clone(),
            manifest_bytes,
            objects,
            Box::new(session),
        )?;
        self.install_prepared_source_inner(
            LanCloneHost::prepare_logical(prepared),
            transport_session_id,
            manifest_id,
        )
    }

    fn install_prepared_shared_source(
        &self,
        mut prepared: PreparedSharedDeltaSource,
    ) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        let session_id = prepared.session_id().to_owned();
        let manifest_id = prepared.manifest_id().to_owned();
        self.install_prepared_source_inner(
            LanCloneHost::prepare_logical(prepared.take_session()?),
            session_id,
            manifest_id,
        )
    }

    fn install_prepared_source_inner(
        &self,
        host: LanCloneHost,
        session_id: String,
        manifest_id: String,
    ) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        let control = host.control();
        let mut runtime = self.lock()?;
        runtime.source_preparing = false;
        runtime.stopped = false;
        runtime.source = Some(DeltaSourceRuntime {
            session_id,
            manifest_id,
            host: Some(host),
            control,
            phase: PeerDeltaSourcePhase::Prepared,
            #[cfg(desktop)]
            tunnel: None,
            #[cfg(desktop)]
            failed_tunnel: None,
            #[cfg(desktop)]
            tunnel_metadata: None,
            pairing_uri: None,
            stop_in_progress: false,
            #[cfg(any(target_os = "android", test))]
            foreground: None,
        });
        source_status(&runtime)
    }

    fn start_source(
        &self,
        session_id: &str,
        advertised_ip: Ipv4Addr,
    ) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        self.start_source_inner(session_id, advertised_ip)
    }

    fn start_source_inner(
        &self,
        session_id: &str,
        advertised_ip: Ipv4Addr,
    ) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        let mut host = self.take_prepared_host(session_id)?;
        #[cfg(desktop)]
        let started = host.start();
        #[cfg(target_os = "android")]
        let started = host.start_private_lan(advertised_ip);
        let pairing = match started {
            Ok(pairing) => pairing,
            Err(error) => {
                self.restore_clean_start(session_id, host)?;
                return Err(error);
            }
        };
        let address = host.address().ok_or_else(|| {
            PeerSyncError::Transport("peer delta source address is unavailable".to_owned())
        })?;
        let endpoint = format!("http://{advertised_ip}:{}", address.port());
        let pairing_uri = build_pairing_uri(&endpoint, &pairing)?;
        let mut runtime = self.lock()?;
        let source = require_source(&mut runtime, session_id)?;
        source.host = Some(host);
        source.pairing_uri = Some(pairing_uri);
        source.phase = PeerDeltaSourcePhase::Running;
        source_status(&runtime)
    }

    fn take_prepared_host(&self, session_id: &str) -> Result<LanCloneHost, PeerSyncError> {
        let mut runtime = self.lock()?;
        let source = require_source(&mut runtime, session_id)?;
        if source.phase != PeerDeltaSourcePhase::Prepared {
            return Err(PeerSyncError::Protocol(
                "peer delta source is not prepared".to_owned(),
            ));
        }
        let host = source.host.take().ok_or_else(|| {
            PeerSyncError::Protocol("peer delta source host is unavailable".to_owned())
        })?;
        source.phase = PeerDeltaSourcePhase::Starting;
        Ok(host)
    }

    fn restore_clean_start(
        &self,
        session_id: &str,
        host: LanCloneHost,
    ) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock()?;
        let source = require_source(&mut runtime, session_id)?;
        source.host = Some(host);
        source.phase = PeerDeltaSourcePhase::Prepared;
        #[cfg(desktop)]
        {
            source.tunnel_metadata = None;
        }
        Ok(())
    }

    #[cfg(desktop)]
    fn start_tunnel(
        &self,
        session_id: &str,
        request: PeerDeltaTunnelStart,
    ) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        self.start_tunnel_inner(session_id, request)
    }

    #[cfg(desktop)]
    fn start_tunnel_inner(
        &self,
        session_id: &str,
        request: PeerDeltaTunnelStart,
    ) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        let metadata = match &request {
            PeerDeltaTunnelStart::Quick => PeerDeltaTunnelMetadata::quick(),
            PeerDeltaTunnelStart::Named { .. } => PeerDeltaTunnelMetadata::named(),
        };
        let mut host = self.take_prepared_host(session_id)?;
        {
            let mut runtime = self.lock()?;
            require_source(&mut runtime, session_id)?.tunnel_metadata = Some(metadata);
        }
        let origin = match metadata.kind {
            PeerDeltaTunnelKind::Quick => host.start_quick_tunnel_origin(),
            PeerDeltaTunnelKind::Named => host.start_named_tunnel_origin(),
        };
        let pairing = match origin {
            Ok(pairing) => pairing,
            Err(error) => {
                self.restore_clean_start(session_id, host)?;
                return Err(
                    if metadata.kind == PeerDeltaTunnelKind::Named
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
        let tunnel = match self.tunnel_launcher.start(request, host) {
            Ok(tunnel) => tunnel,
            Err(mut failure) => {
                match failure.recover_host() {
                    Ok(Some(mut host)) => {
                        if host.stop().is_ok() {
                            self.restore_clean_start(session_id, host)?;
                        } else {
                            let mut runtime = self.lock()?;
                            let source = require_source(&mut runtime, session_id)?;
                            source.host = Some(host);
                            source.phase = PeerDeltaSourcePhase::Stopping;
                        }
                    }
                    Ok(None) | Err(_) => {
                        let mut runtime = self.lock()?;
                        let source = require_source(&mut runtime, session_id)?;
                        source.failed_tunnel = Some(failure);
                        source.phase = PeerDeltaSourcePhase::Stopping;
                    }
                }
                return Err(tunnel_start_error());
            }
        };
        let endpoint = match validate_lan_endpoint(tunnel.transport_url().as_str()) {
            Ok(endpoint) if endpoint.starts_with("https://") => endpoint,
            _ => return Err(self.abort_started_tunnel(session_id, tunnel)),
        };
        let pairing_uri = match build_pairing_uri(&endpoint, &pairing) {
            Ok(uri) => uri,
            Err(_) => return Err(self.abort_started_tunnel(session_id, tunnel)),
        };
        let mut runtime = self.lock()?;
        let source = require_source(&mut runtime, session_id)?;
        source.tunnel = Some(tunnel);
        source.pairing_uri = Some(pairing_uri);
        source.phase = PeerDeltaSourcePhase::Running;
        source_status(&runtime)
    }

    #[cfg(desktop)]
    fn abort_started_tunnel(
        &self,
        session_id: &str,
        mut tunnel: Box<dyn DeltaTunnel>,
    ) -> PeerSyncError {
        let stopped = tunnel.stop().is_ok();
        if let Ok(mut runtime) = self.lock() {
            if let Ok(source) = require_source(&mut runtime, session_id) {
                if stopped {
                    source.phase = PeerDeltaSourcePhase::Stopped;
                    runtime.source = None;
                    runtime.stopped = true;
                } else {
                    source.tunnel = Some(tunnel);
                    source.phase = PeerDeltaSourcePhase::Stopping;
                }
            }
        }
        tunnel_start_error()
    }

    #[cfg(desktop)]
    fn stop_source(&self, session_id: &str) -> Result<(), PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        self.stop_source_inner(session_id)
    }

    #[cfg(desktop)]
    fn stop_source_inner(&self, session_id: &str) -> Result<(), PeerSyncError> {
        let (mut host, mut tunnel, mut failed_tunnel) = {
            let mut runtime = self.lock()?;
            let source = require_source(&mut runtime, session_id)?;
            if source.stop_in_progress {
                return Err(PeerSyncError::Protocol(
                    "peer delta source stop is already in progress".to_owned(),
                ));
            }
            source.stop_in_progress = true;
            source.phase = PeerDeltaSourcePhase::Stopping;
            source.pairing_uri = None;
            (
                source.host.take(),
                source.tunnel.take(),
                source.failed_tunnel.take(),
            )
        };
        let mut error = None;
        if let Some(active) = tunnel.as_mut() {
            if active.stop().is_ok() {
                tunnel = None;
            } else {
                error = Some(tunnel_stop_error());
            }
        }
        if let Some(failure) = failed_tunnel.as_mut() {
            if failure.stop().is_ok() {
                failed_tunnel = None;
            } else if error.is_none() {
                error = Some(tunnel_stop_error());
            }
        }
        if let Some(active) = host.as_mut() {
            match active.stop() {
                Ok(()) => host = None,
                Err(failure) if error.is_none() => error = Some(failure),
                Err(_) => {}
            }
        }
        let mut runtime = self.lock()?;
        let source = require_source(&mut runtime, session_id)?;
        source.stop_in_progress = false;
        source.host = host;
        source.tunnel = tunnel;
        source.failed_tunnel = failed_tunnel;
        if let Some(error) = error {
            return Err(error);
        }
        runtime.source = None;
        runtime.stopped = true;
        Ok(())
    }

    fn revoke(&self, session_id: &str, device_id: &str) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock()?;
        let source = require_source(&mut runtime, session_id)?;
        if !source.control.revoke(device_id) {
            return Err(PeerSyncError::Validation(
                "peer delta source device is absent".to_owned(),
            ));
        }
        Ok(())
    }

    pub(crate) fn revoke_registered_device(&self, device_id: &str) {
        if let Ok(runtime) = self.lock() {
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
        let mut runtime = self.lock()?;
        let source = runtime.source.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer delta source is not prepared".to_owned())
        })?;
        if source.phase != PeerDeltaSourcePhase::Prepared {
            return Err(PeerSyncError::Protocol(
                "peer delta source is not prepared".to_owned(),
            ));
        }
        source
            .host
            .as_mut()
            .ok_or_else(|| {
                PeerSyncError::Protocol("peer delta source host is unavailable".to_owned())
            })?
            .enable_v2_registry(app_root, name, permissions)
    }

    fn status(&self) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        self.status_inner()
    }

    #[cfg(any(target_os = "android", test))]
    fn attach_source_foreground(
        &self,
        session_id: &str,
        key: AndroidForegroundKey,
    ) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock()?;
        require_source(&mut runtime, session_id)?.foreground = Some(key);
        Ok(())
    }

    #[cfg(any(target_os = "android", test))]
    fn pause_source_exact(&self, key: &AndroidForegroundKey) {
        let Ok(_operation) = self.lifecycle_operation.lock() else {
            return;
        };
        let Ok(mut runtime) = self.lock() else {
            return;
        };
        let Some(source) = runtime.source.as_mut() else {
            return;
        };
        if source.foreground.as_ref() != Some(key) {
            return;
        }
        let Some(mut host) = source.host.take() else {
            return;
        };
        if host.stop().is_err() {
            source.host = Some(host);
            source.phase = PeerDeltaSourcePhase::Stopping;
            return;
        }
        source.host = Some(host);
        source.foreground = None;
        source.pairing_uri = None;
        source.phase = PeerDeltaSourcePhase::Prepared;
    }

    #[cfg(any(target_os = "android", test))]
    fn release_source_android(
        &self,
        session_id: &str,
    ) -> Result<Option<AndroidForegroundKey>, PeerSyncError> {
        let operation = self.lock_lifecycle_operation()?;
        let (foreground, mut source) = {
            let mut runtime = self.lock()?;
            let source = runtime
                .source
                .as_ref()
                .filter(|source| source.session_id == session_id)
                .ok_or_else(|| {
                    PeerSyncError::Validation("peer delta source session is absent".to_owned())
                })?;
            let foreground = source.foreground.clone();
            let source = runtime.source.take().expect("checked source");
            runtime.stopped = true;
            (foreground, source)
        };
        if let Some(host) = source.host.as_mut() {
            if let Err(error) = host.stop() {
                let mut runtime = self.lock()?;
                runtime.stopped = false;
                runtime.source = Some(source);
                return Err(error);
            }
        }
        source.foreground = None;
        drop(source);
        drop(operation);
        if let Some(key) = foreground.as_ref() {
            let _ = registry().cancel_exact(key);
            let _ = registry().detach_if_generation(key);
        }
        Ok(foreground)
    }

    fn status_inner(&self) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        #[cfg(desktop)]
        let tunnel_owner = {
            let mut runtime = self.lock()?;
            runtime.source.as_mut().and_then(|source| {
                source
                    .tunnel
                    .take()
                    .map(|tunnel| (source.session_id.clone(), tunnel))
            })
        };
        #[cfg(desktop)]
        if let Some((session_id, mut tunnel)) = tunnel_owner {
            let lifecycle = tunnel.lifecycle();
            let mut runtime = self.lock()?;
            if let Ok(source) = require_source(&mut runtime, &session_id) {
                match lifecycle {
                    Ok(DeltaTunnelLifecycle::Running) => source.tunnel = Some(tunnel),
                    Ok(DeltaTunnelLifecycle::CleanupPending) | Err(_) => {
                        source.tunnel = Some(tunnel);
                        source.phase = PeerDeltaSourcePhase::Stopping;
                        source.pairing_uri = None;
                    }
                    Ok(DeltaTunnelLifecycle::Stopped) => {
                        source.phase = PeerDeltaSourcePhase::Stopping;
                        source.pairing_uri = None;
                    }
                }
            }
            drop(runtime);
            if matches!(lifecycle, Ok(DeltaTunnelLifecycle::Stopped)) {
                let _ = self.stop_source_inner(&session_id);
            }
        }
        let runtime = self.lock()?;
        if runtime.source.is_none() {
            return Ok(PeerDeltaSourceStatus::idle(if runtime.stopped {
                PeerDeltaSourcePhase::Stopped
            } else {
                PeerDeltaSourcePhase::Idle
            }));
        }
        source_status(&runtime)
    }

    #[cfg(desktop)]
    fn tunnel_status(&self) -> Result<PeerDeltaTunnelStatus, PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        let _ = self.status_inner()?;
        let runtime = self.lock()?;
        Ok(tunnel_status(&runtime))
    }

    #[cfg(desktop)]
    pub(crate) fn shutdown_for_exit(&self) {
        let Ok(_operation) = self.lifecycle_operation.lock() else {
            return;
        };
        let session_id = self.runtime.lock().ok().and_then(|runtime| {
            runtime
                .source
                .as_ref()
                .map(|source| source.session_id.clone())
        });
        if let Some(session_id) = session_id {
            let _ = self.stop_source_inner(&session_id);
        }
    }

    fn begin_pull(&self) -> Result<PullGuard, PeerSyncError> {
        let mut runtime = self.lock()?;
        if runtime.pull_in_progress {
            return Err(PeerSyncError::Protocol(
                "peer delta pull is already running".to_owned(),
            ));
        }
        runtime.pull_in_progress = true;
        Ok(PullGuard {
            state: self.clone(),
        })
    }
}

struct PullGuard {
    state: PeerDeltaCommandState,
}

// Clears source_preparing on drop (mirroring PullGuard) so a panic in the
// prepare path cannot wedge every later prepare with "already prepared"
// until app restart. Disarm once the flag has been consumed.
struct SourcePrepareGuard<'a> {
    runtime: &'a Mutex<PeerDeltaRuntime>,
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

struct AbortUnsealedJobOnDrop<'a>(&'a RefCell<DurableCasJob>);

impl Drop for AbortUnsealedJobOnDrop<'_> {
    fn drop(&mut self) {
        let mut job = self.0.borrow_mut();
        if !job.is_sealed() && !job.is_released() {
            let _ = job.release(CasReleaseOutcome::Aborted);
        }
    }
}

impl Drop for PullGuard {
    fn drop(&mut self) {
        if let Ok(mut runtime) = self.state.runtime.lock() {
            runtime.pull_in_progress = false;
        }
    }
}

fn require_source<'a>(
    runtime: &'a mut PeerDeltaRuntime,
    session_id: &str,
) -> Result<&'a mut DeltaSourceRuntime, PeerSyncError> {
    runtime
        .source
        .as_mut()
        .filter(|source| source.session_id == session_id)
        .ok_or_else(|| PeerSyncError::Validation("peer delta source session is absent".to_owned()))
}

fn source_status(runtime: &PeerDeltaRuntime) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
    let Some(source) = &runtime.source else {
        return Ok(PeerDeltaSourceStatus::idle(PeerDeltaSourcePhase::Idle));
    };
    Ok(PeerDeltaSourceStatus {
        session_id: Some(source.session_id.clone()),
        manifest_id: Some(source.manifest_id.clone()),
        pairing_uri: source.pairing_uri.clone(),
        phase: source.phase,
        devices: source
            .control
            .devices()
            .into_iter()
            .map(|device| PeerDeltaSourceDevice {
                device_id: device.device_id,
                transferred_bytes: device.verified_bytes,
                current_object: device.current_object,
                last_seen_at: u64::try_from(device.last_seen_unix_ms).unwrap_or(u64::MAX),
                revoked: device.revoked,
            })
            .collect(),
        #[cfg(desktop)]
        tunnel: source.tunnel_metadata,
    })
}

#[cfg(desktop)]
fn tunnel_status(runtime: &PeerDeltaRuntime) -> PeerDeltaTunnelStatus {
    let Some(source) = runtime
        .source
        .as_ref()
        .filter(|source| source.tunnel_metadata.is_some())
    else {
        return PeerDeltaTunnelStatus {
            session_id: None,
            phase: PeerDeltaTunnelPhase::Idle,
            tunnel: None,
        };
    };
    let phase = match source.phase {
        PeerDeltaSourcePhase::Starting => PeerDeltaTunnelPhase::Starting,
        PeerDeltaSourcePhase::Running => PeerDeltaTunnelPhase::Running,
        PeerDeltaSourcePhase::Stopping => PeerDeltaTunnelPhase::Stopping,
        PeerDeltaSourcePhase::Stopped => PeerDeltaTunnelPhase::Stopped,
        PeerDeltaSourcePhase::Idle | PeerDeltaSourcePhase::Prepared => PeerDeltaTunnelPhase::Idle,
    };
    PeerDeltaTunnelStatus {
        session_id: Some(source.session_id.clone()),
        phase,
        tunnel: source.tunnel_metadata,
    }
}

// Desktop-only tunnel lifecycle errors.
#[cfg_attr(target_os = "android", allow(dead_code))]
fn tunnel_start_error() -> PeerSyncError {
    PeerSyncError::Transport("peer delta tunnel failed to start".to_owned())
}

#[cfg_attr(target_os = "android", allow(dead_code))]
fn tunnel_stop_error() -> PeerSyncError {
    PeerSyncError::Transport("peer delta tunnel failed to stop".to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PeerDeltaPullResult {
    NoChanges {
        revision: i64,
        transferred_objects: u64,
        transferred_bytes: u64,
    },
    Updated {
        revision: i64,
        transferred_objects: u64,
        transferred_bytes: u64,
    },
    FullCloneRequired {
        reason: &'static str,
    },
    Conflict {
        reason: &'static str,
    },
}

#[derive(Default)]
struct MeasuredTransferTotals {
    objects: Cell<u64>,
    bytes: Cell<u64>,
}

struct MeasuredLogicalDeltaSource<'a, S: LogicalDeltaObjectSource + ?Sized> {
    inner: &'a mut S,
    totals: Rc<MeasuredTransferTotals>,
}

impl<'a, S: LogicalDeltaObjectSource + ?Sized> MeasuredLogicalDeltaSource<'a, S> {
    fn new(inner: &'a mut S) -> Self {
        Self {
            inner,
            totals: Rc::new(MeasuredTransferTotals::default()),
        }
    }

    fn totals(&self) -> (u64, u64) {
        (self.totals.objects.get(), self.totals.bytes.get())
    }
}

impl<S: LogicalDeltaObjectSource + ?Sized> LogicalDeltaObjectSource
    for MeasuredLogicalDeltaSource<'_, S>
{
    fn open_object(&mut self, object: &LogicalDeltaObject) -> Result<Box<dyn Read>, PeerSyncError> {
        let reader = self.inner.open_object(object)?;
        let objects = self.totals.objects.get().checked_add(1).ok_or_else(|| {
            PeerSyncError::Validation("logical transfer object count overflow".to_owned())
        })?;
        self.totals.objects.set(objects);
        Ok(Box::new(MeasuredLogicalDeltaReader {
            inner: reader,
            totals: Rc::clone(&self.totals),
        }))
    }
}

struct MeasuredLogicalDeltaReader {
    inner: Box<dyn Read>,
    totals: Rc<MeasuredTransferTotals>,
}

impl Read for MeasuredLogicalDeltaReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(output)?;
        let bytes = self
            .totals
            .bytes
            .get()
            .checked_add(read as u64)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "logical transfer byte count overflow",
                )
            })?;
        self.totals.bytes.set(bytes);
        Ok(read)
    }
}

// Test-facing wrapper around the cancellation-aware pull entry point.
#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
fn pull_logical_delta<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    source_device_id: &str,
    expected_revision: i64,
    remote_manifest_bytes: &[u8],
    remote_source: &mut S,
) -> Result<PeerDeltaPullResult, PeerSyncError> {
    pull_logical_delta_with_cancellation(
        store,
        cas,
        app_root,
        source_device_id,
        expected_revision,
        remote_manifest_bytes,
        remote_source,
        &NeverCancelled,
    )
}

#[allow(clippy::too_many_arguments)]
fn pull_logical_delta_with_cancellation<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    source_device_id: &str,
    expected_revision: i64,
    remote_manifest_bytes: &[u8],
    remote_source: &mut S,
    cancellation: &dyn CancellationProbe,
) -> Result<PeerDeltaPullResult, PeerSyncError> {
    if cancellation.is_cancelled() {
        return Err(PeerSyncError::Cancelled);
    }
    let actual_revision = store.revision().map_err(store_error)?;
    if actual_revision != expected_revision {
        return Ok(PeerDeltaPullResult::Conflict {
            reason: "staleRevision",
        });
    }
    let local = store
        .seal_or_initialize_active_logical_generation(cas)
        .map_err(store_error)?;
    if local.manifest.library_id != PRODUCT_LOGICAL_LIBRARY_ID {
        return Err(PeerSyncError::Validation(
            "active logical library identity is unsupported".to_owned(),
        ));
    }
    let remote_manifest = decode_logical_manifest(remote_manifest_bytes)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    if remote_manifest.library_id != local.manifest.library_id {
        return Ok(PeerDeltaPullResult::FullCloneRequired {
            reason: "noExactCommonBase",
        });
    }

    let job_id = format!("{P4_DELTA_TARGET_JOB_PREFIX}{}", uuid::Uuid::new_v4());
    let job = RefCell::new(DurableCasJob::begin(
        app_root,
        &job_id,
        CasJobKind::LogicalDeltaTarget,
        now_millis()?,
    )?);
    let _abort_unsealed = AbortUnsealedJobOnDrop(&job);

    let staging_root = app_root.join("peer-delta").join("staging");
    let mut target = PersistentLogicalDeltaTarget::new_with_durable_job(
        store,
        cas,
        source_device_id,
        &local.manifest.library_id,
        &local.manifest.generation,
        remote_manifest_bytes,
        &staging_root,
        &job,
    )?;
    if !target.has_common_base()? {
        drop(target);
        job.borrow_mut().seal(store, now_millis()?)?;
        let bootstrap = establish_logical_common_base(
            store,
            cas,
            source_device_id,
            &local.manifest.library_id,
            &local.manifest.generation,
            expected_revision,
            remote_manifest_bytes,
        );
        return finish_bootstrap(&job, expected_revision, bootstrap);
    }

    let plan = match target.build_ready_plan(expected_revision) {
        Ok(plan) => plan,
        Err(error) => {
            drop(target);
            return classify_plan_error(error);
        }
    };
    let local_hashes = local
        .manifest
        .objects
        .iter()
        .map(|object| object.hash.clone())
        .collect::<BTreeSet<_>>();
    let remote_sizes = remote_manifest
        .objects
        .iter()
        .map(|object| (object.hash.clone(), object.size))
        .collect::<BTreeMap<_, _>>();
    let mut measured_source = MeasuredLogicalDeltaSource::new(remote_source);
    let activation = execute_logical_delta_pull_with_pre_activation(
        &plan,
        &local_hashes,
        cas,
        &remote_sizes,
        &mut measured_source,
        &mut target,
        |_| Ok(()),
        cancellation,
    );
    let (transferred_objects, transferred_bytes) = measured_source.totals();
    let durable_abort_succeeded = target.durable_abort_succeeded();
    drop(target);
    finish_pull(
        &job,
        &plan,
        activation,
        transferred_objects,
        transferred_bytes,
        durable_abort_succeeded,
    )
}

fn finish_pull(
    job: &RefCell<DurableCasJob>,
    plan: &ReadyLogicalDeltaPlan,
    activation: Result<LogicalDeltaActivation, PeerSyncError>,
    transferred_objects: u64,
    transferred_bytes: u64,
    durable_abort_succeeded: bool,
) -> Result<PeerDeltaPullResult, PeerSyncError> {
    match activation {
        Ok(LogicalDeltaActivation::Activated { revision }) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Committed);
            Ok(if plan.apply.is_empty() {
                PeerDeltaPullResult::NoChanges {
                    revision,
                    transferred_objects,
                    transferred_bytes,
                }
            } else {
                PeerDeltaPullResult::Updated {
                    revision,
                    transferred_objects,
                    transferred_bytes,
                }
            })
        }
        Ok(LogicalDeltaActivation::AlreadyActive { revision }) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Committed);
            Ok(PeerDeltaPullResult::NoChanges {
                revision,
                transferred_objects: 0,
                transferred_bytes: 0,
            })
        }
        Ok(LogicalDeltaActivation::Conflict {
            actual_revision,
            actual_base_manifest_hash,
        }) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            Ok(classify_activation_conflict(
                &plan,
                actual_revision,
                &actual_base_manifest_hash,
            ))
        }
        Err(error) => {
            if !job.borrow().is_sealed() || durable_abort_succeeded {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            }
            classify_plan_error(error)
        }
    }
}

fn finish_bootstrap(
    job: &RefCell<DurableCasJob>,
    expected_revision: i64,
    bootstrap: Result<(), PeerSyncError>,
) -> Result<PeerDeltaPullResult, PeerSyncError> {
    match bootstrap {
        Ok(()) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Committed);
            Ok(PeerDeltaPullResult::NoChanges {
                revision: expected_revision,
                transferred_objects: 0,
                transferred_bytes: 0,
            })
        }
        Err(PeerSyncError::Validation(message)) if message.contains("remote content differs") => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            Ok(PeerDeltaPullResult::FullCloneRequired {
                reason: "noExactCommonBase",
            })
        }
        Err(error @ PeerSyncError::ActivationConflict { .. }) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            classify_plan_error(error)
        }
        Err(error) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            Err(error)
        }
    }
}

fn open_peer_delta_store(store: &PersistentStore) -> Result<PersistentStore, StoreError> {
    store.open_native_job_store()
}

fn classify_plan_error(error: PeerSyncError) -> Result<PeerDeltaPullResult, PeerSyncError> {
    match error {
        PeerSyncError::LogicalMergeConflict { .. } => Ok(PeerDeltaPullResult::Conflict {
            reason: "localAndRemoteChanged",
        }),
        PeerSyncError::ActivationConflict { expected, actual } => {
            Ok(PeerDeltaPullResult::Conflict {
                reason: if activation_conflict_is_revision(&expected, &actual) {
                    "staleRevision"
                } else {
                    "localAndRemoteChanged"
                },
            })
        }
        error => Err(error),
    }
}

fn activation_conflict_is_revision(expected: &Option<String>, actual: &Option<String>) -> bool {
    expected
        .as_deref()
        .is_some_and(|value| value.parse::<i64>().is_ok())
        && actual
            .as_deref()
            .is_some_and(|value| value.parse::<i64>().is_ok())
}

fn classify_activation_conflict(
    plan: &ReadyLogicalDeltaPlan,
    actual_revision: i64,
    actual_base_manifest_hash: &str,
) -> PeerDeltaPullResult {
    PeerDeltaPullResult::Conflict {
        reason: if actual_revision != plan.expected_local_revision {
            "staleRevision"
        } else if actual_base_manifest_hash != plan.expected_base_manifest_hash {
            "localAndRemoteChanged"
        } else {
            // The target can also reject an otherwise identical revision/base when
            // the active logical head no longer matches the prepared stage.
            "staleRevision"
        },
    }
}

#[tauri::command]
pub async fn peer_delta_prepare(
    app: AppHandle,
    state: State<'_, PeerDeltaCommandState>,
) -> Result<PeerDeltaSourceStatus, String> {
    let state = state.inner().clone();
    let app_root = app_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        {
            let mut runtime = state.lock().map_err(|error| error.to_string())?;
            if runtime.source_preparing || runtime.source.is_some() {
                return Err("peer delta source is already prepared".to_owned());
            }
            runtime.source_preparing = true;
        }
        let guard = SourcePrepareGuard {
            runtime: &state.runtime,
            armed: true,
        };
        let prepared = (|| {
            let cas = PayloadCas::new(&app_root).map_err(|error| error.to_string())?;
            let prepared = persistent_store::commands::with_store_mut(app.state(), |store| {
                prepare_shared_delta_source(store, &cas, &app_root).map_err(|error| {
                    StoreError::Store {
                        message: error.to_string(),
                    }
                })
            })
            .map_err(|error| error.to_string())?;
            state
                .install_prepared_shared_source(prepared)
                .map_err(|error| error.to_string())
        })();
        if prepared.is_ok() {
            // install_source consumed the flag atomically with publishing the
            // prepared source.
            guard.disarm();
        }
        let status = prepared?;
        state
            .configure_v2_registry(
                &app_root,
                super::device_registry::platform_device_name(),
                super::device_registry::DevicePermissions::read(),
            )
            .map_err(|error| error.to_string())?;
        Ok(status)
    })
    .await
    .map_err(|error| format!("peer delta source preparation worker failed: {error}"))?
}

#[tauri::command]
#[cfg(target_os = "android")]
pub fn peer_delta_source_reserve() -> Result<AndroidForegroundKey, String> {
    registry().reserve(AndroidForegroundLane::P4Source)
}

#[tauri::command]
#[cfg(target_os = "android")]
pub fn peer_delta_target_reserve(
    state: State<'_, PeerDeltaCommandState>,
) -> Result<AndroidForegroundKey, String> {
    state
        .reserve_target_foreground()
        .map_err(|error| error.to_string())
}

#[tauri::command]
#[cfg(target_os = "android")]
pub fn peer_delta_target_foreground_status(
    state: State<'_, PeerDeltaCommandState>,
) -> Result<Option<AndroidTargetForegroundStatus>, String> {
    state
        .target_foreground_status()
        .map_err(|error| error.to_string())
}

#[tauri::command]
#[cfg(target_os = "android")]
pub fn peer_delta_target_foreground_release(
    state: State<'_, PeerDeltaCommandState>,
    foreground: AndroidForegroundKey,
) -> Result<bool, String> {
    state
        .release_target_foreground_exact(&foreground)
        .map_err(|error| error.to_string())
}

#[tauri::command]
#[cfg(target_os = "android")]
pub fn peer_delta_target_foreground_cancel(
    state: State<'_, PeerDeltaCommandState>,
    foreground: AndroidForegroundKey,
) -> Result<bool, String> {
    state
        .cancel_target_foreground_exact(&foreground)
        .map_err(|error| error.to_string())
}

#[cfg(desktop)]
#[tauri::command]
pub fn peer_delta_start(
    state: State<'_, PeerDeltaCommandState>,
    session_id: String,
) -> Result<PeerDeltaSourceStatus, String> {
    let address = discover_lan_ipv4().map_err(|error| error.to_string())?;
    state
        .start_source(&session_id, address)
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "android")]
#[tauri::command]
pub async fn peer_delta_start(
    state: State<'_, PeerDeltaCommandState>,
    session_id: String,
    foreground: AndroidForegroundKey,
) -> Result<PeerDeltaSourceStatus, String> {
    let cancellation = acquire_foreground(&foreground, AndroidForegroundLane::P4Source).await?;
    let address = super::android_source_commands::discover_private_lan_address()?;
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if cancellation.is_cancelled() {
            return Err("Android foreground service was cancelled".to_owned());
        }
        let status = state
            .start_source(&session_id, address)
            .map_err(|error| error.to_string())?;
        state
            .attach_source_foreground(&session_id, foreground.clone())
            .map_err(|error| error.to_string())?;
        let callback_state = state.clone();
        let callback_key = foreground.clone();
        if !registry().set_source_stop_callback_exact(&foreground, move || {
            callback_state.pause_source_exact(&callback_key);
        }) {
            state.pause_source_exact(&foreground);
            return Err("Android foreground service detached before source start".to_owned());
        }
        Ok(status)
    })
    .await
    .map_err(|error| format!("peer delta source start worker failed: {error}"))?
}

#[cfg(desktop)]
#[tauri::command]
pub async fn peer_delta_tunnel_start(
    state: State<'_, PeerDeltaCommandState>,
    session_id: String,
    tunnel: PeerDeltaTunnelStart,
) -> Result<PeerDeltaSourceStatus, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.start_tunnel(&session_id, tunnel))
        .await
        .map_err(|error| format!("peer delta tunnel start worker failed: {error}"))?
        .map_err(|_| "peer delta tunnel failed to start".to_owned())
}

#[cfg(desktop)]
#[tauri::command]
pub async fn peer_delta_tunnel_status(
    state: State<'_, PeerDeltaCommandState>,
) -> Result<PeerDeltaTunnelStatus, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.tunnel_status())
        .await
        .map_err(|error| format!("peer delta tunnel status worker failed: {error}"))?
        .map_err(|_| "peer delta tunnel status is unavailable".to_owned())
}

#[cfg(desktop)]
#[tauri::command]
pub async fn peer_delta_tunnel_stop(
    state: State<'_, PeerDeltaCommandState>,
    session_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.stop_source(&session_id))
        .await
        .map_err(|error| format!("peer delta tunnel stop worker failed: {error}"))?
        .map_err(|_| "peer delta tunnel failed to stop".to_owned())
}

#[tauri::command]
pub fn peer_delta_status(
    state: State<'_, PeerDeltaCommandState>,
) -> Result<PeerDeltaSourceStatus, String> {
    state.status().map_err(|error| error.to_string())
}

#[cfg(desktop)]
#[tauri::command]
pub async fn peer_delta_stop(
    state: State<'_, PeerDeltaCommandState>,
    session_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.stop_source(&session_id))
        .await
        .map_err(|error| format!("peer delta source stop worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "android")]
#[tauri::command]
pub async fn peer_delta_stop(
    state: State<'_, PeerDeltaCommandState>,
    session_id: String,
) -> Result<Option<AndroidForegroundKey>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.release_source_android(&session_id))
        .await
        .map_err(|error| format!("peer delta source stop worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn peer_delta_revoke(
    state: State<'_, PeerDeltaCommandState>,
    session_id: String,
    device_id: String,
) -> Result<(), String> {
    state
        .revoke(&session_id, &device_id)
        .map_err(|error| error.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn peer_delta_pull_with_cancellation<C: CancellationProbe + Send + 'static>(
    app: AppHandle,
    state: State<'_, PeerDeltaCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    claim: String,
    expected_revision: i64,
    cancellation: C,
) -> Result<PeerDeltaPullResult, String> {
    let state = state.inner().clone();
    peer_delta_pull_with_client_factory(
        app,
        state,
        expected_revision,
        cancellation,
        move |app_root| match LanLogicalDeltaClient::claim_v2_and_register(
            app_root,
            super::device_registry::platform_device_name(),
            &endpoint,
            &session_id,
            &manifest_id,
            &claim,
        ) {
            Ok(client) => Ok(client),
            Err(error) if super::lan::v2_claim_is_unsupported(&error) => {
                LanLogicalDeltaClient::claim_p4(&endpoint, &session_id, &manifest_id, &claim)
                    .map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        },
    )
    .await
}

pub(crate) async fn peer_delta_pull_registered_client(
    app: AppHandle,
    state: PeerDeltaCommandState,
    client: LanLogicalDeltaClient,
    expected_revision: i64,
) -> Result<PeerDeltaPullResult, String> {
    peer_delta_pull_with_client_factory(app, state, expected_revision, NeverCancelled, move |_| {
        Ok(client)
    })
    .await
}

async fn peer_delta_pull_with_client_factory<
    C: CancellationProbe + Send + 'static,
    F: FnOnce(&Path) -> Result<LanLogicalDeltaClient, String> + Send + 'static,
>(
    app: AppHandle,
    state: PeerDeltaCommandState,
    expected_revision: i64,
    cancellation: C,
    client_factory: F,
) -> Result<PeerDeltaPullResult, String> {
    let app_root = app_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = state.begin_pull().map_err(|error| error.to_string())?;
        reclaim_abandoned_durable_cas_jobs(
            &app_root,
            P4_DELTA_TARGET_JOB_PREFIX,
            CasJobKind::LogicalDeltaTarget,
        )
        .map_err(|error| error.to_string())?;
        let mut client = client_factory(&app_root)?;
        let manifest = client.fetch_manifest().map_err(|error| error.to_string())?;
        let source_device_id = client.source_device_id().to_owned();
        let mut store = persistent_store::commands::with_store_mut(app.state(), |store| {
            open_peer_delta_store(store)
        })
        .map_err(|error| error.to_string())?;
        let cas = PayloadCas::new(&app_root).map_err(|error| error.to_string())?;
        let result = pull_logical_delta_with_cancellation(
            &mut store,
            &cas,
            &app_root,
            &source_device_id,
            expected_revision,
            &manifest,
            &mut client,
            &cancellation,
        )
        .map_err(|error| error.to_string())?;
        record_delta_completion(
            &app_root,
            &source_device_id,
            client.is_v2_registered(),
            &result,
        );
        Ok(result)
    })
    .await
    .map_err(|error| format!("peer delta pull worker failed: {error}"))?
}

#[cfg(desktop)]
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn peer_delta_pull(
    app: AppHandle,
    state: State<'_, PeerDeltaCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    claim: String,
    expected_revision: i64,
) -> Result<PeerDeltaPullResult, String> {
    peer_delta_pull_with_cancellation(
        app,
        state,
        endpoint,
        session_id,
        manifest_id,
        claim,
        expected_revision,
        NeverCancelled,
    )
    .await
}

#[cfg(target_os = "android")]
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn peer_delta_pull(
    app: AppHandle,
    state: State<'_, PeerDeltaCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    claim: String,
    expected_revision: i64,
    foreground: AndroidForegroundKey,
) -> Result<PeerDeltaPullResult, String> {
    let cancellation = acquire_foreground(&foreground, AndroidForegroundLane::P4Target).await?;
    let state_owner = state.inner().clone();
    state_owner
        .mark_target_running_exact(&foreground)
        .map_err(|error| error.to_string())?;
    let outcome = peer_delta_pull_with_cancellation(
        app,
        state,
        endpoint,
        session_id,
        manifest_id,
        claim,
        expected_revision,
        cancellation,
    )
    .await;
    state_owner
        .publish_target_terminal_exact(&foreground, outcome.clone())
        .map_err(|error| error.to_string())?;
    outcome
}

fn record_delta_completion(
    app_root: &Path,
    source_device_id: &str,
    registered_v2: bool,
    result: &PeerDeltaPullResult,
) {
    if !registered_v2 {
        return;
    }
    let transferred_bytes = match result {
        PeerDeltaPullResult::NoChanges {
            transferred_bytes, ..
        }
        | PeerDeltaPullResult::Updated {
            transferred_bytes, ..
        } => *transferred_bytes,
        PeerDeltaPullResult::Conflict { .. } | PeerDeltaPullResult::FullCloneRequired { .. } => {
            return
        }
    };
    super::device_registry::record_incoming_completed_operation_best_effort(
        app_root,
        source_device_id,
        transferred_bytes,
    );
}

fn app_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("failed to resolve application data directory: {error}"))
}

fn build_pairing_uri(endpoint: &str, pairing: &super::LanPairing) -> Result<String, PeerSyncError> {
    super::tunnel_lifecycle::build_lane_pairing_uri("peer-delta", endpoint, pairing)
}

pub(crate) fn canonical_source_device_id(app_root: &Path) -> Result<String, PeerSyncError> {
    super::device_registry::load_or_create_device_id(app_root)
        .map_err(|error| PeerSyncError::Storage(error.to_string()))
}

#[cfg(test)]
pub(crate) fn load_or_create_source_device_id(path: &Path) -> Result<String, PeerSyncError> {
    if let Ok(value) = fs::read_to_string(path) {
        let value = value.trim_end_matches(['\r', '\n']);
        if uuid::Uuid::parse_str(value)
            .map(|parsed| parsed.to_string() != value)
            .unwrap_or(true)
        {
            return Err(PeerSyncError::Storage(
                "invalid persisted source device identity".to_owned(),
            ));
        }
    }
    let legacy_parent = path.parent().ok_or_else(|| {
        PeerSyncError::Storage("source device identity path has no parent".to_owned())
    })?;
    let app_root = legacy_parent.parent().ok_or_else(|| {
        PeerSyncError::Storage("source device identity path has no app root".to_owned())
    })?;
    canonical_source_device_id(app_root)
}

fn now_millis() -> Result<i64, PeerSyncError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| PeerSyncError::Storage(error.to_string()))?
        .as_millis();
    i64::try_from(millis)
        .map_err(|_| PeerSyncError::Storage("system time exceeds SQLite integer range".to_owned()))
}

fn store_error(error: StoreError) -> PeerSyncError {
    PeerSyncError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        asset_repository::job_pins::{collect_durable_cas_job_roots, CasObjectRole},
        local_backup::AtomicCancellation,
        peer_sync::{
            device_registry::{DevicePermissions, IncomingSource, IncomingSourceRegistry},
            logical_delta::{
                build_logical_manifest, BuiltLogicalManifest, LogicalManifest,
                LogicalManifestBuilderInput, LogicalRecordEnvelope, LogicalRecordLocator,
                ProjectedLogicalRecord,
            },
            LogicalDeltaObjectSource,
        },
        persistent_store::WorkingSetCommit,
    };
    use serde_json::json;
    use std::{
        io::{Cursor, Read},
        net::SocketAddr,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc, Arc, Barrier,
        },
        thread,
        time::Duration,
    };

    struct CancelAfterFirstReadSource {
        objects: BTreeMap<String, Vec<u8>>,
        cancelled: Arc<AtomicBool>,
        reads: usize,
    }

    impl LogicalDeltaObjectSource for CancelAfterFirstReadSource {
        fn open_object(
            &mut self,
            object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            self.reads += 1;
            let bytes = self.objects.get(&object.hash).cloned().unwrap();
            Ok(Box::new(CancelAfterFirstReadCursor {
                inner: Cursor::new(bytes),
                cancelled: Arc::clone(&self.cancelled),
                first: true,
            }))
        }
    }

    struct CancelAfterFirstReadCursor {
        inner: Cursor<Vec<u8>>,
        cancelled: Arc<AtomicBool>,
        first: bool,
    }

    const ACCOUNTING_SOURCE_ID: &str = "00000000-0000-4000-8000-000000000024";

    fn register_accounting_source(root: &Path, total_bytes: u64, last_seen_ms: u64) {
        let mut registry = IncomingSourceRegistry::load(root).unwrap();
        registry
            .upsert(IncomingSource {
                device_id: ACCOUNTING_SOURCE_ID.to_owned(),
                name: "accounting source".to_owned(),
                endpoint: "http://192.168.0.24:32145".to_owned(),
                bearer: "a".repeat(64),
                permissions: DevicePermissions::read(),
                last_seen_ms,
                total_bytes,
            })
            .unwrap();
        registry.save().unwrap();
    }

    fn accounting_source(root: &Path) -> IncomingSource {
        IncomingSourceRegistry::load(root).unwrap().sources()[0].clone()
    }

    impl Read for CancelAfterFirstReadCursor {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(output)?;
            if self.first && read > 0 {
                self.first = false;
                self.cancelled.store(true, Ordering::SeqCst);
            }
            Ok(read)
        }
    }

    struct FakeDeltaTunnelLauncher(Arc<Mutex<FakeDeltaTunnelState>>);

    struct FakeDeltaTunnelState {
        endpoint: url::Url,
        lifecycle: DeltaTunnelLifecycle,
        stop_failures: usize,
        stop_calls: usize,
        seen_origin: Option<SocketAddr>,
        seen_named_token: Option<String>,
        stop_pause: Option<Arc<Barrier>>,
        start_pause: Option<Arc<Barrier>>,
        lifecycle_pause: Option<Arc<Barrier>>,
    }

    impl Default for FakeDeltaTunnelState {
        fn default() -> Self {
            Self {
                endpoint: url::Url::parse("https://quick-id.trycloudflare.com").unwrap(),
                lifecycle: DeltaTunnelLifecycle::Running,
                stop_failures: 0,
                stop_calls: 0,
                seen_origin: None,
                seen_named_token: None,
                stop_pause: None,
                start_pause: None,
                lifecycle_pause: None,
            }
        }
    }

    struct FakeDeltaTunnel {
        host: Option<LanCloneHost>,
        state: Arc<Mutex<FakeDeltaTunnelState>>,
    }

    impl DeltaTunnel for FakeDeltaTunnel {
        fn transport_url(&self) -> url::Url {
            self.state.lock().unwrap().endpoint.clone()
        }

        fn lifecycle(&mut self) -> Result<DeltaTunnelLifecycle, String> {
            let (lifecycle, pause) = {
                let state = self.state.lock().unwrap();
                (state.lifecycle, state.lifecycle_pause.clone())
            };
            if let Some(pause) = pause {
                pause.wait();
                pause.wait();
            }
            if lifecycle == DeltaTunnelLifecycle::Stopped {
                if let Some(host) = self.host.as_mut() {
                    host.stop().map_err(|error| error.to_string())?;
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
                    return Err("injected stop failure".to_owned());
                }
                state.stop_pause.clone()
            };
            if let Some(pause) = pause {
                pause.wait();
                pause.wait();
            }
            if let Some(host) = self.host.as_mut() {
                host.stop().map_err(|error| error.to_string())?;
            }
            self.host = None;
            self.state.lock().unwrap().lifecycle = DeltaTunnelLifecycle::Stopped;
            Ok(())
        }
    }

    impl DeltaTunnelLauncher for FakeDeltaTunnelLauncher {
        fn start(
            &self,
            request: PeerDeltaTunnelStart,
            host: LanCloneHost,
        ) -> Result<Box<dyn DeltaTunnel>, Box<dyn FailedDeltaTunnel>> {
            let pause = {
                let mut state = self.0.lock().unwrap();
                state.seen_origin = host.address();
                if let PeerDeltaTunnelStart::Named { token, .. } = request {
                    state.seen_named_token = Some(token);
                }
                state.start_pause.clone()
            };
            if let Some(pause) = pause {
                pause.wait();
                pause.wait();
            }
            Ok(Box::new(FakeDeltaTunnel {
                host: Some(host),
                state: Arc::clone(&self.0),
            }))
        }
    }

    fn prepared_delta_state(
        root: &Path,
        launcher: Arc<dyn DeltaTunnelLauncher>,
    ) -> (PeerDeltaCommandState, String) {
        let (session, manifest_bytes) = prepared_delta_source(root);
        let state = PeerDeltaCommandState::with_tunnel_launcher(launcher);
        let status = state
            .install_source(
                session,
                "00000000-0000-4000-8000-000000000001",
                manifest_bytes,
            )
            .unwrap();
        (state, status.session_id.unwrap())
    }

    fn prepared_delta_source(root: &Path) -> (LogicalDeltaSourceSession, Vec<u8>) {
        let cas = PayloadCas::new(root).unwrap();
        let mut store = PersistentStore::open(root).unwrap();
        let built = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let session = LogicalDeltaSourceSession::open_owned(
            store.open_native_job_store().unwrap(),
            root,
            &built.manifest.library_id,
            &built.manifest.generation,
            P4_SOURCE_PIN_PREFIX,
        )
        .unwrap();
        (session, built.manifest_bytes)
    }

    #[test]
    fn android_full_source_release_drops_lifecycle_gate_before_real_registry_callback() {
        let _registry_guard = test_registry_guard();
        let root = tempfile::tempdir().unwrap();
        let (session, manifest_bytes) = prepared_delta_source(root.path());
        let state = PeerDeltaCommandState::default();
        let status = state
            .install_source(
                session,
                "00000000-0000-4000-8000-000000000001",
                manifest_bytes,
            )
            .unwrap();
        let session_id = status.session_id.unwrap();
        let foreground = registry().reserve(AndroidForegroundLane::P4Source).unwrap();
        assert!(registry().attach_exact(&foreground));
        state
            .attach_source_foreground(&session_id, foreground.clone())
            .unwrap();
        let callback_state = state.clone();
        let callback_key = foreground.clone();
        assert!(
            registry().set_source_stop_callback_exact(&foreground, move || {
                callback_state.pause_source_exact(&callback_key);
            })
        );
        let release_state = state.clone();
        let (released_tx, released_rx) = mpsc::channel();

        let release = thread::spawn(move || {
            let result = release_state.release_source_android(&session_id);
            released_tx.send(result).unwrap();
        });

        assert_eq!(
            released_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("full release deadlocked in foreground callback")
                .unwrap(),
            Some(foreground.clone()),
        );
        release.join().unwrap();
        assert_eq!(state.status().unwrap().phase, PeerDeltaSourcePhase::Stopped);
        assert!(registry().acquire_exact(&foreground).is_none());
        let next = registry().reserve(AndroidForegroundLane::P4Target).unwrap();
        assert!(registry().detach_if_generation(&next));

        let (fresh_session, fresh_manifest) = prepared_delta_source(root.path());
        let prepared = state
            .install_source(
                fresh_session,
                "00000000-0000-4000-8000-000000000001",
                fresh_manifest,
            )
            .unwrap();
        assert_eq!(prepared.phase, PeerDeltaSourcePhase::Prepared);
        state
            .release_source_android(prepared.session_id.as_deref().unwrap())
            .unwrap();
    }

    #[test]
    fn android_target_foreground_release_is_native_owned_and_generation_exact() {
        let _registry_guard = test_registry_guard();
        let state = PeerDeltaCommandState::default();
        let foreground = state.reserve_target_foreground().unwrap();
        assert!(registry().attach_exact(&foreground));
        let mut stale = foreground.clone();
        stale.generation -= 1;
        assert_eq!(
            state
                .mark_target_running_exact(&stale)
                .unwrap_err()
                .to_string(),
            r#"Protocol("Android peer delta target foreground identity is stale")"#,
        );
        assert_eq!(
            state.target_foreground_status().unwrap().unwrap().phase,
            AndroidTargetForegroundPhase::Reserved,
        );
        state.mark_target_running_exact(&foreground).unwrap();
        assert!(!registry().detach_if_generation(&foreground));
        assert!(state.cancel_target_foreground_exact(&foreground).unwrap());
        assert!(!state.release_target_foreground_exact(&foreground).unwrap());
        state
            .publish_target_terminal_exact(
                &foreground,
                Ok(PeerDeltaPullResult::NoChanges {
                    revision: 7,
                    transferred_objects: 0,
                    transferred_bytes: 0,
                }),
            )
            .unwrap();
        let terminal = state.target_foreground_status().unwrap().unwrap();
        assert_eq!(terminal.phase, AndroidTargetForegroundPhase::Terminal);
        assert!(matches!(
            terminal.result,
            Some(PeerDeltaPullResult::NoChanges { revision: 7, .. })
        ));
        assert!(!state.release_target_foreground_exact(&stale).unwrap());
        assert!(state.release_target_foreground_exact(&foreground).unwrap());
        assert!(state.release_target_foreground_exact(&foreground).unwrap());
        assert!(state.target_foreground_status().unwrap().is_none());

        let fresh = state.reserve_target_foreground().unwrap();
        assert!(fresh.generation > foreground.generation);
        assert!(!registry().detach_if_generation(&foreground));
        assert!(state.release_target_foreground_exact(&fresh).unwrap());
    }

    #[test]
    fn committed_target_stays_running_during_terminal_publication_pause() {
        let _registry_guard = test_registry_guard();
        let state = PeerDeltaCommandState::default();
        let foreground = state.reserve_target_foreground().unwrap();
        assert!(registry().attach_exact(&foreground));
        state.mark_target_running_exact(&foreground).unwrap();
        let (committed_tx, committed_rx) = mpsc::channel();
        let (publish_tx, publish_rx) = mpsc::channel();
        let operation_state = state.clone();
        let operation_foreground = foreground.clone();

        let operation = thread::spawn(move || {
            let committed = PeerDeltaPullResult::Updated {
                revision: 12,
                transferred_objects: 1,
                transferred_bytes: 32,
            };
            committed_tx.send(()).unwrap();
            publish_rx.recv().unwrap();
            operation_state
                .publish_target_terminal_exact(&operation_foreground, Ok(committed))
                .unwrap();
        });

        committed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            state.target_foreground_status().unwrap().unwrap().phase,
            AndroidTargetForegroundPhase::Running,
        );
        assert!(state.cancel_target_foreground_exact(&foreground).unwrap());
        assert!(!state.release_target_foreground_exact(&foreground).unwrap());
        assert!(!registry().detach_if_generation(&foreground));
        publish_tx.send(()).unwrap();
        operation.join().unwrap();
        let terminal = state.target_foreground_status().unwrap().unwrap();
        assert!(matches!(
            terminal.result,
            Some(PeerDeltaPullResult::Updated { revision: 12, .. })
        ));
        assert!(state.release_target_foreground_exact(&foreground).unwrap());
    }

    #[test]
    fn target_precommit_failure_is_terminal_and_releases_without_a_result() {
        let _registry_guard = test_registry_guard();
        let state = PeerDeltaCommandState::default();
        let foreground = state.reserve_target_foreground().unwrap();
        assert!(registry().attach_exact(&foreground));
        state.mark_target_running_exact(&foreground).unwrap();
        state
            .publish_target_terminal_exact(
                &foreground,
                Err("cancelled before activation".to_owned()),
            )
            .unwrap();

        let terminal = state.target_foreground_status().unwrap().unwrap();
        assert_eq!(terminal.phase, AndroidTargetForegroundPhase::Terminal);
        assert_eq!(terminal.result, None);
        assert_eq!(
            terminal.error.as_deref(),
            Some("cancelled before activation")
        );
        assert!(state.release_target_foreground_exact(&foreground).unwrap());
    }

    struct FixtureSource {
        objects: BTreeMap<String, Vec<u8>>,
        reads: usize,
    }

    struct BlockingFixtureSource {
        objects: BTreeMap<String, Vec<u8>>,
        opened: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl LogicalDeltaObjectSource for BlockingFixtureSource {
        fn open_object(
            &mut self,
            object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            self.opened
                .send(())
                .map_err(|error| PeerSyncError::Transport(error.to_string()))?;
            self.release
                .recv()
                .map_err(|error| PeerSyncError::Transport(error.to_string()))?;
            let bytes = self.objects.get(&object.hash).ok_or_else(|| {
                PeerSyncError::Transport(format!("fixture object {} is absent", object.hash))
            })?;
            Ok(Box::new(Cursor::new(bytes.clone())))
        }
    }

    impl LogicalDeltaObjectSource for FixtureSource {
        fn open_object(
            &mut self,
            object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            self.reads += 1;
            let bytes = self.objects.get(&object.hash).ok_or_else(|| {
                PeerSyncError::Transport(format!("fixture object {} is absent", object.hash))
            })?;
            Ok(Box::new(Cursor::new(bytes.clone())))
        }
    }

    fn empty_source() -> FixtureSource {
        FixtureSource {
            objects: BTreeMap::new(),
            reads: 0,
        }
    }

    fn remote_root_manifest(
        base: &LogicalManifest,
        generation: &str,
        value: serde_json::Value,
    ) -> BuiltLogicalManifest {
        build_logical_manifest(LogicalManifestBuilderInput {
            library_id: base.library_id.clone(),
            generation: generation.to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: Some(base.generation.clone()),
            source_revision: 1,
            records: vec![ProjectedLogicalRecord::live(
                LogicalRecordLocator::Root,
                LogicalRecordEnvelope::Root {
                    value,
                    owner_heads: vec![],
                },
                vec![],
            )],
        })
        .unwrap()
    }

    #[test]
    fn source_device_identity_is_stable_and_outside_the_persistent_store() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("peer-delta").join(SOURCE_DEVICE_ID_FILE);

        let first = load_or_create_source_device_id(&path).unwrap();
        let second = load_or_create_source_device_id(&path).unwrap();

        assert_eq!(first, second);
        assert_eq!(uuid::Uuid::parse_str(&first).unwrap().to_string(), first);
        assert_eq!(
            fs::read_to_string(root.path().join("peer-sync").join("device-id")).unwrap(),
            first
        );
        assert!(!path.exists());
    }

    #[test]
    fn delta_prepare_stop_prepare_keeps_the_canonical_v2_source_identity() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let canonical =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let legacy = source_root
            .path()
            .join("peer-delta")
            .join(SOURCE_DEVICE_ID_FILE);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "00000000-0000-4000-8000-000000000099").unwrap();
        fs::remove_file(&legacy).unwrap();

        let state = PeerDeltaCommandState::default();
        for _ in 0..2 {
            let source_device_id = canonical_source_device_id(source_root.path()).unwrap();
            assert_eq!(source_device_id, canonical);
            let (session, manifest_bytes) = prepared_delta_source(source_root.path());
            let prepared = state
                .install_source(session, &source_device_id, manifest_bytes)
                .unwrap();
            state
                .configure_v2_registry(
                    source_root.path(),
                    "Windows",
                    super::super::device_registry::DevicePermissions::read(),
                )
                .unwrap();
            let running = state
                .start_source(prepared.session_id.as_deref().unwrap(), Ipv4Addr::LOCALHOST)
                .unwrap();
            let pairing = url::Url::parse(running.pairing_uri.as_deref().unwrap()).unwrap();
            let endpoint = pairing
                .query_pairs()
                .find(|(key, _)| key == "endpoint")
                .unwrap()
                .1
                .into_owned();
            let manifest_id = pairing
                .query_pairs()
                .find(|(key, _)| key == "manifest")
                .unwrap()
                .1
                .into_owned();
            let claim = pairing.fragment().unwrap().strip_prefix("claim=").unwrap();
            let client = super::super::lan::LanLogicalDeltaClient::claim_v2_and_register(
                target_root.path(),
                "Android",
                &endpoint,
                prepared.session_id.as_deref().unwrap(),
                &manifest_id,
                claim,
            )
            .unwrap();
            assert_eq!(client.hello().unwrap().device_id, canonical);
            state
                .stop_source(prepared.session_id.as_deref().unwrap())
                .unwrap();
        }
        assert!(!legacy.exists());
    }

    #[test]
    fn invalid_persisted_source_device_identity_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SOURCE_DEVICE_ID_FILE);
        fs::write(&path, b"not-a-device-id\n").unwrap();

        assert!(matches!(
            load_or_create_source_device_id(&path),
            Err(PeerSyncError::Storage(_))
        ));
    }

    #[test]
    fn delta_pairing_uses_a_dedicated_scheme_without_changing_clone_links() {
        let pairing = super::super::LanPairing {
            session_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            manifest_id: "1".repeat(64),
            claim: "2".repeat(64),
            permissions: None,
        };

        let uri = build_pairing_uri("http://192.168.1.8:1234", &pairing).unwrap();

        assert!(uri.starts_with("risuailocal://peer-delta/v1?"));
        assert!(uri.ends_with(&format!("#claim={}", pairing.claim)));
    }

    #[test]
    fn quick_and_named_tunnels_use_loopback_and_publish_canonical_https() {
        for (request, expected_kind) in [
            (PeerDeltaTunnelStart::Quick, PeerDeltaTunnelKind::Quick),
            (
                PeerDeltaTunnelStart::Named {
                    token: "named-secret-token".to_owned(),
                    expected_public_base_url: "https://sync.example.com".to_owned(),
                },
                PeerDeltaTunnelKind::Named,
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let fake = Arc::new(Mutex::new(FakeDeltaTunnelState {
                endpoint: url::Url::parse("https://sync.example.com").unwrap(),
                ..Default::default()
            }));
            let (state, session_id) = prepared_delta_state(
                root.path(),
                Arc::new(FakeDeltaTunnelLauncher(Arc::clone(&fake))),
            );

            let status = state.start_tunnel(&session_id, request).unwrap();

            assert_eq!(status.phase, PeerDeltaSourcePhase::Running);
            assert_eq!(status.tunnel.unwrap().kind, expected_kind);
            assert!(status
                .pairing_uri
                .unwrap()
                .contains("endpoint=https%3A%2F%2Fsync.example.com"));
            assert_eq!(
                fake.lock().unwrap().seen_origin.unwrap().ip(),
                Ipv4Addr::LOCALHOST
            );
            state.stop_source(&session_id).unwrap();
        }
    }

    #[test]
    fn malformed_tunnel_endpoint_is_stopped_without_publishing_a_pairing() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(Mutex::new(FakeDeltaTunnelState {
            endpoint: url::Url::parse("https://sync.example.com/not-bare").unwrap(),
            ..Default::default()
        }));
        let (state, session_id) = prepared_delta_state(
            root.path(),
            Arc::new(FakeDeltaTunnelLauncher(Arc::clone(&fake))),
        );

        assert!(state
            .start_tunnel(&session_id, PeerDeltaTunnelStart::Quick)
            .is_err());
        assert_eq!(fake.lock().unwrap().stop_calls, 1);
        assert_eq!(state.status().unwrap().phase, PeerDeltaSourcePhase::Stopped);
    }

    #[test]
    fn natural_exit_and_failed_stop_keep_exact_cleanup_owner_for_retry() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(Mutex::new(FakeDeltaTunnelState {
            stop_failures: 1,
            ..Default::default()
        }));
        let (state, session_id) = prepared_delta_state(
            root.path(),
            Arc::new(FakeDeltaTunnelLauncher(Arc::clone(&fake))),
        );
        state
            .start_tunnel(&session_id, PeerDeltaTunnelStart::Quick)
            .unwrap();

        assert!(state.stop_source(&session_id).is_err());
        assert_eq!(
            state.status().unwrap().phase,
            PeerDeltaSourcePhase::Stopping
        );
        state.stop_source(&session_id).unwrap();
        assert_eq!(fake.lock().unwrap().stop_calls, 2);
        assert_eq!(state.status().unwrap().phase, PeerDeltaSourcePhase::Stopped);

        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(Mutex::new(FakeDeltaTunnelState::default()));
        let (state, session_id) = prepared_delta_state(
            root.path(),
            Arc::new(FakeDeltaTunnelLauncher(Arc::clone(&fake))),
        );
        state
            .start_tunnel(&session_id, PeerDeltaTunnelStart::Quick)
            .unwrap();
        fake.lock().unwrap().lifecycle = DeltaTunnelLifecycle::Stopped;
        assert_eq!(state.status().unwrap().phase, PeerDeltaSourcePhase::Stopped);
        assert!(state.revoke(&session_id, "wrong-device").is_err());
    }

    #[test]
    fn stop_and_final_exit_wait_for_tunnel_start_owner() {
        for final_exit in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let pause = Arc::new(Barrier::new(2));
            let fake = Arc::new(Mutex::new(FakeDeltaTunnelState {
                start_pause: Some(Arc::clone(&pause)),
                ..Default::default()
            }));
            let (state, session_id) = prepared_delta_state(
                root.path(),
                Arc::new(FakeDeltaTunnelLauncher(Arc::clone(&fake))),
            );
            let start_state = state.clone();
            let start_session = session_id.clone();
            let start = thread::spawn(move || {
                start_state.start_tunnel(&start_session, PeerDeltaTunnelStart::Quick)
            });
            pause.wait();

            let (done_tx, done_rx) = mpsc::channel();
            let cleanup_state = state.clone();
            let cleanup_session = session_id.clone();
            thread::spawn(move || {
                let result = if final_exit {
                    cleanup_state.shutdown_for_exit();
                    Ok(())
                } else {
                    cleanup_state.stop_source(&cleanup_session)
                };
                done_tx.send(result).unwrap();
            });
            let early_cleanup = done_rx.recv_timeout(Duration::from_millis(100));
            let cleanup_blocked = matches!(&early_cleanup, Err(mpsc::RecvTimeoutError::Timeout));
            pause.wait();
            let start_result = start.join().unwrap();
            let cleanup_result = match early_cleanup {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    done_rx.recv_timeout(Duration::from_secs(2)).unwrap()
                }
                Err(error) => panic!("cleanup channel failed: {error}"),
            };
            assert!(cleanup_blocked);
            start_result.unwrap();
            cleanup_result.unwrap();
            assert_eq!(state.status().unwrap().phase, PeerDeltaSourcePhase::Stopped);
        }
    }

    #[test]
    fn stop_waits_for_status_probe_to_restore_the_exact_tunnel_owner() {
        let root = tempfile::tempdir().unwrap();
        let pause = Arc::new(Barrier::new(2));
        let fake = Arc::new(Mutex::new(FakeDeltaTunnelState {
            lifecycle_pause: Some(Arc::clone(&pause)),
            ..Default::default()
        }));
        let (state, session_id) = prepared_delta_state(
            root.path(),
            Arc::new(FakeDeltaTunnelLauncher(Arc::clone(&fake))),
        );
        state
            .start_tunnel(&session_id, PeerDeltaTunnelStart::Quick)
            .unwrap();
        let status_state = state.clone();
        let status = thread::spawn(move || status_state.status());
        pause.wait();

        let (done_tx, done_rx) = mpsc::channel();
        let stop_state = state.clone();
        let stop_session = session_id.clone();
        thread::spawn(move || done_tx.send(stop_state.stop_source(&stop_session)).unwrap());
        let early_stop = done_rx.recv_timeout(Duration::from_millis(100));
        let stop_blocked = matches!(&early_stop, Err(mpsc::RecvTimeoutError::Timeout));
        pause.wait();
        let status_result = status.join().unwrap();
        let stop_result = match early_stop {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                done_rx.recv_timeout(Duration::from_secs(2)).unwrap()
            }
            Err(error) => panic!("stop channel failed: {error}"),
        };
        assert!(stop_blocked);
        assert_eq!(status_result.unwrap().phase, PeerDeltaSourcePhase::Running);
        stop_result.unwrap();
        assert_eq!(fake.lock().unwrap().stop_calls, 1);
        assert_eq!(state.status().unwrap().phase, PeerDeltaSourcePhase::Stopped);
    }

    #[test]
    fn new_prepare_waits_for_status_probe_and_never_receives_the_old_tunnel() {
        let root = tempfile::tempdir().unwrap();
        let replacement_root = tempfile::tempdir().unwrap();
        let (replacement_session, replacement_manifest) =
            prepared_delta_source(replacement_root.path());
        let pause = Arc::new(Barrier::new(2));
        let fake = Arc::new(Mutex::new(FakeDeltaTunnelState {
            lifecycle_pause: Some(Arc::clone(&pause)),
            ..Default::default()
        }));
        let (state, session_id) = prepared_delta_state(
            root.path(),
            Arc::new(FakeDeltaTunnelLauncher(Arc::clone(&fake))),
        );
        state
            .start_tunnel(&session_id, PeerDeltaTunnelStart::Quick)
            .unwrap();
        let status_state = state.clone();
        let status = thread::spawn(move || status_state.status());
        pause.wait();

        let (prepared_tx, prepared_rx) = mpsc::channel();
        let prepare_state = state.clone();
        thread::spawn(move || {
            prepared_tx
                .send(prepare_state.install_source(
                    replacement_session,
                    "00000000-0000-4000-8000-000000000002",
                    replacement_manifest,
                ))
                .unwrap();
        });
        let early_prepare = prepared_rx.recv_timeout(Duration::from_millis(100));
        let prepare_blocked = matches!(&early_prepare, Err(mpsc::RecvTimeoutError::Timeout));
        pause.wait();
        status.join().unwrap().unwrap();
        let replacement = match early_prepare {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                prepared_rx.recv_timeout(Duration::from_secs(2)).unwrap()
            }
            Err(error) => panic!("prepare channel failed: {error}"),
        };
        assert!(prepare_blocked);
        let replacement = replacement.unwrap();
        let runtime = state.lock().unwrap();
        let source = runtime.source.as_ref().unwrap();
        assert_eq!(source.session_id, replacement.session_id.unwrap());
        assert!(source.tunnel.is_none());
        assert_eq!(source.phase, PeerDeltaSourcePhase::Prepared);
    }

    #[test]
    fn stop_and_final_exit_release_the_owned_p4_source_pin() {
        for final_exit in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let fake = Arc::new(Mutex::new(FakeDeltaTunnelState::default()));
            let (state, session_id) = prepared_delta_state(
                root.path(),
                Arc::new(FakeDeltaTunnelLauncher(Arc::clone(&fake))),
            );
            state
                .start_tunnel(&session_id, PeerDeltaTunnelStart::Quick)
                .unwrap();

            if final_exit {
                state.shutdown_for_exit();
            } else {
                state.stop_source(&session_id).unwrap();
            }

            let mut reopened = PersistentStore::open(root.path()).unwrap();
            assert_eq!(
                reopened
                    .reclaim_logical_generation_pins(P4_SOURCE_PIN_PREFIX)
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn revocation_is_scoped_to_the_exact_source_session_and_device() {
        let root = tempfile::tempdir().unwrap();
        let (state, session_id) = prepared_delta_state(
            root.path(),
            Arc::new(FakeDeltaTunnelLauncher(Arc::new(Mutex::new(
                FakeDeltaTunnelState::default(),
            )))),
        );
        let status = state
            .start_source(&session_id, Ipv4Addr::LOCALHOST)
            .unwrap();
        let uri = url::Url::parse(status.pairing_uri.as_deref().unwrap()).unwrap();
        let endpoint = uri
            .query_pairs()
            .find(|(key, _)| key == "endpoint")
            .unwrap()
            .1;
        let claim = uri.fragment().unwrap().strip_prefix("claim=").unwrap();
        let _client = LanLogicalDeltaClient::claim(
            &endpoint,
            &session_id,
            status.manifest_id.as_deref().unwrap(),
            claim,
        )
        .unwrap();
        let device_id = state.status().unwrap().devices[0].device_id.clone();

        assert!(state
            .revoke("00000000-0000-4000-8000-000000000099", &device_id)
            .is_err());
        assert!(state
            .revoke(&session_id, "00000000-0000-4000-8000-000000000099")
            .is_err());
        state.revoke(&session_id, &device_id).unwrap();
        assert!(state.status().unwrap().devices[0].revoked);
    }

    #[test]
    fn typed_merge_conflict_projects_to_the_product_conflict_result() {
        assert_eq!(
            classify_plan_error(PeerSyncError::LogicalMergeConflict {
                record: "plugin:shared".to_owned(),
            })
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "localAndRemoteChanged",
            }
        );
    }

    #[test]
    fn planning_revision_cas_conflict_projects_as_stale_revision() {
        assert_eq!(
            classify_plan_error(PeerSyncError::ActivationConflict {
                expected: Some("7".to_owned()),
                actual: Some("8".to_owned()),
            })
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );
    }

    #[test]
    fn planning_base_cas_conflict_projects_as_local_and_remote_changed() {
        assert_eq!(
            classify_plan_error(PeerSyncError::ActivationConflict {
                expected: Some("a".repeat(64)),
                actual: Some("c".repeat(64)),
            })
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "localAndRemoteChanged",
            }
        );
    }

    #[test]
    fn changed_retry_revision_cas_conflict_projects_as_stale_revision() {
        assert_eq!(
            classify_plan_error(PeerSyncError::ActivationConflict {
                expected: Some("8".to_owned()),
                actual: Some("9".to_owned()),
            })
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );
    }

    #[test]
    fn bootstrap_revision_conflict_releases_its_sealed_durable_roots() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let job = RefCell::new(
            DurableCasJob::begin(
                directory.path(),
                "bootstrap-conflict",
                CasJobKind::LogicalDeltaTarget,
                0,
            )
            .unwrap(),
        );
        job.borrow_mut()
            .prepare_bytes(&cas, b"bootstrap manifest", CasObjectRole::DirectObject)
            .unwrap();
        job.borrow_mut().seal(&mut store, 0).unwrap();
        assert!(!collect_durable_cas_job_roots(directory.path())
            .object_hashes
            .is_empty());

        assert_eq!(
            finish_bootstrap(
                &job,
                7,
                Err(PeerSyncError::ActivationConflict {
                    expected: Some("7".to_owned()),
                    actual: Some("8".to_owned()),
                }),
            )
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
        assert_eq!(store.revision().unwrap(), 0);
    }

    #[test]
    fn sealed_target_error_releases_roots_after_a_successful_durable_abort() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let job = RefCell::new(
            DurableCasJob::begin(
                directory.path(),
                "p4-delta-target-00000000-0000-4000-8000-000000000006",
                CasJobKind::LogicalDeltaTarget,
                0,
            )
            .unwrap(),
        );
        job.borrow_mut()
            .prepare_bytes(&cas, b"aborted target", CasObjectRole::DirectObject)
            .unwrap();
        job.borrow_mut().seal(&mut store, 0).unwrap();

        let error = finish_pull(
            &job,
            &activation_conflict_plan(),
            Err(PeerSyncError::Storage("activation failed".to_owned())),
            0,
            0,
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains("activation failed"));
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
    }

    #[test]
    fn sealed_target_error_preserves_roots_when_durable_abort_cleanup_fails() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let job = RefCell::new(
            DurableCasJob::begin(
                directory.path(),
                "p4-delta-target-00000000-0000-4000-8000-000000000007",
                CasJobKind::LogicalDeltaTarget,
                0,
            )
            .unwrap(),
        );
        let prepared = job
            .borrow_mut()
            .prepare_bytes(&cas, b"retained target", CasObjectRole::DirectObject)
            .unwrap();
        job.borrow_mut().seal(&mut store, 0).unwrap();

        assert!(finish_pull(
            &job,
            &activation_conflict_plan(),
            Err(PeerSyncError::Storage(
                "activation and abort failed".to_owned()
            )),
            0,
            0,
            false,
        )
        .is_err());

        assert!(collect_durable_cas_job_roots(directory.path())
            .object_hashes
            .contains(&prepared.content_hash));
    }

    #[test]
    fn generic_bootstrap_error_releases_its_sealed_durable_roots() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let job = RefCell::new(
            DurableCasJob::begin(
                directory.path(),
                "p4-delta-target-00000000-0000-4000-8000-000000000008",
                CasJobKind::LogicalDeltaTarget,
                0,
            )
            .unwrap(),
        );
        job.borrow_mut()
            .prepare_bytes(&cas, b"bootstrap target", CasObjectRole::DirectObject)
            .unwrap();
        job.borrow_mut().seal(&mut store, 0).unwrap();

        assert!(finish_bootstrap(
            &job,
            0,
            Err(PeerSyncError::Storage("bootstrap failed".to_owned())),
        )
        .is_err());

        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
    }

    fn activation_conflict_plan() -> ReadyLogicalDeltaPlan {
        ReadyLogicalDeltaPlan {
            expected_local_revision: 7,
            expected_base_manifest_hash: "a".repeat(64),
            expected_remote_generation: "remote".to_owned(),
            apply: vec![],
            preserve_local_keys: vec![],
            candidate_object_hashes: vec![],
            next_base_manifest_hash: "b".repeat(64),
            next_base_generation_sequence: "8".to_owned(),
        }
    }

    #[test]
    fn coordinator_projects_revision_cas_conflict_as_stale_revision() {
        let plan = activation_conflict_plan();

        assert_eq!(
            classify_activation_conflict(&plan, 8, &plan.expected_base_manifest_hash),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );
    }

    #[test]
    fn coordinator_projects_base_cas_conflict_as_local_and_remote_changed() {
        let plan = activation_conflict_plan();

        assert_eq!(
            classify_activation_conflict(&plan, 7, &"c".repeat(64)),
            PeerDeltaPullResult::Conflict {
                reason: "localAndRemoteChanged",
            }
        );
    }

    #[test]
    fn coordinator_projects_matching_revision_and_base_conflict_as_stale_revision() {
        let plan = activation_conflict_plan();

        assert_eq!(
            classify_activation_conflict(
                &plan,
                plan.expected_local_revision,
                &plan.expected_base_manifest_hash,
            ),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );
    }

    #[test]
    fn coordinator_bootstraps_only_an_exact_peer_and_preserves_stale_or_divergent_state() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let local = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let mut source = empty_source();

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                "00000000-0000-4000-8000-000000000001",
                0,
                &local.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::NoChanges {
                revision: 0,
                transferred_objects: 0,
                transferred_bytes: 0,
            }
        );
        assert_eq!(source.reads, 0);
        assert_eq!(store.revision().unwrap(), 0);

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                "00000000-0000-4000-8000-000000000001",
                0,
                &local.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::NoChanges {
                revision: 0,
                transferred_objects: 0,
                transferred_bytes: 0,
            }
        );
        assert_eq!(source.reads, 0);
        assert_eq!(store.revision().unwrap(), 0);

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                "00000000-0000-4000-8000-000000000001",
                1,
                &local.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );
        assert_eq!(store.revision().unwrap(), 0);
        assert_eq!(source.reads, 0);

        let other = remote_root_manifest(&local.manifest, "remote-other", json!({"side":"remote"}));
        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                "00000000-0000-4000-8000-000000000002",
                0,
                &other.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::FullCloneRequired {
                reason: "noExactCommonBase",
            }
        );
        assert_eq!(store.revision().unwrap(), 0);
        assert_eq!(source.reads, 0);

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                "00000000-0000-4000-8000-000000000002",
                0,
                &other.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::FullCloneRequired {
                reason: "noExactCommonBase",
            }
        );
        assert_eq!(store.revision().unwrap(), 0);
        assert_eq!(source.reads, 0);
    }

    #[test]
    fn coordinator_fetches_only_changed_objects_and_activates_the_exact_revision() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let local = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer = "00000000-0000-4000-8000-000000000003";
        let mut source = empty_source();
        pull_logical_delta(
            &mut store,
            &cas,
            directory.path(),
            peer,
            0,
            &local.manifest_bytes,
            &mut source,
        )
        .unwrap();
        let remote =
            remote_root_manifest(&local.manifest, "remote-updated", json!({"side":"remote"}));
        source.objects = remote
            .record_objects
            .iter()
            .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
            .collect();

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                peer,
                0,
                &remote.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::Updated {
                revision: 1,
                transferred_objects: 1,
                transferred_bytes: remote.record_objects[0].object.size,
            }
        );
        assert_eq!(source.reads, 1);
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(
            store.read_root(None).unwrap().value,
            json!({"side":"remote"})
        );
    }

    #[test]
    fn delta_completion_accounting_counts_only_updated_and_no_changes_once() {
        let directory = tempfile::tempdir().unwrap();
        register_accounting_source(directory.path(), 40, 7);

        record_delta_completion(
            directory.path(),
            ACCOUNTING_SOURCE_ID,
            true,
            &PeerDeltaPullResult::Updated {
                revision: 1,
                transferred_objects: 1,
                transferred_bytes: 12,
            },
        );
        let after_updated = accounting_source(directory.path());
        assert_eq!(after_updated.total_bytes, 52);
        assert!(after_updated.last_seen_ms > 7);

        record_delta_completion(
            directory.path(),
            ACCOUNTING_SOURCE_ID,
            true,
            &PeerDeltaPullResult::NoChanges {
                revision: 1,
                transferred_objects: 0,
                transferred_bytes: 0,
            },
        );
        let after_no_changes = accounting_source(directory.path());
        assert_eq!(after_no_changes.total_bytes, 52);
        assert!(after_no_changes.last_seen_ms >= after_updated.last_seen_ms);

        for non_success in [
            PeerDeltaPullResult::Conflict {
                reason: "stale revision",
            },
            PeerDeltaPullResult::FullCloneRequired {
                reason: "missing common base",
            },
        ] {
            record_delta_completion(directory.path(), ACCOUNTING_SOURCE_ID, true, &non_success);
        }
        assert_eq!(accounting_source(directory.path()), after_no_changes);

        record_delta_completion(
            directory.path(),
            ACCOUNTING_SOURCE_ID,
            false,
            &PeerDeltaPullResult::Updated {
                revision: 2,
                transferred_objects: 1,
                transferred_bytes: 99,
            },
        );
        assert_eq!(accounting_source(directory.path()), after_no_changes);
    }

    #[test]
    fn delta_completion_accounting_overflow_preserves_the_successful_pull_result() {
        let directory = tempfile::tempdir().unwrap();
        register_accounting_source(directory.path(), u64::MAX, 7);
        let completed = PeerDeltaPullResult::Updated {
            revision: 1,
            transferred_objects: 1,
            transferred_bytes: 1,
        };
        let before = fs::read(directory.path().join("peer-sync/sources.json")).unwrap();

        record_delta_completion(directory.path(), ACCOUNTING_SOURCE_ID, true, &completed);

        assert!(matches!(completed, PeerDeltaPullResult::Updated { .. }));
        assert_eq!(
            fs::read(directory.path().join("peer-sync/sources.json")).unwrap(),
            before
        );
    }

    #[test]
    fn cancelled_target_cleans_only_its_unsealed_job_and_retries_with_a_fresh_job() {
        let directory = tempfile::tempdir().unwrap();
        register_accounting_source(directory.path(), 40, 7);
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let local = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer = "00000000-0000-4000-8000-000000000023";
        pull_logical_delta(
            &mut store,
            &cas,
            directory.path(),
            peer,
            0,
            &local.manifest_bytes,
            &mut empty_source(),
        )
        .unwrap();
        let remote =
            remote_root_manifest(&local.manifest, "remote-cancel", json!({"side":"remote"}));
        let unowned = directory
            .path()
            .join("peer-delta")
            .join("staging")
            .join("unowned");
        fs::create_dir_all(&unowned).unwrap();
        fs::write(unowned.join("keep"), b"unowned").unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = AtomicCancellation::new(Arc::clone(&cancelled));
        let objects = remote
            .record_objects
            .iter()
            .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut source = CancelAfterFirstReadSource {
            objects: objects.clone(),
            cancelled,
            reads: 0,
        };

        let error = pull_logical_delta_with_cancellation(
            &mut store,
            &cas,
            directory.path(),
            peer,
            0,
            &remote.manifest_bytes,
            &mut source,
            &cancellation,
        )
        .unwrap_err();

        assert_eq!(error, PeerSyncError::Cancelled);
        assert_eq!(accounting_source(directory.path()).total_bytes, 40);
        assert_eq!(accounting_source(directory.path()).last_seen_ms, 7);
        assert_eq!(source.reads, 1);
        assert_eq!(store.revision().unwrap(), 0);
        assert!(unowned.join("keep").is_file());
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );

        let mut retry = FixtureSource { objects, reads: 0 };
        assert!(matches!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                peer,
                0,
                &remote.manifest_bytes,
                &mut retry,
            )
            .unwrap(),
            PeerDeltaPullResult::Updated { revision: 1, .. }
        ));
        assert_eq!(retry.reads, 1);
        assert_eq!(store.revision().unwrap(), 1);
        assert!(unowned.join("keep").is_file());

        assert!(matches!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                peer,
                1,
                &remote.manifest_bytes,
                &mut empty_source(),
            )
            .unwrap(),
            PeerDeltaPullResult::NoChanges { revision: 1, .. }
        ));
    }

    #[test]
    fn dedicated_pull_connection_does_not_hold_the_managed_store_during_object_io() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut setup = PersistentStore::open(directory.path()).unwrap();
        let local = setup
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer = "00000000-0000-4000-8000-000000000013";
        pull_logical_delta(
            &mut setup,
            &cas,
            directory.path(),
            peer,
            0,
            &local.manifest_bytes,
            &mut empty_source(),
        )
        .unwrap();
        let remote = remote_root_manifest(&local.manifest, "remote-slow", json!({"side":"remote"}));
        let export_lease = setup.acquire_revision(0).unwrap();
        let exported = setup.export_risu_save(&export_lease.lease, true).unwrap();
        let exported_path = PathBuf::from(&exported.path);
        let ownership_path = exported_path.with_extension("lease");
        let managed_store = Arc::new(Mutex::new(setup));
        let mut pull_store = {
            let managed = managed_store.lock().unwrap();
            open_peer_delta_store(&managed).unwrap()
        };
        assert!(exported_path.is_file());
        assert!(ownership_path.is_file());
        let (opened_tx, opened_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let root = directory.path().to_path_buf();
        let remote_bytes = remote.manifest_bytes.clone();
        let source = BlockingFixtureSource {
            objects: remote
                .record_objects
                .iter()
                .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
                .collect(),
            opened: opened_tx,
            release: release_rx,
        };
        let pull = thread::spawn(move || {
            let mut source = source;
            let cas = PayloadCas::new(&root).unwrap();
            pull_logical_delta(
                &mut pull_store,
                &cas,
                &root,
                peer,
                0,
                &remote_bytes,
                &mut source,
            )
        });
        opened_rx.recv().unwrap();

        assert_eq!(
            managed_store
                .try_lock()
                .unwrap()
                .read_root(None)
                .unwrap()
                .value,
            json!({})
        );
        assert!(exported_path.is_file());
        assert!(ownership_path.is_file());
        release_tx.send(()).unwrap();
        assert_eq!(
            pull.join().unwrap().unwrap(),
            PeerDeltaPullResult::Updated {
                revision: 1,
                transferred_objects: 1,
                transferred_bytes: remote.record_objects[0].object.size,
            }
        );
    }

    #[test]
    fn coordinator_returns_a_structured_same_record_conflict_without_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer = "00000000-0000-4000-8000-000000000004";
        let mut source = empty_source();
        pull_logical_delta(
            &mut store,
            &cas,
            directory.path(),
            peer,
            0,
            &base.manifest_bytes,
            &mut source,
        )
        .unwrap();
        store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"side":"local"})),
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
        let remote =
            remote_root_manifest(&base.manifest, "remote-conflict", json!({"side":"remote"}));

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                peer,
                1,
                &remote.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "localAndRemoteChanged",
            }
        );
        assert_eq!(source.reads, 0);
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(
            store.read_root(None).unwrap().value,
            json!({"side":"local"})
        );
        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                peer,
                1,
                &remote.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "localAndRemoteChanged",
            }
        );
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(source.reads, 0);
    }
}
