//! Honest storage usage summaries. Local upload receipts establish only a
//! repository lower bound, while provider-wide physical usage remains unknown
//! until a provider exposes an authoritative account API.
use super::{
    connection_commands::ConnectedRepository,
    contract::{Cancellation, ErrorKind, ProviderError, Result},
    control,
    gc_store::GcStore,
    packaging::RemoteObject,
};
use rusqlite::{Connection, OpenFlags};
use std::{collections::BTreeSet, path::Path};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReachableUsage {
    pub snapshot_id: String,
    pub known_direct_objects: u64,
    pub known_direct_bytes: u64,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StorageUsage {
    pub provider_physical_bytes: Option<u64>,
    pub locally_uploaded_objects_lower_bound: u64,
    pub locally_uploaded_bytes_lower_bound: u64,
    pub latest_reachable: Option<ReachableUsage>,
}

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn transient() -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

fn package_cache_root(root: &Path, connection_id: &str) -> std::path::PathBuf {
    root.join("external-storage")
        .join(hex::encode(
            risunest_external_storage_format::content_identity::hash(connection_id.as_bytes()),
        ))
        .join("package-cache")
}

fn local_upload_lower_bound(
    root: &Path,
    connection_id: &str,
    connected: &ConnectedRepository,
) -> Result<(u64, u64)> {
    let path = package_cache_root(root, connection_id).join("snapshot-cache.sqlite");
    if !path.exists() {
        return Ok((0, 0));
    }
    if crate::trust_boundary::is_link_like(
        &std::fs::symlink_metadata(&path).map_err(|_| transient())?,
    ) {
        return Err(corrupt());
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| transient())?;
    let mut statement = db
        .prepare(
            "SELECT value FROM remote_objects \
             WHERE repository_id=?1 AND connection_identity=?2 ORDER BY object_id",
        )
        .map_err(|_| corrupt())?;
    let mut rows = statement
        .query(rusqlite::params![
            connected.stored.descriptor.repository_id,
            connected.handle.connection_identity,
        ])
        .map_err(|_| corrupt())?;
    let mut objects = 0u64;
    let mut bytes = 0u64;
    while let Some(row) = rows.next().map_err(|_| corrupt())? {
        let encoded: String = row.get(0).map_err(|_| corrupt())?;
        let object: RemoteObject = serde_json::from_str(&encoded).map_err(|_| corrupt())?;
        object.stored(&connected.handle)?;
        if object.repository_id != connected.stored.descriptor.repository_id {
            return Err(corrupt());
        }
        objects = objects.checked_add(1).ok_or_else(corrupt)?;
        bytes = bytes
            .checked_add(object.receipt.byte_length)
            .ok_or_else(corrupt)?;
    }
    Ok((objects, bytes))
}

fn direct_reachable(
    snapshot: &RemoteObject,
    document: &control::SnapshotView,
) -> Result<ReachableUsage> {
    let mut identities = BTreeSet::new();
    let mut bytes = 0u64;
    let root_identity = (
        snapshot.receipt.locator.collection.clone(),
        snapshot.receipt.locator.object.clone(),
    );
    if identities.insert(root_identity) {
        bytes = bytes
            .checked_add(snapshot.receipt.byte_length)
            .ok_or_else(corrupt)?;
    }
    for object in super::reachability::document_references(document) {
        let identity = (
            object.locator.collection.clone(),
            object.locator.object.clone(),
        );
        if identities.insert(identity) {
            bytes = bytes
                .checked_add(object.ciphertext_length)
                .ok_or_else(corrupt)?;
        }
    }
    Ok(ReachableUsage {
        snapshot_id: document.snapshot_id.clone(),
        known_direct_objects: identities.len().try_into().map_err(|_| corrupt())?,
        known_direct_bytes: bytes,
        // Catalog payload packs require catalog traversal. Usage inspection
        // deliberately does not download them or scan history.
        complete: false,
    })
}

pub(crate) async fn summarize(
    root: &Path,
    connection_id: &str,
    connected: &ConnectedRepository,
    cancel: &Cancellation,
) -> Result<StorageUsage> {
    let (locally_uploaded_objects_lower_bound, locally_uploaded_bytes_lower_bound) =
        local_upload_lower_bound(root, connection_id, connected)?;
    let latest_reachable = if connected.stored.descriptor.publication_strategy.is_some() {
        match control::read_head(
            connected.provider.as_ref(),
            &connected.handle,
            &connected.stored.descriptor,
            &connected.root_key,
            None,
            cancel,
        )
        .await?
        {
            Some(head) => {
                let document =
                    control::read_snapshot_document(connected, &head.document.state, cancel)
                        .await?;
                let mut reachable = direct_reachable(&head.document.state, &document)?;
                // Only a cleanup walks the catalogs, so a repository no
                // cleanup has finished keeps reporting the lower bound rather
                // than paying for that walk on every inspection.
                if let Some(bytes) = GcStore::open(root)?.last_reachable_bytes(connection_id)? {
                    reachable.known_direct_bytes = bytes;
                    reachable.complete = true;
                }
                Some(reachable)
            }
            None => None,
        }
    } else {
        None
    };
    Ok(StorageUsage {
        // The provider contract currently has no authoritative account or
        // bucket physical-usage operation.
        provider_physical_bytes: None,
        locally_uploaded_objects_lower_bound,
        locally_uploaded_bytes_lower_bound,
        latest_reachable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::{
        contract::{ObjectReceipt, ObjectRole},
        fake,
    };

    fn object(
        repository: &super::super::contract::RepositoryHandle,
        id: &str,
        role: ObjectRole,
    ) -> RemoteObject {
        let mut locator = fake::locator();
        locator.object = id.into();
        let wire_role = match role {
            ObjectRole::SyncState => risunest_external_storage_format::snapshot::ObjectRole::SyncState,
            ObjectRole::Catalog => risunest_external_storage_format::snapshot::ObjectRole::Catalog,
            _ => unreachable!(),
        };
        let header = risunest_external_storage_format::snapshot::PublicObjectHeader::new(
            repository.repository_id.clone(),
            id.into(),
            wire_role,
            1,
        )
        .unwrap();
        let bytes = risunest_external_storage_format::snapshot::envelope_length(&header).unwrap();
        RemoteObject {
            repository_id: repository.repository_id.clone(),
            object_id: id.into(),
            role,
            receipt: ObjectReceipt {
                locator,
                byte_length: bytes,
                version: None,
                checksum: None,
                complete: true,
            },
            ciphertext_sha256: "11".repeat(32),
            plaintext_length: 1,
            plaintext_sha256: "22".repeat(32),
        }
    }

    /// Invariant 33. Every section the head still names counts as reachable,
    /// including one a library-only publication merely carried forward.
    #[test]
    fn direct_reachable_is_an_explicit_incomplete_lower_bound() {
        use risunest_external_storage_format::{
            section::{SectionKind, SECTION_CODEC},
            snapshot::SectionSnapshotRef,
        };
        let repository = fake::repository();
        let snapshot = object(&repository, "snapshot-root", ObjectRole::SyncState);
        let records_object = object(&repository, "records", ObjectRole::Catalog);
        let assets_object = object(&repository, "assets", ObjectRole::Catalog);
        let section_object = object(&repository, "section-hypa", ObjectRole::Catalog);
        let records = records_object.stored(&repository).unwrap();
        let assets = assets_object.stored(&repository).unwrap();
        let document = control::SnapshotView {
            snapshot_id: "snapshot".into(),
            parent_snapshot_id: None,
            library_id: "library".into(),
            created_at_ms: 1,
            revision: "1".into(),
            captured_by_device: None,
            library: risunest_external_storage_format::snapshot::LibrarySnapshotRef {
                record_catalog: records,
                asset_catalog: assets,
                content_fingerprint: [2; 32],
            },
            sections: std::collections::BTreeMap::from([(
                SectionKind::Hypa.id().to_owned(),
                SectionSnapshotRef {
                    kind: SectionKind::Hypa,
                    codec: SECTION_CODEC.into(),
                    generation: risunest_sync_wire::head::Sequence::from(3u64),
                    gc_floor: risunest_sync_wire::head::Sequence::from(0u64),
                    max_write_clock: risunest_sync_wire::head::Sequence::from(9u64),
                    entries_root: section_object.stored(&repository).unwrap(),
                    content_fingerprint: [4; 32],
                },
            )]),
            is_state: true,
        };
        let usage = direct_reachable(&snapshot, &document).unwrap();
        assert!(!usage.complete);
        assert_eq!(usage.known_direct_objects, 4);
        assert_eq!(
            usage.known_direct_bytes,
            snapshot.receipt.byte_length
                + records_object.receipt.byte_length
                + assets_object.receipt.byte_length
                + section_object.receipt.byte_length
        );
    }
}
