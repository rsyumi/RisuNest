use super::maintenance::{cleanup_temp, delete_backup, list_backups, temp_usage};
use super::PeerSyncError;
use crate::asset_repository::job_pins::{CasJobKind, DurableCasJob};
use std::fs;

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
    assert!(
        matches!(delete_backup(root, &backup), Err(PeerSyncError::Validation(message)) if message == "peer-backup-in-use")
    );
}
