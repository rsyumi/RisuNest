use super::*;
use std::fs;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[test]
fn keeps_a_fifo_ring_and_returns_the_newest_tail() {
    let state = NativeLogState::for_tests();
    for number in 0..=1_000 {
        state.record("info", "test", format!("entry {number}"));
    }
    let entries = state.tail(None);
    assert_eq!(entries.len(), 1_000);
    assert_eq!(entries.first().unwrap().message, "entry 1");
    assert_eq!(state.tail(Some(2))[0].message, "entry 999");
    assert_eq!(state.tail(Some(2))[1].message, "entry 1000");
}

#[test]
fn serializes_timestamp_and_masks_sensitive_values_before_every_sink() {
    let state = NativeLogState::for_tests();
    state.record("error", "test", "Authorization: Bearer secret-token\nx-api-key: key-value\nsk-abcdefghijklmnopqrstuvwxyz\n0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    let entry = state.tail(None).pop().unwrap();
    assert!(entry.ts_ms > 0);
    assert_eq!(entry.level, "error");
    assert_eq!(entry.target, "test");
    assert!(!entry.message.contains("secret-token"));
    assert!(!entry.message.contains("key-value"));
    assert!(!entry.message.contains("sk-abcdefghijklmnopqrstuvwxyz"));
    assert!(!entry.message.contains("0123456789abcdef"));
    assert_eq!(
        entry.message,
        "Authorization: ***\nx-api-key: ***\n***\n***"
    );
    assert!(serde_json::to_value(entry).unwrap()["tsMs"].is_number());
}

#[test]
fn masks_sk_secrets_only_at_token_boundaries() {
    let state = NativeLogState::for_tests();
    let secret = "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789";
    state.record("info", "test", format!("token {secret}"));
    let entry = state.tail(None).pop().unwrap();
    assert_eq!(entry.message, "token ***");

    let ordinary = "ask-me-later desk-chair prefixsk-proj-abcdefghijklmnopqrstuvwxyz0123456789";
    state.record("info", "test", ordinary);
    assert_eq!(state.tail(None).pop().unwrap().message, ordinary);
}

#[test]
fn masks_complete_authorization_values_for_every_scheme_case_insensitively() {
    let state = NativeLogState::for_tests();
    let cases = [
        (
            "Authorization: Bearer fixture-bearer-material\r\nmethod=GET",
            "Authorization: ***\r\nmethod=GET",
        ),
        (
            "authorization: Basic fixture-basic-material\r\nmethod=GET",
            "authorization: ***\r\nmethod=GET",
        ),
        (
            "AUTHORIZATION: Custom fixture-custom-material\r\nmethod=GET",
            "AUTHORIZATION: ***\r\nmethod=GET",
        ),
    ];

    for (message, expected) in cases {
        state.record("info", "test", message);
        let masked = state.tail(Some(1)).pop().unwrap().message;
        assert_eq!(masked, expected);
    }
}

#[test]
fn uses_the_exact_spec_replacement_for_bearer_x_api_key_and_long_tokens() {
    let state = NativeLogState::for_tests();
    state.record(
        "info",
        "test",
        "Bearer short-secret, x-api-key: second-secret\n0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    );

    assert_eq!(
        state.tail(Some(1)).pop().unwrap().message,
        "Bearer ***, x-api-key: ***\n***"
    );
}

#[test]
fn masks_json_and_query_secret_values_without_hiding_safe_fields() {
    let state = NativeLogState::for_tests();
    state.record(
        "info",
        "test",
        r#"payload={"authorization":"Custom fixture-json-auth","api_key":"fixture-json-key","access_token":"fixture-json-token","safe":"visible"} url=/path?token=fixture-query-token&x-api-key=fixture-query-key&safe=visible"#,
    );

    let masked = state.tail(Some(1)).pop().unwrap().message;
    assert_eq!(
        masked,
        r#"payload={"authorization":"***","api_key":"***","access_token":"***","safe":"visible"} url=/path?token=***&x-api-key=***&safe=visible"#
    );
}

#[test]
fn formats_console_output_only_after_masking() {
    let line = format_console_line(
        "error",
        "native_log",
        "Authorization: Basic fixture-console-secret\r\nmethod=GET",
    );

    assert_eq!(line, "[error] native_log: Authorization: ***\r\nmethod=GET");
}

#[test]
fn ring_only_entries_never_reach_the_file_sink() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());

    state.record_ring_only("error", "native_log", "command detail");

    assert_eq!(state.tail(Some(1)).pop().unwrap().message, "command detail");
    assert!(!state.file_path().exists());
}

#[test]
fn marker_absence_enables_file_logging_and_toggling_reverses_that() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    assert_eq!(
        state.file_path(),
        temp.path().join("logs").join("risunest.log")
    );
    assert!(state.file_enabled());
    state.set_file_enabled(false).unwrap();
    assert!(!state.file_enabled());
    assert!(temp
        .path()
        .join("logs")
        .join(FILE_LOG_DISABLED_MARKER)
        .exists());
    state.set_file_enabled(true).unwrap();
    assert!(state.file_enabled());
    assert!(!temp
        .path()
        .join("logs")
        .join(FILE_LOG_DISABLED_MARKER)
        .exists());
}

#[test]
fn command_failures_log_native_detail_and_return_only_a_bounded_code() {
    let temp = tempfile::tempdir().unwrap();
    let invalid_root = temp.path().join("not-a-directory");
    fs::write(&invalid_root, b"file").unwrap();
    let state = NativeLogState::initialize(&invalid_root);

    let error = set_file_enabled_for_command(&state, false).unwrap_err();

    assert_eq!(error, NATIVE_LOG_FILE_UPDATE_FAILED);
    let entry = state.tail(Some(1)).pop().unwrap();
    assert_eq!(entry.target, "native_log");
    assert!(entry.message.contains("file logging update failed"));
    assert!(!entry.message.contains(NATIVE_LOG_FILE_UPDATE_FAILED));
}

#[test]
fn unconfigured_file_path_command_returns_only_a_bounded_code() {
    let state = NativeLogState::for_tests();

    let error = file_path_for_command(&state).unwrap_err();

    assert_eq!(error, NATIVE_LOG_FILE_PATH_UNAVAILABLE);
    let entry = state.tail(Some(1)).pop().unwrap();
    assert_eq!(entry.target, "native_log");
    assert!(entry.message.contains("file path unavailable"));
}

#[cfg(unix)]
#[test]
fn diagnostic_directory_and_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::set_permissions(&logs, fs::Permissions::from_mode(0o755)).unwrap();
    let log_path = logs.join("risunest.log");
    fs::write(&log_path, b"existing").unwrap();
    fs::set_permissions(&log_path, fs::Permissions::from_mode(0o644)).unwrap();

    let state = NativeLogState::initialize(temp.path());
    state.record("info", "test", "permission check");

    assert_eq!(
        fs::metadata(&logs).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(state.file_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600,
    );
}

#[test]
fn does_not_rotate_at_the_exact_two_mib_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    fs::write(state.file_path(), vec![b'x'; FILE_ROTATE_BYTES]).unwrap();
    state.record("info", "test", "at boundary");
    assert!(!temp.path().join("logs").join("risunest.log.1").exists());
    assert!(fs::metadata(state.file_path()).unwrap().len() > FILE_ROTATE_BYTES as u64);
}

#[test]
fn rotates_the_file_only_above_two_mib() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    fs::write(state.file_path(), vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
    state.record("info", "test", "after rotation");
    assert_eq!(
        state.file_path(),
        temp.path().join("logs").join("risunest.log")
    );
    assert!(temp.path().join("logs").join("risunest.log.1").exists());
    assert!(fs::read_to_string(state.file_path())
        .unwrap()
        .contains("after rotation"));
}

#[test]
fn file_write_failure_is_nonfatal_when_log_root_is_a_file() {
    let temp = tempfile::tempdir().unwrap();
    let invalid_root = temp.path().join("not-a-directory");
    fs::write(&invalid_root, b"file").unwrap();
    let state = NativeLogState::initialize(&invalid_root);
    state.record("info", "test", "still nonfatal");
    assert_eq!(state.tail(None).len(), 1);
    assert!(!state.file_path().exists());
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
fn panic_hook_captures_the_payload_without_forwarding_the_raw_previous_hook() {
    let state = NativeLogState::for_tests();
    let original = std::panic::take_hook();
    let previous_called = Arc::new(AtomicBool::new(false));
    let previous_called_by_hook = previous_called.clone();
    std::panic::set_hook(Box::new(move |_| {
        previous_called_by_hook.store(true, Ordering::SeqCst);
    }));
    install_panic_hook_for(state.clone());
    let _ = std::panic::catch_unwind(|| panic!("captured panic"));
    let installed = std::panic::take_hook();
    std::panic::set_hook(original);
    drop(installed);

    assert!(!previous_called.load(Ordering::SeqCst));
    assert!(state.tail(None).iter().any(|entry| {
        entry.level == "panic"
            && entry.message.contains("captured panic")
            && entry.message.contains("tests.rs")
    }));
}

#[test]
fn panic_hook_does_not_forward_sensitive_payloads_to_the_previous_hook() {
    let state = NativeLogState::for_tests();
    let original = std::panic::take_hook();
    let previous_called = Arc::new(AtomicBool::new(false));
    let previous_called_by_hook = previous_called.clone();
    std::panic::set_hook(Box::new(move |_| {
        previous_called_by_hook.store(true, Ordering::SeqCst);
    }));
    install_panic_hook_for(state.clone());
    let _ = std::panic::catch_unwind(|| panic!("Authorization: Bearer fixture-panic-secret"));
    let installed = std::panic::take_hook();
    std::panic::set_hook(original);
    drop(installed);

    assert!(!previous_called.load(Ordering::SeqCst));
    let entry = state.tail(Some(1)).pop().unwrap();
    assert!(!entry.message.contains("fixture-panic-secret"));
    assert!(entry.message.contains("Authorization: ***"));
}
