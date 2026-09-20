use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
};

pub const DEFAULT_PORT: u16 = 14319;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkSettings {
    pub schema: u32,
    pub address: IpAddr,
    pub port: u16,
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            schema: 1,
            address: IpAddr::from([127, 0, 0, 1]),
            port: DEFAULT_PORT,
        }
    }
}

impl NetworkSettings {
    pub fn socket(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.port)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != 1 || self.port == 0 || self.address.is_multicast() {
            return Err(Error::new("invalid-network-settings", 400));
        }
        Ok(())
    }

    pub fn load(root: &Path) -> Result<Self> {
        let value = match std::fs::read(root.join("network.json")) {
            Ok(bytes) => serde_json::from_slice::<Self>(&bytes)
                .map_err(|_| Error::new("invalid-network-settings", 400))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error.into()),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        self.validate()?;
        std::fs::create_dir_all(root)?;
        let mut staged = tempfile::NamedTempFile::new_in(root)?;
        staged.write_all(
            &serde_json::to_vec(self).map_err(|_| Error::new("invalid-network-settings", 400))?,
        )?;
        staged.as_file().sync_all()?;
        staged
            .persist(root.join("network.json"))
            .map_err(|e| Error::from(e.error))?;
        Ok(())
    }
}

pub fn tunnel_origin(listener: SocketAddr) -> SocketAddr {
    if listener.ip().is_unspecified() {
        SocketAddr::new(
            if listener.is_ipv4() {
                IpAddr::from([127, 0, 0, 1])
            } else {
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
            },
            listener.port(),
        )
    } else {
        listener
    }
}

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
        if self.listen.ip().is_multicast() {
            return Err(Error::new("invalid-listen-address", 400));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_network_settings_and_rejects_invalid_values() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            NetworkSettings::load(root.path())
                .unwrap()
                .socket()
                .to_string(),
            "127.0.0.1:14319"
        );
        let mut value = NetworkSettings {
            address: "0.0.0.0".parse().unwrap(),
            port: 24319,
            ..Default::default()
        };
        value.save(root.path()).unwrap();
        assert_eq!(NetworkSettings::load(root.path()).unwrap(), value);
        value.port = 0;
        assert!(value.save(root.path()).is_err());
        assert_eq!(NetworkSettings::load(root.path()).unwrap().port, 24319);
        std::fs::write(root.path().join("network.json"), b"{}").unwrap();
        assert!(NetworkSettings::load(root.path()).is_err());
    }

    #[test]
    fn tunnel_targets_a_reachable_address_for_wildcard_and_specific_binds() {
        for (input, expected) in [
            ("0.0.0.0:14319", "127.0.0.1:14319"),
            ("[::]:14319", "[::1]:14319"),
            ("192.0.2.1:14319", "192.0.2.1:14319"),
        ] {
            assert_eq!(tunnel_origin(input.parse().unwrap()).to_string(), expected);
        }
    }
}
