use crate::{Error, Result};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Clone, Debug)]
pub struct Config {
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    /// Explicitly acknowledge an HTTPS terminator forwarding to this loopback listener.
    pub https_proxy: bool,
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if !self.data_dir.is_absolute() {
            return Err(Error::new("absolute-data-dir-required", 400));
        }
        // Initial transport exposes no cleartext LAN/public bind, even with the proxy flag.
        if !self.listen.ip().is_loopback() {
            return Err(Error::new("loopback-listener-required", 400));
        }
        Ok(())
    }
}
