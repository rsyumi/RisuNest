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
}

impl SharedSessionHost {
    pub(crate) fn new(
        clone: PreparedCloneSession,
        delta: PreparedLogicalLanSession,
        bidirectional: PreparedBidirectionalLogicalLanSession,
    ) -> Result<Self, PeerSyncError> {
        Ok(Self {
            host: LanCloneHost::prepare_shared(clone, delta, bidirectional)?,
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
        self.pairing_at("http", advertised_address, pairing)
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
        self.pairing("http", pairing)
    }

    #[cfg(desktop)]
    pub(crate) fn start_fixed_loopback(
        &mut self,
        port: u16,
    ) -> Result<SharedPairingData, PeerSyncError> {
        let pairing = self.host.start_fixed_loopback(port)?;
        self.pairing("http", pairing)
    }

    pub(crate) fn rotate_link(&self) -> Result<SharedPairingData, PeerSyncError> {
        self.pairing("http", self.host.rotate_pairing_link()?)
    }

    pub(crate) fn address(&self) -> Option<SocketAddr> {
        self.host.address()
    }

    pub(crate) fn stop(&mut self) -> Result<(), PeerSyncError> {
        self.host.stop()
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
