#![allow(dead_code)]
use risunest_sync_server::store::{Device, Store};
use risunest_sync_wire::{
    hash, ChangeSet, CommitIntent, Domain, RecordChange, RecordVersion, RemoteHead, Sequence,
};
use std::collections::BTreeMap;

pub const LIBRARY: [Domain; 1] = [Domain::Library];

/// Acknowledge every section at one point. Section isolation has its own tests.
pub fn acks(seq: &Sequence) -> BTreeMap<Domain, Sequence> {
    Domain::ALL
        .into_iter()
        .map(|domain| (domain, seq.clone()))
        .collect()
}
pub fn section_ack(domain: Domain, seq: &Sequence) -> BTreeMap<Domain, Sequence> {
    BTreeMap::from([(domain, seq.clone())])
}
pub fn device(store: &Store) -> Device {
    let credential = store.add_device().unwrap();
    store
        .authenticate(&credential.library_id, &credential.token)
        .unwrap()
}
pub fn changes(key: &str, body: &[u8]) -> ChangeSet {
    ChangeSet {
        changes: vec![RecordChange {
            domain: Domain::Library,
            key: key.into(),
            before: RecordVersion::Absent,
            after: RecordVersion::Live {
                object_hash: hash(body),
                descriptor_hash: None,
            },
        }],
        read_fences: vec![],
        scope_fences: vec![],
    }
}
pub fn stage(
    store: &Store,
    device: &Device,
    head: &RemoteHead,
    seq: u64,
    changes: &ChangeSet,
) -> CommitIntent {
    let staged = store.stage_changes(device, changes).unwrap();
    CommitIntent {
        device_operation_seq: seq.into(),
        expected_head: head.clone(),
        changes_digest: staged.changes_digest,
        staged_changes_id: staged.staged_changes_id,
    }
}
