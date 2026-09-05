//! Shared quick tunnel launcher for the unified device sync source.

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
#[cfg(desktop)]
use std::sync::Mutex;
#[cfg(desktop)]
use std::time::Duration;

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
