//! Durable server-replica bookkeeping. Mutation triggers run only during the
//! actual write phase, after generation copying, inside the same PDS transaction.
use super::{StoreError, StoreResult};
use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};

const SCHEMA: &str = r#"
CREATE TABLE server_sync_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1),config TEXT NOT NULL,head TEXT,next_sequence TEXT NOT NULL DEFAULT '1',full_scan INTEGER NOT NULL DEFAULT 1,registration_required INTEGER NOT NULL DEFAULT 0,reconciling INTEGER NOT NULL DEFAULT 0);
CREATE TABLE server_sync_context(singleton INTEGER PRIMARY KEY CHECK(singleton=1),generation TEXT NOT NULL,revision INTEGER NOT NULL);
CREATE TABLE server_sync_dirty(kind TEXT NOT NULL,key1 TEXT NOT NULL,key2 TEXT NOT NULL,revision INTEGER NOT NULL,PRIMARY KEY(kind,key1,key2));
CREATE TABLE server_sync_base(key TEXT PRIMARY KEY,version TEXT NOT NULL,local_hash TEXT);
CREATE TABLE server_sync_scope_base(scope TEXT PRIMARY KEY,version TEXT NOT NULL);
CREATE TABLE server_sync_scope_clear_base(scope TEXT PRIMARY KEY,version TEXT NOT NULL);
CREATE TABLE server_sync_clears(id TEXT PRIMARY KEY,revision INTEGER NOT NULL,expected_version TEXT);
CREATE TABLE server_sync_clear_members(clear_id TEXT NOT NULL REFERENCES server_sync_clears(id) ON DELETE CASCADE,key TEXT NOT NULL,PRIMARY KEY(clear_id,key));
CREATE TABLE server_sync_operation(singleton INTEGER PRIMARY KEY CHECK(singleton=1),sequence TEXT NOT NULL,intent TEXT NOT NULL,phase TEXT NOT NULL,local_revision INTEGER NOT NULL);
CREATE TABLE server_sync_objects(hash TEXT PRIMARY KEY,size INTEGER NOT NULL,path TEXT NOT NULL);
CREATE TABLE server_sync_operation_records(key TEXT PRIMARY KEY,version TEXT NOT NULL,local_hash TEXT,kind TEXT NOT NULL,key1 TEXT NOT NULL,key2 TEXT NOT NULL,revision INTEGER NOT NULL);
CREATE TABLE server_sync_operation_pages(page INTEGER PRIMARY KEY,body BLOB NOT NULL);
CREATE TABLE server_sync_operation_scopes(scope TEXT PRIMARY KEY,version TEXT NOT NULL);
CREATE TABLE server_sync_remote(key TEXT PRIMARY KEY,version TEXT NOT NULL);
CREATE TABLE server_sync_remote_dirty(key TEXT PRIMARY KEY);
CREATE TABLE server_sync_remote_cursor(singleton INTEGER PRIMARY KEY CHECK(singleton=1),head TEXT NOT NULL,cursor TEXT,complete INTEGER NOT NULL DEFAULT 0);
"#;

use super::content_locators::tracked_tables;
fn trigger_sql(table: &str, event: &str, kind: &str, key1: &str, key2: &str) -> String {
    let row = if event == "DELETE" { "OLD" } else { "NEW" };
    let kind = kind.replace("ROW", row);
    let key1 = key1.replace("ROW", row);
    let key2 = key2.replace("ROW", row);
    format!(
        "CREATE TRIGGER server_sync_{table}_{} AFTER {event} ON {table}
        WHEN EXISTS(SELECT 1 FROM server_sync_context WHERE generation={row}.generation)
        BEGIN INSERT INTO server_sync_dirty(kind,key1,key2,revision)
        SELECT {kind},{key1},{key2},revision FROM server_sync_context WHERE singleton=1
        ON CONFLICT(kind,key1,key2) DO UPDATE SET revision=excluded.revision; END",
        event.to_lowercase()
    )
}
pub(super) fn create_schema(db: &Connection) -> StoreResult<()> {
    db.execute_batch(SCHEMA)?;
    for (table, kind, key1, key2) in tracked_tables() {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            db.execute_batch(&trigger_sql(table, event, kind, key1, key2))?;
        }
    }
    Ok(())
}
pub(super) fn validate_schema(db: &Connection) -> StoreResult<()> {
    let reconciliation: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('server_sync_state') WHERE name='reconciling' AND type='INTEGER' AND \"notnull\"=1)", [], |r|r.get(0))?;
    if !reconciliation {
        return Err(StoreError::Validation {
            message: "Server sync replica schema is incompatible".into(),
        });
    }
    for name in [
        "server_sync_state",
        "server_sync_context",
        "server_sync_dirty",
        "server_sync_base",
        "server_sync_scope_base",
        "server_sync_scope_clear_base",
        "server_sync_clears",
        "server_sync_clear_members",
        "server_sync_operation",
        "server_sync_objects",
        "server_sync_operation_records",
        "server_sync_operation_pages",
        "server_sync_operation_scopes",
        "server_sync_remote",
        "server_sync_remote_dirty",
        "server_sync_remote_cursor",
    ] {
        let present: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |r| r.get(0),
        )?;
        if !present {
            return Err(StoreError::Validation {
                message: "Server sync replica schema is incompatible".into(),
            });
        }
    }
    for (table, kind, key1, key2) in tracked_tables() {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            let name = format!("server_sync_{table}_{}", event.to_lowercase());
            let stored: String = db.query_row(
                "SELECT sql FROM sqlite_master WHERE type='trigger' AND name=?1",
                [name],
                |r| r.get(0),
            )?;
            if stored != trigger_sql(table, event, kind, key1, key2) {
                return Err(StoreError::Validation {
                    message: "Server sync mutation tracking is invalid".into(),
                });
            }
        }
    }
    Ok(())
}
pub(super) fn begin_mutation(
    tx: &Transaction<'_>,
    generation: &str,
    revision: i64,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO server_sync_context SELECT 1,?1,?2 FROM server_sync_state WHERE singleton=1",
        params![generation, revision],
    )?;
    Ok(())
}
pub(super) fn finish_mutation(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute("DELETE FROM server_sync_context", [])?;
    Ok(())
}
pub(super) fn full_replacement(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute("UPDATE server_sync_state SET full_scan=1", [])?;
    Ok(())
}
pub(super) fn capture_clear(tx: &Transaction<'_>, generation: &str) -> StoreResult<()> {
    let id = uuid::Uuid::new_v4().to_string();
    let inserted=tx.execute("INSERT INTO server_sync_clears SELECT ?1,revision,(SELECT version FROM server_sync_scope_base WHERE scope='plugin-storage') FROM server_sync_context WHERE singleton=1",[&id])?;
    if inserted > 0 {
        tx.execute("INSERT INTO server_sync_clear_members SELECT ?1,storage_key FROM plugin_storage WHERE generation=?2",params![id,generation])?;
    }
    Ok(())
}
pub(super) fn restored_copy(db: &Connection) -> StoreResult<()> {
    // Restored sequence/credential history cannot safely issue new operations on
    // the old device identity. Keep dirty/base data for reconciliation after registration.
    db.execute(
        "UPDATE server_sync_state SET registration_required=1,full_scan=1",
        [],
    )?;
    db.execute("DELETE FROM server_sync_context", [])?;
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServerDirtyKey {
    pub kind: String,
    pub key1: String,
    pub key2: String,
    pub revision: i64,
}
pub(crate) fn dirty_page(
    db: &Connection,
    after: Option<(&str, &str, &str)>,
    limit: usize,
) -> StoreResult<Vec<ServerDirtyKey>> {
    let (kind, key1, key2) = after.unwrap_or(("", "", ""));
    let mut statement=db.prepare("SELECT kind,key1,key2,revision FROM server_sync_dirty WHERE (kind,key1,key2)>(?1,?2,?3) ORDER BY kind,key1,key2 LIMIT ?4")?;
    let values = statement
        .query_map(params![kind, key1, key2, limit.min(1024) as i64], |r| {
            Ok(ServerDirtyKey {
                kind: r.get(0)?,
                key1: r.get(1)?,
                key2: r.get(2)?,
                revision: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(values)
}
pub(crate) fn acknowledge_keys(tx: &Transaction<'_>, keys: &[ServerDirtyKey]) -> StoreResult<()> {
    for key in keys {
        tx.execute(
            "DELETE FROM server_sync_dirty WHERE kind=?1 AND key1=?2 AND key2=?3 AND revision<=?4",
            params![key.kind, key.key1, key.key2, key.revision],
        )?;
        if key.kind == "character" {
            tx.execute("DELETE FROM server_sync_dirty WHERE kind='owner' AND key1='character-additional-assets' AND key2=?1 AND revision<=?2",params![key.key1,key.revision])?;
        }
        if key.kind == "root" {
            tx.execute("DELETE FROM server_sync_dirty WHERE kind='owner' AND key1!='character-additional-assets' AND revision<=?1",[key.revision])?;
        }
    }
    Ok(())
}
