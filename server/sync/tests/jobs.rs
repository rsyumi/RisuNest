mod common;
use common::*;
use risunest_sync_server::store::{CommitSubmission, Store};
use risunest_sync_wire::{hash, TerminalStatus};

#[test]
fn reserved_job_survives_restart_rechecks_head_and_has_terminal_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let b = device(&store);
    store.put_object(&a, &hash(b"x"), b"x").unwrap();
    let head = store.head().unwrap();
    let intent = stage(&store, &a, &head, 1, &changes("a", b"x"));
    assert!(matches!(
        store.submit_commit(&a, &intent, &head.etag()).unwrap(),
        CommitSubmission::Pending { .. }
    ));
    let mut other = intent.clone();
    other.device_operation_seq = 2.into();
    assert_eq!(
        store
            .submit_commit(&a, &other, &head.etag())
            .err()
            .unwrap()
            .code,
        "device-operation-active"
    );
    assert_eq!(
        store
            .cancel_staged_changes(&a, &intent.staged_changes_id)
            .unwrap_err()
            .code,
        "device-operation-active"
    );
    let b_intent = stage(&store, &b, &head, 1, &changes("b", b"x"));
    store.commit(&b, &b_intent, &head.etag()).unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert!(store.run_pending_commit().unwrap());
    assert!(!store.run_pending_commit().unwrap());
    match store.submit_commit(&a, &intent, &head.etag()).unwrap() {
        CommitSubmission::Terminal(r) => assert_eq!(r.status, TerminalStatus::Stale),
        _ => panic!("job must be terminal"),
    }
    assert_eq!(store.head().unwrap().seq.as_str(), "1");
}
