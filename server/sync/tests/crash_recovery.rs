mod common;
use common::*;
use risunest_sync_server::store::Store;
use risunest_sync_wire::{hash, TerminalStatus};

#[test]
#[ignore = "child process fixture, invoked by abrupt_process_exit_reopens_wal"]
fn child_exit_without_destructors() {
    let dir =
        std::env::var_os("RISUNEST_SYNTHETIC_CRASH_DIR").expect("synthetic directory required");
    let store = Store::init(std::path::Path::new(&dir)).unwrap();
    let a = device(&store);
    let head = store.head().unwrap();
    store
        .put_object(&a, &hash(b"crash fixture"), b"crash fixture")
        .unwrap();
    let intent = stage(&store, &a, &head, 1, &changes("crash", b"crash fixture"));
    if std::env::var("RISUNEST_SYNTHETIC_CRASH_PHASE").unwrap() == "committed" {
        store.commit(&a, &intent, &head.etag()).unwrap();
    }
    // No Store/SQLite/tempfile destructor, matching process loss after durable publish.
    std::process::exit(77);
}
#[test]
fn abrupt_process_exit_reopens_wal() {
    for phase in ["published", "committed"] {
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child_exit_without_destructors", "--ignored"])
            .env("RISUNEST_SYNTHETIC_CRASH_DIR", dir.path())
            .env("RISUNEST_SYNTHETIC_CRASH_PHASE", phase)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(77));
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            store.head().unwrap().seq.as_str(),
            if phase == "committed" { "1" } else { "0" }
        );
        assert_eq!(
            store.get_object(&hash(b"crash fixture")).unwrap(),
            b"crash fixture"
        );
    }
}

#[test]
fn published_orphan_and_staged_intent_survive_restart_without_advancing_head() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let head = store.head().unwrap();
    store.put_object(&a, &hash(b"object"), b"object").unwrap();
    let intent = stage(&store, &a, &head, 1, &changes("a", b"object"));
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.head().unwrap(), head);
    assert_eq!(store.get_object(&hash(b"object")).unwrap(), b"object");
    assert_eq!(
        store.commit(&a, &intent, &head.etag()).unwrap().status,
        TerminalStatus::Committed
    );
}
#[test]
fn lost_response_retry_after_restart_returns_exact_receipt_without_duplicate_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let head = store.head().unwrap();
    store.put_object(&a, &hash(b"object"), b"object").unwrap();
    let intent = stage(&store, &a, &head, 1, &changes("a", b"object"));
    let receipt = store.commit(&a, &intent, &head.etag()).unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.commit(&a, &intent, &head.etag()).unwrap(), receipt);
    assert_eq!(store.head().unwrap().seq.as_str(), "1");
}
#[test]
fn sqlite_failure_rolls_back_records_head_receipt_and_watermark_together() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let head = store.head().unwrap();
    store.put_object(&a, &hash(b"object"), b"object").unwrap();
    let intent = stage(&store, &a, &head, 1, &changes("a", b"object"));
    let db = rusqlite::Connection::open(dir.path().join("metadata.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER injected_failure BEFORE INSERT ON receipts BEGIN SELECT RAISE(ABORT,'synthetic storage failure'); END;").unwrap();
    assert!(store.commit(&a, &intent, &head.etag()).is_err());
    assert_eq!(store.head().unwrap(), head);
    assert_eq!(
        store.record("a").unwrap(),
        risunest_sync_wire::RecordVersion::Absent
    );
    db.execute_batch("DROP TRIGGER injected_failure").unwrap();
    drop(db);
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store.commit(&a, &intent, &head.etag()).unwrap().status,
        TerminalStatus::Committed
    );
}
#[test]
fn pruned_terminal_receipt_cannot_reexecute_below_durable_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let head = store.head().unwrap();
    let intent = stage(&store, &a, &head, 42, &changes("a", b"missing"));
    assert_eq!(
        store.commit(&a, &intent, &head.etag()).unwrap().status,
        TerminalStatus::Failed
    );
    drop(store);
    let db = rusqlite::Connection::open(dir.path().join("metadata.sqlite")).unwrap();
    db.execute("DELETE FROM receipts", []).unwrap();
    drop(db);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store.commit(&a, &intent, &head.etag()).unwrap_err().code,
        "operation-history-expired"
    );
    assert_eq!(store.head().unwrap(), head);
}
#[test]
fn corrupt_or_unregistered_object_is_never_served() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let digest = hash(b"object");
    store.put_object(&a, &digest, b"object").unwrap();
    std::fs::write(
        dir.path().join("objects").join(&digest[..2]).join(&digest),
        b"broken",
    )
    .unwrap();
    assert_eq!(
        store.get_object(&digest).unwrap_err().code,
        "corrupt-object"
    );
    let unregistered = hash(b"orphan");
    let path = dir.path().join("objects").join(&unregistered[..2]);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join(&unregistered), b"orphan").unwrap();
    assert_eq!(
        store.get_object(&unregistered).unwrap_err().code,
        "object-not-found"
    );
}

#[test]
fn failed_staging_write_never_registers_an_object_or_changes_the_head() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let head = store.head().unwrap();
    let staging = dir.path().join("staging");
    std::fs::remove_dir(&staging).unwrap();
    std::fs::write(&staging, b"synthetic unavailable storage").unwrap();
    assert!(store.put_object(&a, &hash(b"x"), b"x").is_err());
    assert!(store.object_size(&hash(b"x")).unwrap().is_none());
    assert_eq!(store.head().unwrap(), head);
}
