//! Applies a chosen repair. The change is assembled in a staged generation, re-checked by the
//! same gate that guards a backup, and only then activated in one transaction. Nothing is
//! written to the live generation until that activation, so a refused repair leaves no trace.

use super::portable::TABLES;
use super::{snapshot, PersistentStore, RevisionResult, StoreError, StoreResult};
use crate::data_health::journal::{Journal, RecordChange};
use crate::data_health::repair::{RepairAction, RepairCandidate};
use rusqlite::{types::ValueRef, Connection, TransactionBehavior};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// One step of a JSON path the reference graph emits, such as `$.enabledModules[0]`.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Step {
    /// An object member. Removing it removes the key.
    Key(String),
    /// An array element. Removing it removes the element, so several removals in one record are
    /// applied from the last index backwards.
    Index(usize),
}

fn parse_path(path: &str) -> Option<Vec<Step>> {
    let mut steps = Vec::new();
    let mut rest = path.strip_prefix('$')?;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix('.') {
            let end = tail
                .find(['.', '['])
                .unwrap_or(tail.len());
            if end == 0 {
                return None;
            }
            steps.push(Step::Key(tail[..end].to_owned()));
            rest = &tail[end..];
        } else if let Some(tail) = rest.strip_prefix('[') {
            let end = tail.find(']')?;
            steps.push(Step::Index(tail[..end].parse().ok()?));
            rest = &tail[end + 1..];
        } else {
            return None;
        }
    }
    Some(steps)
}

/// Removes what one path points at. A missing path is not an error: the record may already have
/// lost the field, and the gate judges the result either way.
fn remove_at(value: &mut Value, steps: &[Step]) {
    let Some((last, parents)) = steps.split_last() else {
        return;
    };
    let mut cursor = value;
    for step in parents {
        cursor = match step {
            Step::Key(key) => match cursor.as_object_mut().and_then(|map| map.get_mut(key)) {
                Some(next) => next,
                None => return,
            },
            Step::Index(index) => match cursor.as_array_mut().and_then(|items| items.get_mut(*index))
            {
                Some(next) => next,
                None => return,
            },
        };
    }
    match last {
        Step::Key(key) => {
            if let Some(map) = cursor.as_object_mut() {
                map.remove(key);
            }
        }
        Step::Index(index) => {
            if let Some(items) = cursor.as_array_mut() {
                if *index < items.len() {
                    items.remove(*index);
                }
            }
        }
    }
}

/// Where one record lives: the table that holds it and the identity columns that name it.
struct Located {
    table: &'static str,
    key: Vec<String>,
    column: &'static str,
}

/// Maps a finding's owner onto the row that holds it. An owner shape nothing here names cannot
/// be edited, and its candidates are refused rather than applied to the wrong record.
fn locate(owner_kind: &str, owner_id: &str) -> Option<Located> {
    let parts: Vec<String> = owner_id.split('/').map(str::to_owned).collect();
    match owner_kind {
        "root" => Some(Located {
            table: "root",
            key: Vec::new(),
            column: "value",
        }),
        "preset" => Some(Located {
            table: "bot_presets",
            key: vec![owner_id.to_owned()],
            column: "value",
        }),
        "plugin" if parts.len() == 2 => Some(Located {
            table: "plugin_storage",
            key: parts,
            column: "value",
        }),
        "character" => Some(Located {
            table: "characters",
            key: vec![owner_id.to_owned()],
            column: "detail",
        }),
        "conversation" if parts.len() == 2 => Some(Located {
            table: "conversations",
            key: parts,
            column: "detail",
        }),
        "message" if parts.len() == 3 => Some(Located {
            table: "messages",
            key: parts,
            column: "value",
        }),
        _ => None,
    }
}

fn identity_columns(table: &str) -> &'static [&'static str] {
    match table {
        "bot_presets" => &["preset_id"],
        "plugin_storage" => &["owner", "storage_key"],
        "characters" => &["character_id"],
        "conversations" => &["character_id", "conversation_id"],
        "messages" => &["character_id", "conversation_id", "message_index"],
        "asset_aliases" => &["kind", "logical_key"],
        "asset_owner_heads" => &["owner_kind", "owner_locator"],
        _ => &[],
    }
}

fn where_identity(table: &str) -> String {
    identity_columns(table)
        .iter()
        .enumerate()
        .map(|(index, column)| format!(" AND \"{column}\"=?{}", index + 2))
        .collect()
}

fn validation(message: impl Into<String>) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}

/// Copies the active generation into the staging generation. The staging rows are what the
/// repair edits, so the live generation stays untouched until the activation.
fn copy_generation(transaction: &Connection, active: &str, staging: &str) -> StoreResult<()> {
    for table in TABLES {
        transaction.execute(
            &format!(
                "INSERT OR REPLACE INTO {name} (generation, {columns}) SELECT ?1, {columns} FROM {name} WHERE generation=?2",
                name = table.name,
                columns = table.column_list(),
            ),
            rusqlite::params![staging, active],
        )?;
    }
    Ok(())
}

/// Applies every reference removal that belongs to one record, last path first so an earlier
/// removal never shifts the element a later one names.
fn drop_references(
    transaction: &Connection,
    staging: &str,
    located: &Located,
    mut paths: Vec<Vec<Step>>,
) -> StoreResult<()> {
    let sql_where = where_identity(located.table);
    let mut parameters: Vec<&dyn rusqlite::ToSql> = vec![&staging];
    for part in &located.key {
        parameters.push(part);
    }
    let serialized: Option<String> = transaction
        .query_row(
            &format!(
                "SELECT \"{}\" FROM {} WHERE generation=?1{sql_where}",
                located.column, located.table
            ),
            parameters.as_slice(),
            |row| row.get(0),
        )
        .map_err(StoreError::from)
        .map(Some)
        .or_else(|error| match error {
            StoreError::Store { .. } => Ok(None),
            error => Err(error),
        })?;
    let Some(serialized) = serialized else {
        return Ok(());
    };
    let Ok(mut value) = serde_json::from_str::<Value>(&serialized) else {
        return Ok(());
    };
    paths.sort();
    for steps in paths.into_iter().rev() {
        remove_at(&mut value, &steps);
    }
    let updated = serde_json::to_string(&value)?;
    transaction.execute(
        &format!(
            "UPDATE {} SET \"{}\"=?{} WHERE generation=?1{sql_where}",
            located.table,
            located.column,
            located.key.len() + 2
        ),
        rusqlite::params_from_iter(
            std::iter::once(staging.to_owned())
                .chain(located.key.iter().cloned())
                .chain(std::iter::once(updated)),
        ),
    )?;
    Ok(())
}

/// Recomputes the derived columns, counts and orders of one table. Every value it writes is
/// derived from a record the table already holds, so the change adds nothing and loses nothing.
fn normalize(transaction: &Connection, staging: &str, table: &str) -> StoreResult<()> {
    match table {
        "root" => {
            transaction.execute(
                "UPDATE root SET value=json_remove(value, '$.characters', '$.botPresets', '$.pluginCustomStorage', '$.pluginStorageMeta') WHERE generation=?1 AND json_valid(value)",
                [staging],
            )?;
        }
        "characters" => {
            transaction.execute(
                "UPDATE characters SET conversation_count=(SELECT count(*) FROM conversations c WHERE c.generation=characters.generation AND c.character_id=characters.character_id) WHERE generation=?1",
                [staging],
            )?;
            renumber(transaction, staging, "characters", "character_id", "configured_index")?;
            derive_json_columns(transaction, staging, "characters")?;
        }
        "conversations" => {
            transaction.execute(
                "UPDATE conversations SET message_count=(SELECT count(*) FROM messages m WHERE m.generation=conversations.generation AND m.character_id=conversations.character_id AND m.conversation_id=conversations.conversation_id) WHERE generation=?1",
                [staging],
            )?;
            renumber(
                transaction,
                staging,
                "conversations",
                "character_id, conversation_id",
                "configured_index",
            )?;
            derive_json_columns(transaction, staging, "conversations")?;
        }
        "messages" => reindex_messages(transaction, staging)?,
        "bot_presets" => {
            renumber(transaction, staging, "bot_presets", "preset_id", "configured_index")?;
            derive_json_columns(transaction, staging, "bot_presets")?;
        }
        "plugin_storage" => {
            renumber(transaction, staging, "plugin_storage", "storage_key", "ordinal")?;
            transaction.execute(
                "UPDATE plugin_storage SET byte_size=length(CAST(value AS BLOB)) WHERE generation=?1",
                [staging],
            )?;
        }
        _ => return Err(validation(format!("{table} has no derived values to recompute"))),
    }
    Ok(())
}

/// Reassigns an ordering column densely, keeping the order the records already have.
fn renumber(
    transaction: &Connection,
    staging: &str,
    table: &str,
    order: &str,
    column: &str,
) -> StoreResult<()> {
    let identity = identity_columns(table)
        .iter()
        .map(|column| format!("\"{column}\""))
        .collect::<Vec<_>>()
        .join(", ");
    transaction.execute(
        &format!(
            "UPDATE {table} SET \"{column}\"=(
                 SELECT position-1 FROM (
                     SELECT {identity}, row_number() OVER (ORDER BY \"{column}\", {order}) AS position
                     FROM {table} WHERE generation=?1
                 ) ranked WHERE ({identity})=({prefixed})
             ) WHERE generation=?1",
            prefixed = identity_columns(table)
                .iter()
                .map(|column| format!("{table}.\"{column}\""))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        [staging],
    )?;
    Ok(())
}

/// Closes the gaps in one conversation's message indexes without changing their order.
fn reindex_messages(transaction: &Connection, staging: &str) -> StoreResult<()> {
    transaction.execute(
        "CREATE TEMP TABLE repair_message_order AS
         SELECT character_id, conversation_id, message_index,
                row_number() OVER (PARTITION BY character_id, conversation_id ORDER BY message_index) - 1 AS position
         FROM messages WHERE generation=?1",
        [staging],
    )?;
    // Moving to a free range first keeps the primary key unique while the indexes shift.
    transaction.execute(
        "UPDATE messages SET message_index=-message_index-1 WHERE generation=?1",
        [staging],
    )?;
    transaction.execute(
        "UPDATE messages SET message_index=(
             SELECT position FROM repair_message_order o
             WHERE o.character_id=messages.character_id AND o.conversation_id=messages.conversation_id
               AND o.message_index=-messages.message_index-1
         ) WHERE generation=?1",
        [staging],
    )?;
    transaction.execute_batch("DROP TABLE repair_message_order")?;
    Ok(())
}

/// Rewrites the columns a record's own JSON determines, so the projection agrees with the record.
fn derive_json_columns(transaction: &Connection, staging: &str, table: &str) -> StoreResult<()> {
    let (column, fields): (&str, &[(&str, &str)]) = match table {
        "characters" => (
            "detail",
            &[
                ("name", "$.name"),
                ("image", "$.image"),
                ("type", "$.type"),
                ("creator_notes", "$.creatorNotes"),
            ],
        ),
        "conversations" => ("detail", &[("name", "$.name")]),
        "bot_presets" => ("value", &[("name", "$.name"), ("image", "$.image")]),
        _ => return Ok(()),
    };
    for (target, pointer) in fields {
        transaction.execute(
            &format!(
                "UPDATE {table} SET \"{target}\"=json_extract(\"{column}\", '{pointer}') WHERE generation=?1 AND json_valid(\"{column}\")"
            ),
            [staging],
        )?;
    }
    Ok(())
}

/// Keeps one record of a table that must hold exactly one. The kept record is the first by the
/// table's own order, so the choice does not depend on how the rows were written.
fn keep_single(transaction: &Connection, staging: &str, table: &str) -> StoreResult<()> {
    let names: &[&str] = match table {
        "root" => &["root"],
        "authority" => &["asset_repository_authority"],
        _ => return Err(validation(format!("{table} is not a single-record table"))),
    };
    for name in names {
        transaction.execute(
            &format!(
                "DELETE FROM {name} WHERE generation=?1 AND rowid NOT IN (SELECT min(rowid) FROM {name} WHERE generation=?1)"
            ),
            [staging],
        )?;
    }
    Ok(())
}

/// Gives orphans an owner again by restoring the missing container as a trashed record, so
/// nothing is deleted and the reader can decide what to do with it in the ordinary screens.
fn recover_orphans(transaction: &Connection, staging: &str, table: &str, now_ms: i64) -> StoreResult<()> {
    match table {
        "conversations" => {
            transaction.execute(
                "INSERT INTO characters (generation, character_id, configured_index, recent_at, trashed, name, image, conversation_count, type, creator_notes, trash_time, detail)
                 SELECT ?1, c.character_id, 0, 0, 1, c.character_id, NULL, 0, 'character', NULL, ?2,
                        json_object('chaId', c.character_id, 'name', c.character_id, 'type', 'character', 'trashTime', ?2, 'chats', json_array(), 'chatPage', 0)
                 FROM (SELECT DISTINCT character_id FROM conversations WHERE generation=?1) c
                 WHERE NOT EXISTS(SELECT 1 FROM characters p WHERE p.generation=?1 AND p.character_id=c.character_id)",
                rusqlite::params![staging, now_ms],
            )?;
            normalize(transaction, staging, "characters")?;
        }
        "messages" => {
            transaction.execute(
                "INSERT INTO conversations (generation, character_id, conversation_id, configured_index, recent_at, name, message_count, detail)
                 SELECT ?1, m.character_id, m.conversation_id, 0, 0, m.conversation_id, 0,
                        json_object('id', m.conversation_id, 'name', m.conversation_id, 'message', json_array())
                 FROM (SELECT DISTINCT character_id, conversation_id FROM messages WHERE generation=?1) m
                 WHERE NOT EXISTS(SELECT 1 FROM conversations c WHERE c.generation=?1 AND c.character_id=m.character_id AND c.conversation_id=m.conversation_id)",
                [staging],
            )?;
            recover_orphans(transaction, staging, "conversations", now_ms)?;
            normalize(transaction, staging, "conversations")?;
        }
        _ => return Err(validation(format!("{table} has no owner to restore"))),
    }
    Ok(())
}

fn drop_alias(transaction: &Connection, staging: &str, kind: &str, key: &str) -> StoreResult<()> {
    transaction.execute(
        "DELETE FROM asset_aliases WHERE generation=?1 AND kind=?2 AND logical_key=?3",
        rusqlite::params![staging, kind, key],
    )?;
    Ok(())
}

/// Rebinds an alias to what its stored payload actually is. The stored bytes decide, which is
/// why the preview says the payload may not be what the owner meant.
fn adopt_stored_payload(
    transaction: &Connection,
    cas: &crate::asset_repository::PayloadCas,
    staging: &str,
    kind: &str,
    key: &str,
) -> StoreResult<()> {
    let (table, filter): (&str, &str) = ("asset_aliases", "kind=?2 AND logical_key=?3");
    let parameters: Vec<String> = vec![staging.to_owned(), kind.to_owned(), key.to_owned()];
    let hash: Option<String> = transaction
        .query_row(
            &format!("SELECT object_hash FROM {table} WHERE generation=?1 AND {filter}"),
            rusqlite::params_from_iter(parameters.iter()),
            |row| row.get(0),
        )
        .map_err(StoreError::from)?;
    let Some(hash) = hash else {
        return Ok(());
    };
    let Some(size) = cas.stat_object(&hash)? else {
        return Err(validation(
            "the stored payload is absent, so there is nothing to adopt",
        ));
    };
    transaction.execute(
        &format!("UPDATE {table} SET size=?{} WHERE generation=?1 AND {filter}", parameters.len() + 1),
        rusqlite::params_from_iter(
            parameters
                .iter()
                .cloned()
                .chain(std::iter::once(size.to_string())),
        ),
    )?;
    Ok(())
}

/// Settles a storage authority by re-examining the aliases that are actually stored. The
/// compatibility hash is recomputed from those aliases, never declared.
fn settle_authority(transaction: &Connection, staging: &str, subject: &str) -> StoreResult<()> {
    let table = match subject {
        "asset" => "asset_repository_authority",
        _ => return Err(validation(format!("{subject} is not a storage authority"))),
    };
    let migration_id: String = transaction
        .query_row(
            &format!("SELECT json_extract(value,'$.migrationId') FROM {table} WHERE generation=?1"),
            [staging],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(StoreError::from)?
        .unwrap_or_else(|| format!("data-health-{}", uuid::Uuid::new_v4()));
    let hash = alias_compatibility_hash(transaction, staging)?;
    let value = serde_json::json!({
        "format": "v2",
        "migrationId": migration_id,
        "compatibilityHash": hash,
    });
    transaction.execute(
        &format!("UPDATE {table} SET value=?2 WHERE generation=?1"),
        rusqlite::params![staging, serde_json::to_string(&value)?],
    )?;
    Ok(())
}

/// Digest of the alias set a generation registers. Two libraries with the same aliases settle to
/// the same value, which is what the authority records.
fn alias_compatibility_hash(transaction: &Connection, staging: &str) -> StoreResult<String> {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for (sql, label) in [(
        "SELECT kind,logical_key,object_hash,size FROM asset_aliases WHERE generation=?1 ORDER BY kind,logical_key",
        "asset",
    )] {
        digest.update(label.as_bytes());
        let mut statement = transaction.prepare(sql)?;
        let mut rows = statement.query([staging])?;
        while let Some(row) = rows.next()? {
            for index in 0..4 {
                digest.update(
                    match row.get_ref(index)? {
                        ValueRef::Null => String::new(),
                        ValueRef::Integer(value) => value.to_string(),
                        ValueRef::Real(value) => value.to_string(),
                        ValueRef::Text(value) => String::from_utf8_lossy(value).into_owned(),
                        ValueRef::Blob(_) => "blob".to_owned(),
                    }
                    .as_bytes(),
                );
                digest.update([0]);
            }
        }
    }
    Ok(hex::encode(digest.finalize()))
}

/// Raw stored columns of one row, in the table's own column order.
type Row = Vec<Option<String>>;

fn read_row(row: &rusqlite::Row<'_>, columns: usize) -> StoreResult<Row> {
    let mut values = Vec::with_capacity(columns);
    for index in 0..columns {
        values.push(match row.get_ref(index)? {
            ValueRef::Null => None,
            ValueRef::Integer(value) => Some(value.to_string()),
            ValueRef::Real(value) => Some(value.to_string()),
            ValueRef::Text(value) => Some(String::from_utf8_lossy(value).into_owned()),
            ValueRef::Blob(value) => Some(hex::encode(value)),
        });
    }
    Ok(values)
}

fn identity_of(table: &super::portable::PortableTable, row: &Row) -> Vec<String> {
    identity_columns(table.name)
        .iter()
        .filter_map(|column| {
            table
                .columns
                .iter()
                .position(|(name, _)| name == column)
                .and_then(|index| row[index].clone())
        })
        .collect()
}

/// Reads what one generation holds and the other does not, for every table. Running it both
/// ways gives the before and after image of every record a repair changed.
fn differing_rows(
    connection: &Connection,
    generation: &str,
    other: &str,
) -> StoreResult<Vec<(&'static super::portable::PortableTable, Row)>> {
    let mut rows = Vec::new();
    for table in TABLES {
        let columns = table.column_list();
        let mut statement = connection.prepare(&format!(
            "SELECT {columns} FROM {name} WHERE generation=?1
             EXCEPT SELECT {columns} FROM {name} WHERE generation=?2",
            name = table.name,
        ))?;
        let mut cursor = statement.query(rusqlite::params![generation, other])?;
        while let Some(row) = cursor.next()? {
            rows.push((table, read_row(row, table.columns.len())?));
        }
    }
    Ok(rows)
}

/// Every record the staged generation changed, with both images. This is what an undo replays.
fn record_changes(
    connection: &Connection,
    active: &str,
    staging: &str,
) -> StoreResult<Vec<RecordChange>> {
    let mut changes: BTreeMap<(String, Vec<String>), RecordChange> = BTreeMap::new();
    for (table, row) in differing_rows(connection, active, staging)? {
        let identity = identity_of(table, &row);
        changes
            .entry((table.name.to_owned(), identity.clone()))
            .or_insert_with(|| RecordChange {
                table: table.name.to_owned(),
                identity,
                before: None,
                after: None,
            })
            .before = Some(row);
    }
    for (table, row) in differing_rows(connection, staging, active)? {
        let identity = identity_of(table, &row);
        changes
            .entry((table.name.to_owned(), identity.clone()))
            .or_insert_with(|| RecordChange {
                table: table.name.to_owned(),
                identity,
                before: None,
                after: None,
            })
            .after = Some(row);
    }
    Ok(changes.into_values().collect())
}

/// Objects the staged generation stopped referencing. They are held by the journal, so the
/// unused file cleanup leaves them alone while an undo is still possible.
fn released_objects(
    connection: &Connection,
    active: &str,
    staging: &str,
) -> StoreResult<BTreeSet<String>> {
    let mut released = BTreeSet::new();
    let mut statement = connection.prepare(
        "SELECT object_hash FROM asset_aliases
         WHERE generation=?1 AND object_hash IS NOT NULL
         EXCEPT SELECT object_hash FROM asset_aliases
         WHERE generation=?2 AND object_hash IS NOT NULL",
    )?;
    let mut rows = statement.query(rusqlite::params![active, staging])?;
    while let Some(row) = rows.next()? {
        released.insert(row.get::<_, String>(0)?);
    }
    Ok(released)
}

/// Groups the reference removals by the record that holds them, so one record is rewritten once.
fn reference_edits(
    candidates: &[RepairCandidate],
) -> StoreResult<BTreeMap<(String, String), Vec<Vec<Step>>>> {
    let mut edits: BTreeMap<(String, String), Vec<Vec<Step>>> = BTreeMap::new();
    for candidate in candidates {
        let RepairAction::DropReference {
            owner, source_path, ..
        } = &candidate.action
        else {
            continue;
        };
        let steps = parse_path(source_path)
            .ok_or_else(|| validation(format!("{source_path} is not a reference location")))?;
        edits
            .entry((owner.kind.clone(), owner.id.clone()))
            .or_default()
            .push(steps);
    }
    Ok(edits)
}

impl PersistentStore {
    /// Stages the chosen repair, re-checks it with the gate a backup uses, and activates it in
    /// one transaction. The journal it returns is what an undo replays.
    pub(crate) fn apply_repair(
        &mut self,
        expected_revision: i64,
        candidates: &[RepairCandidate],
        now_ms: i64,
    ) -> StoreResult<(RevisionResult, Journal)> {
        if candidates.is_empty() {
            return Err(validation("a repair needs at least one selected change"));
        }
        let staging = self.replace_begin()?.staging_id;
        let outcome = self.stage_and_activate(&staging, expected_revision, candidates, now_ms);
        if outcome.is_err() {
            let _ = self.replace_abort(&staging);
        }
        outcome
    }

    fn stage_and_activate(
        &mut self,
        staging: &str,
        expected_revision: i64,
        candidates: &[RepairCandidate],
        now_ms: i64,
    ) -> StoreResult<(RevisionResult, Journal)> {
        let active = super::active_generation(&self.connection)?;
        let from_revision = self.revision()?;
        if from_revision != expected_revision {
            return Err(StoreError::RevisionConflict {
                expected: expected_revision,
                actual: from_revision,
            });
        }
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        let edits = reference_edits(candidates)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        copy_generation(&transaction, &active, staging)?;
        for ((kind, id), paths) in edits {
            let located = locate(&kind, &id)
                .ok_or_else(|| validation(format!("{kind} records cannot be edited by a repair")))?;
            drop_references(&transaction, staging, &located, paths)?;
        }
        for candidate in candidates {
            match &candidate.action {
                RepairAction::DropReference { .. } => {}
                RepairAction::DropAlias { kind, key } => {
                    drop_alias(&transaction, staging, kind, key)?
                }
                RepairAction::AdoptStoredPayload { kind, key } => {
                    adopt_stored_payload(&transaction, &cas, staging, kind, key)?
                }
                RepairAction::NormalizeRecords { table } => {
                    normalize(&transaction, staging, table)?
                }
                RepairAction::KeepSingleRecord { table } => {
                    keep_single(&transaction, staging, table)?
                }
                RepairAction::RecoverOrphans { table } => {
                    recover_orphans(&transaction, staging, table, now_ms)?
                }
                RepairAction::SettleAuthority { subject } => {
                    settle_authority(&transaction, staging, subject)?
                }
            }
        }
        let records = record_changes(&transaction, &active, staging)?;
        let released = released_objects(&transaction, &active, staging)?;
        transaction.commit()?;

        // The gate that refuses a damaged backup judges the staged result before it is activated.
        self.validate_staged(staging)?;

        let committed = self.replace_commit(staging, Some(expected_revision))?;
        Ok((
            committed.clone(),
            Journal {
                id: format!("repair-{}", uuid::Uuid::new_v4()),
                created_at: now_ms,
                from_revision,
                to_revision: committed.revision,
                applied: candidates.to_vec(),
                records,
                released_objects: released,
            },
        ))
    }

    /// Runs the fail-fast library gate over a staged generation, through its own connection.
    fn validate_staged(&self, staging: &str) -> StoreResult<()> {
        let view = snapshot::open_generation_reader(&self.database_path, staging)?;
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        crate::portable_backup::validate_live_library(&view, &cas, &crate::local_backup::NeverCancelled)
            .map_err(|error| validation(format!("the repaired library is still refused: {error}")))?;
        Ok(())
    }

    /// Restores what a journal holds. A record the reader changed after the repair is left as
    /// it is and reported, so an undo never discards later work.
    pub(crate) fn undo_repair(
        &mut self,
        journal: &Journal,
        expected_revision: i64,
    ) -> StoreResult<(RevisionResult, Vec<String>)> {
        let staging = self.replace_begin()?.staging_id;
        let outcome = self.stage_undo(&staging, journal, expected_revision);
        if outcome.is_err() {
            let _ = self.replace_abort(&staging);
        }
        outcome
    }

    fn stage_undo(
        &mut self,
        staging: &str,
        journal: &Journal,
        expected_revision: i64,
    ) -> StoreResult<(RevisionResult, Vec<String>)> {
        let active = super::active_generation(&self.connection)?;
        let current = self.revision()?;
        if current != expected_revision {
            return Err(StoreError::RevisionConflict {
                expected: expected_revision,
                actual: current,
            });
        }
        let mut skipped = Vec::new();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        copy_generation(&transaction, &active, staging)?;
        for change in &journal.records {
            let Some(table) = TABLES.iter().find(|table| table.name == change.table) else {
                continue;
            };
            let stored = stored_row(&transaction, table, staging, &change.identity)?;
            if stored != change.after {
                skipped.push(format!("{}:{}", change.table, change.identity.join("/")));
                continue;
            }
            match &change.before {
                Some(before) => write_row(&transaction, table, staging, before)?,
                None => delete_row(&transaction, table, staging, &change.identity)?,
            }
        }
        transaction.commit()?;
        // An undo restores a state the store already held, which the backup gate may well have
        // refused: that is why it was repaired. The activation contract inside the commit is the
        // gate here, so a repair stays reversible.
        let committed = self.replace_commit(staging, Some(expected_revision))?;
        Ok((committed, skipped))
    }
}

/// The row a table currently holds under one identity, or nothing when it holds none.
fn stored_row(
    transaction: &Connection,
    table: &super::portable::PortableTable,
    staging: &str,
    identity: &[String],
) -> StoreResult<Option<Row>> {
    let mut statement = transaction.prepare(&format!(
        "SELECT {} FROM {} WHERE generation=?1{}",
        table.column_list(),
        table.name,
        where_identity(table.name)
    ))?;
    let mut rows = statement.query(rusqlite::params_from_iter(
        std::iter::once(staging.to_owned()).chain(identity.iter().cloned()),
    ))?;
    match rows.next()? {
        Some(row) => Ok(Some(read_row(row, table.columns.len())?)),
        None => Ok(None),
    }
}

fn delete_row(
    transaction: &Connection,
    table: &super::portable::PortableTable,
    staging: &str,
    identity: &[String],
) -> StoreResult<()> {
    transaction.execute(
        &format!(
            "DELETE FROM {} WHERE generation=?1{}",
            table.name,
            where_identity(table.name)
        ),
        rusqlite::params_from_iter(
            std::iter::once(staging.to_owned()).chain(identity.iter().cloned()),
        ),
    )?;
    Ok(())
}

/// Writes one stored image back, keeping the storage class each column declares.
fn write_row(
    transaction: &Connection,
    table: &super::portable::PortableTable,
    staging: &str,
    row: &Row,
) -> StoreResult<()> {
    let placeholders = (0..table.columns.len())
        .map(|index| format!("?{}", index + 2))
        .collect::<Vec<_>>()
        .join(", ");
    let mut parameters: Vec<rusqlite::types::Value> = vec![staging.to_owned().into()];
    for ((_, declaration), value) in table.columns.iter().zip(row) {
        parameters.push(match value {
            None => rusqlite::types::Value::Null,
            Some(value) if *declaration == "INTEGER" => value
                .parse::<i64>()
                .map(rusqlite::types::Value::Integer)
                .unwrap_or_else(|_| value.clone().into()),
            Some(value) => value.clone().into(),
        });
    }
    transaction.execute(
        &format!(
            "INSERT OR REPLACE INTO {} (generation, {}) VALUES (?1, {placeholders})",
            table.name,
            table.column_list(),
        ),
        rusqlite::params_from_iter(parameters),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
