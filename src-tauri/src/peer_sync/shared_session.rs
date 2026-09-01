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
use std::sync::Mutex;

#[cfg(desktop)]
use super::{
    bidirectional_commands::{
        prepare_shared_bidirectional_source, PreparedSharedBidirectionalSource,
    },
    commands::{prepare_shared_clone_source, PreparedSharedCloneSource},
    delta_commands::{prepare_shared_delta_source, PreparedSharedDeltaSource},
};
#[cfg(desktop)]
use crate::{
    asset_repository::PayloadCas, local_backup::CancellationProbe,
    persistent_store::PersistentStore,
};

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
        // Prepared sessions can own open package and generation resources.
        // Drop the common host before lane cleanup removes their artifacts.
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
        let has_remaining = !self.runtime()?.acquired.is_empty();
        self.set_phase(if has_remaining {
            SharedSessionPhase::Stopping
        } else {
            SharedSessionPhase::Idle
        })?;
        primary.map_or(Ok(()), Err)
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
#[cfg(desktop)]
pub(crate) struct SharedSourceEngines {
    clone: Option<PreparedSharedCloneSource>,
    delta: Option<PreparedSharedDeltaSource>,
    bidirectional: Option<PreparedSharedBidirectionalSource>,
}

#[cfg(desktop)]
impl SharedSourceEngines {
    pub(crate) fn new() -> Self {
        Self {
            clone: None,
            delta: None,
            bidirectional: None,
        }
    }
}

#[cfg(desktop)]
pub(crate) struct SharedSourcePreparationContext<'a> {
    pub(crate) store: &'a mut PersistentStore,
    pub(crate) cas: &'a PayloadCas,
    pub(crate) app_root: &'a std::path::Path,
    pub(crate) cancellation: &'a dyn CancellationProbe,
    pub(crate) expected_bidirectional_revision: i64,
}

#[cfg(desktop)]
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

#[cfg(desktop)]
impl<'a> SharedSourcePreparation<SharedSourcePreparationContext<'a>> for SharedSourceEngines {
    fn prepare(
        &mut self,
        lane: SharedSourceLane,
        context: &mut SharedSourcePreparationContext<'a>,
    ) -> Result<(), PeerSyncError> {
        match lane {
            SharedSourceLane::Clone => {
                self.clone = Some(prepare_shared_clone_source(
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
                )?);
            }
        }
        Ok(())
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

pub(crate) struct SharedSessionHost {
    host: LanCloneHost,
    advertised_endpoint: Option<String>,
}

impl SharedSessionHost {
    pub(crate) fn new(
        clone: PreparedCloneSession,
        delta: PreparedLogicalLanSession,
        bidirectional: PreparedBidirectionalLogicalLanSession,
    ) -> Result<Self, PeerSyncError> {
        Ok(Self {
            host: LanCloneHost::prepare_shared(clone, delta, bidirectional)?,
            advertised_endpoint: None,
        })
    }

    pub(crate) fn enable_v2_registry(
        &mut self,
        app_root: &std::path::Path,
        source_name: &str,
        permissions: DevicePermissions,
    ) -> Result<(), PeerSyncError> {
        self.host
            .enable_v2_registry(app_root, source_name, permissions)
    }

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
        let pairing = self.host.start_fixed_lan(port)?;
        let data = self.pairing_at("http", advertised_address, pairing)?;
        self.advertised_endpoint = Some(data.endpoint.clone());
        Ok(data)
    }

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
        let pairing = self.host.start_fixed_on(address, port)?;
        let data = self.pairing("http", pairing)?;
        self.advertised_endpoint = Some(data.endpoint.clone());
        Ok(data)
    }

    #[cfg(desktop)]
    pub(crate) fn start_fixed_loopback(
        &mut self,
        port: u16,
    ) -> Result<SharedPairingData, PeerSyncError> {
        let pairing = self.host.start_fixed_loopback(port)?;
        let data = self.pairing("http", pairing)?;
        self.advertised_endpoint = Some(data.endpoint.clone());
        Ok(data)
    }

    pub(crate) fn rotate_link(&mut self) -> Result<SharedPairingData, PeerSyncError> {
        let pairing = self.host.rotate_pairing_link()?;
        let endpoint = self.advertised_endpoint.clone().ok_or_else(|| {
            PeerSyncError::Protocol("shared LAN host has no advertised endpoint".to_owned())
        })?;
        Ok(SharedPairingData {
            endpoint,
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
            claim: pairing.claim,
            expires_at_ms: self.host.pairing_expires_at_ms().ok_or_else(|| {
                PeerSyncError::Protocol("shared LAN host has no pending pairing link".to_owned())
            })?,
        })
    }

    pub(crate) fn address(&self) -> Option<SocketAddr> {
        self.host.address()
    }

    pub(crate) fn stop(&mut self) -> Result<(), PeerSyncError> {
        let result = self.host.stop();
        self.advertised_endpoint = None;
        result
    }

    #[cfg(test)]
    pub(crate) fn expire_link_for_test(&self) {
        self.host.expire_claim_for_test();
    }

    fn pairing(
        &self,
        scheme: &str,
        pairing: LanPairing,
    ) -> Result<SharedPairingData, PeerSyncError> {
        let address = self.host.address().ok_or_else(|| {
            PeerSyncError::Protocol("shared LAN host did not expose an address".to_owned())
        })?;
        Ok(SharedPairingData {
            endpoint: format!("{scheme}://{address}"),
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
            claim: pairing.claim,
            expires_at_ms: self.host.pairing_expires_at_ms().ok_or_else(|| {
                PeerSyncError::Protocol("shared LAN host has no pending pairing link".to_owned())
            })?,
        })
    }

    fn pairing_at(
        &self,
        scheme: &str,
        advertised_address: Ipv4Addr,
        pairing: LanPairing,
    ) -> Result<SharedPairingData, PeerSyncError> {
        let port = self
            .host
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
            expires_at_ms: self.host.pairing_expires_at_ms().ok_or_else(|| {
                PeerSyncError::Protocol("shared LAN host has no pending pairing link".to_owned())
            })?,
        })
    }
}
