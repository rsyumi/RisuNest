use super::{
    active_generation, current_revision, CheckpointMode, LeaseResult, SnapshotCreated,
    SnapshotInfo, StoreError, StoreResult,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Reverse,
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const LEASE_TTL_MS: i64 = 24 * 60 * 60 * 1000;
const PENDING_RESTORE_FILE: &str = "pending-restore.json";
const DATABASE_FILE: &str = "persistent.db";
const MAX_SNAPSHOTS: usize = 8;
const MIN_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
struct PendingRestore {
    path: PathBuf,
}

pub(super) fn apply_pending_restore(
    persistent_dir: &Path,
    snapshots_dir: &Path,
) -> StoreResult<()> {
    let marker = snapshots_dir.join(PENDING_RESTORE_FILE);
    if !marker.exists() {
        return Ok(());
    }

    let result = (|| -> StoreResult<()> {
        let pending: PendingRestore = serde_json::from_slice(&fs::read(&marker)?)?;
        let target = validate_snapshot_path(snapshots_dir, &pending.path)?;
        validate_restore_database(&target)?;

        let database_path = persistent_dir.join(DATABASE_FILE);
        if database_path.is_file() {
            let connection = Connection::open(&database_path)?;
            create(&connection, snapshots_dir, "pre-restore")?;
            drop(connection);
        }

        replace_database(&database_path, &target)?;
        Ok(())
    })();

    fs::remove_file(&marker)?;
    if let Err(error) = result {
        eprintln!("persistent snapshot restore skipped: {error}");
    }
    Ok(())
}

pub(super) fn sweep_temporary_generations(connection: &mut Connection) -> StoreResult<()> {
    let cutoff = now_ms() - LEASE_TTL_MS;
    let transaction = connection.transaction()?;
    transaction.execute(
        "DELETE FROM snapshot_leases WHERE created_at < ?1",
        [cutoff],
    )?;
    transaction.execute(
        "DELETE FROM snapshot_leases
         WHERE generation NOT IN (SELECT generation FROM root)",
        [],
    )?;

    let generations = ["staging-%", "snapshot-%"];
    for pattern in generations {
        let mut statement = transaction.prepare(
            "SELECT generation FROM root
             WHERE generation LIKE ?1
               AND (generation LIKE 'staging-%'
                    OR generation NOT IN (SELECT generation FROM snapshot_leases))",
        )?;
        let stale = statement
            .query_map([pattern], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for generation in stale {
            delete_generation(&transaction, &generation)?;
        }
    }
    transaction.commit()?;
    Ok(())
}

pub(super) fn acquire_revision(
    connection: &mut Connection,
    revision: i64,
) -> StoreResult<LeaseResult> {
    let transaction = connection.transaction()?;
    let actual = current_revision(&transaction)?;
    if actual != revision {
        return Err(StoreError::RevisionConflict {
            expected: revision,
            actual,
        });
    }

    let source = active_generation(&transaction)?;
    let generation = format!("snapshot-{revision}-{}", Uuid::new_v4());
    let root_exists = transaction
        .query_row(
            "SELECT 1 FROM root WHERE generation = ?1",
            [&source],
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

    transaction.execute(
        "INSERT INTO root (generation, value) SELECT ?1, value FROM root WHERE generation = ?2",
        params![generation, source],
    )?;
    for table in ["characters", "conversations", "messages"] {
        let sql = format!(
            "INSERT INTO {table} SELECT ?1, {} FROM {table} WHERE generation = ?2",
            columns_without_generation(table)
        );
        transaction.execute(&sql, params![generation, source])?;
    }
    transaction.execute(
        "INSERT INTO snapshot_leases (generation, created_at) VALUES (?1, ?2)",
        params![generation, now_ms()],
    )?;
    transaction.commit()?;
    Ok(LeaseResult { lease: generation })
}

pub(super) fn release_revision(connection: &mut Connection, lease: &str) -> StoreResult<()> {
    if !lease.starts_with("snapshot-") {
        return Err(validation("revision lease must be a snapshot generation"));
    }
    let transaction = connection.transaction()?;
    delete_generation(&transaction, lease)?;
    transaction.execute("DELETE FROM snapshot_leases WHERE generation = ?1", [lease])?;
    transaction.commit()?;
    Ok(())
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
    fs::create_dir_all(snapshots_dir)?;
    let current_bytes = logical_database_bytes(connection)?;
    let stamp: String =
        connection.query_row("SELECT strftime('%Y%m%d-%H%M%f', 'now')", [], |row| {
            row.get(0)
        })?;
    let reason = safe_name(reason);
    let path = snapshots_dir.join(format!("persistent-{stamp}-{reason}-{}.db", Uuid::new_v4()));
    let started = Instant::now();
    connection.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
    let metadata = fs::metadata(&path)?;
    let created = SnapshotCreated {
        path: path.to_string_lossy().into_owned(),
        bytes: metadata.len(),
        duration_ms: started.elapsed().as_millis() as u64,
    };
    let mut protected = vec![path];
    if let Some(target) = pending_restore_target(snapshots_dir)? {
        protected.push(target);
    }
    rotate(snapshots_dir, current_bytes, &protected)?;
    Ok(created)
}

pub(super) fn list(snapshots_dir: &Path) -> StoreResult<Vec<SnapshotInfo>> {
    let mut snapshots = Vec::new();
    for entry in fs::read_dir(snapshots_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file()
            || path.extension().and_then(|value| value.to_str()) != Some("db")
        {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified_at = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(|error| StoreError::Store {
                message: error.to_string(),
            })?
            .as_millis() as u64;
        snapshots.push(SnapshotInfo {
            path: path.to_string_lossy().into_owned(),
            bytes: metadata.len(),
            modified_at,
        });
    }
    snapshots.sort_by_key(|snapshot| Reverse(snapshot.modified_at));
    Ok(snapshots)
}

pub(super) fn restore_request(snapshots_dir: &Path, path: &Path) -> StoreResult<()> {
    let path = validate_snapshot_path(snapshots_dir, path)?;
    let marker = snapshots_dir.join(PENDING_RESTORE_FILE);
    fs::write(marker, serde_json::to_vec(&PendingRestore { path })?)?;
    Ok(())
}

fn columns_without_generation(table: &str) -> &'static str {
    match table {
        "characters" => "character_id, configured_index, recent_at, trashed, name, image, conversation_count, detail",
        "conversations" => "character_id, conversation_id, configured_index, recent_at, name, message_count, detail",
        "messages" => "character_id, conversation_id, message_index, message_id, value",
        _ => unreachable!(),
    }
}

fn delete_generation(transaction: &rusqlite::Transaction<'_>, generation: &str) -> StoreResult<()> {
    for table in ["messages", "conversations", "characters", "root"] {
        transaction.execute(
            &format!("DELETE FROM {table} WHERE generation = ?1"),
            [generation],
        )?;
    }
    Ok(())
}

fn validate_snapshot_path(snapshots_dir: &Path, path: &Path) -> StoreResult<PathBuf> {
    if path.extension().and_then(|value| value.to_str()) != Some("db") || !path.is_file() {
        return Err(validation("restore target must be a snapshot .db file"));
    }
    let snapshots_dir = fs::canonicalize(snapshots_dir)?;
    let path = fs::canonicalize(path)?;
    if path.parent() != Some(snapshots_dir.as_path()) {
        return Err(validation(
            "restore target must be directly inside the snapshots directory",
        ));
    }
    Ok(path)
}

fn logical_database_bytes(connection: &Connection) -> StoreResult<u64> {
    let page_count: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    Ok((page_count as u64).saturating_mul(page_size as u64))
}

fn pending_restore_target(snapshots_dir: &Path) -> StoreResult<Option<PathBuf>> {
    let marker = snapshots_dir.join(PENDING_RESTORE_FILE);
    if !marker.is_file() {
        return Ok(None);
    }
    let pending: PendingRestore = serde_json::from_slice(&fs::read(marker)?)?;
    Ok(Some(validate_snapshot_path(snapshots_dir, &pending.path)?))
}

fn rotate(
    snapshots_dir: &Path,
    current_database_bytes: u64,
    protected: &[PathBuf],
) -> StoreResult<()> {
    let byte_budget = current_database_bytes
        .saturating_mul(4)
        .max(MIN_SNAPSHOT_BYTES);
    let protected = protected
        .iter()
        .filter_map(|path| fs::canonicalize(path).ok())
        .collect::<HashSet<_>>();
    let mut snapshots = list(snapshots_dir)?;
    snapshots.sort_by(|left, right| {
        left.modified_at
            .cmp(&right.modified_at)
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut total = snapshots
        .iter()
        .fold(0u64, |sum, snapshot| sum.saturating_add(snapshot.bytes));

    while snapshots.len() > MAX_SNAPSHOTS || total > byte_budget {
        let Some(index) = snapshots.iter().position(|snapshot| {
            fs::canonicalize(&snapshot.path)
                .map(|path| !protected.contains(&path))
                .unwrap_or(true)
        }) else {
            break;
        };
        let snapshot = snapshots.remove(index);
        fs::remove_file(&snapshot.path)?;
        total = total.saturating_sub(snapshot.bytes);
    }
    Ok(())
}

fn validate_restore_database(path: &Path) -> StoreResult<()> {
    let connection = Connection::open(path)?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(validation("snapshot integrity check failed"));
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 1 {
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
    let next = database_path.with_extension(format!("db.restore-next-{}", Uuid::new_v4()));
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

    let previous = database_path.with_extension("db.restore-previous");
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

fn safe_name(reason: &str) -> String {
    let name: String = reason
        .chars()
        .filter(|character| {
            matches!(
                *character,
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_'
            )
        })
        .take(32)
        .collect();
    if name.is_empty() {
        "snapshot".to_owned()
    } else {
        name
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn validation(message: impl Into<String>) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}
