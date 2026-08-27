use super::screenshot_output::{
    initialize_root_at, screenshot_spool_fingerprint_controlled, ScreenshotOutputCancelOutcome,
    ScreenshotOutputState, MAX_SCREENSHOT_OUTPUT_APPEND_BYTES, READY_HANDOFF_STALE_AFTER,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Cursor, Write};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

fn fixture() -> (TempDir, ScreenshotOutputState, std::path::PathBuf) {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("screenshot-output");
    let chosen = directory.path().join("chosen");
    fs::create_dir_all(&chosen).unwrap();
    let destination = chosen.join("chat.zip");
    let state = ScreenshotOutputState::initialize(root);
    (directory, state, destination)
}

fn screenshot_zip(pages: &[&[u8]]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (index, page) in pages.iter().enumerate() {
        writer
            .start_file(format!("page-{:04}.png", index + 1), options)
            .unwrap();
        writer.write_all(page).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn append_accepts_64_kib_and_rejects_larger_ipc_chunks() {
    let (_directory, state, destination) = fixture();
    let started = state.start(Some(destination)).unwrap();

    state
        .append(
            &started.job_id,
            &vec![1; MAX_SCREENSHOT_OUTPUT_APPEND_BYTES],
        )
        .unwrap();
    let error = state
        .append(
            &started.job_id,
            &vec![2; MAX_SCREENSHOT_OUTPUT_APPEND_BYTES + 1],
        )
        .unwrap_err();

    assert_eq!(error.code, "invalid-input");
    assert_eq!(
        state.cancel(&started.job_id).unwrap(),
        ScreenshotOutputCancelOutcome::Requested
    );
}

#[test]
fn publish_replaces_the_destination_only_after_the_complete_spool_is_synced() {
    let (_directory, state, destination) = fixture();
    fs::write(&destination, b"previous screenshot").unwrap();
    let started = state.start(Some(destination.clone())).unwrap();
    let expected = screenshot_zip(&[b"first page", b"second page"]);
    let split = expected.len() / 2;

    state.append(&started.job_id, &expected[..split]).unwrap();
    state.append(&started.job_id, &expected[split..]).unwrap();
    let published = state.publish(&started.job_id).unwrap();

    assert_eq!(fs::read(destination).unwrap(), expected);
    assert_eq!(published.bytes, expected.len() as u64);
    assert_eq!(published.sha256, hex::encode(Sha256::digest(&expected)));
    assert_eq!(
        state.cancel(&started.job_id).unwrap(),
        ScreenshotOutputCancelOutcome::Missing
    );
}

#[test]
fn cancellation_preserves_an_existing_destination_and_removes_owned_spool_files() {
    let (directory, state, destination) = fixture();
    fs::write(&destination, b"previous screenshot").unwrap();
    let started = state.start(Some(destination.clone())).unwrap();
    state.append(&started.job_id, b"partial zip").unwrap();

    assert_eq!(
        state.cancel(&started.job_id).unwrap(),
        ScreenshotOutputCancelOutcome::Requested
    );

    assert_eq!(fs::read(destination).unwrap(), b"previous screenshot");
    assert!(fs::read_dir(directory.path().join("screenshot-output"))
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn invalid_zip_never_replaces_an_existing_destination() {
    let (_directory, state, destination) = fixture();
    fs::write(&destination, b"previous screenshot").unwrap();
    let started = state.start(Some(destination.clone())).unwrap();
    state.append(&started.job_id, b"not a zip").unwrap();

    let error = state.publish(&started.job_id).unwrap_err();

    assert_eq!(error.code, "invalid-input");
    assert_eq!(fs::read(destination).unwrap(), b"previous screenshot");
    assert_eq!(
        state.cancel(&started.job_id).unwrap(),
        ScreenshotOutputCancelOutcome::Missing
    );
}

#[test]
fn startup_recovery_removes_only_owned_incomplete_screenshot_jobs() {
    let (directory, state, destination) = fixture();
    let started = state.start(Some(destination.clone())).unwrap();
    state.append(&started.job_id, b"partial zip").unwrap();
    let root = directory.path().join("screenshot-output");
    let unowned = root.join("user-data");
    fs::create_dir_all(&unowned).unwrap();
    fs::write(unowned.join("keep.txt"), b"keep").unwrap();
    drop(state);

    let recovered = ScreenshotOutputState::initialize(root);

    assert!(unowned.join("keep.txt").is_file());
    assert_eq!(
        recovered.cancel(&started.job_id).unwrap(),
        ScreenshotOutputCancelOutcome::Missing
    );
    assert!(!destination.exists());
}

#[test]
fn fingerprint_stops_between_bounded_reads_when_cancellation_is_requested() {
    let directory = TempDir::new().unwrap();
    let source = directory.path().join("large.zip");
    fs::write(&source, vec![1u8; MAX_SCREENSHOT_OUTPUT_APPEND_BYTES * 3]).unwrap();
    let checks = std::cell::Cell::new(0usize);

    let error = screenshot_spool_fingerprint_controlled(&source, || {
        checks.set(checks.get() + 1);
        checks.get() > 1
    })
    .unwrap_err();

    assert_eq!(error.code, "cancelled");
    assert_eq!(checks.get(), 2);
}

#[test]
fn android_handoff_keeps_one_validated_owned_zip_until_release() {
    let (directory, state, _destination) = fixture();
    let started = state.start(None).unwrap();
    let expected = screenshot_zip(&[b"first page", b"second page"]);
    state.append(&started.job_id, &expected).unwrap();

    let prepared = state.publish(&started.job_id).unwrap();
    let source = prepared.source_path.clone().unwrap();

    assert_eq!(fs::read(&source).unwrap(), expected);
    assert_eq!(prepared.bytes, expected.len() as u64);
    assert!(directory
        .path()
        .join("screenshot-output")
        .join(&started.job_id)
        .join("ready")
        .is_file());

    state.release(&started.job_id).unwrap();
    assert!(!std::path::Path::new(&source).exists());
}

#[test]
fn startup_preserves_only_ready_android_handoffs_and_release_reclaims_them() {
    let (directory, state, _destination) = fixture();
    let ready = state.start(None).unwrap();
    state
        .append(&ready.job_id, &screenshot_zip(&[b"ready page"]))
        .unwrap();
    let ready_source = state.publish(&ready.job_id).unwrap().source_path.unwrap();
    let incomplete = state.start(None).unwrap();
    state.append(&incomplete.job_id, b"partial zip").unwrap();
    let root = directory.path().join("screenshot-output");
    drop(state);

    let recovered = ScreenshotOutputState::initialize(root);

    assert!(std::path::Path::new(&ready_source).is_file());
    assert!(!directory
        .path()
        .join("screenshot-output")
        .join(&incomplete.job_id)
        .exists());
    recovered.release(&ready.job_id).unwrap();
    assert!(!std::path::Path::new(&ready_source).exists());
}

#[test]
fn startup_reclaims_only_stale_ready_android_handoffs() {
    let (directory, state, _destination) = fixture();
    let ready = state.start(None).unwrap();
    state
        .append(&ready.job_id, &screenshot_zip(&[b"ready page"]))
        .unwrap();
    let ready_source = state.publish(&ready.job_id).unwrap().source_path.unwrap();
    let root = directory.path().join("screenshot-output");
    drop(state);

    initialize_root_at(
        &root,
        SystemTime::now() + READY_HANDOFF_STALE_AFTER + Duration::from_secs(1),
    )
    .unwrap();

    assert!(!std::path::Path::new(&ready_source).exists());
}

#[test]
fn cancelling_a_ready_android_handoff_removes_its_owned_source() {
    let (_directory, state, _destination) = fixture();
    let started = state.start(None).unwrap();
    state
        .append(&started.job_id, &screenshot_zip(&[b"page"]))
        .unwrap();
    let source = state.publish(&started.job_id).unwrap().source_path.unwrap();

    assert_eq!(
        state.cancel(&started.job_id).unwrap(),
        ScreenshotOutputCancelOutcome::Requested
    );
    assert!(!std::path::Path::new(&source).exists());
}
