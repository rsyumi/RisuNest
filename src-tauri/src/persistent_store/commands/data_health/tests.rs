use super::*;
use crate::data_health::{codes, result_path, Severity};
use crate::local_backup::NeverCancelled;
use crate::persistent_store::{active_generation, PersistentStore};
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
    release(&state, &guard, &session.lease);
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
    // The fresh store still carries its legacy storage authorities, which block a backup.
    assert!(has(&quick, codes::AUTHORITY_INCOMPLETE));
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

/// The candidates that settle the two storage authorities. The fixture also reports references
/// the empty root has always named, which this leaves alone.
fn authority_selection(state: &PersistentStoreState) -> Vec<String> {
    let guard = state.admit_renderer_operation().unwrap();
    let (_, diagnosis) = current_diagnosis(state, &guard).unwrap();
    repair::plan(&diagnosis)
        .into_iter()
        .filter(|candidate| {
            matches!(
                candidate.action,
                crate::data_health::repair::RepairAction::SettleAuthority { .. }
            )
        })
        .map(|candidate| candidate.id)
        .collect()
}

#[test]
fn a_repair_is_selected_against_the_diagnosis_and_reported_with_a_fresh_one() {
    let (directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    let before = quick_scan(&state, &health).unwrap();
    assert!(has(&before, codes::AUTHORITY_INCOMPLETE));

    let selection = authority_selection(&state);
    assert_eq!(selection.len(), 1, "one for the storage authority");
    let applied = apply_repair(&state, &health, &selection, false).unwrap();
    assert_eq!(applied.revision, before.revision + 1);
    assert!(
        !has(&applied.result, codes::AUTHORITY_INCOMPLETE),
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
    quick_scan(&state, &health).unwrap();
    let applied = apply_repair(&state, &health, &authority_selection(&state), false).unwrap();

    let undone = undo_repair(&state, &health, &applied.journal_id).unwrap();
    assert_eq!(undone.revision, applied.revision + 1);
    assert!(undone.skipped.is_empty());
    assert!(
        has(&undone.result, codes::AUTHORITY_INCOMPLETE),
        "the library is what it was before the repair"
    );
    assert!(journals(&state).unwrap().is_empty());
}

#[test]
fn a_repair_selected_against_an_older_diagnosis_is_refused() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    quick_scan(&state, &health).unwrap();
    let selection = authority_selection(&state);

    let guard = state.admit_renderer_operation().unwrap();
    with_store_mutex_mut_admitted(&state, &guard, |store| {
        let staging = store.replace_begin()?;
        store.replace_put_root(&staging.staging_id, &serde_json::json!({}))?;
        store.replace_commit(&staging.staging_id, Some(0))?;
        Ok(())
    })
    .unwrap();
    drop(guard);

    let error = apply_repair(&state, &health, &selection, false).unwrap_err();
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
        apply_repair(&state, &health, &[], false).unwrap_err(),
        StoreError::Validation { .. }
    ));

    quick_scan(&state, &health).unwrap();
    assert!(matches!(
        apply_repair(&state, &health, &["0:nothing".to_owned()], false).unwrap_err(),
        StoreError::Validation { .. }
    ));
}

#[test]
fn the_repair_keeps_a_snapshot_of_the_library_it_was_selected_against() {
    let (_directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    quick_scan(&state, &health).unwrap();
    let applied = apply_repair(&state, &health, &authority_selection(&state), true).unwrap();
    let kept = applied.snapshot.expect("a snapshot was asked for");
    let guard = state.admit_renderer_operation().unwrap();
    let listed = with_store_mutex_admitted(&state, &guard, |store| store.snapshot_list()).unwrap();
    assert!(listed.iter().any(|snapshot| snapshot.id == kept));
}

#[test]
fn the_journal_and_the_diagnosis_stay_in_the_working_folder() {
    let (directory, state, _) = damaged_payload_fixture();
    let health = DataHealthState::default();
    quick_scan(&state, &health).unwrap();
    apply_repair(&state, &health, &authority_selection(&state), false).unwrap();

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
    quick_scan(&state, &health).unwrap();
    apply_repair(&state, &health, &authority_selection(&state), false).unwrap();

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
