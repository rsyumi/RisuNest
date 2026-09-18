#[path = "backup_references.rs"]
pub(crate) mod references;

use super::{Result, SyncError};
use crate::persistent_store::PersistentStore;
use risunest_sync_wire::RemoteHead;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const PAGE_SIZE: usize = 50;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Side {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Availability {
    LocalComplete,
    ConnectionRequired,
    Unavailable,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SideMetrics {
    pub local_required_bytes: u64,
    pub remote_dependent_bytes: u64,
    pub availability: Availability,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Backup {
    pub id: String,
    pub created_at: u64,
    pub head: RemoteHead,
    pub local_revision: i64,
    pub local: SideMetrics,
    pub remote: SideMetrics,
    pub preservation_scope: &'static str,
    #[serde(skip_serializing)]
    pub local_bytes: u64,
    #[serde(skip_serializing)]
    pub remote_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BackupCursor {
    pub created_at: u64,
    pub id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BackupList {
    pub items: Vec<Backup>,
    pub next: Option<BackupCursor>,
}

#[derive(Clone, Debug)]
pub(crate) struct ReferenceSource {
    id: String,
    side: Side,
    index_hash: String,
    index_bytes: u64,
    head: RemoteHead,
    local_revision: i64,
}

impl ReferenceSource {
    pub(crate) fn id(&self) -> &str { &self.id }
    pub(crate) fn side(&self) -> Side { self.side }
    pub(crate) fn index_hash(&self) -> &str { &self.index_hash }
    pub(crate) fn index_bytes(&self) -> u64 { self.index_bytes }
    pub(crate) fn head(&self) -> &RemoteHead { &self.head }
    pub(crate) fn local_revision(&self) -> i64 { self.local_revision }
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

fn metrics(root: &Path, db: &rusqlite::Connection, side: Side) -> Result<SideMetrics> {
    let requirements = references::side_requirements(root, db, side)?;
    let availability = if !requirements.local_required_available {
        Availability::Unavailable
    } else if requirements.remote_dependent_bytes > 0 {
        Availability::ConnectionRequired
    } else {
        Availability::LocalComplete
    };
    Ok(SideMetrics {
        local_required_bytes: requirements.local_required_bytes,
        remote_dependent_bytes: requirements.remote_dependent_bytes,
        availability,
    })
}

pub(super) fn inspect(root: &Path, id: &str) -> Result<Backup> {
    let receipt = references::inspect(root, id)?;
    let db = references::open_for_inspection(root, id)?;
    let local = metrics(root, &db, Side::Local)?;
    let remote = metrics(root, &db, Side::Remote)?;
    let local_bytes = local.local_required_bytes.checked_add(local.remote_dependent_bytes)
        .ok_or_else(|| SyncError::new("storage-size-overflow", 409))?;
    let remote_bytes = remote.local_required_bytes.checked_add(remote.remote_dependent_bytes)
        .ok_or_else(|| SyncError::new("storage-size-overflow", 409))?;
    Ok(Backup {
        id: receipt.id,
        created_at: receipt.created_at_ms,
        head: receipt.head,
        local_revision: receipt.local_revision,
        local,
        remote,
        preservation_scope: "library",
        local_bytes,
        remote_bytes,
    })
}

pub(crate) fn list(root: &Path, before: Option<&BackupCursor>) -> Result<BackupList> {
    if let Some(cursor) = before {
        let uuid = uuid::Uuid::parse_str(&cursor.id)
            .map_err(|_| SyncError::new("invalid-backup-cursor", 400))?;
        if uuid.to_string() != cursor.id {
            return Err(SyncError::new("invalid-backup-cursor", 400));
        }
    }
    let entries = match std::fs::read_dir(root.join("server-sync/backups")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BackupList { items: Vec::new(), next: None });
        }
        Err(e) => return Err(e.into()),
    };
    let mut page = BTreeMap::new();
    let mut matching = 0usize;
    for entry in entries {
        let entry = entry?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else { continue; };
        let Ok(backup) = inspect(root, &id) else { continue; };
        let key = (backup.created_at, id);
        if before.is_some_and(|cursor| key >= (cursor.created_at, cursor.id.clone())) {
            continue;
        }
        matching += 1;
        page.insert(key, backup);
        if page.len() > PAGE_SIZE { page.pop_first(); }
    }
    let next = (matching > PAGE_SIZE).then(|| {
        let ((created_at, id), _) = page.first_key_value().expect("nonempty bounded page");
        BackupCursor { created_at: *created_at, id: id.clone() }
    });
    Ok(BackupList { items: page.into_values().rev().collect(), next })
}

pub(crate) fn source(
    root: &Path,
    id: &str,
    side: Side,
    check: &impl Fn() -> Result<()>,
) -> Result<ReferenceSource> {
    let receipt = references::inspect(root, id)?;
    references::open(root, id, check)?;
    Ok(ReferenceSource { id: receipt.id, side, index_hash: receipt.index_hash,
        index_bytes: receipt.index_bytes, head: receipt.head, local_revision: receipt.local_revision })
}

fn reopen(root: &Path, source: &ReferenceSource, check: &impl Fn() -> Result<()>)
    -> Result<rusqlite::Connection> {
    let receipt = references::inspect(root, &source.id)?;
    if receipt.index_hash != source.index_hash || receipt.index_bytes != source.index_bytes
        || receipt.local_revision != source.local_revision || receipt.head != source.head {
        return Err(SyncError::new("conflict-source-changed", 409));
    }
    references::open(root, &source.id, check)
}

pub(crate) fn validate_reference_source(
    root: &Path,
    source: &ReferenceSource,
    check: &impl Fn() -> Result<()>,
) -> Result<()> {
    drop(reopen(root, source, check)?);
    Ok(())
}

pub(crate) fn visit_reference_records(
    root: &Path,
    source: &ReferenceSource,
    check: &impl Fn() -> Result<()>,
    visit: impl FnMut(references::SourceRecord) -> Result<()>,
) -> Result<()> {
    let db = reopen(root, source, check)?;
    references::visit_source_records(root, &db, source.side, check, visit)
}

pub(crate) fn visit_reference_objects(
    root: &Path,
    source: &ReferenceSource,
    check: &impl Fn() -> Result<()>,
    visit: impl FnMut(references::SourceObject) -> Result<()>,
) -> Result<()> {
    let db = reopen(root, source, check)?;
    references::visit_source_objects(root, &db, source.side, check, visit)
}

pub(crate) struct PreparedReferencePortableStore {
    store: PersistentStore,
    revision: i64,
    scratch: tempfile::TempDir,
}

impl PreparedReferencePortableStore {
    pub(crate) fn into_parts(self) -> (PersistentStore, i64, tempfile::TempDir) {
        (self.store, self.revision, self.scratch)
    }
}

pub(crate) fn prepare_reference_portable_store(
    source_root: &Path,
    source: &ReferenceSource,
    scratch_parent: &Path,
    check: &impl Fn() -> Result<()>,
) -> Result<PreparedReferencePortableStore> {
    check()?;
    let metadata = std::fs::symlink_metadata(scratch_parent)?;
    if !metadata.is_dir() || crate::trust_boundary::is_link_like(&metadata) {
        return Err(SyncError::new("invalid-conflict-export-scratch", 409));
    }
    let scratch = tempfile::Builder::new()
        .prefix("server-conflict-export-")
        .tempdir_in(scratch_parent)?;
    let mut store = PersistentStore::open(&scratch.path().join("pds"))?;
    let prepared = store.prepare_server_conflict_portable_export(
        source_root,
        source,
        0,
        check,
    )?;
    let revision = store.finish_prepared_replace(prepared)?.revision;
    check()?;
    Ok(PreparedReferencePortableStore { store, revision, scratch })
}

#[cfg(test)]
#[path = "backups_tests.rs"]
mod tests;
