use super::*;
use crate::data_health::{codes, result_path, Severity};
use crate::local_backup::NeverCancelled;
use crate::persistent_store::{active_generation, PersistentStore};
use crate::persistent_store::commands::{with_store_mutex, with_store_mutex_mut, SNAPSHOT_ARCHIVE_BUSY};
use sha2::{Digest, Sha256};
use std::fs;
use std::sync::Mutex;
use tempfile::{tempdir, TempDir};

/// A store with one registered object whose stored bytes no longer hash to the registration.
/// Only a reread can see that, so the two depths differ on exactly this fixture.
fn damaged_payload_fixture() -> (TempDir, PersistentStoreState, String) {
    let directory = tempdir().unwrap();
    let store = PersistentStore::open(directory.path()).unwrap();
    let generation = active_generation(&store.connection).unwrap();
    let declared = b"registered-payload-bytes";
    let hash = hex::encode(Sha256::digest(declared));
    let path = directory
        .path()
        .join(crate::asset_repository::object_physical_key(&hash));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // The same length, so only rereading the bytes can tell the two apart.
    fs::write(&path, b"different-payload-bytes!").unwrap();
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext)
             VALUES (?1, 'assets/damaged.bin', ?2, 'asset', ?3, 'application/octet-stream', 'damaged.bin', 'bin')",
            rusqlite::params![generation, hash, declared.len() as i64],
        )
        .unwrap();
    let state = PersistentStoreState {
        store: Mutex::new(Some(store)),
        ..PersistentStoreState::default()
    };
    (directory, state, hash)
}

fn deep_to_completion(state: &PersistentStoreState, health: &DataHealthState) -> ScanResult {
    let mut result = deep_scan(state, health, false).unwrap();
    let mut pages = 0;
    while !result.deep.as_ref().unwrap().complete {
        result = deep_scan(state, health, true).unwrap();
        pages += 1;
        assert!(pages < 64, "the deep pass must terminate");
    }
    result
}

fn has(result: &ScanResult, code: &str) -> bool {
    result.items.iter().any(|finding| finding.code == code)
}

#[test]
fn only_the_deep_scan_rereads_a_stored_object() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();

    let quick = quick_scan(&state, &health).unwrap();
    assert_eq!(quick.depth, ScanDepth::Quick);
    assert!(quick.deep.is_none());
    assert!(
        !has(&quick, codes::ALIAS_OBJECT_MISMATCH),
        "the quick scan compares registrations, not stored bytes: {:?}",
        quick.items
    );

    let deep = deep_to_completion(&state, &health);
    assert_eq!(deep.depth, ScanDepth::Deep);
    assert!(
        has(&deep, codes::ALIAS_OBJECT_MISMATCH),
        "the deep scan rereads the object and sees the digest differ: {:?}",
        deep.items
    );
}

#[test]
fn quick_scan_batches_local_alias_presence() {
    for aliases in [32, 1025] {
        let (_directory, state, hash) = damaged_payload_fixture();
        with_store_mutex(&state, |store| {
            let tx = store.connection.unchecked_transaction()?;
            let generation = active_generation(&tx)?;
            for index in 1..aliases {
                tx.execute("INSERT INTO asset_aliases (generation,logical_key,object_hash,kind,size,mime,name,ext)
                    VALUES (?1,?2,?3,'asset',24,'application/octet-stream','synthetic.bin','bin')",
                    rusqlite::params![generation, format!("assets/synthetic-{index}.bin"), hash])?;
            }
            tx.commit()?;
            Ok(())
        }).unwrap();
        crate::asset_repository::body_io::reset_body_io();
        let started = std::time::Instant::now();
        let result = quick_scan(&state, &DataHealthState::default()).unwrap();
        let io = crate::asset_repository::body_io::take_body_io();
        assert!(!has(&result, codes::ALIAS_OBJECT_MISMATCH));
        assert_eq!(io.stat_requests, 0, "quick validation must not inspect aliases one file request at a time");
        assert!(io.batch_stat_requests <= 2);
        eprintln!("quick_alias_presence aliases={aliases} stat_requests={} batches={} elapsed_us={}", io.stat_requests, io.batch_stat_requests, started.elapsed().as_micros());
    }
}

#[test]
fn a_scan_writes_its_result_to_the_working_folder_for_the_next_reader() {
    let (directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    assert!(last_result(&state).unwrap().is_none());

    let scanned = quick_scan(&state, &health).unwrap();
    let stored = last_result(&state).unwrap().expect("the result is kept");
    assert_eq!(stored, scanned);

    let path = result_path(directory.path());
    assert!(path.starts_with(directory.path().join("persistent")));
    assert!(
        !path.starts_with(directory.path().join("persistent").join("snapshots")),
        "the diagnosis is not a snapshot and must not travel as one"
    );
    assert!(path.exists());
}

#[test]
fn a_deep_scan_resumes_from_its_stored_cursor_and_finishes_once() {
    let (_directory, state, hash) = damaged_payload_fixture();
    let health = DataHealthState::default();

    let first = deep_scan(&state, &health, false).unwrap();
    let progress = first.deep.as_ref().unwrap();
    assert_eq!(progress.total_objects, 1);
    assert!(progress.cursor.is_none() && !progress.complete);
    assert!(!has(&first, codes::ALIAS_OBJECT_MISMATCH));

    let resumed = deep_scan(&state, &health, true).unwrap();
    let progress = resumed.deep.as_ref().unwrap();
    assert_eq!(progress.completed_objects, 1);
    assert_eq!(progress.cursor.as_deref(), Some(hash.as_str()));
    assert!(progress.complete);
    assert!(has(&resumed, codes::ALIAS_OBJECT_MISMATCH));

    // A completed scan is not resumable, so asking again starts a new one rather than adding
    // the same object twice.
    let again = deep_scan(&state, &health, true).unwrap();
    assert_eq!(again.deep.as_ref().unwrap().completed_objects, 0);
    assert_eq!(
        again
            .items
            .iter()
            .filter(|finding| finding.code == codes::ALIAS_OBJECT_MISMATCH)
            .count(),
        0
    );
}

#[test]
fn deep_progress_counts_local_bytes_even_when_registered_sizes_differ() {
    let (directory, state, hash) = damaged_payload_fixture();
    let stored = b"short";
    fs::write(directory.path().join(crate::asset_repository::object_physical_key(&hash)), stored).unwrap();
    with_store_mutex(&state, |store| {
        let generation = active_generation(&store.connection)?;
        store.connection.execute("INSERT INTO asset_aliases (generation,logical_key,object_hash,kind,size,mime,name,ext)
            VALUES (?1,'assets/absent.bin',?2,'asset',1048576,'application/octet-stream','absent.bin','bin')",
            rusqlite::params![generation, "ff".repeat(32)])?;
        Ok(())
    }).unwrap();
    let result = deep_to_completion(&state, &DataHealthState::default());
    let progress = result.deep.unwrap();
    assert_eq!(progress.total_objects, 2);
    assert_eq!(progress.completed_objects, 2);
    assert_eq!(progress.total_bytes, stored.len() as u64);
    assert_eq!(progress.completed_bytes, stored.len() as u64);
}

#[test]
fn a_resume_refuses_a_library_that_changed_under_it() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    deep_scan(&state, &health, false).unwrap();

    let guard = state.admit_renderer_operation().unwrap();
    with_store_mutex_mut_admitted(&state, &guard, |store| {
        let staging = store.replace_begin()?;
        store.replace_put_root(&staging.staging_id, &serde_json::json!({}))?;
        store.replace_commit(&staging.staging_id, Some(0))?;
        Ok(())
    })
    .unwrap();
    drop(guard);

    let error = deep_scan(&state, &health, true).unwrap_err();
    assert!(
        matches!(error, StoreError::RevisionConflict { .. }),
        "a resume onto another revision is refused: {error:?}"
    );
    // Starting over is always available.
    assert!(deep_scan(&state, &health, false).is_ok());
}

#[test]
fn a_cancelled_scan_reports_the_stop_the_renderer_asked_for() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    health.cancel();
    // `begin` clears the previous request, so the stop has to arrive while the scan runs. The
    // probe the scan holds shares the flag, which is what a concurrent cancel command sets.
    let probe = health.begin();
    health.cancel();
    let guard = state.admit_renderer_operation().unwrap();
    let session = open_session(&state, &guard, None).unwrap();
    let error = scan_quick(&session, &probe).unwrap_err();
    session.close(&state, &guard);
    assert!(
        matches!(&error, StoreError::Validation { message } if message == crate::data_health::CANCELLED),
        "{error:?}"
    );
}

#[test]
fn a_damaged_library_reports_every_finding_up_to_the_bound() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let quick = quick_scan(&state, &health).unwrap();
    assert_eq!(
        quick.counts.blocking,
        quick
            .items
            .iter()
            .filter(|finding| finding.severity == Severity::Blocking)
            .count() as u64
    );
    assert_eq!(quick.omitted, 0);
}

/// The preferred repair for the fixture's alias whose stored bytes no longer match its digest.
fn damaged_alias_selection(
    state: &PersistentStoreState,
    health: &DataHealthState,
) -> Vec<String> {
    deep_to_completion(state, health);
    let guard = state.admit_renderer_operation().unwrap();
    let (_, diagnosis) = current_diagnosis(state, &guard).unwrap();
    repair::plan(&diagnosis)
        .into_iter()
        .filter(|candidate| {
            candidate.preferred
                && matches!(
                    candidate.action,
                    crate::data_health::repair::RepairAction::DropAlias { .. }
                )
        })
        .map(|candidate| candidate.id)
        .collect()
}

#[test]
fn a_repair_is_selected_against_the_diagnosis_and_reported_with_a_fresh_one() {
    let (directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let before = deep_to_completion(&state, &health);
    assert!(has(&before, codes::ALIAS_OBJECT_MISMATCH));

    let selection = damaged_alias_selection(&state, &health);
    assert_eq!(selection.len(), 1, "one for the damaged alias");
    let applied = apply_selected(&state, &health, &selection, false).unwrap();
    assert_eq!(applied.revision, before.revision + 1);
    assert!(
        !has(&applied.result, codes::ALIAS_OBJECT_MISMATCH),
        "the reported diagnosis is the repaired library: {:?}",
        applied.result.items
    );
    assert_eq!(applied.result.revision, applied.revision);
    assert!(applied.snapshot.is_none());

    let kept = journals(&state).unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id, applied.journal_id);
    assert!(kept[0].current);
    assert!(crate::data_health::journal::read(directory.path(), &applied.journal_id)
        .unwrap()
        .is_some());
}

#[test]
fn an_undo_returns_the_library_and_drops_the_repair_it_replayed() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let selection = damaged_alias_selection(&state, &health);
    let applied = apply_selected(&state, &health, &selection, false).unwrap();

    let undone = undo_repair(&state, &health, &applied.journal_id, applied.revision).unwrap();
    assert_eq!(undone.revision, applied.revision + 1);
    assert!(undone.skipped.is_empty());
    assert!(
        has(
            &deep_to_completion(&state, &health),
            codes::ALIAS_OBJECT_MISMATCH
        ),
        "the library is what it was before the repair"
    );
    assert!(journals(&state).unwrap().is_empty());
}

#[test]
fn a_repair_selected_against_an_older_diagnosis_is_refused() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let selection = damaged_alias_selection(&state, &health);

    let guard = state.admit_renderer_operation().unwrap();
    with_store_mutex_mut_admitted(&state, &guard, |store| {
        let staging = store.replace_begin()?;
        store.replace_put_root(&staging.staging_id, &serde_json::json!({}))?;
        store.replace_commit(&staging.staging_id, Some(0))?;
        Ok(())
    })
    .unwrap();
    drop(guard);

    let error = apply_selected(&state, &health, &selection, false).unwrap_err();
    assert!(
        matches!(error, StoreError::RevisionConflict { .. }),
        "check the library again before repairing it: {error:?}"
    );
}

#[test]
fn a_repair_needs_a_diagnosis_and_a_selection() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    assert!(matches!(
        apply_repair(&state, &health, &[], None, 0, 0).unwrap_err(),
        StoreError::Validation { .. }
    ));

    quick_scan(&state, &health).unwrap();
    assert!(matches!(
        apply_selected(&state, &health, &["0:nothing".to_owned()], false).unwrap_err(),
        StoreError::Validation { .. }
    ));
}

#[test]
fn the_repair_keeps_a_snapshot_of_the_library_it_was_selected_against() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let selection = damaged_alias_selection(&state, &health);
    for _ in 0..2 {
        with_store_mutex_mut(&state, |store| store.snapshot_create("manual")).unwrap();
    }
    let diagnosis = last_result(&state).unwrap().unwrap();
    let keep_one = KeepCount::requested(Some(1)).unwrap();
    let snapshots = with_store_mutex_mut(&state, |store| Ok(store.snapshots_dir.clone())).unwrap();
    // A snapshot list or delete can hold the archive; the repair waits for it without the store.
    let (busy, read, applied) = std::thread::scope(|scope| {
        let held = crate::persistent_store::snapshot_archive::Archive::open(&snapshots).unwrap();
        let (busy_sent, busy) = std::sync::mpsc::channel();
        let (state, health, selection) = (&state, &health, &selection);
        let repair = scope.spawn(move || {
            SNAPSHOT_ARCHIVE_BUSY.with(|hook| *hook.borrow_mut() = Some(Box::new(move || busy_sent.send(()).unwrap())));
            apply_repair(state, health, selection, Some(keep_one), diagnosis.revision, diagnosis.scanned_at)
        });
        let busy = busy.recv_timeout(std::time::Duration::from_secs(5));
        let (read_sent, read) = std::sync::mpsc::channel();
        scope.spawn(move || read_sent.send(with_store_mutex_mut(state, |store| store.revision())).unwrap());
        let read = read.recv_timeout(std::time::Duration::from_secs(2));
        drop(held);
        (busy, read, repair.join().unwrap())
    });
    assert!(busy.is_ok());
    assert_eq!(read.unwrap().unwrap(), diagnosis.revision);
    let kept = applied.unwrap().snapshot.expect("a snapshot was asked for");
    let guard = state.admit_renderer_operation().unwrap();
    let listed = with_store_mutex_admitted(&state, &guard, |store| store.snapshot_list()).unwrap();
    let listed: Vec<_> = listed.into_iter().map(|snapshot| snapshot.id).collect();
    assert_eq!(listed, vec![kept], "the repair keeps the count it was sent");
}

#[test]
fn the_journal_and_the_diagnosis_stay_in_the_working_folder() {
    let (directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let selection = damaged_alias_selection(&state, &health);
    apply_selected(&state, &health, &selection, false).unwrap();

    let working = directory.path().join("persistent").join("data-health");
    assert!(working.join("result.json").exists());
    assert!(crate::data_health::journal::directory(directory.path()).starts_with(&working));
    let snapshots = directory.path().join("persistent").join("snapshots");
    assert!(
        !working.starts_with(&snapshots),
        "a diagnosis is not a snapshot and never travels as one"
    );
}

/// One stored object nothing registers, so the diagnosis can count it and the cleanup can take it.
fn orphan_object(state: &PersistentStoreState, bytes: &[u8]) -> String {
    let guard = state.admit_renderer_operation().unwrap();
    with_store_mutex_admitted(state, &guard, |store| {
        let cas = crate::asset_repository::PayloadCas::new(store.repository_root())?;
        let stored = cas.prepare_bytes(bytes)?;
        store.connection.execute(
            "INSERT INTO asset_objects(object_hash, byte_size, created_at_ms) VALUES (?1, ?2, 0)",
            rusqlite::params![stored.content_hash, bytes.len() as i64],
        )?;
        Ok(stored.content_hash)
    })
    .unwrap()
}

#[test]
fn a_file_no_record_uses_is_reported_as_something_to_clean_up_and_nothing_more() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let hash = orphan_object(&state, b"nothing points at this");

    let result = quick_scan(&state, &health).unwrap();
    let finding = result
        .items
        .iter()
        .find(|finding| finding.code == codes::OBJECT_UNREFERENCED)
        .expect("the stored object nothing registers is reported");
    assert_eq!(finding.severity, Severity::Informational);
    assert_eq!(finding.owner.id, hash);
    assert_eq!(result.counts.informational, 1);

    // A cleanup candidate is never something a repair offers to change.
    assert!(
        repair::plan(&result)
            .iter()
            .all(|candidate| result.items[candidate.finding].code != codes::OBJECT_UNREFERENCED),
        "deleting a file is the cleanup's decision, never a repair's"
    );
}

#[test]
fn a_registered_file_is_not_reported_as_unused() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let result = quick_scan(&state, &health).unwrap();
    assert!(
        !has(&result, codes::OBJECT_UNREFERENCED),
        "the fixture's one object is registered by an alias: {:?}",
        result.items
    );
}

#[test]
fn a_backup_never_carries_the_diagnosis_or_a_repair_journal() {
    let (directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let selection = damaged_alias_selection(&state, &health);
    apply_selected(&state, &health, &selection, false).unwrap();

    let guard = state.admit_renderer_operation().unwrap();
    let working = directory.path().join("persistent").join("data-health");
    assert!(working.join("result.json").exists());
    assert!(
        crate::data_health::journal::list(directory.path())
            .unwrap()
            .len()
            == 1
    );

    // A library capture reads the leased generation and the stored objects, and the working
    // folder is neither: a raw capture of the library holds no file from it.
    let captured = with_store_mutex_mut_admitted(&state, &guard, |store| {
        let revision = store.revision()?;
        let lease = store.acquire_revision(revision)?.lease;
        let mut destination = rusqlite::Connection::open_in_memory()?;
        crate::persistent_store::portable::create_raw_tables(&destination)?;
        let digests = store.capture_portable_records(&lease, &mut destination, &NeverCancelled)?;
        store.release_revision(&lease)?;
        Ok(digests)
    })
    .unwrap();
    assert!(
        captured.iter().all(|digest| digest.table != "data_health"),
        "the capture projects the library's own tables and nothing beside them"
    );
    assert!(
        std::fs::read_dir(&working).unwrap().count() > 0,
        "the working folder stays where it is"
    );
}

fn apply_selected(state: &PersistentStoreState, health: &DataHealthState, selection: &[String], snapshot: bool) -> StoreResult<RepairApplied> {
    let diagnosis = last_result(state)?.unwrap();
    apply_repair(state, health, selection, snapshot.then_some(KeepCount::MAX), diagnosis.revision, diagnosis.scanned_at)
}

#[test]
fn a_stored_diagnosis_cannot_drop_an_alias_whose_body_the_sync_server_now_holds() {
    let (directory, state, hash) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let object = directory.path().join(crate::asset_repository::object_physical_key(&hash));
    let size = fs::metadata(&object).unwrap().len();
    fs::remove_file(&object).unwrap();
    let before = quick_scan(&state, &health).unwrap();
    assert!(has(&before, codes::ALIAS_OBJECT_ABSENT));
    let drops = |plan: Vec<RepairCandidate>| {
        plan.into_iter()
            .filter(|candidate| matches!(candidate.action, RepairAction::DropAlias { .. }))
            .map(|candidate| candidate.id)
            .collect::<Vec<_>>()
    };
    let selection = drops(repair_plan(&state).unwrap());
    assert_eq!(selection.len(), 1, "a body nothing holds is answered by removing the alias");

    // Custody arrives without a new revision, so the stored diagnosis still names the alias.
    crate::server_sync::residency::test_remote::hold(directory.path(), &[(&hash, size)]);
    assert_eq!(last_result(&state).unwrap().unwrap().revision, before.revision);
    assert_eq!(drops(repair_plan(&state).unwrap()), Vec::<String>::new());
    assert!(matches!(
        apply_selected(&state, &health, &selection, false).unwrap_err(),
        StoreError::Validation { .. }
    ));
    let after = quick_scan(&state, &health).unwrap();
    assert_eq!(after.revision, before.revision, "nothing was repaired");
    assert!(!has(&after, codes::ALIAS_OBJECT_ABSENT), "{:?}", after.items);
}

#[test]
fn diagnosis_identity_rejects_a_replaced_scan_at_the_same_revision() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let selection = damaged_alias_selection(&state, &health);
    let diagnosis = last_result(&state).unwrap().unwrap();
    let error = apply_repair(&state, &health, &selection, None, diagnosis.revision, diagnosis.scanned_at - 1).unwrap_err();
    assert!(matches!(error, StoreError::Validation { .. }));
    assert!(journals(&state).unwrap().is_empty());
}

#[test]
fn journal_failure_after_activation_reports_the_committed_revision() {
    let (directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let selection = damaged_alias_selection(&state, &health);
    let before = last_result(&state).unwrap().unwrap().revision;
    let journal_directory = crate::data_health::journal::directory(directory.path());
    std::fs::create_dir_all(journal_directory.parent().unwrap()).unwrap();
    std::fs::write(&journal_directory, b"synthetic journal failure").unwrap();
    let error = apply_selected(&state, &health, &selection, false).unwrap_err();
    assert!(matches!(error, StoreError::Committed { revision, .. } if revision == before + 1));
    let guard = state.admit_renderer_operation().unwrap();
    assert_eq!(with_store_mutex_admitted(&state, &guard, |store| store.revision()).unwrap(), before + 1);
}


#[test]
fn discard_admission_requires_the_same_diagnosis_and_an_intent_finding() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let result = quick_scan(&state, &health).unwrap();
    assert!(matches!(discard_intent(&state, &health, 0, result.revision, result.scanned_at - 1), Err(StoreError::Validation { .. })));
    assert!(matches!(discard_intent(&state, &health, 0, result.revision - 1, result.scanned_at), Err(StoreError::RevisionConflict { .. })));
    assert!(matches!(discard_intent(&state, &health, usize::MAX, result.revision, result.scanned_at), Err(StoreError::Validation { .. })));
    assert!(matches!(discard_intent(&state, &health, 0, result.revision, result.scanned_at), Err(StoreError::Validation { .. })));
    assert_eq!(last_result(&state).unwrap().unwrap(), result);
}

#[test]
fn completion_requires_current_diagnosis_and_returns_new_native_eligibility() {
    use crate::persistent_store::lww::Header;
    use crate::persistent_store::sync_selection::{SwitchBindingRequest, SyncTarget};
    let (_directory, state, _) = damaged_payload_fixture();
    with_store_mutex_mut(&state, |store| {
        let before = store.lww_binding_state()?;
        store.stop_next_switch_after_library_commit();
        assert!(store.switch_lww_binding(&SwitchBindingRequest {
            header: Header { binding_authority: before.target_authority, request_id: "diagnosed-switch".into() },
            expected_selection_epoch: before.selection_epoch, target: SyncTarget::None,
            inspection_id: None, initial_publication: false,
        }).is_err());
        for _ in 0..3 {
            store.stop_next_switch_after_library_commit();
            let _ = store.lww_recover_intents();
        }
        Ok(())
    }).unwrap();
    let health = DataHealthState::default();
    let diagnosis = quick_scan(&state, &health).unwrap();
    let finding = diagnosis.items.iter().position(|item| item.code == codes::INTENT_QUARANTINED).unwrap();
    assert_eq!(diagnosis.items[finding].intent_action, Some(crate::data_health::IntentAction::Complete));
    assert!(discard_intent(&state, &health, finding, diagnosis.revision, diagnosis.scanned_at).is_err());
    assert!(complete_intent(&state, &health, finding, diagnosis.revision, diagnosis.scanned_at - 1).is_err());
    assert!(complete_intent(&state, &health, finding, diagnosis.revision + 1, diagnosis.scanned_at).is_err());
    assert!(complete_intent(&state, &health, usize::MAX, diagnosis.revision, diagnosis.scanned_at).is_err());
    let completed = complete_intent(&state, &health, finding, diagnosis.revision, diagnosis.scanned_at).unwrap();
    assert!(completed.completed);
    assert_eq!(completed.revision, diagnosis.revision);
    assert!(!has(&completed.result, codes::INTENT_QUARANTINED));
    assert!(complete_intent(&state, &health, finding, diagnosis.revision, diagnosis.scanned_at).is_err());
    assert_eq!(with_store_mutex(&state, |store| store.lww_binding_authority()).unwrap().0, 1);
}

/// The scan's reader holds a read transaction its lease does not count. Released after the lease,
/// it pins the WAL against the truncate checkpoint the release runs, and that checkpoint waits out
/// the store's busy timeout before it settles for a passive one that leaves the WAL in place.
#[test]
fn a_scan_closes_its_reader_before_the_lease_goes_back() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let scans: [&dyn Fn() -> StoreResult<ScanResult>; 2] =
        [&|| quick_scan(&state, &health), &|| deep_scan(&state, &health, false)];
    for (index, scan) in scans.into_iter().enumerate() {
        let wal = with_store_mutex(&state, |store| {
            // A pinned WAL then fails the truncate at once instead of after five seconds.
            store.connection.busy_timeout(std::time::Duration::ZERO)?;
            // The write leaves frames, so the scan's reader reads through the WAL. A value equal
            // to the stored one would leave the page clean and write none.
            store.set_app_kv("device-backup-commit:before-data-health-scan", &serde_json::json!(index))?;
            Ok(std::path::PathBuf::from(format!("{}-wal", store.database_path.display())))
        })
        .unwrap();
        assert!(fs::metadata(&wal).unwrap().len() > 0);
        scan().unwrap();
        assert_eq!(
            fs::metadata(&wal).unwrap().len(),
            0,
            "the release truncates the WAL only when no reader outlives the lease"
        );
    }
}
