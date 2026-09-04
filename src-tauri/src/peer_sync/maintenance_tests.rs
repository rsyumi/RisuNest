use super::maintenance::{
    active_temp_registry_is_locked, cleanup_temp, cleanup_temp_with_locked_predelete_hook,
    cleanup_temp_with_postvalidation_hook, cleanup_temp_with_predelete_hook, delete_backup,
    delete_backup_with_postvalidation_hook, delete_backup_with_predelete_hook,
    desktop_clone_backup_job_is_active, list_backups, peer_backup_delete_from_root,
    peer_backup_list_from_root, peer_temp_cleanup_from_root, peer_temp_usage_from_root, temp_usage,
    ActiveTempGuard, PeerBackupDeleteError,
};
use super::PeerSyncError;
use crate::asset_repository::job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob};
use std::fs;
#[cfg(any(unix, windows))]
use std::path::Path;

#[test]
fn peer_backup_delete_error_serializes_only_a_safe_code() {
    let in_use = serde_json::to_value(PeerBackupDeleteError::from(PeerSyncError::Validation(
        "peer-backup-in-use".to_owned(),
    )))
    .expect("serialize in-use error");
    let generic = serde_json::to_value(PeerBackupDeleteError::from(PeerSyncError::Storage(
        "C:/private/path".to_owned(),
    )))
    .expect("serialize generic error");

    assert_eq!(in_use, serde_json::json!({ "code": "peer-backup-in-use" }));
    assert_eq!(
        generic,
        serde_json::json!({ "code": "peer-backup-delete-failed" })
    );
}

#[test]
fn maintenance_invokes_log_app_root_and_operation_failures_without_changing_contracts() {
    let log_count = |needle: &str| {
        crate::native_log::global_state()
            .tail(None)
            .into_iter()
            .filter(|entry| entry.message.contains(needle))
            .count()
    };
    let assert_masked_app_root_failure = |needle: &str, marker: &str| {
        let entry = crate::native_log::global_state()
            .tail(None)
            .into_iter()
            .rev()
            .find(|entry| entry.message.contains(needle) && entry.message.contains(marker))
            .expect("find maintenance app-root failure log");
        assert!(entry.message.contains("Authorization: ***"));
        assert!(!entry.message.contains("fixture-maintenance-secret"));
    };

    let list_marker = format!("maintenance-list-root-{}", uuid::Uuid::new_v4());
    let list_detail =
        format!("{list_marker} Authorization: Bearer fixture-maintenance-secret\napp root failed");
    let list_contract = PeerSyncError::Storage(list_detail.clone()).to_string();
    assert_eq!(
        peer_backup_list_from_root(Err(PeerSyncError::Storage(list_detail)))
            .expect_err("list app-root failure"),
        list_contract
    );
    assert_masked_app_root_failure("peer_backup_list failed", &list_marker);

    let delete_marker = format!("maintenance-delete-root-{}", uuid::Uuid::new_v4());
    let delete_detail = format!(
        "{delete_marker} Authorization: Bearer fixture-maintenance-secret\napp root failed"
    );
    assert_eq!(
        peer_backup_delete_from_root(
            Err(PeerSyncError::Storage(delete_detail)),
            Path::new("unused")
        )
        .expect_err("delete app-root failure")
        .code,
        "peer-backup-delete-failed"
    );
    assert_masked_app_root_failure("peer_backup_delete failed", &delete_marker);

    let usage_marker = format!("maintenance-usage-root-{}", uuid::Uuid::new_v4());
    let usage_detail =
        format!("{usage_marker} Authorization: Bearer fixture-maintenance-secret\napp root failed");
    let usage_contract = PeerSyncError::Storage(usage_detail.clone()).to_string();
    assert_eq!(
        peer_temp_usage_from_root(Err(PeerSyncError::Storage(usage_detail)))
            .expect_err("usage app-root failure"),
        usage_contract
    );
    assert_masked_app_root_failure("peer_temp_usage failed", &usage_marker);

    let cleanup_marker = format!("maintenance-cleanup-root-{}", uuid::Uuid::new_v4());
    let cleanup_detail = format!(
        "{cleanup_marker} Authorization: Bearer fixture-maintenance-secret\napp root failed"
    );
    let cleanup_contract = PeerSyncError::Storage(cleanup_detail.clone()).to_string();
    assert_eq!(
        peer_temp_cleanup_from_root(Err(PeerSyncError::Storage(cleanup_detail)))
            .expect_err("cleanup app-root failure"),
        cleanup_contract
    );
    assert_masked_app_root_failure("peer_temp_cleanup failed", &cleanup_marker);

    let directory = tempfile::tempdir().expect("temporary directory");
    let invalid_root = directory.path().join("not-a-directory");
    fs::write(&invalid_root, b"file").expect("create invalid app root");

    let list_before = log_count("peer_backup_list failed");
    assert!(peer_backup_list_from_root(Ok(invalid_root.clone())).is_err());
    assert_eq!(log_count("peer_backup_list failed"), list_before + 1);

    let delete_before = log_count("peer_backup_delete failed");
    assert_eq!(
        peer_backup_delete_from_root(
            Ok(directory.path().to_path_buf()),
            &directory.path().join("missing.lossless")
        )
        .expect_err("delete operation failure")
        .code,
        "peer-backup-delete-failed"
    );
    assert_eq!(log_count("peer_backup_delete failed"), delete_before + 1);

    let usage_before = log_count("peer_temp_usage failed");
    assert!(peer_temp_usage_from_root(Ok(invalid_root.clone())).is_err());
    assert_eq!(log_count("peer_temp_usage failed"), usage_before + 1);

    let cleanup_before = log_count("peer_temp_cleanup failed");
    assert!(peer_temp_cleanup_from_root(Ok(invalid_root)).is_err());
    assert_eq!(log_count("peer_temp_cleanup failed"), cleanup_before + 1);
}

#[cfg(unix)]
fn create_file_link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("create file symlink");
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("create directory symlink");
}

#[cfg(windows)]
fn create_directory_link(target: &Path, link: &Path) {
    let output = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link.to_string_lossy().replace('/', "\\"))
        .arg(target.to_string_lossy().replace('/', "\\"))
        .output()
        .expect("invoke junction creation");
    assert!(
        output.status.success(),
        "create directory junction: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn backup_deletion_requires_a_current_listed_regular_file() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let backup = directory
        .path()
        .join("peer-clone/activation/backups/clone.risulossless");
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create root");
    fs::write(&backup, b"backup").expect("write backup");

    let listed = list_backups(directory.path()).expect("list backups");
    assert_eq!(listed.len(), 1);
    delete_backup(directory.path(), std::path::Path::new(&listed[0].path))
        .expect("delete listed backup");
    assert!(!backup.exists());

    let outside = directory.path().join("outside.risulossless");
    fs::write(&outside, b"outside").expect("write outside file");
    assert!(matches!(
        delete_backup(directory.path(), &outside),
        Err(PeerSyncError::Validation { .. })
    ));
    assert!(outside.exists());
}

#[test]
fn temporary_cleanup_keeps_backup_and_active_operation_directories() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let abandoned = root.join("peer-delta/staging/abandoned");
    let active = root.join("peer-bidirectional/staging/active");
    let backup = root.join("peer-clone/activation/backups/backup.risulossless");
    fs::create_dir_all(&abandoned).expect("create abandoned");
    fs::create_dir_all(&active).expect("create active");
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backups");
    fs::write(abandoned.join("payload"), b"abandoned").expect("write abandoned");
    fs::write(active.join("operation.json"), b"{}").expect("write operation");
    fs::write(&backup, b"backup").expect("write backup");

    assert_eq!(temp_usage(root).expect("usage").count, 1);
    assert_eq!(cleanup_temp(root).expect("cleanup").count, 1);
    assert!(!abandoned.exists());
    assert!(active.exists());
    assert!(backup.exists());
}

#[test]
fn clone_activation_stage_jobs_preserve_only_matching_unreleased_stages() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let manifest = "e".repeat(64);
    let desktop_active_id = "00000000-0000-4000-8000-000000000101";
    let android_active_id = "00000000-0000-4000-8000-000000000102";
    let released_id = "00000000-0000-4000-8000-000000000103";
    let unrelated_id = "00000000-0000-4000-8000-000000000104";
    let desktop_active = root.join(format!(
        "peer-clone/activation/{manifest}_{desktop_active_id}"
    ));
    let android_active = root.join(format!(
        "peer-clone-activation/{manifest}_{android_active_id}"
    ));
    let released = root.join(format!("peer-clone/activation/{manifest}_{released_id}"));
    let unrelated = root.join(format!("peer-clone/activation/{manifest}_{unrelated_id}"));
    for stage in [&desktop_active, &android_active, &released, &unrelated] {
        fs::create_dir_all(stage).expect("create stage");
        fs::write(stage.join("payload"), b"stage payload").expect("write stage payload");
    }

    let _desktop_active = DurableCasJob::begin(root, desktop_active_id, CasJobKind::PeerClone, 0)
        .expect("create active desktop job");
    let _android_active = DurableCasJob::begin(root, android_active_id, CasJobKind::PeerClone, 0)
        .expect("create active Android job");
    let mut released_job = DurableCasJob::begin(root, released_id, CasJobKind::PeerClone, 0)
        .expect("create released job");
    released_job
        .release(CasReleaseOutcome::Aborted)
        .expect("release matching job");
    let _unrelated = DurableCasJob::begin(root, unrelated_id, CasJobKind::LogicalDeltaTarget, 0)
        .expect("create unrelated job");

    assert_eq!(temp_usage(root).expect("calculate usage").count, 2);
    assert_eq!(cleanup_temp(root).expect("clean abandoned stages").count, 2);
    assert!(desktop_active.exists());
    assert!(android_active.exists());
    assert!(!released.exists());
    assert!(!unrelated.exists());
}

#[test]
fn final_temp_cleanup_recheck_preserves_a_matching_job_created_after_listing() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let id = "00000000-0000-4000-8000-000000000105";
    let stage = root.join(format!("peer-clone/activation/{}_{id}", "f".repeat(64)));
    fs::create_dir_all(&stage).expect("create stage");
    fs::write(stage.join("payload"), b"stage payload").expect("write stage payload");
    let mut late_job = None;

    let removed = cleanup_temp_with_predelete_hook(root, |_, _| {
        late_job = Some(
            DurableCasJob::begin(root, id, CasJobKind::PeerClone, 0)
                .expect("create matching job during predelete hook"),
        );
        Ok(())
    })
    .expect("late matching job must preserve stage");

    assert!(late_job.is_some());
    assert_eq!(removed.count, 0);
    assert!(stage.exists());
}

#[test]
fn active_clone_stage_before_durable_job_creation_is_not_temp_cleanup() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let id = "00000000-0000-4000-8000-000000000106";
    let stage = root.join(format!("peer-clone/activation/{}_{id}", "a".repeat(64)));
    let active = ActiveTempGuard::acquire(&stage).expect("protect active clone stage");
    fs::create_dir_all(&stage).expect("create active clone stage");
    fs::write(stage.join("incoming.lossless.tmp"), b"partial clone")
        .expect("write active clone payload");

    assert_eq!(temp_usage(root).expect("calculate usage").count, 0);
    assert_eq!(cleanup_temp(root).expect("clean temp").count, 0);
    assert!(stage.exists());

    drop(active);
    assert_eq!(
        cleanup_temp(root)
            .expect("clean abandoned clone stage")
            .count,
        1
    );
    assert!(!stage.exists());
}

#[test]
fn active_delta_and_bidirectional_staging_are_not_temp_cleanup() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let delta = root
        .join("peer-delta")
        .join("staging")
        .join("staging-logical-active-delta");
    let bidirectional = root
        .join("peer-bidirectional")
        .join("staging")
        .join("staging-logical-active-bidi");
    let backup = root
        .join("peer-bidirectional")
        .join("backup-staging")
        .join("00000000-0000-4000-8000-000000000107-local");
    let delta_active = ActiveTempGuard::acquire(&delta).expect("protect active delta stage");
    let bidirectional_active =
        ActiveTempGuard::acquire(&bidirectional).expect("protect active bidirectional stage");
    let backup_active = ActiveTempGuard::acquire(&backup).expect("protect active backup stage");
    for stage in [&delta, &bidirectional, &backup] {
        fs::create_dir_all(stage).expect("create active stage");
        fs::write(stage.join("payload"), b"active").expect("write active stage payload");
    }

    assert_eq!(temp_usage(root).expect("calculate usage").count, 0);
    assert_eq!(cleanup_temp(root).expect("clean temp").count, 0);
    assert!(delta.exists());
    assert!(bidirectional.exists());
    assert!(backup.exists());

    drop((delta_active, bidirectional_active, backup_active));
    assert_eq!(cleanup_temp(root).expect("clean abandoned stages").count, 3);
    assert!(!delta.exists());
    assert!(!bidirectional.exists());
    assert!(!backup.exists());
}

#[test]
fn guard_acquisition_waits_for_cleanup_deletion_and_recreated_stage_stays_active() {
    use std::{sync::mpsc, thread, time::Duration};

    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let stage = root
        .join("peer-bidirectional")
        .join("backup-staging")
        .join("00000000-0000-4000-8000-000000000108-local");
    fs::create_dir_all(&stage).expect("create abandoned stage");
    fs::write(stage.join("abandoned"), b"abandoned").expect("write abandoned payload");

    let (start_tx, start_rx) = mpsc::channel();
    let (attempt_tx, attempt_rx) = mpsc::channel();
    let (acquired_tx, acquired_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker_stage = stage.clone();
    let worker = thread::spawn(move || {
        start_rx.recv().expect("wait for cleanup deletion lock");
        attempt_tx
            .send(())
            .expect("report guard acquisition attempt");
        let guard = ActiveTempGuard::acquire(&worker_stage).expect("acquire resumed stage");
        fs::create_dir_all(&worker_stage).expect("recreate resumed stage");
        fs::write(worker_stage.join("active"), b"active").expect("write active payload");
        acquired_tx.send(()).expect("report resumed stage");
        release_rx.recv().expect("hold resumed stage active");
        drop(guard);
    });

    let removed = cleanup_temp_with_locked_predelete_hook(root, |_, candidate| {
        assert_eq!(candidate, stage);
        start_tx.send(()).expect("start resumed stage acquisition");
        attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker reaches guard acquisition attempt");
        assert!(active_temp_registry_is_locked());
        Ok(())
    })
    .expect("delete only the abandoned stage");

    assert_eq!(removed.count, 1);
    acquired_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("resumed stage acquires after deletion");
    assert_eq!(cleanup_temp(root).expect("preserve resumed stage").count, 0);
    assert_eq!(
        fs::read(stage.join("active")).expect("read active payload"),
        b"active"
    );

    release_tx.send(()).expect("release resumed stage");
    worker.join().expect("join resumed stage worker");
    assert_eq!(
        cleanup_temp(root)
            .expect("remove abandoned resumed stage")
            .count,
        1
    );
    assert!(!stage.exists());
}

#[test]
fn relative_bidirectional_journal_backup_blocks_deletion() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let backup = root.join("peer-bidirectional/backups/referenced.risulossless");
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backups");
    fs::write(&backup, b"backup").expect("write backup");
    let operation = root.join("peer-bidirectional/operation.json");
    let id = "00000000-0000-4000-8000-000000000001";
    fs::write(&operation, format!(r#"{{"phase":"completed","schema":"risunest.peer-bidirectional-operation/v1","result":{{"kind":"done","operationId":"{id}","revision":0,"remoteRevision":0,"transferredObjects":0,"transferredBytes":0,"backups":[{{"packageId":"package","side":"local","path":"peer-bidirectional/backups/referenced.risulossless"}}]}}}}"#)).expect("write operation");

    assert!(
        matches!(delete_backup(root, &backup), Err(PeerSyncError::Validation(message)) if message == "peer-backup-in-use")
    );
    assert!(backup.exists());
}

#[test]
fn desktop_clone_backup_only_blocks_its_matching_unreleased_peer_clone_job() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let id = "00000000-0000-4000-8000-000000000002";
    let backup = root.join(format!(
        "peer-clone/activation/backups/pre-clone-{id}.lossless"
    ));
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backups");
    fs::write(&backup, b"backup").expect("write backup");
    let _unrelated = DurableCasJob::begin(root, "unrelated-job", CasJobKind::LogicalDeltaTarget, 0)
        .expect("unrelated job");
    delete_backup(root, &backup).expect("unrelated job does not block backup");

    fs::write(&backup, b"backup").expect("restore backup");
    let _matching = DurableCasJob::begin(root, id, CasJobKind::PeerClone, 0).expect("matching job");
    assert_eq!(
        DurableCasJob::open(root, id)
            .expect("open matching job")
            .kind(),
        CasJobKind::PeerClone
    );
    assert!(desktop_clone_backup_job_is_active(root, &backup));
    assert!(
        matches!(delete_backup(root, &backup), Err(PeerSyncError::Validation(message)) if message == "peer-backup-in-use")
    );
}

#[test]
fn android_clone_backup_only_blocks_its_matching_unreleased_peer_clone_job() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let id = "00000000-0000-4000-8000-000000000003";
    let backup = root.join(format!(
        "peer-clone-activation/backups/pre-clone-{id}.lossless"
    ));
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backups");
    fs::write(&backup, b"backup").expect("write backup");

    let mut matching =
        DurableCasJob::begin(root, id, CasJobKind::PeerClone, 0).expect("matching job");
    assert!(matches!(
        delete_backup(root, &backup),
        Err(PeerSyncError::Validation(message)) if message == "peer-backup-in-use"
    ));

    matching
        .release(CasReleaseOutcome::Aborted)
        .expect("release matching job");
    delete_backup(root, &backup).expect("released matching job does not block backup");
}

#[test]
fn android_clone_backup_whose_stem_is_not_a_job_id_is_deletable() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let job_id = "00000000-0000-4000-8000-000000000209";
    let backup = root.join("peer-clone-activation/backups/pre-clone-not-a-job-id.lossless");
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backups");
    fs::write(&backup, b"unowned Android backup").expect("write Android backup");
    let jobs_root = root.join("peer-clone-jobs");
    fs::create_dir_all(&jobs_root).expect("create Android jobs root");
    // The current job record carries a deliberately invalid schema, so reading it
    // raises: the backup is deletable only because the unparsable job stem is
    // rejected before the current job is ever consulted.
    fs::write(
        jobs_root.join("current.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": "risunest.android-peer-clone-registry/not-a-schema",
            "jobId": job_id,
        }))
        .expect("serialize Android job record"),
    )
    .expect("write Android job record");

    delete_backup(root, &backup).expect("backup with an unparsable job stem is deletable");
    assert!(!backup.exists());
}

#[test]
fn current_android_clone_backup_remains_protected_after_cas_release() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let job_id = "00000000-0000-4000-8000-000000000205";
    let manifest_id = "b".repeat(64);
    let backup = root.join(format!(
        "peer-clone-activation/backups/pre-clone-{job_id}.lossless"
    ));
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backups");
    fs::write(&backup, b"current Android backup").expect("write Android backup");
    let mut matching =
        DurableCasJob::begin(root, job_id, CasJobKind::PeerClone, 0).expect("matching job");
    matching
        .release(CasReleaseOutcome::Aborted)
        .expect("release CAS job after activation");

    let jobs_root = root.join("peer-clone-jobs");
    let job_root = jobs_root.join(job_id);
    fs::create_dir_all(&job_root).expect("create Android job root");
    let write_json = |path: &Path, value: serde_json::Value| {
        fs::write(
            path,
            serde_json::to_vec(&value).expect("serialize Android job record"),
        )
        .expect("write Android job record");
    };
    write_json(
        &jobs_root.join("current.json"),
        serde_json::json!({
            "schema": "risunest.android-peer-clone-registry/v1",
            "jobId": job_id,
        }),
    );
    write_json(
        &job_root.join("ownership.json"),
        serde_json::json!({
            "schema": "risunest.android-peer-clone-ownership/v1",
            "jobId": job_id,
        }),
    );
    write_json(
        &job_root.join("job.json"),
        serde_json::json!({
            "schema": "risunest.android-peer-clone-job/v1",
            "jobId": job_id,
            "manifestId": manifest_id,
        }),
    );
    write_json(
        &job_root.join("status.json"),
        serde_json::json!({
            "schema": "risunest.android-peer-clone-status/v1",
            "phase": "awaitingActivation",
            "completedBytes": 1,
            "totalBytes": 1,
            "error": null,
            "committedRevision": 2,
            "backupPath": backup,
            "completionAcknowledged": false,
        }),
    );

    assert_eq!(
        delete_backup(root, &backup).expect_err("current backup must remain"),
        PeerSyncError::Validation("peer-backup-in-use".to_owned())
    );
    assert!(backup.is_file());

    fs::remove_file(jobs_root.join("current.json")).expect("release current Android job");
    delete_backup(root, &backup).expect("released backup can be deleted");
    assert!(!backup.exists());
}

#[test]
fn desktop_clone_backup_remains_protected_until_its_exact_receipt_and_ack() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let operation_id = "00000000-0000-4000-8000-000000000201";
    let session_id = "00000000-0000-4000-8000-000000000202";
    let backup = root.join(format!(
        "peer-clone/activation/backups/pre-clone-{operation_id}.lossless"
    ));
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backup root");
    fs::write(&backup, b"retryable desktop backup").expect("write desktop backup");
    let marker = root
        .join("peer-clone/targets")
        .join(session_id)
        .join("activation-operation.json");
    fs::create_dir_all(marker.parent().expect("marker parent")).expect("create target job root");
    let write_marker = |completion_acknowledged: bool| {
        fs::write(
            &marker,
            serde_json::to_vec(&serde_json::json!({
                "schema": "risunest.peer-clone-target-operation/v1",
                "sessionId": session_id,
                "manifestId": "0".repeat(64),
                "operationId": operation_id,
                "backupPath": format!(
                    "activation/backups/pre-clone-{operation_id}.lossless"
                ),
                "completionAcknowledged": completion_acknowledged,
            }))
            .expect("serialize operation marker"),
        )
        .expect("write operation marker");
    };

    write_marker(false);
    assert_eq!(
        delete_backup(root, &backup).expect_err("unacknowledged backup must remain"),
        PeerSyncError::Validation("peer-backup-in-use".to_owned())
    );
    assert!(backup.is_file());

    write_marker(true);
    delete_backup(root, &backup).expect("acknowledged backup can be deleted");
    assert!(!backup.exists());
}

#[test]
fn android_clone_backup_remains_protected_until_its_current_job_is_released() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let job_id = "00000000-0000-4000-8000-000000000203";
    let manifest_id = "1".repeat(64);
    let backup = root.join(format!(
        "peer-clone-activation/backups/pre-clone-{job_id}.lossless"
    ));
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backup root");
    fs::write(&backup, b"retryable Android backup").expect("write Android backup");
    let jobs_root = root.join("peer-clone-jobs");
    let job_root = jobs_root.join(job_id);
    fs::create_dir_all(&job_root).expect("create Android job root");
    let write_json = |path: &Path, value: serde_json::Value| {
        fs::write(
            path,
            serde_json::to_vec(&value).expect("serialize Android job record"),
        )
        .expect("write Android job record");
    };
    write_json(
        &jobs_root.join("current.json"),
        serde_json::json!({
            "schema": "risunest.android-peer-clone-registry/v1",
            "jobId": job_id,
        }),
    );
    write_json(
        &job_root.join("ownership.json"),
        serde_json::json!({
            "schema": "risunest.android-peer-clone-ownership/v1",
            "jobId": job_id,
        }),
    );
    write_json(
        &job_root.join("job.json"),
        serde_json::json!({
            "schema": "risunest.android-peer-clone-job/v1",
            "jobId": job_id,
            "manifestId": manifest_id,
        }),
    );
    write_json(
        &job_root.join("status.json"),
        serde_json::json!({
            "schema": "risunest.android-peer-clone-status/v1",
            "phase": "awaitingActivation",
            "completedBytes": 1,
            "totalBytes": 1,
            "error": null,
            "committedRevision": 2,
            "backupPath": backup,
        }),
    );

    assert_eq!(
        delete_backup(root, &backup).expect_err("current Android backup must remain"),
        PeerSyncError::Validation("peer-backup-in-use".to_owned())
    );
    assert!(backup.is_file());

    let mismatched_receipt = backup
        .parent()
        .expect("backup parent")
        .join("pre-clone-00000000-0000-4000-8000-000000000299.lossless");
    fs::write(&mismatched_receipt, b"other backup").expect("write mismatched receipt backup");
    write_json(
        &job_root.join("status.json"),
        serde_json::json!({
            "schema": "risunest.android-peer-clone-status/v1",
            "phase": "awaitingActivation",
            "completedBytes": 1,
            "totalBytes": 1,
            "error": null,
            "committedRevision": 2,
            "backupPath": mismatched_receipt,
        }),
    );
    assert!(delete_backup(root, &backup).is_err());
    assert!(backup.is_file());

    fs::remove_file(jobs_root.join("current.json")).expect("release current Android job");
    delete_backup(root, &backup).expect("released Android backup can be deleted");
    assert!(!backup.exists());
}

#[test]
fn desktop_clone_backup_matching_released_job_permits_deletion() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let id = "00000000-0000-4000-8000-000000000004";
    let backup = root.join(format!(
        "peer-clone/activation/backups/pre-clone-{}-{id}.lossless",
        "c".repeat(64)
    ));
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backups");
    fs::write(&backup, b"backup").expect("write backup");
    let mut matching =
        DurableCasJob::begin(root, id, CasJobKind::PeerClone, 0).expect("matching job");

    matching
        .release(CasReleaseOutcome::Aborted)
        .expect("release matching job");
    delete_backup(root, &backup).expect("released desktop matching job does not block backup");
    assert!(!backup.exists());
}

#[test]
fn final_predelete_recheck_rejects_a_matching_job_created_after_initial_checks() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let id = "00000000-0000-4000-8000-000000000005";
    let backup = root.join(format!(
        "peer-clone/activation/backups/pre-clone-{}-{id}.lossless",
        "d".repeat(64)
    ));
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backups");
    fs::write(&backup, b"backup").expect("write backup");
    let mut late_job = None;

    let error = delete_backup_with_predelete_hook(root, &backup, || {
        late_job = Some(
            DurableCasJob::begin(root, id, CasJobKind::PeerClone, 0)
                .expect("create matching job during predelete hook"),
        );
        Ok(())
    })
    .expect_err("late matching job must block deletion");

    assert!(late_job.is_some());
    assert!(matches!(
        error,
        PeerSyncError::Validation(message) if message == "peer-backup-in-use"
    ));
    assert!(backup.exists());
}

#[test]
fn final_locked_recheck_rejects_a_desktop_reference_published_after_validation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let operation_id = "00000000-0000-4000-8000-000000000006";
    let session_id = "00000000-0000-4000-8000-000000000007";
    let backup = root.join(format!(
        "peer-clone/activation/backups/pre-clone-{operation_id}.lossless"
    ));
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backups");
    fs::write(&backup, b"backup").expect("write backup");

    let error = delete_backup_with_postvalidation_hook(root, &backup, || {
        let marker = root
            .join("peer-clone/targets")
            .join(session_id)
            .join("activation-operation.json");
        fs::create_dir_all(marker.parent().expect("marker parent"))
            .expect("create target marker root");
        fs::write(
            marker,
            serde_json::to_vec(&serde_json::json!({
                "schema": "risunest.peer-clone-target-operation/v1",
                "sessionId": session_id,
                "manifestId": "0".repeat(64),
                "operationId": operation_id,
                "backupPath": format!(
                    "activation/backups/pre-clone-{operation_id}.lossless"
                ),
                "completionAcknowledged": false,
            }))
            .expect("serialize target marker"),
        )
        .expect("publish target marker after validation");
        Ok(())
    })
    .expect_err("reference published after validation must block deletion");

    assert_eq!(
        error,
        PeerSyncError::Validation("peer-backup-in-use".to_owned())
    );
    assert!(backup.exists());
}

#[cfg(unix)]
#[test]
fn backup_and_temp_maintenance_do_not_follow_leaf_symlinks() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let external_backup = root.join("external-backup.lossless");
    fs::write(&external_backup, b"external backup").expect("write external backup");
    let backup_root = root.join("peer-clone/activation/backups");
    fs::create_dir_all(&backup_root).expect("create backup root");
    let linked_backup = backup_root.join("linked.lossless");
    create_file_link(&external_backup, &linked_backup);

    assert!(list_backups(root).expect("list backups").is_empty());
    assert!(delete_backup(root, &linked_backup).is_err());
    assert_eq!(
        fs::read(&external_backup).expect("read external backup"),
        b"external backup"
    );

    let external_leaf = root.join("external-leaf");
    fs::write(&external_leaf, b"external temp leaf").expect("write external leaf");
    let abandoned = root.join("peer-delta/staging/abandoned");
    fs::create_dir_all(&abandoned).expect("create abandoned directory");
    fs::write(abandoned.join("local"), b"local temp leaf").expect("write local leaf");
    create_file_link(&external_leaf, &abandoned.join("linked"));

    assert_eq!(temp_usage(root).expect("calculate temp usage").count, 1);
    assert_eq!(cleanup_temp(root).expect("cleanup temp").count, 1);
    assert_eq!(
        fs::read(&external_leaf).expect("read external leaf"),
        b"external temp leaf"
    );
}

#[cfg(windows)]
#[test]
fn backup_and_temp_maintenance_do_not_follow_leaf_reparse_points() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let external_backup = root.join("external-backup");
    fs::create_dir(&external_backup).expect("create external backup");
    fs::write(external_backup.join("sentinel"), b"external backup")
        .expect("write external backup sentinel");
    let backup_root = root.join("peer-clone/activation/backups");
    fs::create_dir_all(&backup_root).expect("create backup root");
    let linked_backup = backup_root.join("linked.lossless");
    create_directory_link(&external_backup, &linked_backup);

    assert!(list_backups(root).expect("list backups").is_empty());
    assert!(delete_backup(root, &linked_backup).is_err());
    assert_eq!(
        fs::read(external_backup.join("sentinel")).expect("read external backup sentinel"),
        b"external backup"
    );

    let external_leaf = root.join("external-temp-leaf");
    fs::create_dir(&external_leaf).expect("create external temp leaf");
    fs::write(external_leaf.join("sentinel"), b"external temp leaf")
        .expect("write external temp sentinel");
    let abandoned = root.join("peer-delta/staging/abandoned");
    fs::create_dir_all(&abandoned).expect("create abandoned directory");
    fs::write(abandoned.join("local"), b"local temp leaf").expect("write local leaf");
    create_directory_link(&external_leaf, &abandoned.join("linked"));

    assert_eq!(temp_usage(root).expect("calculate temp usage").count, 1);
    assert_eq!(cleanup_temp(root).expect("cleanup temp").count, 1);
    assert_eq!(
        fs::read(external_leaf.join("sentinel")).expect("read external temp sentinel"),
        b"external temp leaf"
    );
}

#[cfg(unix)]
#[test]
fn final_direct_child_validation_rejects_a_replaced_backup_root() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let backup = root.join("peer-clone/activation/backups/replace-me.lossless");
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backup root");
    fs::write(&backup, b"original backup").expect("write backup");
    let external_root = root.join("external-backups");
    fs::create_dir(&external_root).expect("create external root");
    let external_backup = external_root.join("replace-me.lossless");
    fs::write(&external_backup, b"external backup").expect("write external backup");
    let backup_root = backup.parent().expect("backup parent").to_path_buf();

    let error = delete_backup_with_predelete_hook(root, &backup, || {
        fs::remove_file(&backup).expect("remove original backup");
        fs::remove_dir(&backup_root).expect("remove original backup root");
        create_directory_link(&external_root, &backup_root);
        Ok(())
    })
    .expect_err("replaced root must be rejected");

    assert!(matches!(error, PeerSyncError::Validation { .. }));
    assert_eq!(
        fs::read(&external_backup).expect("read external backup"),
        b"external backup"
    );
}

#[cfg(windows)]
#[test]
fn final_direct_child_validation_rejects_a_replaced_backup_reparse_root() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    let backup = root.join("peer-clone/activation/backups/replace-me.lossless");
    fs::create_dir_all(backup.parent().expect("backup parent")).expect("create backup root");
    fs::write(&backup, b"original backup").expect("write backup");
    let external_root = root.join("external-backups");
    fs::create_dir(&external_root).expect("create external root");
    let external_backup = external_root.join("replace-me.lossless");
    fs::write(&external_backup, b"external backup").expect("write external backup");
    let backup_root = backup.parent().expect("backup parent").to_path_buf();

    let error = delete_backup_with_predelete_hook(root, &backup, || {
        fs::remove_file(&backup).expect("remove original backup");
        fs::remove_dir(&backup_root).expect("remove original backup root");
        create_directory_link(&external_root, &backup_root);
        Ok(())
    })
    .expect_err("replaced reparse root must be rejected");

    assert!(matches!(error, PeerSyncError::Validation { .. }));
    assert_eq!(
        fs::read(&external_backup).expect("read external backup"),
        b"external backup"
    );
}

#[cfg(windows)]
#[test]
fn final_temp_cleanup_validation_rejects_a_replaced_staging_reparse_root() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let app_root = directory.path();
    let staging_root = app_root.join("peer-delta/staging");
    let abandoned = staging_root.join("abandoned");
    fs::create_dir_all(&abandoned).expect("create abandoned stage");
    fs::write(abandoned.join("payload"), b"local payload").expect("write local payload");

    let external_root = app_root.join("external-staging");
    let external_stage = external_root.join("abandoned");
    fs::create_dir_all(&external_stage).expect("create external stage");
    fs::write(external_stage.join("sentinel"), b"external payload")
        .expect("write external payload");

    let error = cleanup_temp_with_predelete_hook(app_root, |root, candidate| {
        assert_eq!(root, staging_root);
        assert_eq!(
            fs::canonicalize(candidate).expect("canonicalize cleanup candidate"),
            fs::canonicalize(&abandoned).expect("canonicalize abandoned stage")
        );
        fs::remove_dir_all(candidate).expect("remove local stage");
        fs::remove_dir(root).expect("remove staging root");
        create_directory_link(&external_root, root);
        Ok(())
    })
    .expect_err("replaced staging root must be rejected");

    assert!(matches!(error, PeerSyncError::Validation { .. }));
    assert_eq!(
        fs::read(external_stage.join("sentinel")).expect("read external payload"),
        b"external payload"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn deletion_boundary_does_not_follow_a_backup_ancestor_replaced_after_validation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let app_root = directory.path().join("app");
    let external_root = directory.path().join("external-backups");
    let backup = app_root.join("peer-clone/activation/backups/replace-me.lossless");
    let backup_root = backup.parent().expect("backup parent").to_path_buf();
    let external_backup = external_root.join("replace-me.lossless");
    fs::create_dir_all(&backup_root).expect("create backup root");
    fs::write(&backup, b"local backup").expect("write local backup");
    fs::create_dir(&external_root).expect("create external backup root");
    fs::write(&external_backup, b"external backup").expect("write external backup");

    let outcome = delete_backup_with_postvalidation_hook(&app_root, &backup, || {
        fs::remove_file(&backup).expect("remove validated local backup");
        if fs::remove_dir(&backup_root).is_ok() {
            create_directory_link(&external_root, &backup_root);
        }
        Ok(())
    });

    assert!(outcome.is_err());
    assert_eq!(
        fs::read(&external_backup).expect("external backup must remain"),
        b"external backup"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn deletion_boundary_does_not_follow_a_temp_ancestor_replaced_after_validation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let app_root = directory.path().join("app");
    let staging_root = app_root.join("peer-delta/staging");
    let abandoned = staging_root.join("abandoned");
    let external_root = directory.path().join("external-staging");
    let external_stage = external_root.join("abandoned");
    let external_sentinel = external_stage.join("sentinel");
    fs::create_dir_all(&abandoned).expect("create abandoned stage");
    fs::write(abandoned.join("payload"), b"local payload").expect("write local payload");
    fs::create_dir_all(&external_stage).expect("create external stage");
    fs::write(&external_sentinel, b"external payload").expect("write external payload");

    let outcome = cleanup_temp_with_postvalidation_hook(&app_root, |root, candidate| {
        assert_eq!(root, staging_root);
        assert_eq!(candidate, abandoned);
        fs::remove_dir_all(candidate).expect("remove validated local stage");
        if fs::remove_dir(root).is_ok() {
            create_directory_link(&external_root, root);
        }
        Ok(())
    });

    assert!(outcome.is_err());
    assert_eq!(
        fs::read(&external_sentinel).expect("external stage must remain"),
        b"external payload"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn intermediate_directory_link_escape_is_rejected_without_external_modification() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let app_root = directory.path().join("app");
    let external_root = directory.path().join("external");
    fs::create_dir(&app_root).expect("create app root");
    fs::create_dir(&external_root).expect("create external root");

    let external_delta = external_root.join("delta");
    let external_stage = external_delta.join("staging/escaped");
    fs::create_dir_all(&external_stage).expect("create external delta stage");
    let delta_sentinel = external_stage.join("sentinel");
    fs::write(&delta_sentinel, b"external delta").expect("write external delta sentinel");
    create_directory_link(&external_delta, &app_root.join("peer-delta"));

    assert!(temp_usage(&app_root).is_err());
    assert!(cleanup_temp(&app_root).is_err());
    assert_eq!(
        fs::read(&delta_sentinel).expect("read external delta sentinel"),
        b"external delta"
    );

    let external_clone = external_root.join("clone");
    let external_backup = external_clone.join("activation/backups/escaped.lossless");
    fs::create_dir_all(external_backup.parent().expect("external backup parent"))
        .expect("create external backup root");
    fs::write(&external_backup, b"external backup").expect("write external backup");
    create_directory_link(&external_clone, &app_root.join("peer-clone"));

    assert!(list_backups(&app_root).is_err());
    assert!(delete_backup(
        &app_root,
        &app_root.join("peer-clone/activation/backups/escaped.lossless")
    )
    .is_err());
    assert_eq!(
        fs::read(&external_backup).expect("read external backup"),
        b"external backup"
    );
}
