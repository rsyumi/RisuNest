use super::*;

fn open() -> (tempfile::TempDir, DeviceStore) {
    let directory = tempfile::tempdir().expect("create device store directory");
    let store = DeviceStore::open(directory.path()).expect("open fresh device store");
    (directory, store)
}

fn insert_embedding(tx: &Transaction<'_>, key: &str, clock: &str) {
    tx.execute(
        "INSERT INTO hypa_embeddings
            (cache_key,producer,model,endpoint,preprocess_version,dimensions,vector,metadata,
             tombstone,write_clock,writer_id,published_clock)
            VALUES (?1,'hypa-v2','model',NULL,1,4,NULL,NULL,0,?2,'writer',NULL)",
        params![key, clock],
    )
    .expect("insert embedding");
}

fn insert_plugin_value(tx: &Transaction<'_>, owner: &str, space: &str, key: &str, clock: &str) {
    tx.execute(
        "INSERT INTO plugin_device_storage
            (owner,space,key,value,byte_size,tombstone,write_clock,writer_id,published_clock)
            VALUES (?1,?2,?3,'v',1,0,?4,'writer',NULL)",
        params![owner, space, key, clock],
    )
    .expect("insert plugin value");
}

fn changes(db: &Connection) -> Vec<(String, String, String, String, i64)> {
    let mut statement = db
        .prepare("SELECT section,key1,key2,key3,revision FROM device_changes ORDER BY section,key1,key2,key3")
        .expect("prepare device changes query");
    statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("read device changes")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect device changes")
}

#[test]
fn every_live_device_table_is_listed_here() {
    // Explicit coverage for the device file. Adding a table is a deliberate
    // change to what this installation keeps outside the library.
    let expected = [
        "asset_residency_policy",
        "device_change_consumers",
        "device_change_context",
        "device_changes",
        "device_meta",
        "device_remote_cursors",
        "device_sections",
        "device_settings",
        "hypa_embeddings",
        "plugin_claim_sessions",
        "plugin_device_storage",
        "plugin_permission_grants",
        "plugin_permissions",
    ];
    let (_directory, store) = open();
    let actual = store
        .connection()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn a_fresh_device_store_seeds_identity_sections_and_policy() {
    let (_directory, store) = open();
    let db = store.connection();

    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .expect("read device schema version"),
        DEVICE_SCHEMA_VERSION
    );
    let writer_id = store.writer_id().expect("read writer identity");
    assert_eq!(
        Uuid::parse_str(&writer_id)
            .expect("writer identity is a UUID")
            .get_version_num(),
        4
    );
    assert_eq!(store.asset_residency_policy().unwrap(), AssetPolicy::Full);

    let mut statement = db
        .prepare("SELECT section,max_write_clock,gc_floor,participating,participation_generation FROM device_sections ORDER BY section")
        .unwrap();
    let sections = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        sections,
        vec![
            (
                "hypa".to_owned(),
                "0".to_owned(),
                "0".to_owned(),
                1,
                "0".to_owned()
            ),
            (
                "local-plugins".to_owned(),
                "0".to_owned(),
                "0".to_owned(),
                0,
                "0".to_owned()
            ),
        ]
    );
}

#[test]
fn reopening_keeps_the_writer_identity_and_the_stored_policy() {
    let directory = tempfile::tempdir().unwrap();
    let store = DeviceStore::open(directory.path()).unwrap();
    let writer_id = store.writer_id().unwrap();
    store
        .set_asset_residency_policy(AssetPolicy::Remote)
        .unwrap();
    drop(store);

    let store = DeviceStore::open(directory.path()).expect("reopen device store");
    assert_eq!(store.writer_id().unwrap(), writer_id);
    assert_eq!(store.asset_residency_policy().unwrap(), AssetPolicy::Remote);
}

#[test]
fn an_unsupported_device_schema_version_is_rejected() {
    for version in [2_u32, 17] {
        let directory = tempfile::tempdir().unwrap();
        drop(DeviceStore::open(directory.path()).unwrap());
        let connection = Connection::open(directory.path().join(DEVICE_DATABASE_FILE)).unwrap();
        connection
            .execute_batch(&format!("PRAGMA user_version = {version};"))
            .unwrap();
        drop(connection);

        let Err(error) = DeviceStore::open(directory.path()) else {
            panic!("device schema version {version} must be rejected");
        };
        assert_eq!(
            error,
            StoreError::Store {
                message: format!("unsupported device schema version {version}"),
            }
        );
    }
}

#[test]
fn a_changed_definition_or_a_missing_control_row_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    drop(DeviceStore::open(directory.path()).unwrap());
    let connection = Connection::open(directory.path().join(DEVICE_DATABASE_FILE)).unwrap();
    connection
        .execute_batch("DROP TRIGGER device_change_hypa_embeddings_delete;")
        .unwrap();
    drop(connection);
    let Err(error) = DeviceStore::open(directory.path()) else {
        panic!("a missing trigger must be rejected");
    };
    assert_eq!(
        error,
        StoreError::Validation {
            message: "Device schema is incompatible".to_owned(),
        }
    );

    let directory = tempfile::tempdir().unwrap();
    drop(DeviceStore::open(directory.path()).unwrap());
    let connection = Connection::open(directory.path().join(DEVICE_DATABASE_FILE)).unwrap();
    connection
        .execute_batch("DELETE FROM device_sections WHERE section='hypa';")
        .unwrap();
    drop(connection);
    let Err(error) = DeviceStore::open(directory.path()) else {
        panic!("a missing control row must be rejected");
    };
    assert_eq!(
        error,
        StoreError::Validation {
            message: "Device control rows are invalid".to_owned(),
        }
    );
}

#[test]
fn a_populated_foreign_database_is_not_adopted() {
    let directory = tempfile::tempdir().unwrap();
    let connection = Connection::open(directory.path().join(DEVICE_DATABASE_FILE)).unwrap();
    connection
        .execute_batch("CREATE TABLE unrelated(id TEXT PRIMARY KEY);")
        .unwrap();
    drop(connection);

    let Err(error) = DeviceStore::open(directory.path()) else {
        panic!("a foreign database must be rejected");
    };
    assert_eq!(
        error,
        StoreError::Store {
            message: "device database is not a supported format".to_owned(),
        }
    );
}

#[test]
fn write_clocks_advance_per_section_and_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(directory.path()).unwrap();
    {
        let tx = store.transaction().unwrap();
        assert_eq!(
            issue_write_clock(&tx, Section::Hypa).unwrap().as_str(),
            "1"
        );
        assert_eq!(
            issue_write_clock(&tx, Section::Hypa).unwrap().as_str(),
            "2"
        );
        assert_eq!(
            issue_write_clock(&tx, Section::LocalPlugins)
                .unwrap()
                .as_str(),
            "1"
        );
        tx.commit().unwrap();
    }
    drop(store);

    let mut store = DeviceStore::open(directory.path()).unwrap();
    let tx = store.transaction().unwrap();
    assert_eq!(
        issue_write_clock(&tx, Section::Hypa).unwrap().as_str(),
        "3"
    );
    assert_eq!(
        issue_write_clock(&tx, Section::LocalPlugins)
            .unwrap()
            .as_str(),
        "2"
    );
    tx.commit().unwrap();
}

#[test]
fn a_rolled_back_transaction_does_not_consume_a_write_clock() {
    let (_directory, mut store) = open();
    {
        let tx = store.transaction().unwrap();
        assert_eq!(
            issue_write_clock(&tx, Section::Hypa).unwrap().as_str(),
            "1"
        );
        tx.rollback().unwrap();
    }
    let tx = store.transaction().unwrap();
    assert_eq!(
        issue_write_clock(&tx, Section::Hypa).unwrap().as_str(),
        "1"
    );
    tx.commit().unwrap();
}

#[test]
fn observed_remote_clocks_compare_by_length_then_value() {
    let (_directory, mut store) = open();
    let tx = store.transaction().unwrap();
    let observe = |value: &str| {
        observe_remote_clock(
            &tx,
            Section::Hypa,
            &Sequence::try_from(value.to_owned()).expect("canonical sequence"),
        )
        .unwrap()
        .as_str()
        .to_owned()
    };

    assert_eq!(observe("9"), "9");
    assert_eq!(observe("10"), "10");
    assert_eq!(observe("9"), "10");
    assert_eq!(observe("10"), "10");
    assert_eq!(
        issue_write_clock(&tx, Section::Hypa).unwrap().as_str(),
        "11"
    );
    // The other section keeps its own counter.
    assert_eq!(
        issue_write_clock(&tx, Section::LocalPlugins)
            .unwrap()
            .as_str(),
        "1"
    );
    tx.commit().unwrap();

    for value in ["", "01", "1x", &"9".repeat(65)] {
        assert!(Sequence::try_from(value.to_owned()).is_err());
    }
}

#[test]
fn a_non_canonical_stored_clock_is_reported_instead_of_being_repaired() {
    let (_directory, mut store) = open();
    let tx = store.transaction().unwrap();
    tx.execute(
        "UPDATE device_sections SET max_write_clock='007' WHERE section='hypa'",
        [],
    )
    .unwrap();
    assert_eq!(
        issue_write_clock(&tx, Section::Hypa).unwrap_err(),
        StoreError::Validation {
            message: "device write clock is invalid".to_owned(),
        }
    );
}

#[test]
fn only_mutations_inside_a_context_reach_the_change_index() {
    let (_directory, mut store) = open();

    let tx = store.transaction().unwrap();
    insert_embedding(&tx, "cache-outside", "1");
    insert_plugin_value(&tx, "owner", "json", "outside", "1");
    tx.commit().unwrap();
    assert!(changes(store.connection()).is_empty());
    assert_eq!(
        store
            .connection()
            .query_row("SELECT revision FROM device_meta WHERE singleton=1", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );

    let tx = store.transaction().unwrap();
    assert_eq!(begin_mutation(&tx).unwrap(), 1);
    insert_embedding(&tx, "cache-one", "2");
    insert_plugin_value(&tx, "owner", "string", "alpha", "2");
    finish_mutation(&tx).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        changes(store.connection()),
        vec![
            (
                "hypa".to_owned(),
                "cache-one".to_owned(),
                String::new(),
                String::new(),
                1
            ),
            (
                "local-plugins".to_owned(),
                "owner".to_owned(),
                "string".to_owned(),
                "alpha".to_owned(),
                1
            ),
        ]
    );

    let tx = store.transaction().unwrap();
    assert_eq!(begin_mutation(&tx).unwrap(), 2);
    tx.execute(
        "UPDATE hypa_embeddings SET tombstone=1 WHERE cache_key='cache-one'",
        [],
    )
    .unwrap();
    tx.execute(
        "DELETE FROM plugin_device_storage WHERE owner='owner' AND space='json' AND key='outside'",
        [],
    )
    .unwrap();
    finish_mutation(&tx).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        changes(store.connection()),
        vec![
            (
                "hypa".to_owned(),
                "cache-one".to_owned(),
                String::new(),
                String::new(),
                2
            ),
            (
                "local-plugins".to_owned(),
                "owner".to_owned(),
                "json".to_owned(),
                "outside".to_owned(),
                2
            ),
            (
                "local-plugins".to_owned(),
                "owner".to_owned(),
                "string".to_owned(),
                "alpha".to_owned(),
                1
            ),
        ]
    );
}

fn vector_bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|value| value.to_le_bytes()).collect()
}

fn embedding(key: &str, values: &[f32]) -> hypa::HypaEmbeddingWrite {
    hypa::HypaEmbeddingWrite {
        cache_key: key.to_owned(),
        producer: "hypa-v2".to_owned(),
        model: "MiniLM".to_owned(),
        endpoint: None,
        preprocess_version: 1,
        dimensions: values.len() as i64,
        vector: vector_bytes(values),
        metadata: None,
    }
}

fn clock(db: &Connection, section: &str) -> String {
    db.query_row(
        "SELECT max_write_clock FROM device_sections WHERE section=?1",
        [section],
        |row| row.get(0),
    )
    .expect("read section clock")
}

#[test]
fn an_embedding_batch_commits_one_write_clock_and_one_change_row_per_key() {
    let (_directory, mut store) = open();
    store
        .write_hypa_embeddings(&[
            embedding("key-a", &[1.0, 2.0]),
            embedding("key-b", &[3.0, 4.0]),
            embedding("key-c", &[5.0, 6.0]),
        ])
        .expect("write embedding batch");

    assert_eq!(clock(store.connection(), "hypa"), "3");
    assert_eq!(clock(store.connection(), "local-plugins"), "0");
    let writer_id = store.writer_id().expect("read writer identity");
    let mut statement = store
        .connection()
        .prepare("SELECT cache_key,write_clock,writer_id,published_clock FROM hypa_embeddings ORDER BY cache_key")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("key-a".to_owned(), "1".to_owned(), writer_id.clone(), None),
            ("key-b".to_owned(), "2".to_owned(), writer_id.clone(), None),
            ("key-c".to_owned(), "3".to_owned(), writer_id, None),
        ]
    );
    assert_eq!(
        changes(store.connection()),
        vec![
            ("hypa".to_owned(), "key-a".to_owned(), String::new(), String::new(), 1),
            ("hypa".to_owned(), "key-b".to_owned(), String::new(), String::new(), 1),
            ("hypa".to_owned(), "key-c".to_owned(), String::new(), String::new(), 1),
        ]
    );
    assert_eq!(
        store
            .connection()
            .query_row("SELECT count(*) FROM device_change_context", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn an_embedding_read_answers_in_request_order_with_gaps_for_misses() {
    let (_directory, mut store) = open();
    store
        .write_hypa_embeddings(&[embedding("key-a", &[1.0, 2.0]), embedding("key-b", &[3.0])])
        .expect("write embedding batch");

    let keys = vec![
        "key-b".to_owned(),
        "missing".to_owned(),
        "key-a".to_owned(),
        "key-b".to_owned(),
    ];
    let rows = store.read_hypa_embeddings(&keys).expect("read embeddings");
    let observed = rows
        .iter()
        .map(|row| (row.cache_key.as_str(), row.dimensions, row.vector.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        observed,
        vec![
            ("key-b", 1, Some(vector_bytes(&[3.0]))),
            ("missing", 0, None),
            ("key-a", 2, Some(vector_bytes(&[1.0, 2.0]))),
            ("key-b", 1, Some(vector_bytes(&[3.0]))),
        ]
    );
}

#[test]
fn a_tombstoned_embedding_reads_as_a_miss() {
    let (_directory, mut store) = open();
    store
        .write_hypa_embeddings(&[embedding("key-a", &[1.0])])
        .expect("write embedding");
    store
        .connection()
        .execute("UPDATE hypa_embeddings SET tombstone=1", [])
        .unwrap();

    let rows = store
        .read_hypa_embeddings(&["key-a".to_owned()])
        .expect("read embeddings");
    assert_eq!(rows[0].dimensions, 0);
    assert!(rows[0].vector.is_none());
}

#[test]
fn rewriting_a_key_replaces_the_vector_and_takes_a_new_clock() {
    let (_directory, mut store) = open();
    store
        .write_hypa_embeddings(&[embedding("key-a", &[1.0, 2.0])])
        .expect("write embedding");
    store
        .connection()
        .execute("UPDATE hypa_embeddings SET published_clock='1', tombstone=1", [])
        .unwrap();
    store
        .write_hypa_embeddings(&[embedding("key-a", &[7.0, 8.0, 9.0])])
        .expect("rewrite embedding");

    let rows = store
        .read_hypa_embeddings(&["key-a".to_owned()])
        .expect("read embeddings");
    assert_eq!(rows[0].dimensions, 3);
    assert_eq!(rows[0].vector, Some(vector_bytes(&[7.0, 8.0, 9.0])));
    assert_eq!(clock(store.connection(), "hypa"), "2");
    assert_eq!(
        store
            .connection()
            .query_row("SELECT published_clock FROM hypa_embeddings", [], |row| row
                .get::<_, Option<String>>(0))
            .unwrap(),
        None
    );
}

#[test]
fn an_embedding_batch_is_rejected_whole_when_one_entry_is_malformed() {
    let (_directory, mut store) = open();
    let mut broken = embedding("key-b", &[1.0, 2.0]);
    broken.dimensions = 3;
    let error = store
        .write_hypa_embeddings(&[embedding("key-a", &[1.0]), broken])
        .expect_err("reject mismatched vector length");
    assert!(matches!(error, StoreError::Validation { .. }));

    assert_eq!(
        store
            .connection()
            .query_row("SELECT count(*) FROM hypa_embeddings", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(clock(store.connection(), "hypa"), "0");
}

#[test]
fn an_embedding_batch_rejects_dimensions_outside_the_supported_range() {
    let (_directory, mut store) = open();
    let mut zero = embedding("key-a", &[]);
    zero.dimensions = 0;
    assert!(store.write_hypa_embeddings(&[zero]).is_err());

    let mut huge = embedding("key-a", &[1.0]);
    huge.dimensions = hypa::MAX_DIMENSIONS + 1;
    huge.vector = vec![0; (hypa::MAX_DIMENSIONS as usize + 1) * hypa::VECTOR_ELEMENT_BYTES];
    assert!(store.write_hypa_embeddings(&[huge]).is_err());
}

#[test]
fn device_settings_round_trip_whole_values_and_remove_them() {
    let (_directory, store) = open();
    assert_eq!(store.read_setting("sync-conflict-backups.index.v1").unwrap(), None);

    let index = serde_json::json!([{ "id": "backup-1", "byteLength": 12 }]);
    store
        .write_setting("sync-conflict-backups.index.v1", &index)
        .unwrap();
    assert_eq!(
        store.read_setting("sync-conflict-backups.index.v1").unwrap(),
        Some(index)
    );

    store
        .write_setting("sync-conflict-backups.index.v1", &serde_json::json!([]))
        .unwrap();
    assert_eq!(
        store.read_setting("sync-conflict-backups.index.v1").unwrap(),
        Some(serde_json::json!([]))
    );

    store
        .remove_setting("sync-conflict-backups.index.v1")
        .unwrap();
    assert_eq!(store.read_setting("sync-conflict-backups.index.v1").unwrap(), None);
    store
        .remove_setting("sync-conflict-backups.index.v1")
        .unwrap();
}

#[test]
fn patching_a_setting_changes_only_the_named_entries() {
    let (_directory, mut store) = open();
    let key = "official-account.association.v1";

    store
        .patch_setting(
            key,
            serde_json::json!({ "officialAssociation:one": "{\"revision\":1}" })
                .as_object()
                .unwrap(),
        )
        .unwrap();
    store
        .patch_setting(
            key,
            serde_json::json!({ "officialAssociation:two": "{\"revision\":2}" })
                .as_object()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        store.read_setting(key).unwrap(),
        Some(serde_json::json!({
            "officialAssociation:one": "{\"revision\":1}",
            "officialAssociation:two": "{\"revision\":2}",
        }))
    );

    store
        .patch_setting(
            key,
            serde_json::json!({ "officialAssociation:one": serde_json::Value::Null })
                .as_object()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        store.read_setting(key).unwrap(),
        Some(serde_json::json!({ "officialAssociation:two": "{\"revision\":2}" }))
    );
}

#[test]
fn patching_a_setting_that_does_not_hold_entries_is_rejected() {
    let (_directory, mut store) = open();
    let key = "sync-conflict-backups.index.v1";
    store.write_setting(key, &serde_json::json!([1, 2])).unwrap();
    assert!(store
        .patch_setting(
            key,
            serde_json::json!({ "entry": "value" }).as_object().unwrap()
        )
        .is_err());
    assert_eq!(
        store.read_setting(key).unwrap(),
        Some(serde_json::json!([1, 2]))
    );
}

#[test]
fn device_settings_are_not_tracked_as_section_changes() {
    // Only the two synchronized sections feed the device change index.
    let (_directory, mut store) = open();
    let tx = store.transaction().unwrap();
    let revision = begin_mutation(&tx).unwrap();
    tx.execute(
        "INSERT INTO device_settings (key,value) VALUES ('accountst','\"able\"')",
        [],
    )
    .unwrap();
    finish_mutation(&tx).unwrap();
    tx.commit().unwrap();
    assert_eq!(revision, 1);
    assert!(changes(store.connection()).is_empty());
}

#[test]
fn device_settings_accept_only_the_assigned_keys() {
    let (_directory, mut store) = open();
    assert_eq!(
        DEVICE_SETTING_KEYS,
        [
            "accountst",
            "ignoreRisuAuth",
            "dosync",
            "hub",
            "risunest_tos_v1",
            "risu_service_tos_v1",
            "risu_lastsaved",
            "nightlyWarned",
            "risuNestDeviceSettings",
            "risuNestUpdateSettings",
            "risuNestServerSyncRestoreHold",
            "official-account.association.v1",
            "official-account.asset-ledger.v1",
            "sync-conflict-backups.index.v1",
        ]
    );

    for key in DEVICE_SETTING_KEYS {
        store
            .write_setting(key, &serde_json::json!("value"))
            .expect("write an assigned key");
    }

    let value = serde_json::json!("value");
    assert!(store.write_setting("mainpage", &value).is_err());
    assert!(store.read_setting("mainpage").is_err());
    assert!(store.remove_setting("mainpage").is_err());
    assert!(store
        .patch_setting(
            "mainpage",
            serde_json::json!({ "entry": "value" }).as_object().unwrap()
        )
        .is_err());
    assert!(store
        .read_settings(&["accountst".to_owned(), "mainpage".to_owned()])
        .is_err());

    let stored: i64 = store
        .connection()
        .query_row("SELECT count(*) FROM device_settings", [], |row| row.get(0))
        .expect("count settings");
    assert_eq!(stored, DEVICE_SETTING_KEYS.len() as i64);
}

#[test]
fn a_settings_batch_answers_in_request_order() {
    let (_directory, store) = open();
    store
        .write_setting("accountst", &serde_json::json!("able"))
        .unwrap();
    store
        .write_setting("dosync", &serde_json::json!("sync"))
        .unwrap();

    assert_eq!(
        store
            .read_settings(&[
                "dosync".to_owned(),
                "hub".to_owned(),
                "accountst".to_owned(),
            ])
            .unwrap(),
        vec![
            Some(serde_json::json!("sync")),
            None,
            Some(serde_json::json!("able")),
        ]
    );
    assert!(store
        .read_settings(
            &DEVICE_SETTING_KEYS
                .iter()
                .chain(["accountst"].iter())
                .map(|key| (*key).to_owned())
                .collect::<Vec<_>>()
        )
        .is_err());
}

#[test]
fn plugin_permissions_and_their_reconfirmation_times_stay_in_their_own_tables() {
    let (_directory, store) = open();
    assert!(store.read_plugin_permissions().unwrap().is_empty());
    assert!(store.read_plugin_permission_grants().unwrap().is_empty());

    store.write_plugin_permission("hash-a", "db", true).unwrap();
    store
        .write_plugin_permission("hash-b", "fetchLogs", false)
        .unwrap();
    store
        .write_plugin_permission_grant("Plugin A", "db", 1_700_000_000_000)
        .unwrap();

    let permissions = store.read_plugin_permissions().unwrap();
    assert_eq!(permissions.len(), 2);
    assert_eq!(permissions[0].code_hash, "hash-a");
    assert_eq!(permissions[0].permission, "db");
    assert!(permissions[0].granted);
    assert!(!permissions[1].granted);

    let grants = store.read_plugin_permission_grants().unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].plugin_name, "Plugin A");
    assert_eq!(grants[0].last_grant_at, 1_700_000_000_000);

    store
        .write_plugin_permission_grant("Plugin A", "db", 1_700_000_001_000)
        .unwrap();
    assert_eq!(
        store.read_plugin_permission_grants().unwrap()[0].last_grant_at,
        1_700_000_001_000
    );

    assert!(store.write_plugin_permission("", "db", true).is_err());
    assert!(store.write_plugin_permission("hash-a", "", true).is_err());
    assert!(store
        .write_plugin_permission_grant("Plugin A", "db", -1)
        .is_err());
}

#[test]
fn a_plugin_reads_lists_and_clears_only_its_own_device_space() {
    use super::plugin_values::PluginDeviceMutation;
    let (_directory, mut store) = open();
    for owner in ["plugin-a", "plugin-b"] {
        store
            .write_plugin_device_values(
                owner,
                &[
                    PluginDeviceMutation::Set {
                        space: "string".to_owned(),
                        key: "shared".to_owned(),
                        value: owner.to_owned(),
                    },
                    PluginDeviceMutation::Set {
                        space: "json".to_owned(),
                        key: "shared".to_owned(),
                        value: format!("{{\"owner\":\"{owner}\"}}"),
                    },
                ],
            )
            .expect("seed device values");
    }

    assert_eq!(
        store
            .read_plugin_device_value("plugin-a", "string", "shared")
            .unwrap(),
        Some("plugin-a".to_owned())
    );
    assert_eq!(
        store
            .list_plugin_device_keys("plugin-a", "string")
            .unwrap(),
        vec!["shared".to_owned()]
    );

    // The two spaces never answer for each other, so clearing one leaves the
    // other alone.
    store
        .write_plugin_device_values(
            "plugin-a",
            &[PluginDeviceMutation::Clear {
                space: "string".to_owned(),
            }],
        )
        .expect("clear one space");
    assert!(store
        .read_plugin_device_value("plugin-a", "string", "shared")
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .read_plugin_device_value("plugin-a", "json", "shared")
            .unwrap(),
        Some("{\"owner\":\"plugin-a\"}".to_owned())
    );
    assert_eq!(
        store
            .read_plugin_device_value("plugin-b", "string", "shared")
            .unwrap(),
        Some("plugin-b".to_owned())
    );

    let hydrated = store.hydrate_plugin_device_storage("plugin-b").unwrap();
    assert!(hydrated.complete);
    assert_eq!(
        hydrated
            .entries
            .iter()
            .map(|entry| (entry.space.clone(), entry.key.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("json".to_owned(), "shared".to_owned()),
            ("string".to_owned(), "shared".to_owned()),
        ]
    );
}

#[test]
fn a_removed_device_value_leaves_a_tombstone_with_its_own_clock() {
    use super::plugin_values::PluginDeviceMutation;
    let (_directory, mut store) = open();
    store
        .write_plugin_device_values(
            "plugin-a",
            &[PluginDeviceMutation::Set {
                space: "string".to_owned(),
                key: "gone".to_owned(),
                value: "value".to_owned(),
            }],
        )
        .unwrap();
    store
        .write_plugin_device_values(
            "plugin-a",
            &[PluginDeviceMutation::Delete {
                space: "string".to_owned(),
                key: "gone".to_owned(),
            }],
        )
        .unwrap();

    let (tombstone, value, byte_size, clock): (i64, Option<String>, i64, String) = store
        .connection()
        .query_row(
            "SELECT tombstone,value,byte_size,write_clock FROM plugin_device_storage
                WHERE owner='plugin-a' AND space='string' AND key='gone'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read tombstone row");
    assert_eq!(tombstone, 1);
    assert!(value.is_none());
    assert_eq!(byte_size, 0);
    let section: String = store
        .connection()
        .query_row(
            "SELECT max_write_clock FROM device_sections WHERE section='local-plugins'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(section, clock);
    assert!(store
        .hydrate_plugin_device_storage("plugin-a")
        .unwrap()
        .entries
        .is_empty());
}

#[test]
fn a_keyspace_past_the_hydration_limit_reports_only_its_size() {
    use super::plugin_values::{PluginDeviceMutation, HYDRATION_LIMIT_BYTES};
    let (_directory, mut store) = open();
    let chunk = "x".repeat(64 * 1024);
    let mut written = 0i64;
    let mut index = 0usize;
    while written <= HYDRATION_LIMIT_BYTES {
        store
            .write_plugin_device_values(
                "plugin-a",
                &[PluginDeviceMutation::Set {
                    space: "string".to_owned(),
                    key: format!("key-{index}"),
                    value: chunk.clone(),
                }],
            )
            .unwrap();
        written += chunk.len() as i64;
        index += 1;
    }

    let refused = store.hydrate_plugin_device_storage("plugin-a").unwrap();
    assert!(!refused.complete);
    assert!(refused.entries.is_empty());
    assert_eq!(refused.byte_size, written);
    // The keyspace is still readable one key at a time.
    assert_eq!(
        store
            .read_plugin_device_value("plugin-a", "string", "key-0")
            .unwrap(),
        Some(chunk.clone())
    );
    assert_eq!(store.list_plugin_device_keys("plugin-a", "string").unwrap().len(), index);

    store
        .write_plugin_device_values(
            "plugin-a",
            &[PluginDeviceMutation::Delete {
                space: "string".to_owned(),
                key: "key-0".to_owned(),
            }],
        )
        .unwrap();
    let accepted = store.hydrate_plugin_device_storage("plugin-a").unwrap();
    assert!(accepted.complete);
    assert_eq!(accepted.byte_size, written - chunk.len() as i64);
    assert_eq!(accepted.entries.len(), index - 1);
}

#[test]
fn the_device_value_list_reports_sizes_without_carrying_values() {
    use super::plugin_values::PluginDeviceMutation;
    let (_directory, mut store) = open();
    store
        .write_plugin_device_values(
            "plugin-a",
            &[PluginDeviceMutation::Set {
                space: "json".to_owned(),
                key: "settings".to_owned(),
                value: "{\"secret\":\"value\"}".to_owned(),
            }],
        )
        .unwrap();
    let listed = store.list_plugin_device_storage().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].owner, "plugin-a");
    assert_eq!(listed[0].space, "json");
    assert_eq!(listed[0].key, "settings");
    assert_eq!(listed[0].byte_size, 18);
    let encoded = serde_json::to_string(&listed).unwrap();
    assert!(!encoded.contains("secret"));
}

#[test]
fn a_committed_device_value_survives_reopening_the_device_file() {
    use super::plugin_values::PluginDeviceMutation;
    let directory = tempfile::tempdir().expect("create device durability directory");
    {
        let mut store = DeviceStore::open(directory.path()).expect("open device store");
        store
            .write_plugin_device_values(
                "plugin-a",
                &[PluginDeviceMutation::Set {
                    space: "string".to_owned(),
                    key: "durable".to_owned(),
                    value: "kept".to_owned(),
                }],
            )
            .unwrap();
    }
    let reopened = DeviceStore::open(directory.path()).expect("reopen device store");
    assert_eq!(
        reopened
            .read_plugin_device_value("plugin-a", "string", "durable")
            .unwrap(),
        Some("kept".to_owned())
    );
}


mod section_exchange {
    use super::super::sections::{SectionCursor, SectionRow, SectionValueRow, LOCAL_SETTING_KEYS};
    use super::super::{plugin_values::PluginDeviceMutation, DeviceStore, Section};
    use super::open;
    use risunest_sync_wire::Sequence;

    fn plugin_row(key: &str, value: &str, clock: u64, writer: &str) -> SectionRow {
        SectionRow {
            key1: "plugin-a".into(),
            key2: "string".into(),
            key3: key.into(),
            value: SectionValueRow::Plugin {
                space: "string".into(),
                value: value.into(),
            },
            write_clock: Sequence::from(clock),
            writer_id: writer.into(),
        }
    }

    fn plugin_tombstone(key: &str, clock: u64, writer: &str) -> SectionRow {
        SectionRow {
            value: SectionValueRow::Tombstone,
            ..plugin_row(key, "", clock, writer)
        }
    }

    fn set(store: &mut DeviceStore, key: &str, value: &str) {
        store
            .write_plugin_device_values(
                "plugin-a",
                &[PluginDeviceMutation::Set {
                    space: "string".to_owned(),
                    key: key.to_owned(),
                    value: value.to_owned(),
                }],
            )
            .expect("write plugin value");
    }

    fn live(store: &mut DeviceStore) -> Vec<(String, Option<String>)> {
        store
            .read_section_rows(Section::LocalPlugins)
            .expect("read section rows")
            .into_iter()
            .map(|row| {
                (
                    row.key3,
                    match row.value {
                        SectionValueRow::Plugin { value, .. } => Some(value),
                        _ => None,
                    },
                )
            })
            .collect()
    }

    /// Invariant 2. A received row is remote material: it keeps the version it
    /// arrived with, counts as published, and a section this device does not
    /// take part in offers nothing for publication.
    #[test]
    fn a_received_row_stays_remote_material_and_a_non_participating_section_is_not_offered() {
        let (_directory, mut store) = open();
        set(&mut store, "mine", "local");
        store
            .apply_section_rows(
                Section::LocalPlugins,
                &[plugin_row("theirs", "remote", 40, "writer-b")],
            )
            .expect("apply remote rows");

        let (clock, writer, published): (String, String, Option<String>) = store
            .connection()
            .query_row(
                "SELECT write_clock,writer_id,published_clock FROM plugin_device_storage
                    WHERE owner='plugin-a' AND space='string' AND key='theirs'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read received row");
        assert_eq!((clock.as_str(), writer.as_str()), ("40", "writer-b"));
        assert_eq!(published.as_deref(), Some("40"));
        let (own_writer, own_published): (String, Option<String>) = store
            .connection()
            .query_row(
                "SELECT writer_id,published_clock FROM plugin_device_storage
                    WHERE owner='plugin-a' AND space='string' AND key='mine'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read own row");
        assert_ne!(own_writer, "writer-b");
        assert!(own_published.is_none());

        store
            .set_section_participating(Section::LocalPlugins, true)
            .unwrap();
        assert!(store
            .sections_await_publication("connection", "library")
            .unwrap());
        store
            .set_section_participating(Section::LocalPlugins, false)
            .unwrap();
        assert!(!store
            .sections_await_publication("connection", "library")
            .unwrap());
    }

    /// Invariant 15. A deletion travels as a tombstone and the value it removed
    /// does not come back, while a clear touches only the observed keys of the
    /// owner and space it names.
    #[test]
    fn a_received_deletion_does_not_come_back_as_a_live_value() {
        let (_directory, mut store) = open();
        set(&mut store, "gone", "value");
        store
            .apply_section_rows(
                Section::LocalPlugins,
                &[plugin_tombstone("gone", 90, "writer-b")],
            )
            .expect("apply remote tombstone");
        assert_eq!(live(&mut store), vec![("gone".to_owned(), None)]);

        store
            .apply_section_rows(
                Section::LocalPlugins,
                &[plugin_row("gone", "value", 2, "writer-b")],
            )
            .expect("apply stale remote value");
        assert_eq!(live(&mut store), vec![("gone".to_owned(), None)]);

        set(&mut store, "kept", "still here");
        store
            .write_plugin_device_values(
                "plugin-a",
                &[PluginDeviceMutation::Clear {
                    space: "json".to_owned(),
                }],
            )
            .expect("clear another space");
        assert_eq!(
            live(&mut store)
                .into_iter()
                .filter(|(_, value)| value.is_some())
                .collect::<Vec<_>>(),
            vec![("kept".to_owned(), Some("still here".to_owned()))]
        );
    }


    /// Invariant 36 on the receiving side. Applying the same remote section
    /// again changes nothing, so an interrupted receive resumes from the same
    /// state instead of writing a second time.
    #[test]
    fn replaying_a_section_apply_changes_nothing() {
        let (_directory, mut store) = open();
        set(&mut store, "mine", "local");
        let batch = [
            plugin_row("alpha", "from-b", 30, "writer-b"),
            plugin_tombstone("beta", 31, "writer-b"),
        ];
        let first = store
            .apply_section_rows(Section::LocalPlugins, &batch)
            .expect("apply once");
        let after_first = live(&mut store);
        let second = store
            .apply_section_rows(Section::LocalPlugins, &batch)
            .expect("apply again");
        assert_eq!(first.applied, 2);
        assert_eq!(second.applied, 0);
        assert_eq!(second.kept, 2);
        assert_eq!(live(&mut store), after_first);
        assert_eq!(
            store
                .section_state(Section::LocalPlugins)
                .unwrap()
                .max_write_clock,
            Sequence::from(31u64)
        );
    }

    /// Invariant 16. The same values and tombstones in either order leave the
    /// same user state behind.
    #[test]
    fn sections_converge_no_matter_which_order_two_devices_arrive_in() {
        let first = [
            plugin_row("alpha", "from-b", 30, "writer-b"),
            plugin_tombstone("beta", 31, "writer-b"),
        ];
        let second = [
            plugin_row("alpha", "from-c", 29, "writer-c"),
            plugin_row("beta", "from-c", 12, "writer-c"),
            plugin_row("gamma", "from-c", 33, "writer-c"),
        ];
        let settle = |batches: [&[SectionRow]; 2]| {
            let (directory, mut store) = open();
            set(&mut store, "alpha", "local");
            for batch in batches {
                store
                    .apply_section_rows(Section::LocalPlugins, batch)
                    .expect("apply section batch");
            }
            let state = live(&mut store);
            let clock = store
                .section_state(Section::LocalPlugins)
                .unwrap()
                .max_write_clock;
            drop(directory);
            (state, clock)
        };
        let forward = settle([&first, &second]);
        let backward = settle([&second, &first]);
        assert_eq!(forward, backward);
        assert_eq!(
            forward.0,
            vec![
                ("alpha".to_owned(), Some("from-b".to_owned())),
                ("beta".to_owned(), None),
                ("gamma".to_owned(), Some("from-c".to_owned())),
            ]
        );
        assert_eq!(forward.1, Sequence::from(33u64));
    }

    /// The same key at the same version with different content is a real
    /// disagreement, so it is reported instead of resolved by guesswork.
    #[test]
    fn a_section_row_that_differs_at_the_same_version_is_refused() {
        let (_directory, mut store) = open();
        store
            .apply_section_rows(
                Section::LocalPlugins,
                &[plugin_row("alpha", "one", 20, "writer-b")],
            )
            .expect("apply first value");
        assert!(store
            .apply_section_rows(
                Section::LocalPlugins,
                &[plugin_row("alpha", "two", 20, "writer-b")],
            )
            .is_err());
        assert!(store
            .apply_section_rows(Section::LocalPlugins, &[plugin_row("alpha", "one", 20, "")])
            .is_err());
        assert_eq!(
            live(&mut store),
            vec![("alpha".to_owned(), Some("one".to_owned()))]
        );
    }

    /// Invariant 32 on the device side. Restored backup material is this
    /// device's own write, so it installs no other writer and no counters, and
    /// coordination settings never enter a bundle in the first place.
    #[test]
    fn restored_backup_material_becomes_this_device_own_write() {
        let (_directory, mut store) = open();
        store
            .restore_section_rows(
                Section::LocalPlugins,
                &[plugin_row("alpha", "from-backup", 0, "")],
            )
            .expect("restore plugin value");
        let (clock, writer, published): (String, String, Option<String>) = store
            .connection()
            .query_row(
                "SELECT write_clock,writer_id,published_clock FROM plugin_device_storage
                    WHERE owner='plugin-a' AND space='string' AND key='alpha'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read restored row");
        assert_eq!(clock, "1");
        assert_eq!(writer, store.writer_id().unwrap());
        assert!(published.is_none());

        for key in [
            "official-account.association.v1",
            "official-account.asset-ledger.v1",
            "sync-conflict-backups.index.v1",
            "risuNestServerSyncRestoreHold",
            "risu_lastsaved",
        ] {
            assert!(!LOCAL_SETTING_KEYS.contains(&key));
            store
                .write_setting(key, &serde_json::json!("control"))
                .expect("write control setting");
        }
        store
            .write_setting("risuNestDeviceSettings", &serde_json::json!({ "a": 1 }))
            .expect("write device setting");
        let rows = store.read_local_setting_rows().expect("read setting rows");
        assert_eq!(
            rows.iter().map(|row| row.key2.as_str()).collect::<Vec<_>>(),
            vec!["risuNestDeviceSettings"]
        );
    }

    /// A cursor only moves forward, so a replayed apply cannot lose ground and
    /// a published section does not offer the same values again.
    #[test]
    fn a_section_cursor_never_moves_backwards() {
        let (_directory, mut store) = open();
        let cursor = |generation: u64, observed: u64| SectionCursor {
            applied_generation: Sequence::from(generation),
            applied_gc_floor: Sequence::from(0u64),
            observed_max_write_clock: Sequence::from(observed),
        };
        store
            .write_section_cursor("connection", "library", Section::Hypa, &cursor(9, 40))
            .unwrap();
        store
            .write_section_cursor("connection", "library", Section::Hypa, &cursor(3, 12))
            .unwrap();
        assert_eq!(
            store
                .read_section_cursor("connection", "library", Section::Hypa)
                .unwrap(),
            Some(cursor(9, 40))
        );
        assert!(store
            .read_section_cursor("other", "library", Section::Hypa)
            .unwrap()
            .is_none());
    }
}
