//! Incremental canonical projection, independent of server wire/view policy.
//! The caller hydrates remote-only owner manifests before preparation, and owns
//! file(true) until the sink and its durable capture reference have been
//! finalized. A payload is sized from its local body or its custody record. No
//! network I/O is performed through this snapshot or while holding the live PDS
//! connection.
use super::{
    content_change_index::{self, ChangeWindow, ContentKey},
    record_projection::{codec_error, missing_source},
    sync_selection::{self, CaptureIdentity},
    PersistentStore, RevisionReadLease, StoreError, StoreResult,
};
use crate::{
    asset_repository::{owner_manifest_codec::decode_owner_manifest, PayloadCas},
    local_backup::CancellationProbe,
    logical_records::*,
};
use rusqlite::{params, Connection};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// The size a capture records for a payload: its local body's when the body is
/// held, and otherwise what its active custody states. Projection never reads
/// a payload, so a custody size is all it needs.
struct PayloadSizes {
    cas: PayloadCas,
    residency: Option<crate::server_sync::residency::Residency>,
}

impl PayloadSizes {
    fn open(repository_root: &Path) -> StoreResult<Self> {
        let residency = if crate::server_sync::residency::Residency::exists(repository_root) {
            Some(
                crate::server_sync::residency::Residency::open(repository_root)
                    .map_err(custody_error)?,
            )
        } else {
            None
        };
        Ok(Self {
            cas: PayloadCas::new(repository_root)?,
            residency,
        })
    }

    fn size(&self, hash: &str) -> StoreResult<Option<u64>> {
        if let Some(size) = self.cas.stat_object(hash)? {
            return Ok(Some(size));
        }
        let Some(residency) = &self.residency else {
            return Ok(None);
        };
        Ok(residency
            .object(hash, None)
            .map_err(custody_error)?
            .map(|object| object.size))
    }
}

fn custody_error(error: crate::server_sync::SyncError) -> StoreError {
    StoreError::Validation {
        message: format!("Payload custody is unavailable: {}", error.code),
    }
}

/// A sink must reset on a full capture, or verify its durable cache is exactly
/// `after_revision` before accepting a delta. Aborting must not replace that cache.
pub(crate) trait ContentCaptureSink {
    fn begin(&mut self, identity: &CaptureIdentity, after_revision: Option<i64>)
        -> StoreResult<()>;
    fn remove_record(&mut self, key: &str) -> StoreResult<()>;
    fn record(&mut self, key: &str, bytes: &[u8]) -> StoreResult<()>;
    fn object(&mut self, hash: &str, bytes: &[u8]) -> StoreResult<()>;
    fn reference(&mut self, record: &str, hash: &str, size: u64) -> StoreResult<()>;
    fn finish(&mut self) -> StoreResult<()>;
}

pub(crate) struct PreparedContentCapture {
    pub identity: CaptureIdentity,
    consumer: String,
    capture_id: String,
    reader: RevisionReadLease,
    full: bool,
    repository_root: PathBuf,
    _inventory: super::snapshot::DeferredAssetInventory,
}

impl PersistentStore {
    /// Pin the index and body to one read snapshot. Registration rechecks the
    /// live floor before committing this snapshot as an incremental baseline.
    pub(crate) fn prepare_content_capture(
        &mut self,
        capture_id: &str,
        consumer: &str,
        revision: i64,
    ) -> StoreResult<PreparedContentCapture> {
        if capture_id.is_empty() {
            return Err(missing_source("capture identity"));
        }
        if consumer.is_empty() {
            return Err(missing_source("capture consumer"));
        }
        let identity = sync_selection::identity(&self.connection)?;
        if identity.revision != revision {
            return Err(StoreError::RevisionConflict {
                expected: revision,
                actual: identity.revision,
            });
        }
        let _repository_guard =
            crate::asset_repository::coordinator::lock_repository_mutation()?;
        let (_, reader) = super::snapshot::acquire_revision(
            &self.database_path,
            revision,
            Arc::clone(&self.active_readers),
        )?;
        let inventory = self.active_readers.defer_asset_inventory();
        Ok(PreparedContentCapture {
            identity,
            consumer: consumer.into(),
            capture_id: capture_id.into(),
            reader,
            full: false,
            repository_root: self.repository_root.clone(),
            _inventory: inventory,
        })
    }
}

impl PersistentStore {
    /// Consume the caller's pinned lease without opening a second snapshot.
    pub(crate) fn prepare_content_capture_from_lease(
        &mut self,
        capture_id: &str,
        consumer: &str,
        lease: &str,
    ) -> StoreResult<PreparedContentCapture> {
        if capture_id.is_empty() || consumer.is_empty() {
            return Err(missing_source("capture identity"));
        }
        let reader = self.revision_leases.get(lease)
            .ok_or_else(|| missing_source("capture lease"))?;
        let identity = sync_selection::identity(&reader.connection)?;
        let current = sync_selection::identity(&self.connection)?;
        if identity.store_id != current.store_id
            || identity.library_epoch != current.library_epoch
            || identity.generation != current.generation
            || identity.selection_epoch != current.selection_epoch
            || identity.revision > current.revision
        {
            return Err(missing_source("matching capture lease authority"));
        }
        // Under the lock a collection sweeps under, so no sweep runs while
        // this capture writes its bodies.
        let inventory = {
            let _repository = crate::asset_repository::coordinator::lock_repository_mutation()?;
            self.active_readers.defer_asset_inventory()
        };
        let reader = self.revision_leases.remove(lease)
            .ok_or_else(|| missing_source("capture lease"))?;
        Ok(PreparedContentCapture {
            identity,
            consumer: consumer.into(),
            capture_id: capture_id.into(),
            reader,
            full: true,
            repository_root: self.repository_root.clone(),
            _inventory: inventory,
        })
    }
}

impl PreparedContentCapture {
    /// File finalization precedes this transaction. The content cursor, index
    /// floor and authoritative GC reference advance together. No remote state is
    /// required, and a rollback keeps the old local cursor usable.
    pub(crate) fn register(
        self,
        store: &mut PersistentStore,
        catalog: &crate::external_storage::capture::CaptureCatalog,
        scope_id: &[u8; 32],
        codec: &str,
    ) -> StoreResult<String> {
        self.register_with_device_capture_id(store,catalog,scope_id,codec,"")
    }

    pub(crate) fn register_with_device_capture_id(
        self,
        store: &mut PersistentStore,
        catalog: &crate::external_storage::capture::CaptureCatalog,
        scope_id: &[u8; 32],
        codec: &str,
        device_capture_id: &str,
    ) -> StoreResult<String> {
        let (file_hash, path, identity) = catalog.manifest()?;
        if identity != &self.identity
            || store.repository_root != self.repository_root
            || codec.is_empty()
            || (!device_capture_id.is_empty() && device_capture_id != hex::encode(file_hash))
        {
            return Err(missing_source("matching capture identity"));
        }
        let root = store
            .repository_root
            .join("external-storage")
            .canonicalize()?;
        let path = path.canonicalize()?;
        if !path.starts_with(&root) {
            return Err(missing_source("native capture spool path"));
        }
        let fingerprint = catalog.content_fingerprint(scope_id)?;
        let tx = store.connection.transaction()?;
        let id = super::external_storage_state::register_capture(
            &tx,
            &self.capture_id,
            &self.identity,
            &hex::encode(scope_id),
            codec,
            device_capture_id,
            &hex::encode(fingerprint),
            &self.consumer,
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO external_storage_capture_files VALUES(?1,?2,?3)",
            params![id, path.to_string_lossy(), hex::encode(file_hash)],
        )?;
        content_change_index::prune(&tx)?;
        tx.commit()?;
        Ok(id)
    }

    /// Retain this prepared capture through durable catalog registration. Drop
    /// it before starting network transfers. Its short GC deferral bridges the
    /// interval before the authoritative capture reference becomes visible.
    pub(crate) fn project(
        &self,
        sink: &mut dyn ContentCaptureSink,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<usize> {
        check(probe)?;
        let db = &self.reader.connection;
        let cas = PayloadCas::new(&self.repository_root)?;
        let sizes = PayloadSizes::open(&self.repository_root)?;
        let window = if self.full {
            ChangeWindow::Rebuild
        } else {
            content_change_index::window(&self.reader, &self.consumer)?
        };
        let after = match window {
            ChangeWindow::Rebuild => None,
            ChangeWindow::Incremental { after_revision } => {
                // Authority transitions can change alias interpretation without
                // touching individual rows, so rebuild before emitting any delta.
                let full: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM content_changes WHERE generation=?1 AND kind='full' AND revision>?2 AND revision<=?3)",
                    params![self.identity.generation,after_revision,self.identity.revision], |r|r.get(0))?;
                if full {
                    None
                } else {
                    Some(after_revision)
                }
            }
        };
        sink.begin(&self.identity, after)?;
        let mut previous = None;
        let mut count = 0;
        loop {
            check(probe)?;
            let keys = match after {
                Some(revision) => {
                    content_change_index::page(&self.reader, revision, previous.as_ref(), 128)?
                }
                None => all_keys(db, &self.identity.generation, previous.as_ref(), 128)?,
            };
            if keys.is_empty() {
                break;
            }
            for key in &keys {
                check(probe)?;
                project_record(db, &cas, &sizes, &self.identity.generation, key, sink, probe)?;
                count += 1;
            }
            previous = keys.last().cloned();
        }
        check(probe)?;
        sink.finish()?;
        Ok(count)
    }
}

fn check(probe: &dyn CancellationProbe) -> StoreResult<()> {
    if probe.is_cancelled() {
        return Err(StoreError::Validation {
            message: "External capture cancelled".into(),
        });
    }
    Ok(())
}

// Identifiers are schema constants. These are logical families, not provider
// keys. Owner locators map to their enclosing record in incremental windows.
const FAMILIES: &[(&str, &str, &str, &str, &str)] = &[
    (
        "asset",
        "asset_aliases",
        "logical_key",
        "''",
        "AND kind='asset'",
    ),
    ("character", "characters", "character_id", "''", ""),
    (
        "conversation",
        "conversations",
        "character_id",
        "conversation_id",
        "",
    ),
    (
        "inlay",
        "asset_aliases",
        "logical_key",
        "''",
        "AND kind='inlay'",
    ),
    ("plugin", "plugin_storage", "owner", "storage_key", ""),
    ("preset", "bot_presets", "preset_id", "''", ""),
    ("root", "root", "''", "''", ""),
];

fn all_keys(
    db: &Connection,
    generation: &str,
    after: Option<&ContentKey>,
    limit: usize,
) -> StoreResult<Vec<ContentKey>> {
    let mut keys = Vec::new();
    for &(kind, table, first, second, predicate) in FAMILIES {
        if after.is_some_and(|key| kind < key.kind.as_str()) || keys.len() == limit {
            continue;
        }
        let same = after.filter(|key| kind == key.kind);
        let condition = if same.is_some() {
            format!("AND ({first},{second})>(?2,?3)")
        } else {
            "AND ?2='' AND ?3=''".into()
        };
        let mut query = db.prepare(&format!("SELECT {first},{second} FROM {table} WHERE generation=?1 {predicate} {condition} ORDER BY {first},{second} LIMIT ?4"))?;
        let page = query
            .query_map(
                params![
                    generation,
                    same.map_or("", |k| k.key1.as_str()),
                    same.map_or("", |k| k.key2.as_str()),
                    (limit - keys.len()) as i64
                ],
                |r| {
                    Ok(ContentKey {
                        kind: kind.into(),
                        key1: r.get(0)?,
                        key2: r.get(1)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        keys.extend(page);
    }
    Ok(keys)
}

fn locator(key: &ContentKey) -> StoreResult<LogicalRecordLocator> {
    Ok(match key.kind.as_str() {
        "root" => LogicalRecordLocator::Root,
        "preset" => LogicalRecordLocator::Preset {
            preset_id: key.key1.clone(),
        },
        "plugin" => LogicalRecordLocator::Plugin {
            owner: key.key1.clone(),
            storage_key: key.key2.clone(),
        },
        "character" => LogicalRecordLocator::Character {
            character_id: key.key1.clone(),
        },
        "conversation" => LogicalRecordLocator::Conversation {
            character_id: key.key1.clone(),
            conversation_id: key.key2.clone(),
        },
        "asset" => LogicalRecordLocator::Asset {
            logical_key: key.key1.clone(),
        },
        "inlay" => LogicalRecordLocator::Inlay {
            logical_key: key.key1.clone(),
        },
        "owner" if key.key1 == "character-additional-assets" => LogicalRecordLocator::Character {
            character_id: key.key2.clone(),
        },
        "owner"
            if matches!(
                key.key1.as_str(),
                "root-module-assets" | "persona-embedded-module-assets"
            ) =>
        {
            LogicalRecordLocator::Root
        }
        _ => return Err(missing_source("content locator")),
    })
}

fn project_record(
    db: &Connection,
    cas: &PayloadCas,
    sizes: &PayloadSizes,
    generation: &str,
    key: &ContentKey,
    sink: &mut dyn ContentCaptureSink,
    probe: &dyn CancellationProbe,
) -> StoreResult<()> {
    let locator = locator(key)?;
    let normalized = match &locator {
        LogicalRecordLocator::Character { character_id } => ContentKey {
            kind: "character".into(),
            key1: character_id.clone(),
            key2: String::new(),
        },
        LogicalRecordLocator::Root => ContentKey {
            kind: "root".into(),
            key1: String::new(),
            key2: String::new(),
        },
        _ => key.clone(),
    };
    let &(_, table, first, second, predicate) = FAMILIES
        .iter()
        .find(|f| f.0 == normalized.kind)
        .ok_or_else(|| missing_source("record family"))?;
    let exists: bool = db.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE generation=?1 {predicate} AND {first}=?2 AND {second}=?3)"),
        params![generation,normalized.key1,normalized.key2], |r|r.get(0))?;
    let wire_key = encode_logical_record_key(&locator).map_err(codec_error)?;
    sink.remove_record(&wire_key)?;
    if !exists {
        return Ok(());
    }
    let mut derived = BTreeMap::new();
    let mut pages = Vec::new();
    if let LogicalRecordLocator::Conversation {
        character_id,
        conversation_id,
    } = &locator
    {
        let cached = super::message_pages::load_pages(db, generation, character_id, conversation_id)?;
        if cached.is_empty() {
            let mut query = db.prepare("SELECT value FROM messages WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 ORDER BY message_index")?;
            let mut rows = query.query(params![generation, character_id, conversation_id])?;
            let mut bodies = Vec::new();
            while let Some(row) = rows.next()? {
                check(probe)?;
                let value: String = row.get(0)?;
                bodies.push(super::stored_json::canonicalize(&value).map_err(codec_error)?);
            }
            let hashes = bodies.iter().map(|body| risunest_external_storage_format::message_pages::MessageHash::from_bytes(body)).collect();
            let result = risunest_external_storage_format::message_pages::repage::<StoreError>(
                &Default::default(), hashes, 0..0, bodies.len(), |index| {
                    check(probe)?;
                    Ok(bodies[index].clone())
                })?;
            for page in result.objects {
                sink.object(&page.hash, &page.bytes)?;
                sink.reference(&wire_key, &page.hash, page.size)?;
                pages.push(page.hash);
            }
        } else {
            for boundary in cached {
                check(probe)?;
                let page = boundary.page;
                let bytes = super::message_pages::object_body(db, &page.hash)?
                    .ok_or_else(|| missing_source("cached message page"))?;
                if bytes.len() as u64 != page.byte_length.0 {
                    return Err(missing_source("cached message page length"));
                }
                sink.object(&page.hash, &bytes)?;
                sink.reference(&wire_key, &page.hash, page.byte_length.0)?;
                pages.push(page.hash);
            }
        }
    }
    let record = super::record_projection::reconstruct_record_with_owner_objects(
        db,
        cas,
        generation,
        &wire_key,
        pages,
        |bytes| {
            derived.insert(
                hex::encode(risunest_external_storage_format::content_identity::hash(
                    bytes,
                )),
                bytes.to_vec(),
            );
            Ok(())
        },
        &|hash| {
            sizes
                .size(hash)?
                .ok_or_else(|| missing_source("local or custody payload"))
        },
    )?;
    let envelope = decode_logical_record(&record).map_err(codec_error)?;
    if let LogicalRecordEnvelope::Asset {
        object_hash, size, ..
    }
    | LogicalRecordEnvelope::Inlay {
        object_hash, size, ..
    } = &envelope
    {
        let hash = object_hash
            .as_ref()
            .ok_or_else(|| missing_source("canonical payload before external capture"))?;
        if sizes.size(hash)? != Some(*size) {
            return Err(missing_source("exact payload length"));
        }
    }
    for (hash, bytes) in &derived {
        sink.object(hash, bytes)?;
    }
    if !matches!(envelope, LogicalRecordEnvelope::Conversation { .. }) {
        let owner_manifest_hashes = match &envelope {
            LogicalRecordEnvelope::Root { owner_heads, .. }
            | LogicalRecordEnvelope::Character { owner_heads, .. }
            | LogicalRecordEnvelope::ArchivedCharacter { owner_heads, .. } => owner_heads
                .iter()
                .filter_map(|head| head.manifest_hash.as_deref())
                .collect::<std::collections::BTreeSet<_>>(),
            _ => std::collections::BTreeSet::new(),
        };
        for hash in envelope.dependency_hashes() {
            let bytes = if owner_manifest_hashes.contains(hash.as_str()) {
                Some(match derived.get(&hash) {
                    Some(bytes) => bytes.clone(),
                    None => cas
                        .read_object(&hash)?
                        .ok_or_else(|| missing_source("owner manifest"))?,
                })
            } else {
                None
            };
            let size = match &bytes {
                Some(bytes) => bytes.len() as u64,
                None => sizes
                    .size(&hash)?
                    .ok_or_else(|| missing_source("local or custody payload"))?,
            };
            sink.reference(&wire_key, &hash, size)?;
            if let Some(bytes) = bytes {
                super::record_projection::verify_object_bytes(&bytes, &hash, size)?;
                for entry in decode_owner_manifest(&bytes).map_err(codec_error)? {
                    if let Some(hash) = entry.payload_hash {
                        let hash = hex::encode(hash);
                        let size = sizes
                            .size(&hash)?
                            .ok_or_else(|| missing_source("local or custody owner payload"))?;
                        sink.reference(&wire_key, &hash, size)?;
                    }
                }
            }
        }
    }
    sink.record(&wire_key, &record)
}
