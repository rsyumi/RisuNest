//! Per-device state that belongs to this installation. A library snapshot
//! restore replaces `persistent.sqlite` as a whole file, so anything that must
//! survive that replacement lives here instead.
use super::{StoreError, StoreResult};
use crate::server_sync::residency::AssetPolicy;
use risunest_sync_wire::Sequence;
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub(crate) const DEVICE_SCHEMA_VERSION: u32 = 1;
pub(crate) const DEVICE_DATABASE_FILE: &str = "device.sqlite";

pub(crate) mod hypa;

const SCHEMA: &str = r#"
CREATE TABLE device_meta(
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  writer_id TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  revision INTEGER NOT NULL CHECK(revision>=0)
);

CREATE TABLE device_sections(
  section TEXT PRIMARY KEY CHECK(section IN ('hypa','local-plugins')),
  max_write_clock TEXT NOT NULL DEFAULT '0',
  gc_floor TEXT NOT NULL DEFAULT '0',
  participating INTEGER NOT NULL CHECK(participating IN (0,1)),
  participation_generation TEXT NOT NULL
);

CREATE TABLE device_settings(
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE plugin_permissions(
  code_hash TEXT NOT NULL,
  permission TEXT NOT NULL,
  granted INTEGER NOT NULL CHECK(granted IN (0,1)),
  PRIMARY KEY (code_hash, permission)
);

CREATE TABLE plugin_permission_grants(
  plugin_name TEXT NOT NULL,
  permission TEXT NOT NULL,
  last_grant_at INTEGER NOT NULL,
  PRIMARY KEY (plugin_name, permission)
);

CREATE TABLE plugin_device_storage(
  owner TEXT NOT NULL,
  space TEXT NOT NULL CHECK(space IN ('string','json')),
  key TEXT NOT NULL,
  value TEXT,
  byte_size INTEGER NOT NULL CHECK(byte_size>=0),
  tombstone INTEGER NOT NULL CHECK(tombstone IN (0,1)),
  write_clock TEXT NOT NULL,
  writer_id TEXT NOT NULL,
  published_clock TEXT,
  PRIMARY KEY (owner, space, key)
);
CREATE INDEX plugin_device_storage_clock ON plugin_device_storage(write_clock);

CREATE TABLE hypa_embeddings(
  cache_key TEXT PRIMARY KEY,
  producer TEXT NOT NULL,
  model TEXT NOT NULL,
  endpoint TEXT,
  preprocess_version INTEGER NOT NULL,
  dimensions INTEGER NOT NULL CHECK(dimensions>0),
  vector BLOB,
  metadata TEXT,
  tombstone INTEGER NOT NULL CHECK(tombstone IN (0,1)),
  write_clock TEXT NOT NULL,
  writer_id TEXT NOT NULL,
  published_clock TEXT
);
CREATE INDEX hypa_embeddings_clock ON hypa_embeddings(write_clock);

CREATE TABLE device_changes(
  section TEXT NOT NULL,
  key1 TEXT NOT NULL,
  key2 TEXT NOT NULL,
  key3 TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision>=0),
  PRIMARY KEY (section, key1, key2, key3)
);
CREATE INDEX device_changes_revision ON device_changes(section, revision);

CREATE TABLE device_change_context(
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  revision INTEGER NOT NULL CHECK(revision>=0)
);

CREATE TABLE device_change_consumers(
  id TEXT NOT NULL,
  section TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision>=0),
  rebuild_required INTEGER NOT NULL CHECK(rebuild_required IN (0,1)),
  PRIMARY KEY (id, section)
);

CREATE TABLE device_remote_cursors(
  connection_id TEXT NOT NULL,
  library_lineage TEXT NOT NULL,
  section TEXT NOT NULL,
  applied_generation TEXT NOT NULL,
  applied_gc_floor TEXT NOT NULL,
  observed_max_write_clock TEXT NOT NULL,
  PRIMARY KEY (connection_id, library_lineage, section)
);

CREATE TABLE plugin_claim_sessions(
  session_id TEXT PRIMARY KEY,
  import_batch_id TEXT NOT NULL,
  owner TEXT NOT NULL,
  code_hash TEXT NOT NULL,
  runtime_instance TEXT NOT NULL,
  started_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  closed INTEGER NOT NULL CHECK(closed IN (0,1))
);

CREATE TABLE asset_residency_policy(
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  policy TEXT NOT NULL
);
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    Hypa,
    LocalPlugins,
}

impl Section {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Hypa => "hypa",
            Self::LocalPlugins => "local-plugins",
        }
    }
}

/// `participating` defaults follow the shipped selection: embeddings travel,
/// per-device plugin values do not until the user opts in.
const SECTIONS: [(Section, i64); 2] = [(Section::Hypa, 1), (Section::LocalPlugins, 0)];

/// SQL expressions use only schema-owned identifiers, never input values.
fn tracked_tables() -> [(&'static str, Section, &'static str, &'static str, &'static str); 2] {
    [
        ("hypa_embeddings", Section::Hypa, "ROW.cache_key", "''", "''"),
        (
            "plugin_device_storage",
            Section::LocalPlugins,
            "ROW.owner",
            "ROW.space",
            "ROW.key",
        ),
    ]
}

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.to_owned(),
    }
}

fn trigger_sql(
    table: &str,
    event: &str,
    section: Section,
    key1: &str,
    key2: &str,
    key3: &str,
) -> String {
    let row = if event == "DELETE" { "OLD" } else { "NEW" };
    let key1 = key1.replace("ROW", row);
    let key2 = key2.replace("ROW", row);
    let key3 = key3.replace("ROW", row);
    let section = section.as_str();
    format!(
        "CREATE TRIGGER device_change_{table}_{} AFTER {event} ON {table}
        WHEN EXISTS(SELECT 1 FROM device_change_context WHERE singleton=1)
        BEGIN INSERT INTO device_changes(section,key1,key2,key3,revision)
        SELECT '{section}',{key1},{key2},{key3},revision FROM device_change_context WHERE singleton=1
        ON CONFLICT(section,key1,key2,key3) DO UPDATE SET revision=excluded.revision; END",
        event.to_lowercase()
    )
}

fn create_triggers(db: &Connection) -> StoreResult<()> {
    for (table, section, key1, key2, key3) in tracked_tables() {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            db.execute_batch(&trigger_sql(table, event, section, key1, key2, key3))?;
        }
    }
    Ok(())
}

fn definitions(db: &Connection) -> StoreResult<Vec<(String, String, String)>> {
    let mut statement = db.prepare(
        "SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name",
    )?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn sequence(value: &str) -> StoreResult<Sequence> {
    Sequence::try_from(value.to_owned()).map_err(|_| invalid("device write clock is invalid"))
}

pub(crate) struct DeviceStore {
    connection: Connection,
}

impl DeviceStore {
    pub(crate) fn open(persistent_dir: &Path) -> StoreResult<Self> {
        let path = persistent_dir.join(DEVICE_DATABASE_FILE);
        let mut connection = Connection::open(path)?;
        connection.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA busy_timeout = 5000;
            PRAGMA foreign_keys = OFF;
            ",
        )?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => create_schema(&mut connection)?,
            DEVICE_SCHEMA_VERSION => validate_schema(&connection)?,
            _ => {
                return Err(StoreError::Store {
                    message: format!("unsupported device schema version {version}"),
                })
            }
        }
        Ok(Self { connection })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn writer_id(&self) -> StoreResult<String> {
        Ok(self.connection.query_row(
            "SELECT writer_id FROM device_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn asset_residency_policy(&self) -> StoreResult<AssetPolicy> {
        let value: String = self.connection.query_row(
            "SELECT policy FROM asset_residency_policy WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        match value.as_str() {
            "full" => Ok(AssetPolicy::Full),
            "remote" => Ok(AssetPolicy::Remote),
            _ => Err(invalid("asset residency policy is invalid")),
        }
    }

    pub(crate) fn set_asset_residency_policy(&self, policy: AssetPolicy) -> StoreResult<()> {
        self.connection.execute(
            "UPDATE asset_residency_policy SET policy=?1 WHERE singleton=1",
            [match policy {
                AssetPolicy::Full => "full",
                AssetPolicy::Remote => "remote",
            }],
        )?;
        Ok(())
    }

    pub(crate) fn transaction(&mut self) -> StoreResult<Transaction<'_>> {
        Ok(self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?)
    }

    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
}

fn now_ms() -> StoreResult<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("device clock is before the epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| invalid("device clock is out of range"))
}

fn create_schema(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let objects: i64 = transaction.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get(0)
    })?;
    if objects != 0 {
        return Err(StoreError::Store {
            message: "device database is not a supported format".to_owned(),
        });
    }
    transaction.execute_batch(SCHEMA)?;
    create_triggers(&transaction)?;
    transaction.execute(
        "INSERT INTO device_meta (singleton, writer_id, created_at, revision) VALUES (1,?1,?2,0)",
        params![Uuid::new_v4().to_string(), now_ms()?],
    )?;
    for (section, participating) in SECTIONS {
        transaction.execute(
            "INSERT INTO device_sections
                (section, max_write_clock, gc_floor, participating, participation_generation)
                VALUES (?1,'0','0',?2,'0')",
            params![section.as_str(), participating],
        )?;
    }
    transaction.execute(
        "INSERT INTO asset_residency_policy (singleton, policy) VALUES (1,'full')",
        [],
    )?;
    validate_schema(&transaction)?;
    transaction.pragma_update(None, "user_version", DEVICE_SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn validate_schema(db: &Connection) -> StoreResult<()> {
    // Compare the complete definitions, including constraints and tracking expressions.
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(SCHEMA)?;
    create_triggers(&reference)?;
    if definitions(db)? != definitions(&reference)? {
        return Err(invalid("Device schema is incompatible"));
    }
    let meta: i64 = db.query_row(
        "SELECT count(*) FROM device_meta WHERE singleton=1 AND length(writer_id)>0",
        [],
        |row| row.get(0),
    )?;
    let sections: i64 = db.query_row("SELECT count(*) FROM device_sections", [], |row| row.get(0))?;
    let policy: i64 = db.query_row(
        "SELECT count(*) FROM asset_residency_policy WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    if meta != 1 || sections != SECTIONS.len() as i64 || policy != 1 {
        return Err(invalid("Device control rows are invalid"));
    }
    Ok(())
}

/// Opens the change context for one mutation and returns the revision the
/// triggers stamp. Staging and copy work runs outside a context and stays
/// invisible to the change index.
pub(crate) fn begin_mutation(tx: &Transaction<'_>) -> StoreResult<i64> {
    tx.execute(
        "UPDATE device_meta SET revision=revision+1 WHERE singleton=1",
        [],
    )?;
    let revision: i64 = tx.query_row(
        "SELECT revision FROM device_meta WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO device_change_context (singleton, revision) VALUES (1,?1)",
        [revision],
    )?;
    Ok(revision)
}

pub(crate) fn finish_mutation(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute("DELETE FROM device_change_context", [])?;
    Ok(())
}

/// Issues the next write clock for a section. The caller commits the value, the
/// writer identity and the change index in this same transaction.
pub(crate) fn issue_write_clock(tx: &Transaction<'_>, section: Section) -> StoreResult<Sequence> {
    let current: String = tx.query_row(
        "SELECT max_write_clock FROM device_sections WHERE section=?1",
        [section.as_str()],
        |row| row.get(0),
    )?;
    let next = sequence(&current)?
        .next()
        .map_err(|_| invalid("device write clock is exhausted"))?;
    tx.execute(
        "UPDATE device_sections SET max_write_clock=?1 WHERE section=?2",
        params![next.as_str(), section.as_str()],
    )?;
    Ok(next)
}

/// Advances the section counter past a clock observed on a remote record.
/// Recovered tombstones are included, so the counter never moves backwards.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn observe_remote_clock(
    tx: &Transaction<'_>,
    section: Section,
    observed: &Sequence,
) -> StoreResult<Sequence> {
    let current: String = tx.query_row(
        "SELECT max_write_clock FROM device_sections WHERE section=?1",
        [section.as_str()],
        |row| row.get(0),
    )?;
    let current = sequence(&current)?;
    if *observed <= current {
        return Ok(current);
    }
    tx.execute(
        "UPDATE device_sections SET max_write_clock=?1 WHERE section=?2",
        params![observed.as_str(), section.as_str()],
    )?;
    Ok(observed.clone())
}

#[cfg(test)]
mod tests;
