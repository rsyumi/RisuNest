use super::Store;
use crate::{Error, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedDevice {
    pub id: String,
    pub name: String,
    pub revoked: bool,
    pub pending: bool,
    pub registration_request: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagementConnection {
    pub endpoint: Option<String>,
    pub cloudflared: Option<PathBuf>,
    pub registry_url: Option<String>,
    pub registry_enabled: bool,
    pub uuid: Option<String>,
}

impl Store {
    pub fn managed_devices(&self) -> Result<Vec<ManagedDevice>> {
        let db = self.reader()?;
        let mut query = db.prepare("SELECT id,name,revoked,EXISTS(SELECT 1 FROM commit_jobs WHERE device=devices.id),registration_request FROM devices ORDER BY rowid")?;
        let rows = query.query_map([], |row| {
            Ok(ManagedDevice {
                id: row.get(0)?,
                name: row.get(1)?,
                revoked: row.get(2)?,
                pending: row.get(3)?,
                registration_request: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn management_connection(&self) -> Result<ManagementConnection> {
        let _gate = self
            .connection_gate
            .lock()
            .map_err(|_| Error::new("connection-state-unavailable", 503))?;
        let state = self.read_connection()?;
        Ok(ManagementConnection {
            endpoint: state.endpoint,
            cloudflared: state.cloudflared,
            registry_url: state.directory.as_ref().map(|d| d.base_url.clone()),
            registry_enabled: state.directory_enabled,
            uuid: state.directory.as_ref().map(|d| d.uuid.clone()),
        })
    }
    pub fn data_path(&self) -> &Path {
        &self.root
    }
}

pub(crate) fn protect(bytes: &[u8], seal: bool) -> Result<Vec<u8>> {
    super::connection::protect(bytes, seal)
}
