use super::PeerSyncError;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

const MAX_REGISTRY_BYTES: usize = 1024 * 1024;
const OUTGOING_SCHEMA: &str = "risunest.peer-device-registry/v1";
const INCOMING_SCHEMA: &str = "risunest.peer-source-registry/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DevicePermissions(Vec<String>);

impl DevicePermissions {
    pub(crate) fn read() -> Self {
        Self(vec!["read".to_owned()])
    }

    pub(crate) fn read_and_bidirectional() -> Self {
        Self(vec!["read".to_owned(), "bidirectional".to_owned()])
    }

    pub(crate) fn allows_read(&self) -> bool {
        self.0.iter().any(|permission| permission == "read")
    }

    pub(crate) fn allows_bidirectional(&self) -> bool {
        self.0
            .iter()
            .any(|permission| permission == "bidirectional")
    }

    fn validate(&self) -> Result<(), PeerSyncError> {
        if !self.allows_read()
            || self
                .0
                .iter()
                .any(|permission| !matches!(permission.as_str(), "read" | "bidirectional"))
        {
            return Err(PeerSyncError::Validation(
                "invalid peer device permissions".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OutgoingDevice {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) bearer_digest: String,
    pub(crate) permissions: DevicePermissions,
    pub(crate) created_at_ms: u64,
    pub(crate) last_seen_ms: u64,
    pub(crate) total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct IncomingSource {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) endpoint: String,
    pub(crate) bearer: String,
    pub(crate) permissions: DevicePermissions,
    pub(crate) last_seen_ms: u64,
    pub(crate) total_bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OutgoingFile {
    schema: String,
    devices: Vec<OutgoingDevice>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IncomingFile {
    schema: String,
    sources: Vec<IncomingSource>,
}

pub(crate) struct OutgoingDeviceRegistry {
    root: PathBuf,
    devices: Vec<OutgoingDevice>,
}

impl OutgoingDeviceRegistry {
    pub(crate) fn load(app_root: &Path) -> Result<Self, PeerSyncError> {
        let root = peer_root(app_root);
        let path = root.join("devices.json");
        let devices = match read_registry(&path)? {
            None => Vec::new(),
            Some(bytes) => {
                let file: OutgoingFile = parse_registry(&bytes)?;
                if file.schema != OUTGOING_SCHEMA {
                    return invalid("unsupported outgoing peer device registry schema");
                }
                file.devices
            }
        };
        validate_outgoing(&devices)?;
        Ok(Self { root, devices })
    }

    pub(crate) fn devices(&self) -> &[OutgoingDevice] {
        &self.devices
    }

    pub(crate) fn upsert(&mut self, device: OutgoingDevice) -> Result<(), PeerSyncError> {
        validate_outgoing(std::slice::from_ref(&device))?;
        if let Some(existing) = self
            .devices
            .iter_mut()
            .find(|item| item.device_id == device.device_id)
        {
            *existing = device;
        } else {
            self.devices.push(device);
        }
        Ok(())
    }

    pub(crate) fn remove(&mut self, device_id: &str) -> Result<(), PeerSyncError> {
        self.devices.retain(|item| item.device_id != device_id);
        Ok(())
    }

    pub(crate) fn record_completed_bytes(
        &mut self,
        device_id: &str,
        bytes: u64,
    ) -> Result<(), PeerSyncError> {
        let device = self
            .devices
            .iter_mut()
            .find(|item| item.device_id == device_id)
            .ok_or_else(|| {
                PeerSyncError::Validation("registered outgoing device is missing".to_owned())
            })?;
        device.total_bytes = device
            .total_bytes
            .checked_add(bytes)
            .ok_or_else(|| PeerSyncError::Validation("peer total bytes overflow".to_owned()))?;
        Ok(())
    }

    pub(crate) fn save(&self) -> Result<(), PeerSyncError> {
        write_registry(
            &self.root.join("devices.json"),
            &OutgoingFile {
                schema: OUTGOING_SCHEMA.to_owned(),
                devices: self.devices.clone(),
            },
        )
    }
}

pub(crate) struct IncomingSourceRegistry {
    root: PathBuf,
    sources: Vec<IncomingSource>,
}

impl IncomingSourceRegistry {
    pub(crate) fn load(app_root: &Path) -> Result<Self, PeerSyncError> {
        let root = peer_root(app_root);
        let path = root.join("sources.json");
        let sources = match read_registry(&path)? {
            None => Vec::new(),
            Some(bytes) => {
                let file: IncomingFile = parse_registry(&bytes)?;
                if file.schema != INCOMING_SCHEMA {
                    return invalid("unsupported incoming peer source registry schema");
                }
                file.sources
            }
        };
        validate_incoming(&sources)?;
        Ok(Self { root, sources })
    }

    pub(crate) fn sources(&self) -> &[IncomingSource] {
        &self.sources
    }

    pub(crate) fn upsert(&mut self, source: IncomingSource) -> Result<(), PeerSyncError> {
        validate_incoming(std::slice::from_ref(&source))?;
        if let Some(existing) = self
            .sources
            .iter_mut()
            .find(|item| item.device_id == source.device_id)
        {
            *existing = source;
        } else {
            self.sources.push(source);
        }
        Ok(())
    }

    pub(crate) fn remove(&mut self, device_id: &str) -> Result<(), PeerSyncError> {
        self.sources.retain(|item| item.device_id != device_id);
        Ok(())
    }

    pub(crate) fn save(&self) -> Result<(), PeerSyncError> {
        write_registry(
            &self.root.join("sources.json"),
            &IncomingFile {
                schema: INCOMING_SCHEMA.to_owned(),
                sources: self.sources.clone(),
            },
        )
    }
}

pub(crate) fn load_or_create_device_id(app_root: &Path) -> Result<String, PeerSyncError> {
    let root = peer_root(app_root);
    let current = root.join("device-id");
    if current.exists() {
        return read_device_id(&current);
    }
    let legacy = app_root.join("peer-delta/source-device-id");
    let device_id = if legacy.exists() {
        read_device_id(&legacy)?
    } else {
        uuid::Uuid::new_v4().to_string()
    };
    fs::create_dir_all(&root)?;
    write_owner_only(&current, device_id.as_bytes())?;
    if legacy.exists() {
        fs::remove_file(legacy)?;
    }
    Ok(device_id)
}

fn peer_root(app_root: &Path) -> PathBuf {
    app_root.join("peer-sync")
}

fn read_device_id(path: &Path) -> Result<String, PeerSyncError> {
    let value = fs::read_to_string(path)?.trim().to_owned();
    uuid::Uuid::parse_str(&value)
        .map_err(|_| PeerSyncError::Validation("invalid peer device id".to_owned()))?;
    Ok(value)
}

fn read_registry(path: &Path) -> Result<Option<Vec<u8>>, PeerSyncError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return invalid("peer registry must be a regular file");
            }
            if metadata.len() as usize > MAX_REGISTRY_BYTES {
                return invalid("peer registry exceeds 1 MiB");
            }
            Ok(Some(fs::read(path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn parse_registry<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, PeerSyncError> {
    serde_json::from_slice(bytes)
        .map_err(|_| PeerSyncError::Validation("malformed peer registry".to_owned()))
}
fn invalid<T>(message: &str) -> Result<T, PeerSyncError> {
    Err(PeerSyncError::Validation(message.to_owned()))
}

fn validate_id(value: &str) -> Result<(), PeerSyncError> {
    if value.is_empty() || value.len() > 128 {
        invalid("invalid peer device id")
    } else {
        Ok(())
    }
}
fn validate_name(value: &str) -> Result<(), PeerSyncError> {
    if value.is_empty() || value.len() > 256 {
        invalid("invalid peer device name")
    } else {
        Ok(())
    }
}
fn validate_outgoing(devices: &[OutgoingDevice]) -> Result<(), PeerSyncError> {
    for item in devices {
        validate_id(&item.device_id)?;
        validate_name(&item.name)?;
        if item.bearer_digest.len() != 64
            || !item
                .bearer_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return invalid("invalid peer bearer digest");
        }
        item.permissions.validate()?;
    }
    Ok(())
}
fn validate_incoming(sources: &[IncomingSource]) -> Result<(), PeerSyncError> {
    for item in sources {
        validate_id(&item.device_id)?;
        validate_name(&item.name)?;
        if item.endpoint.is_empty() || item.endpoint.len() > 2048 || item.bearer.len() != 64 {
            return invalid("invalid registered peer source");
        }
        item.permissions.validate()?;
    }
    Ok(())
}

fn write_registry<T: Serialize>(path: &Path, value: &T) -> Result<(), PeerSyncError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| PeerSyncError::Storage(error.to_string()))?;
    if bytes.len() > MAX_REGISTRY_BYTES {
        return invalid("peer registry exceeds 1 MiB");
    }
    let parent = path
        .parent()
        .ok_or_else(|| PeerSyncError::Storage("peer registry has no parent".to_owned()))?;
    fs::create_dir_all(parent)?;
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return invalid("peer registry cannot be a link");
    }
    let temporary = path.with_extension("json.tmp");
    let result = write_owner_only(&temporary, &bytes).and_then(|_| {
        fs::rename(&temporary, path)?;
        Ok(())
    });
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), PeerSyncError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}
