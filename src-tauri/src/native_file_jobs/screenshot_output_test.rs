use super::screenshot_output::{
    ScreenshotOutputCancelOutcome, ScreenshotOutputState, MAX_SCREENSHOT_OUTPUT_APPEND_BYTES,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Cursor, Write};
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
    let started = state.start(destination).unwrap();

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
    let started = state.start(destination.clone()).unwrap();
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
    let started = state.start(destination.clone()).unwrap();
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
    let started = state.start(destination.clone()).unwrap();
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
    let started = state.start(destination.clone()).unwrap();
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
