//! Local operational authority. Never exported as library content or connection QR.
use super::{active_generation, current_revision, StoreError, StoreResult};
use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};

pub(super) const SCHEMA: &str = r#"
CREATE TABLE library_sync_selection(singleton INTEGER PRIMARY KEY CHECK(singleton=1),target TEXT NOT NULL CHECK(target IN ('none','server','external')),connection_id TEXT,selection_epoch TEXT NOT NULL,paused INTEGER NOT NULL CHECK(paused IN (0,1)),decision_required INTEGER NOT NULL CHECK(decision_required IN (0,1)),CHECK((target='none' AND connection_id IS NULL) OR (target!='none' AND length(connection_id)>0)));
CREATE TABLE local_library_identity(singleton INTEGER PRIMARY KEY CHECK(singleton=1),store_id TEXT NOT NULL,library_epoch TEXT NOT NULL);
"#;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CaptureIdentity {
    pub store_id: String,
    pub library_epoch: String,
    pub generation: String,
    pub selection_epoch: String,
    // Decimal strings are used by the UI bridge; SQLite uses signed 64-bit integers.
    pub revision: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "connectionId", rename_all = "lowercase")]
pub(crate) enum SyncTarget {
    None,
    Server(String),
    External(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub target: SyncTarget,
    pub epoch: String,
    pub paused: bool,
    pub decision_required: bool,
}

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}

pub(super) fn create_schema(db: &Connection) -> StoreResult<()> {
    db.execute_batch(SCHEMA)?;
    db.execute(
        "INSERT INTO library_sync_selection VALUES(1,'none',NULL,?1,0,0)",
        [uuid::Uuid::new_v4().to_string()],
    )?;
    db.execute(
        "INSERT INTO local_library_identity VALUES(1,?1,?2)",
        params![
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string()
        ],
    )?;
    Ok(())
}

pub(crate) fn read(db: &Connection) -> StoreResult<Selection> {
    let (kind,id,epoch,paused,decision):(String,Option<String>,String,bool,bool)=db.query_row("SELECT target,connection_id,selection_epoch,paused,decision_required FROM library_sync_selection WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    let target = match (kind.as_str(), id) {
        ("none", None) => SyncTarget::None,
        ("server", Some(id)) => SyncTarget::Server(id),
        ("external", Some(id)) => SyncTarget::External(id),
        _ => return Err(invalid("Invalid sync selection")),
    };
    Ok(Selection {
        target,
        epoch,
        paused,
        decision_required: decision,
    })
}

pub(crate) fn identity(db: &Connection) -> StoreResult<CaptureIdentity> {
    let (store_id, library_epoch) = db.query_row(
        "SELECT store_id,library_epoch FROM local_library_identity WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(CaptureIdentity {
        store_id,
        library_epoch,
        generation: active_generation(db)?,
        selection_epoch: read(db)?.epoch,
        revision: current_revision(db)?,
    })
}

/// Selection changes require the existing exclusive replacement reservation/permit.
/// The caller must settle server operations before entering this local transaction.
pub(crate) fn select(
    tx: &Transaction<'_>,
    expected_epoch: &str,
    target: &SyncTarget,
) -> StoreResult<Selection> {
    if read(tx)?.epoch != expected_epoch {
        return Err(invalid("Sync selection changed"));
    }
    require_no_pending_publication(tx)?;
    let (kind, id) = match target {
        SyncTarget::None => ("none", None),
        SyncTarget::Server(id) => ("server", Some(id)),
        SyncTarget::External(id) => ("external", Some(id)),
    };
    if id.is_some_and(|id| id.is_empty()) {
        return Err(invalid("Missing sync connection identity"));
    }
    tx.execute("UPDATE library_sync_selection SET target=?1,connection_id=?2,selection_epoch=?3,paused=0,decision_required=0 WHERE singleton=1",params![kind,id,uuid::Uuid::new_v4().to_string()])?;
    tx.execute(
        "UPDATE external_storage_jobs SET phase='stale' WHERE role IN ('sync','restore') AND phase IN ('preparing','ready')",
        [],
    )?;
    read(tx)
}

pub(crate) fn require_no_pending_publication(db: &Connection) -> StoreResult<()> {
    let pending:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM external_storage_jobs WHERE phase='applying') OR EXISTS(SELECT 1 FROM server_sync_operation)",[],|r|r.get(0))?;
    if pending {
        return Err(invalid(
            "Unsettled sync operation blocks library replacement",
        ));
    }
    Ok(())
}

pub(crate) fn require_publish(
    db: &Connection,
    capture: &CaptureIdentity,
    connection: &str,
) -> StoreResult<()> {
    require_publish_with_pause(db, capture, connection, false)
}

/// Exit drain may flush the selected paused target without changing the
/// user's durable pause intent. Native code validates the live exit session
/// immediately before every call to this override.
pub(crate) fn require_publish_exit_drain(
    db: &Connection,
    capture: &CaptureIdentity,
    connection: &str,
) -> StoreResult<()> {
    require_publish_with_pause(db, capture, connection, true)
}

fn require_publish_with_pause(
    db: &Connection,
    capture: &CaptureIdentity,
    connection: &str,
    allow_paused: bool,
) -> StoreResult<()> {
    let current = identity(db)?;
    let selection = read(db)?;
    if current.store_id != capture.store_id
        || current.library_epoch != capture.library_epoch
        || current.generation != capture.generation
        || current.selection_epoch != capture.selection_epoch
        || capture.revision > current.revision
        || capture.revision < 0
    {
        return Err(invalid("Stale external capture"));
    }
    if selection.target != SyncTarget::External(connection.into())
        || (selection.paused && !allow_paused)
        || selection.decision_required
    {
        return Err(invalid("External sync target is not active"));
    }
    Ok(())
}

pub(super) fn replaced(tx: &Transaction<'_>) -> StoreResult<()> {
    require_no_pending_publication(tx)?;
    tx.execute(
        "UPDATE local_library_identity SET library_epoch=?1 WHERE singleton=1",
        [uuid::Uuid::new_v4().to_string()],
    )?;
    tx.execute(
        "UPDATE library_sync_selection SET decision_required=1 WHERE target='external'",
        [],
    )?;
    tx.execute(
        "UPDATE external_storage_jobs SET phase='stale' WHERE role!='backup' AND phase IN ('preparing','ready')",
        [],
    )?;
    Ok(())
}

pub(crate) fn require_server(db: &Connection) -> StoreResult<()> {
    let selected = read(db)?;
    if !matches!(selected.target, SyncTarget::Server(_)) || selected.paused {
        return Err(invalid("Server sync target is not active"));
    }
    Ok(())
}

pub(super) fn restored_copy(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute(
        "UPDATE local_library_identity SET store_id=?1,library_epoch=?2 WHERE singleton=1",
        params![
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string()
        ],
    )?;
    tx.execute("UPDATE library_sync_selection SET selection_epoch=?1,decision_required=1 WHERE singleton=1",[uuid::Uuid::new_v4().to_string()])?;
    tx.execute("UPDATE external_storage_jobs SET phase='stale' WHERE phase NOT IN ('complete','cancelled')",[])?;
    tx.execute("UPDATE content_change_consumers SET rebuild_required=1", [])?;
    tx.execute("DELETE FROM content_change_context", [])?;
    Ok(())
}
