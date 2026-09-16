use super::*;

fn database_path(directory: &Path) -> PathBuf {
    directory.join("persistent").join("persistent.sqlite")
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

fn assert_no_peer_schema(connection: &rusqlite::Connection) {
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name LIKE 'logical_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "fresh and reopened stores must not contain peer tables or indexes"
    );
}

#[test]
fn empty_database_creates_the_whole_current_schema() {
    let directory = tempfile::tempdir().expect("create fresh current directory");
    let store = PersistentStore::open(directory.path()).expect("open fresh current store");

    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read fresh schema version"),
        super::super::schema::SCHEMA_VERSION as i64
    );
    assert_payload_alias_schema(&store.connection);
    assert_asset_object_schema(&store.connection);
    assert_asset_repository_authority_schema(&store.connection, 1);
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
    assert_no_peer_schema(&store.connection);
}

#[test]
fn existing_current_database_revalidates_on_reopen() {
    let directory = tempfile::tempdir().expect("create current reopen directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("reopen current store");
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read reopened schema version"),
        super::super::schema::SCHEMA_VERSION as i64
    );
    assert_payload_alias_schema(&store.connection);
    assert_asset_object_schema(&store.connection);
    assert_no_peer_schema(&store.connection);
    drop(store);

    let connection =
        rusqlite::Connection::open(database_path(directory.path())).expect("open current database");
    connection
        .execute_batch("DELETE FROM asset_gc_maintenance_state;")
        .expect("clear asset GC maintenance state");
    drop(connection);

    let Err(error) = PersistentStore::open(directory.path()) else {
        panic!("reopen must revalidate the current schema");
    };
    assert_eq!(
        error,
        StoreError::Validation {
            message: "asset GC maintenance state row is invalid".to_owned(),
        }
    );
}

#[test]
fn a_broken_device_store_is_reported_without_blocking_the_library() {
    let directory = tempfile::tempdir().expect("create device failure directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    assert!(store.device_store().is_ok());
    assert!(store
        .open_native_job_store()
        .expect("open native job store")
        .device_store()
        .is_ok());
    drop(store);

    let device_path = directory
        .path()
        .join("persistent")
        .join(super::super::device_store::DEVICE_DATABASE_FILE);
    let connection = rusqlite::Connection::open(&device_path).expect("open device database");
    connection
        .execute_batch("PRAGMA user_version = 17;")
        .expect("stamp unknown device schema version");
    drop(connection);

    let store =
        PersistentStore::open(directory.path()).expect("library opens without the device store");
    assert_eq!(store.revision().expect("read library revision"), 0);
    let Err(error) = store.device_store() else {
        panic!("a damaged device store must be reported");
    };
    assert_eq!(
        error,
        StoreError::Store {
            message: "device store is unavailable: unsupported device schema version 17".to_owned(),
        }
    );
}

#[test]
fn unknown_schema_version_is_rejected() {
    for version in [1_i64, 2, 3, 17] {
        let directory = tempfile::tempdir().expect("create unknown version directory");
        let store = PersistentStore::open(directory.path()).expect("create current store");
        drop(store);

        let connection = rusqlite::Connection::open(database_path(directory.path()))
            .expect("open current database");
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
