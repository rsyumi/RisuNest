//! Streams a completed PDS capture into bounded encrypted immutable objects.
//! This layer starts only after the PDS read snapshot has closed.
use super::{
    capabilities::Capabilities,
    contract::{
        Cancellation, ErrorKind, ObjectIntent, ObjectReceipt, ObjectRole, Provider, ProviderError,
        ReadReceipt, RemoteLocator, RepositoryHandle, Result,
    },
    journal::{validate_receipt, TransferJournal},
    transfer::SpoolSink,
    transfer_job,
};
use crate::{
    asset_repository::PayloadCas, external_storage::device_capture::DeviceSnapshot,
    persistent_store::external_capture::CapturedSnapshot,
};
use risunest_external_storage_format::{
    content_identity::{hash, hash_reader},
    crypto::derive_key,
    pack::{self, Chunk, ChunkEncoder, CompressionPolicy, ENTRY_OVERHEAD, MAX_CHUNK_BYTES},
    snapshot as wire,
};
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
        if max_stored_bytes <= capabilities.sdk_overhead_bytes + MIN_OBJECT_BYTES {
            return Err(ProviderError::new(ErrorKind::FileTooLarge));
        }
        Ok(Self {
            max_stored_bytes,
            sdk_overhead_bytes: capabilities.sdk_overhead_bytes,
            target_plaintext_bytes: DEFAULT_TARGET_BYTES,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SnapshotMetadata {
    pub snapshot_id: String,
    pub repository_id: String,
    pub library_id: String,
    pub author_device_id: String,
    pub created_at_ms: u64,
    pub logical_revision: u64,
    pub scope_id: [u8; 32],
    pub library_scope_id: [u8; 32],
    pub parent_snapshot_id: Option<String>,
    pub content_fingerprint: [u8; 32],
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
    pub scope_id: String,
    pub library_scope_id: String,
    pub fingerprint: String,
    pub library_fingerprint: String,
    pub logical_revision: u64,
    pub record_catalog: RemoteObject,
    pub asset_catalog: RemoteObject,
    pub device_catalog: Option<RemoteObject>,
    pub device_identity: Option<String>,
    pub device_sections: Vec<String>,
    pub referenced_objects: Vec<RemoteObject>,
}

fn wire_role(role: ObjectRole) -> Result<wire::ObjectRole> {
    match role {
        ObjectRole::Pack => Ok(wire::ObjectRole::Pack),
        ObjectRole::Catalog => Ok(wire::ObjectRole::Catalog),
        ObjectRole::Snapshot => Ok(wire::ObjectRole::Snapshot),
        ObjectRole::BackupPoint => Ok(wire::ObjectRole::BackupPoint),
        ObjectRole::Descriptor => Ok(wire::ObjectRole::Descriptor),
    }
}
fn native_role(role: wire::ObjectRole) -> Result<ObjectRole> {
    match role {
        wire::ObjectRole::Pack => Ok(ObjectRole::Pack),
        wire::ObjectRole::Catalog => Ok(ObjectRole::Catalog),
        wire::ObjectRole::Snapshot => Ok(ObjectRole::Snapshot),
        wire::ObjectRole::BackupPoint => Ok(ObjectRole::BackupPoint),
        wire::ObjectRole::Descriptor => Ok(ObjectRole::Descriptor),
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
        if value.repository_id != format_repository_id {
            return Err(corrupt("cached repository identity"));
        }
        Ok(Some(value))
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
fn kind_name(kind: wire::CatalogKind) -> &'static str {
    match kind {
        wire::CatalogKind::Records => "records",
        wire::CatalogKind::Assets => "assets",
        wire::CatalogKind::Device => "device",
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

fn max_plaintext(
    limits: PackageLimits,
    repository_id: &str,
    object_id: &str,
    role: wire::ObjectRole,
) -> Result<u64> {
    let available = limits
        .max_stored_bytes
        .checked_sub(limits.sdk_overhead_bytes)
        .ok_or_else(|| ProviderError::new(ErrorKind::FileTooLarge))?;
    let mut low = 0u64;
    let mut high = available;
    while low < high {
        let middle = low + (high - low + 1) / 2;
        let header =
            wire::PublicObjectHeader::new(repository_id.into(), object_id.into(), role, middle)
                .map_err(corrupt)?;
        if wire::envelope_length(&header).map_err(corrupt)? <= available {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    if low < 128 {
        Err(ProviderError::new(ErrorKind::FileTooLarge))
    } else {
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

fn device_sources(device: DeviceSnapshot) -> Result<(Vec<SourceEntry>, [u8; 32], Vec<String>)> {
    let device = super::device_capture::verify_snapshot(device).map_err(corrupt)?;
    let mut sections = device.sections;
    sections.sort();
    if sections.is_empty()
        || sections.iter().any(|section| section.is_empty())
        || sections.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(corrupt("invalid device section inventory"));
    }
    let mut sources = vec![SourceEntry {
        kind: wire::CatalogEntryKind::DeviceCatalog,
        key: "device/catalog".into(),
        content_sha256: hex::encode(device.sqlite.content_hash),
        byte_length: device.sqlite.byte_length,
        path: device.sqlite.path,
        compression: CompressionPolicy::Text,
    }];
    for file in device.blobs {
        sources.push(SourceEntry {
            kind: wire::CatalogEntryKind::DeviceObject,
            key: file.section_id,
            content_sha256: hex::encode(file.content_hash),
            byte_length: file.byte_length,
            path: file.path,
            compression: CompressionPolicy::AlreadyCompressed,
        });
    }
    sources.sort_by(|a, b| a.key.cmp(&b.key));
    if sources.windows(2).any(|pair| pair[0].key == pair[1].key) {
        return Err(corrupt("duplicate device file key"));
    }
    Ok((sources, device.device_identity, sections))
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
    let max_pack = max_plaintext(
        limits,
        format_repository_id,
        &format!("pack-{}", "0".repeat(64)),
        wire::ObjectRole::Pack,
    )?;
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
        if let Some(cached) = cache.entry(
            format_repository_id,
            repository,
            kind,
            &source.key,
            &source.content_sha256,
            source.byte_length,
        )? {
            ready.push(cached);
            continue;
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
    Ok((ready, uploaded_packs))
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
        return Ok(value);
    }
    let wire_role = wire_role(role)?;
    let header = wire::PublicObjectHeader::new(
        format_repository_id.into(),
        object_id.clone(),
        wire_role,
        plaintext_length,
    )
    .map_err(corrupt)?;
    let ciphertext_length = wire::envelope_length(&header).map_err(corrupt)?;
    if ciphertext_length
        .checked_add(limits.sdk_overhead_bytes)
        .is_none_or(|length| length > limits.max_stored_bytes)
    {
        return Err(ProviderError::new(ErrorKind::FileTooLarge));
    }
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
            if !matches!(read, ReadReceipt::Body(body) if body.complete && body.byte_length == record.intent.byte_length)
                || !sink.is_verified()
            {
                return Err(corrupt(
                    "completed journal object could not be reconstructed",
                ));
            }
            downloaded_path
        } else {
            return Err(corrupt("journal ciphertext is missing"));
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
        remove_completed_spool(journal, &value.object_id)?;
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
    remove_completed_spool(journal, &value.object_id)?;
    Ok(value)
}

fn remove_completed_spool(journal: &TransferJournal, object_id: &str) -> Result<()> {
    let path = journal.spool_path(object_id);
    match fs::remove_file(&path) {
        Ok(()) => crate::trust_boundary::sync_directory(path.parent().unwrap())
            .map_err(transient)
            .map(|_| ()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(transient(error)),
    }
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

const TARGET_CATALOG_LEAF_FRAGMENTS: usize = 512;

fn catalog_leaf_fits(
    kind: wire::CatalogKind,
    fragments: &[wire::CatalogEntryFragment],
    available_packs: &BTreeMap<String, wire::StoredObject>,
    max_plain: usize,
) -> Result<bool> {
    let packs = unique_packs(fragments, available_packs)?;
    Ok(wire::CatalogDocument::leaf(kind, fragments.to_vec(), packs)
        .and_then(|document| document.encode(max_plain))
        .is_ok())
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
        referenced.push(root.clone());
        return Ok(root);
    }
    let max_plain = usize::try_from(
        max_plaintext(
            limits,
            format_repository_id,
            &format!("catalog-{}", "0".repeat(64)),
            wire::ObjectRole::Catalog,
        )?
        .min(wire::MAX_METADATA_BYTES as u64),
    )
    .map_err(corrupt)?;
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
        let mut parts: Vec<Vec<wire::StoredChunk>> = Vec::new();
        for chunk in &entry.chunks {
            if parts.is_empty() {
                parts.push(Vec::new());
            }
            let last = parts.last_mut().unwrap();
            last.push(chunk.clone());
            let probe = wire::CatalogEntryFragment {
                kind: entry.kind,
                key: entry.key.clone(),
                content_sha256: decode_hash(&entry.content_sha256)?,
                byte_length: entry.byte_length,
                fragment_index: 0,
                fragment_count: 1,
                chunks: last.clone(),
            };
            let packs = unique_packs(std::slice::from_ref(&probe), &available_packs)?;
            if wire::CatalogDocument::leaf(kind, vec![probe], packs)
                .and_then(|doc| doc.encode(max_plain))
                .is_err()
            {
                let chunk = last.pop().unwrap();
                if last.is_empty() {
                    return Err(ProviderError::new(ErrorKind::FileTooLarge));
                }
                parts.push(vec![chunk]);
            }
        }
        let count = u32::try_from(parts.len()).map_err(corrupt)?;
        for (index, chunks) in parts.into_iter().enumerate() {
            fragments.push(wire::CatalogEntryFragment {
                kind: entry.kind,
                key: entry.key.clone(),
                content_sha256: decode_hash(&entry.content_sha256)?,
                byte_length: entry.byte_length,
                fragment_index: index as u32,
                fragment_count: count,
                chunks,
            });
        }
    }
    let mut leaves = Vec::new();
    let mut start = 0usize;
    while start < fragments.len() {
        let candidate_end = fragments
            .len()
            .min(start.saturating_add(TARGET_CATALOG_LEAF_FRAGMENTS));
        let end = if catalog_leaf_fits(
            kind,
            &fragments[start..candidate_end],
            &available_packs,
            max_plain,
        )? {
            candidate_end
        } else {
            let mut fits_end = start;
            let mut too_large_end = candidate_end;
            while fits_end + 1 < too_large_end {
                let probe_end = fits_end + (too_large_end - fits_end) / 2;
                if catalog_leaf_fits(
                    kind,
                    &fragments[start..probe_end],
                    &available_packs,
                    max_plain,
                )? {
                    fits_end = probe_end;
                } else {
                    too_large_end = probe_end;
                }
            }
            if fits_end == start {
                return Err(ProviderError::new(ErrorKind::FileTooLarge));
            }
            fits_end
        };
        leaves.push(fragments[start..end].to_vec());
        start = end;
    }
    if leaves.is_empty() {
        leaves.push(Vec::new());
    }
    let mut nodes = Vec::new();
    for leaf in leaves {
        let packs = unique_packs(&leaf, &available_packs)?;
        let document = wire::CatalogDocument::leaf(kind, leaf, packs).map_err(corrupt)?;
        let bytes = document.encode(max_plain).map_err(corrupt)?;
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
        let mut batch = Vec::new();
        for child in nodes {
            batch.push(child);
            if wire::CatalogDocument::branch(kind, level, batch.clone())
                .and_then(|doc| doc.encode(max_plain))
                .is_err()
            {
                let tail = batch.pop().unwrap();
                if batch.is_empty() {
                    return Err(ProviderError::new(ErrorKind::FileTooLarge));
                }
                let document =
                    wire::CatalogDocument::branch(kind, level, std::mem::take(&mut batch))
                        .map_err(corrupt)?;
                let bytes = document.encode(max_plain).map_err(corrupt)?;
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
                batch.push(tail);
            }
        }
        if !batch.is_empty() {
            let document = wire::CatalogDocument::branch(kind, level, batch).map_err(corrupt)?;
            let bytes = document.encode(max_plain).map_err(corrupt)?;
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

pub(crate) async fn package_and_upload(
    capture: CapturedSnapshot,
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
    package_and_upload_with_device(
        capture,
        None,
        repository_root,
        cache_root,
        metadata,
        root_key,
        limits,
        journal,
        provider,
        repository,
        cancel,
    )
    .await
}

pub(crate) async fn package_and_upload_with_device(
    capture: CapturedSnapshot,
    device: Option<DeviceSnapshot>,
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
    let library_scope_id = metadata.library_scope_id;
    let cpu = cpu_permit().await?;
    let (record_sources, asset_sources, device_parts, observed_fingerprint) =
        tokio::task::spawn_blocking(move || {
            let (records, assets) = capture_sources(&capture, &repository_root_owned)?;
            let fingerprint = capture
                .catalog
                .content_fingerprint(&library_scope_id)
                .map_err(corrupt)?;
            Ok((
                records,
                assets,
                device.map(device_sources).transpose()?,
                fingerprint,
            ))
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
    let asset_catalog = if let Some(root) = cache.catalog(
        &metadata.repository_id,
        repository,
        wire::CatalogKind::Assets,
        &asset_fingerprint,
    )? {
        referenced.push(root.clone());
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
    let (device_catalog, device_identity, device_sections) =
        if let Some((device_sources, identity, sections)) = device_parts {
            let mut device_fingerprint =
                catalog_fingerprint(wire::CatalogKind::Device, &device_sources);
            device_fingerprint.push_str(&hex::encode(identity));
            for section in &sections {
                device_fingerprint.push_str(section);
            }
            let (device_entries, uploaded) = build_entries(
                wire::CatalogKind::Device,
                device_sources,
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
            let catalog = build_catalog(
                wire::CatalogKind::Device,
                device_entries,
                &device_fingerprint,
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
            (Some(catalog), Some(identity), sections)
        } else {
            (None, None, Vec::new())
        };
    let document = wire::SnapshotDocument::new(
        metadata.snapshot_id.clone(),
        metadata.repository_id.clone(),
        metadata.library_id,
        metadata.author_device_id,
        metadata.created_at_ms,
        metadata.logical_revision,
        metadata.scope_id,
        metadata.library_scope_id,
        metadata.parent_snapshot_id,
        wire::combine_content_fingerprint(
            &metadata.scope_id,
            &metadata.content_fingerprint,
            device_identity.as_ref(),
        ),
        metadata.content_fingerprint,
        record_catalog.stored(repository)?,
        asset_catalog.stored(repository)?,
        device_catalog
            .as_ref()
            .map(|catalog| catalog.stored(repository))
            .transpose()?,
        device_identity,
        device_sections.clone(),
    )
    .map_err(corrupt)?;
    let max_snapshot = usize::try_from(
        max_plaintext(
            limits,
            &metadata.repository_id,
            &format!("snapshot-{}", metadata.snapshot_id),
            wire::ObjectRole::Snapshot,
        )?
        .min(wire::MAX_METADATA_BYTES as u64),
    )
    .map_err(corrupt)?;
    let bytes = document.encode(max_snapshot).map_err(corrupt)?;
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
        ObjectRole::Snapshot,
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
        scope_id: hex::encode(metadata.scope_id),
        library_scope_id: hex::encode(metadata.library_scope_id),
        fingerprint: hex::encode(wire::combine_content_fingerprint(
            &metadata.scope_id,
            &metadata.content_fingerprint,
            device_identity.as_ref(),
        )),
        library_fingerprint: hex::encode(metadata.content_fingerprint),
        logical_revision: metadata.logical_revision,
        record_catalog,
        asset_catalog,
        device_catalog,
        device_identity: device_identity.map(hex::encode),
        device_sections,
        referenced_objects: referenced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        external_storage::{
            device_capture::{self, DeviceFile},
            fake::{self, FakeProvider},
            journal::JobIdentity,
            snapshot_restore,
        },
        persistent_store::{content_capture::ContentCaptureSink, sync_selection::CaptureIdentity},
    };

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
            scope_id: [9; 32],
            library_scope_id: [9; 32],
            parent_snapshot_id: None,
            content_fingerprint: capture.catalog.content_fingerprint(&[9; 32]).unwrap(),
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

    fn synthetic_device_snapshot(root: &Path) -> DeviceSnapshot {
        let directory = root.join("synthetic-device");
        fs::create_dir(&directory).unwrap();
        let blob_bytes = b"synthetic safe plugin binary";
        let blob_hash = hash(blob_bytes);
        let blob_path = directory.join(hex::encode(blob_hash));
        fs::write(&blob_path, blob_bytes).unwrap();
        // This neighboring value models an excluded credential source. It is
        // deliberately absent from the immutable device catalog and inventory.
        fs::write(
            directory.join("excluded-secret"),
            b"never-package-this-token",
        )
        .unwrap();
        let catalog_path = directory.join("device.sqlite");
        let db = Connection::open(&catalog_path).unwrap();
        db.execute_batch(
            "CREATE TABLE device_sections(section TEXT PRIMARY KEY,schema_version INTEGER NOT NULL,included INTEGER NOT NULL,complete INTEGER NOT NULL,present INTEGER NOT NULL,record_count INTEGER NOT NULL,sha256 TEXT NOT NULL);
             CREATE TABLE device_records(section TEXT NOT NULL,ordinal INTEGER NOT NULL,metadata TEXT NOT NULL,PRIMARY KEY(section,ordinal));
             CREATE TABLE device_objects(sha256 TEXT PRIMARY KEY,byte_length INTEGER NOT NULL);",
        )
        .unwrap();
        let section = "local-storage";
        let metadata = r#"{"present":true,"profile":"risunest.device-section/v1","sectionId":"local-storage"}"#;
        let row = format!(
            r#"{{"kind":"value","key":"0073006100660065005f0070006c007500670069006e005f006b00650079","value":{{"version":1,"root":0,"nodes":[{{"type":"blob","reference":"{}","byteLength":{}}}]}}}}"#,
            hex::encode(blob_hash),
            blob_bytes.len()
        );
        let mut section_digest = Sha256::new();
        section_digest.update(b"RisuNest-device-section-v1\0");
        section_digest.update((metadata.len() as u64).to_le_bytes());
        section_digest.update(metadata.as_bytes());
        section_digest.update(0u64.to_le_bytes());
        section_digest.update((row.len() as u64).to_le_bytes());
        section_digest.update(row.as_bytes());
        let section_hash: [u8; 32] = section_digest.finalize().into();
        db.execute(
            "INSERT INTO device_sections VALUES(?1,1,1,1,1,1,?2)",
            params![section, hex::encode(section_hash)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO device_records VALUES(?1,-1,?2)",
            params![section, metadata],
        )
        .unwrap();
        db.execute(
            "INSERT INTO device_records VALUES(?1,0,?2)",
            params![section, row],
        )
        .unwrap();
        db.execute(
            "INSERT INTO device_objects VALUES(?1,?2)",
            params![hex::encode(blob_hash), blob_bytes.len() as i64],
        )
        .unwrap();
        drop(db);
        let mut catalog = crate::trust_boundary::open_regular_source(&catalog_path).unwrap();
        let catalog_length = catalog.metadata().unwrap().len();
        let catalog_hash = hash_reader(&mut catalog, catalog_length).unwrap();
        let mut identity = Sha256::new();
        identity.update(b"RisuNest-external-device-capture-v1\0");
        identity.update((section.len() as u64).to_le_bytes());
        identity.update(section.as_bytes());
        identity.update(section_hash);
        identity.update(blob_hash);
        identity.update((blob_bytes.len() as u64).to_le_bytes());
        let device_identity: [u8; 32] = identity.finalize().into();
        device_capture::verify_snapshot(DeviceSnapshot {
            capture_id: hex::encode(device_identity),
            device_identity,
            sqlite: DeviceFile {
                section_id: "device/catalog".into(),
                content_hash: catalog_hash,
                byte_length: catalog_length,
                path: catalog_path,
            },
            blobs: vec![DeviceFile {
                section_id: format!("device/object/{}", hex::encode(blob_hash)),
                content_hash: blob_hash,
                byte_length: blob_bytes.len() as u64,
                path: blob_path,
            }],
            sections: vec![section.into()],
        })
        .unwrap()
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
    fn device_catalog_and_objects_round_trip_without_excluded_sources() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [19; 32];
            let device = synthetic_device_snapshot(root.path());
            let expected_device_identity = device.device_identity;
            let (capture, _) = captured(
                root.path(),
                "capture-device",
                11,
                b"record with device backup",
                b"library asset",
            );
            let snapshot_metadata = metadata("snapshot-device", &capture);
            let expected_library_fingerprint = snapshot_metadata.content_fingerprint;
            let mut journal = journal(&root.path().join("job-device"), "job-device", &capture);
            let completed = package_and_upload_with_device(
                capture,
                Some(device),
                root.path(),
                &root.path().join("package-cache"),
                snapshot_metadata,
                &key,
                limits(128 * 1024),
                &mut journal,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(
                completed.device_identity.as_deref(),
                Some(hex::encode(expected_device_identity).as_str())
            );
            assert_eq!(completed.device_sections, vec!["local-storage"]);
            assert_eq!(
                completed.fingerprint,
                hex::encode(wire::combine_content_fingerprint(
                    &[9; 32],
                    &expected_library_fingerprint,
                    Some(&expected_device_identity),
                ))
            );
            let restored = snapshot_restore::download_snapshot(
                &completed.reference,
                &root.path().join("restore-device"),
                &key,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let restored_device = restored.device.unwrap();
            assert_eq!(restored_device.device_identity, expected_device_identity);
            assert_eq!(restored_device.sections, vec!["local-storage"]);
            assert_eq!(restored.device_sections, restored_device.sections);
            assert!(!fs::read(restored_device.sqlite.path)
                .unwrap()
                .windows(b"never-package-this-token".len())
                .any(|window| window == b"never-package-this-token"));
        });
    }

    #[test]
    fn stable_job_object_id_rejects_changed_plaintext_after_completed_spool_cleanup() {
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
                ObjectRole::Snapshot,
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
            assert!(!journal.spool_path(&first.object_id).exists());
            let before = provider.state.lock().unwrap().objects.len();
            assert!(upload_plain_object(
                &second_path,
                24,
                hash(b"other immutable snapshot"),
                "snapshot-stable".into(),
                ObjectRole::Snapshot,
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
            snapshot_restore::reset_test_read_counts();
            let restored = snapshot_restore::download_snapshot(
                &completed.reference,
                &root.path().join("inventory-restore"),
                &key,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let restore_elapsed = restore_started.elapsed();
            let (network_reads, pack_reads) = snapshot_restore::test_read_counts();
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
            snapshot_restore::reset_test_read_counts();
            let restore_started = std::time::Instant::now();
            let latest = latest.unwrap();
            let restored = snapshot_restore::download_snapshot(
                &latest.reference,
                &root.path().join("publication-restore"),
                &key,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let restore_elapsed = restore_started.elapsed();
            let (network_reads, pack_reads) = snapshot_restore::test_read_counts();
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
