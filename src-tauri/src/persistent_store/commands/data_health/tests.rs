use super::*;
use crate::data_health::{codes, result_path, Severity};
use crate::persistent_store::{active_generation, PersistentStore};
use sha2::{Digest, Sha256};
use std::fs;
use std::sync::Mutex;
use tempfile::{tempdir, TempDir};

/// A store with one registered object whose stored bytes no longer hash to the registration.
/// Only a reread can see that, so the two depths differ on exactly this fixture.
fn damaged_payload_fixture() -> (TempDir, PersistentStoreState, String) {
    let directory = tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
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
