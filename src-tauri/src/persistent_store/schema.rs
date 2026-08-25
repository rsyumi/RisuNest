use super::{StoreError, StoreResult};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use serde_json::Value;

const SCHEMA_VERSION: u32 = 2;

pub(super) fn initialize(connection: &mut Connection) -> StoreResult<()> {
    connection.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA busy_timeout = 5000;
        PRAGMA cache_size = -16000;
        PRAGMA temp_store = MEMORY;
        PRAGMA journal_size_limit = 67108864;
        PRAGMA foreign_keys = OFF;
        ",
    )?;

    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match version {
        0 => create_v2(connection),
        1 => migrate_v1(connection),
        SCHEMA_VERSION => Ok(()),
        _ => Err(StoreError::Store {
            message: format!("unsupported persistent schema version {version}"),
        }),
    }
}

fn create_v2(connection: &mut Connection) -> StoreResult<()> {
    connection.execute_batch(
        "
        BEGIN IMMEDIATE;
        CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE app_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE snapshot_leases (generation TEXT PRIMARY KEY, created_at INTEGER NOT NULL);
        CREATE TABLE root (generation TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE bot_presets (
            generation TEXT NOT NULL,
            preset_id TEXT NOT NULL,
            configured_index INTEGER NOT NULL,
            name TEXT NOT NULL,
            image TEXT,
            value TEXT NOT NULL,
            PRIMARY KEY (generation, preset_id)
        );
        CREATE INDEX bot_presets_configured ON bot_presets (generation, configured_index);
        CREATE TABLE characters (
            generation TEXT NOT NULL,
            character_id TEXT NOT NULL,
            configured_index INTEGER NOT NULL,
            recent_at INTEGER NOT NULL,
            trashed INTEGER NOT NULL,
            name TEXT NOT NULL,
            image TEXT,
            conversation_count INTEGER NOT NULL,
            type TEXT NOT NULL,
            creator_notes TEXT,
            trash_time INTEGER,
            detail TEXT NOT NULL,
            PRIMARY KEY (generation, character_id)
        );
        CREATE INDEX characters_configured ON characters (generation, configured_index);
        CREATE INDEX characters_recent ON characters (generation, recent_at DESC, configured_index);
        CREATE TABLE conversations (
            generation TEXT NOT NULL,
            character_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            configured_index INTEGER NOT NULL,
            recent_at INTEGER NOT NULL,
            name TEXT NOT NULL,
            message_count INTEGER NOT NULL,
            detail TEXT NOT NULL,
            PRIMARY KEY (generation, character_id, conversation_id)
        );
        CREATE INDEX conversations_configured
            ON conversations (generation, character_id, configured_index);
        CREATE INDEX conversations_recent
            ON conversations (generation, character_id, recent_at DESC, configured_index);
        CREATE TABLE messages (
            generation TEXT NOT NULL,
            character_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            message_index INTEGER NOT NULL,
            message_id TEXT,
            value TEXT NOT NULL,
            PRIMARY KEY (generation, character_id, conversation_id, message_index)
        );
        CREATE INDEX messages_by_id
            ON messages (generation, character_id, conversation_id, message_id);
        PRAGMA user_version = 2;
        COMMIT;
        ",
    )?;
    Ok(())
}

fn migrate_v1(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "
        CREATE TABLE bot_presets (
            generation TEXT NOT NULL,
            preset_id TEXT NOT NULL,
            configured_index INTEGER NOT NULL,
            name TEXT NOT NULL,
            image TEXT,
            value TEXT NOT NULL,
            PRIMARY KEY (generation, preset_id)
        );
        CREATE INDEX bot_presets_configured ON bot_presets (generation, configured_index);
        ALTER TABLE characters ADD COLUMN type TEXT NOT NULL DEFAULT '';
        ALTER TABLE characters ADD COLUMN creator_notes TEXT;
        ALTER TABLE characters ADD COLUMN trash_time INTEGER;
        ",
    )?;
    migrate_roots(&transaction)?;
    backfill_characters(&transaction)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_roots(transaction: &Transaction<'_>) -> StoreResult<()> {
    let rows = {
        let mut statement = transaction.prepare("SELECT generation, value FROM root")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    for (generation, serialized) in rows {
        let mut value: Value = serde_json::from_str(&serialized)?;
        let object = value.as_object_mut().ok_or_else(|| StoreError::Store {
            message: "persistent root must be an object".to_owned(),
        })?;
        let presets = match object.remove("botPresets") {
            None => Vec::new(),
            Some(Value::Array(presets)) => presets,
            Some(_) => {
                return Err(StoreError::Validation {
                    message: format!(
                        "v1 root for generation {generation} has non-array botPresets"
                    ),
                });
            }
        };
        object.remove("characters");
        for (configured_index, preset) in presets.iter().enumerate() {
            insert_preset(transaction, &generation, configured_index as i64, preset)?;
        }
        transaction.execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            params![generation, serde_json::to_string(&value)?],
        )?;
    }
    Ok(())
}

fn backfill_characters(transaction: &Transaction<'_>) -> StoreResult<()> {
    let rows = {
        let mut statement =
            transaction.prepare("SELECT generation, character_id, detail FROM characters")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    for (generation, character_id, serialized) in rows {
        let detail: Value = serde_json::from_str(&serialized)?;
        transaction.execute(
            "UPDATE characters SET type = ?3, creator_notes = ?4, trash_time = ?5
             WHERE generation = ?1 AND character_id = ?2",
            params![
                generation,
                character_id,
                detail
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("character"),
                detail.get("creatorNotes").and_then(Value::as_str),
                detail.get("trashTime").and_then(Value::as_i64),
            ],
        )?;
    }
    Ok(())
}

fn insert_preset(
    transaction: &Transaction<'_>,
    generation: &str,
    configured_index: i64,
    preset: &Value,
) -> StoreResult<()> {
    transaction.execute(
        "INSERT INTO bot_presets (generation, preset_id, configured_index, name, image, value)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            generation,
            configured_index.to_string(),
            configured_index,
            preset
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            preset.get("image").and_then(Value::as_str),
            serde_json::to_string(preset)?,
        ],
    )?;
    Ok(())
}
