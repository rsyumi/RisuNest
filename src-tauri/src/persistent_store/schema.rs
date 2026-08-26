use super::{logical_schema, StoreError, StoreResult};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use serde_json::Value;

pub(super) const SCHEMA_VERSION: u32 = 10;

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
        0 => create_v10(connection),
        1 => migrate_v1(connection),
        2 => migrate_v2(connection),
        3 => migrate_v3(connection),
        4 => migrate_v4(connection),
        5 => migrate_v5(connection),
        6 => migrate_v6_or_v7_to_v10(connection, true),
        7 => migrate_v6_or_v7_to_v10(connection, false),
        8 => migrate_v8_to_v10(connection),
        9 => migrate_v9_to_v10(connection),
        SCHEMA_VERSION => Ok(()),
        _ => Err(StoreError::Store {
            message: format!("unsupported persistent schema version {version}"),
        }),
    }
}

fn create_v10(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "
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
        CREATE TABLE asset_aliases (
            generation TEXT NOT NULL,
            logical_key TEXT NOT NULL,
            object_hash TEXT CHECK (
                object_hash IS NULL OR (
                    length(object_hash) = 64
                    AND object_hash NOT GLOB '*[^0-9a-f]*'
                )
            ),
            kind TEXT NOT NULL CHECK (kind IN ('asset', 'inlay')),
            size INTEGER NOT NULL CHECK (size >= 0),
            mime TEXT NOT NULL,
            name TEXT NOT NULL,
            ext TEXT NOT NULL,
            inlay_type TEXT CHECK (
                inlay_type IS NULL OR inlay_type IN ('image', 'video', 'audio', 'signature')
            ),
            width INTEGER CHECK (width IS NULL OR width >= 0),
            height INTEGER CHECK (height IS NULL OR height >= 0),
            metadata TEXT NOT NULL DEFAULT '{}',
            CHECK (
                (kind = 'asset' AND inlay_type IS NULL AND width IS NULL AND height IS NULL)
                OR (kind = 'inlay' AND inlay_type IS NOT NULL)
            ),
            PRIMARY KEY (generation, kind, logical_key)
        );
        CREATE INDEX asset_aliases_generation ON asset_aliases (generation);
        CREATE TABLE asset_owner_heads (
            generation TEXT NOT NULL,
            owner_kind TEXT NOT NULL CHECK (owner_kind IN (
                'character-additional-assets',
                'root-module-assets',
                'persona-embedded-module-assets'
            )),
            owner_locator TEXT NOT NULL,
            present INTEGER NOT NULL CHECK (present IN (0, 1)),
            manifest_hash TEXT CHECK (
                manifest_hash IS NULL OR (
                    length(manifest_hash) = 64
                    AND manifest_hash NOT GLOB '*[^0-9a-f]*'
                )
            ),
            entry_count INTEGER NOT NULL CHECK (entry_count >= 0),
            CHECK (
                (present = 0 AND manifest_hash IS NULL AND entry_count = 0)
                OR (present = 1 AND manifest_hash IS NOT NULL)
            ),
            PRIMARY KEY (generation, owner_kind, owner_locator)
        );
        CREATE INDEX asset_owner_heads_generation ON asset_owner_heads (generation);
        CREATE TABLE asset_repository_authority (
            generation TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE cold_aliases (
            generation TEXT NOT NULL,
            key TEXT NOT NULL,
            object_hash TEXT CHECK (
                object_hash IS NULL OR (
                    length(object_hash) = 64
                    AND object_hash NOT GLOB '*[^0-9a-f]*'
                )
            ),
            size INTEGER NOT NULL CHECK (size >= 0),
            metadata TEXT NOT NULL,
            PRIMARY KEY (generation, key)
        );
        CREATE INDEX cold_aliases_generation ON cold_aliases (generation);
        ",
    )?;
    finish_v10_migration(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v8_to_v10(connection: &mut Connection) -> StoreResult<()> {
    migrate_v8_to_v9(connection)?;
    migrate_v9_to_v10(connection)
}

fn migrate_v8_to_v9(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    create_asset_repository_authority(&transaction)?;
    transaction.pragma_update(None, "user_version", 9)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v9_to_v10(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    finish_v10_migration(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn finish_v10_migration(transaction: &Transaction<'_>) -> StoreResult<()> {
    logical_schema::create_logical_schema_strict(transaction).map_err(|error| {
        StoreError::Store {
            message: error.to_string(),
        }
    })?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn migrate_v5(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    create_asset_aliases(&transaction)?;
    create_asset_owner_heads(&transaction)?;
    create_asset_repository_authority(&transaction)?;
    create_cold_aliases(&transaction)?;
    finish_v10_migration(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn create_asset_aliases(transaction: &Transaction<'_>) -> StoreResult<()> {
    transaction.execute_batch(
        "
        CREATE TABLE asset_aliases (
            generation TEXT NOT NULL,
            logical_key TEXT NOT NULL,
            object_hash TEXT CHECK (
                object_hash IS NULL OR (
                    length(object_hash) = 64
                    AND object_hash NOT GLOB '*[^0-9a-f]*'
                )
            ),
            kind TEXT NOT NULL CHECK (kind IN ('asset', 'inlay')),
            size INTEGER NOT NULL CHECK (size >= 0),
            mime TEXT NOT NULL,
            name TEXT NOT NULL,
            ext TEXT NOT NULL,
            inlay_type TEXT CHECK (
                inlay_type IS NULL OR inlay_type IN ('image', 'video', 'audio', 'signature')
            ),
            width INTEGER CHECK (width IS NULL OR width >= 0),
            height INTEGER CHECK (height IS NULL OR height >= 0),
            metadata TEXT NOT NULL DEFAULT '{}',
            CHECK (
                (kind = 'asset' AND inlay_type IS NULL AND width IS NULL AND height IS NULL)
                OR (kind = 'inlay' AND inlay_type IS NOT NULL)
            ),
            PRIMARY KEY (generation, kind, logical_key)
        );
        CREATE INDEX asset_aliases_generation ON asset_aliases (generation);
        ",
    )?;
    Ok(())
}

fn create_asset_owner_heads(transaction: &Transaction<'_>) -> StoreResult<()> {
    transaction.execute_batch(
        "
        CREATE TABLE asset_owner_heads (
            generation TEXT NOT NULL,
            owner_kind TEXT NOT NULL CHECK (owner_kind IN (
                'character-additional-assets',
                'root-module-assets',
                'persona-embedded-module-assets'
            )),
            owner_locator TEXT NOT NULL,
            present INTEGER NOT NULL CHECK (present IN (0, 1)),
            manifest_hash TEXT CHECK (
                manifest_hash IS NULL OR (
                    length(manifest_hash) = 64
                    AND manifest_hash NOT GLOB '*[^0-9a-f]*'
                )
            ),
            entry_count INTEGER NOT NULL CHECK (entry_count >= 0),
            CHECK (
                (present = 0 AND manifest_hash IS NULL AND entry_count = 0)
                OR (present = 1 AND manifest_hash IS NOT NULL)
            ),
            PRIMARY KEY (generation, owner_kind, owner_locator)
        );
        CREATE INDEX asset_owner_heads_generation ON asset_owner_heads (generation);
        ",
    )?;
    Ok(())
}

fn create_asset_repository_authority(transaction: &Transaction<'_>) -> StoreResult<()> {
    transaction.execute_batch(
        "
        CREATE TABLE asset_repository_authority (
            generation TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        INSERT INTO asset_repository_authority (generation, value)
        SELECT generation, json_object('format', 'legacy') FROM root;
        ",
    )?;
    Ok(())
}

fn create_cold_aliases(transaction: &Transaction<'_>) -> StoreResult<()> {
    transaction.execute_batch(
        "
        CREATE TABLE cold_aliases (
            generation TEXT NOT NULL,
            key TEXT NOT NULL,
            object_hash TEXT CHECK (
                object_hash IS NULL OR (
                    length(object_hash) = 64
                    AND object_hash NOT GLOB '*[^0-9a-f]*'
                )
            ),
            size INTEGER NOT NULL CHECK (size >= 0),
            metadata TEXT NOT NULL,
            PRIMARY KEY (generation, key)
        );
        CREATE INDEX cold_aliases_generation ON cold_aliases (generation);
        ",
    )?;
    Ok(())
}

fn migrate_v6_or_v7_to_v10(
    connection: &mut Connection,
    create_owner_heads: bool,
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "
        DROP INDEX asset_aliases_generation;
        ALTER TABLE asset_aliases RENAME TO asset_aliases_v6;
        ",
    )?;
    create_asset_aliases(&transaction)?;
    transaction.execute_batch(
        "
        INSERT INTO asset_aliases (
            generation, logical_key, object_hash, kind, size, mime, name, ext,
            inlay_type, width, height, metadata
        )
        SELECT generation, logical_key, object_hash, kind, size, mime, name, ext,
               inlay_type, width, height,
               CASE kind
                   WHEN 'asset' THEN json_object(
                       'name', name, 'ext', ext, 'mime', mime
                   )
                   ELSE json_patch(
                       json_object(
                           'name', name, 'ext', ext, 'mime', mime,
                           'inlayType', inlay_type
                       ),
                       CASE
                           WHEN width IS NOT NULL AND height IS NOT NULL
                               THEN json_object('width', width, 'height', height)
                           WHEN width IS NOT NULL THEN json_object('width', width)
                           WHEN height IS NOT NULL THEN json_object('height', height)
                           ELSE '{}'
                       END
                   )
               END
        FROM asset_aliases_v6;
        DROP TABLE asset_aliases_v6;
        ",
    )?;
    if create_owner_heads {
        create_asset_owner_heads(&transaction)?;
    }
    create_asset_repository_authority(&transaction)?;
    create_cold_aliases(&transaction)?;
    finish_v10_migration(&transaction)?;
    transaction.commit()?;
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
    create_asset_aliases(&transaction)?;
    create_asset_owner_heads(&transaction)?;
    create_asset_repository_authority(&transaction)?;
    create_cold_aliases(&transaction)?;
    finish_v10_migration(&transaction)?;
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
    create_asset_aliases(&transaction)?;
    create_asset_owner_heads(&transaction)?;
    create_asset_repository_authority(&transaction)?;
    create_cold_aliases(&transaction)?;
    finish_v10_migration(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v3(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_plugin_storage(&transaction)?;
    ensure_snapshot_leases(&transaction)?;
    create_asset_aliases(&transaction)?;
    create_asset_owner_heads(&transaction)?;
    create_asset_repository_authority(&transaction)?;
    create_cold_aliases(&transaction)?;
    finish_v10_migration(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v4(connection: &mut Connection) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure_plugin_storage(&transaction)?;
    ensure_snapshot_leases(&transaction)?;
    create_asset_aliases(&transaction)?;
    create_asset_owner_heads(&transaction)?;
    create_asset_repository_authority(&transaction)?;
    create_cold_aliases(&transaction)?;
    finish_v10_migration(&transaction)?;
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
