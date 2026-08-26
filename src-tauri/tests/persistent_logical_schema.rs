#[path = "../src/persistent_store/logical_schema.rs"]
mod logical_schema;

use logical_schema::{create_logical_schema, validate_logical_schema};
use rusqlite::{params, Connection};

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const EMPTY_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn open_schema() -> Connection {
    let connection = Connection::open_in_memory().expect("open in-memory database");
    connection
        .pragma_update(None, "foreign_keys", true)
        .expect("enable foreign keys");
    create_logical_schema(&connection).expect("create logical schema");
    connection
}

fn insert_complete_generation(connection: &Connection) {
    connection
        .execute(
            "INSERT INTO logical_sync_generations (
                library_id,
                generation_id,
                generation_sequence,
                parent_generation_id,
                pds_generation,
                source_revision,
                state,
                manifest_hash,
                created_at,
                completed_at
             ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, 'complete', ?6, ?7, ?8)",
            params![
                "library-a",
                "generation-a",
                "12",
                "pds-a",
                41_i64,
                HASH_A,
                100_i64,
                101_i64
            ],
        )
        .expect("insert complete generation");
}

#[test]
fn creates_and_validates_compact_logical_schema() {
    let connection = open_schema();

    validate_logical_schema(&connection).expect("validate logical schema");

    let tables: Vec<String> = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name LIKE 'logical_%'
             ORDER BY name",
        )
        .expect("prepare table query")
        .query_map([], |row| row.get(0))
        .expect("query logical tables")
        .collect::<rusqlite::Result<_>>()
        .expect("collect logical tables");
    assert_eq!(
        tables,
        vec![
            "logical_message_page_sources",
            "logical_peer_common_bases",
            "logical_record_dependencies",
            "logical_record_heads",
            "logical_sync_generations",
        ]
    );

    let record_bytes_columns: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('logical_record_heads')
             WHERE name IN ('value', 'bytes', 'payload', 'record_bytes')",
            [],
            |row| row.get(0),
        )
        .expect("inspect record heads");
    assert_eq!(record_bytes_columns, 0);
}

#[test]
fn constrains_generation_and_record_head_state() {
    let connection = open_schema();

    let invalid_sequence = connection.execute(
        "INSERT INTO logical_sync_generations (
            library_id, generation_id, generation_sequence, pds_generation,
            source_revision, state, manifest_hash, created_at, completed_at
         ) VALUES ('library-a', 'bad-sequence', '01', 'pds-a', 0,
                   'complete', ?1, 1, 2)",
        [HASH_A],
    );
    assert!(invalid_sequence.is_err());

    let incomplete_complete = connection.execute(
        "INSERT INTO logical_sync_generations (
            library_id, generation_id, generation_sequence, pds_generation,
            source_revision, state, manifest_hash, created_at, completed_at
         ) VALUES ('library-a', 'bad-complete', '1', 'pds-a', 0,
                   'complete', NULL, 1, NULL)",
        [],
    );
    assert!(incomplete_complete.is_err());

    insert_complete_generation(&connection);
    connection
        .execute(
            "INSERT INTO logical_record_heads (
                library_id, generation_id, record_key, record_kind,
                state, object_hash, object_size, deleted_generation_sequence
             ) VALUES (?1, ?2, ?3, 'root', 'live', ?4, 1, NULL)",
            params!["library-a", "generation-a", "r1:root", HASH_B],
        )
        .expect("insert live root head");

    let live_without_hash = connection.execute(
        "INSERT INTO logical_record_heads (
            library_id, generation_id, record_key, record_kind,
            state, object_hash, object_size, deleted_generation_sequence
         ) VALUES ('library-a', 'generation-a', 'r1:plugin', 'plugin',
                   'live', NULL, 0, NULL)",
        [],
    );
    assert!(live_without_hash.is_err());

    let tombstone_with_hash = connection.execute(
        "INSERT INTO logical_record_heads (
            library_id, generation_id, record_key, record_kind,
            state, object_hash, object_size, deleted_generation_sequence
         ) VALUES ('library-a', 'generation-a', 'r1:cold', 'cold',
                   'tombstone', ?1, 1, '12')",
        [HASH_B],
    );
    assert!(tombstone_with_hash.is_err());
}

#[test]
fn records_dependencies_and_message_page_sources_without_payload_bytes() {
    let connection = open_schema();
    insert_complete_generation(&connection);
    connection
        .execute(
            "INSERT INTO logical_record_heads (
                library_id, generation_id, record_key, record_kind,
                state, object_hash, object_size, deleted_generation_sequence
             ) VALUES (?1, ?2, ?3, 'conversation', 'live', ?4, 10, NULL)",
            params!["library-a", "generation-a", "r1:conversation", HASH_A],
        )
        .expect("insert conversation head");
    connection
        .execute(
            "INSERT INTO logical_record_dependencies (
                library_id, generation_id, record_key, object_hash, object_size
             ) VALUES (?1, ?2, ?3, ?4, 20)",
            params!["library-a", "generation-a", "r1:conversation", HASH_B],
        )
        .expect("insert dependency");
    connection
        .execute(
            "INSERT INTO logical_message_page_sources (
                library_id, generation_id, record_key, page_index,
                first_message_index, message_count, object_hash, object_size
             ) VALUES (?1, ?2, ?3, 0, 0, 128, ?4, 20)",
            params!["library-a", "generation-a", "r1:conversation", HASH_B],
        )
        .expect("insert message page source");

    let duplicate_page_start = connection.execute(
        "INSERT INTO logical_message_page_sources (
            library_id, generation_id, record_key, page_index,
            first_message_index, message_count, object_hash, object_size
         ) VALUES ('library-a', 'generation-a', 'r1:conversation', 1,
                   0, 64, ?1, 10)",
        [HASH_A],
    );
    assert!(duplicate_page_start.is_err());

    let non_conversation_page = connection.execute(
        "INSERT INTO logical_message_page_sources (
            library_id, generation_id, record_key, page_index,
            first_message_index, message_count, object_hash, object_size
         ) VALUES ('library-a', 'generation-a', 'r1:root', 0,
                   0, 1, ?1, 10)",
        [HASH_A],
    );
    assert!(non_conversation_page.is_err());
}

#[test]
fn validation_requires_message_pages_to_be_manifest_dependencies() {
    let connection = open_schema();
    insert_complete_generation(&connection);
    connection
        .execute(
            "INSERT INTO logical_record_heads (
                library_id, generation_id, record_key, record_kind,
                state, object_hash, object_size, deleted_generation_sequence
             ) VALUES (?1, ?2, ?3, 'conversation', 'live', ?4, 10, NULL)",
            params!["library-a", "generation-a", "r1:conversation", HASH_A],
        )
        .expect("insert conversation head");
    connection
        .execute(
            "INSERT INTO logical_message_page_sources (
                library_id, generation_id, record_key, page_index,
                first_message_index, message_count, object_hash, object_size
             ) VALUES (?1, ?2, ?3, 0, 0, 32, ?4, 20)",
            params!["library-a", "generation-a", "r1:conversation", HASH_B],
        )
        .expect("insert message page source");

    let error = validate_logical_schema(&connection).expect_err("reject missing dependency");
    assert!(error
        .to_string()
        .contains("message page must also be a record dependency"));

    connection
        .execute(
            "INSERT INTO logical_record_dependencies (
                library_id, generation_id, record_key, object_hash, object_size
             ) VALUES (?1, ?2, ?3, ?4, 20)",
            params!["library-a", "generation-a", "r1:conversation", HASH_B],
        )
        .expect("insert page dependency");
    validate_logical_schema(&connection).expect("validate complete page metadata");
}

#[test]
fn stores_a_durable_explicit_common_base_per_peer_and_library() {
    let connection = open_schema();
    insert_complete_generation(&connection);

    connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params!["peer-a", "library-a", "generation-a", HASH_A, "12", 200_i64],
        )
        .expect("insert common base");
    connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(peer_id, library_id) DO UPDATE SET
                generation_id = excluded.generation_id,
                manifest_hash = excluded.manifest_hash,
                generation_sequence = excluded.generation_sequence,
                updated_at = excluded.updated_at",
            params!["peer-a", "library-a", "generation-a", HASH_A, "12", 201_i64],
        )
        .expect("replace common base atomically");

    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM logical_peer_common_bases",
            [],
            |row| row.get(0),
        )
        .expect("count peer bases");
    assert_eq!(count, 1);

    connection
        .execute(
            "DELETE FROM logical_sync_generations
             WHERE library_id = 'library-a' AND generation_id = 'generation-a'",
            [],
        )
        .expect("delete local generation metadata");
    let remaining_bases: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM logical_peer_common_bases",
            [],
            |row| row.get(0),
        )
        .expect("count durable peer bases");
    assert_eq!(remaining_bases, 1);
}

#[test]
fn validation_rejects_partial_or_wrong_schema() {
    let connection = Connection::open_in_memory().expect("open in-memory database");
    connection
        .execute_batch("CREATE TABLE logical_sync_generations (library_id TEXT);")
        .expect("create partial schema");

    let error = validate_logical_schema(&connection).expect_err("reject partial schema");
    assert!(error.to_string().contains("logical_sync_generations"));
}

#[test]
fn compact_index_supplies_manifest_object_sizes_without_record_bodies() {
    let connection = open_schema();
    insert_complete_generation(&connection);
    connection
        .execute(
            "INSERT INTO logical_record_heads (
                library_id, generation_id, record_key, record_kind,
                state, object_hash, object_size, deleted_generation_sequence
             ) VALUES (?1, ?2, ?3, 'conversation', 'live', ?4, 321, NULL)",
            params!["library-a", "generation-a", "r1:conversation", HASH_A],
        )
        .expect("insert sized conversation head");
    connection
        .execute(
            "INSERT INTO logical_record_dependencies (
                library_id, generation_id, record_key, object_hash, object_size
             ) VALUES (?1, ?2, ?3, ?4, 654)",
            params!["library-a", "generation-a", "r1:conversation", HASH_B],
        )
        .expect("insert sized dependency");
    connection
        .execute(
            "INSERT INTO logical_message_page_sources (
                library_id, generation_id, record_key, page_index,
                first_message_index, message_count, object_hash, object_size
             ) VALUES (?1, ?2, ?3, 0, 0, 128, ?4, 654)",
            params!["library-a", "generation-a", "r1:conversation", HASH_B],
        )
        .expect("insert sized message page source");

    let objects: Vec<(String, i64)> = connection
        .prepare(
            "SELECT object_hash, object_size
             FROM logical_record_heads
             WHERE library_id = ?1 AND generation_id = ?2 AND state = 'live'
             UNION
             SELECT object_hash, object_size
             FROM logical_record_dependencies
             WHERE library_id = ?1 AND generation_id = ?2
             ORDER BY object_hash",
        )
        .expect("prepare compact manifest object query")
        .query_map(params!["library-a", "generation-a"], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .expect("query compact manifest objects")
        .collect::<rusqlite::Result<_>>()
        .expect("collect compact manifest objects");
    assert_eq!(
        objects,
        vec![(HASH_A.to_string(), 321), (HASH_B.to_string(), 654)]
    );

    let page_object: (String, i64) = connection
        .query_row(
            "SELECT object_hash, object_size
             FROM logical_message_page_sources
             WHERE library_id = ?1 AND generation_id = ?2 AND record_key = ?3",
            params!["library-a", "generation-a", "r1:conversation"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read compact page source");
    assert_eq!(page_object, (HASH_B.to_string(), 654));
}

#[test]
fn rejects_invalid_object_sizes_and_empty_hash_size_mismatches() {
    let connection = open_schema();
    insert_complete_generation(&connection);

    let negative_head = connection.execute(
        "INSERT INTO logical_record_heads (
            library_id, generation_id, record_key, record_kind,
            state, object_hash, object_size, deleted_generation_sequence
         ) VALUES ('library-a', 'generation-a', 'r1:negative', 'root',
                   'live', ?1, -1, NULL)",
        [HASH_A],
    );
    assert!(negative_head.is_err());

    let malformed_hash = connection.execute(
        "INSERT INTO logical_record_heads (
            library_id, generation_id, record_key, record_kind,
            state, object_hash, object_size, deleted_generation_sequence
         ) VALUES ('library-a', 'generation-a', 'r1:bad-hash', 'root',
                   'live', 'not-a-sha256', 1, NULL)",
        [],
    );
    assert!(malformed_hash.is_err());

    let zero_sized_non_empty_hash = connection.execute(
        "INSERT INTO logical_record_heads (
            library_id, generation_id, record_key, record_kind,
            state, object_hash, object_size, deleted_generation_sequence
         ) VALUES ('library-a', 'generation-a', 'r1:wrong-zero', 'root',
                   'live', ?1, 0, NULL)",
        [HASH_A],
    );
    assert!(zero_sized_non_empty_hash.is_err());

    let positive_sized_empty_hash = connection.execute(
        "INSERT INTO logical_record_heads (
            library_id, generation_id, record_key, record_kind,
            state, object_hash, object_size, deleted_generation_sequence
         ) VALUES ('library-a', 'generation-a', 'r1:wrong-empty', 'root',
                   'live', ?1, 1, NULL)",
        [EMPTY_HASH],
    );
    assert!(positive_sized_empty_hash.is_err());

    connection
        .execute(
            "INSERT INTO logical_record_heads (
                library_id, generation_id, record_key, record_kind,
                state, object_hash, object_size, deleted_generation_sequence
             ) VALUES ('library-a', 'generation-a', 'r1:empty', 'root',
                       'live', ?1, 0, NULL)",
            [EMPTY_HASH],
        )
        .expect("accept canonical empty object");

    let missing_dependency_size = connection.execute(
        "INSERT INTO logical_record_dependencies (
            library_id, generation_id, record_key, object_hash
         ) VALUES ('library-a', 'generation-a', 'r1:empty', ?1)",
        [EMPTY_HASH],
    );
    assert!(missing_dependency_size.is_err());

    let zero_sized_dependency_with_non_empty_hash = connection.execute(
        "INSERT INTO logical_record_dependencies (
            library_id, generation_id, record_key, object_hash, object_size
         ) VALUES ('library-a', 'generation-a', 'r1:empty', ?1, 0)",
        [HASH_A],
    );
    assert!(zero_sized_dependency_with_non_empty_hash.is_err());

    connection
        .execute(
            "INSERT INTO logical_record_heads (
                library_id, generation_id, record_key, record_kind,
                state, object_hash, object_size, deleted_generation_sequence
             ) VALUES ('library-a', 'generation-a', 'r1:conversation',
                       'conversation', 'live', ?1, 10, NULL)",
            [HASH_A],
        )
        .expect("insert conversation for page constraint");
    let positive_sized_page_with_empty_hash = connection.execute(
        "INSERT INTO logical_message_page_sources (
            library_id, generation_id, record_key, page_index,
            first_message_index, message_count, object_hash, object_size
         ) VALUES ('library-a', 'generation-a', 'r1:conversation',
                   0, 0, 1, ?1, 1)",
        [EMPTY_HASH],
    );
    assert!(positive_sized_page_with_empty_hash.is_err());
}
