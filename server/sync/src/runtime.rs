use crate::{
    publication::{PublicationStatus, Publisher},
    store::Store,
    tunnel::{self, TunnelStatus},
    Result,
};
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    sync::{watch, Notify},
    task::JoinHandle,
};

/// Owned by the serving daemon. Dropping any GUI subscriber does not stop it.
pub struct ConnectionRuntime {
    stop: watch::Sender<bool>,
    changed: Arc<Notify>,
    pub tunnel: watch::Receiver<TunnelStatus>,
    pub publication: watch::Receiver<PublicationStatus>,
    publisher_task: JoinHandle<()>,
    tunnel_task: JoinHandle<()>,
}
impl ConnectionRuntime {
    /// The caller must already own Store and a bound, serving loopback listener.
    pub fn start(store: Arc<Store>, origin: SocketAddr) -> Result<Self> {
        if !origin.ip().is_loopback() {
            return Err(crate::Error::new("invalid-tunnel-origin", 400));
        }
        let publisher = Publisher::new()?;
        let publication = publisher.subscribe();
        let (stop, stopped) = watch::channel(false);
        let changed = Arc::new(Notify::new());
        let (tunnel_status, tunnel) = watch::channel(TunnelStatus {
            phase: "starting",
            endpoint: None,
            error: None,
        });
        let managed = store.managed_cloudflared()?.is_some();
        let publisher_task =
            tokio::spawn(publisher.run(store.clone(), changed.clone(), stopped.clone(), managed));
        let tunnel_task = tokio::spawn(tunnel::supervise(
            store,
            origin,
            changed.clone(),
            tunnel_status,
            stopped,
        ));
        Ok(Self {
            stop,
            changed,
            tunnel,
            publication,
            publisher_task,
            tunnel_task,
        })
    }
    /// Notify after a successful domain operation changes publication state.
    pub fn publication_changed(&self) {
        self.changed.notify_one();
    }
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.tunnel_task.await;
        let _ = self.publisher_task.await;
    }
}
