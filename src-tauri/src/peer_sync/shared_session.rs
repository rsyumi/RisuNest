//! Shared raw-listener source facade.  It owns prepared lane sessions but
//! deliberately delegates every transfer request to `lan`'s existing handler.

use super::{
    device_registry::DevicePermissions,
    lan::{
        LanCloneHost, LanPairing, PreparedBidirectionalLogicalLanSession, PreparedLogicalLanSession,
    },
    PeerSyncError, PreparedCloneSession,
};
use std::net::{Ipv4Addr, SocketAddr};
#[cfg(any(desktop, target_os = "android", test))]
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[cfg(any(desktop, target_os = "android", test))]
use super::{
    bidirectional_commands::{
        prepare_shared_bidirectional_source, PreparedSharedBidirectionalSource,
    },
    delta_commands::{prepare_shared_delta_source, PreparedSharedDeltaSource},
    production::{prepare_unified_clone_source, PreparedUnifiedCloneSource},
};
#[cfg(desktop)]
use crate::local_backup::NeverCancelled;
#[cfg(any(desktop, target_os = "android", test))]
use crate::{
    asset_repository::PayloadCas,
    local_backup::CancellationProbe,
    persistent_store::{self, PersistentStore, StoreError},
};
#[cfg(any(desktop, target_os = "android"))]
use tauri::{AppHandle, Manager, State};

/// The three source preparations have a fixed dependency order.  A source
/// lane owns its preparation artifacts until the shared host has been torn
/// down, so cleanup is always attempted in reverse order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SharedSourceLane {
    Clone,
    Delta,
    Bidirectional,
}

impl SharedSourceLane {
    const PREPARATION_ORDER: [Self; 3] = [Self::Clone, Self::Delta, Self::Bidirectional];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SharedSessionPhase {
    Idle,
    Preparing,
    Prepared,
    Stopping,
}

/// Adapter seam for the real source-preparation engines.  It deliberately
/// models no listener or tunnel operation: building a host is the last
/// preparation step, and starting it belongs to the next slice.
pub(crate) trait SharedSourceOwnership {
    type Host;

    fn build_host(&mut self) -> Result<Self::Host, PeerSyncError>;
    fn cleanup(&mut self, lane: SharedSourceLane) -> Result<(), PeerSyncError>;
    fn stop_host(&mut self, host: &mut Self::Host) -> Result<(), PeerSyncError>;
}

pub(crate) trait SharedSourcePreparation<C>: SharedSourceOwnership {
    fn prepare(&mut self, lane: SharedSourceLane, context: &mut C) -> Result<(), PeerSyncError>;
}

struct SharedSessionRuntime<H> {
    phase: SharedSessionPhase,
    host: Option<H>,
    acquired: Vec<SharedSourceLane>,
}

/// Serialized, idempotent lifecycle around a shared source preparation.
///
/// The operation mutex covers preparation and cleanup rather than merely the
/// visible phase transition.  A concurrent caller therefore observes the
/// finished result of the first operation instead of a half-owned source.
pub(crate) struct SharedSessionLifecycle<P: SharedSourceOwnership> {
    operation: Mutex<()>,
    runtime: Mutex<SharedSessionRuntime<P::Host>>,
    preparation: Mutex<P>,
}

impl<P: SharedSourceOwnership> SharedSessionLifecycle<P> {
    pub(crate) fn new(preparation: P) -> Self {
        Self {
            operation: Mutex::new(()),
            runtime: Mutex::new(SharedSessionRuntime {
                phase: SharedSessionPhase::Idle,
                host: None,
                acquired: Vec::new(),
            }),
            preparation: Mutex::new(preparation),
        }
    }

    pub(crate) fn phase(&self) -> Result<SharedSessionPhase, PeerSyncError> {
        Ok(self.runtime()?.phase)
    }

    pub(crate) fn prepare<C>(&self, context: &mut C) -> Result<(), PeerSyncError>
    where
        P: SharedSourcePreparation<C>,
    {
        let _operation = self.operation()?;
        match self.phase()? {
            SharedSessionPhase::Prepared => return Ok(()),
            SharedSessionPhase::Stopping => {
                return Err(PeerSyncError::Protocol(
                    "shared source cleanup is pending; retry stop before preparing".to_owned(),
                ));
            }
            SharedSessionPhase::Idle | SharedSessionPhase::Preparing => {}
        }
        self.set_phase(SharedSessionPhase::Preparing)?;

        let mut acquired = Vec::new();
        let result = {
            let mut preparation = self.preparation()?;
            let mut result = Ok(());
            for lane in SharedSourceLane::PREPARATION_ORDER {
                if let Err(error) =
                    SharedSourcePreparation::prepare(&mut *preparation, lane, context)
                {
                    result = Err(error);
                    break;
                }
                acquired.push(lane);
            }
            if result.is_ok() {
                match preparation.build_host() {
                    Ok(host) => {
                        let mut runtime = self.runtime()?;
                        runtime.host = Some(host);
                        runtime.acquired = acquired.clone();
                    }
                    Err(error) => result = Err(error),
                }
            }
            if let Err(primary) = result {
                let (remaining, _) = Self::cleanup_lanes(&mut *preparation, &acquired);
                self.runtime()?.acquired = remaining;
                Err(primary)
            } else {
                Ok(())
            }
        };

        self.set_phase(if result.is_ok() {
            SharedSessionPhase::Prepared
        } else if self.runtime()?.acquired.is_empty() {
            SharedSessionPhase::Idle
        } else {
            SharedSessionPhase::Stopping
        })?;
        result
    }

    pub(crate) fn stop(&self) -> Result<(), PeerSyncError> {
        let _operation = self.operation()?;
        if self.phase()? == SharedSessionPhase::Idle {
            return Ok(());
        }
        self.set_phase(SharedSessionPhase::Stopping)?;

        let mut primary = None;
        let mut host = self.runtime()?.host.take();
        if let Some(host) = host.as_mut() {
            if let Err(error) = self.preparation()?.stop_host(host) {
                primary = Some(error);
            }
        }
        if primary.is_some() {
            self.runtime()?.host = host.take();
        }
        // A stopped host can be dropped before lane cleanup removes prepared
        // artifacts. A host that failed to stop remains owned for the retry.
        drop(host);
        {
            let mut preparation = self.preparation()?;
            let acquired = self.runtime()?.acquired.clone();
            let (remaining, cleanup_error) = Self::cleanup_lanes(&mut *preparation, &acquired);
            self.runtime()?.acquired = remaining;
            if primary.is_none() {
                primary = cleanup_error;
            }
        }
        let runtime = self.runtime()?;
        let has_remaining = runtime.host.is_some() || !runtime.acquired.is_empty();
        drop(runtime);
        self.set_phase(if has_remaining {
            SharedSessionPhase::Stopping
        } else {
            SharedSessionPhase::Idle
        })?;
        primary.map_or(Ok(()), Err)
    }

    pub(crate) fn with_host_mut<T>(
        &self,
        operation: impl FnOnce(&mut P::Host) -> T,
    ) -> Result<T, PeerSyncError> {
        let _operation = self.operation()?;
        let mut runtime = self.runtime()?;
        let host = runtime.host.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("shared source host is not prepared".to_owned())
        })?;
        Ok(operation(host))
    }

    #[cfg(test)]
    pub(crate) fn with_preparation<T>(
        &self,
        operation: impl FnOnce(&P) -> T,
    ) -> Result<T, PeerSyncError> {
        let preparation = self.preparation()?;
        Ok(operation(&preparation))
    }

    fn cleanup_lanes(
        preparation: &mut P,
        acquired: &[SharedSourceLane],
    ) -> (Vec<SharedSourceLane>, Option<PeerSyncError>) {
        let mut remaining = Vec::new();
        let mut primary = None;
        for lane in acquired.iter().rev().copied() {
            if let Err(error) = preparation.cleanup(lane) {
                if primary.is_none() {
                    primary = Some(error);
                }
                remaining.push(lane);
            }
        }
        remaining.reverse();
        (remaining, primary)
    }

    fn operation(&self) -> Result<std::sync::MutexGuard<'_, ()>, PeerSyncError> {
        self.operation.lock().map_err(|error| {
            PeerSyncError::Storage(format!("shared session operation mutex poisoned: {error}"))
        })
    }

    fn runtime(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, SharedSessionRuntime<P::Host>>, PeerSyncError> {
        self.runtime.lock().map_err(|error| {
            PeerSyncError::Storage(format!("shared session runtime mutex poisoned: {error}"))
        })
    }

    fn preparation(&self) -> Result<std::sync::MutexGuard<'_, P>, PeerSyncError> {
        self.preparation.lock().map_err(|error| {
            PeerSyncError::Storage(format!(
                "shared session preparation mutex poisoned: {error}"
            ))
        })
    }

    fn set_phase(&self, phase: SharedSessionPhase) -> Result<(), PeerSyncError> {
        self.runtime()?.phase = phase;
        Ok(())
    }
}

/// Concrete desktop adapter around the existing source preparation engines.
/// It deliberately has no listener, tunnel, foreground-service, or Tauri
/// command responsibility.  Those are layered above this preparation slice.
#[cfg(any(desktop, target_os = "android", test))]
pub(crate) struct SharedSourceEngines {
    clone: Option<PreparedUnifiedCloneSource>,
    delta: Option<PreparedSharedDeltaSource>,
    bidirectional: Option<PreparedSharedBidirectionalSource>,
}

#[cfg(any(desktop, target_os = "android", test))]
impl SharedSourceEngines {
    pub(crate) fn new() -> Self {
        Self {
            clone: None,
            delta: None,
            bidirectional: None,
        }
    }
}

#[cfg(any(desktop, target_os = "android", test))]
pub(crate) struct SharedSourcePreparationContext<'a> {
    pub(crate) store: &'a mut PersistentStore,
    pub(crate) cas: &'a PayloadCas,
    pub(crate) app_root: &'a std::path::Path,
    pub(crate) cancellation: &'a dyn CancellationProbe,
    pub(crate) expected_bidirectional_revision: i64,
    pub(crate) remote_commit: Arc<SharedRemoteCommitSlot>,
}

#[cfg(any(desktop, target_os = "android", test))]
impl SharedSourceOwnership for SharedSourceEngines {
    type Host = SharedSessionHost;

    fn build_host(&mut self) -> Result<Self::Host, PeerSyncError> {
        let clone = self.clone.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("shared clone source is not prepared".to_owned())
        })?;
        let delta = self.delta.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("shared delta source is not prepared".to_owned())
        })?;
        let bidirectional = self.bidirectional.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("shared bidirectional source is not prepared".to_owned())
        })?;
        SharedSessionHost::new(
            clone.take_session()?,
            delta.take_session()?,
            bidirectional.take_session()?,
        )
    }

    fn cleanup(&mut self, lane: SharedSourceLane) -> Result<(), PeerSyncError> {
        match lane {
            SharedSourceLane::Clone => {
                if let Some(clone) = self.clone.as_mut() {
                    clone.cleanup()?;
                    self.clone = None;
                }
            }
            SharedSourceLane::Delta => {
                if let Some(delta) = self.delta.as_mut() {
                    delta.cleanup();
                    self.delta = None;
                }
            }
            SharedSourceLane::Bidirectional => {
                if let Some(bidirectional) = self.bidirectional.as_mut() {
                    bidirectional.cleanup();
                    self.bidirectional = None;
                }
            }
        }
        Ok(())
    }

    fn stop_host(&mut self, host: &mut Self::Host) -> Result<(), PeerSyncError> {
        host.stop()
    }
}

#[cfg(any(desktop, target_os = "android", test))]
impl<'a> SharedSourcePreparation<SharedSourcePreparationContext<'a>> for SharedSourceEngines {
    fn prepare(
        &mut self,
        lane: SharedSourceLane,
        context: &mut SharedSourcePreparationContext<'a>,
    ) -> Result<(), PeerSyncError> {
        match lane {
            SharedSourceLane::Clone => {
                self.clone = Some(prepare_unified_clone_source(
                    context.store,
                    context.cas,
                    &context.app_root.join("peer-clone"),
                    context.cancellation,
                )?);
            }
            SharedSourceLane::Delta => {
                self.delta = Some(prepare_shared_delta_source(
                    context.store,
                    context.cas,
                    context.app_root,
                )?);
            }
            SharedSourceLane::Bidirectional => {
                self.bidirectional = Some(prepare_shared_bidirectional_source(
                    context.store,
                    context.cas,
                    context.app_root,
                    context.expected_bidirectional_revision,
                    Arc::clone(&context.remote_commit),
                )?);
            }
        }
        Ok(())
    }
}

#[cfg(any(desktop, target_os = "android", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DeviceSyncListenMethod {
    Lan,
    Quick,
    #[serde(rename = "fixed-url")]
    FixedUrl,
}

#[cfg(any(desktop, target_os = "android", test))]
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSyncPrepareRequest {
    pub method: DeviceSyncListenMethod,
    pub fixed_port: u16,
    pub public_base_url: Option<String>,
}

#[cfg(any(desktop, target_os = "android", test))]
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSyncLinkPermissions {
    pub read: bool,
    pub bidirectional: bool,
}

#[cfg(any(desktop, target_os = "android", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DeviceSyncSourcePhase {
    Idle,
    Preparing,
    Prepared,
    Starting,
    Running,
    Stopping,
    Error,
}

#[cfg(any(desktop, target_os = "android", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceSyncErrorCategory {
    InvalidConfiguration,
    PortUnavailable,
    PreparationFailed,
    TransportUnavailable,
    CleanupFailed,
    StateUnavailable,
}

/// The last bidirectional commit a peer applied under this shared session.
/// It says only that the store revision moved under the renderer, so the
/// renderer can refresh its working set; it is not completion accounting.
#[cfg(any(desktop, target_os = "android", test))]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedRemoteCommit {
    pub operation_id: String,
    pub committed_revision: i64,
}

#[cfg(any(desktop, target_os = "android", test))]
#[derive(Default)]
pub(crate) struct SharedRemoteCommitSlot {
    latest: Mutex<Option<SharedRemoteCommit>>,
}

#[cfg(any(desktop, target_os = "android", test))]
impl SharedRemoteCommitSlot {
    pub(crate) fn record(&self, operation_id: &str, committed_revision: i64) {
        *self.recovered() = Some(SharedRemoteCommit {
            operation_id: operation_id.to_owned(),
            committed_revision,
        });
    }

    pub(crate) fn latest(&self) -> Option<SharedRemoteCommit> {
        self.recovered().clone()
    }

    pub(crate) fn clear(&self) {
        *self.recovered() = None;
    }

    /// A poisoned slot never fails a status read.  Losing every later poll is
    /// worse than losing one recorded commit, and the value is advisory.
    fn recovered(&self) -> std::sync::MutexGuard<'_, Option<SharedRemoteCommit>> {
        self.latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(any(desktop, target_os = "android", test))]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSyncSourceStatus {
    pub phase: DeviceSyncSourcePhase,
    pub endpoint: Option<String>,
    pub pairing_uri: Option<String>,
    pub expires_at_ms: Option<u64>,
    pub latest_error: Option<DeviceSyncErrorCategory>,
    pub last_remote_commit: Option<SharedRemoteCommit>,
}

#[cfg(any(target_os = "android", test))]
pub(crate) trait AndroidDeviceSyncHost {
    fn enable_registry(
        &mut self,
        app_root: &Path,
        source_name: &str,
        permissions: DevicePermissions,
    ) -> Result<(), PeerSyncError>;
    fn start_private_lan(
        &mut self,
        address: Ipv4Addr,
        port: u16,
    ) -> Result<SharedPairingData, PeerSyncError>;
    fn rotate_link(
        &mut self,
        permissions: DevicePermissions,
    ) -> Result<SharedPairingData, PeerSyncError>;
    fn stop_listener(&mut self) -> Result<(), PeerSyncError>;
    fn revoke_registered_device(&mut self, _device_id: &str) {}
}

#[cfg(any(target_os = "android", test))]
impl AndroidDeviceSyncHost for SharedSessionHost {
    fn enable_registry(
        &mut self,
        app_root: &Path,
        source_name: &str,
        permissions: DevicePermissions,
    ) -> Result<(), PeerSyncError> {
        self.enable_v2_registry(app_root, source_name, permissions)
    }

    fn start_private_lan(
        &mut self,
        address: Ipv4Addr,
        port: u16,
    ) -> Result<SharedPairingData, PeerSyncError> {
        SharedSessionHost::start_private_lan(self, address, port)
    }

    fn stop_listener(&mut self) -> Result<(), PeerSyncError> {
        self.stop()
    }

    fn rotate_link(
        &mut self,
        permissions: DevicePermissions,
    ) -> Result<SharedPairingData, PeerSyncError> {
        self.set_pairing_permissions(permissions)?;
        SharedSessionHost::rotate_link(self)
    }

    fn revoke_registered_device(&mut self, device_id: &str) {
        SharedSessionHost::revoke_registered_device(self, device_id);
    }
}

#[cfg(any(target_os = "android", test))]
#[derive(Clone)]
struct AndroidDeviceSyncConfiguration {
    app_root: PathBuf,
    fixed_port: u16,
}

#[cfg(any(target_os = "android", test))]
struct AndroidDeviceSyncRuntime {
    phase: DeviceSyncSourcePhase,
    configuration: Option<AndroidDeviceSyncConfiguration>,
    pairing: Option<SharedPairingData>,
    foreground: Option<super::android_foreground::AndroidForegroundKey>,
    latest_error: Option<DeviceSyncErrorCategory>,
}

#[cfg(any(target_os = "android", test))]
pub(crate) struct AndroidDeviceSyncSourceState<P = SharedSourceEngines>
where
    P: SharedSourceOwnership,
    P::Host: AndroidDeviceSyncHost,
{
    operation: Arc<Mutex<()>>,
    runtime: Arc<Mutex<AndroidDeviceSyncRuntime>>,
    lifecycle: Arc<SharedSessionLifecycle<P>>,
    lan_address_override: Option<Ipv4Addr>,
    remote_commit: Arc<SharedRemoteCommitSlot>,
}

#[cfg(any(target_os = "android", test))]
impl<P> Clone for AndroidDeviceSyncSourceState<P>
where
    P: SharedSourceOwnership,
    P::Host: AndroidDeviceSyncHost,
{
    fn clone(&self) -> Self {
        Self {
            operation: Arc::clone(&self.operation),
            runtime: Arc::clone(&self.runtime),
            lifecycle: Arc::clone(&self.lifecycle),
            lan_address_override: self.lan_address_override,
            remote_commit: Arc::clone(&self.remote_commit),
        }
    }
}

#[cfg(target_os = "android")]
impl Default for AndroidDeviceSyncSourceState<SharedSourceEngines> {
    fn default() -> Self {
        Self::new(SharedSourceEngines::new(), None)
    }
}

#[cfg(any(target_os = "android", test))]
impl<P> AndroidDeviceSyncSourceState<P>
where
    P: SharedSourceOwnership + Send + 'static,
    P::Host: AndroidDeviceSyncHost + Send + 'static,
{
    fn new(preparation: P, lan_address_override: Option<Ipv4Addr>) -> Self {
        Self {
            operation: Arc::new(Mutex::new(())),
            runtime: Arc::new(Mutex::new(AndroidDeviceSyncRuntime {
                phase: DeviceSyncSourcePhase::Idle,
                configuration: None,
                pairing: None,
                foreground: None,
                latest_error: None,
            })),
            lifecycle: Arc::new(SharedSessionLifecycle::new(preparation)),
            lan_address_override,
            remote_commit: Arc::new(SharedRemoteCommitSlot::default()),
        }
    }

    pub(crate) fn remote_commit_slot(&self) -> Arc<SharedRemoteCommitSlot> {
        Arc::clone(&self.remote_commit)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(preparation: P, lan_address: Ipv4Addr) -> Self {
        Self::new(preparation, Some(lan_address))
    }

    #[cfg(test)]
    pub(crate) fn prepare_for_test<C>(
        &self,
        context: &mut C,
        app_root: &Path,
        request: DeviceSyncPrepareRequest,
    ) -> Result<DeviceSyncSourceStatus, PeerSyncError>
    where
        P: SharedSourcePreparation<C>,
    {
        self.prepare(context, app_root, request)
    }

    #[cfg(test)]
    pub(crate) fn start_attached_for_test(
        &self,
        permissions: DeviceSyncLinkPermissions,
        foreground: super::android_foreground::AndroidForegroundKey,
    ) -> Result<DeviceSyncSourceStatus, PeerSyncError> {
        self.start_attached(permissions, foreground)
    }

    #[cfg(test)]
    pub(crate) fn stop_for_test(
        &self,
    ) -> Result<Option<super::android_foreground::AndroidForegroundKey>, PeerSyncError> {
        self.stop()
    }

    #[cfg(test)]
    pub(crate) fn rotate_link_for_test(
        &self,
        permissions: DeviceSyncLinkPermissions,
    ) -> Result<DeviceSyncSourceStatus, PeerSyncError> {
        self.rotate_link(permissions)
    }

    pub(crate) fn prepare<C>(
        &self,
        context: &mut C,
        app_root: &Path,
        request: DeviceSyncPrepareRequest,
    ) -> Result<DeviceSyncSourceStatus, PeerSyncError>
    where
        P: SharedSourcePreparation<C>,
    {
        let _operation = self.operation()?;
        if self.lifecycle.phase()? == SharedSessionPhase::Stopping {
            return Err(PeerSyncError::Protocol(
                "shared source cleanup is pending; retry stop before preparing".to_owned(),
            ));
        }
        if request.method != DeviceSyncListenMethod::Lan || request.fixed_port == 0 {
            return self.fail(
                DeviceSyncErrorCategory::InvalidConfiguration,
                PeerSyncError::Validation(
                    "Android device sync requires Trusted LAN with a nonzero fixed port".to_owned(),
                ),
            );
        }
        if self.runtime()?.phase == DeviceSyncSourcePhase::Prepared {
            return self.status();
        }
        // Only a preparation that actually reseals the source drops the
        // recorded remote commit; every other path keeps it observable.
        self.remote_commit.clear();
        self.set_phase(DeviceSyncSourcePhase::Preparing)?;
        if let Err(error) = self.lifecycle.prepare(context) {
            let mut runtime = self.runtime()?;
            runtime.configuration = None;
            runtime.pairing = None;
            runtime.foreground = None;
            runtime.latest_error = Some(DeviceSyncErrorCategory::PreparationFailed);
            runtime.phase = DeviceSyncSourcePhase::Error;
            return Err(error);
        }
        let mut runtime = self.runtime()?;
        runtime.configuration = Some(AndroidDeviceSyncConfiguration {
            app_root: app_root.to_path_buf(),
            fixed_port: request.fixed_port,
        });
        runtime.pairing = None;
        runtime.foreground = None;
        runtime.latest_error = None;
        runtime.phase = DeviceSyncSourcePhase::Prepared;
        Ok(Self::status_from_runtime(
            &runtime,
            self.remote_commit.latest(),
        ))
    }

    pub(crate) fn start_attached(
        &self,
        permissions: DeviceSyncLinkPermissions,
        foreground: super::android_foreground::AndroidForegroundKey,
    ) -> Result<DeviceSyncSourceStatus, PeerSyncError> {
        use super::android_foreground::{registry, AndroidForegroundLane};

        if foreground.lane != AndroidForegroundLane::DeviceSyncSource
            || registry().acquire_exact(&foreground).is_none()
        {
            return Err(PeerSyncError::Validation(
                "Android device sync foreground identity is not attached".to_owned(),
            ));
        }
        let _operation = self.operation()?;
        if self.runtime()?.phase == DeviceSyncSourcePhase::Running
            && self.runtime()?.foreground.as_ref() == Some(&foreground)
        {
            return self.status();
        }
        let start_result = (|| {
            if !permissions.read {
                return self.fail(
                    DeviceSyncErrorCategory::InvalidConfiguration,
                    PeerSyncError::Validation(
                        "device sync sharing requires read permission".to_owned(),
                    ),
                );
            }
            let configuration = self.runtime()?.configuration.clone().ok_or_else(|| {
                PeerSyncError::Protocol("device sync source is not prepared".to_owned())
            })?;
            if self.runtime()?.phase != DeviceSyncSourcePhase::Prepared {
                return Err(PeerSyncError::Protocol(
                    "device sync source is not prepared".to_owned(),
                ));
            }
            let address = match self.lan_address_override {
                Some(address) => address,
                None => super::lan::discover_lan_ipv4().map_err(|error| {
                    if let Ok(mut runtime) = self.runtime() {
                        runtime.latest_error = Some(DeviceSyncErrorCategory::TransportUnavailable);
                    }
                    error
                })?,
            };
            self.set_phase(DeviceSyncSourcePhase::Starting)?;
            let host_permissions = if permissions.bidirectional {
                DevicePermissions::read_and_bidirectional()
            } else {
                DevicePermissions::read()
            };
            let start = self.lifecycle.with_host_mut(|host| {
                host.enable_registry(
                    &configuration.app_root,
                    super::device_registry::platform_device_name(),
                    host_permissions,
                )?;
                host.start_private_lan(address, configuration.fixed_port)
            });
            let pairing = match start.and_then(|result| result) {
                Ok(pairing) => pairing,
                Err(error) => {
                    let cleanup = self
                        .lifecycle
                        .with_host_mut(AndroidDeviceSyncHost::stop_listener)
                        .and_then(|result| result);
                    let mut runtime = self.runtime()?;
                    runtime.pairing = None;
                    runtime.foreground = None;
                    if let Err(cleanup_error) = cleanup {
                        crate::nlog!(
                            "error",
                            "Android shared listener start cleanup failed: {cleanup_error}"
                        );
                        runtime.phase = DeviceSyncSourcePhase::Error;
                        runtime.latest_error = Some(DeviceSyncErrorCategory::CleanupFailed);
                    } else {
                        runtime.phase = DeviceSyncSourcePhase::Prepared;
                        runtime.latest_error = Some(if is_shared_port_unavailable(&error) {
                            DeviceSyncErrorCategory::PortUnavailable
                        } else {
                            DeviceSyncErrorCategory::TransportUnavailable
                        });
                    }
                    return Err(error);
                }
            };
            let mut runtime = self.runtime()?;
            runtime.phase = DeviceSyncSourcePhase::Running;
            runtime.pairing = Some(pairing);
            runtime.foreground = Some(foreground.clone());
            runtime.latest_error = None;
            Ok(Self::status_from_runtime(
                &runtime,
                self.remote_commit.latest(),
            ))
        })();
        drop(_operation);
        let status = match start_result {
            Ok(status) => status,
            Err(error) => {
                self.release_failed_foreground(&foreground);
                return Err(error);
            }
        };
        let callback_state = self.clone();
        let callback_key = foreground.clone();
        if !registry().set_source_stop_callback_exact(&foreground, move || {
            callback_state.notification_stop_exact(&callback_key);
        }) {
            self.notification_stop_exact(&foreground);
            self.release_failed_foreground(&foreground);
            return Err(PeerSyncError::Protocol(
                "Android foreground service detached before source start".to_owned(),
            ));
        }
        Ok(status)
    }

    pub(crate) fn rotate_link(
        &self,
        permissions: DeviceSyncLinkPermissions,
    ) -> Result<DeviceSyncSourceStatus, PeerSyncError> {
        let _operation = self.operation()?;
        if !permissions.read {
            return self.fail(
                DeviceSyncErrorCategory::InvalidConfiguration,
                PeerSyncError::Validation(
                    "device sync sharing requires read permission".to_owned(),
                ),
            );
        }
        if self.runtime()?.phase != DeviceSyncSourcePhase::Running {
            return self.fail(
                DeviceSyncErrorCategory::StateUnavailable,
                PeerSyncError::Protocol("device sync source is not running".to_owned()),
            );
        }
        let host_permissions = if permissions.bidirectional {
            DevicePermissions::read_and_bidirectional()
        } else {
            DevicePermissions::read()
        };
        let pairing = self
            .lifecycle
            .with_host_mut(|host| host.rotate_link(host_permissions))??;
        let mut runtime = self.runtime()?;
        runtime.pairing = Some(pairing);
        runtime.latest_error = None;
        Ok(Self::status_from_runtime(
            &runtime,
            self.remote_commit.latest(),
        ))
    }

    pub(crate) fn status(&self) -> Result<DeviceSyncSourceStatus, PeerSyncError> {
        let runtime = self.runtime()?;
        Ok(Self::status_from_runtime(
            &runtime,
            self.remote_commit.latest(),
        ))
    }

    pub(crate) fn stop(
        &self,
    ) -> Result<Option<super::android_foreground::AndroidForegroundKey>, PeerSyncError> {
        use super::android_foreground::registry;

        let foreground;
        let cleanup;
        {
            let _operation = self.operation()?;
            foreground = {
                let mut runtime = self.runtime()?;
                runtime.phase = DeviceSyncSourcePhase::Stopping;
                runtime.pairing = None;
                runtime.foreground.clone()
            };
            cleanup = self.lifecycle.stop();
            let mut runtime = self.runtime()?;
            runtime.pairing = None;
            runtime.latest_error = cleanup
                .as_ref()
                .err()
                .map(|_| DeviceSyncErrorCategory::CleanupFailed);
            if cleanup.is_ok() {
                runtime.configuration = None;
                runtime.foreground = None;
            }
            runtime.phase = if cleanup.is_ok() {
                DeviceSyncSourcePhase::Idle
            } else {
                DeviceSyncSourcePhase::Error
            };
        }
        if cleanup.is_ok() {
            if let Some(key) = foreground.as_ref() {
                let _ = registry().cancel_exact(key);
                let _ = registry().detach_if_generation(key);
            }
        }
        cleanup.map(|()| foreground)
    }

    pub(crate) fn revoke_registered_device(&self, device_id: &str) {
        let result = self
            .lifecycle
            .with_host_mut(|host| host.revoke_registered_device(device_id));
        if let Err(error) = result {
            crate::nlog!(
                "warn",
                "Android shared source live bearer revoke skipped: {error}"
            );
        }
    }

    fn notification_stop_exact(&self, key: &super::android_foreground::AndroidForegroundKey) {
        let Ok(_operation) = self.operation() else {
            return;
        };
        let should_stop_running = self.runtime().ok().is_some_and(|runtime| {
            runtime.phase == DeviceSyncSourcePhase::Running
                && runtime.foreground.as_ref() == Some(key)
        });
        if !should_stop_running {
            return;
        }
        if self
            .lifecycle
            .with_host_mut(AndroidDeviceSyncHost::stop_listener)
            .and_then(|result| result)
            .is_err()
        {
            if let Ok(mut runtime) = self.runtime() {
                runtime.phase = DeviceSyncSourcePhase::Error;
                runtime.latest_error = Some(DeviceSyncErrorCategory::CleanupFailed);
            }
            return;
        }
        if let Ok(mut runtime) = self.runtime() {
            if runtime.foreground.as_ref() == Some(key) {
                runtime.foreground = None;
                runtime.pairing = None;
                runtime.latest_error = None;
                runtime.phase = DeviceSyncSourcePhase::Prepared;
            }
        }
    }

    fn release_failed_foreground(
        &self,
        foreground: &super::android_foreground::AndroidForegroundKey,
    ) {
        let registry = super::android_foreground::registry();
        let _ = registry.cancel_exact(foreground);
        let _ = registry.detach_if_generation(foreground);
    }

    fn fail<T>(
        &self,
        category: DeviceSyncErrorCategory,
        error: PeerSyncError,
    ) -> Result<T, PeerSyncError> {
        if let Ok(mut runtime) = self.runtime() {
            runtime.latest_error = Some(category);
        }
        Err(error)
    }

    fn status_from_runtime(
        runtime: &AndroidDeviceSyncRuntime,
        last_remote_commit: Option<SharedRemoteCommit>,
    ) -> DeviceSyncSourceStatus {
        DeviceSyncSourceStatus {
            phase: runtime.phase,
            endpoint: runtime
                .pairing
                .as_ref()
                .map(|pairing| pairing.endpoint.clone()),
            pairing_uri: runtime
                .pairing
                .as_ref()
                .map(SharedPairingData::canonical_uri),
            expires_at_ms: runtime
                .pairing
                .as_ref()
                .and_then(|pairing| u64::try_from(pairing.expires_at_ms).ok()),
            latest_error: runtime.latest_error,
            last_remote_commit,
        }
    }

    fn set_phase(&self, phase: DeviceSyncSourcePhase) -> Result<(), PeerSyncError> {
        self.runtime()?.phase = phase;
        Ok(())
    }

    fn operation(&self) -> Result<std::sync::MutexGuard<'_, ()>, PeerSyncError> {
        self.operation.lock().map_err(|error| {
            PeerSyncError::Storage(format!(
                "Android device sync operation mutex poisoned: {error}"
            ))
        })
    }

    fn runtime(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, AndroidDeviceSyncRuntime>, PeerSyncError> {
        self.runtime.lock().map_err(|error| {
            PeerSyncError::Storage(format!(
                "Android device sync runtime mutex poisoned: {error}"
            ))
        })
    }
}

#[cfg(desktop)]
#[derive(Clone)]
struct DeviceSyncConfiguration {
    app_root: PathBuf,
    method: DeviceSyncListenMethod,
    fixed_port: u16,
    public_base_url: Option<String>,
}

#[cfg(desktop)]
struct DeviceSyncRuntime {
    phase: DeviceSyncSourcePhase,
    configuration: Option<DeviceSyncConfiguration>,
    pairing: Option<SharedPairingData>,
    latest_error: Option<DeviceSyncErrorCategory>,
    tunnel: Option<Box<dyn SharedPeerTunnel>>,
    failed_tunnel: Option<Box<dyn SharedPeerTunnelCleanup>>,
}

#[cfg(desktop)]
struct DeviceSyncRotationFailure {
    category: DeviceSyncErrorCategory,
    error: PeerSyncError,
}

#[cfg(desktop)]
pub(crate) trait SharedPublicOriginVerifier: Send + Sync {
    fn verify(&self, host: &SharedSessionHost, public_url: &url::Url) -> Result<(), PeerSyncError>;
}

#[cfg(desktop)]
struct SystemSharedPublicOriginVerifier;

#[cfg(desktop)]
impl SharedPublicOriginVerifier for SystemSharedPublicOriginVerifier {
    fn verify(&self, host: &SharedSessionHost, public_url: &url::Url) -> Result<(), PeerSyncError> {
        host.verify_public_origin(public_url)
    }
}

#[cfg(desktop)]
pub(crate) trait SharedPeerTunnel: Send {
    fn transport_url(&self) -> url::Url;
    fn lifecycle(&mut self) -> Result<SharedPeerTunnelLifecycle, PeerSyncError> {
        Ok(SharedPeerTunnelLifecycle::Running)
    }
    fn stop(&mut self) -> Result<(), PeerSyncError>;
}

#[cfg(desktop)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SharedPeerTunnelLifecycle {
    Running,
    CleanupPending,
    Stopped,
}

#[cfg(desktop)]
pub(crate) trait SharedPeerTunnelCleanup: Send {
    fn stop(&mut self) -> Result<(), PeerSyncError>;
}

#[cfg(desktop)]
pub(crate) trait SharedPeerTunnelLauncher: Send + Sync {
    fn start(&self, host: SharedSessionHost) -> Result<Box<dyn SharedPeerTunnel>, PeerSyncError>;

    fn take_failed_cleanup(&self) -> Option<Box<dyn SharedPeerTunnelCleanup>> {
        None
    }
}

#[cfg(desktop)]
struct UnavailableSharedPeerTunnelLauncher;

#[cfg(desktop)]
impl SharedPeerTunnelLauncher for UnavailableSharedPeerTunnelLauncher {
    fn start(&self, _: SharedSessionHost) -> Result<Box<dyn SharedPeerTunnel>, PeerSyncError> {
        Err(PeerSyncError::Transport(
            "shared quick tunnel launcher is unavailable".to_owned(),
        ))
    }
}

#[cfg(desktop)]
pub struct DeviceSyncSourceState<P = SharedSourceEngines>
where
    P: SharedSourceOwnership<Host = SharedSessionHost>,
{
    operation: Arc<Mutex<()>>,
    runtime: Arc<Mutex<DeviceSyncRuntime>>,
    lifecycle: Arc<SharedSessionLifecycle<P>>,
    lan_address_override: Option<Ipv4Addr>,
    quick_tunnel_launcher: Arc<dyn SharedPeerTunnelLauncher>,
    public_origin_verifier: Arc<dyn SharedPublicOriginVerifier>,
    remote_commit: Arc<SharedRemoteCommitSlot>,
}

#[cfg(desktop)]
impl<P> Clone for DeviceSyncSourceState<P>
where
    P: SharedSourceOwnership<Host = SharedSessionHost>,
{
    fn clone(&self) -> Self {
        Self {
            operation: Arc::clone(&self.operation),
            runtime: Arc::clone(&self.runtime),
            lifecycle: Arc::clone(&self.lifecycle),
            lan_address_override: self.lan_address_override,
            quick_tunnel_launcher: Arc::clone(&self.quick_tunnel_launcher),
            public_origin_verifier: Arc::clone(&self.public_origin_verifier),
            remote_commit: Arc::clone(&self.remote_commit),
        }
    }
}

#[cfg(desktop)]
impl Default for DeviceSyncSourceState<SharedSourceEngines> {
    fn default() -> Self {
        Self::new(
            SharedSourceEngines::new(),
            None,
            Arc::new(super::tunnel_lifecycle::SystemSharedPeerTunnelLauncher::default()),
            Arc::new(SystemSharedPublicOriginVerifier),
        )
    }
}

#[cfg(desktop)]
impl<P> DeviceSyncSourceState<P>
where
    P: SharedSourceOwnership<Host = SharedSessionHost>,
{
    fn new(
        preparation: P,
        lan_address_override: Option<Ipv4Addr>,
        quick_tunnel_launcher: Arc<dyn SharedPeerTunnelLauncher>,
        public_origin_verifier: Arc<dyn SharedPublicOriginVerifier>,
    ) -> Self {
        Self {
            operation: Arc::new(Mutex::new(())),
            runtime: Arc::new(Mutex::new(DeviceSyncRuntime {
                phase: DeviceSyncSourcePhase::Idle,
                configuration: None,
                pairing: None,
                latest_error: None,
                tunnel: None,
                failed_tunnel: None,
            })),
            lifecycle: Arc::new(SharedSessionLifecycle::new(preparation)),
            lan_address_override,
            quick_tunnel_launcher,
            public_origin_verifier,
            remote_commit: Arc::new(SharedRemoteCommitSlot::default()),
        }
    }

    pub(crate) fn remote_commit_slot(&self) -> Arc<SharedRemoteCommitSlot> {
        Arc::clone(&self.remote_commit)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(preparation: P, lan_address: Ipv4Addr) -> Self {
        Self::new(
            preparation,
            Some(lan_address),
            Arc::new(UnavailableSharedPeerTunnelLauncher),
            Arc::new(SystemSharedPublicOriginVerifier),
        )
    }

    #[cfg(test)]
    pub(crate) fn new_for_test_with_quick_tunnel(
        preparation: P,
        quick_tunnel_launcher: Arc<dyn SharedPeerTunnelLauncher>,
    ) -> Self {
        Self::new(
            preparation,
            Some(Ipv4Addr::LOCALHOST),
            quick_tunnel_launcher,
            Arc::new(SystemSharedPublicOriginVerifier),
        )
    }

    #[cfg(test)]
    pub(crate) fn new_for_test_with_transports(
        preparation: P,
        quick_tunnel_launcher: Arc<dyn SharedPeerTunnelLauncher>,
        public_origin_verifier: Arc<dyn SharedPublicOriginVerifier>,
    ) -> Self {
        Self::new(
            preparation,
            Some(Ipv4Addr::LOCALHOST),
            quick_tunnel_launcher,
            public_origin_verifier,
        )
    }

    pub(crate) fn prepare<C>(
        &self,
        context: &mut C,
        app_root: &Path,
        request: DeviceSyncPrepareRequest,
    ) -> Result<DeviceSyncSourceStatus, PeerSyncError>
    where
        P: SharedSourcePreparation<C>,
    {
        let _operation = self.lock_operation()?;
        if request.fixed_port == 0 {
            return self.fail(
                DeviceSyncErrorCategory::InvalidConfiguration,
                PeerSyncError::Validation("device sync fixed port must be nonzero".to_owned()),
            );
        }
        let public_base_url = match request.method {
            DeviceSyncListenMethod::FixedUrl => Some(
                match validate_fixed_public_base_url(
                    request.public_base_url.as_deref().unwrap_or_default(),
                ) {
                    Ok(url) => url,
                    Err(error) => {
                        return self.fail(DeviceSyncErrorCategory::InvalidConfiguration, error)
                    }
                },
            ),
            DeviceSyncListenMethod::Lan | DeviceSyncListenMethod::Quick => None,
        };
        {
            let runtime = self.lock_runtime()?;
            if matches!(
                runtime.phase,
                DeviceSyncSourcePhase::Prepared | DeviceSyncSourcePhase::Running
            ) {
                return Ok(Self::status_from_runtime(
                    &runtime,
                    self.remote_commit.latest(),
                ));
            }
        }
        // Only a preparation that actually reseals the source drops the
        // recorded remote commit; every other path keeps it observable.
        self.remote_commit.clear();
        self.set_phase(DeviceSyncSourcePhase::Preparing)?;
        if let Err(error) = self.lifecycle.prepare(context) {
            return self.fail(DeviceSyncErrorCategory::PreparationFailed, error);
        }
        let mut runtime = self.lock_runtime()?;
        runtime.configuration = Some(DeviceSyncConfiguration {
            app_root: app_root.to_path_buf(),
            method: request.method,
            fixed_port: request.fixed_port,
            public_base_url: public_base_url.map(|url| url.to_string()),
        });
        runtime.pairing = None;
        runtime.latest_error = None;
        runtime.phase = DeviceSyncSourcePhase::Prepared;
        Ok(Self::status_from_runtime(
            &runtime,
            self.remote_commit.latest(),
        ))
    }

    pub(crate) fn start(
        &self,
        permissions: DeviceSyncLinkPermissions,
    ) -> Result<DeviceSyncSourceStatus, PeerSyncError> {
        let _operation = self.lock_operation()?;
        let was_running = self.lock_runtime()?.phase == DeviceSyncSourcePhase::Running;
        if was_running {
            let status = self.status()?;
            if status.phase == DeviceSyncSourcePhase::Running {
                return Ok(status);
            }
            return Err(PeerSyncError::Protocol(
                "device sync tunnel is no longer running; stop before starting again".to_owned(),
            ));
        }
        {
            let runtime = self.lock_runtime()?;
            if runtime.tunnel.is_some() || runtime.failed_tunnel.is_some() {
                drop(runtime);
                return self.fail(
                    DeviceSyncErrorCategory::CleanupFailed,
                    PeerSyncError::Protocol(
                        "device sync tunnel cleanup is pending; retry stop before starting"
                            .to_owned(),
                    ),
                );
            }
        }
        if !permissions.read || (permissions.bidirectional && !permissions.read) {
            return self.fail(
                DeviceSyncErrorCategory::InvalidConfiguration,
                PeerSyncError::Validation(
                    "device sync sharing requires read permission".to_owned(),
                ),
            );
        }
        let configuration = match self.lock_runtime()?.configuration.clone() {
            Some(configuration) => configuration,
            None => {
                return self.fail(
                    DeviceSyncErrorCategory::StateUnavailable,
                    PeerSyncError::Protocol("device sync source is not prepared".to_owned()),
                )
            }
        };
        let advertised_lan_address = if configuration.method == DeviceSyncListenMethod::Lan {
            Some(match self.lan_address_override {
                Some(address) => address,
                None => match super::lan::discover_lan_ipv4() {
                    Ok(address) => address,
                    Err(error) => {
                        return self.fail(DeviceSyncErrorCategory::TransportUnavailable, error)
                    }
                },
            })
        } else {
            None
        };
        self.set_phase(DeviceSyncSourcePhase::Starting)?;
        let host_permissions = if permissions.bidirectional {
            DevicePermissions::read_and_bidirectional()
        } else {
            DevicePermissions::read()
        };
        let start = self.lifecycle.with_host_mut(|host| {
            host.enable_v2_registry(
                &configuration.app_root,
                super::device_registry::platform_device_name(),
                host_permissions,
            )?;
            match configuration.method {
                DeviceSyncListenMethod::Lan => host
                    .start_fixed_lan(
                        advertised_lan_address.expect("LAN address resolved"),
                        configuration.fixed_port,
                    )
                    .map(|pairing| (pairing, None)),
                DeviceSyncListenMethod::Quick => {
                    let mut pairing = host.start_fixed_loopback(configuration.fixed_port)?;
                    let tunnel = self.quick_tunnel_launcher.start(host.clone())?;
                    let endpoint = tunnel.transport_url().to_string();
                    host.set_advertised_endpoint(endpoint.clone())?;
                    pairing.endpoint = endpoint;
                    Ok((pairing, Some(tunnel)))
                }
                DeviceSyncListenMethod::FixedUrl => {
                    let mut pairing = host.start_fixed_loopback(configuration.fixed_port)?;
                    let public_url = validate_fixed_public_base_url(
                        configuration.public_base_url.as_deref().unwrap_or_default(),
                    )?;
                    self.public_origin_verifier.verify(host, &public_url)?;
                    let endpoint = public_url.to_string();
                    host.set_advertised_endpoint(endpoint.clone())?;
                    pairing.endpoint = endpoint;
                    Ok((pairing, None))
                }
            }
        })?;
        let (pairing, tunnel) = match start {
            Ok(result) => result,
            Err(error) => {
                let category = if is_shared_port_unavailable(&error) {
                    DeviceSyncErrorCategory::PortUnavailable
                } else {
                    DeviceSyncErrorCategory::TransportUnavailable
                };
                if let Some(mut failed_tunnel) = self.quick_tunnel_launcher.take_failed_cleanup() {
                    if let Err(cleanup_error) = failed_tunnel.stop() {
                        crate::nlog!(
                            "error",
                            "shared quick tunnel start cleanup failed: {cleanup_error}"
                        );
                        self.lock_runtime()?.failed_tunnel = Some(failed_tunnel);
                    }
                }
                match self.lifecycle.with_host_mut(SharedSessionHost::stop) {
                    Ok(Ok(())) => {}
                    Ok(Err(cleanup_error)) | Err(cleanup_error) => {
                        crate::nlog!(
                            "error",
                            "shared listener cleanup after start failure failed: {cleanup_error}"
                        );
                    }
                }
                return self.fail(category, error);
            }
        };
        let mut runtime = self.lock_runtime()?;
        runtime.pairing = Some(pairing);
        runtime.tunnel = tunnel;
        runtime.latest_error = None;
        runtime.phase = DeviceSyncSourcePhase::Running;
        Ok(Self::status_from_runtime(
            &runtime,
            self.remote_commit.latest(),
        ))
    }

    pub(crate) fn status(&self) -> Result<DeviceSyncSourceStatus, PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        if runtime.phase == DeviceSyncSourcePhase::Running {
            let lifecycle = runtime.tunnel.as_mut().map(|tunnel| tunnel.lifecycle());
            match lifecycle {
                Some(Ok(SharedPeerTunnelLifecycle::Running)) | None => {}
                Some(Ok(SharedPeerTunnelLifecycle::CleanupPending)) => {
                    runtime.phase = DeviceSyncSourcePhase::Error;
                    runtime.latest_error = Some(DeviceSyncErrorCategory::CleanupFailed);
                    runtime.pairing = None;
                }
                Some(Ok(SharedPeerTunnelLifecycle::Stopped)) => {
                    runtime.phase = DeviceSyncSourcePhase::Error;
                    runtime.latest_error = Some(DeviceSyncErrorCategory::TransportUnavailable);
                    runtime.pairing = None;
                }
                Some(Err(error)) => {
                    crate::nlog!("error", "shared quick tunnel status failed: {error}");
                    runtime.phase = DeviceSyncSourcePhase::Error;
                    runtime.latest_error = Some(DeviceSyncErrorCategory::TransportUnavailable);
                    runtime.pairing = None;
                }
            }
        }
        Ok(Self::status_from_runtime(
            &runtime,
            self.remote_commit.latest(),
        ))
    }

    fn rotate_link(
        &self,
        permissions: DeviceSyncLinkPermissions,
    ) -> Result<DeviceSyncSourceStatus, DeviceSyncRotationFailure> {
        let _operation = self.lock_operation().map_err(|error| {
            self.record_error_category(DeviceSyncErrorCategory::StateUnavailable);
            DeviceSyncRotationFailure {
                category: DeviceSyncErrorCategory::StateUnavailable,
                error,
            }
        })?;
        {
            let mut runtime = self
                .lock_runtime()
                .map_err(|error| DeviceSyncRotationFailure {
                    category: DeviceSyncErrorCategory::StateUnavailable,
                    error,
                })?;
            runtime.latest_error = None;
        }
        if !permissions.read {
            let error = PeerSyncError::Validation(
                "device sync sharing requires read permission".to_owned(),
            );
            let mut runtime = self
                .lock_runtime()
                .map_err(|error| DeviceSyncRotationFailure {
                    category: DeviceSyncErrorCategory::StateUnavailable,
                    error,
                })?;
            if runtime.phase == DeviceSyncSourcePhase::Running {
                runtime.latest_error = Some(DeviceSyncErrorCategory::InvalidConfiguration);
            } else {
                runtime.phase = DeviceSyncSourcePhase::Error;
                runtime.latest_error = Some(DeviceSyncErrorCategory::InvalidConfiguration);
            }
            return Err(DeviceSyncRotationFailure {
                category: DeviceSyncErrorCategory::InvalidConfiguration,
                error,
            });
        }
        {
            let mut runtime = self
                .lock_runtime()
                .map_err(|error| DeviceSyncRotationFailure {
                    category: DeviceSyncErrorCategory::StateUnavailable,
                    error,
                })?;
            if runtime.phase != DeviceSyncSourcePhase::Running {
                runtime.phase = DeviceSyncSourcePhase::Error;
                runtime.latest_error = Some(DeviceSyncErrorCategory::StateUnavailable);
                return Err(DeviceSyncRotationFailure {
                    category: DeviceSyncErrorCategory::StateUnavailable,
                    error: PeerSyncError::Protocol("device sync source is not running".to_owned()),
                });
            }
        }
        let host_permissions = if permissions.bidirectional {
            DevicePermissions::read_and_bidirectional()
        } else {
            DevicePermissions::read()
        };
        let rotated = self.lifecycle.with_host_mut(|host| {
            host.set_pairing_permissions(host_permissions)?;
            host.rotate_link()
        });
        let rotated = match rotated {
            Ok(Ok(pairing)) => pairing,
            Ok(Err(error)) | Err(error) => {
                self.record_error_category(DeviceSyncErrorCategory::StateUnavailable);
                return Err(DeviceSyncRotationFailure {
                    category: DeviceSyncErrorCategory::StateUnavailable,
                    error,
                });
            }
        };
        let mut runtime = self
            .lock_runtime()
            .map_err(|error| DeviceSyncRotationFailure {
                category: DeviceSyncErrorCategory::StateUnavailable,
                error,
            })?;
        runtime.pairing = Some(rotated);
        runtime.latest_error = None;
        runtime.phase = DeviceSyncSourcePhase::Running;
        Ok(Self::status_from_runtime(
            &runtime,
            self.remote_commit.latest(),
        ))
    }

    pub(crate) fn stop(&self) -> Result<DeviceSyncSourceStatus, PeerSyncError> {
        let _operation = self.lock_operation()?;
        if self.lock_runtime()?.phase == DeviceSyncSourcePhase::Idle {
            return self.status();
        }
        self.set_phase(DeviceSyncSourcePhase::Stopping)?;
        let mut primary = None;
        let mut tunnel = self.lock_runtime()?.tunnel.take();
        let mut retain_tunnel = false;
        if let Some(active) = tunnel.as_mut() {
            if let Err(error) = active.stop() {
                primary = Some(error);
                retain_tunnel = true;
            }
        }
        if retain_tunnel {
            self.lock_runtime()?.tunnel = tunnel;
        }
        let mut failed_tunnel = self.lock_runtime()?.failed_tunnel.take();
        let mut retain_failed_tunnel = false;
        if let Some(failed) = failed_tunnel.as_mut() {
            if let Err(error) = failed.stop() {
                if primary.is_none() {
                    primary = Some(error);
                }
                retain_failed_tunnel = true;
            }
        }
        if retain_failed_tunnel {
            self.lock_runtime()?.failed_tunnel = failed_tunnel;
        }
        if let Err(error) = self.lifecycle.stop() {
            if primary.is_none() {
                primary = Some(error);
            }
        }
        self.lock_runtime()?.pairing = None;
        if let Some(error) = primary {
            return self.fail(DeviceSyncErrorCategory::CleanupFailed, error);
        }
        let mut runtime = self.lock_runtime()?;
        runtime.phase = DeviceSyncSourcePhase::Idle;
        runtime.configuration = None;
        runtime.pairing = None;
        runtime.tunnel = None;
        runtime.failed_tunnel = None;
        runtime.latest_error = None;
        Ok(Self::status_from_runtime(
            &runtime,
            self.remote_commit.latest(),
        ))
    }

    pub(crate) fn revoke_registered_device(&self, device_id: &str) {
        let result = self
            .lifecycle
            .with_host_mut(|host| host.revoke_registered_device(device_id));
        if let Err(error) = result {
            crate::nlog!("warn", "shared source live bearer revoke skipped: {error}");
        }
    }

    #[cfg(test)]
    pub(crate) fn pairing_for_test(&self) -> Option<SharedPairingData> {
        self.runtime
            .lock()
            .ok()
            .and_then(|runtime| runtime.pairing.clone())
    }

    #[cfg(test)]
    pub(crate) fn remove_host_for_test(&self) -> Result<(), PeerSyncError> {
        self.lifecycle.stop()
    }

    fn fail<T>(
        &self,
        category: DeviceSyncErrorCategory,
        error: PeerSyncError,
    ) -> Result<T, PeerSyncError> {
        self.record_error_category(category);
        Err(error)
    }

    fn record_error_category(&self, category: DeviceSyncErrorCategory) {
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.phase = DeviceSyncSourcePhase::Error;
            runtime.latest_error = Some(category);
        }
    }

    fn clear_error_category(&self) {
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.latest_error = None;
        }
    }

    fn status_from_runtime(
        runtime: &DeviceSyncRuntime,
        last_remote_commit: Option<SharedRemoteCommit>,
    ) -> DeviceSyncSourceStatus {
        DeviceSyncSourceStatus {
            phase: runtime.phase,
            endpoint: runtime
                .pairing
                .as_ref()
                .map(|pairing| pairing.endpoint.clone()),
            pairing_uri: runtime
                .pairing
                .as_ref()
                .map(SharedPairingData::canonical_uri),
            expires_at_ms: runtime
                .pairing
                .as_ref()
                .and_then(|pairing| u64::try_from(pairing.expires_at_ms).ok()),
            latest_error: runtime.latest_error,
            last_remote_commit,
        }
    }

    fn set_phase(&self, phase: DeviceSyncSourcePhase) -> Result<(), PeerSyncError> {
        self.lock_runtime()?.phase = phase;
        Ok(())
    }

    fn lock_operation(&self) -> Result<std::sync::MutexGuard<'_, ()>, PeerSyncError> {
        self.operation.lock().map_err(|error| {
            PeerSyncError::Storage(format!("device sync operation mutex poisoned: {error}"))
        })
    }

    fn lock_runtime(&self) -> Result<std::sync::MutexGuard<'_, DeviceSyncRuntime>, PeerSyncError> {
        self.runtime.lock().map_err(|error| {
            PeerSyncError::Storage(format!("device sync runtime mutex poisoned: {error}"))
        })
    }
}

#[cfg(desktop)]
fn validate_fixed_public_base_url(value: &str) -> Result<url::Url, PeerSyncError> {
    super::tunnel::validate_public_base_url(value)
        .ok_or_else(|| PeerSyncError::Validation("device sync public URL is invalid".to_owned()))
}

#[cfg(any(desktop, target_os = "android", test))]
fn is_shared_port_unavailable(error: &PeerSyncError) -> bool {
    matches!(error, PeerSyncError::Transport(message) if message == "shared fixed port is unavailable")
}

#[cfg(desktop)]
fn device_sync_store_error(error: PeerSyncError) -> StoreError {
    StoreError::Store {
        message: error.to_string(),
    }
}

#[cfg(desktop)]
fn device_sync_command_failure<P>(
    state: &DeviceSyncSourceState<P>,
    operation: &str,
    fallback: DeviceSyncErrorCategory,
    error: impl std::fmt::Display,
) -> DeviceSyncErrorCategory
where
    P: SharedSourceOwnership<Host = SharedSessionHost>,
{
    crate::nlog!("error", "device sync {operation} failed: {error}");
    let category = state
        .status()
        .ok()
        .and_then(|status| status.latest_error)
        .unwrap_or(fallback);
    state.record_error_category(category);
    category
}

#[cfg(desktop)]
pub(crate) async fn run_device_sync_rotate_link<P>(
    state: DeviceSyncSourceState<P>,
    permissions: DeviceSyncLinkPermissions,
) -> Result<DeviceSyncSourceStatus, DeviceSyncErrorCategory>
where
    P: SharedSourceOwnership<Host = SharedSessionHost> + Send + 'static,
{
    let worker_state = state.clone();
    match tauri::async_runtime::spawn_blocking(move || worker_state.rotate_link(permissions)).await
    {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(failure)) => {
            crate::nlog!(
                "error",
                "device sync link rotation failed: {}",
                failure.error
            );
            Err(failure.category)
        }
        Err(error) => Err(device_sync_command_failure(
            &state,
            "link rotation worker",
            DeviceSyncErrorCategory::StateUnavailable,
            error,
        )),
    }
}

#[cfg(desktop)]
#[tauri::command]
pub async fn device_sync_prepare(
    app: AppHandle,
    state: State<'_, DeviceSyncSourceState>,
    request: DeviceSyncPrepareRequest,
) -> Result<DeviceSyncSourceStatus, DeviceSyncErrorCategory> {
    let state = state.inner().clone();
    state.clear_error_category();
    let worker_state = state.clone();
    match tauri::async_runtime::spawn_blocking(move || {
        let app_root = app.path().app_data_dir().map_err(|error| {
            PeerSyncError::Storage(format!(
                "device sync application data directory is unavailable: {error}"
            ))
        })?;
        let cas = PayloadCas::new(&app_root)?;
        persistent_store::commands::with_store_mut(app.state(), |store| {
            let expected_bidirectional_revision = store.revision()?;
            let mut context = SharedSourcePreparationContext {
                store,
                cas: &cas,
                app_root: &app_root,
                cancellation: &NeverCancelled,
                expected_bidirectional_revision,
                remote_commit: worker_state.remote_commit_slot(),
            };
            worker_state
                .prepare(&mut context, &app_root, request)
                .map_err(device_sync_store_error)
        })
        .map_err(|error| PeerSyncError::Storage(error.to_string()))
    })
    .await
    {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => Err(device_sync_command_failure(
            &state,
            "prepare",
            DeviceSyncErrorCategory::PreparationFailed,
            error,
        )),
        Err(error) => Err(device_sync_command_failure(
            &state,
            "prepare worker",
            DeviceSyncErrorCategory::StateUnavailable,
            error,
        )),
    }
}

#[cfg(desktop)]
#[tauri::command]
pub async fn device_sync_start(
    state: State<'_, DeviceSyncSourceState>,
    permissions: DeviceSyncLinkPermissions,
) -> Result<DeviceSyncSourceStatus, DeviceSyncErrorCategory> {
    let state = state.inner().clone();
    state.clear_error_category();
    let worker_state = state.clone();
    match tauri::async_runtime::spawn_blocking(move || worker_state.start(permissions)).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => Err(device_sync_command_failure(
            &state,
            "start",
            DeviceSyncErrorCategory::TransportUnavailable,
            error,
        )),
        Err(error) => Err(device_sync_command_failure(
            &state,
            "start worker",
            DeviceSyncErrorCategory::StateUnavailable,
            error,
        )),
    }
}

#[cfg(desktop)]
#[tauri::command]
pub fn device_sync_status(
    state: State<'_, DeviceSyncSourceState>,
) -> Result<DeviceSyncSourceStatus, DeviceSyncErrorCategory> {
    state.status().map_err(|error| {
        device_sync_command_failure(
            state.inner(),
            "status",
            DeviceSyncErrorCategory::StateUnavailable,
            error,
        )
    })
}

#[cfg(desktop)]
#[tauri::command]
pub async fn device_sync_stop(
    state: State<'_, DeviceSyncSourceState>,
) -> Result<(), DeviceSyncErrorCategory> {
    let state = state.inner().clone();
    state.clear_error_category();
    let worker_state = state.clone();
    match tauri::async_runtime::spawn_blocking(move || worker_state.stop()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(device_sync_command_failure(
            &state,
            "stop",
            DeviceSyncErrorCategory::CleanupFailed,
            error,
        )),
        Err(error) => Err(device_sync_command_failure(
            &state,
            "stop worker",
            DeviceSyncErrorCategory::StateUnavailable,
            error,
        )),
    }
}

#[cfg(desktop)]
#[tauri::command]
pub async fn device_sync_rotate_link(
    state: State<'_, DeviceSyncSourceState>,
    permissions: DeviceSyncLinkPermissions,
) -> Result<DeviceSyncSourceStatus, DeviceSyncErrorCategory> {
    run_device_sync_rotate_link(state.inner().clone(), permissions).await
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) async fn device_sync_rotate_link(
    state: State<'_, AndroidDeviceSyncSourceState>,
    permissions: DeviceSyncLinkPermissions,
) -> Result<DeviceSyncSourceStatus, DeviceSyncErrorCategory> {
    let state = state.inner().clone();
    let worker_state = state.clone();
    match tauri::async_runtime::spawn_blocking(move || worker_state.rotate_link(permissions)).await
    {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => Err(android_device_sync_failure(
            &state,
            "link rotation",
            DeviceSyncErrorCategory::StateUnavailable,
            error,
        )),
        Err(error) => Err(android_device_sync_failure(
            &state,
            "link rotation worker",
            DeviceSyncErrorCategory::StateUnavailable,
            error,
        )),
    }
}

#[cfg(target_os = "android")]
fn android_device_sync_failure(
    state: &AndroidDeviceSyncSourceState,
    operation: &str,
    fallback: DeviceSyncErrorCategory,
    error: impl std::fmt::Display,
) -> DeviceSyncErrorCategory {
    crate::nlog!("error", "Android device sync {operation} failed: {error}");
    state
        .status()
        .ok()
        .and_then(|status| status.latest_error)
        .unwrap_or(fallback)
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) fn device_sync_source_reserve(
) -> Result<super::android_foreground::AndroidForegroundKey, String> {
    reserve_device_sync_source()
}

#[cfg(any(target_os = "android", test))]
pub(crate) fn reserve_device_sync_source(
) -> Result<super::android_foreground::AndroidForegroundKey, String> {
    super::android_foreground::registry()
        .reserve(super::android_foreground::AndroidForegroundLane::DeviceSyncSource)
        .map_err(|error| {
            crate::nlog!(
                "error",
                "Android device sync source reserve failed: {error}"
            );
            error
        })
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) async fn device_sync_prepare(
    app: AppHandle,
    state: State<'_, AndroidDeviceSyncSourceState>,
    request: DeviceSyncPrepareRequest,
) -> Result<DeviceSyncSourceStatus, DeviceSyncErrorCategory> {
    let state = state.inner().clone();
    let worker_state = state.clone();
    match tauri::async_runtime::spawn_blocking(move || {
        let app_root = app.path().app_data_dir().map_err(|error| {
            PeerSyncError::Storage(format!(
                "device sync application data directory is unavailable: {error}"
            ))
        })?;
        let cas = PayloadCas::new(&app_root)?;
        persistent_store::commands::with_store_mut(app.state(), |store| {
            let expected_bidirectional_revision = store.revision()?;
            let mut context = SharedSourcePreparationContext {
                store,
                cas: &cas,
                app_root: &app_root,
                cancellation: &crate::local_backup::NeverCancelled,
                expected_bidirectional_revision,
                remote_commit: worker_state.remote_commit_slot(),
            };
            worker_state
                .prepare(&mut context, &app_root, request)
                .map_err(|error| StoreError::Store {
                    message: error.to_string(),
                })
        })
        .map_err(|error| PeerSyncError::Storage(error.to_string()))
    })
    .await
    {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => Err(android_device_sync_failure(
            &state,
            "prepare",
            DeviceSyncErrorCategory::PreparationFailed,
            error,
        )),
        Err(error) => Err(android_device_sync_failure(
            &state,
            "prepare worker",
            DeviceSyncErrorCategory::StateUnavailable,
            error,
        )),
    }
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) async fn device_sync_start(
    state: State<'_, AndroidDeviceSyncSourceState>,
    permissions: DeviceSyncLinkPermissions,
    foreground: super::android_foreground::AndroidForegroundKey,
) -> Result<DeviceSyncSourceStatus, DeviceSyncErrorCategory> {
    let cancellation = match super::android_foreground::acquire_foreground_lane(
        &foreground,
        super::android_foreground::AndroidForegroundLane::DeviceSyncSource,
    )
    .await
    {
        Ok(cancellation) => cancellation,
        Err(error) => {
            return Err(android_device_sync_failure(
                state.inner(),
                "foreground attach",
                DeviceSyncErrorCategory::StateUnavailable,
                error,
            ))
        }
    };
    let state = state.inner().clone();
    let worker_state = state.clone();
    match tauri::async_runtime::spawn_blocking(move || {
        if cancellation.is_cancelled() {
            return Err(PeerSyncError::Cancelled);
        }
        worker_state.start_attached(permissions, foreground)
    })
    .await
    {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => Err(android_device_sync_failure(
            &state,
            "start",
            DeviceSyncErrorCategory::TransportUnavailable,
            error,
        )),
        Err(error) => Err(android_device_sync_failure(
            &state,
            "start worker",
            DeviceSyncErrorCategory::StateUnavailable,
            error,
        )),
    }
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) fn device_sync_status(
    state: State<'_, AndroidDeviceSyncSourceState>,
) -> Result<DeviceSyncSourceStatus, DeviceSyncErrorCategory> {
    state.status().map_err(|error| {
        android_device_sync_failure(
            state.inner(),
            "status",
            DeviceSyncErrorCategory::StateUnavailable,
            error,
        )
    })
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) async fn device_sync_stop(
    state: State<'_, AndroidDeviceSyncSourceState>,
) -> Result<Option<super::android_foreground::AndroidForegroundKey>, DeviceSyncErrorCategory> {
    let state = state.inner().clone();
    let worker_state = state.clone();
    match tauri::async_runtime::spawn_blocking(move || worker_state.stop()).await {
        Ok(Ok(foreground)) => Ok(foreground),
        Ok(Err(error)) => Err(android_device_sync_failure(
            &state,
            "stop",
            DeviceSyncErrorCategory::CleanupFailed,
            error,
        )),
        Err(error) => Err(android_device_sync_failure(
            &state,
            "stop worker",
            DeviceSyncErrorCategory::StateUnavailable,
            error,
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SharedPairingData {
    pub(crate) endpoint: String,
    pub(crate) session_id: String,
    pub(crate) manifest_id: String,
    pub(crate) claim: String,
    pub(crate) expires_at_ms: u128,
}

impl SharedPairingData {
    /// The registration data intentionally has no bearer credential.
    pub(crate) fn canonical_uri(&self) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("endpoint", &self.endpoint);
        query.append_pair("session", &self.session_id);
        query.append_pair("manifest", &self.manifest_id);
        format!(
            "risuailocal://peer-clone/v2?{}#claim={}",
            query.finish(),
            self.claim
        )
    }
}

struct SharedSessionHostInner {
    host: LanCloneHost,
    advertised_endpoint: Option<String>,
}

#[derive(Clone)]
pub(crate) struct SharedSessionHost {
    inner: Arc<Mutex<SharedSessionHostInner>>,
}

impl SharedSessionHost {
    pub(crate) fn new(
        clone: PreparedCloneSession,
        delta: PreparedLogicalLanSession,
        bidirectional: PreparedBidirectionalLogicalLanSession,
    ) -> Result<Self, PeerSyncError> {
        Ok(Self {
            inner: Arc::new(Mutex::new(SharedSessionHostInner {
                host: LanCloneHost::prepare_shared(clone, delta, bidirectional)?,
                advertised_endpoint: None,
            })),
        })
    }

    pub(crate) fn enable_v2_registry(
        &mut self,
        app_root: &std::path::Path,
        source_name: &str,
        permissions: DevicePermissions,
    ) -> Result<(), PeerSyncError> {
        self.inner()?
            .host
            .enable_v2_registry(app_root, source_name, permissions)
    }

    // Desktop advertises on every interface; Android binds its selected
    // private interface through start_private_lan.
    #[cfg(any(desktop, test))]
    pub(crate) fn start_fixed_lan(
        &mut self,
        advertised_address: Ipv4Addr,
        port: u16,
    ) -> Result<SharedPairingData, PeerSyncError> {
        if !advertised_address.is_private()
            && !advertised_address.is_link_local()
            && !advertised_address.is_loopback()
        {
            return Err(PeerSyncError::Validation(
                "shared LAN advertised address must be private or link-local IPv4".to_owned(),
            ));
        }
        let mut inner = self.inner()?;
        let pairing = inner
            .host
            .start_fixed_lan(port)
            .map_err(map_shared_listener_start_error)?;
        let data = Self::pairing_at(&inner.host, "http", advertised_address, pairing)?;
        inner.advertised_endpoint = Some(data.endpoint.clone());
        Ok(data)
    }

    // Only the Android device sync host binds an explicitly selected private
    // interface; desktop advertises through start_fixed_lan.
    #[cfg(any(target_os = "android", test))]
    pub(crate) fn start_private_lan(
        &mut self,
        address: Ipv4Addr,
        port: u16,
    ) -> Result<SharedPairingData, PeerSyncError> {
        if port == 0 {
            return Err(PeerSyncError::Validation(
                "shared LAN port must be nonzero".to_owned(),
            ));
        }
        if !address.is_private() && !address.is_link_local() {
            return Err(PeerSyncError::Validation(
                "LAN source address must be private or link-local IPv4".to_owned(),
            ));
        }
        // `LanCloneHost` owns the binding primitive. The private-interface
        // fixed-port entry point is intentionally kept in that same host.
        let mut inner = self.inner()?;
        let pairing = inner
            .host
            .start_fixed_on(address, port)
            .map_err(map_shared_listener_start_error)?;
        let data = Self::pairing(&inner.host, "http", pairing)?;
        inner.advertised_endpoint = Some(data.endpoint.clone());
        Ok(data)
    }

    #[cfg(desktop)]
    pub(crate) fn start_fixed_loopback(
        &mut self,
        port: u16,
    ) -> Result<SharedPairingData, PeerSyncError> {
        let mut inner = self.inner()?;
        let pairing = inner
            .host
            .start_fixed_loopback(port)
            .map_err(map_shared_listener_start_error)?;
        let data = Self::pairing(&inner.host, "http", pairing)?;
        inner.advertised_endpoint = Some(data.endpoint.clone());
        Ok(data)
    }

    pub(crate) fn rotate_link(&mut self) -> Result<SharedPairingData, PeerSyncError> {
        let inner = self.inner()?;
        let pairing = inner.host.rotate_pairing_link()?;
        let endpoint = inner.advertised_endpoint.clone().ok_or_else(|| {
            PeerSyncError::Protocol("shared LAN host has no advertised endpoint".to_owned())
        })?;
        Ok(SharedPairingData {
            endpoint,
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
            claim: pairing.claim,
            expires_at_ms: inner.host.pairing_expires_at_ms().ok_or_else(|| {
                PeerSyncError::Protocol("shared LAN host has no pending pairing link".to_owned())
            })?,
        })
    }

    pub(crate) fn address(&self) -> Option<SocketAddr> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.host.address())
    }

    pub(crate) fn stop(&mut self) -> Result<(), PeerSyncError> {
        let mut inner = self.inner()?;
        let result = inner.host.stop();
        inner.advertised_endpoint = None;
        result
    }

    #[cfg(test)]
    pub(crate) fn expire_link_for_test(&self) {
        if let Ok(inner) = self.inner.lock() {
            inner.host.expire_claim_for_test();
        }
    }

    fn pairing(
        host: &LanCloneHost,
        scheme: &str,
        pairing: LanPairing,
    ) -> Result<SharedPairingData, PeerSyncError> {
        let address = host.address().ok_or_else(|| {
            PeerSyncError::Protocol("shared LAN host did not expose an address".to_owned())
        })?;
        Ok(SharedPairingData {
            endpoint: format!("{scheme}://{address}"),
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
            claim: pairing.claim,
            expires_at_ms: host.pairing_expires_at_ms().ok_or_else(|| {
                PeerSyncError::Protocol("shared LAN host has no pending pairing link".to_owned())
            })?,
        })
    }

    fn pairing_at(
        host: &LanCloneHost,
        scheme: &str,
        advertised_address: Ipv4Addr,
        pairing: LanPairing,
    ) -> Result<SharedPairingData, PeerSyncError> {
        let port = host
            .address()
            .ok_or_else(|| {
                PeerSyncError::Protocol("shared LAN host did not expose an address".to_owned())
            })?
            .port();
        Ok(SharedPairingData {
            endpoint: format!("{scheme}://{advertised_address}:{port}"),
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
            claim: pairing.claim,
            expires_at_ms: host.pairing_expires_at_ms().ok_or_else(|| {
                PeerSyncError::Protocol("shared LAN host has no pending pairing link".to_owned())
            })?,
        })
    }

    fn set_advertised_endpoint(&self, endpoint: String) -> Result<(), PeerSyncError> {
        self.inner()?.advertised_endpoint = Some(endpoint);
        Ok(())
    }

    fn revoke_registered_device(&self, device_id: &str) {
        if let Ok(inner) = self.inner.lock() {
            inner.host.revoke(device_id);
        }
    }

    fn set_pairing_permissions(&self, permissions: DevicePermissions) -> Result<(), PeerSyncError> {
        self.inner()?.host.set_v2_pairing_permissions(permissions)
    }

    #[cfg(desktop)]
    pub(crate) fn issue_tunnel_probe(
        &self,
    ) -> Result<super::lan::TunnelOriginProbe, PeerSyncError> {
        self.inner()?.host.issue_tunnel_probe()
    }

    #[cfg(desktop)]
    pub(crate) fn clear_tunnel_probe(&self) {
        if let Ok(inner) = self.inner.lock() {
            inner.host.clear_tunnel_probe();
        }
    }

    #[cfg(desktop)]
    fn verify_public_origin(&self, public_url: &url::Url) -> Result<(), PeerSyncError> {
        let mut shared = self.clone();
        super::tunnel::verify_public_origin(&mut shared, public_url).map_err(|error| {
            crate::nlog!("error", "shared fixed URL origin probe failed: {error}");
            PeerSyncError::Transport("shared fixed URL origin probe failed".to_owned())
        })
    }

    fn inner(&self) -> Result<std::sync::MutexGuard<'_, SharedSessionHostInner>, PeerSyncError> {
        self.inner
            .lock()
            .map_err(|error| PeerSyncError::Storage(format!("shared host mutex poisoned: {error}")))
    }
}

fn map_shared_listener_start_error(error: PeerSyncError) -> PeerSyncError {
    if matches!(error, PeerSyncError::Transport(_)) {
        crate::nlog!("error", "shared listener start failed: {error}");
        PeerSyncError::Transport("shared fixed port is unavailable".to_owned())
    } else {
        error
    }
}
