//! Streams a completed PDS capture into bounded encrypted immutable objects.
//! This layer starts only after the PDS read snapshot has closed.
use super::{
    capabilities::Capabilities,
    contract::{
        Cancellation, ErrorKind, ObjectIntent, ObjectReceipt, ObjectRole, Provider, ProviderError,
        ReadReceipt, RemoteLocator, RepositoryHandle, Result,
    },
    journal::{validate_receipt, TransferJournal},
    sections::CapturedSection,
    transfer::SpoolSink,
    transfer_job,
};
use crate::{asset_repository::PayloadCas, persistent_store::external_capture::CapturedSnapshot};
use risunest_external_storage_format::{
    content_identity::{hash, hash_reader},
    control as wire_control,
    crypto::derive_key,
    format::library_fingerprint_domain,
    pack::{self, Chunk, ChunkEncoder, CompressionPolicy, ENTRY_OVERHEAD, MAX_CHUNK_BYTES},
    section::SECTION_CODEC,
    snapshot as wire,
};
use risunest_sync_wire::head::Sequence;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

const DEFAULT_MAX_STORED_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_TARGET_BYTES: u64 = 64 * 1024 * 1024;
const MIN_OBJECT_BYTES: u64 = 1024;
static SNAPSHOT_CPU: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

fn corrupt(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn transient(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

pub(super) async fn cpu_permit() -> Result<tokio::sync::OwnedSemaphorePermit> {
    SNAPSHOT_CPU
        .clone()
        .acquire_owned()
        .await
        .map_err(transient)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PackageLimits {
    /// Provider-side stored file limit, before provider SDK wrapping.
    pub max_stored_bytes: u64,
    pub sdk_overhead_bytes: u64,
    pub target_plaintext_bytes: u64,
}
impl PackageLimits {
    pub(crate) fn from_capabilities(capabilities: &Capabilities) -> Result<Self> {
        let max_stored_bytes = capabilities
            .max_stored_bytes
            .unwrap_or(DEFAULT_MAX_STORED_BYTES);
        if capabilities.sdk_overhead_bytes.checked_add(MIN_OBJECT_BYTES)
            .is_none_or(|minimum| max_stored_bytes <= minimum)
        {
            return Err(ProviderError::new(ErrorKind::FileTooLarge));
        }
        Ok(Self {
            max_stored_bytes,
            sdk_overhead_bytes: capabilities.sdk_overhead_bytes,
            target_plaintext_bytes: DEFAULT_TARGET_BYTES,
        })
    }
}

/// What the packaged object is for. A published state inherits the sections
/// the observed state carried, so a library-only publish preserves them. A
/// backup bundle instead declares its own coverage and never merges.
#[derive(Clone, Debug)]
pub(crate) enum SnapshotPurpose {
    SyncState {
        epoch: String,
        generation: Sequence,
        parent_sections: BTreeMap<String, wire::SectionSnapshotRef>,
    },
    BackupBundle {
        source: wire_control::BundleSource,
        remote_generation: Option<Sequence>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct SnapshotMetadata {
    pub snapshot_id: String,
    pub repository_id: String,
    pub library_id: String,
    pub author_device_id: String,
    pub created_at_ms: u64,
    pub logical_revision: u64,
    pub parent_snapshot_id: Option<String>,
    pub content_fingerprint: [u8; 32],
    pub purpose: SnapshotPurpose,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RemoteObject {
    /// Random repository identity from the encrypted format descriptor.
    pub repository_id: String,
    pub object_id: String,
    pub role: ObjectRole,
    pub receipt: ObjectReceipt,
    pub ciphertext_sha256: String,
    pub plaintext_length: u64,
    pub plaintext_sha256: String,
}
impl RemoteObject {
    pub(crate) fn stored(&self, repository: &RepositoryHandle) -> Result<wire::StoredObject> {
        self.receipt.locator.validate_for(repository)?;
        if !self.receipt.complete
            || self.receipt.byte_length == 0
            || !crate::trust_boundary::is_lower_hex_256(&self.ciphertext_sha256)
            || !crate::trust_boundary::is_lower_hex_256(&self.plaintext_sha256)
        {
            return Err(corrupt("invalid remote object"));
        }
        let role = wire_role(self.role)?;
        let header = wire::PublicObjectHeader::new(
            self.repository_id.clone(),
            self.object_id.clone(),
            role,
            self.plaintext_length,
        )
        .map_err(corrupt)?;
        let stored = wire::StoredObject {
            header,
            locator: wire::WireLocator {
                connection_identity: self.receipt.locator.connection_identity.clone(),
                collection: self.receipt.locator.collection.clone(),
                object: self.receipt.locator.object.clone(),
            },
            ciphertext_length: self.receipt.byte_length,
            ciphertext_sha256: decode_hash(&self.ciphertext_sha256)?,
            plaintext_length: self.plaintext_length,
            plaintext_sha256: decode_hash(&self.plaintext_sha256)?,
        };
        stored.validate().map_err(corrupt)?;
        Ok(stored)
    }

    pub(crate) fn from_stored(
        value: &wire::StoredObject,
        repository: &RepositoryHandle,
    ) -> Result<Self> {
        value.validate().map_err(corrupt)?;
        let locator = RemoteLocator {
            connection_identity: value.locator.connection_identity.clone(),
            collection: value.locator.collection.clone(),
            object: value.locator.object.clone(),
        };
        locator.validate_for(repository)?;
        Ok(Self {
            repository_id: value.header.repository_id.clone(),
            object_id: value.header.object_id.clone(),
            role: native_role(value.header.role)?,
            receipt: ObjectReceipt {
                locator,
                byte_length: value.ciphertext_length,
                version: None,
                checksum: None,
                complete: true,
            },
            ciphertext_sha256: hex::encode(value.ciphertext_sha256),
            plaintext_length: value.plaintext_length,
            plaintext_sha256: hex::encode(value.plaintext_sha256),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CompletedSnapshot {
    pub reference: RemoteObject,
    pub snapshot_id: String,
    pub repository_id: String,
    /// The whole-state identifier for a published state, or the bundle
    /// identifier for a backup. Control changes reach it; library content
    /// changes reach `library_fingerprint`.
    pub fingerprint: String,
    pub library_fingerprint: String,
    pub logical_revision: u64,
    pub record_catalog: RemoteObject,
    pub asset_catalog: RemoteObject,
    pub sections: BTreeMap<String, wire::SectionSnapshotRef>,
    pub referenced_objects: Vec<RemoteObject>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationReadiness {
    Verified,
    /// A recreated object has a different locator, or a cached source is gone.
    /// Repackage from the pinned capture under a new snapshot publication intent;
    /// the old immutable snapshot must never be overwritten with different bytes.
    Repackage,
}

/// Call under the active execution's protection immediately before publishing a
/// head or backup point. Packaging a root earlier is not proof of existence now.
/// Only confirmed missing ciphertext is repaired, using the original sealed spool.
pub(crate) async fn verify_publication(
    completed: &CompletedSnapshot,
    cache_root: &Path,
    journal: &mut TransferJournal,
    root_key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<PublicationReadiness> {
    if completed.reference.repository_id != completed.repository_id
        || !matches!(completed.reference.role, ObjectRole::SyncState | ObjectRole::BackupBundle)
    {
        return Err(corrupt("publication root identity differs"));
    }
    let mut cache = PackageCache::open(cache_root)?;
    let mut catalogs = vec![completed.record_catalog.clone(), completed.asset_catalog.clone()];
    for section in completed.sections.values() {
        catalogs.push(RemoteObject::from_stored(&section.entries_root, repository)?);
    }
    for catalog in catalogs {
        if catalog.repository_id != completed.repository_id { return Err(corrupt("publication repository differs")); }
        let Some(objects) = revalidate_cached_catalog(
            &catalog, &mut cache, journal, root_key, provider, repository, cancel,
        ).await? else {
            return Ok(PublicationReadiness::Repackage);
        };
        let current = objects.first().ok_or_else(|| corrupt("empty publication catalog"))?;
        if current.receipt.locator != catalog.receipt.locator {
            return Ok(PublicationReadiness::Repackage);
        }
    }
    let Some(root) = revalidate_cached_object(
        &completed.reference, &mut cache, journal, provider, repository, cancel,
    ).await? else {
        return Ok(PublicationReadiness::Repackage);
    };
    if root.receipt.locator != completed.reference.receipt.locator {
        // The caller's immutable point or prepared head names the previous root.
        return Ok(PublicationReadiness::Repackage);
    }
    Ok(PublicationReadiness::Verified)
}

pub(crate) fn wire_role(role: ObjectRole) -> Result<wire::ObjectRole> {
    match role {
        ObjectRole::Pack => Ok(wire::ObjectRole::Pack),
        ObjectRole::Catalog => Ok(wire::ObjectRole::Catalog),
        ObjectRole::SyncState => Ok(wire::ObjectRole::SyncState),
        ObjectRole::BackupBundle => Ok(wire::ObjectRole::BackupBundle),
        ObjectRole::BackupPoint => Ok(wire::ObjectRole::BackupPoint),
        ObjectRole::Descriptor => Ok(wire::ObjectRole::Descriptor),
        ObjectRole::Lease => Ok(wire::ObjectRole::Lease),
    }
}
pub(crate) fn native_role(role: wire::ObjectRole) -> Result<ObjectRole> {
    match role {
        wire::ObjectRole::Pack => Ok(ObjectRole::Pack),
        wire::ObjectRole::Catalog => Ok(ObjectRole::Catalog),
        wire::ObjectRole::SyncState => Ok(ObjectRole::SyncState),
        wire::ObjectRole::BackupBundle => Ok(ObjectRole::BackupBundle),
        wire::ObjectRole::BackupPoint => Ok(ObjectRole::BackupPoint),
        wire::ObjectRole::Descriptor => Ok(ObjectRole::Descriptor),
        wire::ObjectRole::Lease => Ok(ObjectRole::Lease),
        wire::ObjectRole::Head => Err(corrupt("head is not an immutable snapshot object")),
    }
}
fn decode_hash(value: &str) -> Result<[u8; 32]> {
    hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| corrupt("invalid sha256"))
}

struct PackageCache {
    db: Connection,
}
impl PackageCache {
    fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root).map_err(transient)?;
        if crate::trust_boundary::is_link_like(&fs::symlink_metadata(root).map_err(transient)?) {
            return Err(corrupt("package cache is a link"));
        }
        let db_path = root.join("snapshot-cache.sqlite");
        if db_path.exists()
            && crate::trust_boundary::is_link_like(
                &fs::symlink_metadata(&db_path).map_err(transient)?,
            )
        {
            return Err(corrupt("package cache database is a link"));
        }
        let db = Connection::open(db_path).map_err(transient)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS remote_objects(
               repository_id TEXT NOT NULL, connection_identity TEXT NOT NULL,
               object_id TEXT NOT NULL, plaintext_sha256 TEXT NOT NULL,
               value TEXT NOT NULL,
               PRIMARY KEY(repository_id,connection_identity,object_id));
             CREATE TABLE IF NOT EXISTS entries(
               repository_id TEXT NOT NULL, connection_identity TEXT NOT NULL,
               catalog_kind TEXT NOT NULL, entry_key TEXT NOT NULL,
               content_sha256 TEXT NOT NULL, byte_length INTEGER NOT NULL,
               value TEXT NOT NULL,
               PRIMARY KEY(repository_id,connection_identity,catalog_kind,entry_key));
             CREATE TABLE IF NOT EXISTS catalogs(
               repository_id TEXT NOT NULL, connection_identity TEXT NOT NULL,
               catalog_kind TEXT NOT NULL, fingerprint TEXT NOT NULL,
               value TEXT NOT NULL,
               PRIMARY KEY(repository_id,connection_identity,catalog_kind,fingerprint));",
        )
        .map_err(transient)?;
        Ok(Self { db })
    }
    fn object(
        &self,
        format_repository_id: &str,
        repository: &RepositoryHandle,
        id: &str,
        plaintext: &str,
    ) -> Result<Option<RemoteObject>> {
        let encoded: Option<String> = self.db.query_row(
            "SELECT value FROM remote_objects WHERE repository_id=?1 AND connection_identity=?2 AND object_id=?3 AND plaintext_sha256=?4",
            params![format_repository_id,repository.connection_identity,id,plaintext], |row| row.get(0),
        ).optional().map_err(transient)?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        let value: RemoteObject = serde_json::from_str(&encoded).map_err(corrupt)?;
        value.stored(repository)?;
        if value.repository_id != format_repository_id || value.object_id != id
            || value.plaintext_sha256 != plaintext
        {
            return Err(corrupt("cached object identity"));
        }
        Ok(Some(value))
    }
    fn forget_object(
        &self,
        format_repository_id: &str,
        repository: &RepositoryHandle,
        object_id: &str,
    ) -> Result<()> {
        let transaction = self.db.unchecked_transaction().map_err(transient)?;
        transaction.execute(
            "DELETE FROM remote_objects WHERE repository_id=?1 AND connection_identity=?2 AND object_id=?3",
            params![format_repository_id, repository.connection_identity, object_id],
        ).map_err(transient)?;
        transaction.execute(
            "DELETE FROM entries WHERE repository_id=?1 AND connection_identity=?2
             AND EXISTS(SELECT 1 FROM json_each(entries.value,'$.packs') AS pack
               WHERE json_extract(pack.value,'$.objectId')=?3)",
            params![format_repository_id, repository.connection_identity, object_id],
        ).map_err(transient)?;
        // A changed child can change every parent hash. These roots are only
        // an optimization; their source entries and sealed uploads stay intact.
        transaction.execute(
            "DELETE FROM catalogs WHERE repository_id=?1 AND connection_identity=?2",
            params![format_repository_id, repository.connection_identity],
        ).map_err(transient)?;
        transaction.commit().map_err(transient)
    }

    fn put_object(&self, repository: &RepositoryHandle, value: &RemoteObject) -> Result<()> {
        value.stored(repository)?;
        self.db.execute(
            "INSERT INTO remote_objects VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(repository_id,connection_identity,object_id) DO UPDATE SET plaintext_sha256=excluded.plaintext_sha256,value=excluded.value",
            params![value.repository_id,repository.connection_identity,value.object_id,value.plaintext_sha256,serde_json::to_string(value).map_err(corrupt)?],
        ).map_err(transient)?;
        Ok(())
    }
    fn entry(
        &self,
        format_repository_id: &str,
        repository: &RepositoryHandle,
        kind: wire::CatalogKind,
        key: &str,
        digest: &str,
        length: u64,
    ) -> Result<Option<EntryPlan>> {
        let encoded: Option<String> = self.db.query_row(
            "SELECT value FROM entries WHERE repository_id=?1 AND connection_identity=?2 AND catalog_kind=?3 AND entry_key=?4 AND content_sha256=?5 AND byte_length=?6",
            params![format_repository_id,repository.connection_identity,kind_name(kind),key,digest,i64::try_from(length).map_err(corrupt)?], |row| row.get(0),
        ).optional().map_err(transient)?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        let value: EntryPlan = serde_json::from_str(&encoded).map_err(corrupt)?;
        value.validate(format_repository_id, repository)?;
        Ok(Some(value))
    }
    fn put_entries(
        &self,
        format_repository_id: &str,
        repository: &RepositoryHandle,
        kind: wire::CatalogKind,
        values: &[EntryPlan],
    ) -> Result<()> {
        let transaction = self.db.unchecked_transaction().map_err(transient)?;
        for value in values {
            value.validate(format_repository_id, repository)?;
            transaction.execute(
                "INSERT INTO entries VALUES(?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(repository_id,connection_identity,catalog_kind,entry_key) DO UPDATE SET content_sha256=excluded.content_sha256,byte_length=excluded.byte_length,value=excluded.value",
                params![format_repository_id,repository.connection_identity,kind_name(kind),value.key,value.content_sha256,i64::try_from(value.byte_length).map_err(corrupt)?,serde_json::to_string(value).map_err(corrupt)?],
            ).map_err(transient)?;
        }
        transaction.commit().map_err(transient)?;
        Ok(())
    }
    fn catalog(
        &self,
        format_repository_id: &str,
        repository: &RepositoryHandle,
        kind: wire::CatalogKind,
        fingerprint: &str,
    ) -> Result<Option<RemoteObject>> {
        let encoded: Option<String> = self.db.query_row(
            "SELECT value FROM catalogs WHERE repository_id=?1 AND connection_identity=?2 AND catalog_kind=?3 AND fingerprint=?4",
            params![format_repository_id,repository.connection_identity,kind_name(kind),fingerprint], |row| row.get(0),
        ).optional().map_err(transient)?;
        let Some(encoded) = encoded else {
            return Ok(None);
        };
        let value: RemoteObject = serde_json::from_str(&encoded).map_err(corrupt)?;
        value.stored(repository)?;
        if value.role != ObjectRole::Catalog || value.repository_id != format_repository_id {
            return Err(corrupt("cached catalog role"));
        }
        Ok(Some(value))
    }
    fn put_catalog(
        &self,
        repository: &RepositoryHandle,
        kind: wire::CatalogKind,
        fingerprint: &str,
        value: &RemoteObject,
    ) -> Result<()> {
        value.stored(repository)?;
        self.db
            .execute(
                "INSERT OR REPLACE INTO catalogs VALUES(?1,?2,?3,?4,?5)",
                params![
                    value.repository_id,
                    repository.connection_identity,
                    kind_name(kind),
                    fingerprint,
                    serde_json::to_string(value).map_err(corrupt)?
                ],
            )
            .map_err(transient)?;
        Ok(())
    }
}
/// The existing upload inventory bounds collection ownership. Reading it never
/// discovers foreign packs or treats historical receipts as current existence.
pub(crate) fn known_remote_objects(
    root: &Path,
    format_repository_id: &str,
    repository: &RepositoryHandle,
) -> Result<Vec<RemoteObject>> {
    let path = root.join("snapshot-cache.sqlite");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(transient(error)),
        Ok(_) => {}
    }
    if crate::trust_boundary::is_link_like(&fs::symlink_metadata(root).map_err(transient)?) {
        return Err(corrupt("package cache root is a link"));
    }
    crate::trust_boundary::open_regular_source(&path).map_err(transient)?;
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(transient)?;
    let mut query = db.prepare(
        "SELECT object_id,plaintext_sha256,value FROM remote_objects
         WHERE repository_id=?1 AND connection_identity=?2 ORDER BY object_id",
    ).map_err(transient)?;
    let rows = query.query_map(params![format_repository_id, repository.connection_identity], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    }).map_err(transient)?;
    let mut objects = Vec::new();
    for row in rows {
        let (id, digest, encoded) = row.map_err(transient)?;
        if encoded.len() > 128 * 1024 { return Err(corrupt("inventory object exceeds limit")); }
        let object: RemoteObject = serde_json::from_str(&encoded).map_err(corrupt)?;
        object.stored(repository)?;
        if object.repository_id != format_repository_id || object.object_id != id
            || object.plaintext_sha256 != digest
        {
            return Err(corrupt("inventory object identity differs"));
        }
        objects.push(object);
    }
    Ok(objects)
}

/// Reopening a cache never proves that its old object still exists. A journal
/// can repair a missing object from the original ciphertext; a cache without
/// that source must let its caller rebuild from the pinned capture.
async fn revalidate_cached_object(
    value: &RemoteObject,
    cache: &mut PackageCache,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<Option<RemoteObject>> {
    cancel.check()?;
    value.stored(repository)?;
    // A repaired opaque locator may be newer than an entry's embedded receipt.
    // Consult the existing inventory before declaring that old locator missing;
    // the current remote bytes still have to pass a fresh verification below.
    let mut verified = match cache.object(
        &value.repository_id, repository, &value.object_id, &value.plaintext_sha256,
    )? {
        Some(current) => {
            if current.role != value.role || current.plaintext_length != value.plaintext_length
                || current.ciphertext_sha256 != value.ciphertext_sha256
                || current.receipt.byte_length != value.receipt.byte_length
            {
                return Err(corrupt("cached immutable identity changed"));
            }
            current
        }
        None => value.clone(),
    };
    if let Some(record) = journal.record(&value.object_id)? {
        if record.intent.sha256 != value.ciphertext_sha256
            || record.intent.byte_length != value.receipt.byte_length
            || record.intent.role != value.role
        {
            return Err(corrupt("cached object differs from its sealed source"));
        }
        verified.receipt = transfer_job::upload(
            journal, &value.object_id, provider, repository, cancel,
        ).await?;
    } else {
        let intent = ObjectIntent {
            repository_id: repository.repository_id.clone(),
            job_id: journal.job_id().to_owned(),
            object_id: value.object_id.clone(),
            role: value.role,
            byte_length: value.receipt.byte_length,
            sha256: value.ciphertext_sha256.clone(),
        };
        let mut historical = verified.receipt.clone();
        // A checksum from an old receipt is not a fresh metadata response.
        historical.checksum = None;
        let Some(receipt) = transfer_job::verify_remote_receipt(
            journal.directory(), &intent, provider, repository, historical, cancel,
        ).await? else {
            cache.forget_object(&value.repository_id, repository, &value.object_id)?;
            return Ok(None);
        };
        verified.receipt = receipt;
    }
    // Inherited remote references are not this device's upload inventory.
    if journal.record(&value.object_id)?.is_some()
        || cache.object(&value.repository_id, repository, &value.object_id, &value.plaintext_sha256)?.is_some()
    {
        cache.put_object(repository, &verified)?;
    }
    Ok(Some(verified))
}

/// A catalog cache hit is useful only when its entire authenticated closure
/// still exists. Checking the root alone misses missing shared packs and leaves.
async fn revalidate_cached_catalog(
    root: &RemoteObject,
    cache: &mut PackageCache,
    journal: &mut TransferJournal,
    root_key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<Option<Vec<RemoteObject>>> {
    let metadata_key = derive_key(root_key, &root.repository_id, "metadata").map_err(corrupt)?;
    let mut pending = vec![root.clone()];
    let mut seen = BTreeMap::new();
    let mut verified = Vec::new();
    while let Some(object) = pending.pop() {
        cancel.check()?;
        let identity = super::gc_store::locator_key(&object.receipt.locator)?;
        let stored = object.stored(repository)?;
        if let Some(previous) = seen.insert(identity, stored.clone()) {
            if previous != stored {
                return Err(corrupt("conflicting catalog references"));
            }
            continue;
        }
        if object.repository_id != root.repository_id
            || !matches!(object.role, ObjectRole::Catalog | ObjectRole::Pack)
        {
            return Err(corrupt("invalid cached catalog child"));
        }
        let previous_locator = object.receipt.locator.clone();
        let Some(object) = revalidate_cached_object(
            &object, cache, journal, provider, repository, cancel,
        ).await? else {
            return Ok(None);
        };
        // Recreating an object on an ID-based provider can change its locator.
        // The existing parent still names the old one and must be rebuilt.
        if object.object_id != root.object_id && object.receipt.locator != previous_locator {
            cache.forget_object(&root.repository_id, repository, &root.object_id)?;
            return Ok(None);
        }
        if object.role == ObjectRole::Catalog {
            let stored = object.stored(repository)?;
            if object.plaintext_length > wire::MAX_METADATA_BYTES as u64 {
                return Err(ProviderError::new(ErrorKind::FileTooLarge));
            }
            let temporary = tempfile::tempdir_in(journal.directory()).map_err(transient)?;
            let path = temporary.path().join("catalog");
            let mut sink = SpoolSink::create(&path, object.receipt.byte_length)?;
            let receipt = match provider.read_object(
                repository, &object.receipt.locator, None, &mut sink, cancel,
            ).await {
                Ok(ReadReceipt::Body(receipt)) => receipt,
                Ok(ReadReceipt::NotModified(_)) => return Err(corrupt("unexpected cached response")),
                Err(error) if error.kind == ErrorKind::NotFound => {
                    cache.forget_object(&object.repository_id, repository, &object.object_id)?;
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            if receipt.locator != object.receipt.locator
                || receipt.byte_length != object.receipt.byte_length
                || !receipt.complete || !sink.is_verified()
            {
                return Err(corrupt("catalog receipt differs"));
            }
            let mut input = crate::trust_boundary::open_regular_source(&path).map_err(transient)?;
            if hex::encode(hash_reader(&mut input, object.receipt.byte_length).map_err(corrupt)?)
                != object.ciphertext_sha256
            {
                return Err(corrupt("catalog ciphertext differs"));
            }
            input.seek(SeekFrom::Start(0)).map_err(transient)?;
            let mut plaintext = Vec::new();
            let header = wire::open_envelope(
                &mut input, &mut plaintext, &metadata_key, wire::MAX_METADATA_BYTES as u64,
            ).map_err(corrupt)?;
            if header != stored.header
                || hex::encode(hash(&plaintext)) != object.plaintext_sha256
                || plaintext.len() as u64 != object.plaintext_length
            {
                return Err(corrupt("catalog plaintext differs"));
            }
            let document = wire::CatalogDocument::decode(&plaintext, wire::MAX_METADATA_BYTES)
                .map_err(corrupt)?;
            for child in document.children {
                pending.push(RemoteObject::from_stored(&child.object, repository)?);
            }
            for pack in document.packs {
                pending.push(RemoteObject::from_stored(&pack, repository)?);
            }
        }
        verified.push(object);
    }
    Ok(Some(verified))
}

fn kind_name(kind: wire::CatalogKind) -> &'static str {
    match kind {
        wire::CatalogKind::Records => "records",
        wire::CatalogKind::Assets => "assets",
        wire::CatalogKind::Section => "section",
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EntryPlan {
    kind: wire::CatalogEntryKind,
    key: String,
    content_sha256: String,
    byte_length: u64,
    chunks: Vec<wire::StoredChunk>,
    packs: Vec<RemoteObject>,
}
impl EntryPlan {
    fn validate(&self, format_repository_id: &str, repository: &RepositoryHandle) -> Result<()> {
        if self.key.is_empty() || !crate::trust_boundary::is_lower_hex_256(&self.content_sha256) {
            return Err(corrupt("invalid cached entry"));
        }
        let total = self.chunks.iter().try_fold(0u64, |sum, chunk| {
            sum.checked_add(chunk.plaintext_length)
                .ok_or_else(|| corrupt("entry length overflow"))
        })?;
        if total != self.byte_length {
            return Err(corrupt("cached entry length"));
        }
        let ids: BTreeSet<_> = self
            .packs
            .iter()
            .map(|pack| pack.object_id.as_str())
            .collect();
        for pack in &self.packs {
            pack.stored(repository)?;
            if pack.repository_id != format_repository_id {
                return Err(corrupt("cached pack repository"));
            }
        }
        if self
            .chunks
            .iter()
            .any(|chunk| !ids.contains(chunk.pack_id.as_str()))
        {
            return Err(corrupt("cached entry misses pack"));
        }
        Ok(())
    }
}

struct SourceEntry {
    kind: wire::CatalogEntryKind,
    key: String,
    content_sha256: String,
    byte_length: u64,
    path: PathBuf,
    compression: CompressionPolicy,
}
#[derive(Clone)]
struct PendingChunk {
    pack_index: usize,
    offset: u64,
    stored_length: u64,
    plaintext_length: u64,
    plaintext_sha256: [u8; 32],
}
struct PendingEntry {
    source: SourceEntry,
    chunks: Vec<PendingChunk>,
}
struct PendingPack {
    file: tempfile::NamedTempFile,
    length: u64,
}

struct PreparedPacks {
    entries: Vec<PendingEntry>,
    packs: Vec<PendingPack>,
}

/// The same envelope and SDK calculation bounds packing, metadata and upload.
/// Alignment of resumable non-final requests remains the provider's responsibility.
struct StoredSize<'a> {
    limits: PackageLimits,
    repository_id: &'a str,
}
impl StoredSize<'_> {
    fn ciphertext(&self, object_id: &str, role: wire::ObjectRole, plaintext: u64) -> Result<u64> {
        let header = wire::PublicObjectHeader::new(
            self.repository_id.into(), object_id.into(), role, plaintext,
        ).map_err(corrupt)?;
        wire::envelope_length(&header).map_err(corrupt)
    }
    fn fits(&self, object_id: &str, role: wire::ObjectRole, plaintext: u64) -> Result<bool> {
        Ok(self.ciphertext(object_id, role, plaintext)?.checked_add(self.limits.sdk_overhead_bytes)
            .is_some_and(|stored| stored <= self.limits.max_stored_bytes))
    }
    fn require(&self, object_id: &str, role: wire::ObjectRole, plaintext: u64) -> Result<u64> {
        let ciphertext = self.ciphertext(object_id, role, plaintext)?;
        if ciphertext.checked_add(self.limits.sdk_overhead_bytes)
            .is_none_or(|stored| stored > self.limits.max_stored_bytes)
        {
            return Err(ProviderError::new(ErrorKind::FileTooLarge));
        }
        Ok(ciphertext)
    }
    /// Pack chunking needs a capacity before its plaintext is assembled. Metadata
    /// instead measures its actual serialized document and has no capacity probe.
    fn pack_capacity(&self, object_id: &str) -> Result<u64> {
        let mut low = 0;
        let mut high = self.limits.max_stored_bytes.checked_sub(self.limits.sdk_overhead_bytes)
            .ok_or_else(|| ProviderError::new(ErrorKind::FileTooLarge))?;
        while low < high {
            let distance = high - low;
            let middle = low + distance / 2 + distance % 2;
            if self.fits(object_id, wire::ObjectRole::Pack, middle)? { low = middle; }
            else { high = middle - 1; }
        }
        if low < 128 { return Err(ProviderError::new(ErrorKind::FileTooLarge)); }
        Ok(low)
    }
}

fn capture_sources(
    capture: &CapturedSnapshot,
    repository_root: &Path,
) -> Result<(Vec<SourceEntry>, Vec<SourceEntry>)> {
    let capture_objects = repository_root.join("external-storage").join("objects");
    let mut records = Vec::new();
    let mut query = capture
        .catalog
        .db
        .prepare("SELECT key,hash,bytes FROM records ORDER BY key")
        .map_err(corrupt)?;
    let mut rows = query.query([]).map_err(corrupt)?;
    while let Some(row) = rows.next().map_err(corrupt)? {
        let key: String = row.get(0).map_err(corrupt)?;
        let digest: String = row.get(1).map_err(corrupt)?;
        let bytes: i64 = row.get(2).map_err(corrupt)?;
        if !crate::trust_boundary::is_lower_hex_256(&digest) {
            return Err(corrupt("capture record hash is invalid"));
        }
        records.push(SourceEntry {
            kind: wire::CatalogEntryKind::Record,
            key,
            content_sha256: digest.clone(),
            byte_length: u64::try_from(bytes).map_err(corrupt)?,
            path: capture_objects.join(digest),
            compression: CompressionPolicy::Text,
        });
    }
    let mut generated = capture
        .catalog
        .db
        .prepare("SELECT hash,bytes FROM generated ORDER BY hash")
        .map_err(corrupt)?;
    for row in generated
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(corrupt)?
    {
        let (digest, bytes) = row.map_err(corrupt)?;
        if !crate::trust_boundary::is_lower_hex_256(&digest) {
            return Err(corrupt("generated object hash is invalid"));
        }
        records.push(SourceEntry {
            kind: wire::CatalogEntryKind::Object,
            key: format!("object/{digest}"),
            content_sha256: digest.clone(),
            byte_length: u64::try_from(bytes).map_err(corrupt)?,
            path: capture_objects.join(digest),
            compression: CompressionPolicy::Text,
        });
    }
    records.sort_by(|a, b| a.key.cmp(&b.key));
    let cas = PayloadCas::new(repository_root).map_err(transient)?;
    let mut assets = Vec::new();
    let mut query = capture.catalog.db.prepare("SELECT DISTINCT d.hash,d.bytes FROM dependencies d LEFT JOIN generated g ON g.hash=d.hash WHERE g.hash IS NULL ORDER BY d.hash").map_err(corrupt)?;
    for row in query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(corrupt)?
    {
        let (digest, bytes) = row.map_err(corrupt)?;
        if !crate::trust_boundary::is_lower_hex_256(&digest) {
            return Err(corrupt("capture dependency hash is invalid"));
        }
        let path = cas
            .object_path(&digest)
            .map_err(transient)?
            .ok_or_else(|| corrupt("capture payload missing"))?;
        assets.push(SourceEntry {
            kind: wire::CatalogEntryKind::Object,
            key: format!("object/{digest}"),
            content_sha256: digest,
            byte_length: u64::try_from(bytes).map_err(corrupt)?,
            path,
            compression: CompressionPolicy::AlreadyCompressed,
        });
    }
    Ok((records, assets))
}

/// One section per catalog, so the package cache key names the section as well
/// as its content. Two sections with the same entries are still two catalogs.
fn section_catalog_fingerprint(id: &str, sources: &[SourceEntry]) -> String {
    format!(
        "{id}-{}",
        catalog_fingerprint(wire::CatalogKind::Section, sources)
    )
}

fn catalog_fingerprint(kind: wire::CatalogKind, sources: &[SourceEntry]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"risunest.external-catalog-fingerprint/v1\0");
    digest.update(kind_name(kind).as_bytes());
    for source in sources {
        digest.update((source.key.len() as u64).to_le_bytes());
        digest.update(source.key.as_bytes());
        digest.update(source.byte_length.to_le_bytes());
        digest.update(source.content_sha256.as_bytes());
    }
    hex::encode(digest.finalize())
}

async fn build_entries(
    kind: wire::CatalogKind,
    sources: Vec<SourceEntry>,
    format_repository_id: &str,
    build_root: &Path,
    root_key: &[u8; 32],
    limits: PackageLimits,
    cache: &mut PackageCache,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<(Vec<EntryPlan>, Vec<RemoteObject>)> {
    let max_pack = StoredSize { limits, repository_id: format_repository_id }
        .pack_capacity(&format!("pack-{}", "0".repeat(64)))?;
    let target = limits.target_plaintext_bytes.min(max_pack).max(1);
    let chunk_bytes = usize::try_from(
        max_pack
            .saturating_sub(ENTRY_OVERHEAD + 1)
            .min(MAX_CHUNK_BYTES as u64),
    )
    .map_err(corrupt)?;
    if chunk_bytes == 0 {
        return Err(ProviderError::new(ErrorKind::FileTooLarge));
    }
    let mut ready = Vec::new();
    let mut uncached = Vec::new();
    for source in sources {
        cancel.check()?;
        if let Some(mut cached) = cache.entry(
            format_repository_id,
            repository,
            kind,
            &source.key,
            &source.content_sha256,
            source.byte_length,
        )? {
            let mut exists = true;
            for pack in &mut cached.packs {
                match revalidate_cached_object(pack, cache, journal, provider, repository, cancel).await? {
                    Some(current) => *pack = current,
                    None => { exists = false; break; }
                }
            }
            if exists {
                ready.push(cached);
                continue;
            }
        }
        uncached.push(source);
    }
    let build_root_owned = build_root.to_path_buf();
    let cancel_owned = cancel.clone();
    let cpu = cpu_permit().await?;
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_packs(
            uncached,
            &build_root_owned,
            target,
            max_pack,
            chunk_bytes,
            &cancel_owned,
        )
    })
    .await
    .map_err(transient)??;
    drop(cpu);
    let PendingBuildParts { pending, packs } = PendingBuildParts::from(prepared);
    let data_key = derive_key(root_key, format_repository_id, "data").map_err(corrupt)?;
    let mut uploaded_packs = Vec::new();
    for mut pack in packs {
        pack.file
            .as_file_mut()
            .seek(SeekFrom::Start(0))
            .map_err(transient)?;
        let plain_hash = hash_reader(pack.file.as_file_mut(), pack.length).map_err(corrupt)?;
        let id = wire::keyed_object_id(
            &data_key,
            journal.job_id(),
            wire::ObjectRole::Pack,
            &plain_hash,
        )
        .map_err(corrupt)?;
        let remote = upload_plain_object(
            pack.file.path(),
            pack.length,
            plain_hash,
            id,
            ObjectRole::Pack,
            format_repository_id,
            &data_key,
            limits,
            cache,
            journal,
            provider,
            repository,
            cancel,
        )
        .await?;
        uploaded_packs.push(remote);
    }
    let mut new_entries = Vec::with_capacity(pending.len());
    for entry in pending {
        let mut used = BTreeSet::new();
        let chunks = entry
            .chunks
            .into_iter()
            .map(|chunk| {
                let pack = uploaded_packs
                    .get(chunk.pack_index)
                    .ok_or_else(|| corrupt("missing completed pack"))?;
                used.insert(chunk.pack_index);
                Ok(wire::StoredChunk {
                    pack_id: pack.object_id.clone(),
                    offset: chunk.offset,
                    stored_length: chunk.stored_length,
                    plaintext_length: chunk.plaintext_length,
                    plaintext_sha256: chunk.plaintext_sha256,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let plan = EntryPlan {
            kind: entry.source.kind,
            key: entry.source.key,
            content_sha256: entry.source.content_sha256,
            byte_length: entry.source.byte_length,
            chunks,
            packs: used
                .into_iter()
                .map(|index| uploaded_packs[index].clone())
                .collect(),
        };
        new_entries.push(plan);
    }
    cache.put_entries(format_repository_id, repository, kind, &new_entries)?;
    ready.extend(new_entries);
    ready.sort_by(|a, b| a.key.cmp(&b.key));
    let mut referenced = BTreeMap::new();
    for entry in &ready {
        for pack in &entry.packs {
            referenced.insert(pack.object_id.clone(), pack.clone());
        }
    }
    Ok((ready, referenced.into_values().collect()))
}

// Named separately so the CPU producer and all capture file I/O run on the
// blocking pool. It emits plaintext pack files and bounded metadata only.
struct PendingBuildParts {
    pending: Vec<PendingEntry>,
    packs: Vec<PendingPack>,
}
impl From<PreparedPacks> for PendingBuildParts {
    fn from(value: PreparedPacks) -> Self {
        Self {
            pending: value.entries,
            packs: value.packs,
        }
    }
}
fn prepare_packs(
    sources: Vec<SourceEntry>,
    build_root: &Path,
    target: u64,
    max_pack: u64,
    chunk_bytes: usize,
    cancel: &Cancellation,
) -> Result<PreparedPacks> {
    fs::create_dir_all(build_root).map_err(transient)?;
    if crate::trust_boundary::is_link_like(&fs::symlink_metadata(build_root).map_err(transient)?) {
        return Err(corrupt("snapshot build directory is a link"));
    }
    let mut encoder = ChunkEncoder::new().map_err(corrupt)?;
    let mut packs: Vec<PendingPack> = Vec::new();
    let mut current: Option<PendingPack> = None;
    let mut pending = Vec::new();
    for source in sources {
        cancel.check()?;
        let mut input =
            crate::trust_boundary::open_regular_source(&source.path).map_err(corrupt)?;
        if input.metadata().map_err(corrupt)?.len() != source.byte_length {
            return Err(corrupt("capture source length differs"));
        }
        let mut remaining = source.byte_length;
        let mut source_hash = Sha256::new();
        let mut chunks = Vec::new();
        loop {
            let count = if remaining == 0 && chunks.is_empty() {
                0
            } else {
                remaining.min(chunk_bytes as u64) as usize
            };
            let mut bytes = vec![0; count];
            input.read_exact(&mut bytes).map_err(corrupt)?;
            source_hash.update(&bytes);
            let chunk = Chunk {
                hash: hash(&bytes),
                bytes,
            };
            let mut encoded = Vec::new();
            let stored_length =
                pack::write_entry_with(&mut encoded, &chunk, &mut encoder, source.compression)
                    .map_err(corrupt)?;
            if current
                .as_ref()
                .is_some_and(|pack| pack.length > 0 && pack.length + stored_length > target)
            {
                packs.push(current.take().unwrap());
            }
            if stored_length > max_pack {
                return Err(ProviderError::new(ErrorKind::FileTooLarge));
            }
            if current.is_none() {
                current = Some(PendingPack {
                    file: tempfile::NamedTempFile::new_in(build_root).map_err(transient)?,
                    length: 0,
                });
            }
            let pack = current.as_mut().unwrap();
            if pack.length + stored_length > max_pack {
                return Err(ProviderError::new(ErrorKind::FileTooLarge));
            }
            let pack_index = packs.len();
            let offset = pack.length;
            pack.file
                .as_file_mut()
                .write_all(&encoded)
                .map_err(transient)?;
            pack.length += stored_length;
            chunks.push(PendingChunk {
                pack_index,
                offset,
                stored_length,
                plaintext_length: count as u64,
                plaintext_sha256: chunk.hash,
            });
            if remaining == 0 {
                break;
            }
            remaining -= count as u64;
            if remaining == 0 {
                break;
            }
        }
        let actual = hex::encode(source_hash.finalize());
        if actual != source.content_sha256 {
            return Err(corrupt("capture source hash differs"));
        }
        pending.push(PendingEntry { source, chunks });
    }
    if let Some(pack) = current {
        packs.push(pack);
    }
    for pack in &mut packs {
        pack.file.as_file_mut().sync_all().map_err(transient)?;
    }
    Ok(PreparedPacks {
        entries: pending,
        packs,
    })
}

async fn upload_plain_object(
    path: &Path,
    plaintext_length: u64,
    plaintext_hash: [u8; 32],
    object_id: String,
    role: ObjectRole,
    format_repository_id: &str,
    key: &[u8; 32],
    limits: PackageLimits,
    cache: &mut PackageCache,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<RemoteObject> {
    let plaintext_sha256 = hex::encode(plaintext_hash);
    if let Some(value) = cache.object(
        format_repository_id,
        repository,
        &object_id,
        &plaintext_sha256,
    )? {
        if value.object_id != object_id || value.role != role
            || value.plaintext_length != plaintext_length
            || value.plaintext_sha256 != plaintext_sha256
        {
            return Err(corrupt("cached plaintext identity differs"));
        }
        if let Some(current) = revalidate_cached_object(
            &value, cache, journal, provider, repository, cancel,
        ).await? {
            return Ok(current);
        }
    }
    let wire_role = wire_role(role)?;
    let header = wire::PublicObjectHeader::new(
        format_repository_id.into(),
        object_id.clone(),
        wire_role,
        plaintext_length,
    )
    .map_err(corrupt)?;
    let ciphertext_length = StoredSize { limits, repository_id: format_repository_id }
        .require(&object_id, wire_role, plaintext_length)?;
    if let Some(record) = journal.record(&object_id)? {
        if record.intent.role != role || record.intent.byte_length != ciphertext_length {
            return Err(corrupt("journal role differs"));
        }
        let spool = journal.spool_path(&object_id);
        let downloaded;
        let cipher_path = if spool.exists() {
            spool.clone()
        } else if let Some(receipt) = &record.receipt {
            validate_receipt(&record.intent, repository, receipt)?;
            let parent = path
                .parent()
                .ok_or_else(|| corrupt("plaintext path has no parent"))?;
            downloaded = tempfile::tempdir_in(parent).map_err(transient)?;
            let downloaded_path = downloaded.path().join("ciphertext");
            let mut sink = SpoolSink::create(&downloaded_path, record.intent.byte_length)?;
            let read = provider
                .read_object(repository, &receipt.locator, None, &mut sink, cancel)
                .await?;
            if !matches!(read, ReadReceipt::Body(body) if body.complete && body.locator == receipt.locator && body.byte_length == record.intent.byte_length)
                || !sink.is_verified()
            {
                return Err(corrupt(
                    "completed journal object could not be reconstructed",
                ));
            }
            downloaded_path
        } else {
            return Err(ProviderError::new(ErrorKind::NotFound));
        };
        let expected_ciphertext = record.intent.sha256.clone();
        let expected_plaintext = plaintext_hash;
        let header_copy = header.clone();
        let key_copy = *key;
        let cpu = cpu_permit().await?;
        tokio::task::spawn_blocking(move || {
            let mut input =
                crate::trust_boundary::open_regular_source(&cipher_path).map_err(corrupt)?;
            if input.metadata().map_err(corrupt)?.len() != ciphertext_length
                || hex::encode(hash_reader(&mut input, ciphertext_length).map_err(corrupt)?)
                    != expected_ciphertext
            {
                return Err(corrupt("journal ciphertext differs"));
            }
            input.seek(SeekFrom::Start(0)).map_err(corrupt)?;
            struct HashSink {
                digest: Sha256,
                length: u64,
            }
            impl Write for HashSink {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    self.digest.update(bytes);
                    self.length = self
                        .length
                        .checked_add(bytes.len() as u64)
                        .ok_or_else(|| std::io::Error::other("plaintext length overflow"))?;
                    Ok(bytes.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let mut output = HashSink {
                digest: Sha256::new(),
                length: 0,
            };
            let opened = wire::open_envelope(&mut input, &mut output, &key_copy, plaintext_length)
                .map_err(corrupt)?;
            let actual: [u8; 32] = output.digest.finalize().into();
            if opened != header_copy
                || output.length != plaintext_length
                || actual != expected_plaintext
            {
                return Err(corrupt("journal plaintext differs"));
            }
            Ok(())
        })
        .await
        .map_err(transient)??;
        drop(cpu);
        let receipt =
            transfer_job::upload(journal, &object_id, provider, repository, cancel).await?;
        let value = RemoteObject {
            repository_id: format_repository_id.into(),
            object_id,
            role,
            receipt,
            ciphertext_sha256: record.intent.sha256,
            plaintext_length,
            plaintext_sha256,
        };
        cache.put_object(repository, &value)?;
        return Ok(value);
    }
    let spool = journal.spool_path(&object_id);
    let input_path = path.to_path_buf();
    let spool_path = spool.clone();
    let key_copy = *key;
    let header_copy = header.clone();
    let cpu = cpu_permit().await?;
    let ciphertext_sha256 = tokio::task::spawn_blocking(move || {
        if spool_path.exists() {
            fs::remove_file(&spool_path).map_err(transient)?;
        }
        let mut input = crate::trust_boundary::open_regular_source(&input_path).map_err(corrupt)?;
        if input.metadata().map_err(corrupt)?.len() != plaintext_length {
            return Err(corrupt("plaintext length differs"));
        }
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&spool_path)
            .map_err(transient)?;
        wire::seal_envelope(&mut input, &mut output, &key_copy, &header_copy).map_err(corrupt)?;
        output.sync_all().map_err(transient)?;
        drop(output);
        let mut ciphertext =
            crate::trust_boundary::open_regular_source(&spool_path).map_err(corrupt)?;
        if ciphertext.metadata().map_err(corrupt)?.len() != ciphertext_length {
            return Err(corrupt("ciphertext length differs"));
        }
        Ok(hex::encode(
            hash_reader(&mut ciphertext, ciphertext_length).map_err(corrupt)?,
        ))
    })
    .await
    .map_err(transient)??;
    drop(cpu);
    let intent = ObjectIntent {
        repository_id: repository.repository_id.clone(),
        job_id: journal.job_id().to_owned(),
        object_id: object_id.clone(),
        role,
        byte_length: ciphertext_length,
        sha256: ciphertext_sha256.clone(),
    };
    journal.register(&intent)?;
    let receipt = transfer_job::upload(journal, &object_id, provider, repository, cancel).await?;
    let value = RemoteObject {
        repository_id: format_repository_id.into(),
        object_id,
        role,
        receipt,
        ciphertext_sha256,
        plaintext_length,
        plaintext_sha256,
    };
    cache.put_object(repository, &value)?;
    Ok(value)
}

fn unique_packs(
    entries: &[wire::CatalogEntryFragment],
    available: &BTreeMap<String, wire::StoredObject>,
) -> Result<Vec<wire::StoredObject>> {
    let ids: BTreeSet<_> = entries
        .iter()
        .flat_map(|entry| entry.chunks.iter().map(|chunk| chunk.pack_id.as_str()))
        .collect();
    ids.into_iter()
        .map(|id| {
            available
                .get(id)
                .cloned()
                .ok_or_else(|| corrupt("catalog misses pack locator"))
        })
        .collect()
}

const TARGET_CATALOG_FRAGMENTS: usize = 512;

struct EncodedCatalog {
    document: wire::CatalogDocument,
    bytes: Vec<u8>,
}

/// Invalid metadata is an error, not an oversized leaf. The accepted bytes are
/// the exact bytes uploaded, including final fragment indices and counts.
fn measure_catalog(document: wire::CatalogDocument, size: &StoredSize<'_>) -> Result<Option<EncodedCatalog>> {
    let bytes = match document.encode(wire::MAX_METADATA_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.0 == "catalog-limit-exceeded" => return Ok(None),
        Err(error) => return Err(corrupt(error)),
    };
    // Keyed catalog IDs always contain this fixed prefix and 64 unescaped hex digits.
    if !size.fits(&format!("catalog-{}", "0".repeat(64)), wire::ObjectRole::Catalog, bytes.len() as u64)? {
        return Ok(None);
    }
    Ok(Some(EncodedCatalog { document, bytes }))
}

/// A single measured-serialization path partitions both leaves and branches.
/// The winning serialization is retained rather than probed and rebuilt again.
fn catalog_batches<T>(
    items: &[T],
    size: &StoredSize<'_>,
    document: impl Fn(&[T]) -> Result<wire::CatalogDocument>,
) -> Result<Vec<EncodedCatalog>> {
    let too_large = || ProviderError::new(ErrorKind::FileTooLarge);
    if items.is_empty() {
        return Ok(vec![measure_catalog(document(items)?, size)?.ok_or_else(too_large)?]);
    }
    let mut result = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let end = items.len().min(start.saturating_add(TARGET_CATALOG_FRAGMENTS));
        if let Some(encoded) = measure_catalog(document(&items[start..end])?, size)? {
            result.push(encoded);
            start = end;
            continue;
        }
        let mut fits_end = start;
        let mut too_large_end = end;
        let mut best = None;
        while fits_end + 1 < too_large_end {
            let middle = fits_end + (too_large_end - fits_end) / 2;
            match measure_catalog(document(&items[start..middle])?, size)? {
                Some(encoded) => { fits_end = middle; best = Some(encoded); }
                None => too_large_end = middle,
            }
        }
        result.push(best.ok_or_else(too_large)?);
        start = fits_end;
    }
    Ok(result)
}

async fn build_catalog(
    kind: wire::CatalogKind,
    entries: Vec<EntryPlan>,
    fingerprint: &str,
    format_repository_id: &str,
    build_root: &Path,
    root_key: &[u8; 32],
    limits: PackageLimits,
    cache: &mut PackageCache,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
    referenced: &mut Vec<RemoteObject>,
) -> Result<RemoteObject> {
    if let Some(root) = cache.catalog(format_repository_id, repository, kind, fingerprint)? {
        if let Some(objects) = revalidate_cached_catalog(
            &root, cache, journal, root_key, provider, repository, cancel,
        ).await? {
            let current = objects.first().cloned().ok_or_else(|| corrupt("empty catalog closure"))?;
            referenced.extend(objects);
            return Ok(current);
        }
    }
    let size = StoredSize { limits, repository_id: format_repository_id };
    let metadata_key = derive_key(root_key, format_repository_id, "metadata").map_err(corrupt)?;
    let mut available_packs = BTreeMap::new();
    for entry in &entries {
        for pack in &entry.packs {
            let stored = pack.stored(repository)?;
            if let Some(old) = available_packs.insert(pack.object_id.clone(), stored.clone()) {
                if old != stored {
                    return Err(corrupt("conflicting pack locator"));
                }
            }
        }
    }
    let mut fragments = Vec::new();
    for entry in &entries {
        // Fixed chunk fragments make the final count known before sizing. A
        // growing decimal count cannot make a previously accepted leaf overflow.
        let count = u32::try_from(entry.chunks.len()).map_err(corrupt)?;
        if count == 0 { return Err(corrupt("entry has no chunks")); }
        for (index, chunk) in entry.chunks.iter().enumerate() {
            fragments.push(wire::CatalogEntryFragment {
                kind: entry.kind, key: entry.key.clone(),
                content_sha256: decode_hash(&entry.content_sha256)?, byte_length: entry.byte_length,
                fragment_index: index as u32, fragment_count: count, chunks: vec![chunk.clone()],
            });
        }
    }
    let leaves = catalog_batches(&fragments, &size, |leaf| {
        wire::CatalogDocument::leaf(kind, leaf.to_vec(), unique_packs(leaf, &available_packs)?)
            .map_err(corrupt)
    })?;
    let mut nodes = Vec::new();
    for EncodedCatalog { document, bytes } in leaves {
        let remote = upload_metadata_bytes(
            &bytes,
            "catalog",
            ObjectRole::Catalog,
            format_repository_id,
            &metadata_key,
            limits,
            build_root,
            cache,
            journal,
            provider,
            repository,
            cancel,
        )
        .await?;
        referenced.push(remote.clone());
        nodes.push(wire::CatalogChild {
            first_key: document.first_key,
            last_key: document.last_key,
            object: remote.stored(repository)?,
        });
    }
    let mut level = 1u16;
    while nodes.len() > 1 {
        let previous_count = nodes.len();
        let mut next = Vec::new();
        let batches = catalog_batches(&nodes, &size, |children| {
            wire::CatalogDocument::branch(kind, level, children.to_vec()).map_err(corrupt)
        })?;
        for EncodedCatalog { document, bytes } in batches {
            let remote = upload_metadata_bytes(
                &bytes,
                "catalog",
                ObjectRole::Catalog,
                format_repository_id,
                &metadata_key,
                limits,
                build_root,
                cache,
                journal,
                provider,
                repository,
                cancel,
            )
            .await?;
            referenced.push(remote.clone());
            next.push(wire::CatalogChild {
                first_key: document.first_key,
                last_key: document.last_key,
                object: remote.stored(repository)?,
            });
        }
        if next.len() >= previous_count {
            return Err(ProviderError::new(ErrorKind::FileTooLarge));
        }
        nodes = next;
        level = level
            .checked_add(1)
            .ok_or_else(|| corrupt("catalog depth"))?;
    }
    let root = RemoteObject::from_stored(
        &nodes
            .into_iter()
            .next()
            .ok_or_else(|| corrupt("catalog root"))?
            .object,
        repository,
    )?;
    cache.put_catalog(repository, kind, fingerprint, &root)?;
    Ok(root)
}

async fn upload_metadata_bytes(
    bytes: &[u8],
    _prefix: &str,
    role: ObjectRole,
    format_repository_id: &str,
    key: &[u8; 32],
    limits: PackageLimits,
    build_root: &Path,
    cache: &mut PackageCache,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<RemoteObject> {
    let mut file = tempfile::NamedTempFile::new_in(build_root).map_err(transient)?;
    file.write_all(bytes).map_err(transient)?;
    file.as_file_mut().sync_all().map_err(transient)?;
    let digest = hash(bytes);
    let object_id =
        wire::keyed_object_id(key, journal.job_id(), wire_role(role)?, &digest).map_err(corrupt)?;
    upload_plain_object(
        file.path(),
        bytes.len() as u64,
        digest,
        object_id,
        role,
        format_repository_id,
        key,
        limits,
        cache,
        journal,
        provider,
        repository,
        cancel,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn package_and_upload(
    capture: CapturedSnapshot,
    sections: Vec<CapturedSection>,
    repository_root: &Path,
    cache_root: &Path,
    metadata: SnapshotMetadata,
    root_key: &[u8; 32],
    limits: PackageLimits,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<CompletedSnapshot> {
    cancel.check()?;
    if metadata.repository_id.is_empty()
        || metadata.snapshot_id.is_empty()
        || metadata.logical_revision > i64::MAX as u64
        || capture.identity.revision < 0
        || capture.identity.revision as u64 != metadata.logical_revision
        || capture.identity.library_epoch != metadata.library_id
        || capture.identity.store_id != metadata.author_device_id
    {
        return Err(corrupt("snapshot metadata differs from repository"));
    }
    let build_root = cache_root.join("build");
    let mut cache = PackageCache::open(cache_root)?;
    let repository_root_owned = repository_root.to_path_buf();
    let library_domain = library_fingerprint_domain();
    let cpu = cpu_permit().await?;
    let (record_sources, asset_sources, observed_fingerprint) =
        tokio::task::spawn_blocking(move || {
            let (records, assets) = capture_sources(&capture, &repository_root_owned)?;
            let fingerprint = capture
                .catalog
                .content_fingerprint(&library_domain)
                .map_err(corrupt)?;
            Ok((records, assets, fingerprint))
        })
        .await
        .map_err(transient)??;
    drop(cpu);
    if observed_fingerprint != metadata.content_fingerprint {
        return Err(corrupt("snapshot library fingerprint differs"));
    }
    let record_fingerprint = catalog_fingerprint(wire::CatalogKind::Records, &record_sources);
    let asset_fingerprint = catalog_fingerprint(wire::CatalogKind::Assets, &asset_sources);
    let (record_entries, mut referenced) = build_entries(
        wire::CatalogKind::Records,
        record_sources,
        &metadata.repository_id,
        &build_root,
        root_key,
        limits,
        &mut cache,
        journal,
        provider,
        repository,
        cancel,
    )
    .await?;
    let record_catalog = build_catalog(
        wire::CatalogKind::Records,
        record_entries,
        &record_fingerprint,
        &metadata.repository_id,
        &build_root,
        root_key,
        limits,
        &mut cache,
        journal,
        provider,
        repository,
        cancel,
        &mut referenced,
    )
    .await?;
    let cached_assets = match cache.catalog(
        &metadata.repository_id,
        repository,
        wire::CatalogKind::Assets,
        &asset_fingerprint,
    )? {
        Some(root) => revalidate_cached_catalog(
            &root, &mut cache, journal, root_key, provider, repository, cancel,
        ).await?,
        None => None,
    };
    let asset_catalog = if let Some(objects) = cached_assets {
        let root = objects.first().cloned().ok_or_else(|| corrupt("empty asset catalog closure"))?;
        referenced.extend(objects);
        root
    } else {
        let (asset_entries, uploaded) = build_entries(
            wire::CatalogKind::Assets,
            asset_sources,
            &metadata.repository_id,
            &build_root,
            root_key,
            limits,
            &mut cache,
            journal,
            provider,
            repository,
            cancel,
        )
        .await?;
        referenced.extend(uploaded);
        build_catalog(
            wire::CatalogKind::Assets,
            asset_entries,
            &asset_fingerprint,
            &metadata.repository_id,
            &build_root,
            root_key,
            limits,
            &mut cache,
            journal,
            provider,
            repository,
            cancel,
            &mut referenced,
        )
        .await?
    };
    let library = wire::LibrarySnapshotRef {
        record_catalog: record_catalog.stored(repository)?,
        asset_catalog: asset_catalog.stored(repository)?,
        content_fingerprint: metadata.content_fingerprint,
    };
    // A published state keeps the sections the observed state already had and
    // replaces only the ones this device captured. A bundle starts empty and
    // declares exactly what it covers.
    let mut published_sections = match &metadata.purpose {
        SnapshotPurpose::SyncState {
            parent_sections, ..
        } => parent_sections.clone(),
        SnapshotPurpose::BackupBundle { .. } => BTreeMap::new(),
    };
    for captured in sections {
        // An unchanged section keeps the reference the observed state carried,
        // so its commit number still names the publication that changed it.
        if published_sections.get(captured.kind.id()).is_some_and(|carried| {
            carried.content_fingerprint == captured.content_fingerprint
                && carried.gc_floor == captured.gc_floor
                && carried.max_write_clock == captured.max_write_clock
        }) {
            continue;
        }
        let sources: Vec<SourceEntry> = captured
            .sources
            .iter()
            .map(|source| SourceEntry {
                kind: source.kind,
                key: source.key.clone(),
                content_sha256: source.content_sha256.clone(),
                byte_length: source.byte_length,
                path: source.path.clone(),
                compression: match source.kind {
                    wire::CatalogEntryKind::SectionObject => CompressionPolicy::AlreadyCompressed,
                    _ => CompressionPolicy::Text,
                },
            })
            .collect();
        let id = captured.kind.id();
        let section_fingerprint = section_catalog_fingerprint(id, &sources);
        let (section_entries, uploaded) = build_entries(
            wire::CatalogKind::Section,
            sources,
            &metadata.repository_id,
            &build_root,
            root_key,
            limits,
            &mut cache,
            journal,
            provider,
            repository,
            cancel,
        )
        .await?;
        referenced.extend(uploaded);
        let entries_root = build_catalog(
            wire::CatalogKind::Section,
            section_entries,
            &section_fingerprint,
            &metadata.repository_id,
            &build_root,
            root_key,
            limits,
            &mut cache,
            journal,
            provider,
            repository,
            cancel,
            &mut referenced,
        )
        .await?;
        published_sections.insert(
            id.to_owned(),
            wire::SectionSnapshotRef {
                kind: captured.kind,
                codec: SECTION_CODEC.into(),
                generation: captured.generation,
                gc_floor: captured.gc_floor,
                max_write_clock: captured.max_write_clock,
                entries_root: entries_root.stored(repository)?,
                content_fingerprint: captured.content_fingerprint,
            },
        );
    }
    // Carried, unselected sections are publication references too. Never
    // silently drop them, and never publish a cached reference with a hole.
    for section in published_sections.values() {
        let root = RemoteObject::from_stored(&section.entries_root, repository)?;
        let objects = revalidate_cached_catalog(
            &root, &mut cache, journal, root_key, provider, repository, cancel,
        ).await?.ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        referenced.extend(objects);
    }
    let (bytes, fingerprint, sections, role) = match metadata.purpose {
        SnapshotPurpose::SyncState {
            epoch, generation, ..
        } => {
            let document = wire::SyncStateDocument::new(
                metadata.snapshot_id.clone(),
                metadata.repository_id.clone(),
                metadata.library_id,
                epoch,
                generation,
                metadata.parent_snapshot_id,
                metadata.author_device_id,
                metadata.created_at_ms,
                library,
                published_sections,
            )
            .map_err(corrupt)?;
            let max_state = wire::MAX_METADATA_BYTES;
            (
                document.encode(max_state).map_err(corrupt)?,
                document.state_fingerprint,
                document.sections,
                ObjectRole::SyncState,
            )
        }
        SnapshotPurpose::BackupBundle {
            source,
            remote_generation,
        } => {
            let document = wire_control::BackupBundleDocument::new(
                metadata.repository_id.clone(),
                metadata.snapshot_id.clone(),
                source,
                metadata.created_at_ms,
                Some(Sequence::from(metadata.logical_revision)),
                None,
                remote_generation,
                library,
                published_sections,
            )
            .map_err(corrupt)?;
            let max_bundle = wire::MAX_METADATA_BYTES;
            (
                document.encode(max_bundle).map_err(corrupt)?,
                document.bundle_fingerprint,
                document.sections,
                ObjectRole::BackupBundle,
            )
        }
    };
    let metadata_key =
        derive_key(root_key, &metadata.repository_id, "metadata").map_err(corrupt)?;
    let mut file = tempfile::NamedTempFile::new_in(&build_root).map_err(transient)?;
    file.write_all(&bytes).map_err(transient)?;
    file.as_file_mut().sync_all().map_err(transient)?;
    // The stable random snapshot ID is the immutable object identity. A retry
    // with different bytes is rejected by the journal instead of overwriting it.
    let reference = upload_plain_object(
        file.path(),
        bytes.len() as u64,
        hash(&bytes),
        format!("snapshot-{}", metadata.snapshot_id),
        role,
        &metadata.repository_id,
        &metadata_key,
        limits,
        &mut cache,
        journal,
        provider,
        repository,
        cancel,
    )
    .await?;
    referenced.sort_by(|a, b| a.object_id.cmp(&b.object_id));
    referenced.dedup_by(|a, b| a.object_id == b.object_id);
    Ok(CompletedSnapshot {
        reference,
        snapshot_id: metadata.snapshot_id,
        repository_id: metadata.repository_id,
        fingerprint: hex::encode(fingerprint),
        library_fingerprint: hex::encode(metadata.content_fingerprint),
        logical_revision: metadata.logical_revision,
        record_catalog,
        asset_catalog,
        sections,
        referenced_objects: referenced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        external_storage::{
            fake::{self, FakeProvider},
            journal::JobIdentity,
            snapshot_restore,
        },
        persistent_store::{
            content_capture::ContentCaptureSink,
            device_store::sections::{SectionRow, SectionValueRow},
            sync_selection::CaptureIdentity, PersistentStore,
        },
    };
    use risunest_external_storage_format::section::SectionKind;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn captured(
        root: &Path,
        capture_id: &str,
        revision: i64,
        record: &[u8],
        asset: &[u8],
    ) -> (CapturedSnapshot, String) {
        let cas = PayloadCas::new(root).unwrap();
        let asset = cas.prepare_bytes(asset).unwrap();
        let directory = root
            .join("external-storage")
            .join("captures")
            .join(capture_id);
        let objects = root.join("external-storage").join("objects");
        let mut catalog =
            crate::external_storage::capture::CaptureCatalog::create(&directory, &objects, None)
                .unwrap();
        let identity = CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "epoch".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision,
        };
        catalog.begin(&identity, None).unwrap();
        catalog.record("root", record).unwrap();
        catalog
            .reference("root", &asset.content_hash, asset.byte_size)
            .unwrap();
        catalog.finish().unwrap();
        (
            CapturedSnapshot {
                id: capture_id.into(),
                identity,
                catalog,
                projected_records: 1,
                shared: false,
            },
            asset.content_hash,
        )
    }

    fn captured_asset_inventory(
        root: &Path,
        capture_id: &str,
        revision: i64,
        records: usize,
        edited: usize,
        assets: usize,
    ) -> CapturedSnapshot {
        let directory = root
            .join("external-storage")
            .join("captures")
            .join(capture_id);
        let objects = root.join("external-storage").join("objects");
        let mut catalog =
            crate::external_storage::capture::CaptureCatalog::create(&directory, &objects, None)
                .unwrap();
        let identity = CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "epoch".into(),
            generation: format!("generation-{revision}"),
            selection_epoch: "selection".into(),
            revision,
        };
        catalog.begin(&identity, None).unwrap();
        let unchanged = b"{\"value\":0}".to_vec();
        let changed = format!("{{\"value\":{revision}}}").into_bytes();
        let unchanged_hash = hex::encode(hash(&unchanged));
        let changed_hash = hex::encode(hash(&changed));
        fs::write(objects.join(&unchanged_hash), &unchanged).unwrap();
        fs::write(objects.join(&changed_hash), &changed).unwrap();
        {
            let mut insert = catalog
                .db
                .prepare("INSERT INTO records VALUES(?1,?2,?3)")
                .unwrap();
            for index in 0..records {
                let (digest, length) = if index < edited {
                    (&changed_hash, changed.len())
                } else {
                    (&unchanged_hash, unchanged.len())
                };
                insert
                    .execute(params![format!("record/{index:06}"), digest, length as i64])
                    .unwrap();
            }
        }
        let _cas = PayloadCas::new(root).unwrap();
        let asset_root = root.join("assets-v2").join("objects");
        for shard in 0u16..=255 {
            fs::create_dir_all(asset_root.join(format!("{shard:02x}"))).unwrap();
        }
        let mut insert_dependency = catalog
            .db
            .prepare("INSERT INTO dependencies VALUES(?1,?2,?3)")
            .unwrap();
        for index in 0..assets {
            let bytes = (index as u64).to_le_bytes();
            let digest = hex::encode(hash(&bytes));
            let shard = asset_root.join(&digest[..2]);
            let path = shard.join(&digest[2..]);
            if !path.exists() {
                fs::write(path, bytes).unwrap();
            }
            insert_dependency
                .execute(params!["record/000000", digest, bytes.len() as i64])
                .unwrap();
        }
        drop(insert_dependency);
        catalog.finish().unwrap();
        CapturedSnapshot {
            id: capture_id.into(),
            identity,
            catalog,
            projected_records: records,
            shared: false,
        }
    }

    fn metadata(id: &str, capture: &CapturedSnapshot) -> SnapshotMetadata {
        SnapshotMetadata {
            snapshot_id: id.into(),
            repository_id: "format-repository".into(),
            library_id: capture.identity.library_epoch.clone(),
            author_device_id: capture.identity.store_id.clone(),
            created_at_ms: 1,
            logical_revision: capture.identity.revision as u64,
            purpose: SnapshotPurpose::SyncState {
                epoch: "epoch".into(),
                generation: risunest_sync_wire::head::Sequence::from(1u64),
                parent_sections: std::collections::BTreeMap::new(),
            },
            parent_snapshot_id: None,
            content_fingerprint: capture.catalog.content_fingerprint(&library_fingerprint_domain()).unwrap(),
        }
    }

    fn journal(root: &Path, job: &str, capture: &CapturedSnapshot) -> TransferJournal {
        TransferJournal::open(
            root,
            JobIdentity {
                job_id: job.into(),
                connection_id: "connection".into(),
                repository_id: fake::repository().repository_id,
                capture_id: capture.id.clone(),
                capture: capture.identity.clone(),
            },
        )
        .unwrap()
    }

    fn limits(max: u64) -> PackageLimits {
        PackageLimits {
            max_stored_bytes: max,
            sdk_overhead_bytes: 0,
            target_plaintext_bytes: max,
        }
    }

    fn section_rows(kind: SectionKind, marker: &str) -> Vec<SectionRow> {
        use crate::persistent_store::device_store::sections::SectionValueRow;
        match kind {
            SectionKind::Hypa => vec![SectionRow {
                key1: "a".repeat(64),
                key2: String::new(),
                key3: String::new(),
                value: SectionValueRow::Hypa {
                    producer: "hypa-v2".into(),
                    model: marker.into(),
                    endpoint: None,
                    preprocess_version: 1,
                    dimensions: 4,
                    vector: vec![1u8; 16],
                    metadata: None,
                },
                write_clock: risunest_sync_wire::head::Sequence::from(5u64),
                writer_id: "writer-a".into(),
            }],
            _ => vec![SectionRow {
                key1: "plugin-a".into(),
                key2: "string".into(),
                key3: "token".into(),
                value: SectionValueRow::Plugin {
                    space: "string".into(),
                    value: marker.into(),
                },
                write_clock: risunest_sync_wire::head::Sequence::from(6u64),
                writer_id: "writer-a".into(),
            }],
        }
    }

    fn materialized_section_rows(
        source: &CapturedSection,
        versioned: bool,
    ) -> Vec<SectionRow> {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let section = crate::external_storage::sections::section_of(source.kind).unwrap();
        if versioned {
            store
                .device_store_mut()
                .unwrap()
                .set_section_participating(section, true)
                .unwrap();
            crate::external_storage::sections::apply_received_section(
                &mut store,
                "connection",
                "library",
                crate::external_storage::sections::SectionArrival::Continuing,
                source,
            )
            .unwrap();
        } else {
            let mut prepared = crate::external_storage::sections::prepare_received_backup_sections(
                std::slice::from_ref(source),
                &Cancellation::default(),
            )
            .unwrap();
            store
                .device_store_mut()
                .unwrap()
                .restore_prepared_backup_section(&prepared.remove(0))
                .unwrap();
        }
        store
            .device_store_mut()
            .unwrap()
            .read_backup_section_rows(section)
            .unwrap()
    }

    fn section_values(
        rows: Vec<SectionRow>,
    ) -> Vec<(String, String, String, SectionValueRow)> {
        rows.into_iter()
            .map(|row| (row.key1, row.key2, row.key3, row.value))
            .collect()
    }

    fn section(
        spool: &Path,
        kind: SectionKind,
        marker: &str,
        generation: u64,
    ) -> CapturedSection {
        crate::external_storage::sections::capture_section(
            kind,
            &section_rows(kind, marker),
            generation != 0,
            risunest_sync_wire::head::Sequence::from(generation),
            risunest_sync_wire::head::Sequence::from(0u64),
            risunest_sync_wire::head::Sequence::from(if generation == 0 { 0u64 } else { 6u64 }),
            &spool.join(format!("{}-{marker}", kind.id())),
            &Cancellation::default(),
        )
        .unwrap()
    }

    /// Invariants 17, 30 and 33. A publication that does not carry a section
    /// keeps the reference the observed state had, including the commit number
    /// that named the publication which last changed it, and that inherited
    /// catalog is still readable from the newest state.
    #[test]
    fn a_section_this_publication_did_not_capture_keeps_the_reference_it_inherited() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("package-cache");
            let spool = root.path().join("section-spool");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [9; 32];
            let publish = |id: &'static str,
                           revision: i64,
                           generation: u64,
                           carried: Vec<CapturedSection>,
                           inherited: BTreeMap<String, wire::SectionSnapshotRef>| {
                let cache = cache.clone();
                let provider = &provider;
                let repository = &repository;
                let root = root.path().to_path_buf();
                async move {
                    let (capture, _) =
                        captured(&root, &format!("capture-{id}"), revision, b"record", b"asset");
                    let mut snapshot_metadata = metadata(id, &capture);
                    snapshot_metadata.purpose = SnapshotPurpose::SyncState {
                        epoch: "epoch".into(),
                        generation: risunest_sync_wire::head::Sequence::from(generation),
                        parent_sections: inherited,
                    };
                    let mut transfer = journal(&root.join(id), id, &capture);
                    package_and_upload(
                        capture,
                        carried,
                        &root,
                        &cache,
                        snapshot_metadata,
                        &key,
                        limits(256 * 1024),
                        &mut transfer,
                        provider,
                        repository,
                        &Cancellation::default(),
                    )
                    .await
                    .unwrap()
                }
            };
            let first = publish(
                "state-1",
                1,
                1,
                vec![
                    section(&spool, SectionKind::Hypa, "first", 1),
                    section(&spool, SectionKind::LocalPlugins, "first", 1),
                ],
                BTreeMap::new(),
            )
            .await;
            assert_eq!(first.sections.len(), 2);

            // Only the embeddings changed, so the plugin reference has to come
            // through untouched, commit number included.
            let second = publish(
                "state-2",
                2,
                2,
                vec![section(&spool, SectionKind::Hypa, "second", 2)],
                first.sections.clone(),
            )
            .await;
            assert_eq!(
                second.sections["local-plugins"],
                first.sections["local-plugins"]
            );
            assert_ne!(second.sections["hypa"], first.sections["hypa"]);
            assert_eq!(
                second.sections["hypa"].generation,
                risunest_sync_wire::head::Sequence::from(2u64)
            );

            // Republishing the same embeddings keeps the earlier commit number,
            // because nothing about that section changed.
            let third = publish(
                "state-3",
                3,
                3,
                vec![section(&spool, SectionKind::Hypa, "second", 3)],
                second.sections.clone(),
            )
            .await;
            assert_eq!(third.sections, second.sections);

            // A library-only publication leaves both references in place and
            // the inherited catalogs still read back.
            let fourth = publish("state-4", 4, 4, Vec::new(), third.sections.clone()).await;
            assert_eq!(fourth.sections, third.sections);
            let staging = root.path().join("receive");
            let received = snapshot_restore::download_sections(
                &fourth.reference,
                &BTreeSet::from(["local-plugins".to_owned()]),
                &staging,
                &key,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(received.len(), 1);
            let rows = materialized_section_rows(&received[0], true);
            assert_eq!(
                section_values(rows),
                section_values(section_rows(SectionKind::LocalPlugins, "first"))
            );
        });
    }

    /// Invariant 32. Two devices produce separate bundles. Neither carries the
    /// other's section material and nothing is merged between them.
    #[test]
    fn two_devices_keep_separate_backup_bundles() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("package-cache");
            let spool = root.path().join("section-spool");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [11; 32];
            let bundle = |id: &'static str, writer: &'static str, marker: &'static str| {
                let cache = cache.clone();
                let spool = spool.clone();
                let provider = &provider;
                let repository = &repository;
                let root = root.path().to_path_buf();
                async move {
                    let (capture, _) =
                        captured(&root, &format!("capture-{id}"), 1, b"record", b"asset");
                    let mut snapshot_metadata = metadata(id, &capture);
                    snapshot_metadata.purpose = SnapshotPurpose::BackupBundle {
                        source: wire_control::BundleSource::Device {
                            writer_id: writer.into(),
                        },
                        remote_generation: None,
                    };
                    let mut transfer = journal(&root.join(id), id, &capture);
                    package_and_upload(
                        capture,
                        vec![section(&spool, SectionKind::LocalPlugins, marker, 0)],
                        &root,
                        &cache,
                        snapshot_metadata,
                        &key,
                        limits(256 * 1024),
                        &mut transfer,
                        provider,
                        repository,
                        &Cancellation::default(),
                    )
                    .await
                    .unwrap()
                }
            };
            let mine = bundle("bundle-1", "writer-a", "mine").await;
            let theirs = bundle("bundle-2", "writer-b", "theirs").await;
            assert_eq!(mine.sections.keys().collect::<Vec<_>>(), ["local-plugins"]);
            assert_eq!(theirs.sections.keys().collect::<Vec<_>>(), ["local-plugins"]);
            assert_ne!(
                mine.sections["local-plugins"].content_fingerprint,
                theirs.sections["local-plugins"].content_fingerprint
            );
            assert_ne!(mine.fingerprint, theirs.fingerprint);

            for (completed, writer, marker) in
                [(&mine, "writer-a", "mine"), (&theirs, "writer-b", "theirs")]
            {
                let document = snapshot_restore::download_snapshot(
                    &completed.reference,
                    &root.path().join(format!("read-{marker}")),
                    &key,
                    &provider,
                    &repository,
                    &Cancellation::default(),
                )
                .await
                .unwrap();
                assert_eq!(document.captured_by_device.as_deref(), Some(writer));
                let received = snapshot_restore::download_sections(
                    &completed.reference,
                    &BTreeSet::from(["local-plugins".to_owned()]),
                    &root.path().join(format!("receive-{marker}")),
                    &key,
                    &provider,
                    &repository,
                    &Cancellation::default(),
                )
                .await
                .unwrap();
                let rows = materialized_section_rows(&received[0], false);
                assert_eq!(
                    section_values(rows),
                    section_values(section_rows(SectionKind::LocalPlugins, marker))
                );
            }
        });
    }

    /// Invariant 36. A cancelled capture stops where it is and publishes
    /// nothing, so no half-written section reaches a repository.
    #[test]
    fn a_cancelled_section_capture_publishes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let cancel = Cancellation::default();
        cancel.cancel();
        let spool = root.path().join("cancelled");
        assert!(crate::external_storage::sections::capture_section(
            SectionKind::Hypa,
            &section_rows(SectionKind::Hypa, "stopped"),
            true,
            risunest_sync_wire::head::Sequence::from(1u64),
            risunest_sync_wire::head::Sequence::from(0u64),
            risunest_sync_wire::head::Sequence::from(5u64),
            &spool,
            &cancel,
        )
        .is_err());
        assert!(!spool.exists());
    }

    #[test]
    fn latest_snapshot_restores_without_ancestors_and_text_change_reuses_assets() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("package-cache");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [7; 32];
            let asset_bytes = b"synthetic asset bytes";
            let (first_capture, asset_hash) =
                captured(root.path(), "capture-1", 1, b"record one", asset_bytes);
            let first_metadata = metadata("snapshot-1", &first_capture);
            let mut first_journal = journal(&root.path().join("job-1"), "job-1", &first_capture);
            let first = package_and_upload(
                first_capture,
                Vec::new(),
                root.path(),
                &cache,
                first_metadata,
                &key,
                limits(128 * 1024),
                &mut first_journal,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let first_count = provider.state.lock().unwrap().objects.len();
            assert_eq!(first_count, 5);
            assert_ne!(first.repository_id, repository.repository_id);
            assert!(provider
                .state
                .lock()
                .unwrap()
                .objects
                .keys()
                .all(|object_id| !object_id.contains(&asset_hash)));
            let snapshot_version = provider
                .state
                .lock()
                .unwrap()
                .objects
                .get(&first.reference.receipt.locator.object)
                .unwrap()
                .1;
            assert_eq!(snapshot_version, first_count as u64);

            let (second_capture, _) =
                captured(root.path(), "capture-2", 2, b"record two", asset_bytes);
            let second_metadata = metadata("snapshot-2", &second_capture);
            let mut second_journal = journal(&root.path().join("job-2"), "job-2", &second_capture);
            let second = package_and_upload(
                second_capture,
                Vec::new(),
                root.path(),
                &cache,
                second_metadata,
                &key,
                limits(128 * 1024),
                &mut second_journal,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(
                second.asset_catalog.object_id,
                first.asset_catalog.object_id
            );
            assert_eq!(
                provider.state.lock().unwrap().objects.len() - first_count,
                3
            );

            let staging = root.path().join("restore");
            assert!(snapshot_restore::download_snapshot(
                &second.reference,
                &staging,
                &[8; 32],
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .is_err());
            let restored = snapshot_restore::download_snapshot(
                &second.reference,
                &staging,
                &key,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(restored.snapshot_id, "snapshot-2");
            assert_eq!(restored.records.len(), 1);
            assert_eq!(fs::read(&restored.records[0].path).unwrap(), b"record two");
            let asset = restored
                .objects
                .iter()
                .find(|object| object.content_hash == asset_hash)
                .unwrap();
            assert_eq!(fs::read(&asset.path).unwrap(), asset_bytes);
            let pack = second
                .referenced_objects
                .iter()
                .find(|object| object.role == ObjectRole::Pack)
                .unwrap();
            let pack_plaintext = staging.join("plaintext").join(&pack.plaintext_sha256);
            let pack_ciphertext = staging
                .join("downloads")
                .join(format!("{}.cipher", pack.ciphertext_sha256));
            fs::write(&pack_plaintext, vec![0; pack.plaintext_length as usize]).unwrap();
            fs::write(&pack_ciphertext, vec![0; pack.receipt.byte_length as usize]).unwrap();
            for record in &restored.records {
                fs::remove_file(&record.path).unwrap();
            }
            for object in &restored.objects {
                fs::remove_file(&object.path).unwrap();
            }
            let repaired = snapshot_restore::download_snapshot(
                &second.reference,
                &staging,
                &key,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(fs::read(&repaired.records[0].path).unwrap(), b"record two");
            let mut repaired_plaintext =
                crate::trust_boundary::open_regular_source(&pack_plaintext).unwrap();
            assert_eq!(
                repaired_plaintext.metadata().unwrap().len(),
                pack.plaintext_length
            );
            assert_eq!(
                hex::encode(hash_reader(&mut repaired_plaintext, pack.plaintext_length).unwrap()),
                pack.plaintext_sha256
            );
            let mut repaired_ciphertext =
                crate::trust_boundary::open_regular_source(&pack_ciphertext).unwrap();
            assert_eq!(
                repaired_ciphertext.metadata().unwrap().len(),
                pack.receipt.byte_length
            );
            assert_eq!(
                hex::encode(
                    hash_reader(&mut repaired_ciphertext, pack.receipt.byte_length).unwrap()
                ),
                pack.ciphertext_sha256
            );
        });
    }

    #[test]
    fn c_cached_asset_tree_is_rebuilt_when_a_pack_or_catalog_disappears() {
        runtime().block_on(async {
            for missing_role in [ObjectRole::Pack, ObjectRole::Catalog] {
                let root = tempfile::tempdir().unwrap();
                let cache = root.path().join("cache");
                let provider = FakeProvider::new(false);
                let repository = fake::repository();
                let key = [7; 32];
                let cancel = Cancellation::default();
                let (capture, _) = captured(root.path(), "first", 1, b"record", b"asset");
                let meta = metadata("first", &capture);
                let mut first_journal = journal(&root.path().join("first-job"), "first-job", &capture);
                let first = package_and_upload(capture, Vec::new(), root.path(), &cache, meta, &key,
                    limits(128 * 1024), &mut first_journal, &provider, &repository, &cancel).await.unwrap();
                let bytes = provider.state.lock().unwrap().objects[&first.asset_catalog.receipt.locator.object].0.clone();
                let metadata_key = derive_key(&key, &first.repository_id, "metadata").unwrap();
                let mut plaintext = Vec::new();
                wire::open_envelope(&mut std::io::Cursor::new(bytes), &mut plaintext, &metadata_key,
                    wire::MAX_METADATA_BYTES as u64).unwrap();
                let catalog = wire::CatalogDocument::decode(&plaintext, wire::MAX_METADATA_BYTES).unwrap();
                let asset_pack = &catalog.packs[0];
                let missing = if missing_role == ObjectRole::Pack {
                    asset_pack.locator.object.clone()
                } else { first.asset_catalog.receipt.locator.object.clone() };
                provider.forget(&missing);
                let retained = first.referenced_objects.iter().find(|object| {
                    object.role == ObjectRole::Pack && object.object_id != asset_pack.header.object_id
                }).unwrap();
                let (capture, _) = captured(root.path(), "second", 2, b"record", b"asset");
                let meta = metadata("second", &capture);
                let mut second_journal = journal(&root.path().join("second-job"), "second-job", &capture);
                let second = package_and_upload(capture, Vec::new(), root.path(), &cache, meta, &key,
                    limits(128 * 1024), &mut second_journal, &provider, &repository, &cancel).await.unwrap();
                let restored = snapshot_restore::download_snapshot(&second.reference, &root.path().join("restored"),
                    &key, &provider, &repository, &cancel).await.unwrap();
                assert_eq!(fs::read(&restored.records[0].path).unwrap(), b"record");
                assert!(restored.objects.iter().any(|object| fs::read(&object.path).unwrap() == b"asset"));
                assert_eq!(provider.upload_attempts(&retained.object_id), 1);
                assert!(first_journal.spool_path(&first.reference.object_id).exists());
                assert!(second_journal.spool_path(&second.reference.object_id).exists());
                if missing_role == ObjectRole::Pack {
                    assert_ne!(first.asset_catalog.object_id, second.asset_catalog.object_id);
                }
            }
        });
    }

    #[test]
    fn c_catalog_reuse_surfaces_authorization_errors_without_publishing() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [7; 32];
            let cancel = Cancellation::default();
            let (capture, _) = captured(root.path(), "first", 1, b"record", b"asset");
            let meta = metadata("first", &capture);
            let mut first_journal = journal(&root.path().join("first-job"), "first-job", &capture);
            let first = package_and_upload(capture, Vec::new(), root.path(), &cache, meta, &key,
                limits(128 * 1024), &mut first_journal, &provider, &repository, &cancel).await.unwrap();
            provider.fail_read(&first.asset_catalog.receipt.locator.object, ErrorKind::Unauthorized);
            let (capture, _) = captured(root.path(), "second", 2, b"record", b"asset");
            let meta = metadata("second", &capture);
            let mut second_journal = journal(&root.path().join("second-job"), "second-job", &capture);
            let error = package_and_upload(capture, Vec::new(), root.path(), &cache, meta, &key,
                limits(128 * 1024), &mut second_journal, &provider, &repository, &cancel).await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::Unauthorized);
            assert!(!provider.holds("snapshot-second"));
            assert!(provider.holds(&first.asset_catalog.object_id));
            assert!(first_journal.spool_path(&first.reference.object_id).exists());
        });
    }

    #[test]
    fn c_publication_rechecks_every_source_and_repairs_only_the_missing_ciphertext() {
        runtime().block_on(async {
            for role in [ObjectRole::Pack, ObjectRole::Catalog, ObjectRole::SyncState] {
                let root = tempfile::tempdir().unwrap();
                let cache = root.path().join("cache");
                let directory = root.path().join("job");
                let provider = FakeProvider::new(false);
                let repository = fake::repository();
                let key = [7; 32];
                let cancel = Cancellation::default();
                let (capture, _) = captured(root.path(), "capture", 1, b"record", b"asset");
                let meta = metadata("snapshot", &capture);
                let identity = JobIdentity {
                    job_id: "job".into(), connection_id: "connection".into(),
                    repository_id: repository.repository_id.clone(), capture_id: capture.id.clone(),
                    capture: capture.identity.clone(),
                };
                let mut journal = TransferJournal::open(&directory, identity.clone()).unwrap();
                let completed = package_and_upload(capture, vec![], root.path(), &cache, meta, &key,
                    limits(128 * 1024), &mut journal, &provider, &repository, &cancel).await.unwrap();
                let missing = std::iter::once(&completed.reference).chain(completed.referenced_objects.iter())
                    .find(|object| object.role == role).unwrap().clone();
                let ciphertext = std::fs::read(journal.spool_path(&missing.object_id)).unwrap();
                provider.forget(&missing.receipt.locator.object);
                // No in-memory receipt or protection survives reopening this journal.
                drop(journal);
                let mut journal = TransferJournal::open(&directory, identity).unwrap();
                assert_eq!(verify_publication(&completed, &cache, &mut journal, &key,
                    &provider, &repository, &cancel).await.unwrap(), PublicationReadiness::Verified);
                assert_eq!(provider.upload_attempts(&missing.object_id), 2);
                assert_eq!(provider.state.lock().unwrap().objects[&missing.receipt.locator.object].0, ciphertext);
                for object in std::iter::once(&completed.reference).chain(completed.referenced_objects.iter()) {
                    if object.object_id != missing.object_id { assert_eq!(provider.upload_attempts(&object.object_id), 1); }
                    assert!(journal.spool_path(&object.object_id).is_file());
                }
            }
        });
    }

    #[test]
    fn c_changed_opaque_locator_requires_repackaging_instead_of_publishing_stale_parents() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [7; 32];
            let cancel = Cancellation::default();
            let (capture, _) = captured(root.path(), "capture", 1, b"record", b"asset");
            let meta = metadata("snapshot", &capture);
            let mut journal = journal(&root.path().join("job"), "job", &capture);
            let completed = package_and_upload(capture, vec![], root.path(), &cache, meta, &key,
                limits(128 * 1024), &mut journal, &provider, &repository, &cancel).await.unwrap();
            let missing = completed.referenced_objects.iter().find(|object| object.role == ObjectRole::Pack).unwrap();
            let original = std::fs::read(journal.spool_path(&missing.object_id)).unwrap();
            provider.forget(&missing.receipt.locator.object);
            provider.set_upload_locator(&missing.object_id, "new-opaque-object");
            assert_eq!(verify_publication(&completed, &cache, &mut journal, &key,
                &provider, &repository, &cancel).await.unwrap(), PublicationReadiness::Repackage);
            assert_eq!(provider.state.lock().unwrap().objects["new-opaque-object"].0, original);
            assert_eq!(provider.upload_attempts(&missing.object_id), 2);
            assert_eq!(provider.upload_attempts(&completed.reference.object_id), 1);
            assert!(journal.spool_path(&completed.reference.object_id).exists());

            // The coordinator prepares a new immutable publication identity while
            // the shared capture and repaired inventory remain available.
            let (capture, _) = captured(root.path(), "replacement-capture", 2, b"record", b"asset");
            let meta = metadata("replacement-snapshot", &capture);
            let mut replacement_journal = self::journal(&root.path().join("replacement-job"), "replacement-job", &capture);
            let replacement = package_and_upload(capture, vec![], root.path(), &cache, meta, &key,
                limits(128 * 1024), &mut replacement_journal, &provider, &repository, &cancel).await.unwrap();
            assert_eq!(provider.upload_attempts(&missing.object_id), 2);
            for object in replacement.referenced_objects.iter().filter(|object| object.role == ObjectRole::Pack) {
                assert!(completed.referenced_objects.iter().any(|previous| previous.object_id == object.object_id));
            }
            assert_eq!(verify_publication(&replacement, &cache, &mut replacement_journal, &key,
                &provider, &repository, &cancel).await.unwrap(), PublicationReadiness::Verified);
            let restored = snapshot_restore::download_snapshot(&replacement.reference, &root.path().join("restored"),
                &key, &provider, &repository, &cancel).await.unwrap();
            assert_eq!(std::fs::read(&restored.records[0].path).unwrap(), b"record");
            assert!(restored.objects.iter().any(|object| std::fs::read(&object.path).unwrap() == b"asset"));
            assert!(journal.spool_path(&completed.reference.object_id).exists());
        });
    }

    #[test]
    fn c_actual_catalog_and_envelope_lengths_enforce_exact_boundaries() {
        let kind = wire::CatalogKind::Assets;
        let leaf = wire::CatalogDocument::leaf(kind, Vec::new(), Vec::new()).unwrap();
        let bytes = leaf.encode(wire::MAX_METADATA_BYTES).unwrap();
        let catalog_id = format!("catalog-{}", "0".repeat(64));
        let header = wire::PublicObjectHeader::new(
            "repository".into(), catalog_id, wire::ObjectRole::Catalog, bytes.len() as u64,
        ).unwrap();
        let exact = wire::envelope_length(&header).unwrap() + 137;
        for (maximum, fits) in [(exact - 1, false), (exact, true), (exact + 1, true)] {
            let size = StoredSize {
                limits: PackageLimits { sdk_overhead_bytes: 137, ..limits(maximum) },
                repository_id: "repository",
            };
            let encoded = measure_catalog(leaf.clone(), &size).unwrap();
            assert_eq!(encoded.is_some(), fits);
            if let Some(encoded) = encoded { assert_eq!(encoded.bytes, bytes); }
        }
        for maximum in [8191, 8192, 8193] {
            let size = StoredSize {
                limits: PackageLimits { sdk_overhead_bytes: 137, ..limits(maximum) },
                repository_id: "repository",
            };
            let plain = size.pack_capacity("pack-object").unwrap();
            assert!(size.require("pack-object", wire::ObjectRole::Pack, plain).is_ok());
            assert_eq!(size.require("pack-object", wire::ObjectRole::Pack, plain + 1).unwrap_err().kind, ErrorKind::FileTooLarge);
        }
        let mut capabilities = fake::capabilities(false);
        capabilities.max_stored_bytes = Some(u64::MAX);
        capabilities.sdk_overhead_bytes = u64::MAX;
        assert_eq!(PackageLimits::from_capabilities(&capabilities).unwrap_err().kind, ErrorKind::FileTooLarge);
    }

    #[test]
    fn c_invalid_catalog_is_not_misclassified_as_an_oversized_leaf() {
        let mut document = wire::CatalogDocument::leaf(wire::CatalogKind::Assets, vec![], vec![]).unwrap();
        document.schema = "invalid".into();
        let size = StoredSize { limits: limits(1024 * 1024), repository_id: "repository" };
        assert!(matches!(measure_catalog(document, &size), Err(error) if error.kind == ErrorKind::Corrupt));
        let empty = catalog_batches::<wire::CatalogEntryFragment>(&[], &size, |entries| {
            wire::CatalogDocument::leaf(wire::CatalogKind::Assets, entries.to_vec(), vec![]).map_err(corrupt)
        }).unwrap();
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].bytes, empty[0].document.encode(wire::MAX_METADATA_BYTES).unwrap());
    }

    #[test]
    fn stable_job_object_id_rejects_changed_plaintext_while_retaining_completed_spool() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let (capture, _) = captured(root.path(), "stable-capture", 1, b"record", b"asset");
            let mut journal = journal(&root.path().join("stable-job"), "stable-job", &capture);
            let mut cache = PackageCache::open(&root.path().join("stable-cache")).unwrap();
            let build = root.path().join("stable-build");
            fs::create_dir(&build).unwrap();
            let first_path = build.join("first");
            let second_path = build.join("second");
            fs::write(&first_path, b"first immutable snapshot").unwrap();
            fs::write(&second_path, b"other immutable snapshot").unwrap();
            let key = derive_key(&[31; 32], "format-repository", "metadata").unwrap();
            let first = upload_plain_object(
                &first_path,
                24,
                hash(b"first immutable snapshot"),
                "snapshot-stable".into(),
                ObjectRole::SyncState,
                "format-repository",
                &key,
                limits(128 * 1024),
                &mut cache,
                &mut journal,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert!(journal.spool_path(&first.object_id).exists());
            let before = provider.state.lock().unwrap().objects.len();
            assert!(upload_plain_object(
                &second_path,
                24,
                hash(b"other immutable snapshot"),
                "snapshot-stable".into(),
                ObjectRole::SyncState,
                "format-repository",
                &key,
                limits(128 * 1024),
                &mut cache,
                &mut journal,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .is_err());
            assert_eq!(provider.state.lock().unwrap().objects.len(), before);
        });
    }

    #[test]
    fn small_provider_limit_splits_large_content_and_every_ciphertext_fits() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let mut state = 0x9e37_79b9u32;
            let record: Vec<u8> = (0..40_000)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state as u8
                })
                .collect();
            let (capture, _) = captured(root.path(), "small-limit", 1, &record, b"asset");
            let snapshot_metadata = metadata("bounded", &capture);
            let mut journal = journal(&root.path().join("job"), "job", &capture);
            let completed = package_and_upload(
                capture,
                Vec::new(),
                root.path(),
                &root.path().join("cache"),
                snapshot_metadata,
                &[4; 32],
                limits(8 * 1024),
                &mut journal,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let state = provider.state.lock().unwrap();
            assert!(state
                .objects
                .values()
                .all(|(bytes, _)| bytes.len() <= 8 * 1024));
            assert!(
                state
                    .objects
                    .keys()
                    .filter(|id| id.starts_with("pack-"))
                    .count()
                    >= 6
            );
            assert!(state
                .objects
                .contains_key(&completed.reference.receipt.locator.object));
            drop(state);
            let restored = snapshot_restore::download_snapshot(
                &completed.reference,
                &root.path().join("bounded-restore"),
                &[4; 32],
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(fs::read(&restored.records[0].path).unwrap(), record);
        });
    }

    #[test]
    fn provider_limit_rejects_a_catalog_entry_that_cannot_fit_alone() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let directory = root
                .path()
                .join("external-storage")
                .join("captures")
                .join("oversized-entry");
            let objects = root.path().join("external-storage").join("objects");
            let mut catalog = crate::external_storage::capture::CaptureCatalog::create(
                &directory, &objects, None,
            )
            .unwrap();
            let identity = CaptureIdentity {
                store_id: "store".into(),
                library_epoch: "epoch".into(),
                generation: "generation".into(),
                selection_epoch: "selection".into(),
                revision: 1,
            };
            catalog.begin(&identity, None).unwrap();
            catalog.record(&"k".repeat(16 * 1024), b"record").unwrap();
            catalog.finish().unwrap();
            let capture = CapturedSnapshot {
                id: "oversized-entry".into(),
                identity,
                catalog,
                projected_records: 1,
                shared: false,
            };
            let snapshot_metadata = metadata("oversized-entry", &capture);
            let mut journal = journal(&root.path().join("job"), "job", &capture);
            let error = package_and_upload(
                capture,
                Vec::new(),
                root.path(),
                &root.path().join("cache"),
                snapshot_metadata,
                &[4; 32],
                limits(8 * 1024),
                &mut journal,
                &FakeProvider::new(false),
                &fake::repository(),
                &Cancellation::default(),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind, ErrorKind::FileTooLarge);
        });
    }

    #[test]
    #[ignore = "explicit large-inventory measurement"]
    fn measures_hundred_thousand_asset_package_and_restore() {
        runtime().block_on(async {
            let started = std::time::Instant::now();
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [27; 32];
            let first_capture =
                captured_asset_inventory(root.path(), "inventory-1", 1, 300, 0, 100_000);
            let first_metadata = metadata("inventory-snapshot-1", &first_capture);
            let mut first_journal = journal(
                &root.path().join("inventory-job-1"),
                "inventory-job-1",
                &first_capture,
            );
            let completed = package_and_upload(
                first_capture,
                Vec::new(),
                root.path(),
                &root.path().join("inventory-cache"),
                first_metadata,
                &key,
                limits(512 * 1024),
                &mut first_journal,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let package_elapsed = started.elapsed();
            let (remote_objects, packs, catalogs) = {
                let state = provider.state.lock().unwrap();
                (
                    state.objects.len(),
                    state
                        .objects
                        .keys()
                        .filter(|id| id.starts_with("pack-"))
                        .count(),
                    state
                        .objects
                        .keys()
                        .filter(|id| id.starts_with("catalog-"))
                        .count(),
                )
            };
            let restore_started = std::time::Instant::now();
            let restore_root = root.path().join("inventory-restore");
            snapshot_restore::reset_test_read_counts(&restore_root);
            let restored = snapshot_restore::download_snapshot(
                &completed.reference,
                &restore_root,
                &key,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let restore_elapsed = restore_started.elapsed();
            let (network_reads, pack_reads) =
                snapshot_restore::take_test_read_counts(&restore_root);
            assert_eq!(restored.records.len(), 300);
            assert_eq!(restored.objects.len(), 100_000);
            assert!(provider
                .state
                .lock()
                .unwrap()
                .objects
                .values()
                .all(|(bytes, _)| bytes.len() <= 512 * 1024));
            eprintln!(
                "assets=100000 remote_objects={} packs={} catalogs={} package_ms={} restore_reads={} restore_pack_reads={} restore_ms={}",
                remote_objects,
                packs,
                catalogs,
                package_elapsed.as_millis(),
                network_reads,
                pack_reads,
                restore_elapsed.as_millis(),
            );
        });
    }

    #[test]
    #[ignore = "explicit publication-history measurement"]
    fn measures_three_hundred_small_publications_and_latest_restore() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [29; 32];
            let cache = root.path().join("publication-cache");
            let started = std::time::Instant::now();
            let mut asset_catalog_id = None;
            let mut latest = None;
            for revision in 1..=300i64 {
                let capture_id = format!("publication-capture-{revision}");
                let snapshot_id = format!("publication-snapshot-{revision}");
                let job_id = format!("publication-job-{revision}");
                let capture = captured_asset_inventory(
                    root.path(),
                    &capture_id,
                    revision,
                    1,
                    1,
                    1_000,
                );
                let snapshot_metadata = metadata(&snapshot_id, &capture);
                let mut transfer = journal(&root.path().join(&job_id), &job_id, &capture);
                let before = provider.state.lock().unwrap().objects.len();
                let completed = package_and_upload(
                    capture,
                    Vec::new(),
                    root.path(),
                    &cache,
                    snapshot_metadata,
                    &key,
                    limits(128 * 1024),
                    &mut transfer,
                    &provider,
                    &repository,
                    &Cancellation::default(),
                )
                .await
                .unwrap();
                if let Some(expected) = &asset_catalog_id {
                    assert_eq!(&completed.asset_catalog.object_id, expected);
                    assert_eq!(provider.state.lock().unwrap().objects.len() - before, 3);
                } else {
                    asset_catalog_id = Some(completed.asset_catalog.object_id.clone());
                }
                latest = Some(completed);
            }
            let publication_elapsed = started.elapsed();
            let physical_remote_objects = provider.state.lock().unwrap().objects.len();
            let restore_root = root.path().join("publication-restore");
            snapshot_restore::reset_test_read_counts(&restore_root);
            let restore_started = std::time::Instant::now();
            let latest = latest.unwrap();
            let restored = snapshot_restore::download_snapshot(
                &latest.reference,
                &restore_root,
                &key,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let restore_elapsed = restore_started.elapsed();
            let (network_reads, pack_reads) =
                snapshot_restore::take_test_read_counts(&restore_root);
            assert_eq!(restored.records.len(), 1);
            assert_eq!(restored.objects.len(), 1_000);
            assert!(network_reads < 50);
            assert!(pack_reads < 20);
            eprintln!(
                "assets=1000 publications=300 physical_remote_objects={} publication_ms={} latest_restore_reads={} latest_reachable_packs={} latest_restore_ms={}",
                physical_remote_objects,
                publication_elapsed.as_millis(),
                network_reads,
                pack_reads,
                restore_elapsed.as_millis(),
            );
        });
    }
}
