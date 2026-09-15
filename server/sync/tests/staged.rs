mod common;
use common::*;
use risunest_sync_server::store::{ChangeCursor, Store};
use risunest_sync_wire::{hash, ChangeSet, CommitIntent, Domain, RecordVersion, TerminalStatus};

fn page(start: usize, count: usize) -> ChangeSet {
    ChangeSet {
        changes: (start..start + count)
            .map(|i| changes(&format!("key-{i:06}"), b"x").changes.remove(0))
            .collect(),
        read_fences: vec![],
        scope_fences: vec![],
    }
}

#[test]
fn expired_staging_cannot_resume_or_consume_quota_but_reserved_commit_is_retained() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    store.put_object(&a, &hash(b"x"), b"x").unwrap();
    let staged = store.stage_changes(&a, &page(0, 1)).unwrap();
    let db = rusqlite::Connection::open(dir.path().join("metadata.sqlite")).unwrap();
    db.execute("UPDATE staged_changes SET expires=0", [])
        .unwrap();
    assert_eq!(
        store
            .changes_progress(&a, &staged.staged_changes_id)
            .err()
            .unwrap()
            .code,
        "staging-expired"
    );
    assert_eq!(
        store
            .put_changes_page(&a, &staged.staged_changes_id, 0, &page(0, 1))
            .unwrap_err()
            .code,
        "staging-expired"
    );
    assert!(store.seal_changes(&a, &staged.staged_changes_id).is_err());
    for _ in 0..16 {
        store.begin_changes(&a).unwrap();
    }
    assert!(store.begin_changes(&a).is_err());
    db.execute("UPDATE staged_changes SET expires=0", [])
        .unwrap();
    let staged = store.stage_changes(&a, &page(0, 1)).unwrap();
    let head = store.head().unwrap();
    let intent = CommitIntent {
        device_operation_seq: 1.into(),
        expected_head: head.clone(),
        changes_digest: staged.changes_digest,
        staged_changes_id: staged.staged_changes_id,
    };
    store.submit_commit(&a, &intent, &head.etag()).unwrap();
    db.execute("UPDATE staged_changes SET expires=0", [])
        .unwrap();
    store.maintain().unwrap();
    assert!(store
        .changes_progress(&a, &intent.staged_changes_id)
        .is_ok());
    assert!(store.run_pending_commit().unwrap());
    assert_eq!(store.head().unwrap().seq, 1.into());
}

#[test]
fn multi_page_restart_replay_and_atomic_journal_beyond_1024() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let b = device(&store);
    store.put_object(&a, &hash(b"x"), b"x").unwrap();
    let head = store.head().unwrap();
    let id = store.begin_changes(&a).unwrap();
    assert_eq!(
        store
            .put_changes_page(&a, &id, 1, &page(0, 512))
            .unwrap_err()
            .code,
        "page-gap"
    );
    store.put_changes_page(&a, &id, 0, &page(0, 512)).unwrap();
    assert_eq!(
        store
            .put_changes_page(&b, &id, 0, &page(0, 512))
            .unwrap_err()
            .code,
        "staging-not-found"
    );
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store.changes_progress(&a, &id).unwrap().next_page.as_str(),
        "1"
    );
    store.put_changes_page(&a, &id, 0, &page(0, 512)).unwrap();
    assert_eq!(
        store
            .put_changes_page(&a, &id, 0, &page(0, 511))
            .unwrap_err()
            .code,
        "page-intent-conflict"
    );
    store.put_changes_page(&a, &id, 1, &page(512, 512)).unwrap();
    store
        .put_changes_page(&a, &id, 2, &page(1024, 512))
        .unwrap();
    assert_eq!(
        store.record(Domain::Library, "key-000000").unwrap(),
        RecordVersion::Absent
    );
    let sealed = store.seal_changes(&a, &id).unwrap();
    let intent = CommitIntent {
        expected_head: head.clone(),
        device_operation_seq: 1.into(),
        changes_digest: sealed.changes_digest,
        staged_changes_id: id,
    };
    let receipt = store.commit(&a, &intent, &head.etag()).unwrap();
    assert_eq!(receipt.status, TerminalStatus::Committed);
    assert_eq!(receipt.head.seq.as_str(), "1");
    let mut cursor = ChangeCursor::after_commit(0.into());
    let mut count = 0;
    loop {
        let page = store
            .changes(&head.epoch, &cursor, &receipt.head.seq, &LIBRARY, 127)
            .unwrap();
        count += page.entries.len();
        cursor = page.next;
        if !page.has_more {
            break;
        }
    }
    assert_eq!(count, 1536);
    assert!(store
        .changes(
            &head.epoch,
            &ChangeCursor::after_commit(1.into()),
            &receipt.head.seq,
            &LIBRARY,
            128
        )
        .unwrap()
        .entries
        .is_empty());
}

#[test]
fn partition_independent_digest_and_sql_failure_keeps_whole_stage() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    store.put_object(&a, &hash(b"x"), b"x").unwrap();
    let id = store.begin_changes(&a).unwrap();
    store.put_changes_page(&a, &id, 0, &page(0, 20)).unwrap();
    store.put_changes_page(&a, &id, 1, &page(20, 20)).unwrap();
    assert_eq!(
        store.seal_changes(&a, &id).unwrap().changes_digest,
        page(0, 40).digest().unwrap()
    );
    let id = store.begin_changes(&a).unwrap();
    let db = rusqlite::Connection::open(dir.path().join("metadata.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER fail_stage BEFORE INSERT ON staged_records WHEN NEW.key='key-000010' BEGIN SELECT RAISE(ABORT,'synthetic'); END;").unwrap();
    assert!(store.put_changes_page(&a, &id, 0, &page(0, 20)).is_err());
    assert_eq!(
        store.changes_progress(&a, &id).unwrap().next_page.as_str(),
        "0"
    );
    db.execute_batch("DROP TRIGGER fail_stage").unwrap();
    store.put_changes_page(&a, &id, 0, &page(0, 20)).unwrap();
}
