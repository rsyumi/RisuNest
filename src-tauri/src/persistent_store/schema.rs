use super::{StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

pub(crate) const SCHEMA_VERSION: u32 = 5;

const ASSET_GC_MAINTENANCE_STATE_TABLE_SQL: &str = r#"
CREATE TABLE asset_gc_maintenance_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    catalog_cursor TEXT CHECK (
        catalog_cursor IS NULL OR length(catalog_cursor) BETWEEN 1 AND 512
    )
)
"#;

const ASSET_ALIAS_REPLACEMENT_CANDIDATE_TABLE_SQL: &str = r#"
CREATE TABLE asset_alias_replacement_candidates (
    generation TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('asset', 'inlay')),
    logical_key TEXT NOT NULL CHECK (
        length(logical_key) > 0 AND instr(logical_key, char(0)) = 0
    ),
    object_hash TEXT NOT NULL CHECK (
        length(object_hash) = 64
        AND object_hash NOT GLOB '*[^0-9a-f]*'
    ),
    byte_size INTEGER NOT NULL CHECK (byte_size >= 0),
    PRIMARY KEY (generation, kind, logical_key, object_hash)
)
"#;

const ASSET_OBJECT_DELETION_TABLE_SQL: &str = r#"
CREATE TABLE asset_object_deletions (
    object_hash TEXT PRIMARY KEY CHECK (
        length(object_hash) = 64
        AND object_hash NOT GLOB '*[^0-9a-f]*'
    ),
    byte_size INTEGER NOT NULL CHECK (byte_size >= 0),
    physical_key TEXT NOT NULL UNIQUE CHECK (length(physical_key) = 83),
    state TEXT NOT NULL CHECK (state IN ('pending', 'unlinked')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0)
)
"#;

const ASSET_OBJECT_DELETION_INDEX_SQL: &str = r#"
CREATE INDEX asset_object_deletions_state
    ON asset_object_deletions (state, created_at_ms, object_hash)
"#;

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
        0 => create_schema(connection),
        SCHEMA_VERSION => validate_schema(connection),
        _ => Err(StoreError::Store {
            message: format!("unsupported persistent schema version {version}"),
        }),
    }
}

fn create_schema(connection: &mut Connection) -> StoreResult<()> {
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
            owner TEXT NOT NULL,
            storage_key TEXT NOT NULL,
            byte_size INTEGER NOT NULL,
            ordinal INTEGER NOT NULL,
            value TEXT NOT NULL,
            claimed_from TEXT,
            import_batch_id TEXT,
            assigned_at INTEGER,
            PRIMARY KEY (generation, owner, storage_key)
        );
        CREATE INDEX plugin_storage_owner ON plugin_storage (generation, owner, ordinal);
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
            archived_object TEXT,
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
        CREATE TABLE asset_objects (
            object_hash TEXT PRIMARY KEY CHECK (
                length(object_hash) = 64
                AND object_hash NOT GLOB '*[^0-9a-f]*'
            ),
            byte_size INTEGER NOT NULL CHECK (byte_size >= 0),
            created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0)
        );
        CREATE INDEX asset_objects_created
            ON asset_objects (created_at_ms, object_hash);
        ",
    )?;
    transaction.execute_batch(ASSET_OBJECT_DELETION_TABLE_SQL)?;
    transaction.execute_batch(ASSET_OBJECT_DELETION_INDEX_SQL)?;
    transaction.execute_batch(ASSET_ALIAS_REPLACEMENT_CANDIDATE_TABLE_SQL)?;
    transaction.execute_batch(ASSET_GC_MAINTENANCE_STATE_TABLE_SQL)?;
    transaction.execute(
        "INSERT INTO asset_gc_maintenance_state (singleton, catalog_cursor) VALUES (1, NULL)",
        [],
    )?;
    super::server_sync_outbox::create_schema(&transaction)?;
    super::content_change_index::create_schema(&transaction)?;
    super::sync_selection::create_schema(&transaction)?;
    super::external_storage_state::create_schema(&transaction)?;
    validate_schema(&transaction)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn validate_schema(connection: &Connection) -> StoreResult<()> {
    super::server_sync_outbox::validate_schema(connection)?;
    super::content_change_index::validate_schema(connection)?;
    super::external_storage_state::validate_schema(connection)?;
    validate_object_sql(
        connection,
        "table",
        "asset_object_deletions",
        ASSET_OBJECT_DELETION_TABLE_SQL,
        "asset object deletion tombstone table definition is invalid",
    )?;
    validate_object_sql(
        connection,
        "index",
        "asset_object_deletions_state",
        ASSET_OBJECT_DELETION_INDEX_SQL,
        "asset object deletion tombstone index definition is invalid",
    )?;
    validate_object_sql(
        connection,
        "table",
        "asset_alias_replacement_candidates",
        ASSET_ALIAS_REPLACEMENT_CANDIDATE_TABLE_SQL,
        "asset alias replacement provenance table definition is invalid",
    )?;
    validate_object_sql(
        connection,
        "table",
        "asset_gc_maintenance_state",
        ASSET_GC_MAINTENANCE_STATE_TABLE_SQL,
        "asset GC maintenance state table definition is invalid",
    )?;
    let rows: i64 = connection.query_row(
        "SELECT COUNT(*) FROM asset_gc_maintenance_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let total_rows: i64 = connection.query_row(
        "SELECT COUNT(*) FROM asset_gc_maintenance_state",
        [],
        |row| row.get(0),
    )?;
    if rows != 1 || total_rows != 1 {
        return Err(StoreError::Validation {
            message: "asset GC maintenance state row is invalid".to_owned(),
        });
    }
    Ok(())
}

fn validate_object_sql(
    connection: &Connection,
    kind: &str,
    name: &str,
    expected: &str,
    message: &str,
) -> StoreResult<()> {
    let object_sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
            [kind, name],
            |row| row.get(0),
        )
        .optional()?;
    if object_sql.as_deref().map(normalize_schema_sql) != Some(normalize_schema_sql(expected)) {
        return Err(StoreError::Validation {
            message: message.to_owned(),
        });
    }
    Ok(())
}

fn normalize_schema_sql(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(';')
        .to_owned()
}
