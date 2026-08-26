use rusqlite::Connection;
use std::collections::HashSet;

// These rows are rebuildable indexes over PDS records and CAS objects. They never store record bytes.
pub(crate) const LOGICAL_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS logical_sync_generations (
    library_id TEXT NOT NULL CHECK (length(library_id) > 0),
    generation_id TEXT NOT NULL CHECK (length(generation_id) > 0),
    generation_sequence TEXT NOT NULL CHECK (
        length(generation_sequence) > 0
        AND generation_sequence NOT GLOB '*[^0-9]*'
        AND (generation_sequence = '0' OR substr(generation_sequence, 1, 1) != '0')
    ),
    parent_generation_id TEXT CHECK (
        parent_generation_id IS NULL
        OR (
            length(parent_generation_id) > 0
            AND parent_generation_id != generation_id
        )
    ),
    pds_generation TEXT NOT NULL CHECK (length(pds_generation) > 0),
    source_revision INTEGER NOT NULL CHECK (source_revision >= 0),
    state TEXT NOT NULL CHECK (state IN ('building', 'complete')),
    manifest_hash TEXT CHECK (
        manifest_hash IS NULL OR (
            length(manifest_hash) = 64
            AND manifest_hash NOT GLOB '*[^0-9a-f]*'
        )
    ),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    completed_at INTEGER CHECK (completed_at IS NULL OR completed_at >= created_at),
    CHECK (
        (state = 'building' AND manifest_hash IS NULL AND completed_at IS NULL)
        OR (state = 'complete' AND manifest_hash IS NOT NULL AND completed_at IS NOT NULL)
    ),
    PRIMARY KEY (library_id, generation_id),
    UNIQUE (library_id, pds_generation)
);
CREATE INDEX IF NOT EXISTS logical_sync_generations_parent
    ON logical_sync_generations (library_id, parent_generation_id);
CREATE INDEX IF NOT EXISTS logical_sync_generations_manifest
    ON logical_sync_generations (manifest_hash);

CREATE TABLE IF NOT EXISTS logical_record_heads (
    library_id TEXT NOT NULL,
    generation_id TEXT NOT NULL,
    record_key TEXT NOT NULL CHECK (length(record_key) BETWEEN 1 AND 65536),
    record_kind TEXT NOT NULL CHECK (record_kind IN (
        'root',
        'preset',
        'plugin',
        'character',
        'conversation',
        'asset',
        'inlay',
        'cold'
    )),
    state TEXT NOT NULL CHECK (state IN ('live', 'tombstone')),
    object_hash TEXT CHECK (
        object_hash IS NULL OR (
            length(object_hash) = 64
            AND object_hash NOT GLOB '*[^0-9a-f]*'
        )
    ),
    object_size INTEGER NOT NULL CHECK (object_size >= 0),
    deleted_generation_sequence TEXT CHECK (
        deleted_generation_sequence IS NULL OR (
            length(deleted_generation_sequence) > 0
            AND deleted_generation_sequence NOT GLOB '*[^0-9]*'
            AND (
                deleted_generation_sequence = '0'
                OR substr(deleted_generation_sequence, 1, 1) != '0'
            )
        )
    ),
    CHECK (
        (
            state = 'live'
            AND object_hash IS NOT NULL
            AND deleted_generation_sequence IS NULL
            AND (
                (
                    object_size = 0
                    AND object_hash = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
                )
                OR (
                    object_size > 0
                    AND object_hash != 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
                )
            )
        )
        OR (
            state = 'tombstone'
            AND object_hash IS NULL
            AND object_size = 0
            AND deleted_generation_sequence IS NOT NULL
        )
    ),
    PRIMARY KEY (library_id, generation_id, record_key),
    UNIQUE (library_id, generation_id, record_key, record_kind),
    FOREIGN KEY (library_id, generation_id)
        REFERENCES logical_sync_generations (library_id, generation_id)
        ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS logical_record_heads_kind
    ON logical_record_heads (library_id, generation_id, record_kind, record_key);
CREATE INDEX IF NOT EXISTS logical_record_heads_object
    ON logical_record_heads (library_id, generation_id, object_hash, object_size)
    WHERE object_hash IS NOT NULL;

CREATE TABLE IF NOT EXISTS logical_record_dependencies (
    library_id TEXT NOT NULL,
    generation_id TEXT NOT NULL,
    record_key TEXT NOT NULL,
    object_hash TEXT NOT NULL CHECK (
        length(object_hash) = 64
        AND object_hash NOT GLOB '*[^0-9a-f]*'
    ),
    object_size INTEGER NOT NULL CHECK (
        object_size >= 0
        AND (
            (
                object_size = 0
                AND object_hash = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
            )
            OR (
                object_size > 0
                AND object_hash != 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
            )
        )
    ),
    PRIMARY KEY (library_id, generation_id, record_key, object_hash),
    FOREIGN KEY (library_id, generation_id, record_key)
        REFERENCES logical_record_heads (library_id, generation_id, record_key)
        ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS logical_record_dependencies_object
    ON logical_record_dependencies (library_id, generation_id, object_hash, object_size);

CREATE TABLE IF NOT EXISTS logical_message_page_sources (
    library_id TEXT NOT NULL,
    generation_id TEXT NOT NULL,
    record_key TEXT NOT NULL,
    record_kind TEXT NOT NULL DEFAULT 'conversation' CHECK (record_kind = 'conversation'),
    page_index INTEGER NOT NULL CHECK (page_index >= 0),
    first_message_index INTEGER NOT NULL CHECK (first_message_index >= 0),
    message_count INTEGER NOT NULL CHECK (message_count > 0),
    object_hash TEXT NOT NULL CHECK (
        length(object_hash) = 64
        AND object_hash NOT GLOB '*[^0-9a-f]*'
    ),
    object_size INTEGER NOT NULL CHECK (
        object_size >= 0
        AND (
            (
                object_size = 0
                AND object_hash = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
            )
            OR (
                object_size > 0
                AND object_hash != 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
            )
        )
    ),
    PRIMARY KEY (library_id, generation_id, record_key, page_index),
    UNIQUE (library_id, generation_id, record_key, first_message_index),
    FOREIGN KEY (library_id, generation_id, record_key, record_kind)
        REFERENCES logical_record_heads (
            library_id,
            generation_id,
            record_key,
            record_kind
        )
        ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS logical_message_page_sources_object
    ON logical_message_page_sources (library_id, generation_id, object_hash, object_size);

CREATE TABLE IF NOT EXISTS logical_peer_common_bases (
    peer_id TEXT NOT NULL CHECK (length(peer_id) > 0),
    library_id TEXT NOT NULL CHECK (length(library_id) > 0),
    generation_id TEXT NOT NULL CHECK (length(generation_id) > 0),
    manifest_hash TEXT NOT NULL CHECK (
        length(manifest_hash) = 64
        AND manifest_hash NOT GLOB '*[^0-9a-f]*'
    ),
    generation_sequence TEXT NOT NULL CHECK (
        length(generation_sequence) > 0
        AND generation_sequence NOT GLOB '*[^0-9]*'
        AND (generation_sequence = '0' OR substr(generation_sequence, 1, 1) != '0')
    ),
    updated_at INTEGER NOT NULL CHECK (updated_at >= 0),
    PRIMARY KEY (peer_id, library_id)
);
CREATE INDEX IF NOT EXISTS logical_peer_common_bases_manifest
    ON logical_peer_common_bases (library_id, manifest_hash);
"#;

const REQUIRED_TABLE_COLUMNS: &[(&str, &[&str])] = &[
    (
        "logical_sync_generations",
        &[
            "library_id",
            "generation_id",
            "generation_sequence",
            "parent_generation_id",
            "pds_generation",
            "source_revision",
            "state",
            "manifest_hash",
            "created_at",
            "completed_at",
        ],
    ),
    (
        "logical_record_heads",
        &[
            "library_id",
            "generation_id",
            "record_key",
            "record_kind",
            "state",
            "object_hash",
            "object_size",
            "deleted_generation_sequence",
        ],
    ),
    (
        "logical_record_dependencies",
        &[
            "library_id",
            "generation_id",
            "record_key",
            "object_hash",
            "object_size",
        ],
    ),
    (
        "logical_message_page_sources",
        &[
            "library_id",
            "generation_id",
            "record_key",
            "record_kind",
            "page_index",
            "first_message_index",
            "message_count",
            "object_hash",
            "object_size",
        ],
    ),
    (
        "logical_peer_common_bases",
        &[
            "peer_id",
            "library_id",
            "generation_id",
            "manifest_hash",
            "generation_sequence",
            "updated_at",
        ],
    ),
];

const REQUIRED_INDEXES: &[&str] = &[
    "logical_sync_generations_parent",
    "logical_sync_generations_manifest",
    "logical_record_heads_kind",
    "logical_record_heads_object",
    "logical_record_dependencies_object",
    "logical_message_page_sources_object",
    "logical_peer_common_bases_manifest",
];

#[derive(Debug)]
pub(crate) enum LogicalSchemaError {
    Sql(rusqlite::Error),
    Validation(String),
}

impl std::fmt::Display for LogicalSchemaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(error) => error.fmt(formatter),
            Self::Validation(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for LogicalSchemaError {}

impl From<rusqlite::Error> for LogicalSchemaError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

pub(crate) fn create_logical_schema(connection: &Connection) -> Result<(), LogicalSchemaError> {
    connection.execute_batch(LOGICAL_SCHEMA_SQL)?;
    Ok(())
}

pub(crate) fn validate_logical_schema(connection: &Connection) -> Result<(), LogicalSchemaError> {
    for (table, required_columns) in REQUIRED_TABLE_COLUMNS {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns: HashSet<String> = statement
            .query_map([], |row| row.get(1))?
            .collect::<rusqlite::Result<_>>()?;
        let missing: Vec<&str> = required_columns
            .iter()
            .copied()
            .filter(|column| !columns.contains(*column))
            .collect();
        if !missing.is_empty() {
            return Err(LogicalSchemaError::Validation(format!(
                "logical schema table {table} is missing columns: {}",
                missing.join(", ")
            )));
        }
    }

    for index in REQUIRED_INDEXES {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1
             )",
            [index],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(LogicalSchemaError::Validation(format!(
                "logical schema is missing index {index}"
            )));
        }
    }

    let foreign_key_violation: Option<(String, i64)> = connection
        .prepare("PRAGMA foreign_key_check")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .next()
        .transpose()?;
    if let Some((table, row_id)) = foreign_key_violation {
        return Err(LogicalSchemaError::Validation(format!(
            "logical schema foreign key violation in {table} row {row_id}"
        )));
    }

    let invalid_dependency_count: i64 = connection.query_row(
        "SELECT COUNT(*)
         FROM logical_record_dependencies AS dependency
         JOIN logical_record_heads AS head
           ON head.library_id = dependency.library_id
          AND head.generation_id = dependency.generation_id
          AND head.record_key = dependency.record_key
         WHERE head.state != 'live'",
        [],
        |row| row.get(0),
    )?;
    if invalid_dependency_count != 0 {
        return Err(LogicalSchemaError::Validation(
            "logical tombstones cannot retain object dependencies".to_string(),
        ));
    }

    let missing_page_dependency_count: i64 = connection.query_row(
        "SELECT COUNT(*)
         FROM logical_message_page_sources AS page
         LEFT JOIN logical_record_dependencies AS dependency
           ON dependency.library_id = page.library_id
          AND dependency.generation_id = page.generation_id
          AND dependency.record_key = page.record_key
          AND dependency.object_hash = page.object_hash
         WHERE dependency.object_hash IS NULL",
        [],
        |row| row.get(0),
    )?;
    if missing_page_dependency_count != 0 {
        return Err(LogicalSchemaError::Validation(
            "every logical message page must also be a record dependency".to_string(),
        ));
    }

    Ok(())
}
