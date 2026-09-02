use super::maintenance::{
    cleanup_temp, cleanup_temp_with_predelete_hook, delete_backup,
    delete_backup_with_predelete_hook, desktop_clone_backup_job_is_active, list_backups,
    temp_usage, PeerBackupDeleteError,
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
            candidate,
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
