use super::{Result, SyncError};
use risunest_sync_wire::RemoteHead;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Receipt {
    format: String,
    scope: String,
    head: RemoteHead,
    local_revision: i64,
    local_hash: String,
    remote_hash: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Backup {
    pub id: String,
    pub created_at: u64,
    pub head: RemoteHead,
    pub local_revision: i64,
    pub local_bytes: u64,
    pub remote_bytes: u64,
    pub preservation_scope: &'static str,
    pub recovery_ready: bool,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Side {
    Local,
    Remote,
}
pub(super) fn directory(root: &Path, id: &str) -> Result<PathBuf> {
    let uuid = uuid::Uuid::parse_str(id).map_err(|_| SyncError::new("invalid-backup-id", 400))?;
    if uuid.to_string() != id {
        return Err(SyncError::new("invalid-backup-id", 400));
    }
    let server_root = root.join("server-sync");
    let backup_root = server_root.join("backups");
    for path in [&server_root, &backup_root] {
        let metadata = std::fs::symlink_metadata(path)?;
        if crate::trust_boundary::is_link_like(&metadata) || !metadata.is_dir() {
            return Err(SyncError::new("invalid-backup-path", 409));
        }
    }
    let base = std::fs::canonicalize(backup_root)?;
    let path = base.join(id);
    if std::fs::canonicalize(&path)? != path {
        return Err(SyncError::new("invalid-backup-path", 409));
    }
    Ok(path)
}
pub(super) fn inspect(root: &Path, id: &str) -> Result<Backup> {
    let path = directory(root, id)?;
    let receipt = receipt(&path)?;
    let size = |name: &str| -> Result<u64> {
        let file = path.join(name);
        let metadata = std::fs::symlink_metadata(&file)?;
        if !metadata.is_file() || crate::trust_boundary::is_link_like(&metadata) {
            return Err(SyncError::new("invalid-backup-path", 409));
        }
        Ok(metadata.len())
    };
    Ok(Backup {
        id: id.to_owned(),
        created_at: std::fs::metadata(path.join("complete.json"))?
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        head: receipt.head,
        local_revision: receipt.local_revision,
        local_bytes: size("local.risunest")?,
        remote_bytes: size("remote.risunest")?,
        preservation_scope: "library",
        recovery_ready: true,
    })
}
fn receipt(directory: &Path) -> Result<Receipt> {
    let path = directory.join("complete.json");
    if std::fs::canonicalize(&path)? != path {
        return Err(SyncError::new("invalid-backup-path", 409));
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err(SyncError::new("invalid-backup-receipt", 409));
    }
    let receipt: Receipt = serde_json::from_slice(&bytes)
        .map_err(|_| SyncError::new("invalid-backup-receipt", 409))?;
    receipt.head.validate()?;
    if receipt.format != "risunest-portable-backup" || receipt.scope != "library" {
        return Err(SyncError::new("invalid-backup-scope", 409));
    }
    risunest_sync_wire::validate_hash(&receipt.local_hash)?;
    risunest_sync_wire::validate_hash(&receipt.remote_hash)?;
    Ok(receipt)
}
pub(crate) fn list(root: &Path) -> Result<Vec<Backup>> {
    let entries = match std::fs::read_dir(root.join("server-sync/backups")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut newest = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(backup) = inspect(root, &id) else {
            continue;
        };
        newest.insert((backup.created_at, id), backup);
        if newest.len() > 100 {
            newest.pop_first();
        }
    }
    Ok(newest.into_values().rev().collect())
}
pub(crate) fn source(
    root: &Path,
    id: &str,
    side: Side,
    cancelled: impl Fn() -> bool,
) -> Result<String> {
    let directory = directory(root, id)?;
    let receipt = receipt(&directory)?;
    let (name, expected) = match side {
        Side::Local => ("local.risunest", receipt.local_hash),
        Side::Remote => ("remote.risunest", receipt.remote_hash),
    };
    let path = directory.join(name);
    if std::fs::canonicalize(&path)? != path {
        return Err(SyncError::new("invalid-backup-path", 409));
    }
    let mut file = File::open(&path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    loop {
        if cancelled() {
            return Err(SyncError::new("cancelled", 409));
        }
        let read = file.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        hash.update(&bytes[..read]);
    }
    if format!("{:x}", hash.finalize()) != expected {
        return Err(SyncError::new("backup-hash-mismatch", 409));
    }
    Ok(path.to_string_lossy().into_owned())
}
