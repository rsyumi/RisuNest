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
