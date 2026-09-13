use super::*;
use sha2::{Digest, Sha256};
use tauri::Manager;

const SECTION: &str = "local-storage";
const METADATA: &str = r#"{"present":true,"name":"00610000d800"}"#;

fn state(root: &Path) -> DeviceBackupState {
    let state = DeviceBackupState::initialize(root.join("device-backup"));
    let pds = crate::persistent_store::commands::PersistentStoreState::default();
    state
        .attach_maintenance_guard(pds.acquire_device_maintenance().unwrap())
        .unwrap();
    state
}

fn session(state: &DeviceBackupState, operation: Operation, library: bool) -> String {
    let id = state
        .create_session(
            "synthetic-job",
            operation,
            library,
            &[SECTION.into()],
            Some("old-generation".into()),
            if library {
                Some("new-generation".into())
            } else {
                None
            },
        )
        .unwrap();
    state.main_document_started();
    state.main_document_finished();
    state.bootstrap_for_entry(true).unwrap();
    id
}

fn section(state: &DeviceBackupState, id: &str, spool: Spool, value: &str) -> SectionManifest {
    state.section_begin(id, spool, SECTION, METADATA).unwrap();
    state.row_append(id, spool, SECTION, 0, value).unwrap();
    state.section_finish(id, spool, SECTION).unwrap()
}

fn prepared(state: &DeviceBackupState, id: &str) -> (SectionManifest, SectionManifest) {
    let source = section(
        state,
        id,
        Spool::Source,
        r#"{"key":"d800","value":"source"}"#,
    );
    state.source_ready(id).unwrap();
    let rollback = section(
        state,
        id,
        Spool::Rollback,
        r#"{"key":"d800","value":"rollback"}"#,
    );
    state.prepared(id).unwrap();
    state.allow_device_apply(id).unwrap();
    (source, rollback)
}

fn complete(state: &DeviceBackupState, id: &str, spool: Spool, manifest: &SectionManifest) {
    state
        .section_intent(id, SECTION, spool == Spool::Rollback)
        .unwrap();
    state
        .section_complete(id, SECTION, spool == Spool::Rollback, &manifest.sha256)
        .unwrap();
}

fn write_marker(root: &Path, key: &str, marker: &CommitMarker) {
    std::fs::create_dir_all(root.join("persistent")).unwrap();
    let connection = Connection::open(root.join("persistent/persistent.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS app_kv(key TEXT PRIMARY KEY,value TEXT NOT NULL)",
        )
        .unwrap();
    connection
        .execute(
            "INSERT OR REPLACE INTO app_kv VALUES(?1,?2)",
            params![key, serde_json::to_string(marker).unwrap()],
        )
        .unwrap();
}

#[test]
fn cleanup_retries_after_deletion_and_never_removes_a_pending_session() {
    let root = tempfile::tempdir().unwrap();
    let original = state(root.path());
    let id = session(&original, Operation::Capture, false);
    let manifest = section(&original, &id, Spool::Source, r#"{"value":"synthetic"}"#);
    assert!(original.cleanup(&id).is_err());
    assert_eq!(
        original.section_page(&id, Spool::Source, None, 1).unwrap()[0].sha256,
        manifest.sha256
    );
    original.finish_device(&id).unwrap();
    original.confirm_capture(&id).unwrap();
    original.recovery_complete(&id).unwrap();
    original.cleanup(&id).unwrap();
    original.cleanup(&id).unwrap();
    assert!(original.session(&id).is_err());
    drop(original);
    let reopened = state(root.path());
    reopened.cleanup(&id).unwrap();
    let pending = session(&reopened, Operation::Capture, false);
    // A forgotten receipt can retry while a different maintenance session runs.
    reopened.cleanup(&id).unwrap();
    assert!(reopened.cleanup(&pending).is_err());
    assert_eq!(reopened.session(&pending).unwrap().phase, "capturing");
}

#[test]
fn startup_sweeps_only_inactive_spools_and_cold_acknowledgement_cleans_its_orphan() {
    let root = tempfile::tempdir().unwrap();
    let original = state(root.path());
    let inactive = session(&original, Operation::Capture, false);
    section(
        &original,
        &inactive,
        Spool::Source,
        r#"{"value":"completed"}"#,
    );
    original.finish_device(&inactive).unwrap();
    original.confirm_capture(&inactive).unwrap();
    original.recovery_complete(&inactive).unwrap();
    assert!(original.session(&inactive).is_ok());
    let pds = crate::persistent_store::commands::PersistentStoreState::default();
    original
        .attach_maintenance_guard(pds.acquire_device_maintenance().unwrap())
        .unwrap();
    let pending = session(&original, Operation::Restore, false);
    let (source, rollback) = prepared(&original, &pending);
    drop(original);

    let reopened = state(root.path());
    assert!(reopened.session(&inactive).is_err());
    assert_eq!(
        reopened
            .section_page(&pending, Spool::Source, None, 1)
            .unwrap()[0]
            .sha256,
        source.sha256
    );
    assert_eq!(
        reopened
            .section_page(&pending, Spool::Rollback, None, 1)
            .unwrap()[0]
            .sha256,
        rollback.sha256
    );
    assert!(reopened.cleanup(&pending).is_err());
    assert_eq!(
        reopened.bootstrap().unwrap().session.unwrap().action,
        "rollback"
    );
    complete(&reopened, &pending, Spool::Rollback, &rollback);
    reopened.recovery_complete(&pending).unwrap();
    assert!(reopened.session(&pending).is_err());
    assert!(!reopened.is_blocking().unwrap());
    reopened.cleanup(&pending).unwrap();
}

#[test]
fn cold_cleanup_failure_keeps_committed_recovery_and_spools_for_retry() {
    let root = tempfile::tempdir().unwrap();
    let original = state(root.path());
    let id = session(&original, Operation::Restore, false);
    let (source, _) = prepared(&original, &id);
    complete(&original, &id, Spool::Source, &source);
    original.finish_device(&id).unwrap();
    drop(original);
    let reopened = state(root.path());
    assert_eq!(
        reopened.bootstrap().unwrap().session.unwrap().phase,
        "committed"
    );
    complete(&reopened, &id, Spool::Source, &source);
    reopened.lock().unwrap().connection.as_ref().unwrap().execute_batch("CREATE TRIGGER synthetic_cleanup_failure BEFORE DELETE ON sessions BEGIN SELECT RAISE(FAIL,'synthetic cleanup fault'); END;").unwrap();
    assert!(reopened.recovery_complete(&id).is_err());
    assert!(reopened.is_blocking().unwrap());
    assert_eq!(reopened.session(&id).unwrap().phase, "committed");
    assert_eq!(
        reopened.section_page(&id, Spool::Source, None, 1).unwrap()[0].sha256,
        source.sha256
    );
    reopened
        .lock()
        .unwrap()
        .connection
        .as_ref()
        .unwrap()
        .execute_batch("DROP TRIGGER synthetic_cleanup_failure;")
        .unwrap();
    reopened.recovery_complete(&id).unwrap();
    assert!(!reopened.is_blocking().unwrap());
    assert!(reopened.session(&id).is_err());
}

#[test]
fn capture_is_exclusive_bounded_and_retained_until_native_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    assert_eq!(state.bootstrap().unwrap().mode, "normal");
    let id = session(&state, Operation::Capture, false);
    assert!(state
        .create_session(
            "another",
            Operation::Capture,
            false,
            &[SECTION.into()],
            None,
            None
        )
        .is_err());
    let manifest = section(&state, &id, Spool::Source, r#"{"u16":"d8000000dc00"}"#);
    assert_eq!(manifest.records, 1);
    assert!(state.recovery_complete(&id).is_err());
    assert_eq!(state.finish_device(&id).unwrap().phase, "device-captured");
    assert!(state.recovery_complete(&id).is_err());
    state.confirm_capture(&id).unwrap();
    assert!(state.is_blocking().unwrap());
    state.recovery_complete(&id).unwrap();
    assert_eq!(state.bootstrap().unwrap().mode, "normal");
    assert_eq!(
        state
            .row_read(&id, Spool::Source, SECTION, None, 8)
            .unwrap()
            .rows[0]
            .payload_json
            .as_deref(),
        Some(r#"{"u16":"d8000000dc00"}"#)
    );
    assert!(state
        .row_append(&id, Spool::Source, SECTION, 1, "{}")
        .is_err());
    state.cleanup(&id).unwrap();
    assert!(state.session(&id).is_err());
}

#[test]
fn crash_before_device_marker_requires_verified_rollback() {
    let root = tempfile::tempdir().unwrap();
    let original = state(root.path());
    let id = session(&original, Operation::Restore, false);
    let (source, rollback) = prepared(&original, &id);
    original.section_intent(&id, SECTION, false).unwrap();
    drop(original);
    let recovered = state(root.path());
    assert_eq!(
        recovered.bootstrap().unwrap().session.unwrap().action,
        "rollback"
    );
    assert!(recovered.recovery_complete(&id).is_err());
    assert!(recovered.cleanup(&id).is_err());
    assert_eq!(
        recovered.section_page(&id, Spool::Source, None, 1).unwrap()[0].sha256,
        source.sha256
    );
    assert_eq!(
        recovered
            .section_page(&id, Spool::Rollback, None, 1)
            .unwrap()[0]
            .sha256,
        rollback.sha256
    );
    complete(&recovered, &id, Spool::Rollback, &rollback);
    assert_eq!(recovered.session(&id).unwrap().phase, "rolling-back");
    recovered.recovery_complete(&id).unwrap();
    assert_eq!(
        recovered.session(&id).unwrap_err().code,
        "device-session-missing"
    );
    assert!(!recovered.is_blocking().unwrap());
    assert_eq!(recovered.bootstrap().unwrap().mode, "normal");
}

#[test]
fn device_marker_survives_crash_and_requires_source_reverification() {
    let root = tempfile::tempdir().unwrap();
    let original = state(root.path());
    let id = session(&original, Operation::Restore, false);
    let (source, _) = prepared(&original, &id);
    assert!(original
        .section_complete(&id, SECTION, false, &source.sha256)
        .is_err());
    complete(&original, &id, Spool::Source, &source);
    assert_eq!(original.finish_device(&id).unwrap().phase, "committed");
    drop(original);
    let recovered = state(root.path());
    assert_eq!(
        recovered.bootstrap().unwrap().session.unwrap().action,
        "reapply-source"
    );
    assert!(recovered.recovery_complete(&id).is_err());
    assert!(recovered.section_intent(&id, SECTION, true).is_err());
    complete(&recovered, &id, Spool::Source, &source);
    recovered.recovery_complete(&id).unwrap();
}

#[test]
fn library_marker_is_authority_in_pds_commit_journal_crash_window() {
    let root = tempfile::tempdir().unwrap();
    let original = state(root.path());
    let id = session(&original, Operation::Restore, true);
    let (source, _) = prepared(&original, &id);
    complete(&original, &id, Spool::Source, &source);
    assert_eq!(
        original.finish_device(&id).unwrap().phase,
        "committing-library"
    );
    assert!(original.mark_library_committed(&id).is_err());
    assert!(original.fail(&id, "cancelled").is_err());
    let (key, marker) = original.commit_marker(&id).unwrap();
    write_marker(root.path(), &key, &marker);
    drop(original);
    let recovered = state(root.path());
    assert_eq!(
        recovered.bootstrap().unwrap().session.unwrap().phase,
        "committed"
    );
    complete(&recovered, &id, Spool::Source, &source);
    recovered.recovery_complete(&id).unwrap();
}

#[test]
fn absent_library_marker_rolls_back_and_wrong_marker_blocks_startup() {
    let root = tempfile::tempdir().unwrap();
    let original = state(root.path());
    let id = session(&original, Operation::Restore, true);
    let (source, _) = prepared(&original, &id);
    complete(&original, &id, Spool::Source, &source);
    original.finish_device(&id).unwrap();
    let (key, mut marker) = original.commit_marker(&id).unwrap();
    drop(original);
    let recovered = state(root.path());
    assert_eq!(
        recovered.bootstrap().unwrap().session.unwrap().phase,
        "rolling-back"
    );
    drop(recovered);
    marker.session_id = "different-session".into();
    write_marker(root.path(), &key, &marker);
    let blocked = state(root.path());
    assert!(blocked.bootstrap().is_err());
    assert!(blocked.is_blocking().unwrap());
}

#[test]
fn failed_rollback_remains_blocked_and_spools_survive_retry() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let id = session(&state, Operation::Restore, false);
    let (_, rollback) = prepared(&state, &id);
    state.section_intent(&id, SECTION, false).unwrap();
    assert_eq!(
        state.fail(&id, "quota-exceeded").unwrap().phase,
        "rolling-back"
    );
    assert_eq!(
        state.fail(&id, "rollback-quota-exceeded").unwrap().phase,
        "recovery-required"
    );
    assert!(state.recovery_complete(&id).is_err());
    assert!(state.cleanup(&id).is_err());
    assert_eq!(state.retry_recovery(&id).unwrap().phase, "rolling-back");
    complete(&state, &id, Spool::Rollback, &rollback);
    state.recovery_complete(&id).unwrap();
}

#[test]
fn prewrite_capture_failure_can_be_acknowledged_without_a_rollback() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let id = session(&state, Operation::Capture, false);
    state
        .section_begin(&id, Spool::Source, SECTION, METADATA)
        .unwrap();
    assert_eq!(
        state.fail(&id, "unsupported-clone").unwrap().phase,
        "rolled-back"
    );
    state.recovery_complete(&id).unwrap();
    assert!(!state.is_blocking().unwrap());
}

#[test]
fn chunked_blob_roundtrip_checks_offsets_digest_aliases_and_transport_bounds() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let id = session(&state, Operation::Capture, false);
    let bytes = (0..MAX_CHUNK_BYTES * 2 + 17)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    state
        .blob_begin(&id, Spool::Source, "temporary-object")
        .unwrap();
    assert!(state
        .blob_append(&id, Spool::Source, "temporary-object", 1, &[1])
        .is_err());
    assert!(state
        .blob_append(
            &id,
            Spool::Source,
            "temporary-object",
            0,
            &vec![0; MAX_CHUNK_BYTES + 1]
        )
        .is_err());
    let mut offset = 0;
    for chunk in bytes.chunks(MAX_CHUNK_BYTES) {
        state
            .blob_append(&id, Spool::Source, "temporary-object", offset, chunk)
            .unwrap();
        offset += chunk.len() as u64;
    }
    let manifest = state
        .blob_finish(&id, Spool::Source, "temporary-object")
        .unwrap();
    assert_eq!(manifest.sha256, hex::encode(Sha256::digest(&bytes)));
    assert_eq!(
        state
            .blob_read(
                &id,
                Spool::Source,
                &manifest.sha256,
                (MAX_CHUNK_BYTES - 5) as u64,
                20
            )
            .unwrap(),
        bytes[MAX_CHUNK_BYTES - 5..MAX_CHUNK_BYTES + 15]
    );
    assert!(state
        .blob_read(&id, Spool::Source, &manifest.sha256, 0, MAX_CHUNK_BYTES + 1)
        .is_err());
    assert!(state
        .blob_append(
            &id,
            Spool::Source,
            &manifest.sha256,
            bytes.len() as u64,
            &[1]
        )
        .is_err());
}

#[test]
fn large_graph_uses_chunked_metadata_and_bounded_row_reads() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let id = session(&state, Operation::Capture, false);
    let payload = format!("{{\"utf16\":\"{}\"}}", "d800".repeat(MAX_CHUNK_BYTES));
    state
        .section_begin(&id, Spool::Source, SECTION, METADATA)
        .unwrap();
    assert!(state
        .row_append(&id, Spool::Source, SECTION, 0, &payload)
        .is_err());
    state.blob_begin(&id, Spool::Source, "graph").unwrap();
    let mut offset = 0;
    for chunk in payload.as_bytes().chunks(MAX_CHUNK_BYTES) {
        state
            .blob_append(&id, Spool::Source, "graph", offset, chunk)
            .unwrap();
        offset += chunk.len() as u64;
    }
    let blob = state.blob_finish(&id, Spool::Source, "graph").unwrap();
    state
        .row_append_from_blob(&id, Spool::Source, SECTION, 0, &blob.sha256)
        .unwrap();
    let manifest = state.section_finish(&id, Spool::Source, SECTION).unwrap();
    assert_eq!(manifest.records, 1);
    let page = state
        .row_read(&id, Spool::Source, SECTION, None, 1)
        .unwrap();
    assert!(page.rows[0].payload_json.is_none());
    let mut received = Vec::new();
    while received.len() < payload.len() {
        received.extend(
            state
                .row_read_bytes(
                    &id,
                    Spool::Source,
                    SECTION,
                    0,
                    received.len() as u64,
                    MAX_CHUNK_BYTES,
                )
                .unwrap(),
        );
    }
    assert_eq!(received, payload.as_bytes());
    assert!(state
        .row_append(&id, Spool::Source, SECTION, 1, "{}")
        .is_err());
}

#[test]
fn source_and_rollback_manifests_detect_corruption_before_device_writes() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let id = session(&state, Operation::Restore, false);
    let (source, _) = prepared(&state, &id);
    assert!(state
        .section_begin(&id, Spool::Source, SECTION, METADATA)
        .is_err());
    assert!(state
        .section_complete(&id, SECTION, false, &"0".repeat(64))
        .is_err());
    {
        let inner = state.lock().unwrap();
        inner
            .connection
            .as_ref()
            .unwrap()
            .execute(
                "UPDATE records SET payload=?1 WHERE session=?2 AND spool='source'",
                params![b"{}".as_slice(), id],
            )
            .unwrap();
    }
    assert!(state.section_intent(&id, SECTION, false).is_err());
    assert!(state
        .section_complete(&id, SECTION, false, &source.sha256)
        .is_err());
}

#[test]
fn archive_roundtrip_preserves_empty_presence_metadata_and_raw_binary() {
    let root = tempfile::tempdir().unwrap();
    let capture = state(root.path());
    let id = session(&capture, Operation::Capture, false);
    capture.blob_begin(&id, Spool::Source, "binary").unwrap();
    capture
        .blob_append(&id, Spool::Source, "binary", 0, &[0, 255, 13, 10])
        .unwrap();
    let blob = capture.blob_finish(&id, Spool::Source, "binary").unwrap();
    capture
        .section_begin(&id, Spool::Source, SECTION, r#"{"present":false}"#)
        .unwrap();
    let source = capture.section_finish(&id, Spool::Source, SECTION).unwrap();
    capture.finish_device(&id).unwrap();
    capture.confirm_capture(&id).unwrap();
    capture.recovery_complete(&id).unwrap();
    let archive = Connection::open_in_memory().unwrap();
    archive.execute_batch("CREATE TABLE device_sections(section TEXT PRIMARY KEY,schema_version INTEGER,included INTEGER,complete INTEGER,present INTEGER,record_count INTEGER,sha256 TEXT);CREATE TABLE device_records(section TEXT,ordinal INTEGER,metadata TEXT,PRIMARY KEY(section,ordinal));").unwrap();
    let mut objects = std::collections::HashMap::new();
    capture
        .export_spool(&id, Spool::Source, &archive, |hash, _, reader| {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            objects.insert(hash.to_owned(), bytes);
            Ok(())
        })
        .unwrap();
    let destination = tempfile::tempdir().unwrap();
    let restore = state(destination.path());
    let restore_id = session(&restore, Operation::Restore, false);
    restore
        .import_spool(
            &restore_id,
            &archive,
            &[(blob.sha256.clone(), blob.bytes)],
            |hash, writer| {
                writer.write_all(&objects[hash])?;
                Ok(())
            },
        )
        .unwrap();
    let imported = restore.section_list(&restore_id, Spool::Source).unwrap();
    assert_eq!(imported[0].sha256, source.sha256);
    assert!(!imported[0].present);
    assert_eq!(imported[0].records, 0);
    assert_eq!(
        restore
            .blob_read(&restore_id, Spool::Source, &blob.sha256, 0, 4)
            .unwrap(),
        [0, 255, 13, 10]
    );
}

#[test]
fn renderer_reload_recovers_device_intent_but_waits_for_live_library_worker() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let id = session(&state, Operation::Restore, true);
    let (source, rollback) = prepared(&state, &id);
    state.section_intent(&id, SECTION, false).unwrap();
    state.main_document_started();
    state.main_document_finished();
    assert_eq!(
        state
            .bootstrap_for_entry(true)
            .unwrap()
            .session
            .unwrap()
            .action,
        "rollback"
    );
    complete(&state, &id, Spool::Rollback, &rollback);
    state.recovery_complete(&id).unwrap();

    let pds = crate::persistent_store::commands::PersistentStoreState::default();
    state
        .attach_maintenance_guard(pds.acquire_device_maintenance().unwrap())
        .unwrap();
    let id = session(&state, Operation::Restore, true);
    prepared(&state, &id);
    complete(&state, &id, Spool::Source, &source);
    state.finish_device(&id).unwrap();
    state.main_document_started();
    state.main_document_finished();
    assert_eq!(
        state
            .bootstrap_for_entry(true)
            .unwrap()
            .session
            .unwrap()
            .action,
        "await-library"
    );
    assert!(state.section_intent(&id, SECTION, true).is_err());
    state.library_commit_failed(&id).unwrap();
    assert_eq!(
        state.bootstrap().unwrap().session.unwrap().action,
        "rollback"
    );
}

#[test]
fn no_session_can_start_without_native_fence_and_ack_releases_it() {
    let root = tempfile::tempdir().unwrap();
    let state = DeviceBackupState::initialize(root.path().join("device-backup"));
    assert!(state
        .create_session(
            "job",
            Operation::Capture,
            false,
            &[SECTION.into()],
            None,
            None
        )
        .is_err());
    let pds = crate::persistent_store::commands::PersistentStoreState::default();
    state
        .attach_maintenance_guard(pds.acquire_device_maintenance().unwrap())
        .unwrap();
    let id = session(&state, Operation::Capture, false);
    assert!(pds.admit_renderer_operation().is_err());
    state.fail(&id, "capture-aborted").unwrap();
    assert!(pds.admit_renderer_operation().is_err());
    state.recovery_complete(&id).unwrap();
    assert!(pds.admit_renderer_operation().is_ok());
}

#[test]
fn selection_rejects_non_plugin_databases_and_absence_cannot_hide_rows() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    assert!(state
        .create_session(
            "job",
            Operation::Capture,
            false,
            &["indexed-db:006100700070".into()],
            None,
            None
        )
        .is_err());
    assert!(state
        .create_session(
            "job",
            Operation::Capture,
            false,
            &["../persistent.db".into()],
            None,
            None
        )
        .is_err());
    let id = session(&state, Operation::Capture, false);
    state
        .section_begin(&id, Spool::Source, SECTION, r#"{"present":false}"#)
        .unwrap();
    state
        .row_append(&id, Spool::Source, SECTION, 0, "{}")
        .unwrap();
    assert!(state.section_finish(&id, Spool::Source, SECTION).is_err());
}

#[test]
fn section_manifest_ipc_pages_keep_large_metadata_bounded() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let id = state
        .create_session(
            "job",
            Operation::Capture,
            false,
            &[
                "local-storage".into(),
                "localforage".into(),
                "device-settings".into(),
            ],
            None,
            None,
        )
        .unwrap();
    let metadata = format!(
        "{{\"present\":true,\"schema\":\"{}\"}}",
        "x".repeat(150_000)
    );
    for section in ["local-storage", "localforage", "device-settings"] {
        state
            .section_begin(&id, Spool::Source, section, &metadata)
            .unwrap();
        state.section_finish(&id, Spool::Source, section).unwrap();
    }
    let first = state.section_page(&id, Spool::Source, None, 128).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].section_id, "device-settings");
    let next = state
        .section_page(&id, Spool::Source, Some(&first[0].section_id), 128)
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].section_id, "local-storage");
    assert_eq!(state.section_list(&id, Spool::Source).unwrap().len(), 3);
}

#[test]
fn native_publication_gates_precede_acknowledgement_and_first_device_intent() {
    let root = tempfile::tempdir().unwrap();
    let capture = state(root.path());
    let id = session(&capture, Operation::Capture, false);
    capture.main_document_started();
    capture.main_document_finished();
    capture.bootstrap_for_entry(true).unwrap();
    assert!(capture.maintenance_entered(&id).unwrap());
    section(&capture, &id, Spool::Source, "{}");
    assert_eq!(capture.finish_device(&id).unwrap().action, "await-capture");
    assert!(capture.recovery_complete(&id).is_err());
    capture.confirm_capture(&id).unwrap();
    capture.recovery_complete(&id).unwrap();
    let root = tempfile::tempdir().unwrap();
    let restore = state(root.path());
    let id = session(&restore, Operation::Restore, false);
    section(&restore, &id, Spool::Source, "{}");
    restore.source_ready(&id).unwrap();
    section(&restore, &id, Spool::Rollback, "{}");
    restore.prepared(&id).unwrap();
    assert_eq!(
        restore.session(&id).unwrap().action,
        "await-native-preparation"
    );
    assert!(restore.section_intent(&id, SECTION, false).is_err());
    restore.allow_device_apply(&id).unwrap();
    restore.section_intent(&id, SECTION, false).unwrap();
}

#[test]
fn interrupted_capture_reports_failure_and_diagnostic_detail_survives_reopen() {
    let root = tempfile::tempdir().unwrap();
    let original = state(root.path());
    let id = session(&original, Operation::Capture, false);
    original
        .fail_with_detail(
            &id,
            "unsupported-clone-type",
            Some(FailureDetail {
                section_id: SECTION.into(),
                value_type: "CryptoKey".into(),
                location: "record[0].value".into(),
            }),
        )
        .unwrap();
    drop(original);
    let recovered = state(root.path());
    let recovered_session = recovered.bootstrap().unwrap().session.unwrap();
    assert_eq!(
        recovered_session.failure_code.as_deref(),
        Some("unsupported-clone-type")
    );
    assert_eq!(
        recovered_session.failure_detail.unwrap().value_type,
        "CryptoKey"
    );
    recovered.recovery_complete(&id).unwrap();
    let root = tempfile::tempdir().unwrap();
    let original = state(root.path());
    let id = session(&original, Operation::Capture, false);
    drop(original);
    let recovered = state(root.path());
    let session = recovered.bootstrap().unwrap().session.unwrap();
    assert_eq!(session.phase, "rolled-back");
    assert_eq!(
        session.failure_code.as_deref(),
        Some("interrupted-maintenance")
    );
    recovered.recovery_complete(&id).unwrap();
}

#[test]
fn native_catalog_adapter_roundtrip_preserves_verified_device_objects() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let root = tempfile::tempdir().unwrap();
    let capture = state(root.path());
    let id = session(&capture, Operation::Capture, false);
    capture.blob_begin(&id, Spool::Source, "object").unwrap();
    capture
        .blob_append(&id, Spool::Source, "object", 0, b"synthetic")
        .unwrap();
    let blob = capture.blob_finish(&id, Spool::Source, "object").unwrap();
    let source = section(&capture, &id, Spool::Source, "{}");
    let catalog =
        crate::portable_backup::Catalog::create(root.path(), "synthetic-device", 0).unwrap();
    catalog
        .db
        .execute("INSERT INTO root VALUES('{}')", [])
        .unwrap();
    capture
        .export_to_catalog(&id, Spool::Source, &catalog, &Never)
        .unwrap();
    let path = root.path().join("device.risunest");
    catalog.write_candidate(&path, false, &Never).unwrap();
    let archive = crate::portable_backup::VerifiedArchive::open(
        std::fs::File::open(path).unwrap(),
        root.path(),
        &Never,
    )
    .unwrap();
    let destination = tempfile::tempdir().unwrap();
    let restore = state(destination.path());
    let id = session(&restore, Operation::Restore, false);
    restore.import_from_archive(&id, &archive, &Never).unwrap();
    assert_eq!(
        restore.section_list(&id, Spool::Source).unwrap()[0].sha256,
        source.sha256
    );
    assert_eq!(
        restore
            .blob_read(&id, Spool::Source, &blob.sha256, 0, 9)
            .unwrap(),
        b"synthetic"
    );
}

#[test]
fn failed_creation_releases_fence_only_when_no_durable_session_exists() {
    let root = tempfile::tempdir().unwrap();
    let state = DeviceBackupState::initialize(root.path().join("device-backup"));
    let pds = crate::persistent_store::commands::PersistentStoreState::default();
    state
        .attach_maintenance_guard(pds.acquire_device_maintenance().unwrap())
        .unwrap();
    assert!(state
        .create_session("job", Operation::Capture, false, &[], None, None)
        .is_err());
    state.release_unused_maintenance().unwrap();
    assert!(pds.admit_renderer_operation().is_ok());
    state
        .attach_maintenance_guard(pds.acquire_device_maintenance().unwrap())
        .unwrap();
    session(&state, Operation::Capture, false);
    assert!(state.release_unused_maintenance().is_err());
    assert!(pds.admit_renderer_operation().is_err());
}

#[test]
fn cleanup_reclaims_large_spool_pages_without_removing_pending_sessions() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let id = session(&state, Operation::Capture, true);
    state.blob_begin(&id, Spool::Source, "large").unwrap();
    for chunk in 0..8 {
        state
            .blob_append(
                &id,
                Spool::Source,
                "large",
                chunk * MAX_CHUNK_BYTES as u64,
                &vec![7; MAX_CHUNK_BYTES],
            )
            .unwrap();
    }
    state.blob_finish(&id, Spool::Source, "large").unwrap();
    section(&state, &id, Spool::Source, "{}");
    state.finish_device(&id).unwrap();
    state.confirm_capture(&id).unwrap();
    let path = root.path().join("device-backup/coordinator.sqlite");
    let before = std::fs::metadata(&path).unwrap().len();
    assert!(state.cleanup(&id).is_err());
    state.recovery_complete(&id).unwrap();
    state.cleanup(&id).unwrap();
    assert!(std::fs::metadata(path).unwrap().len() < before / 2);
}

#[test]
fn renderer_claim_cannot_replace_native_document_completion_and_poll_can_advance_it() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let id = state
        .create_session(
            "job",
            Operation::Capture,
            false,
            &[SECTION.into()],
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        state
            .bootstrap_for_entry(true)
            .unwrap()
            .session
            .unwrap()
            .action,
        "await-navigation"
    );
    assert!(!state.maintenance_entered(&id).unwrap());
    assert_eq!(
        state.bootstrap().unwrap().session.unwrap().action,
        "await-navigation"
    );
    state.main_document_started();
    state.main_document_finished();
    assert_eq!(
        state.bootstrap().unwrap().session.unwrap().action,
        "capture"
    );
    assert!(state.maintenance_entered(&id).unwrap());
    // The next renderer requires another completion; polling never invents one.
    assert_eq!(
        state
            .bootstrap_for_entry(true)
            .unwrap()
            .session
            .unwrap()
            .action,
        "await-navigation"
    );
    assert!(!state.maintenance_entered(&id).unwrap());
    state.main_document_started();
    state.main_document_finished();
    assert_eq!(
        state.bootstrap().unwrap().session.unwrap().action,
        "capture"
    );
}

#[test]
fn completion_of_a_navigation_started_before_session_does_not_open_the_barrier() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    state.main_document_started();
    let id = state
        .create_session(
            "job",
            Operation::Capture,
            false,
            &[SECTION.into()],
            None,
            None,
        )
        .unwrap();
    state.bootstrap_for_entry(true).unwrap();
    state.main_document_finished();
    assert_eq!(
        state.bootstrap().unwrap().session.unwrap().action,
        "await-navigation"
    );
    assert!(!state.maintenance_entered(&id).unwrap());
    state.main_document_started();
    state.main_document_finished();
    assert_eq!(
        state.bootstrap().unwrap().session.unwrap().action,
        "capture"
    );
    assert!(state.maintenance_entered(&id).unwrap());
}

fn renderer_app(state: DeviceBackupState) -> tauri::App<tauri::test::MockRuntime> {
    tauri::test::mock_builder()
        .manage(state)
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap()
}

fn assert_renderer_mutations_blocked(state: tauri::State<'_, DeviceBackupState>, id: &str) {
    let results = [
        native_device_backup_section_begin(
            state.clone(),
            id.into(),
            Spool::Source,
            SECTION.into(),
            METADATA.into(),
        ),
        native_device_backup_row_append(
            state.clone(),
            id.into(),
            Spool::Source,
            SECTION.into(),
            0,
            "{}".into(),
        ),
        native_device_backup_row_append_from_blob(
            state.clone(),
            id.into(),
            Spool::Source,
            SECTION.into(),
            0,
            "0".repeat(64),
        ),
        native_device_backup_section_finish(
            state.clone(),
            id.into(),
            Spool::Source,
            SECTION.into(),
        )
        .map(|_| ()),
        native_device_backup_blob_begin(state.clone(), id.into(), Spool::Source, "object".into()),
        native_device_backup_blob_append(
            state.clone(),
            id.into(),
            Spool::Source,
            "object".into(),
            0,
            vec![1],
        ),
        native_device_backup_blob_finish(state.clone(), id.into(), Spool::Source, "object".into())
            .map(|_| ()),
        native_device_backup_prepared(state.clone(), id.into()),
        native_device_backup_section_intent(state.clone(), id.into(), SECTION.into(), false),
        native_device_backup_section_complete(
            state.clone(),
            id.into(),
            SECTION.into(),
            false,
            "0".repeat(64),
        ),
        native_device_backup_finish_device(state.clone(), id.into()).map(|_| ()),
        native_device_backup_recovery_complete(state.clone(), id.into()),
        native_device_backup_fail(state.clone(), id.into(), "synthetic-failure".into(), None)
            .map(|_| ()),
        native_device_backup_retry_recovery(state, id.into()).map(|_| ()),
    ];
    for (index, result) in results.into_iter().enumerate() {
        assert_eq!(
            result.unwrap_err().code,
            "device-maintenance-not-entered",
            "mutation {index}"
        );
    }
}

#[test]
fn renderer_mutations_require_native_completion_and_confirmed_maintenance_entry() {
    let root = tempfile::tempdir().unwrap();
    let app = renderer_app(state(root.path()));
    let state = app.state::<DeviceBackupState>();
    let id = state
        .create_session(
            "renderer-capture",
            Operation::Capture,
            false,
            &[SECTION.into()],
            None,
            None,
        )
        .unwrap();
    assert_renderer_mutations_blocked(state.clone(), &id);
    assert_eq!(state.session(&id).unwrap().phase, "capturing");
    assert!(state.section_list(&id, Spool::Source).unwrap().is_empty());

    // A renderer claim alone cannot authorize any mutation, nor can Started.
    state.bootstrap_for_entry(true).unwrap();
    assert_renderer_mutations_blocked(state.clone(), &id);
    state.main_document_started();
    assert_renderer_mutations_blocked(state.clone(), &id);
    state.main_document_finished();
    assert_renderer_mutations_blocked(state.clone(), &id);
    assert_eq!(
        state.bootstrap().unwrap().session.unwrap().action,
        "capture"
    );
    assert!(state.maintenance_entered(&id).unwrap());

    native_device_backup_section_begin(
        state.clone(),
        id.clone(),
        Spool::Source,
        SECTION.into(),
        METADATA.into(),
    )
    .unwrap();
    native_device_backup_row_append(
        state.clone(),
        id.clone(),
        Spool::Source,
        SECTION.into(),
        0,
        "{}".into(),
    )
    .unwrap();
    native_device_backup_blob_begin(state.clone(), id.clone(), Spool::Source, "graph".into())
        .unwrap();
    native_device_backup_blob_append(
        state.clone(),
        id.clone(),
        Spool::Source,
        "graph".into(),
        0,
        b"{}".to_vec(),
    )
    .unwrap();
    let blob =
        native_device_backup_blob_finish(state.clone(), id.clone(), Spool::Source, "graph".into())
            .unwrap();
    native_device_backup_row_append_from_blob(
        state.clone(),
        id.clone(),
        Spool::Source,
        SECTION.into(),
        1,
        blob.sha256,
    )
    .unwrap();
    let manifest = native_device_backup_section_finish(
        state.clone(),
        id.clone(),
        Spool::Source,
        SECTION.into(),
    )
    .unwrap();
    assert_eq!(manifest.records, 2);
    assert_eq!(
        native_device_backup_finish_device(state.clone(), id.clone())
            .unwrap()
            .phase,
        "device-captured"
    );
    state.confirm_capture(&id).unwrap();
    native_device_backup_recovery_complete(state.clone(), id).unwrap();
    assert!(!state.is_blocking().unwrap());
}

#[test]
fn native_source_import_remains_available_before_renderer_restore_mutations() {
    let root = tempfile::tempdir().unwrap();
    let app = renderer_app(state(root.path()));
    let state = app.state::<DeviceBackupState>();
    let id = state
        .create_session(
            "renderer-restore",
            Operation::Restore,
            false,
            &[SECTION.into()],
            None,
            None,
        )
        .unwrap();
    let source = section(&state, &id, Spool::Source, "{}");
    state.source_ready(&id).unwrap();
    assert_renderer_mutations_blocked(state.clone(), &id);
    assert_eq!(state.session(&id).unwrap().phase, "preparing");
    state.main_document_started();
    state.main_document_finished();
    state.bootstrap_for_entry(true).unwrap();
    assert!(state.maintenance_entered(&id).unwrap());
    native_device_backup_section_begin(
        state.clone(),
        id.clone(),
        Spool::Rollback,
        SECTION.into(),
        METADATA.into(),
    )
    .unwrap();
    native_device_backup_section_finish(state.clone(), id.clone(), Spool::Rollback, SECTION.into())
        .unwrap();
    native_device_backup_prepared(state.clone(), id.clone()).unwrap();
    state.allow_device_apply(&id).unwrap();
    native_device_backup_section_intent(state.clone(), id.clone(), SECTION.into(), false).unwrap();
    native_device_backup_section_complete(
        state.clone(),
        id.clone(),
        SECTION.into(),
        false,
        source.sha256,
    )
    .unwrap();
    assert_eq!(
        native_device_backup_finish_device(state.clone(), id.clone())
            .unwrap()
            .phase,
        "committed"
    );

    // A later navigation revokes the old renderer's mutation permission.
    state.main_document_started();
    assert_renderer_mutations_blocked(state.clone(), &id);
    assert_eq!(state.session(&id).unwrap().phase, "committed");
}

#[test]
fn live_library_marker_poll_recovers_journal_update_without_discarding_write_proof() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let id = session(&state, Operation::Restore, true);
    let (source, _) = prepared(&state, &id);
    complete(&state, &id, Spool::Source, &source);
    state.finish_device(&id).unwrap();
    let waiting = state.bootstrap().unwrap().session.unwrap();
    assert_eq!(waiting.phase, "committing-library");
    assert_eq!(waiting.action, "await-library");
    assert!(state.recovery_complete(&id).is_err());

    let (key, marker) = state.commit_marker(&id).unwrap();
    write_marker(root.path(), &key, &marker);
    // Simulate successful PDS commit followed by a failed coordinator update.
    let committed = state.bootstrap().unwrap().session.unwrap();
    assert_eq!(committed.phase, "committed");
    state.recovery_complete(&id).unwrap();
    assert!(!state.is_blocking().unwrap());
}

fn full_device_catalog(root: &Path) -> crate::portable_backup::Catalog {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let capture = state(root);
    let id = capture
        .create_session(
            "catalog-validation",
            Operation::Capture,
            true,
            &[SECTION.into(), "localforage".into()],
            None,
            None,
        )
        .unwrap();
    for section in [SECTION, "localforage"] {
        capture
            .section_begin(&id, Spool::Source, section, METADATA)
            .unwrap();
        capture
            .row_append(
                &id,
                Spool::Source,
                section,
                0,
                r#"{"utf16":"d800","graph":{"nodes":[]}}"#,
            )
            .unwrap();
        capture.section_finish(&id, Spool::Source, section).unwrap();
    }
    let catalog = crate::portable_backup::Catalog::create(root, "synthetic-catalog", 0).unwrap();
    catalog
        .db
        .execute("INSERT INTO root VALUES('{}')", [])
        .unwrap();
    capture
        .export_to_catalog(&id, Spool::Source, &catalog, &Never)
        .unwrap();
    catalog
}

#[test]
fn catalog_validation_is_read_only_and_validates_all_sections_without_a_session() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let root = tempfile::tempdir().unwrap();
    let catalog = full_device_catalog(root.path());
    catalog.db.execute_batch("PRAGMA query_only=ON").unwrap();
    let before: i64 = catalog
        .db
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap();
    validate_archive_catalog(&catalog.db, &Never).unwrap();
    let after: i64 = catalog
        .db
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after, before);
}

#[test]
fn normal_archive_reader_rejects_unselected_device_corruption_before_any_restore_selection() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    // A library-only or local-storage-only restore never selects localforage.
    // Opening the archive must still reject each corrupt localforage section.
    for (case,mutation) in [
        ("value digest",r#"UPDATE device_records SET metadata='{"changed":true}' WHERE section='localforage' AND ordinal=0"#),
        ("record count","UPDATE device_sections SET record_count=2 WHERE section='localforage'"),
        ("header presence",r#"UPDATE device_records SET metadata='{"present":false}' WHERE section='localforage' AND ordinal=-1"#),
        ("missing header","DELETE FROM device_records WHERE section='localforage' AND ordinal=-1"),
        ("ordinal gap","UPDATE device_records SET ordinal=2 WHERE section='localforage' AND ordinal=0"),
        ("invalid JSON","UPDATE device_records SET metadata='not-json' WHERE section='localforage' AND ordinal=0"),
        ("excluded payload","UPDATE device_sections SET included=0 WHERE section='localforage'"),
        ("invalid flag","UPDATE device_sections SET complete=2 WHERE section='localforage'"),
        ("unknown schema","UPDATE device_sections SET schema_version=2 WHERE section='localforage'"),
        ("orphan row","INSERT INTO device_records VALUES('orphan',0,'{}')"),
        ("negative ordinal","UPDATE device_records SET ordinal=-2 WHERE section='localforage' AND ordinal=0"),
    ] {
        let root=tempfile::tempdir().unwrap();let catalog=full_device_catalog(root.path());
        catalog.db.execute_batch(mutation).unwrap();
        assert!(validate_archive_catalog(&catalog.db,&Never).is_err(),"{case}");
        // The ZIP and whole-catalog digest are correct. The broken per-section
        // contract must be checked by the ordinary reader, not selected import.
        let path=root.path().join("corrupt-unselected.risunest");catalog.write_candidate(&path,false,&Never).unwrap();
        assert!(crate::portable_backup::VerifiedArchive::open(std::fs::File::open(path).unwrap(),root.path(),&Never).is_err(),"reader accepted {case}");
    }
}

#[test]
fn catalog_validator_honors_cancellation_during_record_verification() {
    struct CancelAfter(std::cell::Cell<u32>);
    impl crate::local_backup::CancellationProbe for CancelAfter {
        fn is_cancelled(&self) -> bool {
            let count = self.0.get();
            self.0.set(count + 1);
            count >= 3
        }
    }
    let root = tempfile::tempdir().unwrap();
    let catalog = full_device_catalog(root.path());
    assert_eq!(
        validate_archive_catalog(&catalog.db, &CancelAfter(std::cell::Cell::new(0)))
            .unwrap_err()
            .code,
        "device-cancelled"
    );
}

#[test]
fn invalid_record_json_is_rejected_even_with_matching_device_and_catalog_hashes() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let root = tempfile::tempdir().unwrap();
    let catalog = full_device_catalog(root.path());
    let payload = b"not-json";
    let mut hash = Sha256::new();
    hash.update(b"RisuNest-device-section-v1\0");
    hash.update((METADATA.len() as u64).to_le_bytes());
    hash.update(METADATA.as_bytes());
    hash.update(0u64.to_le_bytes());
    hash.update((payload.len() as u64).to_le_bytes());
    hash.update(payload);
    catalog.db.execute("UPDATE device_records SET metadata='not-json' WHERE section='localforage' AND ordinal=0",[]).unwrap();
    catalog
        .db
        .execute(
            "UPDATE device_sections SET sha256=?1 WHERE section='localforage'",
            [hex::encode(hash.finalize())],
        )
        .unwrap();
    assert_eq!(
        validate_archive_catalog(&catalog.db, &Never)
            .unwrap_err()
            .code,
        "device-metadata-invalid"
    );
    let path = root.path().join("invalid-json.risunest");
    catalog.write_candidate(&path, false, &Never).unwrap();
    assert!(crate::portable_backup::VerifiedArchive::open(
        std::fs::File::open(path).unwrap(),
        root.path(),
        &Never
    )
    .is_err());
}

#[test]
fn excluded_empty_section_is_inspectable_when_its_header_and_digest_are_valid() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    let root = tempfile::tempdir().unwrap();
    let catalog = full_device_catalog(root.path());
    let metadata = r#"{"present":false}"#;
    let mut hash = Sha256::new();
    hash.update(b"RisuNest-device-section-v1\0");
    hash.update((metadata.len() as u64).to_le_bytes());
    hash.update(metadata.as_bytes());
    catalog
        .db
        .execute(
            "DELETE FROM device_records WHERE section='localforage' AND ordinal=0",
            [],
        )
        .unwrap();
    catalog
        .db
        .execute(
            "UPDATE device_records SET metadata=?1 WHERE section='localforage' AND ordinal=-1",
            [metadata],
        )
        .unwrap();
    catalog.db.execute("UPDATE device_sections SET included=0,complete=0,present=0,record_count=0,sha256=?1 WHERE section='localforage'",[hex::encode(hash.finalize())]).unwrap();
    validate_archive_catalog(&catalog.db, &Never).unwrap();
    let path = root.path().join("excluded.risunest");
    catalog.write_candidate(&path, false, &Never).unwrap();
    crate::portable_backup::VerifiedArchive::open(
        std::fs::File::open(path).unwrap(),
        root.path(),
        &Never,
    )
    .unwrap();
}
