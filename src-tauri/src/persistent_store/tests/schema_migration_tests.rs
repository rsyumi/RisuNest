use super::*;

fn create_v1_database(path: &Path) {
    fs::create_dir_all(path.parent().expect("v1 database parent")).expect("create v1 parent");
    let connection = rusqlite::Connection::open(path).expect("create v1 database");
    connection
        .execute_batch(
            "
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE app_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE snapshot_leases (generation TEXT PRIMARY KEY, created_at INTEGER NOT NULL);
            CREATE TABLE root (generation TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE characters (
                generation TEXT NOT NULL, character_id TEXT NOT NULL,
                configured_index INTEGER NOT NULL, recent_at INTEGER NOT NULL,
                trashed INTEGER NOT NULL, name TEXT NOT NULL, image TEXT,
                conversation_count INTEGER NOT NULL, detail TEXT NOT NULL,
                PRIMARY KEY (generation, character_id)
            );
            CREATE INDEX characters_configured ON characters (generation, configured_index);
            CREATE INDEX characters_recent ON characters (generation, recent_at DESC, configured_index);
            CREATE TABLE conversations (
                generation TEXT NOT NULL, character_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL, configured_index INTEGER NOT NULL,
                recent_at INTEGER NOT NULL, name TEXT NOT NULL,
                message_count INTEGER NOT NULL, detail TEXT NOT NULL,
                PRIMARY KEY (generation, character_id, conversation_id)
            );
            CREATE INDEX conversations_configured
                ON conversations (generation, character_id, configured_index);
            CREATE INDEX conversations_recent
                ON conversations (generation, character_id, recent_at DESC, configured_index);
            CREATE TABLE messages (
                generation TEXT NOT NULL, character_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL, message_index INTEGER NOT NULL,
                message_id TEXT, value TEXT NOT NULL,
                PRIMARY KEY (generation, character_id, conversation_id, message_index)
            );
            CREATE INDEX messages_by_id
                ON messages (generation, character_id, conversation_id, message_id);
            INSERT INTO meta VALUES ('currentRevision', '7');
            INSERT INTO meta VALUES ('activeGeneration', '\"revision-7\"');
            PRAGMA user_version = 1;
            ",
        )
        .expect("create v1 schema");
    connection
        .execute(
            "INSERT INTO root VALUES (?1, ?2)",
            rusqlite::params![
                "revision-7",
                json!({
                    "username": "V1 active",
                    "characters": ["strip"],
                    "botPresets": [
                        { "name": "V1 first", "image": "v1.png" },
                        { "name": "V1 second" }
                    ],
                    "pluginCustomStorage": {
                        "active-memory": { "turns": [1, 2, 3] }
                    }
                })
                .to_string()
            ],
        )
        .expect("insert v1 active root");
    connection
        .execute(
            "INSERT INTO root VALUES (?1, ?2)",
            rusqlite::params![
                "revision-old",
                json!({
                    "username": "V1 old",
                    "botPresets": [{ "name": "Old preset" }],
                    "pluginCustomStorage": { "old-memory": "preserved" }
                })
                .to_string()
            ],
        )
        .expect("insert v1 old root");
    let detail = json!({
        "type": "character",
        "chaId": "v1-character",
        "name": "V1 character",
        "creatorNotes": "migrated notes",
        "trashTime": 123
    });
    connection
        .execute(
            "INSERT INTO characters
             (generation, character_id, configured_index, recent_at, trashed, name, image,
              conversation_count, detail)
             VALUES (?1, ?2, 0, 0, 1, ?3, NULL, 0, ?4)",
            rusqlite::params![
                "revision-7",
                "v1-character",
                "V1 character",
                detail.to_string()
            ],
        )
        .expect("insert v1 character");
}

fn create_v2_database(path: &Path) {
    create_v1_database(path);
    let connection = rusqlite::Connection::open(path).expect("open v1 database for v2 setup");
    connection
        .execute_batch(
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
            PRAGMA user_version = 2;
            ",
        )
        .expect("create v2 schema additions");
    connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params![
                "revision-7",
                json!({
                    "username": "V2 active",
                    "pluginCustomStorage": { "v2-memory": { "lossless": true } }
                })
                .to_string()
            ],
        )
        .expect("write v2 plugin root");
}

fn create_v2_database_with_lease(path: &Path) {
    create_v2_database(path);
    let connection = rusqlite::Connection::open(path).expect("open v2 fixture database");
    connection
        .execute_batch(
            "
            INSERT INTO root (generation, value)
                VALUES ('snapshot-7-v2fixture',
                        '{\"username\":\"V2 leased\",\"pluginCustomStorage\":{\"leased-zero\":0}}');
            INSERT INTO snapshot_leases (generation, created_at)
                VALUES ('snapshot-7-v2fixture', 4102444800000);
            ",
        )
        .expect("create v2 fixture lease");
}

fn assert_j2_v8_payload_schema(connection: &rusqlite::Connection) {
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
}

fn assert_m4_v9_authority_schema(connection: &rusqlite::Connection, expected_rows: i64) {
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
            .expect("count M4 authority rows"),
        expected_rows
    );
}

fn assert_cold_v11_authority_schema(connection: &rusqlite::Connection, expected_rows: i64) {
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

fn insert_logical_schema_fixture(connection: &rusqlite::Connection) {
    connection
        .execute_batch(
            r#"
            INSERT INTO logical_sync_generations (
                library_id, generation_id, generation_sequence, parent_generation_id,
                pds_generation, source_revision, state, manifest_hash, created_at, completed_at
            ) VALUES (
                'library-fixture', 'logical-generation-1', '1', NULL,
                'revision-0', 0, 'complete',
                'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 10, 11
            );
            INSERT INTO logical_record_heads (
                library_id, generation_id, record_key, record_kind, state,
                object_hash, object_size, deleted_generation_sequence
            ) VALUES (
                'library-fixture', 'logical-generation-1', 'r1:conversation:fixture',
                'conversation', 'live',
                '1111111111111111111111111111111111111111111111111111111111111111',
                11, NULL
            );
            INSERT INTO logical_record_dependencies (
                library_id, generation_id, record_key, object_hash, object_size
            ) VALUES (
                'library-fixture', 'logical-generation-1', 'r1:conversation:fixture',
                '2222222222222222222222222222222222222222222222222222222222222222',
                22
            );
            INSERT INTO logical_message_page_sources (
                library_id, generation_id, record_key, record_kind, page_index,
                first_message_index, message_count, object_hash, object_size
            ) VALUES (
                'library-fixture', 'logical-generation-1', 'r1:conversation:fixture',
                'conversation', 0, 0, 4,
                '2222222222222222222222222222222222222222222222222222222222222222',
                22
            );
            INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
            ) VALUES (
                'peer-fixture', 'library-fixture', 'logical-generation-1',
                'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
                '1', 12
            );
            "#,
        )
        .expect("insert logical schema fixture");
}

fn assert_logical_schema_fixture(connection: &rusqlite::Connection) {
    let generation: (String, String, i64) = connection
        .query_row(
            "SELECT generation_sequence, manifest_hash, source_revision
             FROM logical_sync_generations
             WHERE library_id = 'library-fixture' AND generation_id = 'logical-generation-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read retained logical generation");
    assert_eq!(generation, ("1".to_owned(), "aa".repeat(32), 0));

    for table in [
        "logical_record_heads",
        "logical_record_dependencies",
        "logical_message_page_sources",
        "logical_peer_common_bases",
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count retained logical rows");
        assert_eq!(count, 1, "{table} fixture row must be retained");
    }
}

#[test]
fn fresh_schema_v16_contains_dual_authority_and_empty_p4_logical_tables() {
    let directory = tempfile::tempdir().expect("create fresh v16 directory");
    let store = PersistentStore::open(directory.path()).expect("open fresh v16 store");

    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read fresh schema version"),
        17
    );
    assert_j2_v8_payload_schema(&store.connection);
    assert_m4_v9_authority_schema(&store.connection, 1);
    assert_cold_v11_authority_schema(&store.connection, 1);
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
fn schema_v11_backfills_cold_authority_for_active_and_leased_v10_generations() {
    let directory = tempfile::tempdir().expect("create v10 cold migration directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    store
        .connection
        .execute_batch(
            r#"
            INSERT INTO root (generation, value)
                VALUES ('snapshot-v10-cold', '{"username":"leased"}');
            INSERT INTO snapshot_leases (lease, generation, revision, created_at)
                VALUES ('snapshot-v10-cold', 'snapshot-v10-cold', 0, 4102444800000);
            DROP TABLE cold_payload_authority;
            DROP INDEX logical_sync_device_ack_proofs_local_generation;
            DROP TABLE logical_sync_device_ack_proofs;
            DROP INDEX logical_sync_devices_status;
            DROP TABLE logical_sync_devices;
            DROP TABLE asset_gc_maintenance_state;
            DROP TABLE asset_alias_replacement_candidates;
            DROP TABLE asset_object_deletions;
            DROP TABLE asset_objects;
            PRAGMA user_version = 10;
            "#,
        )
        .expect("create v10 cold fixture");
    drop(store);

    let mut connection =
        rusqlite::Connection::open(directory.path().join("persistent").join("persistent.db"))
            .expect("reopen v10 cold fixture");
    super::schema::initialize(&mut connection).expect("migrate v10 cold authority");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read migrated cold schema version"),
        17
    );
    assert_cold_v11_authority_schema(&connection, 2);
    for generation in ["revision-0", "snapshot-v10-cold"] {
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM cold_payload_authority WHERE generation = ?1",
                    [generation],
                    |row| row.get::<_, String>(0),
                )
                .expect("read backfilled cold authority"),
            r#"{"format":"legacy"}"#
        );
    }
}

#[test]
fn schema_v10_migrates_v8_through_m4_v9_and_preserves_j2_data() {
    let directory = tempfile::tempdir().expect("create v8 migration directory");
    let store = PersistentStore::open(directory.path()).expect("create v8 store");
    let cas_sentinel = directory
        .path()
        .join("assets-v2/objects/aa/preserved-object");
    fs::create_dir_all(cas_sentinel.parent().expect("CAS sentinel parent"))
        .expect("create CAS sentinel parent");
    fs::write(&cas_sentinel, b"must remain untouched").expect("write CAS sentinel");
    store
        .connection
        .execute_batch(
            r#"
            UPDATE root SET value = '{"v8":"preserved"}' WHERE generation = 'revision-0';
            INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height, metadata
            ) VALUES
                (
                    'revision-0', 'shared-key',
                    '1111111111111111111111111111111111111111111111111111111111111111',
                    'asset', 17, 'application/octet-stream', 'asset-name', 'BIN',
                    NULL, NULL, NULL, '{"source":"v8-asset"}'
                ),
                (
                    'revision-0', 'shared-key',
                    '2222222222222222222222222222222222222222222222222222222222222222',
                    'inlay', 23, 'image/webp', 'inlay-name', 'WebP',
                    'image', 640, 480, '{"source":"v8-inlay","width":640,"height":480}'
                );
            INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
            ) VALUES (
                'revision-0', 'root-module-assets', '0', 1,
                '3333333333333333333333333333333333333333333333333333333333333333', 2
            );
            INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
            VALUES (
                'revision-0', 'cold-preserved',
                '4444444444444444444444444444444444444444444444444444444444444444',
                29, '{"source":"v8-cold"}'
            );
            DROP TABLE logical_generation_session_pins;
            DROP TABLE logical_library_head;
            DROP TABLE logical_message_page_sources;
            DROP TABLE logical_peer_common_bases;
            DROP TABLE logical_record_dependencies;
            DROP TABLE logical_record_heads;
            DROP TABLE logical_sync_generations;
            DROP TABLE cold_payload_authority;
            DROP TABLE asset_repository_authority;
            DROP INDEX logical_sync_device_ack_proofs_local_generation;
            DROP TABLE logical_sync_device_ack_proofs;
            DROP INDEX logical_sync_devices_status;
            DROP TABLE logical_sync_devices;
            DROP TABLE asset_gc_maintenance_state;
            DROP TABLE asset_alias_replacement_candidates;
            DROP TABLE asset_object_deletions;
            DROP TABLE asset_objects;
            PRAGMA user_version = 8;
            "#,
        )
        .expect("seed final J2 v8 data");
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read v8 fixture version"),
        8
    );
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("migrate v8 store to v10");

    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read migrated schema version"),
        17
    );
    assert_j2_v8_payload_schema(&store.connection);
    assert_m4_v9_authority_schema(&store.connection, 1);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT value FROM asset_repository_authority WHERE generation = 'revision-0'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read migrated legacy authority"),
        r#"{"format":"legacy"}"#
    );
    assert_empty_logical_schema(&store.connection);
    assert_eq!(
        store.read_root(None).expect("read preserved v8 root").value,
        json!({ "v8": "preserved" })
    );
    assert_eq!(
        store
            .read_asset_alias("asset", "shared-key", None)
            .expect("read preserved ordinary alias")
            .expect("ordinary alias exists")
            .value,
        AssetAlias {
            key: "shared-key".to_owned(),
            object_hash: Some("11".repeat(32)),
            kind: "asset".to_owned(),
            size: 17,
            mime: "application/octet-stream".to_owned(),
            name: "asset-name".to_owned(),
            ext: "BIN".to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({ "source": "v8-asset" }),
        }
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", "shared-key", None)
            .expect("read preserved Inlay alias")
            .expect("Inlay alias exists")
            .value,
        AssetAlias {
            key: "shared-key".to_owned(),
            object_hash: Some("22".repeat(32)),
            kind: "inlay".to_owned(),
            size: 23,
            mime: "image/webp".to_owned(),
            name: "inlay-name".to_owned(),
            ext: "WebP".to_owned(),
            inlay_type: Some("image".to_owned()),
            width: Some(640),
            height: Some(480),
            metadata: json!({
                "source": "v8-inlay",
                "width": 640,
                "height": 480
            }),
        }
    );
    assert_eq!(
        store
            .read_asset_owner_head(&AssetOwnerLocator::RootModuleAssets { index: 0 }, None)
            .expect("read preserved owner head")
            .expect("owner head exists")
            .value,
        AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 0 },
            "33".repeat(32),
            2,
        )
    );
    assert_eq!(
        store
            .read_cold_alias("cold-preserved", None)
            .expect("read preserved cold alias")
            .expect("cold alias exists")
            .value,
        ColdAlias {
            key: "cold-preserved".to_owned(),
            object_hash: Some("44".repeat(32)),
            size: 29,
            metadata: json!({ "source": "v8-cold" }),
        }
    );
    assert_eq!(
        fs::read(&cas_sentinel).expect("read untouched CAS sentinel"),
        b"must remain untouched"
    );
}

#[test]
fn schema_v10_migration_collision_preserves_completed_m4_v9_and_v8_rows() {
    let directory = tempfile::tempdir().expect("create v8 rollback directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    store
        .connection
        .execute_batch(
            r#"
            UPDATE root SET value = '{"rollback":"preserved"}' WHERE generation = 'revision-0';
            INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height, metadata
            ) VALUES (
                'revision-0', 'rollback-asset', NULL, 'asset', 0,
                'application/octet-stream', 'Rollback asset', 'bin',
                NULL, NULL, NULL, '{"source":"v8"}'
            );
            INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
            ) VALUES (
                'revision-0', 'root-module-assets', '0', 1,
                '5555555555555555555555555555555555555555555555555555555555555555', 1
            );
            INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
            VALUES ('revision-0', 'rollback-cold', NULL, 0, '{"source":"v8"}');
            DROP TABLE logical_generation_session_pins;
            DROP TABLE logical_library_head;
            DROP TABLE logical_message_page_sources;
            DROP TABLE logical_peer_common_bases;
            DROP TABLE logical_record_dependencies;
            DROP TABLE logical_record_heads;
            DROP TABLE logical_sync_generations;
            DROP TABLE cold_payload_authority;
            DROP TABLE asset_repository_authority;
            DROP INDEX logical_sync_device_ack_proofs_local_generation;
            DROP TABLE logical_sync_device_ack_proofs;
            DROP INDEX logical_sync_devices_status;
            DROP TABLE logical_sync_devices;
            DROP TABLE asset_gc_maintenance_state;
            DROP TABLE asset_alias_replacement_candidates;
            DROP TABLE asset_object_deletions;
            DROP TABLE asset_objects;
            CREATE TABLE logical_record_heads (collision_marker TEXT NOT NULL);
            PRAGMA user_version = 8;
            "#,
        )
        .expect("create colliding v8 fixture");
    drop(store);

    let error = match PersistentStore::open(directory.path()) {
        Ok(_) => panic!("logical table collision must fail migration"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("logical_record_heads"));

    let database_path = directory.path().join("persistent/persistent.db");
    let connection = rusqlite::Connection::open(database_path).expect("reopen retained M4 v9 db");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read rolled-back schema version"),
        9
    );
    assert_j2_v8_payload_schema(&connection);
    assert_m4_v9_authority_schema(&connection, 1);
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM asset_repository_authority WHERE generation = 'revision-0'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read retained M4 legacy authority"),
        r#"{"format":"legacy"}"#
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM root WHERE generation = 'revision-0'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read preserved v8 root"),
        r#"{"rollback":"preserved"}"#
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT metadata FROM cold_aliases
                 WHERE generation = 'revision-0' AND key = 'rollback-cold'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read preserved v8 cold alias"),
        r#"{"source":"v8"}"#
    );
    for table in ["asset_aliases", "asset_owner_heads", "cold_aliases"] {
        let count: i64 = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE generation = 'revision-0'"),
                [],
                |row| row.get(0),
            )
            .expect("count preserved v8 J2 rows");
        assert_eq!(count, 1, "{table} row must survive migration rollback");
    }
    assert_eq!(
        table_columns(&connection, "logical_record_heads"),
        vec![(
            "collision_marker".to_owned(),
            "TEXT".to_owned(),
            true,
            None,
            0,
        )]
    );
    let partial_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'table'
               AND name IN ('logical_sync_generations', 'logical_record_dependencies')",
            [],
            |row| row.get(0),
        )
        .expect("count rolled-back logical tables");
    assert_eq!(partial_table_count, 0);
}

#[test]
fn schema_v10_migration_collision_rolls_back_v9_logical_ddl_only() {
    let directory = tempfile::tempdir().expect("create M4 v9 rollback directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    store
        .connection
        .execute_batch(
            r#"
            DROP TABLE logical_generation_session_pins;
            DROP TABLE logical_library_head;
            DROP TABLE logical_message_page_sources;
            DROP TABLE logical_peer_common_bases;
            DROP TABLE logical_record_dependencies;
            DROP TABLE logical_record_heads;
            DROP TABLE logical_sync_generations;
            DROP TABLE cold_payload_authority;
            DROP INDEX logical_sync_device_ack_proofs_local_generation;
            DROP TABLE logical_sync_device_ack_proofs;
            DROP INDEX logical_sync_devices_status;
            DROP TABLE logical_sync_devices;
            DROP TABLE asset_gc_maintenance_state;
            DROP TABLE asset_alias_replacement_candidates;
            DROP TABLE asset_object_deletions;
            DROP TABLE asset_objects;
            CREATE TABLE logical_record_heads (collision_marker TEXT NOT NULL);
            PRAGMA user_version = 9;
            "#,
        )
        .expect("create colliding M4 v9 fixture");
    drop(store);

    let error = match PersistentStore::open(directory.path()) {
        Ok(_) => panic!("logical table collision must fail v9 to v10 migration"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("logical_record_heads"));

    let database_path = directory.path().join("persistent/persistent.db");
    let connection = rusqlite::Connection::open(database_path).expect("reopen rolled-back v9 db");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read rolled-back M4 schema version"),
        9
    );
    assert_m4_v9_authority_schema(&connection, 1);
    assert_eq!(
        table_columns(&connection, "logical_record_heads"),
        vec![(
            "collision_marker".to_owned(),
            "TEXT".to_owned(),
            true,
            None,
            0,
        )]
    );
    let partial_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'table'
               AND name IN ('logical_sync_generations', 'logical_record_dependencies')",
            [],
            |row| row.get(0),
        )
        .expect("count rolled-back logical tables");
    assert_eq!(partial_table_count, 0);
}

#[test]
fn schema_v10_logical_rows_survive_snapshot_restore_and_reopen() {
    let directory = tempfile::tempdir().expect("create v10 snapshot directory");
    let store = PersistentStore::open(directory.path()).expect("open v10 store");
    insert_logical_schema_fixture(&store.connection);
    let snapshot = store
        .snapshot_create("logical-v10")
        .expect("create v10 logical snapshot");
    store
        .connection
        .execute_batch(
            "DELETE FROM logical_message_page_sources;
             DELETE FROM logical_record_dependencies;
             DELETE FROM logical_record_heads;
             DELETE FROM logical_peer_common_bases;
             DELETE FROM logical_sync_generations;",
        )
        .expect("change logical rows after snapshot");
    store
        .snapshot_restore_request(Path::new(&snapshot.path))
        .expect("request v10 logical snapshot restore");
    drop(store);

    let restored = PersistentStore::open(directory.path()).expect("restore v10 snapshot on reopen");
    assert_eq!(
        restored
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read restored schema version"),
        17
    );
    assert_logical_schema_fixture(&restored.connection);
    drop(restored);

    let reopened = PersistentStore::open(directory.path()).expect("reopen restored v10 store");
    assert_logical_schema_fixture(&reopened.connection);
}

#[test]
fn schema_v8_adds_empty_payload_alias_tables_to_v5() {
    let directory = tempfile::tempdir().expect("create v5 migration directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    drop(store);
    let database_path = directory.path().join("persistent/persistent.db");
    let connection = rusqlite::Connection::open(&database_path).expect("open migration fixture");
    connection
        .execute_batch(
            "DROP TABLE asset_aliases;
             DROP TABLE asset_owner_heads;
             DROP TABLE cold_aliases;
             DROP TABLE cold_payload_authority;
             DROP TABLE asset_repository_authority;
             DROP INDEX logical_sync_device_ack_proofs_local_generation;
             DROP TABLE logical_sync_device_ack_proofs;
             DROP INDEX logical_sync_devices_status;
             DROP TABLE logical_sync_devices;
             DROP TABLE asset_gc_maintenance_state;
             DROP TABLE asset_alias_replacement_candidates;
             DROP TABLE asset_object_deletions;
             DROP TABLE asset_objects;
             DROP TABLE logical_generation_session_pins;
             DROP TABLE logical_library_head;
             DROP TABLE logical_message_page_sources;
             DROP TABLE logical_peer_common_bases;
             DROP TABLE logical_record_dependencies;
             DROP TABLE logical_record_heads;
             DROP TABLE logical_sync_generations;
             PRAGMA user_version = 5;",
        )
        .expect("downgrade fixture schema marker");
    drop(connection);

    let store = PersistentStore::open(directory.path()).expect("migrate v5 store");
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated schema version");
    let alias_count: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM asset_aliases", [], |row| row.get(0))
        .expect("count migrated aliases");
    let head_count: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM asset_owner_heads", [], |row| {
            row.get(0)
        })
        .expect("count migrated owner heads");

    assert_eq!(version, 17);
    assert_eq!(alias_count, 0);
    assert_eq!(head_count, 0);
    assert_eq!(
        store.read_root(None).expect("read migrated root").revision,
        0
    );
}

#[test]
fn schema_v10_chains_m4_authority_and_p4_logical_migrations_from_v8() {
    let directory = tempfile::tempdir().expect("create v8 authority migration directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    store
        .connection
        .execute_batch(
            "DROP TABLE cold_payload_authority;
             DROP TABLE asset_repository_authority;
             DROP INDEX logical_sync_device_ack_proofs_local_generation;
             DROP TABLE logical_sync_device_ack_proofs;
             DROP INDEX logical_sync_devices_status;
             DROP TABLE logical_sync_devices;
             DROP TABLE asset_gc_maintenance_state;
             DROP TABLE asset_alias_replacement_candidates;
             DROP TABLE asset_object_deletions;
             DROP TABLE asset_objects;
             DROP TABLE logical_generation_session_pins;
             DROP TABLE logical_library_head;
             DROP TABLE logical_message_page_sources;
             DROP TABLE logical_peer_common_bases;
             DROP TABLE logical_record_dependencies;
             DROP TABLE logical_record_heads;
             DROP TABLE logical_sync_generations;
             PRAGMA user_version = 8;",
        )
        .expect("downgrade authority schema fixture");
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("migrate v8 authority store");
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated schema version");
    assert_eq!(version, 17);
    assert_eq!(
        store
            .read_asset_repository_authority(None)
            .expect("read migrated legacy authority")
            .value,
        AssetRepositoryAuthorityState::Legacy
    );
}

#[test]
fn schema_v8_migrates_v6_alias_without_changing_its_value() {
    let directory = tempfile::tempdir().expect("create v6 alias migration directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    let alias = AssetAlias {
        key: "legacy/shared-key".to_owned(),
        object_hash: Some("ab".repeat(32)),
        kind: "asset".to_owned(),
        size: 17,
        mime: "application/octet-stream".to_owned(),
        name: "Legacy alias".to_owned(),
        ext: "BIN".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({
            "name": "Legacy alias",
            "ext": "BIN",
            "mime": "application/octet-stream"
        }),
    };
    let inlay = AssetAlias {
        key: "legacy/inlay-key".to_owned(),
        object_hash: Some("cd".repeat(32)),
        kind: "inlay".to_owned(),
        size: 23,
        mime: "image/webp".to_owned(),
        name: "Legacy Inlay".to_owned(),
        ext: "WebP".to_owned(),
        inlay_type: Some("image".to_owned()),
        width: Some(640),
        height: None,
        metadata: json!({
            "name": "Legacy Inlay",
            "ext": "WebP",
            "mime": "image/webp",
            "inlayType": "image",
            "width": 640
        }),
    };
    for legacy in [&alias, &inlay] {
        store
            .connection
            .execute(
                "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height
             ) VALUES ('revision-0', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    legacy.key,
                    legacy.object_hash,
                    legacy.kind,
                    legacy.size,
                    legacy.mime,
                    legacy.name,
                    legacy.ext,
                    legacy.inlay_type,
                    legacy.width,
                    legacy.height,
                ],
            )
            .expect("insert legacy alias");
    }
    store
        .connection
        .execute_batch(
            "
            DROP INDEX asset_aliases_generation;
            ALTER TABLE asset_aliases RENAME TO asset_aliases_v8;
            CREATE TABLE asset_aliases (
                generation TEXT NOT NULL,
                logical_key TEXT NOT NULL,
                object_hash TEXT,
                kind TEXT NOT NULL,
                size INTEGER NOT NULL,
                mime TEXT NOT NULL,
                name TEXT NOT NULL,
                ext TEXT NOT NULL,
                inlay_type TEXT,
                width INTEGER,
                height INTEGER,
                PRIMARY KEY (generation, logical_key)
            );
            INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height
            )
            SELECT generation, logical_key, object_hash, kind, size, mime, name, ext,
                   inlay_type, width, height
            FROM asset_aliases_v8;
            DROP TABLE asset_aliases_v8;
            CREATE INDEX asset_aliases_generation ON asset_aliases (generation);
            DROP TABLE asset_owner_heads;
            DROP TABLE cold_aliases;
            DROP TABLE cold_payload_authority;
            DROP TABLE asset_repository_authority;
            DROP INDEX logical_sync_device_ack_proofs_local_generation;
            DROP TABLE logical_sync_device_ack_proofs;
            DROP INDEX logical_sync_devices_status;
            DROP TABLE logical_sync_devices;
            DROP TABLE asset_gc_maintenance_state;
            DROP TABLE asset_alias_replacement_candidates;
            DROP TABLE asset_object_deletions;
            DROP TABLE asset_objects;
            DROP TABLE logical_generation_session_pins;
            DROP TABLE logical_library_head;
            DROP TABLE logical_message_page_sources;
            DROP TABLE logical_peer_common_bases;
            DROP TABLE logical_record_dependencies;
            DROP TABLE logical_record_heads;
            DROP TABLE logical_sync_generations;
            PRAGMA user_version = 6;
            ",
        )
        .expect("downgrade alias schema fixture to v6");
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("migrate v6 alias store");
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated schema version");
    assert_eq!(version, 17);
    assert_eq!(
        store
            .read_asset_alias("asset", &alias.key, None)
            .expect("read migrated alias"),
        Some(super::Versioned {
            revision: 0,
            value: alias,
        })
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", &inlay.key, None)
            .expect("read migrated Inlay alias"),
        Some(super::Versioned {
            revision: 0,
            value: inlay,
        })
    );
    assert_eq!(
        store
            .list_cold_aliases(None)
            .expect("read empty migrated cold inventory")
            .value,
        Vec::<ColdAlias>::new()
    );
    assert_eq!(
        store
            .read_asset_owner_head(&AssetOwnerLocator::RootModuleAssets { index: 0 }, None)
            .expect("read empty migrated owner-head table"),
        None
    );
    let primary_key_columns = {
        let mut statement = store
            .connection
            .prepare(
                "SELECT name FROM pragma_table_info('asset_aliases')
                 WHERE pk > 0 ORDER BY pk",
            )
            .expect("prepare alias primary key query");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query alias primary key")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect alias primary key")
    };
    assert_eq!(
        primary_key_columns,
        vec!["generation", "kind", "logical_key"]
    );
}

#[test]
fn schema_v8_preserves_m5_v7_owner_heads_through_cow_and_pinned_reads() {
    let directory = tempfile::tempdir().expect("create v7 migration directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    store
        .connection
        .execute_batch(
            r#"
            UPDATE root
            SET value = '{"modules":[{"id":"v7-module","assets":[["v7","assets/v7.bin","BIN"]]}]}'
            WHERE generation = 'revision-0';
            INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
            ) VALUES (
                'revision-0', 'root-module-assets', '0', 1,
                '8383838383838383838383838383838383838383838383838383838383838383', 1
            );
            DROP TABLE cold_aliases;
            DROP TABLE cold_payload_authority;
            DROP TABLE asset_repository_authority;
            DROP INDEX logical_sync_device_ack_proofs_local_generation;
            DROP TABLE logical_sync_device_ack_proofs;
            DROP INDEX logical_sync_devices_status;
            DROP TABLE logical_sync_devices;
            DROP TABLE asset_gc_maintenance_state;
            DROP TABLE asset_alias_replacement_candidates;
            DROP TABLE asset_object_deletions;
            DROP TABLE asset_objects;
            DROP TABLE logical_generation_session_pins;
            DROP TABLE logical_library_head;
            DROP TABLE logical_message_page_sources;
            DROP TABLE logical_peer_common_bases;
            DROP TABLE logical_record_dependencies;
            DROP TABLE logical_record_heads;
            DROP TABLE logical_sync_generations;
            PRAGMA user_version = 7;
            "#,
        )
        .expect("create M5 v7 owner-head fixture");
    drop(store);

    let mut store = PersistentStore::open(directory.path()).expect("migrate M5 v7 store");
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated schema version");
    let cold_table_exists: bool = store
        .connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'cold_aliases'
             )",
            [],
            |row| row.get(0),
        )
        .expect("query migrated cold table");

    assert_eq!(version, 17);
    assert!(cold_table_exists);
    let owner = AssetOwnerLocator::RootModuleAssets { index: 0 };
    let head = AssetOwnerHead::present(owner.clone(), "83".repeat(32), 1);
    assert_eq!(
        store
            .read_asset_owner_head(&owner, None)
            .expect("read migrated current owner head"),
        Some(super::Versioned {
            revision: 0,
            value: head.clone(),
        })
    );
    let lease = store.acquire_revision(0).expect("pin migrated v7 revision");
    let alias = AssetAlias {
        key: "assets/post-v7.bin".to_owned(),
        object_hash: Some("84".repeat(32)),
        kind: "asset".to_owned(),
        size: 1,
        mime: "application/octet-stream".to_owned(),
        name: "Post-v7".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let current = store
        .commit_asset_alias(&alias, 0)
        .expect("copy migrated owner head into new generation");
    assert_eq!(
        store
            .read_asset_owner_head(&owner, None)
            .expect("read copied current owner head"),
        Some(super::Versioned {
            revision: current.revision,
            value: head.clone(),
        })
    );
    assert_eq!(
        store
            .read_asset_owner_head(&owner, Some(&lease.lease))
            .expect("read migrated pinned owner head"),
        Some(super::Versioned {
            revision: 0,
            value: head,
        })
    );
}

#[test]
fn schema_v8_creates_the_m5_owner_head_table() {
    let directory = tempfile::tempdir().expect("create owner-head schema directory");
    let store = PersistentStore::open(directory.path()).expect("create v8 store");
    let table_exists: bool = store
        .connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_schema
                WHERE type = 'table' AND name = 'asset_owner_heads'
             )",
            [],
            |row| row.get(0),
        )
        .expect("query owner-head table");

    assert!(table_exists);
}

#[test]
fn schema_v8_migrates_v2_snapshot_lease_and_plugin_records() {
    let directory = tempfile::tempdir().expect("create v2 migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v2_database_with_lease(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate v2 store");
    assert_eq!(
        store
            .read_asset_alias("asset", "assets/not-backfilled.bin", None)
            .expect("query empty migrated alias table"),
        None
    );
    assert!(matches!(
        store.read_root(Some("snapshot-7-v2fixture")),
        Err(StoreError::SnapshotReleased)
    ));
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated version");
    assert_eq!(version, 17);

    let snapshot_rows: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM plugin_storage WHERE generation = 'snapshot-7-v2fixture'",
            [],
            |row| row.get(0),
        )
        .expect("count released migrated plugin records");
    assert_eq!(snapshot_rows, 0);
}

fn create_v3_database(path: &Path) {
    create_v2_database(path);
    let connection = rusqlite::Connection::open(path).expect("open v2 database for v3 setup");
    connection
        .execute_batch(
            "
            CREATE TABLE plugin_storage (
                generation TEXT NOT NULL,
                storage_key TEXT NOT NULL,
                byte_size INTEGER NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (generation, storage_key)
            );
            ",
        )
        .expect("create v3 plugin table");
    let roots = {
        let mut statement = connection
            .prepare("SELECT generation, value FROM root")
            .expect("prepare v3 roots");
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query v3 roots")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect v3 roots")
    };
    for (generation, serialized) in roots {
        let mut value: Value = serde_json::from_str(&serialized).expect("parse v3 root");
        let object = value.as_object_mut().expect("v3 root object");
        if let Some(storage) = object.remove("pluginCustomStorage") {
            for (key, value) in storage.as_object().expect("v3 plugin object") {
                let serialized = serde_json::to_string(value).expect("serialize v3 plugin value");
                connection
                    .execute(
                        "INSERT INTO plugin_storage (generation, storage_key, byte_size, value)
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![generation, key, serialized.len() as i64, serialized],
                    )
                    .expect("insert v3 plugin value");
            }
        }
        connection
            .execute(
                "UPDATE root SET value = ?2 WHERE generation = ?1",
                rusqlite::params![generation, value.to_string()],
            )
            .expect("strip v3 root");
    }
    connection
        .execute(
            "INSERT INTO plugin_storage (generation, storage_key, byte_size, value)
             VALUES ('revision-7', 'zeta', 1, '0'),
                    ('revision-7', '10', 1, '0'),
                    ('revision-7', '2', 1, '0')",
            [],
        )
        .expect("insert v3 ordering values");
    connection
        .pragma_update(None, "user_version", 3)
        .expect("set v3 schema version");
}

fn create_snapshot_v3_database(path: &Path) {
    create_v2_database(path);
    let connection = rusqlite::Connection::open(path).expect("open snapshot v3 fixture database");
    connection
        .execute_batch(
            "
            DROP TABLE snapshot_leases;
            CREATE TABLE snapshot_leases (
                lease TEXT PRIMARY KEY,
                generation TEXT NOT NULL,
                revision INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX snapshot_leases_generation ON snapshot_leases (generation);
            INSERT INTO snapshot_leases (lease, generation, revision, created_at)
                VALUES ('snapshot-v3fixture', 'revision-7', 7, 4102444800000);
            PRAGMA user_version = 3;
            ",
        )
        .expect("create snapshot v3 fixture schema");
}

fn create_task4_v4_database(path: &Path) {
    create_v3_database(path);
    let connection = rusqlite::Connection::open(path).expect("open Task 4 v4 fixture database");
    connection
        .execute_batch(
            "
            ALTER TABLE plugin_storage ADD COLUMN ordinal INTEGER NOT NULL DEFAULT 0;
            UPDATE plugin_storage AS target
            SET ordinal = (
                SELECT COUNT(*) - 1
                FROM plugin_storage AS predecessor
                WHERE predecessor.generation = target.generation
                  AND predecessor.storage_key <= target.storage_key
            );
            INSERT INTO root (generation, value)
                VALUES ('snapshot-7-task4v4', '{\"username\":\"Task 4 v4 leased\"}');
            INSERT INTO plugin_storage (generation, storage_key, byte_size, ordinal, value)
                VALUES ('snapshot-7-task4v4', 'leased-zero', 1, 0, '0');
            INSERT INTO snapshot_leases (generation, created_at)
                VALUES ('snapshot-7-task4v4', 4102444800000);
            PRAGMA user_version = 4;
            ",
        )
        .expect("create Task 4 v4 fixture schema");
}

#[test]
fn schema_v8_migrates_snapshot_v3_without_plugin_table() {
    let directory = tempfile::tempdir().expect("create snapshot v3 migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_snapshot_v3_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate snapshot v3 store");
    assert!(matches!(
        store.read_root(Some("snapshot-v3fixture")),
        Err(StoreError::SnapshotReleased)
    ));
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read snapshot v3 migrated version"),
        17
    );
    assert_eq!(
        store
            .read_plugin_storage("v2-memory", None)
            .expect("read active migrated plugin value")
            .expect("active migrated plugin value exists")
            .value,
        json!({ "lossless": true })
    );
}

#[test]
fn schema_v8_migrates_task4_v4_lease_with_plugin_ordinal() {
    let directory = tempfile::tempdir().expect("create Task 4 v4 migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_task4_v4_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate Task 4 v4 store");
    assert!(matches!(
        store.read_root(Some("snapshot-7-task4v4")),
        Err(StoreError::SnapshotReleased)
    ));
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read Task 4 v4 migrated version"),
        17
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM plugin_storage WHERE generation = 'snapshot-7-task4v4'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count released Task 4 v4 plugin rows"),
        0
    );
}

#[test]
fn schema_v8_migrates_existing_v2_plugin_storage() {
    let directory = tempfile::tempdir().expect("create v2 migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v2_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate v2 store");

    assert!(store
        .read_root(None)
        .expect("read v2 migrated root")
        .value
        .get("pluginCustomStorage")
        .is_none());
    assert_eq!(
        store
            .read_plugin_storage("v2-memory", None)
            .expect("read v2 migrated plugin key")
            .expect("v2 migrated plugin key exists")
            .value,
        json!({ "lossless": true })
    );
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read v2 migrated version");
    assert_eq!(version, 17);
}

#[test]
fn schema_v8_adds_durable_plugin_ordinals_to_task4_v3() {
    let directory = tempfile::tempdir().expect("create v3 migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v3_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate v3 store");

    assert_eq!(
        store
            .query_plugin_storage(None)
            .expect("query migrated v3 storage")
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        vec!["2", "10", "v2-memory", "zeta"]
    );
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated v3 version");
    assert_eq!(version, 17);
}

#[test]
fn schema_v8_migrates_large_retained_roots_one_generation_at_a_time() {
    let directory = tempfile::tempdir().expect("create retained root migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v2_database(&database_path);
    let connection = rusqlite::Connection::open(&database_path).expect("open retained roots");
    let payload = "x".repeat(512 * 1024);
    for index in 0..12 {
        connection
            .execute(
                "INSERT INTO root (generation, value) VALUES (?1, ?2)",
                rusqlite::params![
                    format!("retained-{index}"),
                    json!({
                        "username": format!("Retained {index}"),
                        "pluginCustomStorage": { format!("memory-{index}"): payload }
                    })
                    .to_string()
                ],
            )
            .expect("insert retained root");
    }
    drop(connection);

    let store = PersistentStore::open(directory.path()).expect("migrate retained roots");
    let migrated_count: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM plugin_storage WHERE generation LIKE 'retained-%'",
            [],
            |row| row.get(0),
        )
        .expect("count retained plugin rows");
    let retained_root_fields: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM root
             WHERE generation LIKE 'retained-%' AND value LIKE '%pluginCustomStorage%'",
            [],
            |row| row.get(0),
        )
        .expect("count retained root plugin fields");
    assert_eq!(migrated_count, 12);
    assert_eq!(retained_root_fields, 0);
}

#[test]
fn schema_v8_migrates_records_for_every_v1_generation() {
    let directory = tempfile::tempdir().expect("create migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v1_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate v1 store");
    assert_eq!(
        store
            .query_presets(None)
            .expect("query migrated presets")
            .items
            .len(),
        2
    );
    assert!(store
        .read_root(None)
        .expect("read migrated root")
        .value
        .get("botPresets")
        .is_none());
    assert!(store
        .read_root(None)
        .expect("read migrated root")
        .value
        .get("pluginCustomStorage")
        .is_none());
    assert_eq!(
        store
            .read_plugin_storage("active-memory", None)
            .expect("read migrated plugin storage")
            .expect("migrated plugin key exists")
            .value,
        json!({ "turns": [1, 2, 3] })
    );
    let old_root: String = store
        .connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-old'",
            [],
            |row| row.get(0),
        )
        .expect("read migrated old root");
    assert_eq!(
        serde_json::from_str::<Value>(&old_root).expect("parse old root"),
        json!({ "username": "V1 old" })
    );
    let old_presets: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM bot_presets WHERE generation = 'revision-old'",
            [],
            |row| row.get(0),
        )
        .expect("count old presets");
    assert_eq!(old_presets, 1);
    let old_plugin_storage: String = store
        .connection
        .query_row(
            "SELECT value FROM plugin_storage
             WHERE generation = 'revision-old' AND storage_key = 'old-memory'",
            [],
            |row| row.get(0),
        )
        .expect("read old plugin storage");
    assert_eq!(
        serde_json::from_str::<Value>(&old_plugin_storage).expect("parse old plugin storage"),
        json!("preserved")
    );
    let summary = &store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: true,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query migrated character")
        .items[0];
    assert_eq!(summary.r#type, "character");
    assert_eq!(summary.creator_notes.as_deref(), Some("migrated notes"));
    assert_eq!(summary.trash_time, Some(123));
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated version");
    assert_eq!(version, 17);
}

#[test]
fn schema_v8_rolls_back_when_v1_bot_presets_is_not_an_array() {
    let directory = tempfile::tempdir().expect("create migration rollback directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v1_database(&database_path);
    let invalid_root = json!({
        "username": "Invalid v1 root",
        "botPresets": { "legacy": "unsupported" }
    });
    let connection = rusqlite::Connection::open(&database_path).expect("open v1 database");
    let original_active: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-7'",
            [],
            |row| row.get(0),
        )
        .expect("read original active v1 root");
    connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params!["revision-old", invalid_root.to_string()],
        )
        .expect("write invalid v1 root");
    drop(connection);

    assert!(PersistentStore::open(directory.path()).is_err());

    let connection =
        rusqlite::Connection::open(&database_path).expect("reopen rolled back v1 database");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read rolled back schema version");
    let preserved: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-old'",
            [],
            |row| row.get(0),
        )
        .expect("read preserved v1 root");
    let preserved_active: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-7'",
            [],
            |row| row.get(0),
        )
        .expect("read preserved active v1 root");
    let preset_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'bot_presets'",
            [],
            |row| row.get(0),
        )
        .expect("check rolled back preset table");
    assert_eq!(version, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&preserved).expect("parse preserved root"),
        invalid_root
    );
    assert_eq!(preserved_active, original_active);
    assert_eq!(preset_table_count, 0);
}

#[test]
fn schema_v8_rolls_back_when_v1_plugin_storage_is_not_an_object() {
    let directory = tempfile::tempdir().expect("create plugin migration rollback directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v1_database(&database_path);
    let invalid_root = json!({
        "username": "Invalid plugin root",
        "botPresets": [{ "name": "Still valid" }],
        "pluginCustomStorage": ["unsupported"]
    });
    let connection = rusqlite::Connection::open(&database_path).expect("open v1 database");
    connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params!["revision-old", invalid_root.to_string()],
        )
        .expect("write invalid plugin storage");
    drop(connection);

    assert!(PersistentStore::open(directory.path()).is_err());

    let connection =
        rusqlite::Connection::open(&database_path).expect("reopen rolled back plugin migration");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read rolled back version");
    let preserved: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-old'",
            [],
            |row| row.get(0),
        )
        .expect("read preserved plugin root");
    let plugin_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'plugin_storage'",
            [],
            |row| row.get(0),
        )
        .expect("check rolled back plugin table");
    assert_eq!(version, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&preserved).expect("parse preserved plugin root"),
        invalid_root
    );
    assert_eq!(plugin_table_count, 0);
}

#[test]
fn pending_v1_snapshot_restores_then_migrates_to_v8() {
    let directory = tempfile::tempdir().expect("create restore directory");
    let store = PersistentStore::open(directory.path()).expect("open current v5 store");
    let candidate = directory
        .path()
        .join("persistent/snapshots/persistent-v1.db");
    create_v1_database(&candidate);
    store
        .snapshot_restore_request(&candidate)
        .expect("request v1 snapshot restore");
    drop(store);

    let restored =
        PersistentStore::open(directory.path()).expect("restore and migrate v1 snapshot");
    assert_eq!(restored.revision().expect("read restored revision"), 7);
    assert_eq!(
        restored
            .query_presets(None)
            .expect("query restored presets")
            .items[0]
            .name,
        "V1 first"
    );
    let version: i64 = restored
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read restored version");
    assert_eq!(version, 17);
}

#[test]
fn invalid_pending_v1_snapshot_preserves_live_database_and_restore_marker() {
    let directory = tempfile::tempdir().expect("create invalid restore directory");
    let mut store = PersistentStore::open(directory.path()).expect("open current v5 store");
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Preserved live database" })),
            replace_presets: Some(vec![json!({ "name": "Live preset" })]),
            ..empty_working_set_commit(0)
        })
        .expect("seed live database");

    let candidate = directory
        .path()
        .join("persistent/snapshots/persistent-invalid-v1.db");
    create_v1_database(&candidate);
    let connection = rusqlite::Connection::open(&candidate).expect("open invalid v1 candidate");
    connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params![
                "revision-7",
                json!({ "username": "Invalid candidate", "botPresets": { "legacy": true } })
                    .to_string()
            ],
        )
        .expect("corrupt candidate preset shape");
    drop(connection);
    store
        .snapshot_restore_request(&candidate)
        .expect("request invalid v1 restore");
    drop(store);

    let reopened = PersistentStore::open(directory.path())
        .expect("invalid candidate must not replace live database");
    assert_eq!(reopened.revision().expect("read preserved revision"), 1);
    assert_eq!(
        reopened
            .read_root(None)
            .expect("read preserved live root")
            .value["username"],
        "Preserved live database"
    );
    assert_eq!(
        reopened
            .query_presets(None)
            .expect("query preserved live presets")
            .items[0]
            .name,
        "Live preset"
    );
    assert!(directory
        .path()
        .join("persistent/snapshots/pending-restore.json")
        .is_file());
    assert!(candidate.is_file());
}

#[test]
fn semantically_invalid_pending_v1_snapshot_preserves_live_database_and_restore_marker() {
    let directory = tempfile::tempdir().expect("create semantic restore directory");
    let mut store = PersistentStore::open(directory.path()).expect("open current v5 store");
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Preserved semantic live database" })),
            ..empty_working_set_commit(0)
        })
        .expect("seed semantic live database");

    let candidate = directory
        .path()
        .join("persistent/snapshots/persistent-semantic-invalid-v1.db");
    create_v1_database(&candidate);
    let connection = rusqlite::Connection::open(&candidate).expect("open semantic v1 candidate");
    connection
        .execute(
            "UPDATE meta SET value = '\"missing-generation\"' WHERE key = 'activeGeneration'",
            [],
        )
        .expect("make active generation semantically invalid");
    drop(connection);
    store
        .snapshot_restore_request(&candidate)
        .expect("request semantic invalid v1 restore");
    drop(store);

    let reopened = PersistentStore::open(directory.path())
        .expect("semantic invalid candidate must not replace live database");
    assert_eq!(reopened.revision().expect("read preserved revision"), 1);
    assert_eq!(
        reopened
            .read_root(None)
            .expect("read preserved semantic live root")
            .value["username"],
        "Preserved semantic live database"
    );
    assert!(directory
        .path()
        .join("persistent/snapshots/pending-restore.json")
        .is_file());
    assert!(candidate.is_file());
}
