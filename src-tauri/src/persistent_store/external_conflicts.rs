//! External conflict ownership lives in the device database so replacing the
//! library database cannot discard a preserved source.
use super::{sync_selection::CaptureIdentity, StoreError, StoreResult};
use crate::external_storage::capture::{
    registered_capture_roots, validate_capture_sources, DurableCaptureReference,
    RegisteredCaptureRoots,
};
use risunest_external_storage_format::snapshot::{ObjectRole, StoredObject};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::Path;

const MAX_ID_BYTES: usize = 1024;
const MAX_PATH_BYTES: usize = 8192;
const CONFLICT_PAGE_LIMIT: usize = 50;

pub(crate) const DEVICE_SCHEMA: &str = r#"
CREATE TABLE external_conflicts(
    id TEXT PRIMARY KEY,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    connection_id TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    local_identity TEXT NOT NULL,
    local_capture_id TEXT NOT NULL,
    local_catalog_path TEXT NOT NULL,
    local_catalog_hash TEXT NOT NULL,
    remote_state TEXT NOT NULL,
    remote_point TEXT,
    resolved INTEGER NOT NULL DEFAULT 0 CHECK(resolved IN (0,1))
);
CREATE INDEX external_conflicts_newest
ON external_conflicts(created_at_ms DESC, id DESC);
"#;

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}

fn bounded(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.contains('\0')
}

pub(crate) fn conflict_capture_owner(id: &str) -> StoreResult<String> {
    if !bounded(id, MAX_ID_BYTES) {
        return Err(invalid("External conflict identity is invalid"));
    }
    Ok(format!("conflict:{id}"))
}

fn encode<T: Serialize>(value: &T) -> StoreResult<String> {
    Ok(serde_json::to_string(value)?)
}

fn decode<T: DeserializeOwned + Serialize>(value: &str, message: &str) -> StoreResult<T> {
    let decoded: T = serde_json::from_str(value).map_err(|_| invalid(message))?;
    if serde_json::to_string(&decoded).map_err(|_| invalid(message))? != value {
        return Err(invalid(message));
    }
    Ok(decoded)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PreservedHeadObservation {
    pub commit_id: String,
    pub authenticated_body_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PreservedRemoteState {
    pub snapshot: StoredObject,
    pub logical_revision: i64,
    pub commit_id: String,
    pub head: PreservedHeadObservation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExternalConflictRecord {
    pub id: String,
    pub created_at_ms: i64,
    pub connection_id: String,
    pub repository_id: String,
    pub local: DurableCaptureReference,
    pub remote: PreservedRemoteState,
    pub remote_point: Option<StoredObject>,
    pub resolved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExternalConflictCursor {
    pub created_at_ms: i64,
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExternalConflictPage {
    pub conflicts: Vec<ExternalConflictRecord>,
    pub next: Option<ExternalConflictCursor>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ConflictSide {
    Local,
    Remote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConflictSourceDescriptor {
    Local {
        conflict_id: String,
        repository_id: String,
        capture: DurableCaptureReference,
    },
    Remote {
        conflict_id: String,
        connection_id: String,
        repository_id: String,
        snapshot: StoredObject,
    },
}

pub(crate) fn create_device_schema(db: &Connection) -> StoreResult<()> {
    db.execute_batch(DEVICE_SCHEMA)?;
    Ok(())
}

pub(crate) fn validate_device_schema(db: &Connection) -> StoreResult<()> {
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(DEVICE_SCHEMA)?;
    for (kind, name) in [
        ("table", "external_conflicts"),
        ("index", "external_conflicts_newest"),
    ] {
        let expected: String = reference.query_row(
            "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
            params![kind, name],
            |row| row.get(0),
        )?;
        let actual: Option<String> = db
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
                params![kind, name],
                |row| row.get(0),
            )
            .optional()?;
        if actual.as_deref() != Some(expected.as_str()) {
            return Err(invalid("External conflict schema is incompatible"));
        }
    }
    Ok(())
}

fn validate_identity(identity: &CaptureIdentity) -> StoreResult<()> {
    if !bounded(&identity.store_id, MAX_ID_BYTES)
        || !bounded(&identity.library_epoch, MAX_ID_BYTES)
        || !bounded(&identity.generation, MAX_ID_BYTES)
        || !bounded(&identity.selection_epoch, MAX_ID_BYTES)
        || identity.revision < 0
    {
        return Err(invalid("External conflict local identity is invalid"));
    }
    Ok(())
}

fn validate_capture(reference: &DurableCaptureReference) -> StoreResult<()> {
    validate_identity(&reference.identity)?;
    if !bounded(&reference.capture_id, MAX_ID_BYTES)
        || !bounded(&reference.catalog_path, MAX_PATH_BYTES)
        || Path::new(&reference.catalog_path)
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        || !crate::trust_boundary::is_lower_hex_256(&reference.catalog_hash)
    {
        return Err(invalid("External conflict capture reference is invalid"));
    }
    Ok(())
}

fn validate_remote(remote: &PreservedRemoteState, repository_id: &str) -> StoreResult<()> {
    remote
        .snapshot
        .validate()
        .map_err(|_| invalid("External conflict remote state is invalid"))?;
    if remote.snapshot.header.repository_id != repository_id
        || !matches!(remote.snapshot.header.role, ObjectRole::SyncState | ObjectRole::BackupBundle)
        || remote.logical_revision < 0
        || !bounded(&remote.commit_id, MAX_ID_BYTES)
        || remote.head.commit_id != remote.commit_id
        || !crate::trust_boundary::is_lower_hex_256(&remote.head.authenticated_body_hash)
    {
        return Err(invalid("External conflict remote state is invalid"));
    }
    Ok(())
}

fn validate_remote_point(point: &StoredObject, conflict_id: &str, repository_id: &str) -> StoreResult<()> {
    point
        .validate()
        .map_err(|_| invalid("External conflict point is invalid"))?;
    if point.header.repository_id != repository_id
        || point.header.role != ObjectRole::BackupPoint
        || point.header.object_id != format!("backup-point-{conflict_id}")
    {
        return Err(invalid("External conflict point binding differs"));
    }
    Ok(())
}

fn validate_record(record: &ExternalConflictRecord) -> StoreResult<()> {
    if !bounded(&record.id, MAX_ID_BYTES)
        || !bounded(&record.connection_id, MAX_ID_BYTES)
        || !bounded(&record.repository_id, 128)
        || record.created_at_ms < 0
    {
        return Err(invalid("External conflict identity is invalid"));
    }
    validate_capture(&record.local)?;
    validate_remote(&record.remote, &record.repository_id)?;
    if let Some(point) = &record.remote_point {
        validate_remote_point(point, &record.id, &record.repository_id)?;
    }
    Ok(())
}

type ConflictRow = (
    String, i64, String, String, String, String, String, String, String, Option<String>, bool,
);

fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConflictRow> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?, row.get(10)?))
}

fn from_row(row: ConflictRow) -> StoreResult<ExternalConflictRecord> {
    let (id, created_at_ms, connection_id, repository_id, local_identity, local_capture_id, local_catalog_path, local_catalog_hash, remote_state, remote_point, resolved) = row;
    let record = ExternalConflictRecord {
        id,
        created_at_ms,
        connection_id,
        repository_id,
        local: DurableCaptureReference {
            capture_id: local_capture_id,
            identity: decode(&local_identity, "External conflict local identity is invalid")?,
            catalog_path: local_catalog_path,
            catalog_hash: local_catalog_hash,
        },
        remote: decode(&remote_state, "External conflict remote state is invalid")?,
        remote_point: remote_point.map(|value| decode(&value, "External conflict point is invalid")).transpose()?,
        resolved,
    };
    validate_record(&record)?;
    Ok(record)
}

const COLUMNS: &str = "id,created_at_ms,connection_id,repository_id,local_identity,local_capture_id,local_catalog_path,local_catalog_hash,remote_state,remote_point,resolved";

pub(crate) fn external_conflict(db: &Connection, id: &str) -> StoreResult<Option<ExternalConflictRecord>> {
    if !bounded(id, MAX_ID_BYTES) {
        return Err(invalid("External conflict identity is invalid"));
    }
    let sql = format!("SELECT {COLUMNS} FROM external_conflicts WHERE id=?1");
    db.query_row(&sql, [id], decode_row).optional()?.map(from_row).transpose()
}

fn same_preservation(left: &ExternalConflictRecord, right: &ExternalConflictRecord) -> bool {
    left.id == right.id
        && left.created_at_ms == right.created_at_ms
        && left.connection_id == right.connection_id
        && left.repository_id == right.repository_id
        && left.local == right.local
        && left.remote == right.remote
}

/// Persists the local owner before a job may release its own capture reference.
/// This function performs no remote request and accepts no renderer path.
pub(crate) fn preserve_local_conflict(db: &Connection, record: &ExternalConflictRecord) -> StoreResult<ExternalConflictRecord> {
    validate_record(record)?;
    if record.remote_point.is_some() || record.resolved {
        return Err(invalid("A new external conflict is already complete"));
    }
    let tx = db.unchecked_transaction()?;
    if let Some(existing) = external_conflict(&tx, &record.id)? {
        if same_preservation(&existing, record) {
            tx.commit()?;
            return Ok(existing);
        }
        return Err(invalid("External conflict identity was reused"));
    }
    tx.execute(
        "INSERT INTO external_conflicts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,NULL,0)",
        params![record.id, record.created_at_ms, record.connection_id, record.repository_id, encode(&record.local.identity)?, record.local.capture_id, record.local.catalog_path, record.local.catalog_hash, encode(&record.remote)?],
    )?;
    tx.commit()?;
    Ok(record.clone())
}

pub(crate) fn confirm_external_conflict_point(db: &Connection, id: &str, point: &StoredObject) -> StoreResult<ExternalConflictRecord> {
    let tx = db.unchecked_transaction()?;
    let mut record = external_conflict(&tx, id)?.ok_or_else(|| invalid("External conflict does not exist"))?;
    validate_remote_point(point, id, &record.repository_id)?;
    if let Some(existing) = &record.remote_point {
        if existing != point {
            return Err(invalid("External conflict point changed"));
        }
        tx.commit()?;
        return Ok(record);
    }
    if tx.execute("UPDATE external_conflicts SET remote_point=?2 WHERE id=?1 AND remote_point IS NULL", params![id, encode(point)?])? != 1 {
        return Err(invalid("External conflict point changed"));
    }
    tx.commit()?;
    record.remote_point = Some(point.clone());
    Ok(record)
}

pub(crate) fn external_conflict_for_resolution(db: &Connection, id: &str) -> StoreResult<ExternalConflictRecord> {
    let record = external_conflict(db, id)?.ok_or_else(|| invalid("External conflict does not exist"))?;
    if record.resolved || record.remote_point.is_none() {
        return Err(invalid("External conflict preservation is incomplete"));
    }
    Ok(record)
}

pub(crate) fn mark_external_conflict_resolved(db: &Connection, id: &str) -> StoreResult<ExternalConflictRecord> {
    let mut record = external_conflict_for_resolution(db, id)?;
    if db.execute("UPDATE external_conflicts SET resolved=1 WHERE id=?1 AND resolved=0 AND remote_point IS NOT NULL", [id])? != 1 {
        return Err(invalid("External conflict resolution changed"));
    }
    record.resolved = true;
    Ok(record)
}

pub(crate) fn delete_external_conflict(db: &Connection, id: &str) -> StoreResult<ExternalConflictRecord> {
    let tx = db.unchecked_transaction()?;
    let record = external_conflict(&tx, id)?.ok_or_else(|| invalid("External conflict does not exist"))?;
    if tx.execute("DELETE FROM external_conflicts WHERE id=?1", [id])? != 1 {
        return Err(invalid("External conflict changed before deletion"));
    }
    tx.commit()?;
    Ok(record)
}

pub(crate) fn external_conflicts_page(db: &Connection, after: Option<&ExternalConflictCursor>, limit: usize) -> StoreResult<ExternalConflictPage> {
    if limit == 0 || limit > CONFLICT_PAGE_LIMIT {
        return Err(invalid("External conflict page limit is invalid"));
    }
    if after.is_some_and(|cursor| cursor.created_at_ms < 0 || !bounded(&cursor.id, MAX_ID_BYTES)) {
        return Err(invalid("External conflict cursor is invalid"));
    }
    let mut conflicts = Vec::with_capacity(limit + 1);
    if let Some(cursor) = after {
        let sql = format!("SELECT {COLUMNS} FROM external_conflicts WHERE created_at_ms<?1 OR (created_at_ms=?1 AND id<?2) ORDER BY created_at_ms DESC,id DESC LIMIT ?3");
        let mut statement = db.prepare(&sql)?;
        for row in statement.query_map(params![cursor.created_at_ms, cursor.id, (limit + 1) as i64], decode_row)? {
            conflicts.push(from_row(row?)?);
        }
    } else {
        let sql = format!("SELECT {COLUMNS} FROM external_conflicts ORDER BY created_at_ms DESC,id DESC LIMIT ?1");
        let mut statement = db.prepare(&sql)?;
        for row in statement.query_map([(limit + 1) as i64], decode_row)? {
            conflicts.push(from_row(row?)?);
        }
    }
    let next = if conflicts.len() > limit {
        conflicts.pop();
        conflicts.last().map(|record| ExternalConflictCursor { created_at_ms: record.created_at_ms, id: record.id.clone() })
    } else {
        None
    };
    Ok(ExternalConflictPage { conflicts, next })
}

/// Includes resolved conflicts because resolution changes display state only.
pub(crate) fn registered_conflict_roots(db: &Connection, repository_root: &Path) -> StoreResult<RegisteredCaptureRoots> {
    let mut statement = db.prepare("SELECT local_identity,local_capture_id,local_catalog_path,local_catalog_hash FROM external_conflicts ORDER BY id")?;
    let mut rows = statement.query([])?;
    let mut roots = RegisteredCaptureRoots::default();
    while let Some(row) = rows.next()? {
        let reference = DurableCaptureReference {
            identity: decode(&row.get::<_, String>(0)?, "External conflict local identity is invalid")?,
            capture_id: row.get(1)?,
            catalog_path: row.get(2)?,
            catalog_hash: row.get(3)?,
        };
        validate_capture(&reference)?;
        let registered = registered_capture_roots([&reference], repository_root)?;
        roots.assets.object_hashes.extend(registered.assets.object_hashes);
        roots.catalogs.extend(registered.catalogs);
        roots.logical_records.extend(registered.logical_records);
    }
    Ok(roots)
}

pub(crate) fn conflict_source_descriptor(db: &Connection, repository_root: &Path, id: &str, side: ConflictSide) -> StoreResult<ConflictSourceDescriptor> {
    let record = external_conflict(db, id)?.ok_or_else(|| invalid("External conflict does not exist"))?;
    match side {
        ConflictSide::Local => {
            validate_capture_sources([&record.local], repository_root)?;
            Ok(ConflictSourceDescriptor::Local {
                conflict_id: record.id,
                repository_id: record.repository_id,
                capture: record.local,
            })
        }
        ConflictSide::Remote => {
            if record.remote_point.is_none() {
                return Err(invalid("External conflict remote point is unconfirmed"));
            }
            Ok(ConflictSourceDescriptor::Remote { conflict_id: record.id, connection_id: record.connection_id, repository_id: record.repository_id, snapshot: record.remote.snapshot })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistent_store::content_capture::ContentCaptureSink;
    use risunest_external_storage_format::snapshot::{
        envelope_length, PublicObjectHeader, WireLocator,
    };

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        create_device_schema(&db).unwrap();
        validate_device_schema(&db).unwrap();
        db
    }

    fn object(repository: &str, id: &str, role: ObjectRole) -> StoredObject {
        let header = PublicObjectHeader::new(repository.into(), id.into(), role, 1).unwrap();
        StoredObject {
            ciphertext_length: envelope_length(&header).unwrap(),
            header,
            locator: WireLocator { connection_identity: "account/root".into(), collection: None, object: id.into() },
            ciphertext_sha256: [2; 32],
            plaintext_length: 1,
            plaintext_sha256: [1; 32],
        }
    }

    fn record(id: &str, created_at_ms: i64) -> ExternalConflictRecord {
        ExternalConflictRecord {
            id: id.into(), created_at_ms, connection_id: "connection".into(), repository_id: "repository".into(),
            local: DurableCaptureReference {
                capture_id: format!("capture-{id}"),
                identity: CaptureIdentity { store_id: "store".into(), library_epoch: "library".into(), generation: "generation".into(), selection_epoch: "selection".into(), revision: 7 },
                catalog_path: format!("captures/{id}/capture.sqlite"), catalog_hash: "01".repeat(32),
            },
            remote: PreservedRemoteState {
                snapshot: object("repository", "snapshot-remote", ObjectRole::SyncState), logical_revision: 8, commit_id: "remote-commit".into(),
                head: PreservedHeadObservation { commit_id: "remote-commit".into(), authenticated_body_hash: "02".repeat(32) },
            },
            remote_point: None, resolved: false,
        }
    }

    #[test]
    fn local_preservation_is_idempotent_and_requires_a_confirmed_point_for_resolution() {
        let db = db();
        let input = record("conflict", 1);
        assert_eq!(conflict_capture_owner(&input.id).unwrap(), "conflict:conflict");
        assert_eq!(preserve_local_conflict(&db, &input).unwrap(), input);
        assert_eq!(preserve_local_conflict(&db, &input).unwrap(), input);
        assert!(external_conflict_for_resolution(&db, "conflict").is_err());
        let point = object("repository", "backup-point-conflict", ObjectRole::BackupPoint);
        let confirmed = confirm_external_conflict_point(&db, "conflict", &point).unwrap();
        assert_eq!(confirmed.remote_point.as_ref(), Some(&point));
        assert_eq!(confirm_external_conflict_point(&db, "conflict", &point).unwrap(), confirmed);
        assert_eq!(external_conflict_for_resolution(&db, "conflict").unwrap(), confirmed);
    }

    #[test]
    fn a_conflict_id_cannot_be_reused_for_different_preserved_bytes() {
        let db = db();
        let input = record("conflict", 1);
        preserve_local_conflict(&db, &input).unwrap();
        let mut different = input;
        different.remote.logical_revision += 1;
        assert!(preserve_local_conflict(&db, &different).is_err());
        let wrong = object("repository", "backup-point-another", ObjectRole::BackupPoint);
        assert!(confirm_external_conflict_point(&db, "conflict", &wrong).is_err());
    }

    #[test]
    fn resolved_conflicts_remain_listed_until_explicit_deletion() {
        let db = db();
        let input = record("conflict", 1);
        preserve_local_conflict(&db, &input).unwrap();
        let point = object("repository", "backup-point-conflict", ObjectRole::BackupPoint);
        confirm_external_conflict_point(&db, "conflict", &point).unwrap();
        assert!(mark_external_conflict_resolved(&db, "conflict").unwrap().resolved);
        let page = external_conflicts_page(&db, None, 50).unwrap();
        assert_eq!(page.conflicts.len(), 1);
        assert!(page.conflicts[0].resolved);
        delete_external_conflict(&db, "conflict").unwrap();
        assert!(external_conflicts_page(&db, None, 50).unwrap().conflicts.is_empty());
    }

    #[test]
    fn conflict_pages_are_bounded_and_stable() {
        let db = db();
        for n in 0..55 {
            preserve_local_conflict(&db, &record(&format!("conflict-{n:02}"), n)).unwrap();
        }
        let first = external_conflicts_page(&db, None, 50).unwrap();
        assert_eq!(first.conflicts.len(), 50);
        let second = external_conflicts_page(&db, first.next.as_ref(), 50).unwrap();
        assert_eq!(second.conflicts.len(), 5);
        assert!(second.next.is_none());
    }

    #[test]
    fn conflict_and_local_source_survive_library_database_file_replacement() {
        let root = tempfile::tempdir().unwrap();
        let external = root.path().join("external-storage");
        let capture_directory = external.join("captures/conflict");
        let object_directory = external.join("objects");
        let mut catalog = crate::external_storage::capture::CaptureCatalog::create(
            &capture_directory,
            &object_directory,
            None,
        )
        .unwrap();
        let identity = CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "library".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision: 7,
        };
        catalog.begin(&identity, None).unwrap();
        catalog.record("root", b"preserved local state").unwrap();
        catalog.finish().unwrap();
        let local = catalog
            .durable_reference("capture-conflict", root.path())
            .unwrap();

        let device_path = root.path().join("device.sqlite");
        let device = Connection::open(&device_path).unwrap();
        create_device_schema(&device).unwrap();
        let mut preserved = record("conflict", 1);
        preserved.local = local;
        preserve_local_conflict(&device, &preserved).unwrap();
        drop(device);

        let library = root.path().join("persistent.sqlite");
        std::fs::write(&library, b"old synthetic library").unwrap();
        let replacement = root.path().join("persistent.sqlite.replacement");
        std::fs::write(&replacement, b"replacement synthetic library").unwrap();
        std::fs::remove_file(&library).unwrap();
        std::fs::rename(replacement, &library).unwrap();

        let reopened = Connection::open(device_path).unwrap();
        let restored = external_conflict(&reopened, "conflict").unwrap().unwrap();
        assert_eq!(restored.local.identity, identity);
        let roots = registered_conflict_roots(&reopened, root.path()).unwrap();
        assert_eq!(roots.catalogs.len(), 1);
        assert_eq!(roots.logical_records.len(), 1);
        assert!(conflict_source_descriptor(
            &reopened,
            root.path(),
            "conflict",
            ConflictSide::Local,
        )
        .is_ok());
    }
}
