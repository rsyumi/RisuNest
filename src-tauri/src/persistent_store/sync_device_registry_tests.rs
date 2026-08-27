use super::{
    PersistentStore, RegisteredSyncDeviceStatus, SyncGenerationIdentity,
    VerifiedSyncDeviceRegistration,
};
use rusqlite::params;

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn seed_complete_generation(
    store: &PersistentStore,
    library_id: &str,
    generation_id: &str,
    generation_sequence: &str,
    manifest_hash: &str,
) {
    store
        .connection
        .execute(
            "INSERT INTO logical_sync_generations (
                library_id, generation_id, generation_sequence, parent_generation_id,
                pds_generation, source_revision, state, manifest_hash, created_at, completed_at
             ) VALUES (?1, ?2, ?3, NULL, ?2, 0, 'complete', ?4, 1, 1)",
            params![
                library_id,
                generation_id,
                generation_sequence,
                manifest_hash
            ],
        )
        .expect("seed complete logical generation");
}

#[test]
fn schema_v13_creates_generation_independent_device_registry() {
    let directory = tempfile::tempdir().expect("create schema fixture");
    let store = PersistentStore::open(directory.path()).expect("open fresh store");

    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read schema version"),
        13
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT COUNT(*) FROM logical_sync_devices", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("query device registry"),
        0
    );
    assert!(super::GENERATION_TABLES
        .iter()
        .all(|(table, _)| *table != "logical_sync_devices"));
}

#[test]
fn verified_registration_and_ack_commit_exact_identity_with_common_base() {
    let directory = tempfile::tempdir().expect("create registry fixture");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);

    let identity = SyncGenerationIdentity {
        generation_id: "generation-1".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "1".to_owned(),
    };
    store
        .register_verified_sync_device(
            VerifiedSyncDeviceRegistration::for_test("library", "device-a", identity.clone(), 10),
            0,
        )
        .expect("register verified device");

    let devices = store
        .list_sync_devices("library")
        .expect("list registered devices");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].status, RegisteredSyncDeviceStatus::Active);
    assert_eq!(devices[0].acknowledged_generation, identity);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM logical_peer_common_bases
                 WHERE peer_id = 'device-a' AND library_id = 'library'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count matching common base"),
        1
    );
}

#[test]
fn authenticated_p5_registration_attaches_to_an_exact_p4_common_base_idempotently() {
    let directory = tempfile::tempdir().expect("create P5 attachment fixture");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    let identity = SyncGenerationIdentity {
        generation_id: "generation-1".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "1".to_owned(),
    };
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('device-a', 'library', 'generation-1', ?1, '1', 10)",
            [HASH_A],
        )
        .expect("seed existing P4 common base");
    let receipt = VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
        "library",
        "device-a",
        identity.clone(),
        10,
    )
    .expect("construct authenticated P5 receipt");

    let attached = store
        .attach_verified_sync_device_at_common_base(receipt.clone(), 0)
        .expect("attach P5 registry row to P4 common base");
    let retried = store
        .attach_verified_sync_device_at_common_base(receipt, 0)
        .expect("return exact active registration on retry");

    assert_eq!(attached, retried);
    assert_eq!(attached.status, RegisteredSyncDeviceStatus::Active);
    assert_eq!(attached.acknowledged_generation, identity);
    assert_eq!(
        store
            .connection
            .query_row("SELECT COUNT(*) FROM logical_sync_devices", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count attached registry rows"),
        1
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM logical_peer_common_bases",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .expect("preserve existing P4 common base"),
        1
    );
}

#[test]
fn authenticated_p5_registration_rejects_mismatched_common_base_without_mutation() {
    let directory = tempfile::tempdir().expect("create mismatched attachment fixture");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    seed_complete_generation(&store, "library", "generation-2", "2", HASH_A);
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('device-a', 'library', 'generation-1', ?1, '1', 10)",
            [HASH_A],
        )
        .expect("seed P4 common base");
    let stale = SyncGenerationIdentity {
        generation_id: "generation-2".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "2".to_owned(),
    };

    assert!(store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                "library", "device-a", stale, 20,
            )
            .expect("construct authenticated P5 receipt"),
            0,
        )
        .is_err());
    assert!(store
        .list_sync_devices("library")
        .expect("list unchanged registry")
        .is_empty());
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT generation_id FROM logical_peer_common_bases
                 WHERE peer_id = 'device-a' AND library_id = 'library'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read unchanged P4 common base"),
        "generation-1"
    );
}

#[test]
fn authenticated_p5_registration_rejects_revoked_forgotten_and_stale_devices_without_mutation() {
    let directory = tempfile::tempdir().expect("create lifecycle attachment fixture");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    seed_complete_generation(&store, "library", "generation-2", "2", HASH_A);
    let first = SyncGenerationIdentity {
        generation_id: "generation-1".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "1".to_owned(),
    };
    let second = SyncGenerationIdentity {
        generation_id: "generation-2".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "2".to_owned(),
    };
    for device_id in ["revoked", "forgotten", "stale"] {
        store
            .connection
            .execute(
                "INSERT INTO logical_peer_common_bases (
                    peer_id, library_id, generation_id, manifest_hash,
                    generation_sequence, updated_at
                 ) VALUES (?1, 'library', 'generation-1', ?2, '1', 10)",
                params![device_id, HASH_A],
            )
            .expect("seed P4 common base");
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    "library",
                    device_id,
                    first.clone(),
                    10,
                )
                .expect("construct authenticated P5 receipt"),
                0,
            )
            .expect("attach active device");
    }
    store
        .revoke_sync_device("library", "revoked", &first)
        .expect("revoke device");
    store
        .forget_sync_device("library", "forgotten", &first)
        .expect("forget device");

    for (device_id, identity) in [
        ("revoked", first.clone()),
        ("forgotten", first.clone()),
        ("stale", second),
    ] {
        assert!(store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    "library", device_id, identity, 20,
                )
                .expect("construct authenticated P5 retry receipt"),
                0,
            )
            .is_err());
    }

    let devices = store
        .list_sync_devices("library")
        .expect("list unchanged lifecycle registry");
    assert_eq!(devices[0].status, RegisteredSyncDeviceStatus::Forgotten);
    assert_eq!(devices[1].status, RegisteredSyncDeviceStatus::Revoked);
    assert_eq!(devices[2].status, RegisteredSyncDeviceStatus::Active);
    assert_eq!(devices[2].acknowledged_generation, first);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM logical_peer_common_bases
                 WHERE library_id = 'library'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count unchanged common bases"),
        2
    );
}

#[test]
fn device_id_limit_counts_unicode_characters_and_generation_id_is_unbounded() {
    let directory = tempfile::tempdir().expect("create identifier fixture");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    let device_id = "🐿".repeat(1_024);
    let generation_id = "generation".repeat(1_025);
    seed_complete_generation(&store, "library", &generation_id, "1", HASH_A);
    let identity = SyncGenerationIdentity {
        generation_id,
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "1".to_owned(),
    };

    store
        .register_verified_sync_device(
            VerifiedSyncDeviceRegistration::for_test("library", &device_id, identity.clone(), 10),
            0,
        )
        .expect("accept 1024 Unicode characters and a long generation id");

    let oversized_device_id = format!("{device_id}🐿");
    assert!(store
        .register_verified_sync_device(
            VerifiedSyncDeviceRegistration::for_test(
                "library",
                oversized_device_id.as_str(),
                identity,
                10,
            ),
            0,
        )
        .is_err());
}

#[test]
fn tombstone_plan_is_bounded_and_revoked_devices_always_block() {
    let directory = tempfile::tempdir().expect("create tombstone fixture");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    store
        .connection
        .execute(
            "INSERT INTO logical_library_head (singleton, library_id, generation_id)
             VALUES (1, 'library', 'generation-1')",
            [],
        )
        .expect("seed logical head");
    store
        .connection
        .execute(
            "INSERT INTO logical_record_heads (
                library_id, generation_id, record_key, record_kind, state,
                object_hash, object_size, deleted_generation_sequence
             ) VALUES ('library', 'generation-1', 'asset:old', 'asset', 'tombstone', NULL, 0, '1')",
            [],
        )
        .expect("seed tombstone");
    let identity = SyncGenerationIdentity {
        generation_id: "generation-1".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "1".to_owned(),
    };
    store
        .register_verified_sync_device(
            VerifiedSyncDeviceRegistration::for_test("library", "device-a", identity.clone(), 10),
            0,
        )
        .expect("register device");

    store
        .register_verified_sync_device(
            VerifiedSyncDeviceRegistration::for_test("library", "device-b", identity.clone(), 10),
            0,
        )
        .expect("register second device");

    store
        .revoke_sync_device("library", "device-a", &identity)
        .expect("revoke device");

    let page = store
        .plan_tombstone_collection(&identity, None, 1)
        .expect("plan tombstone collection");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].record_key, "asset:old");
    assert_eq!(page.items[0].blocking_device_ids, ["device-a", "device-b"]);
    assert!(page.next_cursor.is_none());
}

#[test]
fn schema_v12_common_bases_migrate_to_revoked_exact_audit_rows() {
    let directory = tempfile::tempdir().expect("create migration fixture");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('device-a', 'library', 'generation-1', ?1, '1', 42)",
            [HASH_A],
        )
        .expect("seed v12 common base");
    store
        .connection
        .execute_batch(
            "DROP INDEX logical_sync_devices_status;
             DROP TABLE logical_sync_devices;
             PRAGMA user_version = 12;",
        )
        .expect("downgrade fixture to v12");
    drop(store);

    let migrated = PersistentStore::open(directory.path()).expect("migrate v12 fixture");
    let device = migrated
        .list_sync_devices("library")
        .expect("list migrated devices")
        .pop()
        .expect("migrated device");
    assert_eq!(device.status, RegisteredSyncDeviceStatus::Revoked);
    assert_eq!(device.acknowledged_generation.generation_id, "generation-1");
    assert_eq!(device.acknowledged_generation.manifest_hash, HASH_A);
    assert_eq!(device.acknowledged_generation.generation_sequence, "1");
    assert_eq!(device.registered_at, 42);
    assert_eq!(device.acknowledged_at, 42);
    assert_eq!(device.revoked_at, Some(42));
}

#[test]
fn invalid_v12_common_base_rolls_back_the_entire_v13_migration() {
    let directory = tempfile::tempdir().expect("create invalid migration fixture");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES (?1, 'library', 'generation-1', ?2, '1', 42)",
            params!["x".repeat(1_025), HASH_A],
        )
        .expect("seed oversized legacy peer id");
    store
        .connection
        .execute_batch(
            "DROP INDEX logical_sync_devices_status;
             DROP TABLE logical_sync_devices;
             PRAGMA user_version = 12;",
        )
        .expect("downgrade invalid fixture to v12");
    drop(store);

    assert!(PersistentStore::open(directory.path()).is_err());
    let connection = rusqlite::Connection::open(directory.path().join("persistent/persistent.db"))
        .expect("reopen rolled-back v12 database");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read rolled-back version"),
        12
    );
    assert!(!connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name = 'logical_sync_devices'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .expect("inspect rolled-back schema"));
}

#[test]
fn invalid_v12_common_base_hash_sequence_and_timestamp_each_roll_back_v13() {
    for corruption in [
        "UPDATE logical_peer_common_bases SET manifest_hash = 'corrupt'",
        "UPDATE logical_peer_common_bases SET generation_sequence = '01'",
        "UPDATE logical_peer_common_bases SET updated_at = -1",
        "UPDATE logical_sync_generations SET completed_at = NULL",
    ] {
        let directory = tempfile::tempdir().expect("create corrupt migration fixture");
        let store = PersistentStore::open(directory.path()).expect("create current store");
        seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
        store
            .connection
            .execute(
                "INSERT INTO logical_peer_common_bases (
                    peer_id, library_id, generation_id, manifest_hash,
                    generation_sequence, updated_at
                 ) VALUES ('device-a', 'library', 'generation-1', ?1, '1', 42)",
                [HASH_A],
            )
            .expect("seed valid v12 common base");
        store
            .connection
            .execute_batch(&format!(
                "PRAGMA ignore_check_constraints = ON;
                 {corruption};
                 PRAGMA ignore_check_constraints = OFF;
                 DROP INDEX logical_sync_devices_status;
                 DROP TABLE logical_sync_devices;
                 PRAGMA user_version = 12;"
            ))
            .expect("corrupt v12 common base");
        drop(store);

        assert!(PersistentStore::open(directory.path()).is_err());
        let connection =
            rusqlite::Connection::open(directory.path().join("persistent/persistent.db"))
                .expect("reopen rolled-back corrupt database");
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .expect("read rolled-back schema version"),
            12
        );
    }
}

#[test]
fn v13_reopen_rejects_changed_table_constraints_and_index_definition() {
    let table_directory = tempfile::tempdir().expect("create table corruption fixture");
    let table_store = PersistentStore::open(table_directory.path()).expect("open current store");
    table_store
        .connection
        .execute_batch(
            "DROP INDEX logical_sync_devices_status;
             DROP TABLE logical_sync_devices;
             CREATE TABLE logical_sync_devices (
                library_id TEXT NOT NULL,
                device_id TEXT NOT NULL,
                status TEXT NOT NULL,
                acknowledged_generation_id TEXT NOT NULL,
                acknowledged_manifest_hash TEXT NOT NULL,
                acknowledged_generation_sequence TEXT NOT NULL,
                registered_at INTEGER NOT NULL,
                acknowledged_at INTEGER NOT NULL,
                revoked_at INTEGER,
                forgotten_at INTEGER,
                PRIMARY KEY (library_id, device_id)
             );
             CREATE INDEX logical_sync_devices_status
                ON logical_sync_devices (library_id, status, device_id);",
        )
        .expect("replace registry with weaker constraints");
    drop(table_store);
    assert!(PersistentStore::open(table_directory.path()).is_err());

    let index_directory = tempfile::tempdir().expect("create index corruption fixture");
    let index_store = PersistentStore::open(index_directory.path()).expect("open current store");
    index_store
        .connection
        .execute_batch(
            "DROP INDEX logical_sync_devices_status;
             CREATE INDEX logical_sync_devices_status
                ON logical_sync_devices (device_id, status, library_id);",
        )
        .expect("replace registry index definition");
    drop(index_store);
    assert!(PersistentStore::open(index_directory.path()).is_err());
}

#[test]
fn v12_snapshot_restore_migrates_common_base_to_revoked_device() {
    let directory = tempfile::tempdir().expect("create v12 snapshot fixture");
    let store = PersistentStore::open(directory.path()).expect("open current store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('snapshot-device', 'library', 'generation-1', ?1, '1', 42)",
            [HASH_A],
        )
        .expect("seed snapshot common base");
    let snapshot = store
        .snapshot_create("p5-v12")
        .expect("create v13 snapshot");
    let snapshot_connection =
        rusqlite::Connection::open(&snapshot.path).expect("open snapshot candidate");
    snapshot_connection
        .execute_batch(
            "DROP INDEX logical_sync_devices_status;
             DROP TABLE logical_sync_devices;
             PRAGMA user_version = 12;",
        )
        .expect("convert snapshot candidate to v12");
    drop(snapshot_connection);
    store
        .snapshot_restore_request(std::path::Path::new(&snapshot.path))
        .expect("request v12 snapshot restore");
    drop(store);

    let restored =
        PersistentStore::open(directory.path()).expect("restore and migrate v12 snapshot");
    let devices = restored
        .list_sync_devices("library")
        .expect("list restored devices");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device_id, "snapshot-device");
    assert_eq!(devices[0].status, RegisteredSyncDeviceStatus::Revoked);
    assert_eq!(devices[0].registered_at, 42);
}

#[test]
fn acknowledgement_rejects_forks_regression_and_stale_common_base_atomically() {
    let directory = tempfile::tempdir().expect("create acknowledgement fixture");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    seed_complete_generation(&store, "library", "generation-2", "2", HASH_A);
    seed_complete_generation(&store, "library", "generation-fork", "1", &"b".repeat(64));
    let first = SyncGenerationIdentity {
        generation_id: "generation-1".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "1".to_owned(),
    };
    store
        .register_verified_sync_device(
            VerifiedSyncDeviceRegistration::for_test("library", "device-a", first.clone(), 10),
            0,
        )
        .expect("register device");

    assert!(store
        .advance_sync_device_ack(
            "library",
            "device-a",
            &first,
            &SyncGenerationIdentity {
                generation_id: "generation-0".to_owned(),
                manifest_hash: HASH_A.to_owned(),
                generation_sequence: "0".to_owned(),
            },
        )
        .is_err());

    assert!(store
        .advance_sync_device_ack(
            "library",
            "device-a",
            &first,
            &SyncGenerationIdentity {
                generation_id: "generation-fork".to_owned(),
                manifest_hash: "b".repeat(64),
                generation_sequence: "1".to_owned(),
            },
        )
        .is_err());
    let retry = store
        .advance_sync_device_ack("library", "device-a", &first, &first)
        .expect("retry exact acknowledgement");
    assert_eq!(retry.acknowledged_generation, first);

    store
        .connection
        .execute(
            "UPDATE logical_peer_common_bases SET manifest_hash = ?1
             WHERE peer_id = 'device-a' AND library_id = 'library'",
            ["b".repeat(64)],
        )
        .expect("corrupt common base expected state");
    let second = SyncGenerationIdentity {
        generation_id: "generation-2".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "2".to_owned(),
    };
    assert!(store
        .advance_sync_device_ack("library", "device-a", &first, &second)
        .is_err());
    assert_eq!(
        store
            .list_sync_devices("library")
            .expect("read unchanged registry")[0]
            .acknowledged_generation,
        first
    );
}

#[test]
fn acknowledgement_rejects_reused_generation_ids_and_missing_exact_retry_targets() {
    let directory = tempfile::tempdir().expect("create reused generation fixture");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    let first = SyncGenerationIdentity {
        generation_id: "generation-1".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "1".to_owned(),
    };
    store
        .register_verified_sync_device(
            VerifiedSyncDeviceRegistration::for_test("library", "device-a", first.clone(), 10),
            0,
        )
        .expect("register device");
    store
        .connection
        .execute(
            "DELETE FROM logical_sync_generations
             WHERE library_id = 'library' AND generation_id = 'generation-1'",
            [],
        )
        .expect("remove acknowledged generation to model corruption");

    assert!(store
        .advance_sync_device_ack("library", "device-a", &first, &first)
        .is_err());
    seed_complete_generation(&store, "library", "generation-1", "2", &"b".repeat(64));
    assert!(store
        .advance_sync_device_ack(
            "library",
            "device-a",
            &first,
            &SyncGenerationIdentity {
                generation_id: "generation-1".to_owned(),
                manifest_hash: "b".repeat(64),
                generation_sequence: "2".to_owned(),
            },
        )
        .is_err());
    assert_eq!(
        store
            .list_sync_devices("library")
            .expect("read unchanged device")[0]
            .acknowledged_generation,
        first
    );
}

#[test]
fn stale_ack_and_revoke_compare_and_swap_states_cannot_both_commit() {
    let directory = tempfile::tempdir().expect("create lifecycle CAS fixture");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    seed_complete_generation(&store, "library", "generation-2", "2", HASH_A);
    let first = SyncGenerationIdentity {
        generation_id: "generation-1".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "1".to_owned(),
    };
    let second = SyncGenerationIdentity {
        generation_id: "generation-2".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "2".to_owned(),
    };
    for device_id in ["ack-first", "revoke-first"] {
        store
            .register_verified_sync_device(
                VerifiedSyncDeviceRegistration::for_test("library", device_id, first.clone(), 10),
                0,
            )
            .expect("register CAS device");
    }

    store
        .advance_sync_device_ack("library", "ack-first", &first, &second)
        .expect("commit ACK first");
    assert!(store
        .revoke_sync_device("library", "ack-first", &first)
        .is_err());
    store
        .revoke_sync_device("library", "revoke-first", &first)
        .expect("commit revoke first");
    assert!(store
        .advance_sync_device_ack("library", "revoke-first", &first, &second)
        .is_err());

    let devices = store
        .list_sync_devices("library")
        .expect("list CAS devices");
    assert_eq!(devices[0].device_id, "ack-first");
    assert_eq!(devices[0].status, RegisteredSyncDeviceStatus::Active);
    assert_eq!(devices[0].acknowledged_generation, second);
    assert_eq!(devices[1].device_id, "revoke-first");
    assert_eq!(devices[1].status, RegisteredSyncDeviceStatus::Revoked);
    assert_eq!(devices[1].acknowledged_generation, first);
}

#[test]
fn forgetting_removes_common_base_but_preserves_audit_and_blocks_reuse() {
    let directory = tempfile::tempdir().expect("create lifecycle fixture");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    let identity = SyncGenerationIdentity {
        generation_id: "generation-1".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "1".to_owned(),
    };
    store
        .register_verified_sync_device(
            VerifiedSyncDeviceRegistration::for_test("library", "device-a", identity.clone(), 10),
            0,
        )
        .expect("register device");
    store
        .revoke_sync_device("library", "device-a", &identity)
        .expect("revoke device");
    assert!(store
        .advance_sync_device_ack("library", "device-a", &identity, &identity)
        .is_err());
    let forgotten = store
        .forget_sync_device("library", "device-a", &identity)
        .expect("forget device");

    assert_eq!(forgotten.status, RegisteredSyncDeviceStatus::Forgotten);
    assert_eq!(forgotten.acknowledged_generation, identity);
    assert!(forgotten.revoked_at.is_some());
    assert!(forgotten.forgotten_at.is_some());
    assert!(store
        .advance_sync_device_ack("library", "device-a", &identity, &identity)
        .is_err());
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM logical_peer_common_bases
                 WHERE peer_id = 'device-a' AND library_id = 'library'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count removed common base"),
        0
    );
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('device-a', 'library', 'generation-1', ?1, '1', 30)",
            [HASH_A],
        )
        .expect("inject forgotten common base corruption");
    assert!(store
        .forget_sync_device("library", "device-a", &identity)
        .is_err());
    assert!(store
        .register_verified_sync_device(
            VerifiedSyncDeviceRegistration::for_test("library", "device-a", identity, 20),
            0,
        )
        .is_err());
}

#[test]
fn tombstone_pages_are_stable_exact_and_read_only() {
    let directory = tempfile::tempdir().expect("create paged plan fixture");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    seed_complete_generation(&store, "library", "generation-3", "3", HASH_A);
    store
        .connection
        .execute(
            "INSERT INTO logical_library_head (singleton, library_id, generation_id)
             VALUES (1, 'library', 'generation-3')",
            [],
        )
        .expect("seed generation head");
    for (record_key, deleted_sequence) in [("asset:a", "1"), ("asset:b", "2"), ("asset:c", "3")] {
        store
            .connection
            .execute(
                "INSERT INTO logical_record_heads (
                    library_id, generation_id, record_key, record_kind, state,
                    object_hash, object_size, deleted_generation_sequence
                 ) VALUES ('library', 'generation-3', ?1, 'asset', 'tombstone', NULL, 0, ?2)",
                params![record_key, deleted_sequence],
            )
            .expect("seed tombstone");
    }
    let first = SyncGenerationIdentity {
        generation_id: "generation-1".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "1".to_owned(),
    };
    let third = SyncGenerationIdentity {
        generation_id: "generation-3".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "3".to_owned(),
    };
    store
        .register_verified_sync_device(
            VerifiedSyncDeviceRegistration::for_test("library", "device-a", first.clone(), 10),
            0,
        )
        .expect("register device");
    store
        .advance_sync_device_ack("library", "device-a", &first, &third)
        .expect("advance acknowledgement");

    let before: (i64, i64, i64) = (
        store
            .connection
            .query_row("SELECT COUNT(*) FROM logical_record_heads", [], |row| {
                row.get(0)
            })
            .expect("count tombstones before plan"),
        store
            .connection
            .query_row("SELECT COUNT(*) FROM logical_sync_generations", [], |row| {
                row.get(0)
            })
            .expect("count generations before plan"),
        store
            .connection
            .query_row("SELECT COUNT(*) FROM asset_objects", [], |row| row.get(0))
            .expect("count catalog before plan"),
    );
    let first_page = store
        .plan_tombstone_collection(&third, None, 2)
        .expect("plan first page");
    assert!(store.plan_tombstone_collection(&third, None, 0).is_err());
    assert!(store
        .plan_tombstone_collection(&third, None, 4_097)
        .is_err());
    assert_eq!(
        first_page
            .items
            .iter()
            .map(|item| item.record_key.as_str())
            .collect::<Vec<_>>(),
        ["asset:a", "asset:b"]
    );
    assert!(first_page
        .items
        .iter()
        .all(|item| item.blocking_device_ids.is_empty()));
    let cursor = first_page.next_cursor.expect("first page cursor");
    let second_page = store
        .plan_tombstone_collection(&third, Some(&cursor), 2)
        .expect("plan second page");
    assert_eq!(second_page.items.len(), 1);
    assert_eq!(second_page.items[0].record_key, "asset:c");
    assert_eq!(second_page.items[0].blocking_device_ids, ["device-a"]);
    assert!(second_page.next_cursor.is_none());
    assert!(store
        .plan_tombstone_collection(&first, Some(&cursor), 2)
        .is_err());
    assert_eq!(
        (
            store
                .connection
                .query_row("SELECT COUNT(*) FROM logical_record_heads", [], |row| row
                    .get(0))
                .expect("count tombstones after plan"),
            store
                .connection
                .query_row("SELECT COUNT(*) FROM logical_sync_generations", [], |row| {
                    row.get(0)
                })
                .expect("count generations after plan"),
            store
                .connection
                .query_row("SELECT COUNT(*) FROM asset_objects", [], |row| row.get(0))
                .expect("count catalog after plan"),
        ),
        before
    );
}

#[test]
fn tombstone_plan_fails_closed_on_corrupt_forgotten_audit_rows() {
    let directory = tempfile::tempdir().expect("create corrupt registry fixture");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    seed_complete_generation(&store, "library", "generation-1", "1", HASH_A);
    store
        .connection
        .execute(
            "INSERT INTO logical_library_head (singleton, library_id, generation_id)
             VALUES (1, 'library', 'generation-1')",
            [],
        )
        .expect("seed generation head");
    let identity = SyncGenerationIdentity {
        generation_id: "generation-1".to_owned(),
        manifest_hash: HASH_A.to_owned(),
        generation_sequence: "1".to_owned(),
    };
    store
        .register_verified_sync_device(
            VerifiedSyncDeviceRegistration::for_test("library", "device-a", identity.clone(), 10),
            0,
        )
        .expect("register device");
    store
        .forget_sync_device("library", "device-a", &identity)
        .expect("forget device");
    store
        .connection
        .execute_batch(
            "PRAGMA ignore_check_constraints = ON;
             UPDATE logical_sync_devices
             SET forgotten_at = 9
             WHERE library_id = 'library' AND device_id = 'device-a';
             PRAGMA ignore_check_constraints = OFF;",
        )
        .expect("inject corrupt forgotten timestamp");
    assert!(store.plan_tombstone_collection(&identity, None, 1).is_err());
    store
        .connection
        .execute_batch(
            "PRAGMA ignore_check_constraints = ON;
             UPDATE logical_sync_devices
             SET forgotten_at = 10, acknowledged_manifest_hash = 'corrupt'
             WHERE library_id = 'library' AND device_id = 'device-a';
             PRAGMA ignore_check_constraints = OFF;",
        )
        .expect("inject corrupt forgotten audit row");

    assert!(store.plan_tombstone_collection(&identity, None, 1).is_err());
}
