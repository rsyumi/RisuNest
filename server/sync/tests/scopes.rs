mod common;
use common::*;
use risunest_sync_server::store::{Device, Store};
use risunest_sync_wire::{
    descriptor::RecordDescriptor, hash, ChangeSet, Domain, RecordVersion, ScopeFence,
    TerminalStatus,
};

fn scoped(store: &Store, device: &Device, key: &str) -> ChangeSet {
    let body = key.as_bytes();
    store.put_object(device, &hash(body), body).unwrap();
    let descriptor = RecordDescriptor {
        scopes: vec!["plugin-storage".into()],
        ..RecordDescriptor::content(hash(body))
    };
    let bytes = descriptor.bytes().unwrap();
    let digest = hash(&bytes);
    store.put_object(device, &digest, &bytes).unwrap();
    let mut c = changes(key, body);
    if let RecordVersion::Live {
        descriptor_hash, ..
    } = &mut c.changes[0].after
    {
        *descriptor_hash = Some(digest);
    }
    c
}
#[test]
fn clear_keeps_original_scope_fence_after_remote_new_key_and_preserves_every_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let b = device(&store);
    let head = store.head().unwrap();
    let c = scoped(&store, &a, "plugin/a");
    let intent = stage(&store, &a, &head, 1, &c);
    let head = store.commit(&a, &intent, &head.etag()).unwrap().head;
    let original_scope = store.scope_version("plugin-storage").unwrap();
    let mut clear = c.clone();
    clear.changes[0].before = c.changes[0].after.clone();
    clear.changes[0].after = RecordVersion::Tombstone {
        deletion_id: "clear-a".into(),
    };
    clear.scope_fences.push(ScopeFence {
        scope: "plugin-storage".into(),
        expected_version: original_scope,
        clear: true,
    });
    let c = scoped(&store, &b, "plugin/b");
    let intent = stage(&store, &b, &head, 1, &c);
    let head = store.commit(&b, &intent, &head.etag()).unwrap().head;
    let intent = stage(&store, &a, &head, 2, &clear);
    let failure = store.commit(&a, &intent, &head.etag()).unwrap();
    assert_eq!(failure.error.as_deref(), Some("scope-fence-mismatch"));
    assert_eq!(store.head().unwrap(), head);
    for key in ["plugin/a", "plugin/b"] {
        assert!(matches!(
            store.record(Domain::Library, key).unwrap(),
            RecordVersion::Live { .. }
        ));
    }
    clear.scope_fences[0].expected_version = store.scope_version("plugin-storage").unwrap();
    let intent = stage(&store, &a, &head, 3, &clear);
    assert_eq!(
        store
            .commit(&a, &intent, &head.etag())
            .unwrap()
            .error
            .as_deref(),
        Some("incomplete-scope-clear")
    );
    let mut delete = c.changes[0].clone();
    delete.before = delete.after;
    delete.after = RecordVersion::Tombstone {
        deletion_id: "clear-b".into(),
    };
    clear.changes.push(delete);
    let intent = stage(&store, &a, &head, 4, &clear);
    assert_eq!(
        store.commit(&a, &intent, &head.etag()).unwrap().status,
        TerminalStatus::Committed
    );
}

#[test]
fn empty_clear_and_clear_then_identical_set_are_durable_scope_operations() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    let head = store.head().unwrap();
    let original = store.scope_version("plugin-storage").unwrap();
    let mut clear = ChangeSet {
        changes: vec![],
        read_fences: vec![],
        scope_fences: vec![ScopeFence {
            scope: "plugin-storage".into(),
            expected_version: original.clone(),
            clear: true,
        }],
    };
    let intent = stage(&store, &a, &head, 1, &clear);
    let receipt = store.commit(&a, &intent, &head.etag()).unwrap();
    assert_eq!(receipt.status, TerminalStatus::Committed);
    assert_ne!(store.scope_version("plugin-storage").unwrap(), original);
    assert_eq!(store.commit(&a, &intent, &head.etag()).unwrap(), receipt);
    let c = scoped(&store, &a, "plugin/a");
    let head = store.head().unwrap();
    let intent = stage(&store, &a, &head, 2, &c);
    let head = store.commit(&a, &intent, &head.etag()).unwrap().head;
    clear.scope_fences[0].expected_version = store.scope_version("plugin-storage").unwrap();
    clear.read_fences.push(risunest_sync_wire::ReadFence {
        domain: Domain::Library,
        key: "plugin/a".into(),
        version: c.changes[0].after.clone(),
    });
    let intent = stage(&store, &a, &head, 3, &clear);
    assert_eq!(
        store.commit(&a, &intent, &head.etag()).unwrap().status,
        TerminalStatus::Committed
    );
    assert_eq!(
        store.record(Domain::Library, "plugin/a").unwrap(),
        c.changes[0].after
    );
}
