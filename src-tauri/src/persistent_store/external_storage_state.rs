//! Authoritative external publication state is committed in the library database.
use super::{
    sync_selection::{self, CaptureIdentity},
    StoreError, StoreResult,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

const SCHEMA: &str = r#"
CREATE TABLE external_storage_jobs(id TEXT PRIMARY KEY,connection_id TEXT NOT NULL,repository_id TEXT NOT NULL,capture_id TEXT NOT NULL,identity TEXT NOT NULL,role TEXT NOT NULL CHECK(role IN ('backup','sync','restore','history')),strategy TEXT CHECK(strategy IN ('cas','sequential')),expected_head TEXT,commit_id TEXT NOT NULL,phase TEXT NOT NULL CHECK(phase IN ('preparing','ready','publishing','publicationUnknown','published','historyPending','applying','complete','cancelled','stale')));
CREATE TABLE external_storage_bases(connection_id TEXT PRIMARY KEY,repository_id TEXT NOT NULL,snapshot_id TEXT NOT NULL,commit_id TEXT NOT NULL,head_observation TEXT NOT NULL,identity TEXT NOT NULL);
CREATE TABLE external_storage_captures(id TEXT PRIMARY KEY,identity TEXT NOT NULL,scope_id TEXT NOT NULL,codec_id TEXT NOT NULL,device_capture_id TEXT NOT NULL,manifest_hash TEXT NOT NULL CHECK(length(manifest_hash)=64 AND manifest_hash NOT GLOB '*[^0-9a-f]*'),UNIQUE(identity,scope_id,codec_id,device_capture_id));
CREATE TABLE external_storage_capture_refs(capture_id TEXT NOT NULL,job_id TEXT NOT NULL,PRIMARY KEY(capture_id,job_id));
CREATE TABLE external_storage_capture_files(capture_id TEXT PRIMARY KEY,catalog_path TEXT NOT NULL,file_hash TEXT NOT NULL CHECK(length(file_hash)=64 AND file_hash NOT GLOB '*[^0-9a-f]*'));
CREATE TABLE external_storage_content_cache(consumer_id TEXT NOT NULL,generation TEXT NOT NULL,kind TEXT NOT NULL,key1 TEXT NOT NULL,key2 TEXT NOT NULL,content_hash TEXT NOT NULL CHECK(length(content_hash)=64 AND content_hash NOT GLOB '*[^0-9a-f]*'),byte_size INTEGER NOT NULL CHECK(byte_size>=0),PRIMARY KEY(consumer_id,generation,kind,key1,key2));
"#;

pub(super) fn create_schema(db: &Connection) -> StoreResult<()> {
    db.execute_batch(SCHEMA)?;
    Ok(())
}

pub(super) fn validate_schema(db: &Connection) -> StoreResult<()> {
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(SCHEMA)?;
    reference.execute_batch(sync_selection::SCHEMA)?;
    let mut query = reference.prepare("SELECT name,sql FROM sqlite_master WHERE type='table'")?;
    for entry in query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (name, sql) = entry?;
        let actual: Option<String> = db
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        if actual.as_deref() != Some(&sql) {
            return Err(invalid("External operational schema is incompatible"));
        }
    }
    sync_selection::read(db)?;
    Ok(())
}
fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}

/// A verified remote snapshot selected for normal sync receive. For restore
/// jobs, capture_id identifies this remote snapshot, not a local capture/pin.
/// Network authentication and full staged payload validation precede activation.
pub(crate) struct ReceiveIntent<'a> {
    pub job_id: &'a str,
    pub connection_id: &'a str,
    pub repository_id: &'a str,
    pub snapshot_id: &'a str,
    pub commit_id: &'a str,
    pub authenticated_head: &'a str,
    pub identity: &'a CaptureIdentity,
}

pub(crate) fn prepare_receive(tx: &Transaction<'_>, intent: &ReceiveIntent<'_>) -> StoreResult<()> {
    sync_selection::require_publish(tx, intent.identity, intent.connection_id)?;
    if sync_selection::identity(tx)? != *intent.identity {
        return Err(invalid("Local revision changed before remote receive"));
    }
    sync_selection::require_no_pending_publication(tx)?;
    if [
        intent.job_id,
        intent.repository_id,
        intent.snapshot_id,
        intent.commit_id,
        intent.authenticated_head,
    ]
    .iter()
    .any(|value| value.is_empty())
    {
        return Err(invalid("Incomplete remote receive intent"));
    }
    let busy: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_storage_jobs WHERE connection_id=?1 AND phase NOT IN ('complete','cancelled','stale','historyPending'))",
        [intent.connection_id], |r| r.get(0),
    )?;
    if busy {
        return Err(invalid("Destination already has an active job"));
    }
    tx.execute(
        "INSERT INTO external_storage_jobs VALUES(?1,?2,?3,?4,?5,'restore',NULL,?6,?7,'ready')",
        params![
            intent.job_id,
            intent.connection_id,
            intent.repository_id,
            intent.snapshot_id,
            serde_json::to_string(intent.identity)?,
            intent.authenticated_head,
            intent.commit_id
        ],
    )?;
    Ok(())
}

/// Called only inside the SAME transaction that switches the staged generation.
pub(super) fn begin_receive_activation(tx: &Transaction<'_>, job: &str) -> StoreResult<()> {
    let (connection, encoded, phase): (String,String,String) = tx.query_row(
        "SELECT connection_id,identity,phase FROM external_storage_jobs WHERE id=?1 AND role='restore'",
        [job], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    )?;
    let identity: CaptureIdentity = serde_json::from_str(&encoded)?;
    sync_selection::require_publish(tx, &identity, &connection)?;
    sync_selection::require_no_pending_publication(tx)?;
    if phase != "ready" || sync_selection::identity(tx)? != identity {
        return Err(invalid("Remote receive became stale"));
    }
    tx.execute(
        "UPDATE external_storage_jobs SET phase='applying' WHERE id=?1",
        [job],
    )?;
    Ok(())
}

pub(super) fn finish_receive_activation(tx: &Transaction<'_>, job: &str) -> StoreResult<()> {
    let (connection,repository,snapshot,commit,observation): (String,String,String,String,String) = tx.query_row(
        "SELECT connection_id,repository_id,capture_id,commit_id,expected_head FROM external_storage_jobs WHERE id=?1 AND role='restore' AND phase='applying'",
        [job], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
    )?;
    let identity = serde_json::to_string(&sync_selection::identity(tx)?)?;
    tx.execute("INSERT INTO external_storage_bases VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(connection_id) DO UPDATE SET repository_id=excluded.repository_id,snapshot_id=excluded.snapshot_id,commit_id=excluded.commit_id,head_observation=excluded.head_observation,identity=excluded.identity",
        params![connection,repository,snapshot,commit,observation,identity])?;
    tx.execute(
        "UPDATE external_storage_jobs SET phase='complete' WHERE id=?1",
        [job],
    )?;
    // Local-only immutable backup captures remain valid, but any other prepared
    // sync job was based on the generation just replaced.
    tx.execute("UPDATE external_storage_jobs SET phase='stale' WHERE id!=?1 AND role!='backup' AND phase IN ('preparing','ready')", [job])?;
    Ok(())
}

pub(crate) struct PublishIntent<'a> {
    pub job_id: &'a str,
    pub connection_id: &'a str,
    pub repository_id: &'a str,
    pub capture_id: &'a str,
    pub identity: &'a CaptureIdentity,
    pub strategy: &'a str,
    pub expected_head: Option<&'a str>,
    pub commit_id: &'a str,
}

/// Capture files and their manifest must already be verified and fsynced. The
/// transaction binds their immutable reference, content cursor and consumer.
pub(crate) fn register_capture(
    tx: &Transaction<'_>,
    id: &str,
    identity: &CaptureIdentity,
    scope: &str,
    codec: &str,
    device: &str,
    manifest_hash: &str,
    consumer: &str,
) -> StoreResult<String> {
    let current = sync_selection::identity(tx)?;
    if identity.store_id != current.store_id
        || identity.library_epoch != current.library_epoch
        || identity.generation != current.generation
        || identity.selection_epoch != current.selection_epoch
        || identity.revision > current.revision
    {
        return Err(invalid("Capture identity changed"));
    }
    let encoded = serde_json::to_string(identity)?;
    let existing:Option<(String,String)>=tx.query_row("SELECT id,manifest_hash FROM external_storage_captures WHERE identity=?1 AND scope_id=?2 AND codec_id=?3 AND device_capture_id=?4",params![encoded,scope,codec,device],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let capture_id = if let Some((existing, hash)) = existing {
        if hash != manifest_hash {
            return Err(invalid("Shared capture manifest differs"));
        }
        existing
    } else {
        tx.execute(
            "INSERT INTO external_storage_captures VALUES(?1,?2,?3,?4,?5,?6)",
            params![id, encoded, scope, codec, device, manifest_hash],
        )?;
        id.into()
    };
    super::content_change_index::commit_cursor(
        tx,
        consumer,
        &identity.generation,
        identity.revision,
    )?;
    tx.execute("DELETE FROM content_capture_reservations WHERE id=?1", [id])?;
    Ok(capture_id)
}

pub(crate) fn prepare_backup(
    tx: &Transaction<'_>,
    job: &str,
    connection: &str,
    repository: &str,
    capture: &str,
    point: &str,
) -> StoreResult<()> {
    let identity: String = tx.query_row(
        "SELECT identity FROM external_storage_captures WHERE id=?1",
        [capture],
        |r| r.get(0),
    )?;
    let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM external_storage_jobs WHERE connection_id=?1 AND phase NOT IN ('complete','cancelled','stale'))",[connection],|r|r.get(0))?;
    if busy {
        return Err(invalid("External destination already has an active job"));
    }
    tx.execute(
        "INSERT INTO external_storage_jobs VALUES(?1,?2,?3,?4,?5,'backup',NULL,NULL,?6,'ready')",
        params![job, connection, repository, capture, identity, point],
    )?;
    tx.execute(
        "INSERT INTO external_storage_capture_refs VALUES(?1,?2)",
        params![capture, job],
    )?;
    Ok(())
}

pub(crate) fn cancel_prepared(tx: &Transaction<'_>, job: &str) -> StoreResult<()> {
    if tx.execute("UPDATE external_storage_jobs SET phase='cancelled' WHERE id=?1 AND phase IN ('preparing','ready','stale')",[job])?!=1 {return Err(invalid("In-flight publication must be reconciled before cancellation"));}
    tx.execute(
        "DELETE FROM external_storage_capture_refs WHERE job_id=?1",
        [job],
    )?;
    Ok(())
}

pub(crate) fn capture_has_consumers(db: &Connection, capture: &str) -> StoreResult<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_storage_capture_refs WHERE capture_id=?1)",
        [capture],
        |r| r.get(0),
    )?)
}

pub(crate) fn prepare_publication(
    tx: &Transaction<'_>,
    intent: &PublishIntent<'_>,
) -> StoreResult<()> {
    sync_selection::require_publish(tx, intent.identity, intent.connection_id)?;
    let identity = serde_json::to_string(intent.identity)?;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_storage_captures WHERE id=?1 AND identity=?2)",
        params![intent.capture_id, identity],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(invalid("Publication requires a durable capture"));
    }
    let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM external_storage_jobs WHERE connection_id=?1 AND phase NOT IN ('complete','cancelled','stale'))",[intent.connection_id],|r|r.get(0))?;
    if busy {
        return Err(invalid("External destination already has an active job"));
    }
    tx.execute(
        "INSERT INTO external_storage_jobs VALUES(?1,?2,?3,?4,?5,'sync',?6,?7,?8,'ready')",
        params![
            intent.job_id,
            intent.connection_id,
            intent.repository_id,
            intent.capture_id,
            identity,
            intent.strategy,
            intent.expected_head,
            intent.commit_id
        ],
    )?;
    tx.execute(
        "INSERT INTO external_storage_capture_refs VALUES(?1,?2)",
        params![intent.capture_id, intent.job_id],
    )?;
    Ok(())
}

/// The caller owns file(false) through the head request AND this result's
/// settlement. Commit this intent before making the network request.
pub(crate) fn begin_publication(tx: &Transaction<'_>, job: &str) -> StoreResult<()> {
    let (connection, identity, phase): (String, String, String) = tx.query_row(
        "SELECT connection_id,identity,phase FROM external_storage_jobs WHERE id=?1 AND role='sync'",
        [job],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if phase != "ready" {
        return Err(invalid("Publication cannot blindly retry a head write"));
    }
    sync_selection::require_publish(tx, &serde_json::from_str(&identity)?, &connection)?;
    tx.execute(
        "UPDATE external_storage_jobs SET phase='publishing' WHERE id=?1",
        [job],
    )?;
    Ok(())
}

pub(crate) fn publication_unknown(tx: &Transaction<'_>, job: &str) -> StoreResult<()> {
    if tx.execute("UPDATE external_storage_jobs SET phase='publicationUnknown' WHERE id=?1 AND phase='publishing'",[job])?!=1 { return Err(invalid("No in-flight publication")); }
    Ok(())
}

/// Call only after verifying the remote commit and authenticated head, never on
/// the strength of a local receipt file alone. Capture revision preserves R+1.
pub(crate) fn confirm_publication(
    tx: &Transaction<'_>,
    job: &str,
    commit: &str,
    snapshot: &str,
    observation: &str,
    history_pending: bool,
) -> StoreResult<()> {
    let (connection,repository,identity,expected,phase):(String,String,String,String,String)=tx.query_row("SELECT connection_id,repository_id,identity,commit_id,phase FROM external_storage_jobs WHERE id=?1",[job],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    if expected != commit || !matches!(phase.as_str(), "publishing" | "publicationUnknown") {
        return Err(invalid(
            "Remote confirmation differs from publication intent",
        ));
    }
    sync_selection::require_publish(tx, &serde_json::from_str(&identity)?, &connection)?;
    tx.execute("INSERT INTO external_storage_bases VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(connection_id) DO UPDATE SET repository_id=excluded.repository_id,snapshot_id=excluded.snapshot_id,commit_id=excluded.commit_id,head_observation=excluded.head_observation,identity=excluded.identity",params![connection,repository,snapshot,commit,observation,identity])?;
    tx.execute(
        "UPDATE external_storage_jobs SET phase=?1 WHERE id=?2",
        params![
            if history_pending {
                "historyPending"
            } else {
                "complete"
            },
            job
        ],
    )?;
    if !history_pending {
        tx.execute(
            "DELETE FROM external_storage_capture_refs WHERE job_id=?1",
            [job],
        )?;
    }
    Ok(())
}

pub(crate) fn finish_history(tx: &Transaction<'_>, job: &str) -> StoreResult<()> {
    if tx.execute(
        "UPDATE external_storage_jobs SET phase='complete' WHERE id=?1 AND phase='historyPending'",
        [job],
    )? != 1
    {
        return Err(invalid("No pending history point"));
    }
    tx.execute(
        "DELETE FROM external_storage_capture_refs WHERE job_id=?1",
        [job],
    )?;
    Ok(())
}
