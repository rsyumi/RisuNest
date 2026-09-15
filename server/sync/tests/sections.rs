mod common;
use common::*;
use risunest_sync_server::store::{ChangeCursor, Store};
use risunest_sync_wire::{hash, ChangeSet, Domain, RecordChange, RecordVersion};

fn section_changes(domain: Domain, key: &str, body: &[u8]) -> ChangeSet {
    let mut set = changes(key, body);
    set.changes[0].domain = domain;
    set
}
fn from_start(
    store: &Store,
    epoch: &str,
    through: &risunest_sync_wire::Sequence,
    domain: Domain,
) -> risunest_sync_server::Result<usize> {
    Ok(store
        .changes(
            epoch,
            &ChangeCursor::after_commit(0.into()),
            through,
            &[domain],
            128,
        )?
        .entries
        .len())
}

#[test]
fn sections_share_one_sequence_while_keeping_separate_state_and_records() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    store.put_object(&a, &hash(b"x"), b"x").unwrap();
    let genesis = store.head().unwrap();
    genesis.validate().unwrap();

    let intent = stage(
        &store,
        &a,
        &genesis,
        1,
        &section_changes(Domain::Hypa, "cache", b"x"),
    );
    let h1 = store.commit(&a, &intent, &genesis.etag()).unwrap().head;
    assert_eq!(h1.seq.as_str(), "1");
    assert_eq!(h1.section(Domain::Hypa).unwrap().changed_seq, h1.seq);
    assert_eq!(
        h1.section(Domain::Library).unwrap(),
        genesis.section(Domain::Library).unwrap()
    );
    assert_ne!(
        h1.section(Domain::Hypa).unwrap().state_id,
        genesis.section(Domain::Hypa).unwrap().state_id
    );

    // The same key in another section is a different record, left untouched here.
    let intent = stage(&store, &a, &h1, 2, &changes("cache", b"x"));
    let h2 = store.commit(&a, &intent, &h1.etag()).unwrap().head;
    assert_eq!(
        store.record(Domain::Hypa, "cache").unwrap(),
        store.record(Domain::Library, "cache").unwrap()
    );
    assert_eq!(
        h2.section(Domain::Hypa).unwrap(),
        h1.section(Domain::Hypa).unwrap()
    );
    assert_eq!(h2.section(Domain::Library).unwrap().changed_seq, h2.seq);
    assert_eq!(
        store.record(Domain::LocalPlugins, "cache").unwrap(),
        RecordVersion::Absent
    );

    assert_eq!(
        from_start(&store, &h2.epoch, &h2.seq, Domain::Hypa).unwrap(),
        1
    );
    assert_eq!(
        from_start(&store, &h2.epoch, &h2.seq, Domain::Library).unwrap(),
        1
    );
    assert_eq!(
        from_start(&store, &h2.epoch, &h2.seq, Domain::LocalPlugins).unwrap(),
        0
    );
    let both = store
        .changes(
            &h2.epoch,
            &ChangeCursor::after_commit(0.into()),
            &h2.seq,
            &[Domain::Library, Domain::Hypa, Domain::Hypa],
            128,
        )
        .unwrap();
    assert_eq!(both.entries.len(), 2);
    assert_eq!(both.domains, [Domain::Hypa, Domain::Library]);
    assert_eq!(
        store
            .changes(
                &h2.epoch,
                &ChangeCursor::after_commit(0.into()),
                &h2.seq,
                &[],
                128
            )
            .err()
            .unwrap()
            .code,
        "invalid-domains"
    );
}

#[test]
fn a_section_acknowledgement_never_releases_another_sections_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    store.put_object(&a, &hash(b"x"), b"x").unwrap();
    let head = store.head().unwrap();
    let hypa = section_changes(Domain::Hypa, "cache", b"x");
    let intent = stage(&store, &a, &head, 1, &hypa);
    let head = store.commit(&a, &intent, &head.etag()).unwrap().head;
    let mut plugins = section_changes(Domain::LocalPlugins, "owner/space/key", b"x");
    let intent = stage(&store, &a, &head, 2, &plugins);
    let head = store.commit(&a, &intent, &head.etag()).unwrap().head;
    plugins.changes[0].before = plugins.changes[0].after.clone();
    plugins.changes[0].after = RecordVersion::Tombstone {
        deletion_id: "removed".into(),
    };
    let intent = stage(&store, &a, &head, 3, &plugins);
    let head = store.commit(&a, &intent, &head.etag()).unwrap().head;

    store
        .acknowledge(&a, &head.epoch, &section_ack(Domain::Hypa, &head.seq))
        .unwrap();
    assert_eq!(store.section_ack_floor(Domain::Hypa).unwrap(), head.seq);
    assert_eq!(
        store
            .section_ack_floor(Domain::LocalPlugins)
            .unwrap()
            .as_str(),
        "0"
    );
    assert_eq!(
        store.section_ack_floor(Domain::Library).unwrap().as_str(),
        "0"
    );

    store.maintain().unwrap();
    let reclaimed = store.head().unwrap();
    reclaimed.validate().unwrap();
    assert_eq!(reclaimed.section(Domain::Hypa).unwrap().gc_floor, head.seq);
    assert_eq!(
        reclaimed
            .section(Domain::LocalPlugins)
            .unwrap()
            .gc_floor
            .as_str(),
        "0"
    );
    assert_eq!(reclaimed.min_retained_seq.as_str(), "0");
    // The plugin deletion is still deliverable incrementally; hypa is not.
    assert_eq!(
        from_start(&store, &head.epoch, &head.seq, Domain::LocalPlugins).unwrap(),
        2
    );
    assert_eq!(
        from_start(&store, &head.epoch, &head.seq, Domain::Hypa)
            .unwrap_err()
            .code,
        "checkpoint-required"
    );
    assert_eq!(
        store
            .pin_changes(&a, &head.epoch, &0.into(), &[Domain::Hypa])
            .err()
            .unwrap()
            .code,
        "checkpoint-required"
    );
    store
        .pin_changes(&a, &head.epoch, &0.into(), &[Domain::LocalPlugins])
        .unwrap();
    assert_eq!(
        store
            .acknowledge(&a, &head.epoch, &section_ack(Domain::Hypa, &0.into()))
            .unwrap_err()
            .code,
        "invalid-ack"
    );
}

#[test]
fn checkpoints_carry_only_the_requested_sections() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(dir.path()).unwrap();
    let a = device(&store);
    store.put_object(&a, &hash(b"x"), b"x").unwrap();
    let head = store.head().unwrap();
    let mut set = section_changes(Domain::Hypa, "cache", b"x");
    set.changes.push(RecordChange {
        domain: Domain::Library,
        ..set.changes[0].clone()
    });
    set.changes.push(RecordChange {
        domain: Domain::LocalPlugins,
        ..set.changes[0].clone()
    });
    let intent = stage(&store, &a, &head, 1, &set);
    store.commit(&a, &intent, &head.etag()).unwrap();

    let checkpoint = store
        .create_checkpoint(&a, &[Domain::Hypa, Domain::LocalPlugins])
        .unwrap();
    assert_eq!(checkpoint.domains, [Domain::Hypa, Domain::LocalPlugins]);
    let page = store
        .checkpoint_page(&a, &checkpoint.checkpoint_id, None, 128)
        .unwrap();
    assert_eq!(
        page.records
            .iter()
            .map(|record| record.domain)
            .collect::<Vec<_>>(),
        [Domain::Hypa, Domain::LocalPlugins]
    );
    assert!(page.next.is_none());
    let first = store
        .checkpoint_page(&a, &checkpoint.checkpoint_id, None, 1)
        .unwrap();
    let cursor = first.next.unwrap();
    assert_eq!(cursor.domain, Domain::Hypa);
    let second = store
        .checkpoint_page(&a, &checkpoint.checkpoint_id, Some(&cursor), 128)
        .unwrap();
    assert_eq!(second.records.len(), 1);
    assert_eq!(second.records[0].domain, Domain::LocalPlugins);
    assert_eq!(
        store.create_checkpoint(&a, &[]).err().unwrap().code,
        "invalid-domains"
    );
}
