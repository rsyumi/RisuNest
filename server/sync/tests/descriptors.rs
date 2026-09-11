mod common;
use common::*;
use risunest_sync_server::store::{Device, Store};
use risunest_sync_wire::{
    descriptor::{build_reference_tree, RecordDescriptor},
    hash, ChangeSet, RecordChange, RecordVersion, TerminalStatus,
};

fn related(store: &Store, device: &Device, key: &str, target: &str) -> ChangeSet {
    let (root, pages) = build_reference_tree(&[target.to_owned()], true).unwrap();
    for (hash, bytes) in pages {
        store.put_object(device, &hash, &bytes).unwrap();
    }
    let body = b"synthetic content";
    store.put_object(device, &hash(body), body).unwrap();
    let descriptor = RecordDescriptor {
        relation_root: root,
        ..RecordDescriptor::content(hash(body))
    };
    let bytes = descriptor.bytes().unwrap();
    let digest = hash(&bytes);
    store.put_object(device, &digest, &bytes).unwrap();
    ChangeSet {
        changes: vec![RecordChange {
            key: key.into(),
            before: RecordVersion::Absent,
            after: RecordVersion::Live {
                object_hash: hash(body),
                descriptor_hash: Some(digest),
            },
        }],
        read_fences: vec![],
        scope_fences: vec![],
    }
}
#[test]
fn parent_delete_cannot_orphan_child_and_joint_delete_is_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let head = store.head().unwrap();
    let mut c = related(&store, &a, "child", "parent");
    c.changes
        .push(changes("parent", b"synthetic content").changes.remove(0));
    let intent = stage(&store, &a, &head, 1, &c);
    let head = store.commit(&a, &intent, &head.etag()).unwrap().head;
    let delete = |key: &str| RecordChange {
        key: key.into(),
        before: store.record(key).unwrap(),
        after: RecordVersion::Tombstone {
            deletion_id: format!("delete-{key}"),
        },
    };
    let c = ChangeSet {
        changes: vec![delete("parent")],
        read_fences: vec![],
        scope_fences: vec![],
    };
    let intent = stage(&store, &a, &head, 2, &c);
    let failure = store.commit(&a, &intent, &head.etag()).unwrap();
    assert_eq!(failure.error.as_deref(), Some("referenced-record-deleted"));
    assert_eq!(store.head().unwrap(), head);
    let c = ChangeSet {
        changes: vec![delete("child"), delete("parent")],
        read_fences: vec![],
        scope_fences: vec![],
    };
    let intent = stage(&store, &a, &head, 3, &c);
    assert_eq!(
        store.commit(&a, &intent, &head.etag()).unwrap().status,
        TerminalStatus::Committed
    );
}
#[test]
fn missing_relation_fails_with_terminal_receipt_and_no_partial_record() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let head = store.head().unwrap();
    let c = related(&store, &a, "child", "missing");
    let intent = stage(&store, &a, &head, 1, &c);
    let failed = store.commit(&a, &intent, &head.etag()).unwrap();
    assert_eq!(failed.error.as_deref(), Some("missing-related-record"));
    assert_eq!(store.record("child").unwrap(), RecordVersion::Absent);
    assert_eq!(store.head().unwrap(), head);
    assert_eq!(store.commit(&a, &intent, &head.etag()).unwrap(), failed);
}
#[test]
fn descriptor_content_and_missing_dependency_cannot_be_forged() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let mut c = related(&store, &a, "child", "parent");
    if let RecordVersion::Live { object_hash, .. } = &mut c.changes[0].after {
        *object_hash = hash(b"wrong");
    }
    assert_eq!(
        store.stage_changes(&a, &c).err().unwrap().code,
        "descriptor-object-mismatch"
    );
    let (root, pages) = build_reference_tree(&[hash(b"missing dependency")], false).unwrap();
    for (digest, bytes) in pages {
        store.put_object(&a, &digest, &bytes).unwrap();
    }
    let descriptor = RecordDescriptor {
        dependency_root: root,
        ..RecordDescriptor::content(hash(b"synthetic content"))
    };
    let bytes = descriptor.bytes().unwrap();
    let digest = hash(&bytes);
    store.put_object(&a, &digest, &bytes).unwrap();
    c.changes[0].after = RecordVersion::Live {
        object_hash: descriptor.object_hash,
        descriptor_hash: Some(digest),
    };
    assert_eq!(
        store.stage_changes(&a, &c).err().unwrap().code,
        "missing-dependency"
    );
}
