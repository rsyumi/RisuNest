use super::*;

fn database_path(directory: &Path) -> PathBuf {
    directory.join("persistent").join("persistent.db")
}

fn assert_payload_alias_schema(connection: &rusqlite::Connection) {
    assert_eq!(
        table_columns(connection, "asset_aliases"),
        vec![
            ("generation".to_owned(), "TEXT".to_owned(), true, None, 1),
            ("logical_key".to_owned(), "TEXT".to_owned(), true, None, 3),
            ("object_hash".to_owned(), "TEXT".to_owned(), false, None, 0),
            ("kind".to_owned(), "TEXT".to_owned(), true, None, 2),
            ("size".to_owned(), "INTEGER".to_owned(), true, None, 0),
            ("mime".to_owned(), "TEXT".to_owned(), true, None, 0),
            ("name".to_owned(), "TEXT".to_owned(), true, None, 0),
            ("ext".to_owned(), "TEXT".to_owned(), true, None, 0),
            ("inlay_type".to_owned(), "TEXT".to_owned(), false, None, 0),
            ("width".to_owned(), "INTEGER".to_owned(), false, None, 0),
            ("height".to_owned(), "INTEGER".to_owned(), false, None, 0),
            (
                "metadata".to_owned(),
                "TEXT".to_owned(),
                true,
                Some("'{}'".to_owned()),
                0,
            ),
        ]
    );
    assert_eq!(
        table_columns(connection, "asset_owner_heads"),
        vec![
            ("generation".to_owned(), "TEXT".to_owned(), true, None, 1),
            ("owner_kind".to_owned(), "TEXT".to_owned(), true, None, 2),
            ("owner_locator".to_owned(), "TEXT".to_owned(), true, None, 3),
            ("present".to_owned(), "INTEGER".to_owned(), true, None, 0),
            (
                "manifest_hash".to_owned(),
                "TEXT".to_owned(),
                false,
                None,
                0
            ),
            (
                "entry_count".to_owned(),
                "INTEGER".to_owned(),
                true,
                None,
                0
            ),
        ]
    );
    assert_eq!(
        table_columns(connection, "cold_aliases"),
        vec![
            ("generation".to_owned(), "TEXT".to_owned(), true, None, 1),
            ("key".to_owned(), "TEXT".to_owned(), true, None, 2),
            ("object_hash".to_owned(), "TEXT".to_owned(), false, None, 0),
            ("size".to_owned(), "INTEGER".to_owned(), true, None, 0),
            ("metadata".to_owned(), "TEXT".to_owned(), true, None, 0),
        ]
    );
    assert_eq!(
        table_columns(connection, "asset_alias_replacement_candidates"),
        vec![
            ("generation".to_owned(), "TEXT".to_owned(), true, None, 1),
            ("kind".to_owned(), "TEXT".to_owned(), true, None, 2),
            ("logical_key".to_owned(), "TEXT".to_owned(), true, None, 3),
            ("object_hash".to_owned(), "TEXT".to_owned(), true, None, 4),
            ("byte_size".to_owned(), "INTEGER".to_owned(), true, None, 0),
        ]
    );
}

fn assert_asset_object_schema(connection: &rusqlite::Connection) {
    assert_eq!(
        table_columns(connection, "asset_objects"),
        vec![
            ("object_hash".to_owned(), "TEXT".to_owned(), false, None, 1),
            ("byte_size".to_owned(), "INTEGER".to_owned(), true, None, 0),
            (
                "created_at_ms".to_owned(),
                "INTEGER".to_owned(),
                true,
                None,
                0
            ),
        ]
    );
    assert_eq!(
        table_columns(connection, "asset_object_deletions"),
        vec![
            ("object_hash".to_owned(), "TEXT".to_owned(), false, None, 1),
            ("byte_size".to_owned(), "INTEGER".to_owned(), true, None, 0),
            ("physical_key".to_owned(), "TEXT".to_owned(), true, None, 0),
            ("state".to_owned(), "TEXT".to_owned(), true, None, 0),
            (
                "created_at_ms".to_owned(),
                "INTEGER".to_owned(),
                true,
                None,
                0
            ),
        ]
    );
    assert_eq!(
        table_columns(connection, "asset_gc_maintenance_state"),
        vec![
            ("singleton".to_owned(), "INTEGER".to_owned(), false, None, 1),
            (
                "catalog_cursor".to_owned(),
                "TEXT".to_owned(),
                false,
                None,
                0
            ),
        ]
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM asset_gc_maintenance_state WHERE singleton = 1
                 AND catalog_cursor IS NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count asset GC maintenance rows"),
        1
    );
}

fn assert_asset_repository_authority_schema(connection: &rusqlite::Connection, expected_rows: i64) {
    assert_eq!(
        table_columns(connection, "asset_repository_authority"),
        vec![
            ("generation".to_owned(), "TEXT".to_owned(), false, None, 1),
            ("value".to_owned(), "TEXT".to_owned(), true, None, 0),
        ]
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM asset_repository_authority",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count asset repository authority rows"),
        expected_rows
    );
}

fn assert_cold_payload_authority_schema(connection: &rusqlite::Connection, expected_rows: i64) {
    assert_eq!(
        table_columns(connection, "cold_payload_authority"),
        vec![
            ("generation".to_owned(), "TEXT".to_owned(), false, None, 1),
            ("value".to_owned(), "TEXT".to_owned(), true, None, 0),
        ]
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM cold_payload_authority", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count cold authority rows"),
        expected_rows
    );
}

fn assert_empty_logical_schema(connection: &rusqlite::Connection) {
    const TABLES: &[&str] = &[
        "logical_generation_session_pins",
        "logical_library_head",
        "logical_message_page_sources",
        "logical_peer_common_bases",
        "logical_record_dependencies",
        "logical_record_heads",
        "logical_sync_device_ack_proofs",
        "logical_sync_devices",
        "logical_sync_generations",
    ];
    const INDEXES: &[&str] = &[
        "logical_generation_session_pins_generation",
        "logical_message_page_sources_object",
        "logical_peer_common_bases_manifest",
        "logical_record_dependencies_object",
        "logical_record_heads_kind",
        "logical_record_heads_object",
        "logical_sync_device_ack_proofs_local_generation",
        "logical_sync_devices_status",
        "logical_sync_generations_manifest",
        "logical_sync_generations_parent",
    ];

    let schema_names = |kind: &str| {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type = ?1 AND name LIKE 'logical_%'
                 ORDER BY name",
            )
            .expect("prepare logical schema inventory query");
        statement
            .query_map([kind], |row| row.get::<_, String>(0))
            .expect("query logical schema inventory")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect logical schema inventory")
    };

    assert_eq!(
        schema_names("table"),
        TABLES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        schema_names("index"),
        INDEXES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    );
    for table in TABLES {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count logical schema rows");
        assert_eq!(count, 0, "{table} must start empty");
    }

    let expected_columns: &[(&str, &[&str])] = &[
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
            "logical_library_head",
            &["singleton", "library_id", "generation_id"],
        ),
        (
            "logical_generation_session_pins",
            &["session_id", "library_id", "generation_id", "created_at"],
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
        (
            "logical_sync_device_ack_proofs",
            &[
                "library_id",
                "device_id",
                "shared_generation_id",
                "shared_manifest_hash",
                "shared_generation_sequence",
                "local_generation_id",
                "local_manifest_hash",
                "local_generation_sequence",
                "verified_at",
            ],
        ),
        (
            "logical_sync_devices",
            &[
                "library_id",
                "device_id",
                "status",
                "acknowledged_generation_id",
                "acknowledged_manifest_hash",
                "acknowledged_generation_sequence",
                "registered_at",
                "acknowledged_at",
                "revoked_at",
                "forgotten_at",
            ],
        ),
    ];
    for (table, expected) in expected_columns {
        let actual = table_columns(connection, table)
            .into_iter()
            .map(|column| column.0)
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            expected
                .iter()
                .map(|column| (*column).to_owned())
                .collect::<Vec<_>>(),
            "{table} must contain only bounded logical metadata columns"
        );
    }
}

#[test]
fn empty_database_creates_the_whole_v1_schema() {
    let directory = tempfile::tempdir().expect("create fresh v1 directory");
    let store = PersistentStore::open(directory.path()).expect("open fresh v1 store");

    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read fresh schema version"),
        1
    );
    assert_payload_alias_schema(&store.connection);
    assert_asset_object_schema(&store.connection);
    assert_asset_repository_authority_schema(&store.connection, 1);
    assert_cold_payload_authority_schema(&store.connection, 1);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT value FROM asset_repository_authority WHERE generation = 'revision-0'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read fresh legacy authority"),
        r#"{"format":"legacy"}"#
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT value FROM cold_payload_authority WHERE generation = 'revision-0'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read fresh legacy cold authority"),
        r#"{"format":"legacy"}"#
    );
    assert_empty_logical_schema(&store.connection);
}

#[test]
fn existing_v1_database_revalidates_on_reopen() {
    let directory = tempfile::tempdir().expect("create v1 reopen directory");
    let store = PersistentStore::open(directory.path()).expect("create v1 store");
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("reopen v1 store");
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read reopened schema version"),
        1
    );
    assert_payload_alias_schema(&store.connection);
    assert_asset_object_schema(&store.connection);
    assert_empty_logical_schema(&store.connection);
    drop(store);

    let connection =
        rusqlite::Connection::open(database_path(directory.path())).expect("open v1 database");
    connection
        .execute_batch("DELETE FROM asset_gc_maintenance_state;")
        .expect("clear asset GC maintenance state");
    drop(connection);

    let Err(error) = PersistentStore::open(directory.path()) else {
        panic!("reopen must revalidate the v1 schema");
    };
    assert_eq!(
        error,
        StoreError::Validation {
            message: "asset GC maintenance state row is invalid".to_owned(),
        }
    );
}

#[test]
fn unknown_schema_version_is_rejected() {
    for version in [2_i64, 17] {
        let directory = tempfile::tempdir().expect("create unknown version directory");
        let store = PersistentStore::open(directory.path()).expect("create v1 store");
        drop(store);

        let connection =
            rusqlite::Connection::open(database_path(directory.path())).expect("open v1 database");
        connection
            .execute_batch(&format!("PRAGMA user_version = {version};"))
            .expect("stamp unknown schema version");
        drop(connection);

        let Err(error) = PersistentStore::open(directory.path()) else {
            panic!("unknown schema version {version} must be rejected");
        };
        assert_eq!(
            error,
            StoreError::Store {
                message: format!("unsupported persistent schema version {version}"),
            }
        );
    }
}
