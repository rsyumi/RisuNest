use super::super::server_sync_outbox as outbox;
use super::*;

fn bind(store: &PersistentStore) {
    store
        .connection
        .execute(
            "INSERT INTO server_sync_state(singleton,config,full_scan) VALUES(1,'{}',0)",
            [],
        )
        .unwrap();
}
#[test]
fn incremental_outbox_is_atomic_sparse_across_generation_copy_and_tail_ack() {
    let (_dir, mut store, _) = open_fixture();
    bind(&store);
    let lease = store.acquire_revision(1).unwrap();
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({"username":"synthetic changed"})),
            ..empty_working_set_commit(1)
        })
        .unwrap();
    let sent = outbox::dirty_page(&store.connection, None, 1024).unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].kind, "root");
    assert_eq!(sent[0].revision, 2);
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({"username":"synthetic newer"})),
            ..empty_working_set_commit(2)
        })
        .unwrap();
    let tx = store.connection.transaction().unwrap();
    outbox::acknowledge_keys(&tx, &sent).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        outbox::dirty_page(&store.connection, None, 1024).unwrap()[0].revision,
        3
    );
    store.connection.execute_batch("CREATE TRIGGER synthetic_commit_failure BEFORE UPDATE ON meta WHEN NEW.key='currentRevision' BEGIN SELECT RAISE(ABORT,'synthetic'); END").unwrap();
    assert!(store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "uncommitted".into(),
                value: json!(true)
            }]),
            ..empty_working_set_commit(3)
        })
        .is_err());
    assert_eq!(
        outbox::dirty_page(&store.connection, None, 1024)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>("SELECT count(*) FROM server_sync_context", [], |r| r.get(0))
            .unwrap(),
        0
    );
    store
        .connection
        .execute_batch("DROP TRIGGER synthetic_commit_failure")
        .unwrap();
    store.release_revision(&lease.lease).unwrap();
}

#[test]
fn plugin_clear_keeps_original_membership_and_scope_then_local_set_remains_dirty() {
    let (_dir, mut store, _) = open_fixture();
    bind(&store);
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![
                PluginStorageMutation::Set {
                    key: "first".into(),
                    value: json!(1),
                },
                PluginStorageMutation::Set {
                    key: "second".into(),
                    value: json!(2),
                },
            ]),
            ..empty_working_set_commit(1)
        })
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO server_sync_scope_base VALUES('plugin-storage',?1)",
            ["a".repeat(64)],
        )
        .unwrap();
    let original: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM plugin_storage WHERE generation=?1",
            [active_generation(&store.connection).unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![
                PluginStorageMutation::Clear,
                PluginStorageMutation::Set {
                    key: "later".into(),
                    value: json!(3),
                },
            ]),
            ..empty_working_set_commit(2)
        })
        .unwrap();
    let captured: i64 = store
        .connection
        .query_row("SELECT count(*) FROM server_sync_clear_members", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(captured, original);
    let version: String = store
        .connection
        .query_row("SELECT expected_version FROM server_sync_clears", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(version, "a".repeat(64));
    assert!(outbox::dirty_page(&store.connection, None, 1024)
        .unwrap()
        .iter()
        .any(|k| k.kind == "plugin" && k.key1 == "later"));
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM server_sync_clear_members WHERE key='later'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn replacement_restore_and_peer_activation_cannot_bypass_server_replica() {
    let (_dir, mut store, _) = open_fixture();
    bind(&store);
    let staged = stage_root(&mut store, "synthetic replacement");
    store.replace_commit(&staged, Some(1)).unwrap();
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>("SELECT full_scan FROM server_sync_state", [], |r| r.get(0))
            .unwrap(),
        1
    );
    assert!(outbox::require_peer_unbound(&store.connection).is_err());
    outbox::restored_copy(&store.connection).unwrap();
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT registration_required FROM server_sync_state",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        1
    );
}
