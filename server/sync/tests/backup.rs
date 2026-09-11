mod common;
use risunest_sync_server::store::Store;
use risunest_sync_wire::{hash, CommitIntent};
#[test]
fn cold_backup_restores_exact_objects_under_a_new_epoch_and_rejects_corruption() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let backup = directory.path().join("backup");
    let restored = directory.path().join("restored");
    let store = Store::init(&source).unwrap();
    let credential = store.add_device().unwrap();
    let device = store
        .authenticate(&credential.library_id, &credential.token)
        .unwrap();
    let bytes = b"synthetic exact bytes\0\xff";
    let digest = hash(bytes);
    store.put_object(&device, &digest, bytes).unwrap();
    let staged = store
        .stage_changes(&device, &common::changes("key", bytes))
        .unwrap();
    let intent = CommitIntent {
        device_operation_seq: 1.into(),
        expected_head: store.head().unwrap(),
        changes_digest: staged.changes_digest,
        staged_changes_id: staged.staged_changes_id,
    };
    let receipt = store
        .commit(&device, &intent, &intent.expected_head.etag())
        .unwrap();
    let manifest = store.backup(&backup).unwrap();
    assert_eq!(manifest.head, receipt.head);
    assert!(store.backup(&backup).is_err());
    let restored = Store::restore_backup(&backup, &restored).unwrap();
    assert_eq!(restored.get_object(&digest).unwrap(), bytes);
    assert_eq!(restored.head().unwrap().seq, 0.into());
    assert_ne!(restored.head().unwrap().epoch, receipt.head.epoch);
    assert_eq!(restored.head().unwrap().library_id, receipt.head.library_id);
    assert!(restored
        .operation_status(&device, &receipt.operation_id)
        .is_err());
    std::fs::write(
        backup.join("objects").join(&digest[..2]).join(&digest),
        b"corrupt",
    )
    .unwrap();
    assert!(Store::restore_backup(&backup, &directory.path().join("bad-restore")).is_err());
}
