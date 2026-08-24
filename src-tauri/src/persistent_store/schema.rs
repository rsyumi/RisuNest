use rusqlite::Connection;

pub(super) fn initialize(connection: &Connection) -> rusqlite::Result<()> {
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
        0 => connection.execute_batch(
            "
            BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS app_kv (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS snapshot_leases (
                generation TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS root (
                generation TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS characters (
                generation TEXT NOT NULL,
                character_id TEXT NOT NULL,
                configured_index INTEGER NOT NULL,
                recent_at INTEGER NOT NULL,
                trashed INTEGER NOT NULL,
                name TEXT NOT NULL,
                image TEXT,
                conversation_count INTEGER NOT NULL,
                detail TEXT NOT NULL,
                PRIMARY KEY (generation, character_id)
            );
            CREATE INDEX IF NOT EXISTS characters_configured
                ON characters (generation, configured_index);
            CREATE INDEX IF NOT EXISTS characters_recent
                ON characters (generation, recent_at DESC, configured_index);
            CREATE TABLE IF NOT EXISTS conversations (
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
            CREATE INDEX IF NOT EXISTS conversations_configured
                ON conversations (generation, character_id, configured_index);
            CREATE INDEX IF NOT EXISTS conversations_recent
                ON conversations (generation, character_id, recent_at DESC, configured_index);
            CREATE TABLE IF NOT EXISTS messages (
                generation TEXT NOT NULL,
                character_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL,
                message_index INTEGER NOT NULL,
                message_id TEXT,
                value TEXT NOT NULL,
                PRIMARY KEY (generation, character_id, conversation_id, message_index)
            );
            CREATE INDEX IF NOT EXISTS messages_by_id
                ON messages (generation, character_id, conversation_id, message_id);
            PRAGMA user_version = 1;
            COMMIT;
            ",
        ),
        1 => Ok(()),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}
