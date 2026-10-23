use super::*;

fn open() -> (tempfile::TempDir, DeviceStore) {
    let directory = tempfile::tempdir().expect("create device store directory");
    let store = DeviceStore::open(directory.path()).expect("open fresh device store");
    (directory, store)
}

#[test]
fn binary_plugin_values_survive_multipart_backup_restore_and_reopen() {
    use super::plugin_values::{raw_value, PluginDeviceMutation, PluginDeviceRead};
    use super::sections::{SectionSpoolBuilder, freeze_backup_sections, restore_frozen_backup_sections};
    use risunest_external_storage_format::{section::SectionKind, format::FingerprintBuilder, content_identity::hash};
    let (directory, mut source) = open();
    let (target_directory, mut target) = open();
    let bytes = raw_value::fixture(&vec![255; plugin_values::text_value::PART_BYTES + 20]);
    let opened = source.open_plugin_device_binary_write("owner", "binary", bytes.len() as u64).unwrap();
    let mut offset = 0;
    for part in bytes.chunks(crate::asset_byte_transport::BINARY_PART_BYTES) {
        offset = source.append_plugin_device_bytes(&opened.session_id, offset, part).unwrap();
    }
    source.write_plugin_device_values("owner", &[PluginDeviceMutation::Staged { session_id: opened.session_id }]).unwrap();
    let captured = source.capture_backup_sections(&[SectionKind::Hypa, SectionKind::LocalPlugins, SectionKind::LocalSettings], directory.path()).unwrap();
    let mut received = vec![];
    for section in &captured {
        let mut builder = SectionSpoolBuilder::new_backup(section.kind(), directory.path()).unwrap();
        let mut fingerprint = FingerprintBuilder::new(&section.kind().fingerprint_domain());
        section.visit_entries(|entry, object| {
            assert_eq!(entry.value.object_references().len(), 2);
            fingerprint.push(&entry.key, &hash(&entry.encode().unwrap())).unwrap();
            builder.push_backup_entry(entry.clone(), object)
        }).unwrap();
        received.push(builder.finish(&fingerprint.finish()).unwrap());
    }
    let frozen = freeze_backup_sections(&received.iter().collect::<Vec<_>>()).unwrap();
    let metadata = serde_json::to_vec(&frozen).unwrap();
    assert!(metadata.len() < 4096);
    let frozen = serde_json::from_slice(&metadata).unwrap();
    let tx = target.transaction().unwrap();
    for section in &received { section.retain_plugin_objects(&tx).unwrap(); }
    let stamp = risunest_sync_wire::stamp::Stamp { physical_ms: 1.into(), logical: 0, writer_id: uuid::Uuid::new_v4().to_string() };
    restore_frozen_backup_sections(&tx, &frozen, &stamp).unwrap();
    tx.commit().unwrap();
    drop(target);
    let target = DeviceStore::open(target_directory.path()).unwrap();
    let Some(PluginDeviceRead::Binary { version, byte_size, .. }) = target.read_plugin_device_transfer("owner", "json", "binary").unwrap() else { panic!("expected binary value"); };
    assert_eq!(byte_size, bytes.len() as u64);
    let mut restored = vec![];
    while restored.len() < bytes.len() {
        restored.extend(target.read_plugin_device_bytes("owner", "json", "binary", &version, restored.len() as u64).unwrap());
    }
    assert_eq!(restored, bytes);
    assert!(target.read_plugin_device_transfer("other", "json", "binary").unwrap().is_none());
    assert_eq!(target.list_plugin_device_keys("owner", "json").unwrap(), vec!["binary"]);
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
        "device_sections",
        "device_settings",
        "external_lww_objects",
        "external_lww_receives",
        "external_lww_seen",
        "external_lww_segments",
        "external_lww_sequences",
        "external_lww_versions",
        "hypa_embeddings",
        "lww_clock",
        "lww_device_context",
        "lww_initialization_scopes",
        "lww_intent_failures",
        "lww_intent_proofs",
        "lww_intent_rows",
        "lww_intents",
        "lww_new_device_authorizations",
        "lww_outbox",
        "lww_progress",
        "lww_publications",
        "lww_receive",
        "lww_receive_rows",
        "lww_requests",
        "lww_retired",
        "lww_units",
        "lww_unpublished_proofs",
        "message_page_indexes",
        "message_page_manifests",
        "message_page_object_marks",
        "message_page_objects",
        "message_page_proofs",
        "message_page_sweep_cursor",
        "message_page_verified_objects",
        "plugin_claim_sessions",
        "plugin_device_storage",
        "plugin_gc_state",
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
            StoreError::SchemaMismatch {
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
        StoreError::SchemaMismatch {
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

    assert_eq!(clock(store.connection(), "hypa"), "1");
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
            ("key-b".to_owned(), "1".to_owned(), writer_id.clone(), None),
            ("key-c".to_owned(), "1".to_owned(), writer_id, None),
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
    let stamps = store.connection().prepare("SELECT key,stamp FROM lww_units ORDER BY key").unwrap()
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(stamps.len(), 3);
    assert!(stamps.iter().all(|(_, stamp)| stamp == &stamps[0].1));
    let pending = store.connection().prepare("SELECT key,stamp FROM lww_outbox ORDER BY key").unwrap()
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(pending, stamps);
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
fn an_embedding_batch_with_metadata_is_rejected_without_mutation() {
    let (_directory, mut store) = open();
    let mut unsupported = embedding("key-b", &[2.0]);
    unsupported.metadata = Some("{\"scope\":\"synthetic\"}".to_owned());
    let error = store
        .write_hypa_embeddings(&[embedding("key-a", &[1.0]), unsupported])
        .expect_err("reject embedding metadata");
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
            "mcpStdioApprovals",
            "risuNestDeviceSettings",
            "risuNestStartupExclusions",
            "risuNestPluginSyncSettings",
            "risuNestDefaultPluginsAttempted",
            "risuNestUpdateSettings",
            "risuNestServerSyncRestoreHold",
            "official-account.association.v1",
            "official-account.asset-ledger.v1",
            "official-account.pending-assets.v1",
            "official-account.asset-continuation.v1",
            "sync-conflict-backups.index.v1",
            "native-file-restore-receipts.v1",
        ]
    );

    for key in DEVICE_SETTING_KEYS {
        store
            .write_setting(key, &serde_json::json!("value"))
            .expect("write an assigned key");
    }

    let pending_key = "official-account.pending-assets.v1";
    let pending = serde_json::json!({ "accountId": "synthetic-account" });
    store.write_setting(pending_key, &pending).unwrap();
    drop(store);
    let mut store = DeviceStore::open(_directory.path()).expect("reopen device store");
    assert_eq!(store.read_setting(pending_key).unwrap(), Some(pending));
    store.remove_setting(pending_key).unwrap();
    assert_eq!(store.read_setting(pending_key).unwrap(), None);
    store.write_setting(pending_key, &serde_json::json!("value")).unwrap();

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

/// A backed-up setting travels as one section object, so a value that would
/// not fit one is refused when it is written instead of failing a later backup.
#[test]
fn a_backed_up_setting_is_refused_above_the_section_object_limit() {
    use crate::persistent_store::StoreError;
    use risunest_external_storage_format::section::MAX_SECTION_OBJECT_BYTES;
    use super::sections::LOCAL_SETTING_KEYS;
    let refused = |result: StoreResult<()>, key: &str| match result {
        Err(StoreError::Validation { message }) => {
            assert_eq!(message, "device setting value is too large", "{key}")
        }
        other => panic!("{key}: {other:?}"),
    };
    // A string serializes with its two quotes, and `{"e":...}` adds six bytes.
    let text = |length: usize| Value::String("s".repeat(length - 2));
    let entry = |length: usize| {
        serde_json::json!({ "e": text(length - 6) }).as_object().unwrap().clone()
    };
    let at_limit = text(MAX_SECTION_OBJECT_BYTES);
    let above_limit = text(MAX_SECTION_OBJECT_BYTES + 1);
    assert_eq!(serde_json::to_string(&at_limit).unwrap().len(), MAX_SECTION_OBJECT_BYTES);
    assert_eq!(
        serde_json::to_string(&entry(MAX_SECTION_OBJECT_BYTES)).unwrap().len(),
        MAX_SECTION_OBJECT_BYTES
    );

    let (_directory, mut store) = open();
    for key in LOCAL_SETTING_KEYS {
        assert!(DEVICE_SETTING_KEYS.contains(&key), "{key}");
        store.write_setting(key, &serde_json::json!("kept")).unwrap();
        refused(store.write_setting(key, &above_limit), key);
        assert_eq!(store.read_setting(key).unwrap(), Some(serde_json::json!("kept")));
        store.write_setting(key, &at_limit).expect("a value at the limit is kept");

        store.write_setting(key, &serde_json::json!({})).unwrap();
        refused(store.patch_setting(key, &entry(MAX_SECTION_OBJECT_BYTES + 1)), key);
        assert_eq!(store.read_setting(key).unwrap(), Some(serde_json::json!({})));
        store
            .patch_setting(key, &entry(MAX_SECTION_OBJECT_BYTES))
            .expect("an entry that fills the limit is kept");
        store.remove_setting(key).unwrap();
    }

    // A setting that stays on this device is never carried in a backup.
    store
        .write_setting("mcpStdioApprovals", &above_limit)
        .expect("a device-only setting has no section object to fit");
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
    let (_directory, mut store) = open();
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

    store.clear_plugin_permissions().unwrap();
    assert!(store.read_plugin_permissions().unwrap().is_empty());
    assert!(store.read_plugin_permission_grants().unwrap().is_empty());
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
    use risunest_external_storage_format::section::SectionKind;

    use super::super::sections::{
        SectionRow, SectionSpoolBuilder,
        SectionValueRow, TombstonePublication, LOCAL_SETTING_KEYS,
    };
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
        marked_plugin_tombstone(key, clock, writer, None)
    }

    fn marked_plugin_tombstone(
        key: &str,
        clock: u64,
        writer: &str,
        marker: Option<(u64, u64)>,
    ) -> SectionRow {
        SectionRow {
            value: SectionValueRow::Tombstone {
                first_published: marker.map(|(generation, at_ms)| TombstonePublication {
                    generation: Sequence::from(generation),
                    at_ms,
                }),
            },
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
    /// arrived with and counts as published.
    #[test]
    fn a_received_row_stays_remote_material() {
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

    /// Invariant 32 on the device side. Coordination settings never enter a
    /// bundle in the first place.
    #[test]
    fn coordination_settings_never_enter_local_setting_rows() {
        let (_directory, mut store) = open();
        for key in [
            "official-account.association.v1",
            "official-account.asset-ledger.v1",
            "official-account.pending-assets.v1",
            "official-account.asset-continuation.v1",
            "sync-conflict-backups.index.v1",
            "native-file-restore-receipts.v1",
            "risuNestServerSyncRestoreHold",
            "risu_lastsaved",
            "risuNestDefaultPluginsAttempted",
            "risuNestPluginSyncSettings",
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

    #[test]
    fn backup_spool_rejects_duplicate_keys_before_restore() {
        let mut spool = SectionSpoolBuilder::new_backup(SectionKind::LocalPlugins, &std::env::temp_dir()).unwrap();
        let row = plugin_row("duplicate", "one", 0, "");
        spool.push_backup_row(row.clone()).unwrap();
        assert!(spool.push_backup_row(row).is_err());
    }

    /// The stored text decides the form: up to the inline limit a value rides
    /// in its entry, past it the entry names an object holding that text.
    #[test]
    fn a_device_value_past_the_inline_limit_becomes_an_object() {
        use risunest_external_storage_format::section::MAX_INLINE_VALUE_BYTES;
        let encoded = |row: SectionRow, kind| {
            String::from_utf8(row.to_entry(kind, false).unwrap().encode().unwrap()).unwrap()
        };
        let json_row = |value: String| SectionRow {
            key2: "json".into(),
            value: SectionValueRow::Plugin { space: "json".into(), value },
            ..plugin_row("edge", "", 0, "")
        };
        let setting_row = |value: String| SectionRow {
            key1: "setting".into(),
            key2: "risuNestDeviceSettings".into(),
            key3: String::new(),
            value: SectionValueRow::Setting { value },
            write_clock: Sequence::from(0u64),
            writer_id: String::new(),
        };
        let quoted = |length: usize| format!("\"{}\"", "q".repeat(length - 2));
        for (at_limit, past, inline, object) in [
            (
                encoded(plugin_row("edge", &"a".repeat(MAX_INLINE_VALUE_BYTES), 0, ""), SectionKind::LocalPlugins),
                encoded(plugin_row("edge", &"a".repeat(MAX_INLINE_VALUE_BYTES + 1), 0, ""), SectionKind::LocalPlugins),
                "\"localPlugin\":{\"space\":\"string\",\"value\":",
                "\"localPlugin\":{\"space\":\"string\",\"object\":{\"contentSha256\":",
            ),
            (
                encoded(json_row(quoted(MAX_INLINE_VALUE_BYTES)), SectionKind::LocalPlugins),
                encoded(json_row(quoted(MAX_INLINE_VALUE_BYTES + 1)), SectionKind::LocalPlugins),
                "\"localPlugin\":{\"space\":\"json\",\"value\":",
                "\"localPlugin\":{\"space\":\"json\",\"object\":{\"contentSha256\":",
            ),
            (
                encoded(setting_row(quoted(MAX_INLINE_VALUE_BYTES)), SectionKind::LocalSettings),
                encoded(setting_row(quoted(MAX_INLINE_VALUE_BYTES + 1)), SectionKind::LocalSettings),
                "\"localSetting\":{\"value\":",
                "\"localSetting\":{\"object\":{\"contentSha256\":",
            ),
        ] {
            assert!(at_limit.contains(inline) && !at_limit.contains(object));
            assert!(past.contains(object) && !past.contains(inline));
            assert!(past.contains(&format!("\"byteLength\":\"{}\"", MAX_INLINE_VALUE_BYTES + 1)));
        }
        use super::super::{sections, plugin_values::text_value};
        use risunest_external_storage_format::{content_identity::hash, format::FingerprintBuilder};
        let (directory, mut store) = open();
        let (_target_directory, mut target) = open();
        let text = format!("{}가😀", "q".repeat(text_value::PART_BYTES - 3));
        for space in ["string", "json"] {
            let value = if space == "json" { serde_json::to_string(&text).unwrap() } else { text.clone() };
            let opened = store.open_plugin_device_write("plugin-a", space, "multipart", text_value::encoded_length(&value)).unwrap();
            let id = serde_json::to_value(opened).unwrap()["sessionId"].as_str().unwrap().to_owned();
            let mut start = 0;
            while start < value.len() {
                let mut end = (start + text_value::PART_BYTES - 16).min(value.len());
                while !value.is_char_boundary(end) { end -= 1; }
                store.append_plugin_device_write("plugin-a", &id, start as u64, &value[start..end]).unwrap(); start = end;
            }
            store.write_plugin_device_values("plugin-a", &[PluginDeviceMutation::Staged { session_id: id }]).unwrap();
        }
        let captured = store.capture_backup_sections(&[SectionKind::Hypa, SectionKind::LocalPlugins, SectionKind::LocalSettings], directory.path()).unwrap();
        let mut received = Vec::new();
        for section in &captured {
            let mut builder = SectionSpoolBuilder::new_backup(section.kind(), directory.path()).unwrap();
            let mut fingerprint = FingerprintBuilder::new(&section.kind().fingerprint_domain());
            section.visit_entries(|entry, object| {
                if section.kind() == SectionKind::LocalPlugins {
                    assert_eq!(entry.value.object_references().len(), 2);
                    assert!(entry.value.object_references().iter().all(|part| part.byte_length <= text_value::PART_BYTES as u64));
                    let mut refused = SectionSpoolBuilder::new_backup(section.kind(), directory.path())?;
                    assert!(refused.push_backup_entry(entry.clone(), |_| Ok(vec![0])).is_err());
                }
                fingerprint.push(&entry.key, &hash(&entry.encode().unwrap())).unwrap();
                builder.push_backup_entry(entry.clone(), object)
            }).unwrap();
            received.push(builder.finish(&fingerprint.finish()).unwrap());
        }
        let frozen = sections::freeze_backup_sections(&received.iter().collect::<Vec<_>>()).unwrap();
        let encoded = serde_json::to_vec(&frozen).unwrap();
        assert!(encoded.len() < 4096);
        let frozen = serde_json::from_slice(&encoded).unwrap();
        let tx = target.transaction().unwrap();
        for section in &received { section.retain_plugin_objects(&tx).unwrap(); }
        drop(received);
        let stamp = risunest_sync_wire::stamp::Stamp { physical_ms: 1.into(), logical: 0, writer_id: uuid::Uuid::new_v4().to_string() };
        sections::restore_frozen_backup_sections(&tx, &frozen, &stamp).unwrap();
        tx.commit().unwrap();
        assert_eq!(target.read_plugin_device_value("plugin-a", "string", "multipart").unwrap(), Some(text.clone()));
        assert_eq!(target.read_plugin_device_value("plugin-a", "json", "multipart").unwrap(), Some(serde_json::to_string(&text).unwrap()));
        let types: i64 = target.connection().query_row("SELECT count(*) FROM plugin_device_storage WHERE typeof(value)='text'", [], |row| row.get(0)).unwrap();
        assert_eq!(types, 2);
    }

    /// An object installs only as the stored text its entry names: the body
    /// matches the reference, it is text, and a JSON value still parses.
    #[test]
    fn a_device_value_object_that_is_not_its_stored_text_is_refused() {
        use crate::persistent_store::StoreError;
        use risunest_external_storage_format::{
            content_identity::hash,
            section::{
                local_plugin_entry_key, LocalPluginValue, LocalSettingValue, ObjectReference,
                PluginSpace, SectionEntry, SectionValue, ValueOrObject,
            },
        };
        let object = |body: &[u8]| ValueOrObject::Object(ObjectReference {
            content_sha256: hash(body),
            byte_length: body.len() as u64,
        });
        let plugin = |space: PluginSpace, body: &[u8]| {
            let id = if space == PluginSpace::Json { "json" } else { "string" };
            SectionEntry::new(
                SectionKind::LocalPlugins,
                local_plugin_entry_key("plugin-a", id, "key").unwrap(),
                SectionValue::LocalPlugin(LocalPluginValue { space, value: object(body).into() }),
                None,
            )
            .unwrap()
        };
        let setting = |body: &[u8]| {
            SectionEntry::new(
                SectionKind::LocalSettings,
                serde_json::to_string(&["setting", "risuNestDeviceSettings", ""]).unwrap(),
                SectionValue::LocalSetting(LocalSettingValue { value: object(body) }),
                None,
            )
            .unwrap()
        };
        let push = |entry: SectionEntry, body: &[u8]| {
            let mut spool = SectionSpoolBuilder::new_backup(entry.kind, &std::env::temp_dir()).unwrap();
            spool.push_backup_entry(entry, |_| Ok(body.to_vec()))
        };
        let refused = |result: crate::persistent_store::StoreResult<()>| {
            assert!(matches!(result, Err(StoreError::Validation { .. })), "{result:?}");
        };
        let json = format!("{{ \"pad\": \"{}\" }}", "p".repeat(5_000));
        let broken = format!("{{\"pad\":\"{}\"", "b".repeat(5_000));
        let mut not_text = "t".repeat(5_000).into_bytes();
        not_text[7] = 0xff;
        let mut altered = json.clone().into_bytes();
        altered[10] = b'q';

        push(plugin(PluginSpace::Json, json.as_bytes()), json.as_bytes()).unwrap();
        push(plugin(PluginSpace::String, broken.as_bytes()), broken.as_bytes()).unwrap();
        push(setting(json.as_bytes()), json.as_bytes()).unwrap();
        refused(push(plugin(PluginSpace::Json, broken.as_bytes()), broken.as_bytes()));
        refused(push(setting(broken.as_bytes()), broken.as_bytes()));
        refused(push(plugin(PluginSpace::String, &not_text), &not_text));
        refused(push(plugin(PluginSpace::String, json.as_bytes()), &altered));
        refused(push(plugin(PluginSpace::String, json.as_bytes()), &json.as_bytes()[1..]));

        let unparsable = SectionRow {
            key2: "json".into(),
            value: SectionValueRow::Plugin { space: "json".into(), value: broken.clone() },
            ..plugin_row("key", "", 0, "")
        };
        assert!(matches!(
            unparsable.to_entry(SectionKind::LocalPlugins, false),
            Err(StoreError::Validation { .. })
        ));
    }

    /// Stored JSON text that does not parse is refused the same way whether it
    /// would ride inline or as an object, and the refusal never repeats it.
    #[test]
    fn unparseable_stored_json_is_refused_alike_at_any_length() {
        use crate::persistent_store::StoreError;
        use risunest_external_storage_format::section::MAX_INLINE_VALUE_BYTES;
        let refusal = |kind: SectionKind, broken: &str| {
            let (_directory, mut store) = open();
            if kind == SectionKind::LocalPlugins {
                store
                    .write_plugin_device_values(
                        "plugin-a",
                        &[PluginDeviceMutation::Set {
                            space: "json".to_owned(),
                            key: "broken".to_owned(),
                            value: broken.to_owned(),
                        }],
                    )
                    .expect("write plugin value");
            } else {
                store
                    .connection()
                    .execute(
                        "INSERT INTO device_settings(key,value) VALUES('risuNestDeviceSettings',?1)",
                        [broken],
                    )
                    .expect("write setting");
            }
            match store.capture_backup_sections(&[kind], &std::env::temp_dir()).err().expect("capture refused") {
                StoreError::Validation { message } => {
                    assert!(!message.contains("secret"), "{message}");
                    message
                }
                other => panic!("{other:?}"),
            }
        };
        let small = "{\"secret\":\"s\"".to_owned();
        let large = format!("{{\"secret\":\"{}\"", "s".repeat(MAX_INLINE_VALUE_BYTES));
        assert!(small.len() <= MAX_INLINE_VALUE_BYTES && large.len() > MAX_INLINE_VALUE_BYTES);
        for kind in [SectionKind::LocalPlugins, SectionKind::LocalSettings] {
            assert_eq!(refusal(kind, &small), refusal(kind, &large));
        }
    }

}

/// Synchronisation subscribes to the device counter the way it subscribes to
/// the library revision, so a stored section value has to advance it and a
/// device-fixed write has to leave it alone.
#[test]
fn a_synchronised_section_write_advances_the_device_revision_and_a_fixed_write_does_not() {
    use super::hypa::HypaEmbeddingWrite;
    use super::plugin_values::PluginDeviceMutation;
    let (_directory, mut store) = open();
    let start = store.revision().expect("read device revision");

    store
        .write_hypa_embeddings(&[])
        .expect("write no embeddings");
    assert_eq!(store.revision().unwrap(), start);

    store
        .write_hypa_embeddings(&[HypaEmbeddingWrite {
            cache_key: "a".repeat(64),
            producer: "hypa-v2".into(),
            model: "synthetic-embedding".into(),
            endpoint: None,
            preprocess_version: 1,
            dimensions: 4,
            vector: vec![0x11; 16],
            metadata: None,
        }])
        .expect("write an embedding");
    let after_hypa = store.revision().unwrap();
    assert!(after_hypa > start);

    store
        .write_plugin_device_values(
            "synthetic-plugin",
            &[PluginDeviceMutation::Set {
                space: "string".into(),
                key: "token".into(),
                value: "held".into(),
            }],
        )
        .expect("write a plugin value");
    let after_plugin = store.revision().unwrap();
    assert!(after_plugin > after_hypa);

    store
        .patch_setting(
            "risuNestDeviceSettings",
            serde_json::json!({ "performanceProfile": "balanced" })
                .as_object()
                .unwrap(),
        )
        .expect("patch a device setting");
    store
        .write_plugin_permission_grant("synthetic-plugin", "network", 1)
        .expect("grant a plugin permission");
    assert_eq!(store.revision().unwrap(), after_plugin);
}

#[test]
fn clear_hypa_cache_reclaims_vectors_and_issues_publishable_tombstones() {
    let (_directory, mut store) = open();
    store.write_hypa_embeddings(&[embedding("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", &[1.0,2.0]), embedding("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", &[3.0])]).unwrap();
    assert_eq!(store.hypa_embedding_usage().unwrap(), (2,12));
    let before = store.revision().unwrap();
    store.clear_hypa_embeddings().unwrap();
    assert!(store.revision().unwrap() > before);
    assert_eq!(store.hypa_embedding_usage().unwrap(), (0,0));
    assert!(store.read_hypa_embeddings(&["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned()]).unwrap().iter().all(|row| row.vector.is_none()));
    let tombstones: i64 = store.connection().query_row("SELECT count(*) FROM hypa_embeddings WHERE tombstone=1 AND vector IS NULL AND published_clock IS NULL", [], |row| row.get(0)).unwrap();
    assert_eq!(tombstones, 2);
    assert!(store.read_backup_section_rows(Section::Hypa).unwrap().is_empty());
    let (_peer_directory, mut peer) = open();
    peer.write_hypa_embeddings(&[embedding("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", &[1.0,2.0])]).unwrap();
    let changes = store.read_section_rows(Section::Hypa).unwrap();
    peer.apply_section_rows(Section::Hypa, &changes).unwrap();
    assert_eq!(peer.hypa_embedding_usage().unwrap(), (0,0));
    let cleared = store.revision().unwrap();
    store.clear_hypa_embeddings().unwrap();
    assert_eq!(store.revision().unwrap(), cleared);
}
