use super::*;
use std::fs;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[test]
fn keeps_a_fifo_ring_and_returns_the_newest_tail() {
    let state = NativeLogState::for_tests();
    for number in 0..=RING_CAPACITY {
        state.record("info", "test", format!("entry {number}"));
    }
    let entries = state.tail(None);
    assert_eq!(entries.len(), RING_CAPACITY);
    assert_eq!(entries.first().unwrap().message, "entry 1");
    assert_eq!(state.tail(Some(2))[0].message, "entry 1999");
    assert_eq!(state.tail(Some(2))[1].message, "entry 2000");
}

#[test]
fn serializes_timestamp_and_masks_sensitive_values_before_every_sink() {
    let state = NativeLogState::for_tests();
    state.record("error", "test", "Authorization: Bearer secret-token x-api-key: key-value sk-abcdefghijklmnopqrstuvwxyz 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    let entry = state.tail(None).pop().unwrap();
    assert!(entry.ts_ms > 0);
    assert_eq!(entry.level, "error");
    assert_eq!(entry.target, "test");
    assert!(!entry.message.contains("secret-token"));
    assert!(!entry.message.contains("key-value"));
    assert!(!entry.message.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    assert!(!entry.message.contains("0123456789abcdef"));
    assert!(serde_json::to_value(entry).unwrap()["tsMs"].is_number());
}

#[test]
fn marker_absence_enables_file_logging_and_toggling_reverses_that() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    assert!(state.file_enabled());
    state.set_file_enabled(false).unwrap();
    assert!(!state.file_enabled());
    assert!(temp.path().join(FILE_LOG_DISABLED_MARKER).exists());
    state.set_file_enabled(true).unwrap();
    assert!(state.file_enabled());
    assert!(!temp.path().join(FILE_LOG_DISABLED_MARKER).exists());
}

#[test]
fn rotates_the_file_at_five_mib_and_ignores_file_write_failures() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    fs::write(state.file_path(), vec![b'x'; FILE_ROTATE_BYTES]).unwrap();
    state.record("info", "test", "after rotation");
    assert!(state.file_path().with_extension("log.1").exists());
    assert!(fs::read_to_string(state.file_path())
        .unwrap()
        .contains("after rotation"));
    let unwritable = NativeLogState::initialize(temp.path().join("not-a-directory"));
    unwritable.record("info", "test", "still nonfatal");
    assert_eq!(unwritable.tail(None).len(), 1);
}

#[test]
fn panic_capture_includes_payload_and_location() {
    let state = NativeLogState::for_tests();
    state.record_panic("panic payload", "file.rs", 42, 7);
    let entry = state.tail(None).pop().unwrap();
    assert_eq!(entry.level, "panic");
    assert!(entry.message.contains("panic payload"));
    assert!(entry.message.contains("file.rs:42:7"));
}

#[test]
fn panic_hook_captures_the_payload_and_preserves_the_previous_hook() {
    let state = NativeLogState::for_tests();
    let previous = std::panic::take_hook();
    let previous_called = Arc::new(AtomicBool::new(false));
    let previous_called_by_hook = previous_called.clone();
    std::panic::set_hook(Box::new(move |_| {
        previous_called_by_hook.store(true, Ordering::SeqCst);
    }));
    install_panic_hook_for(state.clone());
    let _ = std::panic::catch_unwind(|| panic!("captured panic"));
    let installed = std::panic::take_hook();
    std::panic::set_hook(previous);

    assert!(previous_called.load(Ordering::SeqCst));
    assert!(state.tail(None).iter().any(|entry| {
        entry.level == "panic"
            && entry.message.contains("captured panic")
            && entry.message.contains("tests.rs")
    }));
    drop(installed);
}
