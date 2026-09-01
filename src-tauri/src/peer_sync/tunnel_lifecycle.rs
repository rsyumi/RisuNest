//! Shared tunnel lifecycle scaffolding for the peer sync command lanes.
//!
//! The clone, delta, and bidirectional lanes expose the same tunnel wire
//! contract (start request, kind, metadata, phase) and the clone and delta
//! lanes drive cloudflared through the same wrapper trio; each lane keeps
//! only its lane label for user-facing error strings.

#[cfg(desktop)]
use super::lan::LanCloneHost;
#[cfg(desktop)]
use super::tunnel::{self, RunningTunnelLifecycle, SystemTunnelProcess, TunnelStartFailure};
#[cfg(desktop)]
use super::{
    shared_session::{
        SharedPeerTunnel, SharedPeerTunnelCleanup, SharedPeerTunnelLauncher,
        SharedPeerTunnelLifecycle, SharedSessionHost,
    },
    PeerSyncError,
};
use serde::{Deserialize, Serialize};
#[cfg(desktop)]
use std::sync::Mutex;
#[cfg(desktop)]
use std::time::Duration;

// Android lanes never start tunnels; their sources pair over the trusted LAN.
#[cfg_attr(target_os = "android", allow(dead_code))]
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PeerTunnelStart {
    Quick,
    Named {
        token: String,
        #[serde(rename = "expectedPublicBaseUrl")]
        expected_public_base_url: String,
    },
}

#[cfg_attr(target_os = "android", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerTunnelKind {
    Quick,
    Named,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerTunnelMetadata {
    pub(crate) kind: PeerTunnelKind,
    experimental: bool,
    one_shot: bool,
}

#[cfg_attr(target_os = "android", allow(dead_code))]
impl PeerTunnelMetadata {
    pub(crate) fn quick() -> Self {
        Self {
            kind: PeerTunnelKind::Quick,
            experimental: true,
            one_shot: true,
        }
    }

    pub(crate) fn named() -> Self {
        Self {
            kind: PeerTunnelKind::Named,
            experimental: false,
            one_shot: false,
        }
    }
}

#[cfg_attr(target_os = "android", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerTunnelPhase {
    Idle,
    Starting,
    Running,
    Stopping,
    Stopped,
}

#[cfg(desktop)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerTunnelLifecycle {
    Running,
    CleanupPending,
    Stopped,
}

#[cfg(desktop)]
pub(crate) trait PeerTunnel: Send {
    fn transport_url(&self) -> url::Url;
    fn lifecycle(&mut self) -> Result<PeerTunnelLifecycle, String>;
    fn stop(&mut self) -> Result<(), String>;
}

#[cfg(desktop)]
pub(crate) trait FailedPeerTunnel: Send {
    fn recover_host(&mut self) -> Result<Option<LanCloneHost>, String>;
    fn stop(&mut self) -> Result<(), String>;
}

#[cfg(desktop)]
pub(crate) trait PeerTunnelLauncher: Send + Sync {
    fn start(
        &self,
        request: PeerTunnelStart,
        host: LanCloneHost,
    ) -> Result<Box<dyn PeerTunnel>, Box<dyn FailedPeerTunnel>>;
}

#[cfg(desktop)]
pub(crate) struct SystemPeerTunnel {
    tunnel: tunnel::RunningTunnel,
    lane: &'static str,
}

#[cfg(desktop)]
impl PeerTunnel for SystemPeerTunnel {
    fn transport_url(&self) -> url::Url {
        self.tunnel.transport_url().clone()
    }

    fn lifecycle(&mut self) -> Result<PeerTunnelLifecycle, String> {
        self.tunnel
            .poll_lifecycle()
            .map(|lifecycle| match lifecycle {
                RunningTunnelLifecycle::Running => PeerTunnelLifecycle::Running,
                RunningTunnelLifecycle::CleanupPending => PeerTunnelLifecycle::CleanupPending,
                RunningTunnelLifecycle::Stopped => PeerTunnelLifecycle::Stopped,
            })
            .map_err(|_| format!("peer {} tunnel status is unavailable", self.lane))
    }

    fn stop(&mut self) -> Result<(), String> {
        self.tunnel
            .stop(Duration::from_secs(2))
            .map_err(|_| format!("peer {} tunnel failed to stop", self.lane))
    }
}

#[cfg(desktop)]
type SystemPeerTunnelStartFailure = TunnelStartFailure<SystemTunnelProcess, LanCloneHost>;

#[cfg(desktop)]
pub(crate) struct SystemFailedPeerTunnel {
    failure: Option<SystemPeerTunnelStartFailure>,
    lane: &'static str,
}

#[cfg(desktop)]
impl FailedPeerTunnel for SystemFailedPeerTunnel {
    fn recover_host(&mut self) -> Result<Option<LanCloneHost>, String> {
        let Some(failure) = self.failure.take() else {
            return Ok(None);
        };
        match failure.retry_into_peer_session() {
            Ok(host) => Ok(Some(host)),
            Err(failure) => {
                self.failure = Some(failure);
                Err(format!("peer {} tunnel cleanup is pending", self.lane))
            }
        }
    }

    fn stop(&mut self) -> Result<(), String> {
        let Some(failure) = self.failure.as_mut() else {
            return Ok(());
        };
        failure
            .retry_cleanup()
            .map_err(|_| format!("peer {} tunnel failed to stop", self.lane))?;
        self.failure = None;
        Ok(())
    }
}

#[cfg(desktop)]
pub(crate) struct SystemPeerTunnelLauncher {
    pub(crate) lane: &'static str,
}

#[cfg(desktop)]
type SystemSharedPeerTunnelStartFailure =
    TunnelStartFailure<SystemTunnelProcess, SharedSessionHost>;

#[cfg(desktop)]
#[derive(Default)]
pub(crate) struct SystemSharedPeerTunnelLauncher {
    failed: Mutex<Option<SystemSharedPeerTunnelStartFailure>>,
}

#[cfg(desktop)]
struct SystemSharedPeerTunnel {
    tunnel: tunnel::RunningTunnel,
}

#[cfg(desktop)]
struct SystemFailedSharedPeerTunnel {
    failure: Option<SystemSharedPeerTunnelStartFailure>,
}

#[cfg(desktop)]
impl SharedPeerTunnel for SystemSharedPeerTunnel {
    fn transport_url(&self) -> url::Url {
        self.tunnel.transport_url().clone()
    }

    fn lifecycle(&mut self) -> Result<SharedPeerTunnelLifecycle, PeerSyncError> {
        self.tunnel
            .poll_lifecycle()
            .map(|lifecycle| match lifecycle {
                RunningTunnelLifecycle::Running => SharedPeerTunnelLifecycle::Running,
                RunningTunnelLifecycle::CleanupPending => SharedPeerTunnelLifecycle::CleanupPending,
                RunningTunnelLifecycle::Stopped => SharedPeerTunnelLifecycle::Stopped,
            })
            .map_err(|error| {
                crate::nlog!("error", "shared quick tunnel status failed: {error}");
                PeerSyncError::Transport("shared quick tunnel status is unavailable".to_owned())
            })
    }

    fn stop(&mut self) -> Result<(), PeerSyncError> {
        self.tunnel.stop(Duration::from_secs(2)).map_err(|error| {
            crate::nlog!("error", "shared quick tunnel stop failed: {error}");
            PeerSyncError::Transport("shared quick tunnel failed to stop".to_owned())
        })
    }
}

#[cfg(desktop)]
impl SharedPeerTunnelCleanup for SystemFailedSharedPeerTunnel {
    fn stop(&mut self) -> Result<(), PeerSyncError> {
        let Some(failure) = self.failure.as_mut() else {
            return Ok(());
        };
        failure.retry_cleanup().map_err(|error| {
            crate::nlog!("error", "shared quick tunnel start cleanup failed: {error}");
            PeerSyncError::Transport("shared quick tunnel failed to stop".to_owned())
        })?;
        self.failure = None;
        Ok(())
    }
}

#[cfg(desktop)]
impl SharedPeerTunnelLauncher for SystemSharedPeerTunnelLauncher {
    fn start(&self, host: SharedSessionHost) -> Result<Box<dyn SharedPeerTunnel>, PeerSyncError> {
        let origin = host.address().ok_or_else(|| {
            PeerSyncError::Transport("shared quick tunnel origin is unavailable".to_owned())
        })?;
        tunnel::start_quick_desktop_tunnel_session(host, origin)
            .map(|tunnel| Box::new(SystemSharedPeerTunnel { tunnel }) as Box<dyn SharedPeerTunnel>)
            .map_err(|failure| {
                crate::nlog!(
                    "error",
                    "shared quick tunnel start failed: {}",
                    failure.error_message()
                );
                match self.failed.lock() {
                    Ok(mut failed) => *failed = Some(failure),
                    Err(error) => {
                        crate::nlog!(
                            "error",
                            "shared quick tunnel cleanup owner lock failed: {error}"
                        );
                    }
                }
                PeerSyncError::Transport("shared quick tunnel failed to start".to_owned())
            })
    }

    fn take_failed_cleanup(&self) -> Option<Box<dyn SharedPeerTunnelCleanup>> {
        self.failed.lock().ok().and_then(|mut failed| {
            failed.take().map(|failure| {
                Box::new(SystemFailedSharedPeerTunnel {
                    failure: Some(failure),
                }) as Box<dyn SharedPeerTunnelCleanup>
            })
        })
    }
}

#[cfg(desktop)]
impl PeerTunnelLauncher for SystemPeerTunnelLauncher {
    fn start(
        &self,
        request: PeerTunnelStart,
        host: LanCloneHost,
    ) -> Result<Box<dyn PeerTunnel>, Box<dyn FailedPeerTunnel>> {
        let started = match request {
            PeerTunnelStart::Quick => tunnel::start_quick_desktop_tunnel(host),
            PeerTunnelStart::Named {
                token,
                expected_public_base_url,
            } => tunnel::start_named_desktop_tunnel(host, token, &expected_public_base_url),
        };
        let lane = self.lane;
        started
            .map(|running| {
                Box::new(SystemPeerTunnel {
                    tunnel: running,
                    lane,
                }) as Box<dyn PeerTunnel>
            })
            .map_err(|failure| {
                Box::new(SystemFailedPeerTunnel {
                    failure: Some(failure),
                    lane,
                }) as Box<dyn FailedPeerTunnel>
            })
    }
}

/// Builds the lane's pairing link. The scheme path segment (peer-clone,
/// peer-delta, peer-sync) is the lane's wire identity; the TypeScript pairing
/// parser consumes all three.
pub(crate) fn build_lane_pairing_uri(
    lane_path: &str,
    endpoint: &str,
    pairing: &super::LanPairing,
) -> Result<String, super::PeerSyncError> {
    let mut uri = url::Url::parse(&format!("risuailocal://{lane_path}/v1"))
        .map_err(|error| super::PeerSyncError::Protocol(error.to_string()))?;
    uri.query_pairs_mut()
        .append_pair("endpoint", endpoint)
        .append_pair("session", &pairing.session_id)
        .append_pair("manifest", &pairing.manifest_id);
    uri.set_fragment(Some(&format!("claim={}", pairing.claim)));
    Ok(uri.to_string())
}
