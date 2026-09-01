use super::maintenance::{
    cleanup_temp, delete_backup, delete_backup_with_predelete_hook,
    desktop_clone_backup_job_is_active, list_backups, temp_usage,
};
use super::PeerSyncError;
use crate::asset_repository::job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob};
use std::fs;
#[cfg(any(unix, windows))]
use std::path::Path;

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
        "peer-clone/activation/backups/pre-clone-{}-{id}.lossless",
        "a".repeat(64)
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
        "peer-clone-activation/backups/pre-clone-{}-{id}.lossless",
        "b".repeat(64)
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
