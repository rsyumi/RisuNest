mod common;
use common::*;
use risunest_sync_server::store::{ChangeCursor, Store};
use risunest_sync_wire::{hash, Domain, RecordChange, RecordVersion};

#[test]
fn fixed_through_pagination_excludes_concurrent_commits_and_followup_gets_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let first = store.head().unwrap();
    store.put_object(&a, &hash(b"x"), b"x").unwrap();
    let mut c = changes("a", b"x");
    c.changes.push(RecordChange {
        key: "b".into(),
        ..c.changes[0].clone()
    });
    let intent = stage(&store, &a, &first, 1, &c);
    let h1 = store.commit(&a, &intent, &first.etag()).unwrap().head;
    let genesis = store
        .changes(
            &first.epoch,
            &ChangeCursor::after_commit(0.into()),
            &0.into(),
            &LIBRARY,
            1,
        )
        .unwrap();
    assert_eq!(genesis.through, first);
    assert!(genesis.entries.is_empty());
    let page1 = store
        .changes(
            &h1.epoch,
            &ChangeCursor::after_commit(0.into()),
            &h1.seq,
            &LIBRARY,
            1,
        )
        .unwrap();
    assert!(page1.has_more);
    assert_eq!(page1.entries[0].change.key, "a");
    let intent = stage(&store, &a, &h1, 2, &changes("c", b"x"));
    let h2 = store.commit(&a, &intent, &h1.etag()).unwrap().head;
    let page2 = store
        .changes(&h1.epoch, &page1.next, &h1.seq, &LIBRARY, 1)
        .unwrap();
    assert!(!page2.has_more);
    assert_eq!(page2.through, h1);
    assert_eq!(page2.entries[0].change.key, "b");
    let page3 = store
        .changes(
            &h2.epoch,
            &ChangeCursor::after_commit(h1.seq),
            &h2.seq,
            &LIBRARY,
            10,
        )
        .unwrap();
    assert_eq!(page3.entries.len(), 1);
    assert_eq!(page3.entries[0].change.key, "c");
    assert!(store
        .changes("wrong", &page3.next, &h2.seq, &LIBRARY, 10)
        .is_err());
}
#[test]
fn tombstones_keep_identity_and_do_not_release_other_device_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let b = device(&store);
    let first = store.head().unwrap();
    store.put_object(&a, &hash(b"x"), b"x").unwrap();
    let c = changes("a", b"x");
    let intent = stage(&store, &a, &first, 1, &c);
    let h1 = store.commit(&a, &intent, &first.etag()).unwrap().head;
    let mut delete = c.clone();
    delete.changes[0].before = c.changes[0].after.clone();
    delete.changes[0].after = RecordVersion::Tombstone {
        deletion_id: "synthetic-deletion".into(),
    };
    let intent = stage(&store, &a, &h1, 2, &delete);
    let h2 = store.commit(&a, &intent, &h1.etag()).unwrap().head;
    store.acknowledge(&a, &h2.epoch, &acks(&h2.seq)).unwrap();
    store.acknowledge(&b, &h2.epoch, &acks(&0.into())).unwrap();
    let page = store
        .changes(
            &h2.epoch,
            &ChangeCursor::after_commit(0.into()),
            &h2.seq,
            &LIBRARY,
            10,
        )
        .unwrap();
    assert_eq!(page.entries.len(), 2);
    assert_eq!(
        store.record(Domain::Library, "a").unwrap(),
        delete.changes[0].after
    );
    assert_eq!(store.get_object(&hash(b"x")).unwrap(), b"x");
}
