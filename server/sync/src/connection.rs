//! Configuration and secret-free status consumed by CLI and local management clients.
use crate::{Error, Result};
use risunest_sync_connect::{validate_endpoint, Directory};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionOptions {
    pub endpoint: Option<String>,
    pub cloudflared: Option<PathBuf>,
    pub registry_url: Option<String>,
}
impl ConnectionOptions {
    pub fn validate(&self) -> Result<()> {
        if self.endpoint.is_some() == self.cloudflared.is_some() {
            return Err(Error::new("choose-fixed-or-managed-tunnel", 400));
        }
        if let Some(endpoint) = &self.endpoint {
            validate_endpoint(endpoint, self.registry_url.is_none())?;
        }
        if let Some(path) = &self.cloudflared {
            if !path.is_absolute() || !path.is_file() {
                return Err(Error::new("absolute-cloudflared-executable-required", 400));
            }
        }
        if let Some(url) = &self.registry_url {
            validate_endpoint(url, true)?;
        }
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionStatus {
    pub mode: &'static str,
    /// Last known public address, not a claim that the Tunnel is currently running.
    pub endpoint: Option<String>,
    pub directory_enabled: bool,
    pub publication: &'static str,
}

/// Private request material; must not be serialized into public status or logs.
#[derive(Clone)]
pub struct Publication {
    pub directory: Directory,
    pub envelope: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PendingPublication {
    pub endpoint: String,
    pub envelope: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConnectionState {
    pub directory_enabled: bool,
    pub endpoint: Option<String>,
    pub cloudflared: Option<PathBuf>,
    pub directory: Option<Directory>,
    pub last_published: Option<String>,
    pub last_published_at: i64,
    pub pending: Option<PendingPublication>,
}
impl ConnectionState {
    pub fn validate(&self) -> Result<()> {
        if let Some(endpoint) = &self.endpoint {
            validate_endpoint(endpoint, !self.directory_enabled)?;
        }
        if let Some(path) = &self.cloudflared {
            if !path.is_absolute() {
                return Err(Error::new("invalid-connection-state", 409));
            }
        }
        if let Some(directory) = &self.directory {
            directory.validate()?;
        }
        if self.directory_enabled && self.directory.is_none() {
            return Err(Error::new("invalid-connection-state", 409));
        }
        if let Some(last) = &self.last_published {
            validate_endpoint(last, false)?;
        }
        if let Some(pending) = &self.pending {
            let directory = self
                .directory
                .as_ref()
                .ok_or(Error::new("invalid-connection-state", 409))?;
            if risunest_sync_connect::open_endpoint(
                &directory.uuid,
                &directory.key,
                &pending.envelope,
            )? != pending.endpoint
                || Some(&pending.endpoint) != self.endpoint.as_ref()
            {
                return Err(Error::new("invalid-connection-state", 409));
            }
        }
        Ok(())
    }
}
