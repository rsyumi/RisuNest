use super::*;
use std::fs;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

static PANIC_HOOK_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn startup_failure_diagnostic_is_redacted_and_bounded_without_a_data_root() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("startup.log");
    append_startup_diagnostic(&path, &format!("x-api-key: synthetic-secret\n{}", "detail ".repeat(1500)));
    let log = fs::read_to_string(path).unwrap();
    assert!(!log.contains("synthetic-secret"));
    assert_eq!(log.chars().count(), 4097);
}

#[test]
fn frontend_failures_reach_diagnostics_with_secrets_masked_and_bounded_text() {
    let state = NativeLogState::for_tests();
    state.record_frontend_error(&format!(
        "Backup import failed: x-api-key: secret-value\n{}",
        "한".repeat(3000)
    ));
    let entry = state.tail(None).pop().unwrap();
    assert_eq!(entry.level, "error");
    assert_eq!(entry.target, "webview");
    assert!(entry.message.starts_with("Backup import failed:"));
    assert!(!entry.message.contains("secret-value"));
    assert!(entry.message.chars().count() <= 2048);
}

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
        r#"payload={"authorization":"Custom fixture-json-auth","api_key":"fixture-json-key","access_token":"fixture-json-token","password":"fixture-json-password","secret":"fixture-json-secret","safe":"visible"} url=/path?token=fixture-query-token&x-api-key=fixture-query-key&password=fixture-query-password&secret=fixture-query-secret&safe=visible"#,
    );

    let masked = state.tail(Some(1)).pop().unwrap().message;
    assert_eq!(
        masked,
        r#"payload={"authorization":"***","api_key":"***","access_token":"***","password":"***","secret":"***","safe":"visible"} url=/path?token=***&x-api-key=***&password=***&secret=***&safe=visible"#
    );
}

#[test]
fn masks_plain_password_and_secret_values_without_hiding_surrounding_text() {
    let state = NativeLogState::for_tests();
    state.record(
        "info",
        "test",
        "database password is fixture-password while retry remains safe; secret=fixture-secret next safe",
    );

    assert_eq!(
        state.tail(Some(1)).pop().unwrap().message,
        "database password is *** while retry remains safe; secret=*** next safe"
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

struct SyntheticFailure {
    code: &'static str,
    detail: Option<String>,
    expected: bool,
}

impl CommandFailure for SyntheticFailure {
    fn code(&self) -> std::borrow::Cow<'_, str> {
        self.code.into()
    }
    fn detail(&self) -> Option<std::borrow::Cow<'_, str>> {
        self.detail.as_deref().map(Into::into)
    }
    fn expected(&self) -> bool {
        self.expected
    }
}

fn command_line(command: &str) -> LogEntry {
    global_state()
        .tail(None)
        .into_iter()
        .rev()
        .find(|entry| entry.message.starts_with(&format!("{command} failed: ")))
        .expect("the command failure is logged")
}

#[test]
fn a_failed_command_logs_one_masked_line_with_its_code_cause_and_caller() {
    let failure = SyntheticFailure {
        code: "synthetic-code",
        detail: Some(format!("disk refused password=synthetic-secret {}", "x ".repeat(1500))),
        expected: false,
    };
    let line = line!() + 1;
    let returned = logged::<(), _>("synthetic_failed_command", Err(failure));
    assert_eq!(returned.err().map(|error| error.code), Some("synthetic-code"));
    let entry = command_line("synthetic_failed_command");
    assert_eq!(entry.level, "error");
    assert_eq!(entry.target, COMMAND_TARGET);
    assert!(entry
        .message
        .starts_with("synthetic_failed_command failed: code=synthetic-code cause=disk refused password=*** x x"));
    assert!(!entry.message.contains("synthetic-secret"));
    assert!(entry.message.ends_with(&format!(" at={}:{line}", file!())));
    assert!(entry.message.chars().count() < 2300);
    assert_eq!(
        global_state()
            .tail(None)
            .iter()
            .filter(|entry| entry.message.starts_with("synthetic_failed_command failed: "))
            .count(),
        1
    );
}

#[test]
fn an_expected_refusal_is_a_warning_without_its_cause_and_success_logs_nothing() {
    let refusal = SyntheticFailure {
        code: "synthetic-busy",
        detail: Some("synthetic-hidden-detail".to_owned()),
        expected: true,
    };
    let _ = logged::<(), _>("synthetic_refused_command", Err(refusal));
    let entry = command_line("synthetic_refused_command");
    assert_eq!(entry.level, "warn");
    assert!(entry
        .message
        .starts_with("synthetic_refused_command failed: code=synthetic-busy at="));
    assert!(!entry.message.contains("synthetic-hidden-detail"));

    assert_eq!(logged::<_, SyntheticFailure>("synthetic_quiet_command", Ok(7)).ok(), Some(7));
    assert!(global_state()
        .tail(None)
        .iter()
        .all(|entry| !entry.message.starts_with("synthetic_quiet_command")));
}

#[test]
fn a_kept_failure_never_quotes_json_input_or_network_text() {
    let shape = serde_json::from_str::<u32>("\"private-payload-value\"").unwrap_err();
    let text = failure_text(&shape);
    assert!(text.starts_with("json failure at line 1 column"), "{text}");
    assert!(!text.contains("private-payload-value"));
    assert_eq!(failure_text(&&shape), text);
    assert_eq!(failure_text(&Box::new(shape)), text);
    let mut mutable_shape = serde_json::from_str::<u32>("\"private-payload-value\"").unwrap_err();
    assert_eq!(failure_text(&&mut mutable_shape), text);
    assert_eq!(json_failure(&mutable_shape), format!("json-shape{}", &text["json failure".len()..]));
    let network = reqwest::Client::new()
        .get("http://[private-signature")
        .build()
        .unwrap_err();
    assert_eq!(failure_text(&network), "network failure");
    assert_eq!(failure_text(&&network), "network failure");
    let io = std::io::Error::other("disk refused");
    assert_eq!(failure_text(&io), "disk refused");
}

#[test]
fn command_summaries_preserve_the_response_and_never_copy_private_detail() {
    let private = "synthetic private credential contents".to_owned();
    let returned = logged_without_detail::<(), _>("synthetic_private_command", Err(private.clone()));
    assert_eq!(returned.unwrap_err(), private);
    let entries = global_state().tail(None);
    let failures: Vec<_> = entries.iter().filter(|entry| entry.message.starts_with("synthetic_private_command failed: ")).collect();
    assert_eq!(failures.len(), 1);
    assert!(failures[0].message.contains("code=error at="));
    assert!(!failures[0].message.contains(&private));
    let _ = logged_without_detail::<(), _>("synthetic_private_refusal", Err("cancelled".to_owned()));
    assert_eq!(command_line("synthetic_private_refusal").level, "warn");
    assert_eq!(logged_without_detail::<_, String>("synthetic_private_success", Ok(7)).unwrap(), 7);
    assert!(global_state().tail(None).iter().all(|entry| !entry.message.starts_with("synthetic_private_success")));
}

#[test]
fn a_json_reader_failure_keeps_the_reader_text() {
    struct Refusing;
    impl std::io::Read for Refusing {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("synthetic restore cancelled"))
        }
    }
    let error = serde_json::from_reader::<_, serde_json::Value>(Refusing).unwrap_err();
    assert_eq!(json_failure(&error), format!("json-io: {error}"));
    assert!(json_failure(&error).contains("synthetic restore cancelled"));
}

#[test]
fn a_string_failure_is_logged_as_its_code_or_as_a_message() {
    let code = "cleanup-journal-unavailable".to_owned();
    assert_eq!((code.code().as_ref(), code.detail(), code.expected()), ("cleanup-journal-unavailable", None, false));
    let message = "CAS upload mutex poisoned: synthetic".to_owned();
    assert_eq!(message.code().as_ref(), "error");
    assert_eq!(message.detail().as_deref(), Some("CAS upload mutex poisoned: synthetic"));
    assert!(!message.expected());
    for message in ["Upper-case", "two words", "", "trailing-"] {
        assert_eq!(message.to_owned().code().as_ref(), "error", "{message}");
    }
    assert_eq!(&*"x".repeat(65).code(), "error");
    assert!("cancelled".to_owned().expected());
    assert!("library-operation-busy".to_owned().expected());
    assert!("cleanup-pending".to_owned().expected());
    assert!("native-media-busy".to_owned().expected());
    assert!(!"native-media-unavailable".to_owned().expected());
}

#[test]
fn a_logged_failure_never_names_the_temporary_file_path() {
    let root = tempfile::tempdir().unwrap();
    let error = tempfile::Builder::new()
        .tempfile_in(root.path().join("private-\"folder\"-name"))
        .unwrap_err();
    assert!(error.to_string().contains("private-"));
    let failure = SyntheticFailure {
        code: "synthetic-path",
        detail: Some(format!("{error}; retry {error}; kept")),
        expected: false,
    };
    let _ = logged::<(), _>("synthetic_path_command", Err(failure));
    let entry = command_line("synthetic_path_command");
    assert!(!entry.message.contains("private-"), "{}", entry.message);
    assert!(entry.message.contains("; retry "), "{}", entry.message);
    assert!(entry.message.contains("; kept at="), "{}", entry.message);
}

#[test]
fn unconfigured_file_path_command_returns_only_a_bounded_code() {
    let state = NativeLogState::for_tests();

    let error = file_path_for_command(&state).unwrap_err();

    assert_eq!(error, NATIVE_LOG_FILE_PATH_UNAVAILABLE);
    let entry = state.tail(Some(1)).pop().unwrap();
    assert_eq!(entry.target, "native_log");
    assert!(entry.message.contains("file path unavailable"));

    let error = file_page::read_page_for_command(&state, file_page::LogFile::Current, None).unwrap_err();
    assert_eq!(error, NATIVE_LOG_FILE_PATH_UNAVAILABLE);
    assert!(state.tail(Some(1)).pop().unwrap().message.contains("file read unavailable"));
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
fn panic_capture_masks_payload_and_includes_location() {
    let state = NativeLogState::for_tests();
    state.record_panic(
        Some("request failed with Authorization: Bearer fixture-panic-secret"),
        "file.rs",
        42,
        7,
    );
    let entry = state.tail(None).pop().unwrap();
    assert_eq!(entry.level, "panic");
    assert_eq!(
        entry.message,
        "panic captured at file.rs:42:7: request failed with Authorization: ***"
    );
    assert!(!entry.message.contains("fixture-panic-secret"));
}

#[test]
fn configured_panic_hook_writes_the_first_setup_panic_immediately() {
    let _hook_lock = PANIC_HOOK_TEST_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    let original = std::panic::take_hook();
    install_panic_hook_for(state.clone());
    let _ = std::panic::catch_unwind(|| panic!("Authorization: Bearer fixture-pre-setup-secret"));
    let installed = std::panic::take_hook();
    std::panic::set_hook(original);
    drop(installed);

    let log = fs::read_to_string(state.file_path()).unwrap();
    assert!(log.contains("panic captured at"));
    assert!(!log.contains("fixture-pre-setup-secret"));
}

#[test]
fn first_file_configuration_flushes_pending_entries_exactly_once() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::for_tests();
    state.record("info", "boot", "pending boot entry");

    state.configure_file_path(temp.path());
    state.configure_file_path(temp.path());

    let log = fs::read_to_string(state.file_path()).unwrap();
    assert_eq!(log.matches("pending boot entry").count(), 1);
}

#[test]
fn pending_file_entries_evict_the_oldest_at_ring_capacity() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::for_tests();
    for number in 0..=RING_CAPACITY {
        state.record("info", "boot", format!("pending-{number:04}"));
    }

    state.configure_file_path(temp.path());

    let log = fs::read_to_string(state.file_path()).unwrap();
    let lines = log.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), RING_CAPACITY);
    assert!(!log.contains("pending-0000"));
    for (line, expected) in lines.iter().zip(1..=RING_CAPACITY) {
        assert!(line.ends_with(&format!("pending-{expected:04}")));
    }
}

#[test]
fn file_disabled_marker_drops_pending_and_future_entries() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(logs.join(FILE_LOG_DISABLED_MARKER), b"disabled").unwrap();
    let state = NativeLogState::for_tests();
    state.record("info", "boot", "pending marker entry");

    state.configure_file_path(temp.path());
    state.record("info", "setup", "future marker entry");

    assert!(!state.file_path().exists());
}

#[test]
fn ring_only_entries_before_configuration_never_reach_the_file_sink() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::for_tests();
    state.record_ring_only("error", "native_log", "early command detail");

    state.configure_file_path(temp.path());

    assert_eq!(
        state.tail(Some(1)).pop().unwrap().message,
        "early command detail"
    );
    assert!(!state.file_path().exists());
}

#[test]
fn panic_hook_records_before_calling_the_previous_hook_exactly_once() {
    let _hook_lock = PANIC_HOOK_TEST_LOCK.lock().unwrap();
    let state = NativeLogState::for_tests();
    let original = std::panic::take_hook();
    let previous_calls = Arc::new(AtomicUsize::new(0));
    let previous_calls_by_hook = previous_calls.clone();
    let recorded_before_previous = Arc::new(AtomicBool::new(false));
    let recorded_before_previous_by_hook = recorded_before_previous.clone();
    let state_seen_by_hook = state.clone();
    std::panic::set_hook(Box::new(move |_| {
        recorded_before_previous_by_hook.store(
            state_seen_by_hook
                .tail(Some(1))
                .pop()
                .is_some_and(|entry| entry.message.contains("panic captured at")),
            Ordering::SeqCst,
        );
        previous_calls_by_hook.fetch_add(1, Ordering::SeqCst);
    }));
    install_panic_hook_for(state.clone());
    let _ = std::panic::catch_unwind(|| panic!("captured panic"));
    let installed = std::panic::take_hook();
    std::panic::set_hook(original);
    drop(installed);

    assert_eq!(previous_calls.load(Ordering::SeqCst), 1);
    assert!(recorded_before_previous.load(Ordering::SeqCst));
    assert!(state.tail(None).iter().any(|entry| {
        entry.level == "panic"
            && entry.message.contains("panic captured at")
            && entry.message.contains("tests.rs")
    }));
}

#[test]
fn repeated_panic_hook_initialization_does_not_duplicate_the_chain() {
    let _hook_lock = PANIC_HOOK_TEST_LOCK.lock().unwrap();
    let state = NativeLogState::for_tests();
    let original = std::panic::take_hook();
    let previous_calls = Arc::new(AtomicUsize::new(0));
    let previous_calls_by_hook = previous_calls.clone();
    std::panic::set_hook(Box::new(move |_| {
        previous_calls_by_hook.fetch_add(1, Ordering::SeqCst);
    }));
    let installed = std::sync::Once::new();

    install_panic_hook_once_for(&installed, state.clone());
    install_panic_hook_once_for(&installed, state.clone());
    let _ = std::panic::catch_unwind(|| panic!("one captured panic"));
    let installed_hook = std::panic::take_hook();
    std::panic::set_hook(original);
    drop(installed_hook);

    assert_eq!(previous_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        state
            .tail(None)
            .iter()
            .filter(|entry| entry.level == "panic")
            .count(),
        1
    );
}

#[test]
fn panic_hook_masks_sensitive_payload_in_native_log_while_preserving_previous_hook() {
    let _hook_lock = PANIC_HOOK_TEST_LOCK.lock().unwrap();
    let state = NativeLogState::for_tests();
    let original = std::panic::take_hook();
    let previous_calls = Arc::new(AtomicUsize::new(0));
    let previous_calls_by_hook = previous_calls.clone();
    std::panic::set_hook(Box::new(move |_| {
        previous_calls_by_hook.fetch_add(1, Ordering::SeqCst);
    }));
    install_panic_hook_for(state.clone());
    let _ = std::panic::catch_unwind(|| panic!("database password is fixture-panic-secret"));
    let installed = std::panic::take_hook();
    std::panic::set_hook(original);
    drop(installed);

    assert_eq!(previous_calls.load(Ordering::SeqCst), 1);
    let entry = state.tail(Some(1)).pop().unwrap();
    assert!(!entry.message.contains("fixture-panic-secret"));
    assert!(entry.message.contains("database password is ***"));
    assert!(entry.message.contains("panic captured at"));
}

#[test]
fn panic_hook_records_str_and_string_payloads_with_location() {
    let _hook_lock = PANIC_HOOK_TEST_LOCK.lock().unwrap();
    let state = NativeLogState::for_tests();
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    install_panic_hook_for(state.clone());

    let _ = std::panic::catch_unwind(|| panic!("literal panic context"));
    let _ = std::panic::catch_unwind(|| std::panic::panic_any(String::from("owned panic context")));

    let installed = std::panic::take_hook();
    std::panic::set_hook(original);
    drop(installed);

    let entries = state.tail(None);
    assert_eq!(entries.len(), 2);
    assert!(entries[0].message.contains("literal panic context"));
    assert!(entries[0].message.contains("tests.rs"));
    assert!(entries[1].message.contains("owned panic context"));
    assert!(entries[1].message.contains("tests.rs"));
}

#[test]
fn panic_hook_uses_a_safe_label_for_non_string_payloads() {
    let _hook_lock = PANIC_HOOK_TEST_LOCK.lock().unwrap();
    let state = NativeLogState::for_tests();
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    install_panic_hook_for(state.clone());

    let _ = std::panic::catch_unwind(|| std::panic::panic_any(42_u8));

    let installed = std::panic::take_hook();
    std::panic::set_hook(original);
    drop(installed);

    let entry = state.tail(Some(1)).pop().unwrap();
    assert!(entry.message.contains("non-string panic payload"));
    assert!(entry.message.contains("tests.rs"));
    assert!(!entry.message.contains("42"));
}

/// The longest line one entry can write: a capped message of four-byte characters.
fn longest_file_line(level: &str, target: &str) -> u64 {
    file_line(&LogEntry {
        ts_ms: u128::from(u64::MAX),
        level: level.to_owned(),
        target: target.to_owned(),
        message: "𝄞".repeat(MAX_MESSAGE_CHARS),
    })
    .len() as u64
}

#[test]
fn the_log_and_its_rotation_together_stay_within_twice_the_bound() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    let logs = temp.path().join("logs");
    let message = "𝄞".repeat(MAX_MESSAGE_CHARS + 100);
    let mut written = 0u64;
    let mut largest = 0u64;
    while written < 3 * FILE_ROTATE_BYTES as u64 {
        state.record("error", "synthetic", &message);
        written += longest_file_line("error", "synthetic");
        let sizes = [LOG_FILE_NAME, PREVIOUS_LOG_FILE_NAME]
            .map(|name| fs::metadata(logs.join(name)).map(|metadata| metadata.len()).unwrap_or(0));
        largest = largest.max(sizes[0] + sizes[1]);
    }

    let line = longest_file_line("error", "synthetic");
    assert!(logs.join(PREVIOUS_LOG_FILE_NAME).exists());
    assert!(largest <= 2 * (FILE_ROTATE_BYTES as u64 + line), "{largest}");
    let mut names = fs::read_dir(&logs)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, [LOG_FILE_NAME, PREVIOUS_LOG_FILE_NAME]);
}

#[test]
fn one_oversized_message_is_capped_before_the_ring_and_the_file() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());

    state.record("error", "synthetic", "가".repeat(1024 * 1024));

    assert_eq!(state.tail(Some(1))[0].message.chars().count(), MAX_MESSAGE_CHARS);
    assert!(fs::metadata(state.file_path()).unwrap().len() <= longest_file_line("error", "synthetic"));
}

#[test]
fn a_log_that_cannot_be_rotated_starts_over_instead_of_growing() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    let blocked = temp.path().join("logs").join(PREVIOUS_LOG_FILE_NAME);
    fs::create_dir_all(blocked.join("occupied")).unwrap();
    fs::write(state.file_path(), vec![b'x'; FILE_ROTATE_BYTES + 1]).unwrap();

    state.record("info", "test", "after a failed rotation");

    let log = fs::read_to_string(state.file_path()).unwrap();
    assert_eq!(log, file_line(&state.tail(Some(1))[0]));
}

#[test]
fn startup_failure_diagnostic_rotates_instead_of_growing() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("startup.log");
    let line = 4097u64 * 4;
    for _ in 0..64 {
        append_startup_diagnostic(&path, &"𝄞".repeat(5000));
    }

    let rotated = root.path().join("startup.log.1");
    assert!(rotated.exists());
    let total = fs::metadata(&path).unwrap().len() + fs::metadata(&rotated).unwrap().len();
    assert!(total <= 2 * (STARTUP_DIAGNOSTIC_ROTATE_BYTES + line), "{total}");
}

#[test]
fn the_app_run_marks_its_start_before_lines_recorded_earlier_once() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::for_tests();
    state.record("warn", "boot", "pending boot entry");

    state.configure_file_path_for_run(temp.path(), "9.9.9-synthetic");
    state.configure_file_path_for_run(temp.path(), "9.9.9-synthetic");
    state.record("info", "setup", "later entry");

    let log = fs::read_to_string(state.file_path()).unwrap();
    let entries = file_page::parse_entries(log.as_bytes());
    let summary = entries
        .iter()
        .map(|entry| (entry.level.as_str(), entry.target.as_str(), entry.message.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            ("info", RUN_START_TARGET, "RisuNest 9.9.9-synthetic"),
            ("warn", "boot", "pending boot entry"),
            ("info", "setup", "later entry"),
        ]
    );
    assert!(entries[0].ts_ms <= entries[1].ts_ms);
    assert!(state.tail(None).iter().all(|entry| entry.target != RUN_START_TARGET));
}

fn write_lines(path: &std::path::Path, entries: &[LogEntry]) {
    let text = entries.iter().map(file_line).collect::<String>();
    fs::write(path, text).unwrap();
}

fn synthetic_entries(prefix: &str, count: usize) -> Vec<LogEntry> {
    (0..count)
        .map(|index| LogEntry {
            ts_ms: 1_700_000_000_000 + index as u128,
            level: if index % 3 == 0 { "error" } else { "info" }.to_owned(),
            target: "risunest_lib::server_sync::commands".to_owned(),
            message: if index % 4 == 0 {
                format!("{prefix}-{index} first line\n  at frame [x]: detail\nlast")
            } else {
                format!("{prefix}-{index} [ok] value: {}", "v".repeat(index % 40))
            },
        })
        .collect()
}

#[test]
fn saved_log_pages_walk_back_through_both_files_without_splitting_entries() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    let previous = synthetic_entries("previous", 120);
    let current = synthetic_entries("current", 90);
    write_lines(&logs.join(PREVIOUS_LOG_FILE_NAME), &previous);
    write_lines(&logs.join(LOG_FILE_NAME), &current);
    let page_bytes = 300;

    let mut pages = Vec::new();
    let mut cursor = None;
    loop {
        let (file, before) = cursor.map_or((file_page::LogFile::Current, None), |cursor: file_page::FileCursor| (cursor.file, cursor.before));
        let page = file_page::read_page(&logs, file, before, page_bytes).unwrap();
        assert!(!page.entries.is_empty());
        let span: usize = page.entries.iter().map(|entry| file_line(entry).len()).sum();
        assert!(span <= page_bytes, "{span}");
        pages.push(page.entries);
        match page.older {
            Some(older) => cursor = Some(older),
            None => break,
        }
    }

    let read = pages.into_iter().rev().flatten().map(|entry| file_line(&entry)).collect::<Vec<_>>();
    let expected = previous.iter().chain(&current).map(file_line).collect::<Vec<_>>();
    assert_eq!(read, expected);
}

#[test]
fn a_saved_log_page_from_the_command_never_reads_more_than_its_bound() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    let logs = temp.path().join("logs");
    write_lines(&logs.join(LOG_FILE_NAME), &synthetic_entries("large", 6000));
    let size = fs::metadata(logs.join(LOG_FILE_NAME)).unwrap().len();
    assert!(size > 3 * file_page::FILE_PAGE_BYTES as u64);

    let page = file_page::read_page_for_command(&state, file_page::LogFile::Current, None).unwrap();

    let start = page.older.unwrap().before.unwrap();
    assert!(size - start <= file_page::FILE_PAGE_BYTES as u64);
    assert_eq!(page.entries.last().unwrap().message, synthetic_entries("large", 6000)[5999].message);
}

#[test]
fn a_cursor_from_before_a_rotation_continues_in_the_previous_file() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    let entries = synthetic_entries("rotated", 40);
    write_lines(&logs.join(LOG_FILE_NAME), &entries);
    let first = file_page::read_page(&logs, file_page::LogFile::Current, None, 400).unwrap();
    let older = first.older.unwrap();

    fs::rename(logs.join(LOG_FILE_NAME), logs.join(PREVIOUS_LOG_FILE_NAME)).unwrap();
    fs::write(logs.join(LOG_FILE_NAME), file_line(&synthetic_entries("new", 1)[0])).unwrap();
    let second = file_page::read_page(&logs, older.file, older.before, 400).unwrap();

    let next = second.entries.last().unwrap();
    let expected = &entries[entries.len() - first.entries.len() - 1];
    assert_eq!(file_line(next), file_line(expected));
    assert_eq!(second.older.unwrap().file, file_page::LogFile::Previous);
}

#[test]
fn missing_or_empty_saved_logs_read_as_an_empty_page_or_the_previous_file() {
    let temp = tempfile::tempdir().unwrap();
    let logs = temp.path().join("logs");
    fs::create_dir_all(&logs).unwrap();

    let page = file_page::read_page(&logs, file_page::LogFile::Current, None, 400).unwrap();
    assert!(page.entries.is_empty());
    assert_eq!(page.older, None);

    fs::write(logs.join(LOG_FILE_NAME), b"").unwrap();
    write_lines(&logs.join(PREVIOUS_LOG_FILE_NAME), &synthetic_entries("previous", 2));
    let page = file_page::read_page(&logs, file_page::LogFile::Current, None, 4000).unwrap();
    assert_eq!(page.entries.len(), 2);
    assert_eq!(page.older, None);
}

#[test]
fn saved_lines_parse_back_into_the_entries_that_wrote_them() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    let messages = [
        "plain",
        "",
        "colon: inside [brackets] and ] closing",
        "first line\n1700000000000 [info] lookalike: continued\nthird",
    ];
    for message in messages {
        state.record("warn", "risunest_lib::native_log", message);
    }

    let page = file_page::read_page(&temp.path().join("logs"), file_page::LogFile::Current, None, 4000).unwrap();
    let ring = state.tail(None);
    assert_eq!(page.entries.len(), 5);
    for (read, recorded) in page.entries.iter().zip(&ring).take(3) {
        assert_eq!((read.ts_ms, &read.level, &read.target, &read.message), (recorded.ts_ms, &recorded.level, &recorded.target, &recorded.message));
    }
    // A continuation line that looks like an entry is read as one; nothing is lost.
    assert_eq!(page.entries[3].message, "first line");
    assert_eq!(page.entries[4].target, "lookalike");
    assert_eq!(page.entries[4].message, "continued\nthird");
}

fn exported(state: &NativeLogState) -> Vec<u8> {
    let mut output = Vec::new();
    let log = state.open_whole_log().unwrap();
    let bytes = log.copy_to(&mut output).unwrap();
    assert_eq!(bytes, output.len() as u64);
    output
}

fn file_text(entries: &[LogEntry]) -> Vec<u8> {
    entries.iter().map(file_line).collect::<String>().into_bytes()
}

#[test]
fn the_whole_saved_log_is_the_previous_file_then_the_current_one() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    let logs = temp.path().join("logs");
    let previous = synthetic_entries("previous", 30);
    let current = synthetic_entries("current", 20);
    write_lines(&logs.join(PREVIOUS_LOG_FILE_NAME), &previous);
    write_lines(&logs.join(LOG_FILE_NAME), &current);

    let log = state.open_whole_log().unwrap();
    // A line written after the save starts is not part of it.
    state.record("info", "later", "written while saving");
    let mut output = Vec::new();
    log.copy_to(&mut output).unwrap();

    assert_eq!(output, [file_text(&previous), file_text(&current)].concat());
}

#[test]
fn the_whole_saved_log_is_the_one_file_that_exists_or_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    let logs = temp.path().join("logs");
    assert_eq!(state.open_whole_log().unwrap().bytes(), 0);
    assert_eq!(exported(&state), b"");

    let current = synthetic_entries("current", 5);
    write_lines(&logs.join(LOG_FILE_NAME), &current);
    assert_eq!(exported(&state), file_text(&current));

    let previous = synthetic_entries("previous", 5);
    fs::remove_file(logs.join(LOG_FILE_NAME)).unwrap();
    write_lines(&logs.join(PREVIOUS_LOG_FILE_NAME), &previous);
    assert_eq!(exported(&state), file_text(&previous));
    fs::write(logs.join(LOG_FILE_NAME), b"").unwrap();
    assert_eq!(state.open_whole_log().unwrap().bytes(), file_text(&previous).len() as u64);
    assert_eq!(exported(&state), file_text(&previous));

    assert_eq!(
        NativeLogState::for_tests().open_whole_log().err(),
        Some(NATIVE_LOG_FILE_PATH_UNAVAILABLE)
    );
}

#[test]
fn a_rotation_during_the_save_keeps_the_files_it_opened() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    let logs = temp.path().join("logs");
    let previous = synthetic_entries("previous", 50);
    write_lines(&logs.join(PREVIOUS_LOG_FILE_NAME), &previous);
    let current = [file_text(&synthetic_entries("current", 50)), vec![b'x'; FILE_ROTATE_BYTES], b"\n".to_vec()].concat();
    fs::write(logs.join(LOG_FILE_NAME), &current).unwrap();

    let log = state.open_whole_log().unwrap();
    state.record("info", "rotation", "first line of the next file");
    assert_eq!(fs::read(logs.join(PREVIOUS_LOG_FILE_NAME)).unwrap(), current);
    let mut output = Vec::new();
    log.copy_to(&mut output).unwrap();

    assert!(output == [file_text(&previous), current].concat(), "{} bytes", output.len());
}

#[test]
fn a_file_that_ends_mid_line_does_not_join_the_next_file() {
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    let logs = temp.path().join("logs");
    fs::write(logs.join(PREVIOUS_LOG_FILE_NAME), b"1700000000000 [error] cut: interrupted").unwrap();
    let current = synthetic_entries("current", 2);
    write_lines(&logs.join(LOG_FILE_NAME), &current);

    let output = exported(&state);

    assert_eq!(output, [b"1700000000000 [error] cut: interrupted\n".to_vec(), file_text(&current)].concat());
    assert_eq!(file_page::parse_entries(&output).len(), 3);
}

#[test]
fn the_whole_saved_log_is_copied_a_bounded_piece_at_a_time() {
    struct Pieces { bytes: usize, largest: usize }
    impl std::io::Write for Pieces {
        fn write(&mut self, piece: &[u8]) -> std::io::Result<usize> {
            self.bytes += piece.len();
            self.largest = self.largest.max(piece.len());
            Ok(piece.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let temp = tempfile::tempdir().unwrap();
    let state = NativeLogState::initialize(temp.path());
    let logs = temp.path().join("logs");
    let size = 3 * file_export::COPY_BUFFER_BYTES + 17;
    fs::write(logs.join(PREVIOUS_LOG_FILE_NAME), vec![b'p'; size]).unwrap();
    fs::write(logs.join(LOG_FILE_NAME), vec![b'c'; size]).unwrap();

    let mut pieces = Pieces { bytes: 0, largest: 0 };
    state.open_whole_log().unwrap().copy_to(&mut pieces).unwrap();

    // The previous file ends mid-line, so one newline joins the two.
    assert_eq!(pieces.bytes, 2 * size + 1);
    assert!(pieces.largest <= file_export::COPY_BUFFER_BYTES, "{}", pieces.largest);
}
