//! Coalesced content changes for external consumers. Never acknowledges the server outbox.
//! Only actual mutations open a context; staging/copy operations stay invisible.
use super::{active_generation, current_revision, PersistentStore, StoreError, StoreResult};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

/// The working set inside the WebView. Every other consumer is a connection id,
/// so the renderer never names its own consumer and cannot move another cursor.
pub(crate) const WORKING_SET_CONSUMER: &str = "ui-working-set";

const SCHEMA: &str = r#"
CREATE TABLE content_change_context(singleton INTEGER PRIMARY KEY CHECK(singleton=1),generation TEXT NOT NULL,revision INTEGER NOT NULL CHECK(revision>=0),origin TEXT NOT NULL CHECK(origin IN ('local','server','external')));
CREATE TABLE content_changes(generation TEXT NOT NULL,kind TEXT NOT NULL,key1 TEXT NOT NULL,key2 TEXT NOT NULL,revision INTEGER NOT NULL CHECK(revision>=0),PRIMARY KEY(generation,kind,key1,key2));
CREATE INDEX content_changes_revision ON content_changes(generation,revision);
CREATE TABLE content_change_consumers(id TEXT PRIMARY KEY,generation TEXT NOT NULL,revision INTEGER NOT NULL CHECK(revision>=0),rebuild_required INTEGER NOT NULL CHECK(rebuild_required IN (0,1)));
CREATE TABLE content_change_floor(singleton INTEGER PRIMARY KEY CHECK(singleton=1),generation TEXT NOT NULL,revision INTEGER NOT NULL CHECK(revision>=0));
INSERT INTO content_change_floor VALUES(1,'revision-0',0);
"#;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContentKey {
    pub kind: String,
    pub key1: String,
    pub key2: String,
}

/// What the renderer needs to decide between a targeted pass and a reprojection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContentChangeWindow {
    pub revision: i64,
    pub after_revision: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChangeWindow {
    Rebuild,
    Incremental { after_revision: i64 },
}

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}

fn trigger_sql(table: &str, event: &str, kind: &str, key1: &str, key2: &str) -> String {
    let row = if event == "DELETE" { "OLD" } else { "NEW" };
    let kind = kind.replace("ROW", row);
    let key1 = key1.replace("ROW", row);
    let key2 = key2.replace("ROW", row);
    format!("CREATE TRIGGER content_change_{table}_{} AFTER {event} ON {table}
        WHEN EXISTS(SELECT 1 FROM content_change_context WHERE generation={row}.generation)
        BEGIN INSERT INTO content_changes(generation,kind,key1,key2,revision)
        SELECT generation,{kind},{key1},{key2},revision FROM content_change_context WHERE singleton=1
        ON CONFLICT(generation,kind,key1,key2) DO UPDATE SET revision=excluded.revision; END", event.to_lowercase())
}

pub(super) fn create_schema(db: &Connection) -> StoreResult<()> {
    db.execute_batch(SCHEMA)?;
    for (table, kind, key1, key2) in super::content_locators::tracked_tables() {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            db.execute_batch(&trigger_sql(table, event, kind, key1, key2))?;
        }
    }
    Ok(())
}

pub(super) fn validate_schema(db: &Connection) -> StoreResult<()> {
    // Compare the complete definitions, including constraints and tracking expressions.
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(SCHEMA)?;
    let mut definitions =
        reference.prepare("SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL")?;
    for entry in definitions.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (kind, name, sql) = entry?;
        let actual: Option<String> = db
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
                params![kind, name],
                |r| r.get(0),
            )
            .optional()?;
        if actual.as_deref() != Some(&sql) {
            return Err(invalid("Content change schema is incompatible"));
        }
    }
    for (table, kind, key1, key2) in super::content_locators::tracked_tables() {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            let name = format!("content_change_{table}_{}", event.to_lowercase());
            let actual: Option<String> = db
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='trigger' AND name=?1",
                    [name],
                    |r| r.get(0),
                )
                .optional()?;
            if actual.as_deref() != Some(&trigger_sql(table, event, kind, key1, key2)) {
                return Err(invalid("Content change tracking is incompatible"));
            }
        }
    }
    Ok(())
}

pub(super) fn begin_mutation(
    tx: &Transaction<'_>,
    generation: &str,
    revision: i64,
    origin: &str,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO content_change_context VALUES(1,?1,?2,?3)",
        params![generation, revision, origin],
    )?;
    Ok(())
}

pub(super) fn finish_mutation(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute("DELETE FROM content_change_context", [])?;
    Ok(())
}

pub(super) fn full_replacement(
    tx: &Transaction<'_>,
    generation: &str,
    revision: i64,
) -> StoreResult<()> {
    tx.execute("DELETE FROM content_changes", [])?;
    tx.execute("UPDATE content_change_consumers SET rebuild_required=1", [])?;
    tx.execute(
        "UPDATE content_change_floor SET generation=?1,revision=?2 WHERE singleton=1",
        params![generation, revision],
    )?;
    Ok(())
}

/// Must be read through the same lease as the content projected for this window.
pub(crate) fn window(
    lease: &super::RevisionReadLease,
    consumer: &str,
) -> StoreResult<ChangeWindow> {
    let db = &lease.connection;
    let generation = active_generation(db)?;
    let revision = current_revision(db)?;
    let floor: (String, i64) = db.query_row(
        "SELECT generation,revision FROM content_change_floor WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let cursor: Option<(String, i64, bool)> = db
        .query_row(
            "SELECT generation,revision,rebuild_required FROM content_change_consumers WHERE id=?1",
            [consumer],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match cursor {
        Some((g, r, false))
            if g == generation && floor.0 == generation && r >= floor.1 && r <= revision =>
        {
            ChangeWindow::Incremental { after_revision: r }
        }
        _ => ChangeWindow::Rebuild,
    })
}

/// Returns locators only. Callers project each locator using `lease.connection`,
/// including absence after delete and the final contents after delete/recreate.
pub(crate) fn page(
    lease: &super::RevisionReadLease,
    after_revision: i64,
    after_key: Option<&ContentKey>,
    limit: usize,
) -> StoreResult<Vec<ContentKey>> {
    if limit == 0 || limit > 1024 || after_revision < 0 {
        return Err(invalid("Invalid content change page"));
    }
    let db = &lease.connection;
    let generation = active_generation(db)?;
    let revision = current_revision(db)?;
    let (floor_generation, floor): (String, i64) = db.query_row(
        "SELECT generation,revision FROM content_change_floor WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if floor_generation != generation || after_revision < floor || after_revision > revision {
        return Err(invalid("Content index rebuild required"));
    }
    let (kind, key1, key2) = after_key
        .map(|k| (k.kind.as_str(), k.key1.as_str(), k.key2.as_str()))
        .unwrap_or(("", "", ""));
    let mut query = db.prepare("SELECT kind,key1,key2 FROM content_changes WHERE generation=?1 AND revision>?2 AND revision<=?3 AND (kind,key1,key2)>(?4,?5,?6) ORDER BY kind,key1,key2 LIMIT ?7")?;
    let keys = query
        .query_map(
            params![
                generation,
                after_revision,
                revision,
                kind,
                key1,
                key2,
                limit as i64
            ],
            |r| {
                Ok(ContentKey {
                    kind: r.get(0)?,
                    key1: r.get(1)?,
                    key2: r.get(2)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(keys)
}

/// Called in the transaction that commits verified local capture references.
/// Remote publication never owns this cursor.
pub(crate) fn commit_cursor(
    tx: &Transaction<'_>,
    consumer: &str,
    generation: &str,
    revision: i64,
) -> StoreResult<()> {
    if consumer.is_empty()
        || revision < 0
        || generation != active_generation(tx)?
        || revision > current_revision(tx)?
    {
        return Err(invalid("Invalid content cursor identity"));
    }
    let floor: i64 = tx.query_row(
        "SELECT revision FROM content_change_floor WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    if revision < floor {
        return Err(invalid("Content index rebuild required"));
    }
    let old: Option<(String, i64, bool)> = tx
        .query_row(
            "SELECT generation,revision,rebuild_required FROM content_change_consumers WHERE id=?1",
            [consumer],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if old.is_some_and(|(g, r, rebuild)| g == generation && !rebuild && r > revision) {
        return Err(invalid("Content cursor cannot move backwards"));
    }
    tx.execute("INSERT INTO content_change_consumers VALUES(?1,?2,?3,0) ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,revision=excluded.revision,rebuild_required=0",params![consumer,generation,revision])?;
    Ok(())
}

pub(crate) fn require_rebuild(tx: &Transaction<'_>, consumer: &str) -> StoreResult<()> {
    tx.execute(
        "UPDATE content_change_consumers SET rebuild_required=1 WHERE id=?1",
        [consumer],
    )?;
    Ok(())
}

pub(crate) fn prune(tx: &Transaction<'_>) -> StoreResult<i64> {
    let generation = active_generation(tx)?;
    let current = current_revision(tx)?;
    let (floor_generation, old_floor): (String, i64) = tx.query_row(
        "SELECT generation,revision FROM content_change_floor WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if floor_generation != generation || old_floor > current {
        return Err(invalid("Content index floor is inconsistent"));
    }
    // These consumers already need a rebuild under the existing window rule.
    tx.execute(
        "UPDATE content_change_consumers SET rebuild_required=1 WHERE generation=?1 AND revision<?2",
        params![generation, old_floor],
    )?;
    let cutoff: i64 = tx.query_row(
        "SELECT min(revision) FROM (SELECT ?2 AS revision UNION ALL SELECT revision FROM content_change_consumers WHERE generation=?1 AND rebuild_required=0)",
        params![generation, current],
        |row| row.get(0),
    )?;
    if cutoff < old_floor || cutoff > current {
        return Err(invalid("Content index prune boundary is inconsistent"));
    }
    tx.execute(
        "DELETE FROM content_changes WHERE generation=?1 AND revision<=?2",
        params![generation, cutoff],
    )?;
    tx.execute(
        "UPDATE content_change_floor SET revision=?2 WHERE singleton=1 AND generation=?1",
        params![generation, cutoff],
    )?;
    Ok(cutoff)
}

pub(crate) fn remove_connection_consumer(
    tx: &Transaction<'_>,
    connection: &str,
) -> StoreResult<()> {
    if connection.is_empty() || connection == WORKING_SET_CONSUMER {
        return Err(invalid("Invalid content consumer connection"));
    }
    tx.execute(
        "DELETE FROM content_change_consumers WHERE id=?1",
        [connection],
    )?;
    prune(tx)?;
    Ok(())
}

impl PersistentStore {
    /// The window and every record reprojected for it come from one lease, so the
    /// change list and the content that explains it share a revision.
    pub(crate) fn working_set_change_window(
        &self,
        lease: &str,
    ) -> StoreResult<ContentChangeWindow> {
        let reader = self.working_set_reader(lease)?;
        Ok(ContentChangeWindow {
            revision: reader.target.revision,
            after_revision: match window(reader, WORKING_SET_CONSUMER)? {
                ChangeWindow::Rebuild => None,
                ChangeWindow::Incremental { after_revision } => Some(after_revision),
            },
        })
    }

    pub(crate) fn working_set_change_page(
        &self,
        lease: &str,
        after_revision: i64,
        after_key: Option<ContentKey>,
        limit: usize,
    ) -> StoreResult<Vec<ContentKey>> {
        let reader = self.working_set_reader(lease)?;
        page(reader, after_revision, after_key.as_ref(), limit)
    }

    /// Called only once the renderer has installed the projection for `revision`.
    pub(crate) fn commit_working_set_change_cursor(&mut self, revision: i64) -> StoreResult<()> {
        let transaction = self.connection.transaction()?;
        let generation = active_generation(&transaction)?;
        commit_cursor(&transaction, WORKING_SET_CONSUMER, &generation, revision)?;
        prune(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    fn working_set_reader(&self, lease: &str) -> StoreResult<&super::RevisionReadLease> {
        self.revision_leases
            .get(lease)
            .ok_or(StoreError::SnapshotReleased)
    }
}
