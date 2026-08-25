use super::{StoreError, StoreResult};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use serde_json::Value;

const SCHEMA_VERSION: u32 = 5;

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
        0 => create_v5(connection),
        1 => migrate_v1(connection),
        2 => migrate_v2(connection),
        3 => migrate_v3(connection),
        4 => migrate_v4(connection),
        SCHEMA_VERSION => Ok(()),
        _ => Err(StoreError::Store {
            message: format!("unsupported persistent schema version {version}"),
        }),
    }
}

fn create_v5(connection: &mut Connection) -> StoreResult<()> {
    connection.execute_batch(
        "
        BEGIN IMMEDIATE;
        CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE app_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE snapshot_leases (
            lease TEXT PRIMARY KEY,
            generation TEXT NOT NULL,
            revision INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX snapshot_leases_generation ON snapshot_leases (generation);
        CREATE TABLE root (generation TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE plugin_storage (
            generation TEXT NOT NULL,
            storage_key TEXT NOT NULL,
            byte_size INTEGER NOT NULL,
            ordinal INTEGER NOT NULL,
            value TEXT NOT NULL,
            PRIMARY KEY (generation, storage_key)
        );
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
        PRAGMA user_version = 5;
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
        CREATE TABLE plugin_storage (
            generation TEXT NOT NULL,
            storage_key TEXT NOT NULL,
            byte_size INTEGER NOT NULL,
            ordinal INTEGER NOT NULL,
            value TEXT NOT NULL,
            PRIMARY KEY (generation, storage_key)
        );
        ALTER TABLE characters ADD COLUMN type TEXT NOT NULL DEFAULT '';
        ALTER TABLE characters ADD COLUMN creator_notes TEXT;
        ALTER TABLE characters ADD COLUMN trash_time INTEGER;
        ",
    )?;
    migrate_roots(&transaction)?;
    backfill_characters(&transaction)?;
    migrate_snapshot_leases(&transaction)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v2(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "
        CREATE TABLE plugin_storage (
            generation TEXT NOT NULL,
            storage_key TEXT NOT NULL,
            byte_size INTEGER NOT NULL,
            ordinal INTEGER NOT NULL,
            value TEXT NOT NULL,
            PRIMARY KEY (generation, storage_key)
        );
        ",
    )?;
    migrate_plugin_storage(&transaction)?;
    migrate_snapshot_leases(&transaction)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v3(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_plugin_storage(&transaction)?;
    ensure_snapshot_leases(&transaction)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v4(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_plugin_storage(&transaction)?;
    ensure_snapshot_leases(&transaction)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn ensure_plugin_storage(transaction: &Transaction<'_>) -> StoreResult<()> {
    if !table_exists(transaction, "plugin_storage")? {
        transaction.execute_batch(
            "
            CREATE TABLE plugin_storage (
                generation TEXT NOT NULL,
                storage_key TEXT NOT NULL,
                byte_size INTEGER NOT NULL,
                ordinal INTEGER NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (generation, storage_key)
            );
            ",
        )?;
        migrate_plugin_storage(transaction)?;
    } else if !column_exists(transaction, "plugin_storage", "ordinal")? {
        transaction.execute_batch(
            "
            ALTER TABLE plugin_storage ADD COLUMN ordinal INTEGER NOT NULL DEFAULT 0;
            UPDATE plugin_storage AS target
            SET ordinal = (
                SELECT COUNT(*) - 1
                FROM plugin_storage AS predecessor
                WHERE predecessor.generation = target.generation
                  AND predecessor.storage_key <= target.storage_key
            );
            ",
        )?;
    }
    Ok(())
}

fn ensure_snapshot_leases(transaction: &Transaction<'_>) -> StoreResult<()> {
    if column_exists(transaction, "snapshot_leases", "lease")? {
        return Ok(());
    }
    migrate_snapshot_leases(&transaction)?;
    Ok(())
}

fn table_exists(transaction: &Transaction<'_>, table: &str) -> StoreResult<bool> {
    Ok(transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn column_exists(transaction: &Transaction<'_>, table: &str, column: &str) -> StoreResult<bool> {
    let sql = format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1)");
    Ok(transaction.query_row(&sql, [column], |row| row.get(0))?)
}

fn migrate_snapshot_leases(transaction: &Transaction<'_>) -> StoreResult<()> {
    let leases = {
        let mut statement =
            transaction.prepare("SELECT generation, created_at FROM snapshot_leases")?;
        let leases = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        leases
    };
    transaction.execute_batch(
        "
        DROP TABLE snapshot_leases;
        CREATE TABLE snapshot_leases (
            lease TEXT PRIMARY KEY,
            generation TEXT NOT NULL,
            revision INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX snapshot_leases_generation ON snapshot_leases (generation);
        ",
    )?;
    for (lease, created_at) in leases {
        let Some(revision) = lease
            .strip_prefix("snapshot-")
            .and_then(|value| value.split('-').next())
            .and_then(|value| value.parse::<i64>().ok())
        else {
            continue;
        };
        transaction.execute(
            "INSERT INTO snapshot_leases (lease, generation, revision, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![lease, lease, revision, created_at],
        )?;
    }
    Ok(())
}

fn migrate_roots(transaction: &Transaction<'_>) -> StoreResult<()> {
    for generation in root_generations(transaction)? {
        let serialized: String = transaction.query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [&generation],
            |row| row.get(0),
        )?;
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
        migrate_plugin_storage_value(transaction, &generation, object)?;
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

fn migrate_plugin_storage(transaction: &Transaction<'_>) -> StoreResult<()> {
    for generation in root_generations(transaction)? {
        let serialized: String = transaction.query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [&generation],
            |row| row.get(0),
        )?;
        let mut value: Value = serde_json::from_str(&serialized)?;
        let object = value.as_object_mut().ok_or_else(|| StoreError::Store {
            message: "persistent root must be an object".to_owned(),
        })?;
        migrate_plugin_storage_value(transaction, &generation, object)?;
        transaction.execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            params![generation, serde_json::to_string(&value)?],
        )?;
    }
    Ok(())
}

fn root_generations(transaction: &Transaction<'_>) -> StoreResult<Vec<String>> {
    let mut statement = transaction.prepare("SELECT generation FROM root")?;
    let generations = statement
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(generations)
}

fn migrate_plugin_storage_value(
    transaction: &Transaction<'_>,
    generation: &str,
    root: &mut serde_json::Map<String, Value>,
) -> StoreResult<()> {
    let Some(storage) = root.remove("pluginCustomStorage") else {
        return Ok(());
    };
    let storage = storage.as_object().ok_or_else(|| StoreError::Validation {
        message: format!(
            "persistent root for generation {generation} has non-object pluginCustomStorage"
        ),
    })?;
    for (ordinal, (key, value)) in storage.iter().enumerate() {
        let serialized = serde_json::to_string(value)?;
        transaction.execute(
            "INSERT INTO plugin_storage (generation, storage_key, byte_size, ordinal, value)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                generation,
                key,
                serialized.len() as i64,
                ordinal as i64,
                serialized
            ],
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
