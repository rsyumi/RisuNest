use super::super::{
    content_change_index as changes, external_storage_state as external,
    sync_selection as selection,
};
use super::*;

fn count(store: &PersistentStore, table: &str) -> i64 {
    store
        .connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
fn edit(store: &mut PersistentStore, n: i64) {
    store
        .commit(&WorkingSetCommit {
            root_mutations: Some(vec![super::super::RootMutation::Set {
                key: "synthetic".into(),
                value: json!(n),
            }]),
            ..empty_working_set_commit(store.revision().unwrap())
        })
        .unwrap();
}
fn cursor(store: &mut PersistentStore, id: &str, revision: i64) {
    let tx = store.connection.transaction().unwrap();
    changes::commit_cursor(&tx, id, &active_generation(&tx).unwrap(), revision).unwrap();
    tx.commit().unwrap();
}

#[test]
fn external_changes_coalesce_and_rollback_without_server_binding() {
    let (_dir, mut store, _) = open_fixture();
    assert_eq!(count(&store, "content_changes"), 0);
    edit(&mut store, 1);
    edit(&mut store, 2);
    assert_eq!(count(&store, "content_changes"), 1);
    assert_eq!(count(&store, "server_sync_dirty"), 0);
    let revision = store.revision().unwrap();
    store.connection.execute_batch("CREATE TRIGGER synthetic_failure BEFORE UPDATE ON meta WHEN NEW.key='currentRevision' BEGIN SELECT RAISE(ABORT,'synthetic'); END").unwrap();
    assert!(store
        .commit(&WorkingSetCommit {
            root: Some(json!({"synthetic":3})),
            ..empty_working_set_commit(revision)
        })
        .is_err());
    assert_eq!(store.revision().unwrap(), revision);
    let tracked: i64 = store
        .connection
        .query_row("SELECT revision FROM content_changes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tracked, revision);
    assert_eq!(count(&store, "content_change_context"), 0);
}

#[test]
fn external_cursor_and_rows_share_snapshot_and_preserve_r_plus_one() {
    let (_dir, mut store, _) = open_fixture();
    cursor(&mut store, "backup", 1);
    edit(&mut store, 1);
    let lease_id = store.acquire_revision(2).unwrap().lease;
    edit(&mut store, 2);
    let lease = store.revision_leases.get(&lease_id).unwrap();
    assert_eq!(
        changes::window(lease, "backup").unwrap(),
        changes::ChangeWindow::Incremental { after_revision: 1 }
    );
    let page = changes::page(lease, 1, None, 128).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].kind, "root");
    assert_eq!(
        store.read_root(Some(&lease_id)).unwrap().value["synthetic"],
        json!(1)
    );
    cursor(&mut store, "backup", 2);
    store.release_revision(&lease_id).unwrap();
    let lease_id = store.acquire_revision(3).unwrap().lease;
    assert_eq!(
        changes::page(store.revision_leases.get(&lease_id).unwrap(), 2, None, 128)
            .unwrap()
            .len(),
        1
    );
    store.release_revision(&lease_id).unwrap();
}

#[test]
fn external_delete_recreate_projects_final_state_and_messages_dirty_conversation() {
    let (_dir, mut store, _) = open_fixture();
    let generation = active_generation(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    changes::begin_mutation(&tx, &generation, 2, "local").unwrap();
    tx.execute(
        "INSERT INTO plugin_storage VALUES(?1,'synthetic',1,0,'1')",
        [&generation],
    )
    .unwrap();
    tx.execute(
        "DELETE FROM plugin_storage WHERE storage_key='synthetic'",
        [],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO plugin_storage VALUES(?1,'synthetic',1,0,'2')",
        [&generation],
    )
    .unwrap();
    tx.execute(
        "UPDATE messages SET message_id=NULL WHERE generation=?1",
        [&generation],
    )
    .unwrap();
    changes::finish_mutation(&tx).unwrap();
    super::super::commit::set_active(&tx, 2, &generation).unwrap();
    tx.commit().unwrap();
    let lease_id = store.acquire_revision(2).unwrap().lease;
    let lease = store.revision_leases.get(&lease_id).unwrap();
    let page = changes::page(lease, 1, None, 128).unwrap();
    assert!(page.iter().any(|k| k.kind == "conversation"));
    assert_eq!(page.iter().filter(|k| k.kind == "plugin").count(), 1);
    let value: String = lease
        .connection
        .query_row(
            "SELECT value FROM plugin_storage WHERE storage_key='synthetic'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(value, "2");
    store.release_revision(&lease_id).unwrap();
}

#[test]
fn external_floor_respects_each_consumer_and_capture_reservations() {
    let (_dir, mut store, _) = open_fixture();
    cursor(&mut store, "slow", 1);
    edit(&mut store, 1);
    cursor(&mut store, "fast", 2);
    let tx = store.connection.transaction().unwrap();
    changes::reserve(&tx, "capture", &active_generation(&tx).unwrap(), 1).unwrap();
    assert_eq!(changes::prune(&tx).unwrap(), 1);
    changes::require_rebuild(&tx, "slow").unwrap();
    assert_eq!(changes::prune(&tx).unwrap(), 1);
    tx.execute(
        "DELETE FROM content_capture_reservations WHERE id='capture'",
        [],
    )
    .unwrap();
    assert_eq!(changes::prune(&tx).unwrap(), 2);
    tx.commit().unwrap();
    let lease_id = store.acquire_revision(2).unwrap().lease;
    let lease = store.revision_leases.get(&lease_id).unwrap();
    assert_eq!(
        changes::window(lease, "slow").unwrap(),
        changes::ChangeWindow::Rebuild
    );
    assert!(changes::page(lease, 1, None, 128).is_err());
    store.release_revision(&lease_id).unwrap();
}

fn select_external(store: &mut PersistentStore) {
    let epoch = selection::read(&store.connection).unwrap().epoch;
    let tx = store.connection.transaction().unwrap();
    selection::select(
        &tx,
        &epoch,
        &selection::SyncTarget::External("synthetic-connection".into()),
    )
    .unwrap();
    tx.commit().unwrap();
}
fn capture(store: &mut PersistentStore, job: &str) -> selection::CaptureIdentity {
    let identity = selection::identity(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    let id = external::register_capture(
        &tx,
        "synthetic-capture",
        &identity,
        "library",
        "codec-1",
        "",
        &"a".repeat(64),
        "synthetic-consumer",
    )
    .unwrap();
    external::prepare_publication(
        &tx,
        &external::PublishIntent {
            job_id: job,
            connection_id: "synthetic-connection",
            repository_id: "synthetic-repository",
            capture_id: &id,
            identity: &identity,
            strategy: "sequential",
            expected_head: None,
            commit_id: "synthetic-commit",
        },
    )
    .unwrap();
    tx.commit().unwrap();
    identity
}

#[test]
fn paused_target_allows_only_live_exit_drain_publication_paths() {
    let (_dir, mut store, _) = open_fixture();
    select_external(&mut store);
    let identity = selection::identity(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    let capture_id = external::register_capture(
        &tx,
        "exit-capture",
        &identity,
        "library",
        "codec-1",
        "",
        &"b".repeat(64),
        "exit-consumer",
    )
    .unwrap();
    tx.commit().unwrap();
    store
        .connection
        .execute(
            "UPDATE library_sync_selection SET paused=1 WHERE singleton=1",
            [],
        )
        .unwrap();
    let intent = external::PublishIntent {
        job_id: "exit-job",
        connection_id: "synthetic-connection",
        repository_id: "synthetic-repository",
        capture_id: &capture_id,
        identity: &identity,
        strategy: "cas",
        expected_head: None,
        commit_id: "exit-commit",
    };
    let tx = store.connection.transaction().unwrap();
    assert!(external::prepare_publication(&tx, &intent).is_err());
    external::prepare_publication_exit_drain(&tx, &intent).unwrap();
    tx.commit().unwrap();
    let tx = store.connection.transaction().unwrap();
    assert!(external::begin_publication(&tx, "exit-job").is_err());
    external::begin_publication_exit_drain(&tx, "exit-job").unwrap();
    assert!(external::begin_publication_exit_drain(&tx, "exit-job").is_err());
    external::publication_unknown(&tx, "exit-job").unwrap();
    assert!(external::confirm_publication(
        &tx,
        "exit-job",
        "exit-commit",
        "snapshot",
        "observation",
        false
    )
    .is_err());
    external::confirm_publication_exit_drain(
        &tx,
        "exit-job",
        "exit-commit",
        "snapshot",
        "observation",
        false,
    )
    .unwrap();
    tx.commit().unwrap();
    assert!(selection::read(&store.connection).unwrap().paused);
}

#[test]
fn equivalent_remote_advances_only_the_retained_capture_revision() {
    let (_dir, mut store, _) = open_fixture();
    select_external(&mut store);
    let retained = selection::identity(&store.connection).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO external_storage_bases VALUES(?1,?2,?3,?4,?5,?6)",
            rusqlite::params![
                "synthetic-connection",
                "synthetic-repository",
                "old-snapshot",
                "old-commit",
                "old-observation",
                serde_json::to_string(&retained).unwrap()
            ],
        )
        .unwrap();
    edit(&mut store, 10);
    let current = selection::identity(&store.connection).unwrap();
    assert!(current.revision > retained.revision);
    store
        .external_accept_equivalent(
            "synthetic-connection",
            "synthetic-repository",
            "old-commit",
            "old-observation",
            "remote-snapshot",
            "remote-commit",
            "remote-observation",
            &retained,
        )
        .unwrap();
    let base = store
        .external_base("synthetic-connection")
        .unwrap()
        .unwrap();
    assert_eq!(base.identity, retained);
    assert_eq!(base.commit_id, "remote-commit");
    assert_eq!(selection::identity(&store.connection).unwrap(), current);
    assert_ne!(base.identity.revision, current.revision);
}

#[test]
fn preserved_conflict_pins_capture_and_round_trips_remote_revision() {
    let (_dir, mut store, _) = open_fixture();
    select_external(&mut store);
    let identity = capture(&mut store, "conflict-job");
    let record = super::super::external_conflicts::ConflictRecord {
        id: "conflict-job".into(),
        connection_id: "synthetic-connection".into(),
        repository_id: "synthetic-repository".into(),
        local_capture_id: "synthetic-capture".into(),
        local_snapshot: Some("{\"side\":\"local\"}".into()),
        local_identity: identity.clone(),
        remote_snapshot: Some("{\"side\":\"remote\"}".into()),
        remote_logical_revision: Some(17),
        remote_commit_id: Some("remote-commit".into()),
        remote_head_observation: Some("remote-observation".into()),
        created_at_ms: 1,
        preservation: super::super::external_conflicts::ConflictPreservation::RemoteComplete,
        phase: super::super::external_conflicts::ConflictPhase::Pending,
    };
    store.external_record_conflict(&record).unwrap();
    let loaded = store.external_conflict("conflict-job").unwrap().unwrap();
    assert_eq!(loaded.remote_logical_revision, Some(17));
    assert_eq!(loaded.local_identity, identity);
    assert_eq!(count(&store, "external_storage_capture_refs"), 1);
    assert!(store.external_cancel_prepared("conflict-job").is_err());
    assert_eq!(
        store.external_jobs("synthetic-connection").unwrap()[0].phase,
        "stale"
    );
    let resolving = store
        .external_begin_conflict_resolution("conflict-job", "remote-observation")
        .unwrap();
    assert_eq!(
        resolving.phase,
        super::super::external_conflicts::ConflictPhase::Resolving
    );
    store
        .external_prepare_conflict_publication(&external::PublishIntent {
            job_id: "conflict-job",
            connection_id: "synthetic-connection",
            repository_id: "synthetic-repository",
            capture_id: "synthetic-capture",
            identity: &identity,
            strategy: "sequential",
            expected_head: Some("remote-observation"),
            commit_id: "resolved-local-commit",
        })
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE external_storage_jobs SET phase='complete' WHERE id='conflict-job'",
            [],
        )
        .unwrap();
    store.external_finish_conflict("conflict-job").unwrap();
    assert_eq!(
        store.external_conflict("conflict-job").unwrap().unwrap().phase,
        super::super::external_conflicts::ConflictPhase::Resolved
    );
    assert_eq!(count(&store, "external_storage_capture_refs"), 0);
}

#[test]
fn local_only_conflict_keeps_its_capture_until_remote_preservation_completes() {
    let (_dir, mut store, _) = open_fixture();
    select_external(&mut store);
    let identity = capture(&mut store, "conflict-local-only");
    let local = super::super::external_conflicts::ConflictRecord {
        id: "conflict-local-only".into(),
        connection_id: "synthetic-connection".into(),
        repository_id: "synthetic-repository".into(),
        local_capture_id: "synthetic-capture".into(),
        local_snapshot: None,
        local_identity: identity.clone(),
        remote_snapshot: Some("{\"side\":\"remote\"}".into()),
        remote_logical_revision: None,
        remote_commit_id: Some("remote-commit".into()),
        remote_head_observation: Some("remote-observation".into()),
        created_at_ms: 1,
        preservation: super::super::external_conflicts::ConflictPreservation::LocalOnly,
        phase: super::super::external_conflicts::ConflictPhase::Pending,
    };
    store.external_record_local_conflict(&local).unwrap();
    assert!(store
        .external_conflict("conflict-local-only")
        .unwrap()
        .unwrap()
        .local_snapshot
        .is_none());
    assert_eq!(
        store.external_jobs("synthetic-connection").unwrap()[0].phase,
        "conflictPreserving"
    );
    assert_eq!(count(&store, "external_storage_capture_refs"), 1);
    assert!(store.external_cancel_prepared("conflict-local-only").is_err());
    let local = store
        .external_bind_local_conflict_snapshot(
            "conflict-local-only",
            "{\"side\":\"local\"}",
        )
        .unwrap();
    assert_eq!(
        local.local_snapshot.as_deref(),
        Some("{\"side\":\"local\"}")
    );
    assert!(store
        .external_complete_conflict_preservation(
            "conflict-local-only",
            "{\"side\":\"other-remote\"}",
            17,
            "remote-commit",
            "remote-observation",
        )
        .is_err());
    assert_eq!(
        store
            .external_conflict("conflict-local-only")
            .unwrap()
            .unwrap()
            .preservation,
        super::super::external_conflicts::ConflictPreservation::LocalOnly
    );
    assert_eq!(count(&store, "external_storage_capture_refs"), 1);

    let completed = store
        .external_complete_conflict_preservation(
            "conflict-local-only",
            "{\"side\":\"remote\"}",
            17,
            "remote-commit",
            "remote-observation",
        )
        .unwrap();
    assert_eq!(
        completed.preservation,
        super::super::external_conflicts::ConflictPreservation::RemoteComplete
    );
    assert_eq!(completed.remote_logical_revision, Some(17));
    assert_eq!(
        store.external_jobs("synthetic-connection").unwrap()[0].phase,
        "stale"
    );
    assert_eq!(count(&store, "external_storage_capture_refs"), 1);
    store
        .external_begin_conflict_resolution("conflict-local-only", "remote-observation")
        .unwrap();
    store
        .external_reject_conflict_resolution("conflict-local-only")
        .unwrap();
    assert_eq!(
        store
            .external_conflict("conflict-local-only")
            .unwrap()
            .unwrap()
            .phase,
        super::super::external_conflicts::ConflictPhase::Pending
    );
    assert_eq!(
        store.external_jobs("synthetic-connection").unwrap()[0].phase,
        "cancelled"
    );
    assert_eq!(count(&store, "external_storage_capture_refs"), 1);
    store
        .external_begin_conflict_resolution("conflict-local-only", "remote-observation")
        .unwrap();
    store
        .external_prepare_conflict_receive(&external::ReceiveIntent {
            job_id: "conflict-local-only",
            connection_id: "synthetic-connection",
            repository_id: "synthetic-repository",
            snapshot_id: "remote-snapshot",
            commit_id: "remote-commit",
            authenticated_head: "remote-observation",
            identity: &identity,
        })
        .unwrap();
    let resumed = store.external_jobs("synthetic-connection").unwrap();
    assert_eq!(resumed[0].phase, "ready");
    assert_eq!(resumed[0].role, "restore");
    assert_eq!(count(&store, "external_storage_capture_refs"), 1);
    store
        .connection
        .execute(
            "UPDATE external_storage_jobs SET phase='complete' WHERE id='conflict-local-only'",
            [],
        )
        .unwrap();
    store
        .external_finish_conflict("conflict-local-only")
        .unwrap();
    assert_eq!(
        store
            .external_conflict("conflict-local-only")
            .unwrap()
            .unwrap()
            .phase,
        super::super::external_conflicts::ConflictPhase::Resolved
    );
    assert_eq!(count(&store, "external_storage_capture_refs"), 0);
}

#[test]
fn definite_head_rejection_is_preserved_when_pause_arrives_after_the_response() {
    let (_dir, mut store, _) = open_fixture();
    select_external(&mut store);
    let identity = capture(&mut store, "paused-conflict");
    store.external_begin_publication("paused-conflict").unwrap();
    store
        .connection
        .execute(
            "UPDATE library_sync_selection SET paused=1 WHERE singleton=1",
            [],
        )
        .unwrap();
    store
        .external_record_local_conflict_after_rejection(
            &super::super::external_conflicts::ConflictRecord {
                id: "paused-conflict".into(),
                connection_id: "synthetic-connection".into(),
                repository_id: "synthetic-repository".into(),
                local_capture_id: "synthetic-capture".into(),
                local_snapshot: Some("{\"side\":\"local\"}".into()),
                local_identity: identity,
                remote_snapshot: None,
                remote_logical_revision: None,
                remote_commit_id: None,
                remote_head_observation: None,
                created_at_ms: 1,
                preservation:
                    super::super::external_conflicts::ConflictPreservation::LocalOnly,
                phase: super::super::external_conflicts::ConflictPhase::Pending,
            },
        )
        .unwrap();
    assert!(selection::read(&store.connection).unwrap().paused);
    assert_eq!(
        store.external_jobs("synthetic-connection").unwrap()[0].phase,
        "conflictPreserving"
    );
    assert_eq!(count(&store, "external_storage_capture_refs"), 1);
    assert!(store.external_cancel_prepared("paused-conflict").is_err());
}

#[test]
fn only_an_untouched_empty_library_is_a_pristine_first_attach_target() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    assert!(store.external_library_is_pristine().unwrap());
    edit(&mut store, 1);
    assert!(!store.external_library_is_pristine().unwrap());
}

#[test]
fn external_intent_prevents_blind_retries_and_blocks_replacement_until_settlement() {
    let (_dir, mut store, _) = open_fixture();
    select_external(&mut store);
    let identity = capture(&mut store, "job");
    let tx = store.connection.transaction().unwrap();
    external::begin_publication(&tx, "job").unwrap();
    external::publication_unknown(&tx, "job").unwrap();
    assert!(external::begin_publication(&tx, "job").is_err());
    assert!(selection::require_no_pending_publication(&tx).is_err());
    tx.commit().unwrap();
    edit(&mut store, 1);
    let tx = store.connection.transaction().unwrap();
    assert!(external::confirm_publication(
        &tx,
        "job",
        "wrong-commit",
        "snapshot",
        "observation",
        false
    )
    .is_err());
    external::confirm_publication(
        &tx,
        "job",
        "synthetic-commit",
        "snapshot",
        "observation",
        true,
    )
    .unwrap();
    tx.commit().unwrap();
    let base: String = store
        .connection
        .query_row("SELECT identity FROM external_storage_bases", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<selection::CaptureIdentity>(&base).unwrap(),
        identity
    );
    assert_eq!(store.revision().unwrap(), identity.revision + 1);
    assert_eq!(count(&store, "external_storage_capture_refs"), 1);
    let tx = store.connection.transaction().unwrap();
    assert!(external::begin_publication(&tx, "job").is_err());
    external::finish_history(&tx, "job").unwrap();
    tx.commit().unwrap();
    assert_eq!(count(&store, "external_storage_capture_refs"), 0);
}

#[test]
fn external_base_and_job_phase_commit_atomically() {
    let (dir, mut store, _) = open_fixture();
    select_external(&mut store);
    capture(&mut store, "job");
    let tx = store.connection.transaction().unwrap();
    external::begin_publication(&tx, "job").unwrap();
    tx.commit().unwrap();
    let tx = store.connection.transaction().unwrap();
    external::confirm_publication(
        &tx,
        "job",
        "synthetic-commit",
        "snapshot",
        "observation",
        false,
    )
    .unwrap();
    tx.rollback().unwrap();
    drop(store);
    let mut store = PersistentStore::open(dir.path()).unwrap();
    assert_eq!(count(&store, "external_storage_bases"), 0);
    assert!(selection::require_no_pending_publication(&store.connection).is_err());
    let tx = store.connection.transaction().unwrap();
    external::confirm_publication(
        &tx,
        "job",
        "synthetic-commit",
        "snapshot",
        "observation",
        false,
    )
    .unwrap();
    tx.commit().unwrap();
    drop(store);
    let store = PersistentStore::open(dir.path()).unwrap();
    assert_eq!(count(&store, "external_storage_bases"), 1);
    assert!(selection::require_no_pending_publication(&store.connection).is_ok());
}

#[test]
fn external_replacement_and_physical_copy_invalidate_old_identity() {
    let (_dir, mut store, _) = open_fixture();
    select_external(&mut store);
    let old = capture(&mut store, "job");
    let staging = stage_root(&mut store, "synthetic-replacement");
    store.replace_commit(&staging, Some(old.revision)).unwrap();
    let next = selection::identity(&store.connection).unwrap();
    assert_ne!(old.library_epoch, next.library_epoch);
    assert_ne!(old.generation, next.generation);
    assert!(
        selection::read(&store.connection)
            .unwrap()
            .decision_required
    );
    assert!(selection::require_publish(&store.connection, &old, "synthetic-connection").is_err());
    let tx = store.connection.transaction().unwrap();
    selection::restored_copy(&tx).unwrap();
    tx.commit().unwrap();
    let restored = selection::identity(&store.connection).unwrap();
    assert_ne!(restored.store_id, next.store_id);
    assert_ne!(restored.library_epoch, next.library_epoch);
    assert_ne!(restored.selection_epoch, next.selection_epoch);
}

#[test]
fn external_schema_rejects_missing_trigger_without_migration() {
    let (dir, store, _) = open_fixture();
    store
        .connection
        .execute_batch("DROP TRIGGER content_change_messages_update")
        .unwrap();
    drop(store);
    assert!(PersistentStore::open(dir.path()).is_err());
}

fn receive(store: &mut PersistentStore) -> selection::CaptureIdentity {
    let identity = selection::identity(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    external::prepare_receive(
        &tx,
        &external::ReceiveIntent {
            job_id: "receive",
            connection_id: "synthetic-connection",
            repository_id: "repository",
            snapshot_id: "remote-snapshot",
            commit_id: "remote-commit",
            authenticated_head: "verified-head",
            identity: &identity,
        },
    )
    .unwrap();
    tx.commit().unwrap();
    identity
}

#[test]
fn external_receive_activation_base_and_phase_rollback_and_reopen_together() {
    let (dir, mut store, _) = open_fixture();
    select_external(&mut store);
    let before = receive(&mut store);
    let stage = stage_root(&mut store, "synthetic-remote");
    let prepared = store
        .prepare_replace_commit(&stage, Some(before.revision))
        .unwrap();
    // Fail after the generation switch would have happened, during base write.
    store.connection.execute_batch("CREATE TRIGGER synthetic_base_failure BEFORE INSERT ON external_storage_bases BEGIN SELECT RAISE(ABORT,'synthetic'); END").unwrap();
    assert!(store.finish_external_receive(prepared, "receive").is_err());
    assert_eq!(selection::identity(&store.connection).unwrap(), before);
    assert_eq!(count(&store, "external_storage_bases"), 0);
    let phase: String = store
        .connection
        .query_row(
            "SELECT phase FROM external_storage_jobs WHERE id='receive'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(phase, "ready");
    store
        .connection
        .execute_batch("DROP TRIGGER synthetic_base_failure")
        .unwrap();
    drop(store);
    let mut store = PersistentStore::open(dir.path()).unwrap();
    assert_eq!(selection::identity(&store.connection).unwrap(), before);
    let phase: String = store
        .connection
        .query_row(
            "SELECT phase FROM external_storage_jobs WHERE id='receive'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(phase, "ready");
    // PDS intentionally removes abandoned staging on open. The retained verified
    // download is staged again by the receive coordinator before another apply.
    let stage = stage_root(&mut store, "synthetic-remote");
    let prepared = store
        .prepare_replace_commit(&stage, Some(before.revision))
        .unwrap();
    store.finish_external_receive(prepared, "receive").unwrap();
    drop(store);
    let store = PersistentStore::open(dir.path()).unwrap();
    let after = selection::identity(&store.connection).unwrap();
    assert_eq!(after.revision, before.revision + 1);
    assert_eq!(after.library_epoch, before.library_epoch);
    assert_ne!(after.generation, before.generation);
    assert!(
        !selection::read(&store.connection)
            .unwrap()
            .decision_required
    );
    let (base, snapshot, phase): (String,String,String) = store.connection.query_row(
        "SELECT identity,snapshot_id,(SELECT phase FROM external_storage_jobs WHERE id='receive') FROM external_storage_bases",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).unwrap();
    assert_eq!(
        serde_json::from_str::<selection::CaptureIdentity>(&base).unwrap(),
        after
    );
    assert_eq!(snapshot, "remote-snapshot");
    assert_eq!(phase, "complete");
}

#[test]
fn external_receive_rechecks_revision_and_target_before_activation() {
    for change_target in [false, true] {
        let (_dir, mut store, _) = open_fixture();
        select_external(&mut store);
        let before = receive(&mut store);
        let stage = stage_root(&mut store, "synthetic-remote");
        let prepared = store
            .prepare_replace_commit(&stage, Some(before.revision))
            .unwrap();
        if change_target {
            let tx = store.connection.transaction().unwrap();
            selection::select(&tx, &before.selection_epoch, &selection::SyncTarget::None).unwrap();
            tx.commit().unwrap();
        } else {
            edit(&mut store, 42);
        }
        let current = selection::identity(&store.connection).unwrap();
        assert!(store.finish_external_receive(prepared, "receive").is_err());
        assert_eq!(selection::identity(&store.connection).unwrap(), current);
        assert_eq!(count(&store, "external_storage_bases"), 0);
    }
}

fn capture_fixture() -> (tempfile::TempDir, PersistentStore, Value) {
    let (directory, store, database) = open_fixture();
    let generation = active_generation(&store.connection).unwrap();
    // This synthetic library has a complete empty alias inventory. Its missing
    // fixture images are explicitly absent, not undiscovered legacy files.
    let authority=json!({"format":"v2","migrationId":"synthetic-external","compatibilityHash":"a".repeat(64)}).to_string();
    for table in ["asset_repository_authority", "cold_payload_authority"] {
        store
            .connection
            .execute(
                &format!("UPDATE {table} SET value=?2 WHERE generation=?1"),
                params![generation, authority],
            )
            .unwrap();
    }
    (directory, store, database)
}

fn capture_library(
    store: &mut PersistentStore,
    consumer: &str,
    scope: &risunest_external_storage_format::format::Scope,
    probe: &dyn crate::local_backup::CancellationProbe,
) -> super::super::StoreResult<super::super::external_capture::CapturedSnapshot> {
    let hydration = store.hydrate_external_capture_dependencies(consumer, scope, probe)?;
    store.capture_external_library(consumer, scope, &hydration, probe)
}

#[test]
fn external_capture_manager_shares_consumers_and_reuses_their_durable_cursor() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (_directory, mut store, _) = capture_fixture();
    let scope = risunest_external_storage_format::format::Scope {
        library: true,
        referenced_assets: true,
        device_settings: false,
        device_plugins: false,
    };
    let first = capture_library(&mut store, "destination-a", &scope, &Never).unwrap();
    assert!(first.projected_records > 1);
    assert!(!first.shared);
    store
        .retain_external_capture(&first.id, "destination-a")
        .unwrap();
    let second = capture_library(&mut store, "destination-b", &scope, &Never).unwrap();
    assert!(second.shared);
    assert_eq!(second.projected_records, 0);
    assert_eq!(first.id, second.id);
    store
        .retain_external_capture(&second.id, "destination-b")
        .unwrap();
    assert!(!store
        .release_external_capture(&first.id, "destination-a")
        .unwrap());
    assert!(external::capture_has_consumers(&store.connection, &first.id).unwrap());
    edit(&mut store, 2);
    let next = capture_library(&mut store, "destination-b", &scope, &Never).unwrap();
    assert_eq!(next.projected_records, 1);
    assert!(!next.catalog.rebuilt);
    assert_ne!(next.id, first.id);
    assert!(store
        .release_external_capture(&first.id, "destination-b")
        .unwrap());
    assert_eq!(count(&store, "content_capture_reservations"), 0);
}

#[test]
fn external_capture_rebuilds_an_unreferenced_missing_cache_and_collects_old_metadata() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (_directory, mut store, _) = capture_fixture();
    let scope = risunest_external_storage_format::format::Scope {
        library: true,
        referenced_assets: true,
        device_settings: false,
        device_plugins: false,
    };
    let first = capture_library(&mut store, "destination", &scope, &Never).unwrap();
    let first_id = first.id.clone();
    let (_, first_path, _) = first.catalog.manifest().unwrap();
    let first_path = first_path.to_owned();
    drop(first);
    fs::remove_file(first_path).unwrap();

    let rebuilt = capture_library(&mut store, "destination", &scope, &Never).unwrap();
    assert!(!rebuilt.shared);
    assert_ne!(rebuilt.id, first_id);
    assert_eq!(count(&store, "external_storage_captures"), 1);
    assert_eq!(count(&store, "external_storage_capture_files"), 1);
}

#[test]
fn external_capture_rebuilds_an_unreferenced_corrupt_cache() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (_directory, mut store, _) = capture_fixture();
    let scope = risunest_external_storage_format::format::Scope {
        library: true,
        referenced_assets: true,
        device_settings: false,
        device_plugins: false,
    };
    let first = capture_library(&mut store, "destination", &scope, &Never).unwrap();
    let first_id = first.id.clone();
    let (_, path, _) = first.catalog.manifest().unwrap();
    let path = path.to_owned();
    drop(first);
    fs::write(path, b"synthetic-corrupt-capture-cache").unwrap();

    let rebuilt = capture_library(&mut store, "destination", &scope, &Never).unwrap();
    assert!(!rebuilt.shared);
    assert_ne!(rebuilt.id, first_id);
    assert_eq!(count(&store, "external_storage_captures"), 1);
}

#[test]
fn external_capture_never_discards_a_missing_cache_owned_by_a_pending_job() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (_directory, mut store, _) = capture_fixture();
    let scope = risunest_external_storage_format::format::Scope {
        library: true,
        referenced_assets: true,
        device_settings: false,
        device_plugins: false,
    };
    let first = capture_library(&mut store, "destination", &scope, &Never).unwrap();
    let first_id = first.id.clone();
    let (_, path, _) = first.catalog.manifest().unwrap();
    let path = path.to_owned();
    let tx = store.connection.transaction().unwrap();
    external::prepare_backup(
        &tx,
        "pending-backup",
        "destination",
        "repository",
        &first_id,
        "history-point",
    )
    .unwrap();
    tx.commit().unwrap();
    drop(first);
    fs::remove_file(path).unwrap();

    let error = match capture_library(&mut store, "other-destination", &scope, &Never) {
        Ok(_) => panic!("referenced damaged cache was reused"),
        Err(error) => error.to_string(),
    };
    assert_eq!(error, "Referenced capture cache is unavailable");
    assert_eq!(count(&store, "external_storage_captures"), 1);
    assert_eq!(count(&store, "external_storage_capture_refs"), 1);
    let phase: String = store
        .connection
        .query_row(
            "SELECT phase FROM external_storage_jobs WHERE id='pending-backup'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(phase, "ready");
}

#[test]
fn external_capture_reopens_a_pinned_old_revision_and_gc_keeps_only_needed_cache() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (_directory, mut store, _) = capture_fixture();
    let scope = risunest_external_storage_format::format::Scope {
        library: true,
        referenced_assets: true,
        device_settings: false,
        device_plugins: false,
    };
    let first = capture_library(&mut store, "destination", &scope, &Never).unwrap();
    let first_id = first.id.clone();
    let first_revision = first.identity.revision;
    drop(first);
    store
        .retain_external_capture(&first_id, "pending-owner")
        .unwrap();
    edit(&mut store, 2);
    let second = capture_library(&mut store, "destination", &scope, &Never).unwrap();
    assert_ne!(second.id, first_id);
    assert_eq!(count(&store, "external_storage_captures"), 2);
    let reopened = store.reopen_external_capture(&first_id).unwrap();
    assert_eq!(reopened.identity.revision, first_revision);
    drop(reopened);
    drop(second);

    store
        .release_external_capture(&first_id, "pending-owner")
        .unwrap();
    edit(&mut store, 3);
    let third = capture_library(&mut store, "destination", &scope, &Never).unwrap();
    assert_eq!(count(&store, "external_storage_captures"), 1);
    assert_eq!(count(&store, "external_storage_capture_files"), 1);
    assert!(store.reopen_external_capture(&third.id).is_err());
}

#[test]
fn external_capture_text_delta_does_not_hydrate_unchanged_remote_only_payloads() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let payload = cas.prepare_bytes(b"unchanged-remote-only-payload").unwrap();
    store
        .commit_asset_alias(
            &AssetAlias {
                key: "assets/remote-only.bin".into(),
                object_hash: Some(payload.content_hash.clone()),
                kind: "asset".into(),
                size: payload.byte_size as i64,
                mime: "application/octet-stream".into(),
                name: "remote-only".into(),
                ext: "bin".into(),
                inlay_type: None,
                width: None,
                height: None,
                metadata: json!({}),
            },
            1,
        )
        .unwrap();
    let scope = risunest_external_storage_format::format::Scope {
        library: true,
        referenced_assets: true,
        device_settings: false,
        device_plugins: false,
    };
    let first = capture_library(&mut store, "destination", &scope, &Never).unwrap();
    drop(first);
    crate::server_sync::residency::Residency::open(directory.path()).unwrap();
    fs::remove_file(
        cas.object_path(&payload.content_hash)
            .unwrap()
            .expect("payload path"),
    )
    .unwrap();
    let revision = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "synthetic-text-setting".into(),
                value: json!("edited"),
            }]),
            ..empty_working_set_commit(revision)
        })
        .unwrap();

    let delta = capture_library(&mut store, "destination", &scope, &Never).unwrap();
    assert_eq!(delta.projected_records, 1);
    assert!(!delta.catalog.rebuilt);
    assert!(cas.stat_object(&payload.content_hash).unwrap().is_none());
}

#[test]
fn external_capture_manager_never_reuses_library_revision_for_device_scope() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (_directory, mut store, _) = capture_fixture();
    let scope = risunest_external_storage_format::format::Scope {
        library: true,
        referenced_assets: true,
        device_settings: true,
        device_plugins: false,
    };
    assert!(capture_library(&mut store, "destination", &scope, &Never).is_err());
    assert_eq!(count(&store, "external_storage_captures"), 0);
}

#[test]
fn external_capture_refuses_to_omit_unresolved_legacy_asset_storage() {
    use crate::external_storage::capture::CaptureCatalog;
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = open_fixture();
    let mut catalog = CaptureCatalog::create(
        &directory.path().join("capture"),
        &directory.path().join("objects"),
        None,
    )
    .unwrap();
    let prepared = store
        .prepare_content_capture("legacy", "consumer", 1)
        .unwrap();
    assert_eq!(
        prepared
            .project(&mut catalog, &Never)
            .unwrap_err()
            .to_string(),
        "Canonical asset authority required before external capture"
    );
    assert!(catalog.manifest().is_err());
    assert_eq!(count(&store, "external_storage_captures"), 0);
}

#[test]
fn external_capture_projects_only_changed_records_at_the_reserved_snapshot() {
    use super::super::content_capture::ContentCaptureSink;
    use crate::external_storage::capture::CaptureCatalog;
    use crate::logical_records::{decode_logical_record, LogicalRecordEnvelope};
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let objects = directory.path().join("external-storage/objects");
    let mut full = CaptureCatalog::create(
        &directory.path().join("external-storage/full"),
        &objects,
        None,
    )
    .unwrap();
    let prepared = store
        .prepare_content_capture("full", "consumer", 1)
        .unwrap();
    assert!(store.active_readers.detached_asset_roots().is_err());
    assert!(prepared.project(&mut full, &Never).unwrap() > 1);
    assert!(full.remove_record("root").is_err());
    prepared
        .register(&mut store, &full, &[1; 32], "logical-v1")
        .unwrap();
    assert!(count(&store, "external_storage_content_cache") > 1);
    assert!(store.active_readers.detached_asset_roots().is_ok());
    let (digest, path, _) = full.manifest().unwrap();
    let mut delta = CaptureCatalog::create(
        &directory.path().join("external-storage/delta"),
        &objects,
        Some((path, &digest)),
    )
    .unwrap();
    edit(&mut store, 2);
    let prepared = store
        .prepare_content_capture("delta", "consumer", 2)
        .unwrap();
    edit(&mut store, 3);
    assert_eq!(prepared.project(&mut delta, &Never).unwrap(), 1);
    assert!(!delta.rebuilt);
    let changed: i64 = delta
        .db
        .query_row("SELECT count(*) FROM delta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(changed, 1);
    let keys: Vec<(String, String)> = delta
        .db
        .prepare("SELECT key,hash FROM records ORDER BY key")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut saw_root = false;
    for (_, hash) in keys {
        let bytes = fs::read(objects.join(hash)).unwrap();
        if let LogicalRecordEnvelope::Root { value, .. } = decode_logical_record(&bytes).unwrap() {
            assert_eq!(value["synthetic"], json!(2));
            saw_root = true;
        }
    }
    assert!(saw_root);
    prepared
        .register(&mut store, &delta, &[1; 32], "logical-v1")
        .unwrap();
    assert_eq!(count(&store, "content_capture_reservations"), 0);
    assert_eq!(count(&store, "external_storage_capture_files"), 2);
    let lease = store.acquire_revision(3).unwrap().lease;
    assert_eq!(
        changes::page(store.revision_leases.get(&lease).unwrap(), 2, None, 128)
            .unwrap()
            .len(),
        1
    );
    store.release_revision(&lease).unwrap();
}

#[test]
fn external_capture_rejects_mismatched_cache_and_releases_gc_guard_on_cancel() {
    use crate::external_storage::capture::CaptureCatalog;
    struct Cancel(bool);
    impl crate::local_backup::CancellationProbe for Cancel {
        fn is_cancelled(&self) -> bool {
            self.0
        }
    }
    let (directory, mut store, _) = capture_fixture();
    cursor(&mut store, "consumer", 1);
    let mut empty = CaptureCatalog::create(
        &directory.path().join("empty"),
        &directory.path().join("objects"),
        None,
    )
    .unwrap();
    let prepared = store
        .prepare_content_capture("bad-cache", "consumer", 1)
        .unwrap();
    assert!(prepared
        .project(&mut empty, &Cancel(false))
        .unwrap_err()
        .to_string()
        .contains("Capture cache requires rebuild"));
    assert_eq!(
        prepared
            .project(&mut empty, &Cancel(true))
            .unwrap_err()
            .to_string(),
        "External capture cancelled"
    );
    assert!(empty.manifest().is_err());
    drop(prepared);
    assert!(store.active_readers.detached_asset_roots().is_ok());
    store.abandon_content_capture("bad-cache").unwrap();
    assert_eq!(count(&store, "external_storage_captures"), 0);
    assert_eq!(count(&store, "content_capture_reservations"), 0);
}

#[test]
fn external_capture_registration_failure_does_not_advance_cache_or_cursor() {
    use crate::external_storage::capture::CaptureCatalog;
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let mut catalog = CaptureCatalog::create(
        &directory.path().join("external-storage/job"),
        &directory.path().join("external-storage/objects"),
        None,
    )
    .unwrap();
    let prepared = store
        .prepare_content_capture("register-failure", "consumer", 1)
        .unwrap();
    prepared.project(&mut catalog, &Never).unwrap();
    store.connection.execute_batch("CREATE TRIGGER synthetic_capture_failure BEFORE INSERT ON external_storage_capture_files BEGIN SELECT RAISE(ABORT,'synthetic'); END").unwrap();
    assert!(prepared
        .register(&mut store, &catalog, &[1; 32], "logical-v1")
        .is_err());
    for table in [
        "external_storage_captures",
        "external_storage_content_cache",
        "content_change_consumers",
        "external_storage_capture_files",
    ] {
        assert_eq!(count(&store, table), 0);
    }
    assert_eq!(count(&store, "content_capture_reservations"), 1);
    assert!(store.active_readers.detached_asset_roots().is_ok());
    store
        .connection
        .execute_batch("DROP TRIGGER synthetic_capture_failure")
        .unwrap();
    store.abandon_content_capture("register-failure").unwrap();
    drop(store);
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let prepared = store
        .prepare_content_capture("retry", "consumer", 1)
        .unwrap();
    prepared
        .register(&mut store, &catalog, &[1; 32], "logical-v1")
        .unwrap();
    assert_eq!(count(&store, "external_storage_captures"), 1);
    assert_eq!(count(&store, "content_capture_reservations"), 0);
}

#[test]
fn external_capture_keeps_deleted_source_payload_pinned_after_reopen() {
    use crate::external_storage::capture::{registered_roots, CaptureCatalog};
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let (directory, mut store, _) = capture_fixture();
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let payload = cas
        .prepare_bytes(b"synthetic-external-capture-asset")
        .unwrap();
    let alias = AssetAlias {
        key: "assets/synthetic.bin".into(),
        object_hash: Some(payload.content_hash.clone()),
        kind: "asset".into(),
        size: payload.byte_size as i64,
        mime: "application/octet-stream".into(),
        name: "synthetic".into(),
        ext: "bin".into(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    store.commit_asset_alias(&alias, 1).unwrap();
    let mut catalog = CaptureCatalog::create(
        &directory.path().join("external-storage/job"),
        &directory.path().join("external-storage/objects"),
        None,
    )
    .unwrap();
    let prepared = store
        .prepare_content_capture("asset-capture", "consumer", 2)
        .unwrap();
    prepared.project(&mut catalog, &Never).unwrap();
    prepared
        .register(&mut store, &catalog, &[1; 32], "logical-v1")
        .unwrap();
    store.delete_asset_alias("asset", &alias.key, 2).unwrap();
    drop(store);
    let store = PersistentStore::open(directory.path()).unwrap();
    let roots = registered_roots(&store.connection, directory.path()).unwrap();
    assert!(roots.object_hashes.contains(&payload.content_hash));
    // A damaged registered catalog must stop GC instead of dropping its pins.
    let (_, path, _) = catalog.manifest().unwrap();
    let path = path.to_path_buf();
    drop(catalog);
    fs::write(path, b"synthetic-corrupt-catalog").unwrap();
    assert!(registered_roots(&store.connection, directory.path()).is_err());
}

#[test]
fn external_backup_consumers_share_capture_but_cancel_and_device_identity_are_independent() {
    let (dir, mut store, _) = capture_fixture();
    let identity = selection::identity(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    let first = external::register_capture(
        &tx,
        "capture-a",
        &identity,
        "library-device",
        "codec-1",
        "device-1",
        &"a".repeat(64),
        "consumer-a",
    )
    .unwrap();
    let shared = external::register_capture(
        &tx,
        "capture-b",
        &identity,
        "library-device",
        "codec-1",
        "device-1",
        &"a".repeat(64),
        "consumer-b",
    )
    .unwrap();
    assert_eq!(first, shared);
    let newer_device = external::register_capture(
        &tx,
        "capture-device-2",
        &identity,
        "library-device",
        "codec-1",
        "device-2",
        &"b".repeat(64),
        "consumer-c",
    )
    .unwrap();
    assert_ne!(first, newer_device);
    external::prepare_backup(
        &tx,
        "job-a",
        "destination-a",
        "repository-a",
        &first,
        "point-a",
    )
    .unwrap();
    external::prepare_backup(
        &tx,
        "job-b",
        "destination-b",
        "repository-b",
        &shared,
        "point-b",
    )
    .unwrap();
    external::cancel_prepared(&tx, "job-a").unwrap();
    assert!(external::capture_has_consumers(&tx, &shared).unwrap());
    tx.commit().unwrap();
    drop(store);
    let mut store = PersistentStore::open(dir.path()).unwrap();
    assert!(external::capture_has_consumers(&store.connection, &shared).unwrap());
    assert_eq!(
        selection::read(&store.connection).unwrap().target,
        selection::SyncTarget::None
    );
    let tx = store.connection.transaction().unwrap();
    external::cancel_prepared(&tx, "job-b").unwrap();
    assert!(!external::capture_has_consumers(&tx, &shared).unwrap());
    tx.commit().unwrap();
}
