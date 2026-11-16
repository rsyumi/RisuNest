use super::*;

const EXTERNAL_WRITER: &str = "00000000-0000-4000-8000-0000000000aa";

fn device_count(store: &PersistentStore, sql: &str) -> i64 {
    store.device_store().unwrap().connection().query_row(sql, [], |row| row.get(0)).unwrap()
}
fn library_count(store: &PersistentStore, sql: &str) -> i64 {
    store.connection.query_row(sql, [], |row| row.get(0)).unwrap()
}
fn page(store: &PersistentStore, id: &str, kind: &str, writer: Option<&str>, cursor: u64, changes: Vec<Change>) -> StageReceive {
    StageReceive {
        header: Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: id.into() },
        changes,
        progress: Progress { kind: kind.into(), cursor: cursor.into(), writer_id: writer.map(Into::into) },
        admitted_time_upper_ms: u64::MAX.into(),
    }
}
fn deliver(store: &mut PersistentStore, page: &StageReceive, generating: Vec<MessageLocator>) -> ApplyResult {
    store.lww_stage_receive(page).unwrap();
    let applied = store.lww_apply_receive(&ApplyReceive { header: page.header.clone(), generating }).unwrap();
    store.lww_finish_receive(&page.header).unwrap();
    applied
}
fn finished_receives(store: &PersistentStore) -> Vec<String> {
    let device = store.device_store().unwrap().connection();
    let mut statement = device.prepare("SELECT request_id FROM lww_receive WHERE finished=1 ORDER BY request_id").unwrap();
    let ids = statement.query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    ids
}
fn receive_rows(store: &PersistentStore) -> (i64, i64) {
    let sql = "SELECT count(*) FROM lww_receive_rows";
    (library_count(store, sql), device_count(store, sql))
}
fn next_time(store: &PersistentStore) -> u64 {
    store.lww_clock_state().unwrap().issued.unwrap().physical_ms.0 + 1
}

#[test]
fn recovery_still_removes_rows_left_by_an_intent_that_is_gone() {
    let (_dir, mut store) = store();
    let db = store.device_store().unwrap().connection();
    db.execute("INSERT INTO lww_intent_proofs VALUES('gone-intent','identity')", []).unwrap();
    db.execute("INSERT INTO lww_intent_failures VALUES('gone-intent',1,'synthetic-failure',0)", []).unwrap();
    db.execute("INSERT INTO lww_intent_rows VALUES('gone-intent',1,?1,NULL,'1',0)", [unit_key(&["root", "username"]).unwrap().as_str()]).unwrap();
    store.lww_recover_intents().unwrap();
    for table in ["lww_intent_proofs", "lww_intent_failures", "lww_intent_rows"] {
        assert_eq!(device_count(&store, &format!("SELECT count(*) FROM {table}")), 0, "{table}");
    }
}

#[test]
fn completed_writes_leave_no_intent_and_their_receipt_still_answers_a_retry() {
    let (_dir, mut store) = store();
    for index in 0..20 {
        save(&mut store, vec![mutation(&["root", "username"], serde_json::json!(format!("user-{index}")))]);
    }
    assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intents"), 0);
    assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intent_rows"), 0);
    let retried = WorkingSetCommit {
        expected_revision: store.revision().unwrap(),
        request_id: Some("retried-write".into()),
        unit_mutations: Some(vec![mutation(&["root", "language"], serde_json::json!("ko"))]),
        ..Default::default()
    };
    let first = store.commit(&retried).unwrap();
    assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intents"), 0);
    assert_eq!(store.commit(&retried).unwrap().revision, first.revision);
    assert_eq!(store.revision().unwrap(), first.revision);
    let changed = WorkingSetCommit {
        unit_mutations: Some(vec![mutation(&["root", "language"], serde_json::json!("ja"))]),
        ..retried.clone()
    };
    assert!(matches!(store.commit(&changed), Err(StoreError::Validation { message }) if message == "request-id-integrity"));
    assert_eq!(store.read_root(None).unwrap().value["language"], "ko");
    assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intents"), 0);
}

#[test]
fn polling_keeps_only_the_newest_finished_page_of_each_stream() {
    let (_dir, mut store) = store();
    let mut last = None;
    for index in 1..=20u64 {
        let server = page(&store, &format!("server-{index:02}"), "server", None, index,
            vec![change(&["root", "username"], index, serde_json::json!(format!("server-{index}")))]);
        deliver(&mut store, &server, vec![]);
        if index % 5 == 0 {
            let external = page(&store, &format!("external-{index:02}"), "external", Some(EXTERNAL_WRITER), index,
                vec![change(&["root", "language"], index, serde_json::json!(format!("external-{index}")))]);
            deliver(&mut store, &external, vec![]);
        }
        last = Some(server);
    }
    assert_eq!(finished_receives(&store), ["external-20", "server-20"]);
    assert_eq!(receive_rows(&store), (0, 0));
    let last = last.unwrap();
    store.server_assert_receive_finished(&last).unwrap();
    let revision = store.revision().unwrap();
    deliver(&mut store, &last, vec![]);
    assert_eq!(store.revision().unwrap(), revision);
    assert_eq!(finished_receives(&store), ["external-20", "server-20"]);
    assert_eq!(receive_rows(&store), (0, 0));
    store.server_assert_receive_finished(&last).unwrap();
}

#[test]
fn only_external_pages_that_repeat_the_cursor_stay_until_a_later_page_moves_it() {
    let (_dir, mut store) = store();
    let part = |store: &PersistentStore, index: u64, cursor: u64| page(store, &format!("group-{index}"), "external", Some(EXTERNAL_WRITER), cursor,
        vec![change(&["root", "language"], index + 1, serde_json::json!(format!("part-{index}")))]);
    let first = part(&store, 0, 4);
    deliver(&mut store, &first, vec![]);
    for index in 1..3u64 {
        let next = part(&store, index, 4);
        deliver(&mut store, &next, vec![]);
    }
    // A state bootstrap repeats the server cursor on every page but its last.
    for index in 0..3u64 {
        let bootstrap = page(&store, &format!("bootstrap-{index}"), "server", None, 7,
            vec![change(&["root", "username"], index + 1, serde_json::json!(format!("bootstrap-{index}")))]);
        deliver(&mut store, &bootstrap, vec![]);
    }
    assert_eq!(finished_receives(&store), ["bootstrap-2", "group-0", "group-1", "group-2"]);
    let revision = store.revision().unwrap();
    deliver(&mut store, &first, vec![]);
    assert_eq!(store.revision().unwrap(), revision);
    let last = part(&store, 3, 5);
    deliver(&mut store, &last, vec![]);
    assert_eq!(finished_receives(&store), ["bootstrap-2", "group-3"]);
    assert_eq!(store.read_root(None).unwrap().value["language"], "part-3");
}

#[test]
fn finishing_a_page_of_an_external_group_costs_in_proportion_to_the_group() {
    use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
    let (_dir, mut store) = store();
    let mut steps = Vec::new();
    for index in 0..48u64 {
        let part = page(&store, &format!("group-{index:02}"), "external", Some(EXTERNAL_WRITER), 4,
            vec![change(&["root", "language"], index + 1, serde_json::json!(format!("part-{index}")))]);
        store.lww_stage_receive(&part).unwrap();
        store.lww_apply_receive(&ApplyReceive { header: part.header.clone(), generating: vec![] }).unwrap();
        let counted = Arc::new(AtomicUsize::new(0));
        let counter = counted.clone();
        store.device_store().unwrap().connection().progress_handler(8, Some(move || {
            counter.fetch_add(1, Ordering::Relaxed);
            false
        })).unwrap();
        store.lww_finish_receive(&part.header).unwrap();
        store.device_store().unwrap().connection().progress_handler(8, None::<fn() -> bool>).unwrap();
        steps.push(counted.load(Ordering::Relaxed));
    }
    assert_eq!(finished_receives(&store).len(), 48);
    // Comparing every retained page with every later one would cost the square of the group.
    assert!(steps[47] <= steps[11] * 4, "{steps:?}");
}

#[test]
fn held_and_deferred_rows_keep_their_page_until_the_drain_releases_them() {
    let (_dir, mut store) = store();
    create_conversation(&mut store, "char", "chat");
    let time = next_time(&store);
    let message = Change {
        key: unit_key(&["messages", "char", "chat"]).unwrap(),
        stamp: stamp(time),
        value: incoming_message_value(&store, "char", "chat", "deferred-message"),
    };
    let held = change(&["character", "waiting", "name"], time, serde_json::json!("held-name"));
    let generating = vec![MessageLocator { character_id: "char".into(), conversation_id: "chat".into(), start: None }];
    let deferred_page = page(&store, "deferred-page", "server", None, 1, vec![message.clone()]);
    assert_eq!(deliver(&mut store, &deferred_page, generating).deferred_keys, vec![message.key.clone()]);
    let held_page = page(&store, "held-page", "server", None, 2, vec![held.clone()]);
    assert_eq!(deliver(&mut store, &held_page, vec![]).held_keys, vec![held.key.clone()]);
    for index in 3..=5u64 {
        let later = page(&store, &format!("later-{index}"), "server", None, index, vec![]);
        deliver(&mut store, &later, vec![]);
    }
    assert_eq!(finished_receives(&store), ["deferred-page", "held-page", "later-5"]);
    let lease = store.lww_acquire_library_backup_capture(store.revision().unwrap()).unwrap();
    let units = store.lww_backup_unit_values(&lease.lease).unwrap();
    assert_eq!(units[&held.key], held.value);
    assert_eq!(units[&message.key], message.value);
    store.release_revision(&lease.lease).unwrap();

    let drain = Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: "drain".into() };
    store.lww_drain_deferred(&ApplyReceive { header: drain, generating: vec![] }).unwrap();
    assert_eq!(read_unit(&store.connection, &message.key).unwrap().unwrap().1, message.value);
    assert_eq!(finished_receives(&store), ["held-page", "later-5"]);

    let parent = page(&store, "parent", "server", None, 6,
        vec![change(&["exists", "character", "waiting"], time, serde_json::json!({"type":"character"}))]);
    assert!(deliver(&mut store, &parent, vec![]).affected_keys.contains(&held.key));
    assert_eq!(finished_receives(&store), ["parent"]);
    assert_eq!(receive_rows(&store), (0, 0));
}

#[test]
fn the_deferred_drain_reads_its_rows_through_the_status_index() {
    let (_dir, store) = store();
    for db in [&store.connection, store.device_store().unwrap().connection()] {
        let mut statement = db.prepare(&format!("EXPLAIN QUERY PLAN {DRAIN_ROWS_SQL}")).unwrap();
        let plan: Vec<String> = statement.query_map([], |row| row.get(3)).unwrap().collect::<Result<_, _>>().unwrap();
        assert!(plan.iter().any(|step| step.contains("USING INDEX lww_receive_status (status=?)")), "{plan:?}");
    }
}

#[test]
fn a_replay_that_keeps_failing_with_a_store_error_is_set_aside_and_the_store_opens() {
    let (dir, mut store) = store();
    let header = Header { binding_authority: 0.into(), request_id: "synthetic-stuck".into() };
    let intent = Intent::Commit {
        commit: WorkingSetCommit {
            expected_revision: 0,
            unit_mutations: Some(vec![mutation(&["root", "language"], serde_json::json!("ko"))]),
            ..Default::default()
        },
        aliases: vec![],
    };
    store.reserve_intent(&header, &intent).unwrap();
    store.connection.execute_batch(
        "CREATE TRIGGER synthetic_stuck_receipt BEFORE INSERT ON lww_requests WHEN NEW.request_id='synthetic-stuck'
         BEGIN SELECT RAISE(ABORT,'synthetic store failure'); END;",
    ).unwrap();
    drop(store);
    let failures = |store: &PersistentStore| -> Option<(i64, bool, String)> {
        store.device_store().unwrap().connection().query_row(
            "SELECT failures,quarantined,error FROM lww_intent_failures WHERE request_id='synthetic-stuck'", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().unwrap()
    };
    let edit = |store: &mut PersistentStore, name: &str| store.commit(&WorkingSetCommit {
        expected_revision: store.revision().unwrap(),
        unit_mutations: Some(vec![mutation(&["root", "username"], serde_json::json!(name))]),
        ..Default::default()
    });

    let mut store = PersistentStore::open(dir.path()).expect("the store opens while the intent keeps failing");
    assert_eq!(failures(&store), Some((1, false, "synthetic store failure".into())));
    assert_eq!(store.revision().unwrap(), 0);
    assert!(edit(&mut store, "blocked").is_err());
    assert_eq!(failures(&store), Some((2, false, "synthetic store failure".into())));
    let saved = edit(&mut store, "saved").unwrap();
    assert_eq!(failures(&store), Some((3, true, "synthetic store failure".into())));
    assert_eq!(store.revision().unwrap(), saved.revision);
    let root = store.read_root(None).unwrap().value;
    assert_eq!(root["username"], "saved");
    assert_ne!(root["language"], "ko");
    let quarantined = store.lww_quarantined_intents().unwrap();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0].request_id, "synthetic-stuck");
    assert_eq!(quarantined[0].kind, "commit");
    assert_eq!(quarantined[0].error, "synthetic store failure");
    assert_eq!(quarantined[0].token.len(), 64);
    let (lease, _) = store.lww_acquire_backup_capture(store.revision().unwrap()).unwrap();
    store.release_revision(&lease.lease).unwrap();
    let lease = store.acquire_revision(store.revision().unwrap()).unwrap();
    let findings = store.data_health_reader(&lease.lease).unwrap().scan(100, &crate::local_backup::NeverCancelled).unwrap();
    store.release_revision(&lease.lease).unwrap();
    let finding = findings.items.iter().find(|finding| finding.code == "intent-quarantined").expect("the set-aside change is reported");
    assert_eq!(finding.severity, crate::data_health::Severity::Degraded);
    assert_eq!((finding.owner.kind.as_str(), finding.owner.id.as_str()), ("intent", "synthetic-stuck"));
    assert_eq!(finding.detail, "synthetic store failure");
    drop(store);

    let store = PersistentStore::open(dir.path()).unwrap();
    assert_eq!(failures(&store), Some((3, true, "synthetic store failure".into())));
    assert_eq!(store.read_root(None).unwrap().value["username"], "saved");
}

#[test]
fn an_edit_under_a_deleted_record_is_refused_and_a_delete_there_changes_nothing() {
    let (_dir, mut store) = store();
    create_conversation(&mut store, "char", "chat");
    let name = unit_key(&["character", "char", "name"]).unwrap();
    save(&mut store, vec![mutation(&["character", "char", "name"], serde_json::json!("before"))]);
    let time = next_time(&store);
    receive(&mut store, "remote-delete", vec![Change {
        key: unit_key(&["exists", "character", "char"]).unwrap(),
        stamp: stamp(time),
        value: UnitValue::Deleted,
    }], vec![]);
    assert_eq!(parent_status(&store.connection, &name).unwrap(), "retired");
    let revision = store.revision().unwrap();
    let refused = store.commit(&WorkingSetCommit {
        expected_revision: revision,
        unit_mutations: Some(vec![
            mutation(&["root", "username"], serde_json::json!("not-saved")),
            mutation(&["character", "char", "name"], serde_json::json!("after")),
        ]),
        ..Default::default()
    });
    assert!(matches!(refused, Err(StoreError::Validation { ref message }) if message == "retired-record-id"), "{refused:?}");
    assert_eq!(store.revision().unwrap(), revision);
    assert_ne!(store.read_root(None).unwrap().value["username"], "not-saved");
    save(&mut store, vec![UnitMutation::Delete { key: name.clone() }]);
    assert!(store.lww_read_outbox(0.into(), 100).unwrap().entries.iter().all(|entry| entry.key != name));
    assert_eq!(parent_status(&store.connection, &name).unwrap(), "retired");
}

#[test]
fn short_device_connections_leave_the_wal_files_of_the_open_store() {
    use risunest_external_storage_format::section::SectionKind;
    let (_dir, mut store) = store();
    let path = std::path::PathBuf::from(store.device_store().unwrap().connection().path().unwrap());
    let sidecar = |suffix: &str| {
        let mut name = path.clone().into_os_string();
        name.push(suffix);
        std::path::PathBuf::from(name)
    };
    let (wal, shm) = (sidecar("-wal"), sidecar("-shm"));
    let kept = |step: &str| assert!(wal.exists() && shm.exists(), "{step} removed the WAL files of the open device store");
    save(&mut store, vec![mutation(&["root", "username"], serde_json::json!("written"))]);
    kept("a commit");
    let fence = store.device_store().unwrap().visit_plugin_gc_values(|_, _, _, _| Ok(()), |_, _| Ok(())).unwrap();
    kept("the plugin GC snapshot");
    assert!(store.device_store().unwrap().plugin_gc_fence_is_current(&fence).unwrap());
    kept("the plugin GC fence check");
    drop(store.device_store().unwrap().acquire_plugin_gc_barrier(&fence).unwrap());
    kept("the plugin GC barrier");
    store.device_store_mut().unwrap().capture_backup_sections(&[SectionKind::LocalSettings], &std::env::temp_dir()).unwrap();
    kept("the section capture");
    let (lease, _) = store.lww_acquire_backup_capture(store.revision().unwrap()).unwrap();
    store.release_revision(&lease.lease).unwrap();
    kept("the backup capture");
    drop(store);
    assert!(!wal.exists(), "closing the last connection removes the WAL file");
}

fn quarantine_fixture(store: &mut PersistentStore, id: &str, value: &UnitValue) -> QuarantinedIntent {
    let db = store.device_store_mut().unwrap().connection();
    db.execute("INSERT INTO lww_intents VALUES(?1,'0','{}','{}','synthetic-digest',0)", [id]).unwrap();
    db.execute("INSERT INTO lww_intent_failures VALUES(?1,3,'synthetic-failure',1)", [id]).unwrap();
    db.execute("INSERT INTO lww_intent_rows VALUES(?1,1,?2,NULL,?3,0)", params![id,unit_key(&["root","username"]).unwrap().as_str(),serde_json::to_string(value).unwrap()]).unwrap();
    store.lww_quarantined_intents().unwrap().into_iter().find(|intent| intent.request_id == id).unwrap()
}

#[test]
fn discarding_quarantine_is_atomic_identity_checked_and_preserves_committed_records() {
    let (dir, mut store) = store();
    let value = inline(&serde_json::json!("synthetic")).unwrap();
    let selected = quarantine_fixture(&mut store, "discard-selected", &value);
    let other = quarantine_fixture(&mut store, "discard-other", &value);
    let revision = store.revision().unwrap();
    assert!(store.lww_discard_quarantined_intent(&selected.request_id, &selected.token, revision + 1).is_err());
    assert!(store.lww_discard_quarantined_intent(&selected.request_id, "stale-token", revision).is_err());
    store.device_store_mut().unwrap().connection().execute("UPDATE lww_intent_failures SET quarantined=0 WHERE request_id=?1", [&selected.request_id]).unwrap();
    assert!(store.lww_discard_quarantined_intent(&selected.request_id, &selected.token, revision).is_err());
    store.device_store_mut().unwrap().connection().execute("UPDATE lww_intent_failures SET quarantined=1 WHERE request_id=?1", [&selected.request_id]).unwrap();
    store.connection.execute("INSERT INTO lww_requests VALUES(?1,'synthetic-digest',0,NULL)", [&other.request_id]).unwrap();
    assert!(store.lww_discard_quarantined_intent(&other.request_id, &other.token, revision).is_err());
    store.device_store_mut().unwrap().connection().execute_batch("CREATE TRIGGER reject_discard BEFORE DELETE ON lww_intents BEGIN SELECT RAISE(ABORT,'synthetic discard failure'); END;").unwrap();
    assert!(store.lww_discard_quarantined_intent(&selected.request_id, &selected.token, revision).is_err());
    assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intent_rows"), 2);
    assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intent_failures"), 2);
    store.device_store_mut().unwrap().connection().execute_batch("DROP TRIGGER reject_discard").unwrap();
    let root = store.read_root(None).unwrap().value;
    assert!(store.lww_discard_quarantined_intent(&selected.request_id, &selected.token, revision).unwrap());
    assert!(!store.lww_discard_quarantined_intent(&selected.request_id, &selected.token, revision).unwrap());
    assert_eq!(store.revision().unwrap(), revision);
    assert_eq!(store.read_root(None).unwrap().value, root);
    let remaining = store.lww_quarantined_intents().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].request_id, other.request_id);
    assert!(!remaining[0].discardable);
    assert_eq!(library_count(&store, "SELECT count(*) FROM lww_requests WHERE request_id='discard-other'"), 1);
    drop(store);
    let store = PersistentStore::open(dir.path()).unwrap();
    assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intents WHERE request_id='discard-selected'"), 0);
    assert_eq!(store.revision().unwrap(), revision);
}

#[test]
fn discarded_intent_roots_expire_only_after_grace_and_shared_bodies_remain() {
    use crate::persistent_store::{message_pages, MessageObjectStore, ASSET_GC_PRODUCT_MINIMUM_GRACE_MS};
    let (_dir, mut store) = store();
    let unique = risunest_sync_wire::hash(b"synthetic-only-quarantine");
    let shared = risunest_sync_wire::hash(b"synthetic-shared-quarantine");
    for db in [&store.connection, store.device_store().unwrap().connection()] {
        message_pages::put_object(db, &unique, b"synthetic-only-quarantine").unwrap();
        message_pages::put_object(db, &shared, b"synthetic-shared-quarantine").unwrap();
    }
    let unique_value = UnitValue::object(risunest_sync_wire::descriptor::RecordDescriptor::content(unique.clone())).unwrap();
    let shared_value = UnitValue::object(risunest_sync_wire::descriptor::RecordDescriptor::content(shared.clone())).unwrap();
    let selected = quarantine_fixture(&mut store, "discard-root", &unique_value);
    let retained = quarantine_fixture(&mut store, "keep-shared-root", &shared_value);
    let now = 1_900_000_000_000i64;
    for target in [MessageObjectStore::Library, MessageObjectStore::Device] {
        store.sweep_message_page_objects(target, now, 256).unwrap();
    }
    let computed = store.message_object_roots_computed();
    let revision = store.revision().unwrap();
    store.lww_discard_quarantined_intent(&selected.request_id, &selected.token, revision).unwrap();
    for target in [MessageObjectStore::Library, MessageObjectStore::Device] {
        store.sweep_message_page_objects(target, now, 256).unwrap();
    }
    assert_eq!(store.message_object_roots_computed(), computed + 2);
    for db in [&store.connection, store.device_store().unwrap().connection()] {
        assert!(message_pages::object_body(db, &unique).unwrap().is_some());
    }
    for target in [MessageObjectStore::Library, MessageObjectStore::Device] {
        store.sweep_message_page_objects(target, now + ASSET_GC_PRODUCT_MINIMUM_GRACE_MS, 256).unwrap();
    }
    for db in [&store.connection, store.device_store().unwrap().connection()] {
        assert!(message_pages::object_body(db, &unique).unwrap().is_none());
        assert!(message_pages::object_body(db, &shared).unwrap().is_some());
    }
    assert_eq!(store.lww_quarantined_intents().unwrap(), vec![retained]);
}

#[test]
fn corrupt_replacement_tail_never_activates_a_prefix_and_exact_replay_keeps_its_stamp() {
    let (_dir, mut store) = store();
    let stage = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&stage, &serde_json::json!({"username":"synthetic-replacement"})).unwrap();
    let header = Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: "synthetic-replay-tail".into() };
    let changes = intent_rows::write(&mut store.device_store_mut().unwrap().connection, &header.request_id, "changes", (0..1025).map(|index| {
        intent_rows::replacement_row(&unit_key(&["root", &format!("synthetic-{index:04}")])?, &inline(&serde_json::json!(index))?, true)
    })).unwrap();
    let intent = Intent::Replacement {
        device_revision: device_revision(store.device_store().unwrap().connection()).unwrap(),
        staging_id: stage.clone(), base_revision: store.revision().unwrap(), staging_digest: binding_stage::catalog_digest(&store.connection, &stage).unwrap(),
        changes, source_units: None, device_sections: None, device_settings_digest: None, device_changes: Cow::Owned(vec![]),
    };
    let (stamp, _) = store.reserve_intent(&header, &intent).unwrap();
    let generation = active_generation(&store.connection).unwrap();
    store.device_store_mut().unwrap().connection().execute("UPDATE lww_intent_rows SET source_override=0 WHERE request_id=?1 AND ordinal=1025", [&header.request_id]).unwrap();
    assert!(store.lww_recover_intents().is_err());
    assert_eq!(active_generation(&store.connection).unwrap(), generation);
    assert_eq!(store.revision().unwrap(), 0);
    assert_eq!(library_count(&store, "SELECT count(*) FROM lww_requests WHERE request_id='synthetic-replay-tail'"), 0);
    store.device_store_mut().unwrap().connection().execute("UPDATE lww_intent_rows SET source_override=1 WHERE request_id=?1 AND ordinal=1025", [&header.request_id]).unwrap();
    store.lww_recover_intents().unwrap();
    assert_eq!(active_generation(&store.connection).unwrap(), stage);
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(store.read_root(None).unwrap().value["username"], "synthetic-replacement");
    let versions: (i64,i64) = store.connection.query_row("SELECT count(*),count(DISTINCT stamp) FROM lww_units WHERE version=?1", [&header.request_id], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(versions, (1025,1));
    assert_eq!(read_unit(&store.connection, &unit_key(&["root","synthetic-1024"]).unwrap()).unwrap().unwrap().0, stamp);
    assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intent_rows WHERE request_id='synthetic-replay-tail'"), 0);
}

fn quarantine_issued(store: &mut PersistentStore, request_id: &str) -> QuarantinedIntent {
    for _ in 0..3 { store.record_intent_failure(request_id, &error("synthetic-cleanup-failure")).unwrap(); }
    store.lww_quarantined_intents().unwrap().into_iter().find(|intent| intent.request_id == request_id).unwrap()
}

#[test]
fn discarding_applied_commit_keeps_receipt_and_retry_result() {
    let (_dir, mut store) = store();
    store.device_store_mut().unwrap().connection().execute_batch("CREATE TRIGGER block_completion BEFORE DELETE ON lww_intents BEGIN SELECT RAISE(ABORT,'synthetic cleanup failure'); END;").unwrap();
    let input = WorkingSetCommit {
        request_id: Some("applied-commit".into()), expected_revision: 0,
        unit_mutations: Some(vec![mutation(&["root","username"], serde_json::json!("committed"))]), ..Default::default()
    };
    assert!(store.commit(&input).is_err());
    assert_eq!(store.revision().unwrap(), 1);
    let selected = quarantine_issued(&mut store, "applied-commit");
    assert!(selected.discardable);
    store.device_store_mut().unwrap().connection().execute_batch("DROP TRIGGER block_completion").unwrap();
    assert!(store.lww_discard_quarantined_intent(&selected.request_id, &selected.token, 1).unwrap());
    assert_eq!(store.commit(&input).unwrap().revision, 1);
    assert_eq!(store.read_root(None).unwrap().value["username"], "committed");
}

#[test]
fn discarded_applied_replacement_retains_the_completed_receipt_body() {
    let (_dir, mut store) = store();
    let stage = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&stage, &serde_json::json!({"username":"replacement"})).unwrap();
    let header = Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: "applied-replacement".into() };
    store.device_store_mut().unwrap().connection().execute_batch("CREATE TRIGGER block_completion BEFORE UPDATE OF complete ON lww_intents BEGIN SELECT RAISE(ABORT,'synthetic cleanup failure'); END;").unwrap();
    assert!(store.lww_commit_replacement(&header, &stage).is_err());
    assert_eq!(store.revision().unwrap(), 1);
    let selected = quarantine_issued(&mut store, &header.request_id);
    assert!(selected.discardable);
    store.device_store_mut().unwrap().connection().execute_batch("DROP TRIGGER block_completion").unwrap();
    assert!(store.lww_discard_quarantined_intent(&selected.request_id, &selected.token, 1).unwrap());
    assert!(!store.lww_discard_quarantined_intent(&selected.request_id, &selected.token, 1).unwrap());
    assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intents WHERE request_id='applied-replacement' AND complete=1"), 1);
    assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intent_rows WHERE request_id='applied-replacement'"), 0);
    assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intent_failures WHERE request_id='applied-replacement'"), 0);
    assert_eq!(store.lww_commit_replacement(&header, &stage).unwrap().revision, 1);
    assert_eq!(store.read_root(None).unwrap().value["username"], "replacement");
}

#[test]
fn cross_store_quarantine_cannot_abandon_device_or_binding_completion() {
    let (_dir, mut store) = store();
    let change = crate::persistent_store::sync_selection::BindingSelectionChange {
        expected_epoch: "before".into(), new_epoch: "after".into(),
        target: crate::persistent_store::sync_selection::SyncTarget::None, library_id: None, target_id: None, inspection_id: None, initial_publication: false,
    };
    let rows = intent_rows::digest("target", std::iter::empty()).unwrap();
    let sections = full_device_backup(Some("unapplied-device-restore"));
    let cases = [
        Intent::Target { device_revision: 0, staging_id: "stage".into(), changes: rows.clone() },
        Intent::Replacement { device_revision: 0, staging_id: "stage".into(), base_revision: 0, staging_digest: "synthetic".into(), changes: rows, source_units: None,
            device_sections: Some(Cow::Owned(device_store::sections::freeze_backup_sections(&sections.iter().collect::<Vec<_>>()).unwrap())),
            device_settings_digest: Some(device_store::sections::replacement_local_settings_digest(store.device_store().unwrap().connection()).unwrap()), device_changes: Cow::Owned(vec![]) },
        Intent::Switch { device_revision: 0, change: change.clone(), new_authority: 1.into() },
        Intent::NewDevice { device_revision: 0, authorization_id: "authorized".into(), staging_id: "stage".into(), changes: vec![], old_writer_id: "old".into(), writer_id: "new".into(), new_authority: 1.into(), selection_change: change },
    ];
    for (index, intent) in cases.into_iter().enumerate() {
        let header = Header { binding_authority: 0.into(), request_id: format!("partial-{index}") };
        let (_, digest) = store.reserve_intent(&header, &intent).unwrap();
        store.connection.execute("INSERT INTO lww_requests VALUES(?1,?2,1,'stage')", params![header.request_id,digest]).unwrap();
        let selected = quarantine_issued(&mut store, &header.request_id);
        assert!(!selected.discardable);
        assert!(store.lww_discard_quarantined_intent(&selected.request_id, &selected.token, 0).is_err());
    }
    assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intents WHERE complete=0"), 4);
    assert_eq!(library_count(&store, "SELECT count(*) FROM lww_requests"), 4);
}

#[test]
fn quarantine_classification_checks_repair_versions_in_both_stores() {
    let (_dir, mut store) = store();
    for device in [false, true] {
        let header = Header { binding_authority: 0.into(), request_id: format!("repair-{device}") };
        let (stamp, _) = store.reserve_intent(&header, &Intent::Repair { entries: vec![] }).unwrap();
        let selected = quarantine_issued(&mut store, &header.request_id);
        assert!(selected.discardable);
        let db = if device { store.device_store().unwrap().connection() } else { &store.connection };
        put_unit(db, &unit_key(&["root", "synthetic-repair"]).unwrap(), &stamp, &inline(&serde_json::json!(true)).unwrap(), &header.request_id, Some(0.into())).unwrap();
        let selected = store.lww_quarantined_intents().unwrap().into_iter().find(|intent| intent.request_id == header.request_id).unwrap();
        assert!(!selected.discardable);
        assert!(store.lww_discard_quarantined_intent(&selected.request_id, &selected.token, 0).is_err());
    }
}

fn interrupted_binding(new_device: bool) -> (tempfile::TempDir, PersistentStore, Header, String) {
    use crate::persistent_store::sync_selection::{SwitchBindingRequest, SyncTarget};
    let (dir, mut store) = store();
    let (mut header, stage, changes) = new_device_stage(&mut store);
    let authorization = if new_device {
        let prepared = store.prepare_lww_new_device(&header, &stage).unwrap();
        store.authorize_lww_new_device(&prepared.authorization_id).unwrap();
        Some(prepared.authorization_id)
    } else {
        let inspection: String = store.connection.query_row("SELECT inspection_id FROM lww_binding_stages WHERE staging_id=?1", [&stage], |row| row.get(0)).unwrap();
        let state = store.lww_binding_state().unwrap();
        let switched = store.switch_lww_binding(&SwitchBindingRequest {
            header: Header { binding_authority: state.target_authority, request_id: Uuid::new_v4().to_string() },
            expected_selection_epoch: state.selection_epoch, target: SyncTarget::Server("fresh-connection".into()),
            inspection_id: Some(inspection), initial_publication: false,
        }).unwrap();
        header.binding_authority = switched.target_authority;
        None
    };
    store.device_store().unwrap().connection().execute_batch("CREATE TEMP TRIGGER fail_device_completion BEFORE UPDATE OF complete ON lww_intents BEGIN SELECT RAISE(ABORT,'synthetic-device-completion'); END").unwrap();
    let failed = if let Some(authorization) = authorization {
        store.lww_replace_target_as_new_device(&header, &stage, &authorization).map(|_| ())
    } else { store.lww_replace_target(&header, &stage, &changes).map(|_| ()) };
    assert!(failed.is_err());
    assert_eq!(store.lww_activated_receipt(&header.request_id).unwrap().unwrap().1, stage);
    let selected = quarantine_issued(&mut store, &header.request_id);
    assert!(selected.recoverable && !selected.discardable);
    store.device_store().unwrap().connection().execute_batch("DROP TRIGGER fail_device_completion").unwrap();
    (dir, store, header, stage)
}

#[test]
fn quarantined_target_and_new_device_finish_once_after_reopen_and_keep_later_publication() {
    for new_device in [false, true] {
        let (dir, store, header, stage) = interrupted_binding(new_device);
        let old_writer = store.lww_clock_state().unwrap().writer_id;
        drop(store);
        let mut store = PersistentStore::open(dir.path()).unwrap();
        assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intent_failures WHERE quarantined=1"), 1);
        save(&mut store, vec![mutation(&["root", "askRemoval"], serde_json::json!(true))]);
        let later = store.lww_read_outbox(header.binding_authority, 100).unwrap().entries.into_iter().find(|row| row.key == unit_key(&["root", "askRemoval"]).unwrap()).unwrap();
        let selected = store.lww_quarantined_intents().unwrap().remove(0);
        let revision = store.revision().unwrap();
        assert!(store.lww_complete_quarantined_intent(&selected.request_id, &selected.token, revision).unwrap());
        assert!(!store.lww_complete_quarantined_intent(&selected.request_id, &selected.token, revision).unwrap());
        assert_eq!(active_generation(&store.connection).unwrap(), stage);
        assert_eq!(store.read_root(None).unwrap().value["askRemoval"], true);
        assert_eq!(store.revision().unwrap(), revision);
        let state = store.lww_clock_state().unwrap();
        assert_eq!(state.binding_authority.0, header.binding_authority.0 + u64::from(new_device));
        assert_eq!(state.writer_id == old_writer, !new_device);
        let published = store.lww_read_outbox(state.binding_authority, 100).unwrap().entries.into_iter().find(|row| row.key == later.key).unwrap();
        assert_eq!(published.stamp, later.stamp);
        assert_eq!(published.version, later.version);
        assert_eq!(published.value, later.value);
        assert!(store.lww_quarantined_intents().unwrap().is_empty());
        assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intent_rows"), 0);
    }
}

#[test]
fn quarantined_device_restore_finishes_each_device_boundary_without_replacing_later_library_edits() {
    for phase in ["settings", "outbox", "completion"] {
        let (dir, mut store) = store();
        store.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
        write_restore_device_fixture(&mut store, "before");
        let sections = full_device_backup(Some("restored"));
        let stage = restore_stage(&mut store, "restored");
        let header = Header { binding_authority: 0.into(), request_id: format!("quarantined-restore-{phase}") };
        let sql = match phase {
            "settings" => "CREATE TEMP TRIGGER fail_device BEFORE INSERT ON device_settings BEGIN SELECT RAISE(ABORT,'settings'); END",
            "outbox" => "CREATE TEMP TRIGGER fail_device BEFORE INSERT ON lww_outbox BEGIN SELECT RAISE(ABORT,'outbox'); END",
            _ => "CREATE TEMP TRIGGER fail_device BEFORE UPDATE OF complete ON lww_intents BEGIN SELECT RAISE(ABORT,'completion'); END",
        };
        store.device_store().unwrap().connection().execute_batch(sql).unwrap();
        assert!(store.lww_commit_replacement_with_device_sections(&header, &stage, None, &sections.iter().collect::<Vec<_>>()).is_err());
        assert_eq!(store.revision().unwrap(), 1);
        assert_device_restore_label(&store, "before");
        let selected = quarantine_issued(&mut store, &header.request_id);
        assert!(selected.recoverable);
        assert!(store.lww_complete_quarantined_intent(&selected.request_id, &selected.token, 1).is_err());
        assert_eq!(store.lww_quarantined_intents().unwrap()[0].token, selected.token);
        assert!(device_count(&store, "SELECT count(*) FROM lww_intent_rows") > 0);
        store.device_store().unwrap().connection().execute_batch("DROP TRIGGER fail_device").unwrap();
        drop(store);
        let mut store = PersistentStore::open(dir.path()).unwrap();
        store.device_store().unwrap().write_setting("risu_lastsaved", &serde_json::json!("later")).unwrap();
        save(&mut store, vec![mutation(&["root", "username"], serde_json::json!("keep-later"))]);
        let revision = store.revision().unwrap();
        let selected = store.lww_quarantined_intents().unwrap().remove(0);
        assert!(store.lww_complete_quarantined_intent(&selected.request_id, &selected.token, revision).unwrap());
        assert_device_restore_label(&store, "restored");
        assert_eq!(store.read_root(None).unwrap().value["username"], "keep-later");
        assert_eq!(store.device_store().unwrap().read_setting("risu_lastsaved").unwrap(), Some(serde_json::json!("later")));
        let device_revision = store.device_store().unwrap().revision().unwrap();
        store.device_store().unwrap().write_setting("accountst", &serde_json::json!("after-receipt")).unwrap();
        store.device_store().unwrap().write_plugin_permission("restored", "synthetic", false).unwrap();
        assert_eq!(store.device_store().unwrap().revision().unwrap(), device_revision);
        drop(store);
        let mut store = PersistentStore::open(dir.path()).unwrap();
        assert_eq!(store.lww_device_replacement_receipt(&header, &stage).unwrap().unwrap().revision, 1);
        assert_eq!(store.lww_commit_replacement_with_device_sections(&header, &stage, None, &sections.iter().collect::<Vec<_>>()).unwrap().revision, 1);
        assert!(!store.lww_complete_quarantined_intent(&selected.request_id, &selected.token, revision).unwrap());
        assert!(store.lww_quarantined_intents().unwrap().is_empty());
        assert_eq!(store.device_store().unwrap().read_setting("accountst").unwrap(), Some(serde_json::json!("after-receipt")));
        assert!(!store.device_store().unwrap().read_plugin_permissions().unwrap().into_iter().find(|permission| permission.code_hash == "restored").unwrap().granted);
        assert_eq!(store.device_store().unwrap().revision().unwrap(), device_revision);
        assert_eq!(store.revision().unwrap(), revision);
    }
}

#[test]
fn quarantined_device_restore_preserves_later_fixed_settings_and_permissions_at_each_boundary() {
    for phase in ["settings", "outbox", "completion"] {
        for mutation in ["setting-update", "setting-add", "setting-delete", "permission-update", "permission-add", "permission-delete"] {
            let (dir, mut store) = store();
            store.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
            write_restore_device_fixture(&mut store, "before");
            let sections = full_device_backup(Some("restored"));
            let stage = restore_stage(&mut store, "restored");
            let header = Header { binding_authority: 0.into(), request_id: format!("fixed-edit-{phase}-{mutation}") };
            let sql = match phase {
                "settings" => "CREATE TEMP TRIGGER fail_device BEFORE INSERT ON device_settings BEGIN SELECT RAISE(ABORT,'settings'); END",
                "outbox" => "CREATE TEMP TRIGGER fail_device BEFORE INSERT ON lww_outbox BEGIN SELECT RAISE(ABORT,'outbox'); END",
                _ => "CREATE TEMP TRIGGER fail_device BEFORE UPDATE OF complete ON lww_intents BEGIN SELECT RAISE(ABORT,'completion'); END",
            };
            store.device_store().unwrap().connection().execute_batch(sql).unwrap();
            assert!(store.lww_commit_replacement_with_device_sections(&header, &stage, None, &sections.iter().collect::<Vec<_>>()).is_err());
            assert_eq!(store.revision().unwrap(), 1);
            assert_device_restore_label(&store, "before");
            assert!(store.lww_device_replacement_receipt(&header, &stage).unwrap().is_none());
            let selected = quarantine_issued(&mut store, &header.request_id);
            assert!(selected.recoverable);
            let device = store.device_store().unwrap();
            let original: (String, String, String) = device.connection().query_row(
                "SELECT body,digest,stamp FROM lww_intents WHERE request_id=?1", [&header.request_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap();
            let original_rows = device_count(&store, "SELECT count(*) FROM lww_intent_rows");
            let original_proofs = device_count(&store, "SELECT count(*) FROM lww_intent_proofs");
            let device_revision = device.revision().unwrap();
            device.connection().execute_batch("DROP TRIGGER fail_device").unwrap();
            match mutation {
                "setting-update" => device.write_setting("accountst", &serde_json::json!("later")).unwrap(),
                "setting-add" => device.write_setting("risuNestDeviceSettings", &serde_json::json!({"later":true})).unwrap(),
                "setting-delete" => device.remove_setting("accountst").unwrap(),
                "permission-update" => device.write_plugin_permission("before", "synthetic", false).unwrap(),
                "permission-add" => device.write_plugin_permission("later", "synthetic", true).unwrap(),
                _ => store.device_store_mut().unwrap().clear_plugin_permissions().unwrap(),
            }
            let local_rows = |store: &PersistentStore| -> (Vec<(String, String)>, Vec<(String, String, bool)>) {
                let device = store.device_store().unwrap();
                let settings = device.connection().prepare("SELECT key,value FROM device_settings ORDER BY key").unwrap()
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap();
                let permissions = device.read_plugin_permissions().unwrap().into_iter().map(|row| (row.code_hash, row.permission, row.granted)).collect();
                (settings, permissions)
            };
            let later = local_rows(&store);
            assert_eq!(store.device_store().unwrap().revision().unwrap(), device_revision);
            drop(store);
            let mut store = PersistentStore::open(dir.path()).unwrap();
            let current = store.lww_quarantined_intents().unwrap().remove(0);
            assert_eq!(current.token, selected.token);
            assert!(!current.recoverable && !current.discardable, "{phase}/{mutation}");
            assert!(store.lww_complete_quarantined_intent(&selected.request_id, &selected.token, 1).is_err(), "{phase}/{mutation}");
            assert!(store.lww_discard_quarantined_intent(&selected.request_id, &selected.token, 1).is_err());
            assert!(store.lww_commit_replacement_with_device_sections(&header, &stage, None, &sections.iter().collect::<Vec<_>>()).is_err());
            let after: (String, String, String) = store.device_store().unwrap().connection().query_row(
                "SELECT body,digest,stamp FROM lww_intents WHERE request_id=?1", [&header.request_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap();
            assert_eq!(after, original);
            assert_eq!(local_rows(&store), later);
            assert_eq!(store.device_store().unwrap().revision().unwrap(), device_revision);
            assert_eq!(store.revision().unwrap(), 1);
            assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intent_failures WHERE quarantined=1"), 1);
            assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intent_rows"), original_rows);
            assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intent_proofs"), original_proofs);
            assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intents WHERE complete=0"), 1);
            if phase == "completion" && mutation == "setting-update" {
                let stale_intent: Intent = serde_json::from_str(&original.0).unwrap();
                let new_header = Header { binding_authority: 0.into(), request_id: "stale-preimage-reservation".into() };
                assert!(store.reserve_intent(&new_header, &stale_intent).is_err());
                assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intents"), 1);
            }
        }
    }
}

#[test]
fn quarantined_completion_rejects_missing_rows_receipt_and_changed_device_without_losing_evidence() {
    for failure in ["rows", "receipt", "device", "authority", "body"] {
        let (_dir, mut store, header, _) = interrupted_binding(false);
        let selected = store.lww_quarantined_intents().unwrap().remove(0);
        let revision = store.revision().unwrap();
        assert!(store.lww_complete_quarantined_intent(&selected.request_id, "wrong-token", revision).is_err());
        assert!(store.lww_complete_quarantined_intent(&selected.request_id, &selected.token, revision - 1).is_err());
        match failure {
            "rows" => { store.device_store().unwrap().connection().execute("DELETE FROM lww_intent_rows WHERE request_id=?1", [&header.request_id]).unwrap(); }
            "receipt" => { store.connection.execute("UPDATE lww_requests SET digest='changed' WHERE request_id=?1", [&header.request_id]).unwrap(); }
            "device" => {
                let device = store.device_store_mut().unwrap();
                let before = device.revision().unwrap();
                device.write_plugin_device_values("quarantine-fixture", &[device_store::plugin_values::PluginDeviceMutation::Set {
                    space: "string".into(), key: "later".into(), value: "later-device".into(),
                }]).unwrap();
                assert!(device.revision().unwrap() > before);
            }
            "authority" => { store.device_store().unwrap().connection().execute("UPDATE lww_clock SET binding_authority='999'", []).unwrap(); }
            _ => { store.device_store().unwrap().connection().execute("UPDATE lww_intents SET body=json_set(body,'$.staging_id','changed') WHERE request_id=?1", [&header.request_id]).unwrap(); }
        }
        assert!(store.lww_complete_quarantined_intent(&selected.request_id, &selected.token, revision).is_err(), "{failure}");
        let current = store.lww_quarantined_intents().unwrap().remove(0);
        assert!(!current.recoverable && !current.discardable);
        assert_eq!(device_count(&store, "SELECT count(*) FROM lww_intent_failures WHERE quarantined=1"), 1);
        assert_eq!(store.revision().unwrap(), revision);
    }
}
