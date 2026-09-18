use super::snapshot_archive::Archive;
use super::{
    active_generation, current_revision, CheckpointMode, ReadTarget, SnapshotCreated, SnapshotInfo,
    StoreError, StoreResult, DATABASE_FILE, GENERATION_TABLES,
};
use crate::asset_repository::migration_gc::AssetRootSet;
use crate::asset_repository::PayloadCas;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

const MIN_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;
const UNSCANNABLE_BLOCKER: &str = "record-unscannable";
const CAS_PHYSICAL_PREFIX: &[u8] = b"assets-v2/objects/";
const COLD_STORAGE_HEADER: &str = "\u{ef01}COLDSTORAGE\u{ef01}";

#[cfg(test)]
thread_local! {
    pub(super) static ASSET_ROOT_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Default)]
pub(crate) struct ActiveReaderRegistry {
    count: AtomicUsize,
    deferred_asset_inventories: AtomicUsize,
    detached_asset_roots: Mutex<HashMap<String, AssetRootSet>>,
}

impl ActiveReaderRegistry {
    /// Short local captures pin their referenced objects incrementally. During
    /// that interval GC/eviction must defer instead of rescanning the full asset
    /// library on every small edit. Register under the repository mutation lock.
    pub(crate) fn defer_asset_inventory(self: &Arc<Self>) -> DeferredAssetInventory {
        self.deferred_asset_inventories
            .fetch_add(1, Ordering::SeqCst);
        DeferredAssetInventory(Arc::clone(self))
    }
    fn register(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }

    fn release(&self) {
        let previous = self.count.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "active reader registry underflow");
    }

    pub(crate) fn active_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    fn publish_detached_asset_roots(&self, lease: &str, roots: AssetRootSet) -> StoreResult<()> {
        self.detached_asset_roots
            .lock()
            .map_err(|error| StoreError::Store {
                message: format!("active reader roots mutex poisoned: {error}"),
            })?
            .insert(lease.to_owned(), roots);
        Ok(())
    }

    fn remove_detached_asset_roots(&self, lease: &str) {
        if let Ok(mut roots) = self.detached_asset_roots.lock() {
            roots.remove(lease);
        }
    }

    pub(crate) fn detached_asset_roots(&self) -> StoreResult<Vec<AssetRootSet>> {
        if self.deferred_asset_inventories.load(Ordering::SeqCst) > 0 {
            return Err(StoreError::Validation {
                message: "Asset inventory is being pinned by a local capture".into(),
            });
        }
        Ok(self
            .detached_asset_roots
            .lock()
            .map_err(|error| StoreError::Store {
                message: format!("active reader roots mutex poisoned: {error}"),
            })?
            .values()
            .cloned()
            .collect())
    }
}

pub(crate) struct DeferredAssetInventory(Arc<ActiveReaderRegistry>);
impl Drop for DeferredAssetInventory {
    fn drop(&mut self) {
        let previous = self
            .0
            .deferred_asset_inventories
            .fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0);
    }
}

pub(crate) struct RevisionReadLease {
    pub(crate) connection: Connection,
    pub(crate) target: ReadTarget,
    lease: String,
    repository_root: PathBuf,
    active_readers: Arc<ActiveReaderRegistry>,
    transaction_open: bool,
    #[cfg(test)]
    pub(crate) acquired_at: Instant,
}

impl RevisionReadLease {
    pub(crate) fn active_readers(&self) -> Arc<ActiveReaderRegistry> {
        Arc::clone(&self.active_readers)
    }

    pub(crate) fn publish_detached_asset_roots(&self) -> StoreResult<()> {
        let roots = collect_asset_roots(&self.connection)?;
        self.active_readers
            .publish_detached_asset_roots(&self.lease, roots)
    }

    #[cfg(feature = "native-official-publication")]
    pub(crate) fn asset_roots(&self) -> StoreResult<AssetRootSet> {
        collect_asset_roots_for_generation(&self.connection, &self.target.generation)
    }
}

impl Drop for RevisionReadLease {
    fn drop(&mut self) {
        if self.transaction_open {
            let _ = self.connection.execute_batch("ROLLBACK");
        }
        self.active_readers.remove_detached_asset_roots(&self.lease);
        self.active_readers.release();
    }
}

// Returns `Some(message)` when a pending restore existed but was skipped, so
// the caller can surface the failure instead of silently opening the old
// database. The marker is intentionally kept on failure so the restore retries
// on the next open (pinned by the restore-marker preservation tests).
pub(super) fn apply_pending_restore(
    persistent_dir: &Path,
    snapshots_dir: &Path,
) -> StoreResult<Option<String>> {
    if !snapshots_dir.join("snapshots.sqlite").exists()
        && !snapshots_dir.join("pending-restore.json").exists()
    {
        return Ok(None);
    }
    let result = (|| -> StoreResult<()> {
        let mut archive = Archive::open(snapshots_dir)?;
        let Some(id) = archive.pending_restore()? else {
            return Ok(());
        };
        let reconstructed = archive.scratch()?;
        let metadata = archive.restore(&id, &reconstructed.path)?;
        validate_restore_database(&reconstructed.path)?;
        let connection = Connection::open(&reconstructed.path)?;
        if current_revision(&connection)? != metadata.revision {
            return Err(validation("restored snapshot revision mismatch"));
        }
        drop(connection);
        let database_path = persistent_dir.join(DATABASE_FILE);
        let candidate = prepare_restore_candidate(persistent_dir, &reconstructed.path)?;
        let replacement = (|| -> StoreResult<()> {
            if database_path.is_file() {
                let connection = Connection::open(&database_path)?;
                create_in_archive(&connection, &mut archive, "pre-restore")?;
            }
            replace_database(&database_path, &candidate)
        })();
        if let Err(error) = remove_database_files(&candidate) {
            crate::nlog!(
                "warn",
                "persistent restore candidate cleanup skipped: {error}"
            );
        }
        replacement?;
        archive.clear_pending_restore(&id)?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(None),
        Err(error) => {
            let message = format!("persistent snapshot restore skipped: {error}");
            crate::nlog!("warn", "{message}");
            Ok(Some(message))
        }
    }
}

fn prepare_restore_candidate(persistent_dir: &Path, target: &Path) -> StoreResult<PathBuf> {
    let candidate = persistent_dir.join(format!(
        "{DATABASE_FILE}.restore-candidate-{}",
        Uuid::new_v4()
    ));
    fs::copy(target, &candidate)
        .map_err(|error| path_error("copy restore candidate", &candidate, error))?;

    let result = (|| -> StoreResult<()> {
        let mut connection = Connection::open(&candidate)?;
        super::schema::initialize(&mut connection)?;
        let transaction = connection.transaction()?;
        super::server_sync_outbox::restored_copy(&transaction)?;
        super::sync_selection::restored_copy(&transaction)?;
        transaction.commit()?;
        let _ = super::query::materialize(&connection, None)?;
        let integrity: String =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(validation("migrated snapshot integrity check failed"));
        }
        checkpoint(&connection, CheckpointMode::Truncate)?;
        drop(connection);
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&candidate)?
            .sync_all()?;
        Ok(())
    })();

    if result.is_err() {
        if let Err(error) = remove_database_files(&candidate) {
            crate::nlog!(
                "warn",
                "persistent restore candidate cleanup skipped: {error}"
            );
        }
    }
    result?;
    Ok(candidate)
}

pub(super) fn sweep_temporary_generations(
    connection: &mut Connection,
    retained_stage: Option<&str>,
) -> StoreResult<()> {
    let transaction = connection.transaction()?;
    if let Some(retained_stage) = retained_stage {
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM root WHERE generation=?1)",
            [retained_stage],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StoreError::Validation {
                message: "Native portable restore stage is missing".into(),
            });
        }
    }
    let legacy_generations = {
        let mut statement = transaction.prepare("SELECT generation FROM snapshot_leases")?;
        let generations = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        generations
    };
    transaction.execute("DELETE FROM snapshot_leases", [])?;
    let active = active_generation(&transaction)?;
    let mut statement = transaction.prepare(
        "SELECT generation FROM root
         WHERE generation LIKE 'staging-%'
            OR generation LIKE 'snapshot-%'",
    )?;
    let mut stale = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    stale.extend(legacy_generations);
    stale.sort();
    stale.dedup();
    for generation in stale {
        if generation != active && retained_stage != Some(generation.as_str()) {
            delete_generation(&transaction, &generation)?;
        }
    }
    transaction.commit()?;
    Ok(())
}

pub(super) fn acquire_revision(
    database_path: &Path,
    revision: i64,
    active_readers: Arc<ActiveReaderRegistry>,
) -> StoreResult<(String, RevisionReadLease)> {
    let repository_root = repository_root_from_database_path(database_path)?;
    let connection = open_revision_reader(database_path)?;
    #[cfg(test)]
    let acquired_at = Instant::now();
    let target = (|| -> StoreResult<ReadTarget> {
        let actual = current_revision(&connection)?;
        if actual != revision {
            return Err(StoreError::RevisionConflict {
                expected: revision,
                actual,
            });
        }
        let generation = active_generation(&connection)?;
        let root_exists = connection
            .query_row(
                "SELECT 1 FROM root WHERE generation = ?1",
                [&generation],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !root_exists {
            return Err(StoreError::RevisionConflict {
                expected: revision,
                actual,
            });
        }
        Ok(ReadTarget {
            revision,
            generation,
        })
    })();
    let target = match target {
        Ok(target) => target,
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            return Err(error);
        }
    };
    let lease = format!("snapshot-{revision}-{}", Uuid::new_v4());
    active_readers.register();
    Ok((
        lease.clone(),
        RevisionReadLease {
            connection,
            target,
            lease,
            repository_root,
            active_readers,
            transaction_open: true,
            #[cfg(test)]
            acquired_at,
        },
    ))
}

pub(super) fn open_revision_reader(database_path: &Path) -> StoreResult<Connection> {
    open_reader(database_path, &|_| Ok(()))
}

/// A reader that presents one generation under the raw table names. The views are temp objects,
/// so they have to exist before the connection becomes read-only.
pub(super) fn open_generation_reader(
    database_path: &Path,
    generation: &str,
) -> StoreResult<Connection> {
    open_reader(database_path, &|connection| {
        super::portable::install_generation_views(connection, generation)
    })
}

fn open_reader(
    database_path: &Path,
    prepare: &dyn Fn(&Connection) -> StoreResult<()>,
) -> StoreResult<Connection> {
    let preferred = revision_reader_open_flags_for_target(cfg!(target_os = "android"));
    if cfg!(target_os = "android") {
        return configure_revision_reader(database_path, preferred, prepare);
    }
    match configure_revision_reader(database_path, preferred, prepare) {
        Ok(connection) => Ok(connection),
        Err(_) => configure_revision_reader(
            database_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            prepare,
        ),
    }
}

pub(super) fn revision_reader_open_flags_for_target(is_android: bool) -> OpenFlags {
    let access = if is_android {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    access | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

fn configure_revision_reader(
    database_path: &Path,
    flags: OpenFlags,
    prepare: &dyn Fn(&Connection) -> StoreResult<()>,
) -> StoreResult<Connection> {
    let connection = Connection::open_with_flags(database_path, flags)?;
    connection.busy_timeout(Duration::ZERO)?;
    connection.execute_batch("PRAGMA cache_size = -2048;")?;
    prepare(&connection)?;
    connection.execute_batch(
        "
        PRAGMA query_only = ON;
        BEGIN DEFERRED;
        ",
    )?;
    Ok(connection)
}

pub(super) fn close_revision(mut lease: RevisionReadLease) -> StoreResult<()> {
    let result = lease
        .connection
        .execute_batch("ROLLBACK")
        .map_err(StoreError::from);
    if result.is_ok() {
        lease.transaction_open = false;
    }
    result
}

pub(super) fn checkpoint(connection: &Connection, mode: CheckpointMode) -> StoreResult<()> {
    let (mode, reject_busy) = match mode {
        CheckpointMode::Passive => ("PASSIVE", false),
        CheckpointMode::Truncate => ("TRUNCATE", true),
    };
    let (busy, _, _): (i64, i64, i64) =
        connection.query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if reject_busy && busy != 0 {
        return Err(StoreError::Store {
            message: "truncate checkpoint could not complete because the database is busy"
                .to_owned(),
        });
    }
    Ok(())
}

pub(super) fn create(
    connection: &Connection,
    snapshots_dir: &Path,
    reason: &str,
) -> StoreResult<SnapshotCreated> {
    let mut archive = Archive::open(snapshots_dir)?;
    create_in_archive(connection, &mut archive, reason)
}

fn create_in_archive(
    connection: &Connection,
    archive: &mut Archive,
    reason: &str,
) -> StoreResult<SnapshotCreated> {
    let started = Instant::now();
    let current_bytes = logical_database_bytes(connection)?;
    let scratch = archive.scratch()?;
    connection.execute("VACUUM INTO ?1", [scratch.path.to_string_lossy().as_ref()])?;
    let captured = Connection::open(&scratch.path)?;
    let revision = current_revision(&captured)?;
    let roots = collect_asset_roots(&captured)?;
    drop(captured);
    let metadata = archive.insert(&scratch.path, revision, reason, roots)?;
    archive.rotate(byte_budget(current_bytes), &metadata.id)?;
    Ok(SnapshotCreated {
        id: metadata.id,
        revision,
        bytes: metadata.bytes,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

pub(super) fn byte_budget(current_bytes: u64) -> u64 {
    current_bytes.saturating_mul(4).max(MIN_SNAPSHOT_BYTES)
}

pub(super) fn list(snapshots_dir: &Path) -> StoreResult<Vec<SnapshotInfo>> {
    Archive::open(snapshots_dir)?.list()
}

pub(super) fn snapshot_path_is_link_or_reparse(path: &Path) -> StoreResult<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Ok(metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0)
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

fn delete_generation(transaction: &rusqlite::Transaction<'_>, generation: &str) -> StoreResult<()> {
    for (table, _) in GENERATION_TABLES.iter().rev() {
        transaction.execute(
            &format!("DELETE FROM {table} WHERE generation = ?1"),
            [generation],
        )?;
    }
    Ok(())
}

fn logical_database_bytes(connection: &Connection) -> StoreResult<u64> {
    let page_count: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    Ok((page_count as u64).saturating_mul(page_size as u64))
}

pub(super) fn collect_asset_roots(
    connection: &Connection,
) -> StoreResult<AssetRootSet> {
    #[cfg(test)]
    ASSET_ROOT_SCANS.with(|count| count.set(count.get() + 1));
    collect_asset_roots_scoped(connection, None)
}

#[cfg(feature = "native-official-publication")]
fn collect_asset_roots_for_generation(
    connection: &Connection,
    generation: &str,
) -> StoreResult<AssetRootSet> {
    collect_asset_roots_scoped(connection, Some(generation))
}

// One scanner serves both the global GC-root collection and the per-generation
// publication pinning so the two table lists can never drift apart. The global
// scope additionally covers the logical-sync manifests, which are meaningless
// for a single generation.
fn collect_asset_roots_scoped(
    connection: &Connection,
    generation: Option<&str>,
) -> StoreResult<AssetRootSet> {
    let mut roots = AssetRootSet::default();
    let scope_params: Vec<&dyn rusqlite::ToSql> = generation
        .as_ref()
        .map(|generation| vec![generation as &dyn rusqlite::ToSql])
        .unwrap_or_default();
    let scope_params = scope_params.as_slice();
    let scoped = generation.is_some();

    scan_optional_hash_column(
        connection,
        if scoped {
            "SELECT manifest_hash FROM asset_owner_heads
         WHERE generation = ?1 AND present = 1"
        } else {
            "SELECT manifest_hash FROM asset_owner_heads WHERE present = 1"
        },
        scope_params,
        HashTarget::Manifest,
        &mut roots,
    )?;
    scan_asset_alias_roots(
        connection,
        if scoped {
            "SELECT logical_key, object_hash FROM asset_aliases WHERE generation = ?1"
        } else {
            "SELECT logical_key, object_hash FROM asset_aliases"
        },
        scope_params,
        &mut roots,
    )?;
    // An archived character keeps no scannable detail, so its payload object and
    // the asset hashes it recorded are the only roots that hold those bytes.
    scan_archived_object_roots(
        connection,
        if scoped {
            "SELECT archived_object FROM characters
         WHERE generation = ?1 AND archived_object IS NOT NULL"
        } else {
            "SELECT archived_object FROM characters WHERE archived_object IS NOT NULL"
        },
        scope_params,
        &mut roots,
    )?;
    if !scoped {
        if table_exists(connection, "server_sync_objects")? {
            scan_optional_hash_column(
                connection,
                "SELECT hash FROM server_sync_objects",
                [],
                HashTarget::Object,
                &mut roots,
            )?;
        }
    }

    for (table, column) in [
        ("root", "value"),
        ("bot_presets", "value"),
        ("characters", "detail"),
        ("conversations", "detail"),
        ("messages", "value"),
        ("plugin_storage", "value"),
    ] {
        let query = if scoped {
            format!("SELECT {column} FROM {table} WHERE generation = ?1")
        } else {
            format!("SELECT {column} FROM {table}")
        };
        scan_json_column(connection, &query, scope_params, &mut roots)?;
    }
    for table in ["bot_presets", "characters"] {
        let query = if scoped {
            format!("SELECT image FROM {table} WHERE generation = ?1 AND image IS NOT NULL")
        } else {
            format!("SELECT image FROM {table} WHERE image IS NOT NULL")
        };
        scan_text_column(connection, &query, scope_params, &mut roots)?;
    }
    let plugin_rows: i64 = connection.query_row(
        if scoped {
            "SELECT COUNT(*) FROM plugin_storage WHERE generation = ?1"
        } else {
            "SELECT COUNT(*) FROM plugin_storage"
        },
        scope_params,
        |row| row.get(0),
    )?;
    if plugin_rows > 0 {
        roots.blockers.insert("plugin-storage-opaque".to_owned());
        roots.retain_all_objects = true;
    }
    // Nothing stores a cold payload any more, so a record that still references
    // one hides an unknowable set of attachments. Keep every object instead.
    if !roots.cold_keys.is_empty() {
        roots.blockers.insert("cold-payload-unscanned".to_owned());
        roots.retain_all_objects = true;
    }
    Ok(roots)
}

/// Every CAS object one character's records reach, so archiving can record them
/// and keep them out of the sweep while the character has no scannable detail.
pub(super) fn collect_character_asset_hashes(
    connection: &Connection,
    cas: &PayloadCas,
    generation: &str,
    character_id: &str,
) -> StoreResult<Vec<String>> {
    let mut roots = AssetRootSet::default();
    let scope: [&dyn rusqlite::ToSql; 2] = [&generation, &character_id];
    for query in [
        "SELECT detail FROM characters WHERE generation = ?1 AND character_id = ?2",
        "SELECT detail FROM conversations WHERE generation = ?1 AND character_id = ?2",
        "SELECT value FROM messages WHERE generation = ?1 AND character_id = ?2",
    ] {
        scan_json_column(connection, query, scope.as_slice(), &mut roots)?;
    }
    scan_text_column(
        connection,
        "SELECT image FROM characters
         WHERE generation = ?1 AND character_id = ?2 AND image IS NOT NULL",
        scope.as_slice(),
        &mut roots,
    )?;

    let mut hashes: BTreeSet<String> = std::mem::take(&mut roots.object_hashes);
    let manifest_hash: Option<String> = connection
        .query_row(
            "SELECT manifest_hash FROM asset_owner_heads
             WHERE generation = ?1 AND owner_kind = 'character-additional-assets'
               AND owner_locator = ?2 AND present = 1",
            rusqlite::params![generation, character_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    if let Some(manifest_hash) = manifest_hash {
        if let Some(canonical) = cas.read_object(&manifest_hash)? {
            if let Ok(entries) =
                crate::asset_repository::owner_manifest_codec::decode_owner_manifest(&canonical)
            {
                for entry in entries {
                    if let Some(payload) = entry.payload_hash {
                        hashes.insert(hex::encode(payload));
                    }
                }
            }
        }
        hashes.insert(manifest_hash);
    }

    let mut candidates: BTreeSet<String> = roots
        .legacy_asset_keys
        .iter()
        .chain(&roots.inlay_ids)
        .cloned()
        .collect();
    collect_alias_key_candidates(connection, generation, character_id, &mut candidates)?;
    for candidate in candidates {
        let mut statement = connection.prepare_cached(
            "SELECT object_hash FROM asset_aliases
             WHERE generation = ?1 AND logical_key = ?2 AND object_hash IS NOT NULL",
        )?;
        let found = statement
            .query_map(rusqlite::params![generation, candidate], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        hashes.extend(found);
    }
    Ok(hashes.into_iter().collect())
}

/// String leaves of the character detail are the only place an alias logical key
/// can appear without a recognizable prefix, so they are looked up directly.
fn collect_alias_key_candidates(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    candidates: &mut BTreeSet<String>,
) -> StoreResult<()> {
    const MAX_CANDIDATE_BYTES: usize = 512;
    let detail: Option<String> = connection
        .query_row(
            "SELECT detail FROM characters WHERE generation = ?1 AND character_id = ?2",
            rusqlite::params![generation, character_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(detail) = detail else {
        return Ok(());
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&detail) else {
        return Ok(());
    };
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            serde_json::Value::String(value) => {
                if !value.is_empty()
                    && value.len() <= MAX_CANDIDATE_BYTES
                    && !value.contains(char::is_whitespace)
                {
                    candidates.insert(value);
                }
            }
            serde_json::Value::Array(values) => pending.extend(values),
            serde_json::Value::Object(values) => {
                pending.extend(values.into_iter().map(|(_, value)| value))
            }
            _ => {}
        }
    }
    Ok(())
}

fn repository_root_from_database_path(database_path: &Path) -> StoreResult<PathBuf> {
    database_path
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| validation("persistent database has no repository root"))
}

fn table_exists(connection: &Connection, table: &str) -> StoreResult<bool> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(true),
        )
        .optional()
        .map(|value| value.unwrap_or(false))
        .map_err(StoreError::from)
}

// A snapshot exists to preserve the rows it captures, so a value whose storage
// class or encoding is damaged widens retention instead of failing the capture.
enum ScannedText {
    Text(String),
    Null,
    Damaged,
}

fn scanned_text(row: &rusqlite::Row<'_>, index: usize) -> StoreResult<ScannedText> {
    Ok(match row.get_ref(index)? {
        rusqlite::types::ValueRef::Null => ScannedText::Null,
        rusqlite::types::ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
            Ok(value) => ScannedText::Text(value.to_owned()),
            Err(_) => ScannedText::Damaged,
        },
        _ => ScannedText::Damaged,
    })
}

fn retain_unscannable_record(roots: &mut AssetRootSet) {
    roots.blockers.insert(UNSCANNABLE_BLOCKER.to_owned());
    roots.retain_all_objects = true;
}

enum HashTarget {
    Manifest,
    Object,
}

fn scan_optional_hash_column<P: rusqlite::Params>(
    connection: &Connection,
    query: &str,
    params: P,
    target: HashTarget,
    roots: &mut AssetRootSet,
) -> StoreResult<()> {
    let mut statement = connection.prepare(query)?;
    let mut rows = statement.query(params)?;
    while let Some(row) = rows.next()? {
        match scanned_text(row, 0)? {
            ScannedText::Text(value) => {
                match target {
                    HashTarget::Manifest => roots.manifest_hashes.insert(value),
                    HashTarget::Object => roots.object_hashes.insert(value),
                };
            }
            ScannedText::Null => {}
            ScannedText::Damaged => retain_unscannable_record(roots),
        }
    }
    Ok(())
}

fn scan_asset_alias_roots<P: rusqlite::Params>(
    connection: &Connection,
    query: &str,
    params: P,
    roots: &mut AssetRootSet,
) -> StoreResult<()> {
    let mut statement = connection.prepare(query)?;
    let mut rows = statement.query(params)?;
    while let Some(row) = rows.next()? {
        match scanned_text(row, 1)? {
            ScannedText::Text(object_hash) => {
                roots.object_hashes.insert(object_hash);
            }
            ScannedText::Null => match scanned_text(row, 0)? {
                ScannedText::Text(logical_key) => {
                    roots.legacy_asset_keys.insert(logical_key);
                }
                _ => retain_unscannable_record(roots),
            },
            ScannedText::Damaged => retain_unscannable_record(roots),
        }
    }
    Ok(())
}

fn scan_archived_object_roots<P: rusqlite::Params>(
    connection: &Connection,
    query: &str,
    params: P,
    roots: &mut AssetRootSet,
) -> StoreResult<()> {
    let mut statement = connection.prepare(query)?;
    let mut rows = statement.query(params)?;
    while let Some(row) = rows.next()? {
        let ScannedText::Text(encoded) = scanned_text(row, 0)? else {
            retain_unscannable_record(roots);
            continue;
        };
        match serde_json::from_str::<super::archive::ArchivedObject>(&encoded) {
            Ok(archived) => {
                roots.object_hashes.insert(archived.object_hash);
                roots.object_hashes.extend(archived.asset_hashes);
            }
            Err(_) => retain_unscannable_record(roots),
        }
    }
    Ok(())
}

fn scan_json_column<P: rusqlite::Params>(
    connection: &Connection,
    query: &str,
    params: P,
    roots: &mut AssetRootSet,
) -> StoreResult<()> {
    let mut statement = connection.prepare(query)?;
    let mut rows = statement.query(params)?;
    while let Some(row) = rows.next()? {
        // The snapshot contains this exact record. If its references cannot
        // be decoded, retain objects instead of discarding the raw backup.
        match scanned_text(row, 0)? {
            ScannedText::Text(encoded) => match serde_json::from_str(&encoded) {
                Ok(value) => observe_json_value(&value, None, roots),
                Err(_) => retain_unscannable_record(roots),
            },
            _ => retain_unscannable_record(roots),
        }
    }
    Ok(())
}

fn scan_text_column<P: rusqlite::Params>(
    connection: &Connection,
    query: &str,
    params: P,
    roots: &mut AssetRootSet,
) -> StoreResult<()> {
    let mut statement = connection.prepare(query)?;
    let mut rows = statement.query(params)?;
    while let Some(row) = rows.next()? {
        match scanned_text(row, 0)? {
            ScannedText::Text(value) => observe_text(&value, roots),
            _ => retain_unscannable_record(roots),
        }
    }
    Ok(())
}

fn observe_json_value(
    value: &serde_json::Value,
    parent_key: Option<&str>,
    roots: &mut AssetRootSet,
) {
    match value {
        serde_json::Value::String(value) => {
            observe_text(value, roots);
            if parent_key == Some("coldstorage") && !value.is_empty() {
                roots.cold_keys.insert(value.clone());
            } else if parent_key == Some("coldStoragedChats") {
                roots.cold_keys.insert(value.clone());
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                observe_json_value(value, parent_key, roots);
            }
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                observe_json_value(value, Some(key), roots);
            }
        }
        _ => {}
    }
}

fn observe_text(value: &str, roots: &mut AssetRootSet) {
    if value.starts_with("assets/") {
        roots.legacy_asset_keys.insert(value.to_owned());
    }
    if let Some(cold_key) = value.strip_prefix(COLD_STORAGE_HEADER) {
        if !cold_key.is_empty() {
            roots.cold_keys.insert(cold_key.to_owned());
        }
    }
    observe_native_cas_paths(value, roots);
    for prefix in ["{{inlay::", "{{inlayed::", "{{inlayeddata::"] {
        let mut remainder = value;
        while let Some(start) = remainder.find(prefix) {
            remainder = &remainder[start + prefix.len()..];
            let Some(end) = remainder.find("}}") else {
                break;
            };
            roots.inlay_ids.insert(remainder[..end].to_owned());
            remainder = &remainder[end + 2..];
        }
    }
}

fn observe_native_cas_paths(value: &str, roots: &mut AssetRootSet) {
    let bytes = value.as_bytes();
    let physical_len = CAS_PHYSICAL_PREFIX.len() + 65;
    if bytes.len() >= physical_len {
        for start in 0..=bytes.len() - physical_len {
            let candidate = &bytes[start..start + physical_len];
            if let Some(hash) = cas_hash_from_physical_key(candidate) {
                if bytes
                    .get(start + physical_len)
                    .is_none_or(|next| !next.is_ascii_hexdigit() && *next != b'/')
                {
                    roots.object_hashes.insert(hash);
                }
            }
        }
    }

    for (start, _) in value.char_indices() {
        let remainder = &value[start..];
        if !starts_with_url_scheme(remainder) {
            continue;
        }
        let end = remainder.find(is_url_delimiter).unwrap_or(remainder.len());
        let candidate = &remainder[..end];
        let native_marker = candidate.to_ascii_lowercase().contains("risuasset");
        let Some(physical_key) = crate::native_media::decode_physical_key(candidate) else {
            if native_marker {
                retain_unknown_native_url(roots);
            }
            continue;
        };
        if let Some(hash) = cas_hash_from_physical_key(physical_key.as_bytes()) {
            roots.object_hashes.insert(hash);
        } else {
            retain_unknown_native_url(roots);
        }
    }
}

fn starts_with_url_scheme(value: &str) -> bool {
    ["risuasset:", "http:", "https:"].iter().any(|prefix| {
        value
            .get(..prefix.len())
            .is_some_and(|value| value.eq_ignore_ascii_case(prefix))
    })
}

fn is_url_delimiter(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}'
        )
}

fn retain_unknown_native_url(roots: &mut AssetRootSet) {
    roots.retain_all_objects = true;
    roots
        .blockers
        .insert("native-asset-url-unresolved".to_owned());
}

fn cas_hash_from_physical_key(value: &[u8]) -> Option<String> {
    if value.len() != CAS_PHYSICAL_PREFIX.len() + 65 || !value.starts_with(CAS_PHYSICAL_PREFIX) {
        return None;
    }
    let suffix = &value[CAS_PHYSICAL_PREFIX.len()..];
    if suffix[2] != b'/'
        || !suffix[..2]
            .iter()
            .chain(&suffix[3..])
            .all(|byte| crate::trust_boundary::is_lower_hex_byte(*byte))
    {
        return None;
    }
    let mut hash = Vec::with_capacity(64);
    hash.extend_from_slice(&suffix[..2]);
    hash.extend_from_slice(&suffix[3..]);
    String::from_utf8(hash).ok()
}

fn validate_restore_database(path: &Path) -> StoreResult<()> {
    let connection = Connection::open(path)?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(validation("snapshot integrity check failed"));
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != i64::from(super::schema::SCHEMA_VERSION) {
        return Err(validation("snapshot schema version is not supported"));
    }
    Ok(())
}

fn remove_database_files(database_path: &Path) -> StoreResult<()> {
    for path in [
        database_path.to_path_buf(),
        PathBuf::from(format!("{}-wal", database_path.display())),
        PathBuf::from(format!("{}-shm", database_path.display())),
    ] {
        remove_file_if_exists(&path)?;
    }
    Ok(())
}

fn replace_database(database_path: &Path, target: &Path) -> StoreResult<()> {
    let next = database_path.with_extension(format!("sqlite.restore-next-{}", Uuid::new_v4()));
    fs::copy(target, &next).map_err(|error| path_error("copy restore candidate", &next, error))?;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&next)?
        .sync_all()?;

    if !database_path.exists() {
        fs::rename(&next, database_path)
            .map_err(|error| path_error("activate restore candidate", database_path, error))?;
        return Ok(());
    }

    let previous = database_path.with_extension("sqlite.restore-previous");
    if previous.exists() {
        fs::remove_file(&previous)?;
    }
    fs::rename(database_path, &previous)
        .map_err(|error| path_error("preserve current database", &previous, error))?;
    if let Err(error) = fs::rename(&next, database_path) {
        fs::rename(&previous, database_path).map_err(|rollback| {
            path_error("roll back current database", database_path, rollback)
        })?;
        remove_file_if_exists(&next)?;
        return Err(path_error(
            "activate restore candidate",
            database_path,
            error,
        ));
    }
    remove_database_files(&previous)?;
    remove_file_if_exists(&PathBuf::from(format!("{}-wal", database_path.display())))?;
    remove_file_if_exists(&PathBuf::from(format!("{}-shm", database_path.display())))?;
    Ok(())
}

fn path_error(context: &str, path: &Path, error: std::io::Error) -> StoreError {
    StoreError::Store {
        message: format!("{context} at {}: {error}", path.display()),
    }
}

fn remove_file_if_exists(path: &Path) -> StoreResult<()> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn validation(message: impl Into<String>) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}
