// Extracted from logical_delta_target.rs so the target module stays
// reviewable; this file is the same `tests` module and keeps every
// `super::` path unchanged.

use super::*;
use crate::{
    asset_repository::{
        job_pins::CasJobKind,
        owner_manifest_codec::{encode_owner_manifest, OwnerManifestEntry},
        PayloadCas,
    },
    peer_sync::{
        execute_logical_delta_pull,
        logical_delta::{
            build_logical_manifest, encode_asset_alias_metadata, encode_logical_manifest,
            encode_logical_record_key, encode_message_page, LogicalAssetAliasMetadata,
            LogicalManifest, LogicalManifestBuilderInput, LogicalManifestLiveRecord,
            LogicalManifestObject, LogicalManifestRecord, LogicalManifestTombstoneRecord,
            LogicalOwnerHead, LogicalOwnerLocator, LogicalRecordEnvelope, LogicalRecordLocator,
            ProjectedLogicalRecord, LOGICAL_MANIFEST_SCHEMA,
        },
        LogicalDeltaActivation, LogicalDeltaApplyOperation, LogicalDeltaObject,
        LogicalDeltaObjectSource, ReadyLogicalDeltaPlan,
    },
    persistent_store::{
        logical_index::LogicalIndexBuildRequest, snapshot, AssetOwnerHead, AssetOwnerLocator,
        PersistentStore, SyncGenerationIdentity, VerifiedSyncDeviceRegistration, WorkingSetCommit,
    },
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Read},
};

struct EmptySource {
    content_gets: usize,
}

impl LogicalDeltaObjectSource for EmptySource {
    fn open_object(
        &mut self,
        _object: &LogicalDeltaObject,
    ) -> Result<Box<dyn Read>, crate::peer_sync::PeerSyncError> {
        self.content_gets += 1;
        Ok(Box::new(Cursor::new(Vec::<u8>::new())))
    }
}

struct MapSource {
    objects: BTreeMap<String, Vec<u8>>,
    content_gets: usize,
}

struct PrefixPlusOneReader {
    bytes: Vec<u8>,
    position: usize,
}

impl Read for PrefixPlusOneReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        assert!(
            self.position < self.bytes.len(),
            "staging read beyond the declared payload plus one trailing byte"
        );
        let read = output.len().min(self.bytes.len() - self.position);
        output[..read].copy_from_slice(&self.bytes[self.position..self.position + read]);
        self.position += read;
        Ok(read)
    }
}

impl LogicalDeltaObjectSource for MapSource {
    fn open_object(
        &mut self,
        object: &LogicalDeltaObject,
    ) -> Result<Box<dyn Read>, crate::peer_sync::PeerSyncError> {
        self.content_gets += 1;
        let bytes = self.objects.get(&object.hash).ok_or_else(|| {
            crate::peer_sync::PeerSyncError::Transport(format!(
                "fixture object {} is missing",
                object.hash
            ))
        })?;
        Ok(Box::new(Cursor::new(bytes.clone())))
    }
}

fn open_fixture() -> (tempfile::TempDir, PersistentStore, PayloadCas) {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
    (directory, store, cas)
}

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn descriptor(bytes: &[u8]) -> LogicalManifestObject {
    LogicalManifestObject {
        hash: hash(bytes),
        size: bytes.len() as u64,
    }
}

fn bytes_hash(hash: &str) -> [u8; 32] {
    hex::decode(hash)
        .expect("decode fixture hash")
        .try_into()
        .expect("32-byte hash")
}

fn store_remote_base_manifest(
    cas: &PayloadCas,
    local: &LogicalManifest,
    generation: &str,
) -> (LogicalManifest, Vec<u8>, String) {
    let mut base = local.clone();
    base.generation = generation.to_owned();
    base.parent_generation = None;
    let bytes = encode_logical_manifest(&base).expect("encode remote common-base manifest");
    let manifest_hash = hash(&bytes);
    let prepared = cas
        .prepare_bytes(&bytes)
        .expect("store remote common-base manifest");
    assert_eq!(prepared.content_hash, manifest_hash);
    (base, bytes, manifest_hash)
}

fn expect_begin_validation(
    target: &mut PersistentLogicalDeltaTarget<'_>,
    plan: &ReadyLogicalDeltaPlan,
) {
    match target.begin(plan) {
        Err(PeerSyncError::Validation(_)) => {}
        Err(error) => panic!("expected validation error, got {error}"),
        Ok(stage) => {
            target
                .abort(stage)
                .expect("abort unexpectedly accepted stage");
            panic!("forged logical delta plan reached staging");
        }
    }
}

fn expect_begin_merge_conflict(
    target: &mut PersistentLogicalDeltaTarget<'_>,
    plan: &ReadyLogicalDeltaPlan,
) {
    match target.begin(plan) {
        Err(PeerSyncError::LogicalMergeConflict { .. }) => {}
        Err(error) => panic!("expected logical merge conflict, got {error}"),
        Ok(stage) => {
            target
                .abort(stage)
                .expect("abort unexpectedly accepted conflict stage");
            panic!("conflicting logical delta plan reached staging");
        }
    }
}

#[test]
fn stage_payload_rejects_trailing_bytes_without_reading_past_the_first_extra_byte() {
    let (directory, mut store, cas) = open_fixture();
    let remote = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "remote-1".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: Some("remote-0".to_owned()),
        source_revision: 1,
        records: vec![ProjectedLogicalRecord::live(
            LogicalRecordLocator::Plugin {
                storage_key: "remote-plugin".to_owned(),
            },
            LogicalRecordEnvelope::Plugin {
                ordinal: 0,
                value: json!({"remote":true}),
            },
            vec![],
        )],
    })
    .expect("build one-record remote manifest");
    let record = &remote.record_objects[0].object;
    let staging_root = directory.path().join("logical-delta-staging");
    let staging_directory = staging_root.join("staging-logical-bounded-reader");
    fs::create_dir_all(&staging_directory).expect("create staging directory");
    let mut target = PersistentLogicalDeltaTarget::new(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote.manifest_bytes,
        &staging_root,
    )
    .expect("open bounded-reader target");
    let mut stage = PersistentLogicalDeltaStage::Changed {
        expected_base: PeerBase {
            generation_id: "remote-0".to_owned(),
            manifest_hash: "0".repeat(64),
            generation_sequence: "0".to_owned(),
        },
        expected_local_revision: 0,
        staging_id: "staging-logical-bounded-reader".to_owned(),
        logical_generation_id: "local-bounded-reader".to_owned(),
        merged_generation_sequence: "2".to_owned(),
        pin_lease_id: "logical-delta-pin-bounded-reader".to_owned(),
        staging_directory,
        maintenance_guard: None,
        staged_objects: BTreeMap::new(),
        database_staged: false,
        initialized: true,
    };
    let mut bytes = record.bytes.clone();
    bytes.push(0xff);
    let mut reader = PrefixPlusOneReader { bytes, position: 0 };

    assert!(matches!(
        target.stage_payload(
            &mut stage,
            &LogicalDeltaObject {
                hash: record.hash.clone(),
                size: record.size,
            },
            &mut reader,
        ),
        Err(PeerSyncError::Validation(_))
    ));
    assert_eq!(reader.position as u64, record.size + 1);
}

#[test]
fn durable_stage_payload_publishes_only_complete_exact_objects() {
    let (directory, mut store, cas) = open_fixture();
    let remote = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "remote-1".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: Some("remote-0".to_owned()),
        source_revision: 1,
        records: vec![ProjectedLogicalRecord::live(
            LogicalRecordLocator::Plugin {
                storage_key: "remote-plugin".to_owned(),
            },
            LogicalRecordEnvelope::Plugin {
                ordinal: 0,
                value: json!({"remote":true}),
            },
            vec![],
        )],
    })
    .expect("build durable reader manifest");
    let record = &remote.record_objects[0].object;
    let job = RefCell::new(
        DurableCasJob::begin(
            directory.path(),
            "logical-durable-reader",
            CasJobKind::LogicalDeltaTarget,
            0,
        )
        .expect("begin durable reader job"),
    );
    let staging_root = directory.path().join("logical-delta-staging");
    let mut target = PersistentLogicalDeltaTarget::new_with_durable_job(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote.manifest_bytes,
        &staging_root,
        &job,
    )
    .expect("open durable reader target");
    let mut stage = PersistentLogicalDeltaStage::Changed {
        expected_base: PeerBase {
            generation_id: "remote-0".to_owned(),
            manifest_hash: "0".repeat(64),
            generation_sequence: "0".to_owned(),
        },
        expected_local_revision: 0,
        staging_id: "staging-logical-durable-reader".to_owned(),
        logical_generation_id: "local-durable-reader".to_owned(),
        merged_generation_sequence: "2".to_owned(),
        pin_lease_id: "logical-delta-pin-durable-reader".to_owned(),
        staging_directory: staging_root.join("staging-logical-durable-reader"),
        maintenance_guard: None,
        staged_objects: BTreeMap::new(),
        database_staged: false,
        initialized: true,
    };

    let mut partial = Cursor::new(record.bytes[..record.bytes.len() - 1].to_vec());
    assert!(target
        .stage_payload(
            &mut stage,
            &LogicalDeltaObject {
                hash: record.hash.clone(),
                size: record.size,
            },
            &mut partial,
        )
        .is_err());
    assert_eq!(cas.stat_object(&record.hash).unwrap(), None);
    assert_eq!(job.borrow().pin_count(), 1);

    let mut wrong = record.bytes.clone();
    wrong[0] ^= 0xff;
    let wrong_hash = hex::encode(sha2::Sha256::digest(&wrong));
    for _ in 0..3 {
        assert!(target
            .stage_payload(
                &mut stage,
                &LogicalDeltaObject {
                    hash: record.hash.clone(),
                    size: record.size,
                },
                &mut Cursor::new(wrong.clone()),
            )
            .is_err());
        assert_eq!(job.borrow().pin_count(), 1);
        assert_eq!(cas.stat_object(&wrong_hash).unwrap(), None);
        assert_eq!(
            std::fs::read_dir(directory.path().join("assets-v2").join("staging"))
                .unwrap()
                .count(),
            0
        );
    }
    assert_eq!(cas.stat_object(&record.hash).unwrap(), None);

    assert!(target
        .stage_payload(
            &mut stage,
            &LogicalDeltaObject {
                hash: record.hash.clone(),
                size: record.size + 1,
            },
            &mut Cursor::new(record.bytes.clone()),
        )
        .is_err());
    assert_eq!(job.borrow().pin_count(), 1);

    target
        .stage_payload(
            &mut stage,
            &LogicalDeltaObject {
                hash: record.hash.clone(),
                size: record.size,
            },
            &mut Cursor::new(record.bytes.clone()),
        )
        .expect("promote complete exact object");
    let PersistentLogicalDeltaStage::Changed { staged_objects, .. } = &stage else {
        panic!("expected changed durable stage");
    };
    assert_eq!(
        staged_objects.get(&record.hash),
        cas.object_path(&record.hash).unwrap().as_ref()
    );
    assert_eq!(job.borrow().pin_count(), 2);
    target
        .promote_payload_object(staged_objects, &record.hash)
        .expect("reuse same-run verified job preparation");
    assert_eq!(target.payload_reprepare_count.get(), 0);
    assert_eq!(job.borrow().pin_count(), 2);
}

#[test]
fn merged_generation_sequence_advances_the_greater_parent_sequence() {
    assert_eq!(merged_generation_sequence("12", "13").unwrap(), "14");
    assert_eq!(merged_generation_sequence("99", "8").unwrap(), "100");
    assert!(matches!(
        merged_generation_sequence(&"9".repeat(64), "1"),
        Err(PeerSyncError::Validation(_))
    ));
}

#[test]
fn exact_plan_rejects_omission_of_a_base_tombstone() {
    let key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
        storage_key: "deleted-plugin".to_owned(),
    })
    .unwrap();
    let base = LogicalManifest {
        schema: LOGICAL_MANIFEST_SCHEMA.to_owned(),
        library_id: "library".to_owned(),
        generation: "base".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: None,
        source_revision: 1,
        records: vec![LogicalManifestRecord::Tombstone(
            LogicalManifestTombstoneRecord {
                key: key.clone(),
                state: "tombstone".to_owned(),
                deleted_generation_sequence: "1".to_owned(),
            },
        )],
        objects: vec![],
    };
    let local = base.clone();
    let mut remote = base.clone();
    remote.generation = "remote".to_owned();
    remote.generation_sequence = "2".to_owned();
    remote.records.clear();

    assert!(matches!(
        derive_exact_three_way_plan(&base, &local, &remote),
        Err(PeerSyncError::Validation(_))
    ));

    let resurrected = LogicalManifestRecord::Live(LogicalManifestLiveRecord {
        key,
        state: "live".to_owned(),
        object_hash: "a".repeat(64),
        dependencies: vec![],
    });
    let mut local = base.clone();
    local.generation = "local-live".to_owned();
    local.generation_sequence = "2".to_owned();
    local.records = vec![resurrected.clone()];
    assert!(matches!(
        derive_exact_three_way_plan(&base, &local, &base),
        Err(PeerSyncError::Validation(_))
    ));
    let mut remote = base.clone();
    remote.generation = "remote-live".to_owned();
    remote.generation_sequence = "2".to_owned();
    remote.records = vec![resurrected];
    assert!(matches!(
        derive_exact_three_way_plan(&base, &base, &remote),
        Err(PeerSyncError::Validation(_))
    ));

    let mut local = base.clone();
    local.generation = "local".to_owned();
    local.generation_sequence = "2".to_owned();
    local.records.clear();
    let remote = base.clone();
    assert!(matches!(
        derive_exact_three_way_plan(&base, &local, &remote),
        Err(PeerSyncError::Validation(_))
    ));
}

#[test]
fn exact_plan_rejects_omission_of_a_base_live_record_from_either_descendant() {
    let key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
        storage_key: "live-plugin".to_owned(),
    })
    .unwrap();
    let base = LogicalManifest {
        schema: LOGICAL_MANIFEST_SCHEMA.to_owned(),
        library_id: "library".to_owned(),
        generation: "base".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: None,
        source_revision: 1,
        records: vec![LogicalManifestRecord::Live(LogicalManifestLiveRecord {
            key,
            state: "live".to_owned(),
            object_hash: "a".repeat(64),
            dependencies: vec![],
        })],
        objects: vec![LogicalManifestObject {
            hash: "a".repeat(64),
            size: 1,
        }],
    };
    let mut local = base.clone();
    local.generation = "local".to_owned();
    local.generation_sequence = "2".to_owned();
    let mut remote = base.clone();
    remote.generation = "remote".to_owned();
    remote.generation_sequence = "2".to_owned();

    local.records.clear();
    local.objects.clear();
    assert!(matches!(
        derive_exact_three_way_plan(&base, &local, &remote),
        Err(PeerSyncError::Validation(_))
    ));

    local = base.clone();
    remote.records.clear();
    remote.objects.clear();
    assert!(matches!(
        derive_exact_three_way_plan(&base, &local, &remote),
        Err(PeerSyncError::Validation(_))
    ));
}

#[test]
fn exact_plan_treats_an_absent_zero_byte_object_as_a_put() {
    let key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
        storage_key: "empty-plugin".to_owned(),
    })
    .unwrap();
    let empty_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let empty_manifest = |generation: &str, sequence: &str| LogicalManifest {
        schema: LOGICAL_MANIFEST_SCHEMA.to_owned(),
        library_id: "library".to_owned(),
        generation: generation.to_owned(),
        generation_sequence: sequence.to_owned(),
        parent_generation: None,
        source_revision: 1,
        records: vec![],
        objects: vec![],
    };
    let base = empty_manifest("base", "1");
    let local = empty_manifest("local", "2");
    let mut remote = empty_manifest("remote", "2");
    remote
        .records
        .push(LogicalManifestRecord::Live(LogicalManifestLiveRecord {
            key: key.clone(),
            state: "live".to_owned(),
            object_hash: empty_hash.to_owned(),
            dependencies: vec![],
        }));
    remote.objects.push(LogicalManifestObject {
        hash: empty_hash.to_owned(),
        size: 0,
    });

    let (apply, preserve, candidates) =
        derive_exact_three_way_plan(&base, &local, &remote).unwrap();
    assert_eq!(
        apply,
        vec![LogicalDeltaApplyOperation::Put {
            key,
            object_hash: empty_hash.to_owned(),
            dependencies: vec![],
        }]
    );
    assert!(preserve.is_empty());
    assert_eq!(candidates, vec![empty_hash]);
}

#[test]
fn exact_plan_reports_delete_vs_edit_as_a_typed_merge_conflict() {
    let key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
        storage_key: "shared-plugin".to_owned(),
    })
    .unwrap();
    let manifest = |generation: &str,
                    sequence: &str,
                    record: LogicalManifestRecord,
                    objects: Vec<LogicalManifestObject>| LogicalManifest {
        schema: LOGICAL_MANIFEST_SCHEMA.to_owned(),
        library_id: "library".to_owned(),
        generation: generation.to_owned(),
        generation_sequence: sequence.to_owned(),
        parent_generation: None,
        source_revision: 1,
        records: vec![record],
        objects,
    };
    let live = |hash: &str| {
        LogicalManifestRecord::Live(LogicalManifestLiveRecord {
            key: key.clone(),
            state: "live".to_owned(),
            object_hash: hash.repeat(64),
            dependencies: vec![],
        })
    };
    let base = manifest(
        "base",
        "0",
        live("a"),
        vec![LogicalManifestObject {
            hash: "a".repeat(64),
            size: 1,
        }],
    );
    let local = manifest(
        "local",
        "1",
        LogicalManifestRecord::Tombstone(LogicalManifestTombstoneRecord {
            key: key.clone(),
            state: "tombstone".to_owned(),
            deleted_generation_sequence: "1".to_owned(),
        }),
        vec![],
    );
    let remote = manifest(
        "remote",
        "1",
        live("b"),
        vec![LogicalManifestObject {
            hash: "b".repeat(64),
            size: 1,
        }],
    );

    assert!(matches!(
        derive_exact_three_way_plan(&base, &local, &remote),
        Err(PeerSyncError::LogicalMergeConflict { record }) if record == key
    ));
}

#[test]
fn policy_reject_returns_sorted_typed_conflicts_without_a_plan() {
    let plugin = |key: &str, hash: &str| {
        LogicalManifestRecord::Live(LogicalManifestLiveRecord {
            key: encode_logical_record_key(&LogicalRecordLocator::Plugin {
                storage_key: key.to_owned(),
            })
            .unwrap(),
            state: "live".to_owned(),
            object_hash: hash.repeat(64),
            dependencies: vec![],
        })
    };
    let tombstone = |key: &str| {
        LogicalManifestRecord::Tombstone(LogicalManifestTombstoneRecord {
            key: encode_logical_record_key(&LogicalRecordLocator::Plugin {
                storage_key: key.to_owned(),
            })
            .unwrap(),
            state: "tombstone".to_owned(),
            deleted_generation_sequence: "2".to_owned(),
        })
    };
    let manifest = |generation: &str, records| LogicalManifest {
        schema: LOGICAL_MANIFEST_SCHEMA.to_owned(),
        library_id: "library".to_owned(),
        generation: generation.to_owned(),
        generation_sequence: "2".to_owned(),
        parent_generation: None,
        source_revision: 1,
        records,
        objects: vec![],
    };
    let base = manifest("base", vec![plugin("alpha", "a"), plugin("beta", "a")]);
    let local = manifest("local", vec![plugin("alpha", "b"), tombstone("beta")]);
    let remote = manifest("remote", vec![plugin("alpha", "c"), plugin("beta", "c")]);

    let resolution =
        derive_policy_three_way_plan(&base, &local, &remote, LogicalDeltaConflictPolicy::Reject)
            .unwrap();

    assert_eq!(resolution.apply, vec![]);
    assert_eq!(resolution.preserve_local_keys, Vec::<String>::new());
    assert_eq!(
        resolution.conflicts,
        vec![
            LogicalDeltaConflict {
                record: encode_logical_record_key(&LogicalRecordLocator::Plugin {
                    storage_key: "alpha".to_owned(),
                })
                .unwrap(),
                kind: LogicalDeltaConflictKind::SameRecord,
            },
            LogicalDeltaConflict {
                record: encode_logical_record_key(&LogicalRecordLocator::Plugin {
                    storage_key: "beta".to_owned(),
                })
                .unwrap(),
                kind: LogicalDeltaConflictKind::DeleteVsEdit,
            },
        ]
    );
}

#[test]
fn policy_prefer_local_preserves_conflicts_and_applies_remote_only_changes() {
    let key = |storage_key: &str| {
        encode_logical_record_key(&LogicalRecordLocator::Plugin {
            storage_key: storage_key.to_owned(),
        })
        .unwrap()
    };
    let live = |storage_key: &str, hash: &str| {
        LogicalManifestRecord::Live(LogicalManifestLiveRecord {
            key: key(storage_key),
            state: "live".to_owned(),
            object_hash: hash.repeat(64),
            dependencies: vec![],
        })
    };
    let manifest = |generation: &str, records| LogicalManifest {
        schema: LOGICAL_MANIFEST_SCHEMA.to_owned(),
        library_id: "library".to_owned(),
        generation: generation.to_owned(),
        generation_sequence: "2".to_owned(),
        parent_generation: None,
        source_revision: 1,
        records,
        objects: vec![],
    };
    let base = manifest("base", vec![live("shared", "a")]);
    let local = manifest("local", vec![live("shared", "b")]);
    let remote = manifest(
        "remote",
        vec![live("remote-only", "d"), live("shared", "c")],
    );

    let resolution = derive_policy_three_way_plan(
        &base,
        &local,
        &remote,
        LogicalDeltaConflictPolicy::PreferLocal,
    )
    .unwrap();

    assert_eq!(
        resolution.apply,
        vec![LogicalDeltaApplyOperation::Put {
            key: key("remote-only"),
            object_hash: "d".repeat(64),
            dependencies: vec![],
        }]
    );
    assert_eq!(resolution.preserve_local_keys, vec![key("shared")]);
    assert_eq!(
        resolution.conflicts,
        vec![LogicalDeltaConflict {
            record: key("shared"),
            kind: LogicalDeltaConflictKind::SameRecord,
        }]
    );
}

#[test]
fn policy_prefer_remote_applies_remote_winner_for_every_conflict() {
    let key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
        storage_key: "shared".to_owned(),
    })
    .unwrap();
    let live = |hash: &str| {
        LogicalManifestRecord::Live(LogicalManifestLiveRecord {
            key: key.clone(),
            state: "live".to_owned(),
            object_hash: hash.repeat(64),
            dependencies: vec![],
        })
    };
    let manifest = |generation: &str, record| LogicalManifest {
        schema: LOGICAL_MANIFEST_SCHEMA.to_owned(),
        library_id: "library".to_owned(),
        generation: generation.to_owned(),
        generation_sequence: "2".to_owned(),
        parent_generation: None,
        source_revision: 1,
        records: vec![record],
        objects: vec![],
    };
    let base = manifest("base", live("a"));
    let local = manifest("local", live("b"));
    let remote = manifest("remote", live("c"));

    let resolution = derive_policy_three_way_plan(
        &base,
        &local,
        &remote,
        LogicalDeltaConflictPolicy::PreferRemote,
    )
    .unwrap();

    assert_eq!(
        resolution.apply,
        vec![LogicalDeltaApplyOperation::Put {
            key: key.clone(),
            object_hash: "c".repeat(64),
            dependencies: vec![],
        }]
    );
    assert!(resolution.preserve_local_keys.is_empty());
    assert_eq!(
        resolution.conflicts,
        vec![LogicalDeltaConflict {
            record: key,
            kind: LogicalDeltaConflictKind::SameRecord,
        }]
    );
}

#[test]
fn generation_identity_binds_the_same_id_to_hash_and_sequence() {
    assert!(matches!(
        validate_same_generation_identity(
            "shared",
            &"a".repeat(64),
            "1",
            "shared",
            &"b".repeat(64),
            "1",
        ),
        Err(PeerSyncError::Validation(_))
    ));
    assert!(matches!(
        validate_same_generation_identity(
            "shared",
            &"a".repeat(64),
            "1",
            "shared",
            &"a".repeat(64),
            "2",
        ),
        Err(PeerSyncError::Validation(_))
    ));
    assert!(validate_same_generation_identity(
        "left",
        &"a".repeat(64),
        "1",
        "right",
        &"b".repeat(64),
        "2",
    )
    .is_ok());
}

#[test]
fn first_common_base_is_established_only_from_an_exact_active_logical_state() {
    let (directory, mut store, cas) = open_fixture();
    let local = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "local-0".to_owned(),
                generation_sequence: "7".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    let mut remote = local.manifest.clone();
    remote.generation = "remote-0".to_owned();
    remote.generation_sequence = "11".to_owned();
    remote.parent_generation = None;
    remote.source_revision = 99;
    let remote_bytes = encode_logical_manifest(&remote).unwrap();
    let remote_hash = hash(&remote_bytes);
    assert_eq!(cas.stat_object(&remote_hash).unwrap(), None);

    establish_logical_common_base(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        0,
        &remote_bytes,
    )
    .unwrap();
    assert_eq!(
        store
            .connection
            .query_row::<(String, String, String), _, _>(
                "SELECT generation_id, manifest_hash, generation_sequence
                 FROM logical_peer_common_bases
                 WHERE peer_id = 'peer' AND library_id = 'library'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap(),
        ("remote-0".to_owned(), remote_hash.clone(), "11".to_owned())
    );
    assert_eq!(
        cas.read_object(&remote_hash).unwrap().unwrap(),
        remote_bytes
    );
    assert!(snapshot::collect_asset_roots(&store.connection, &cas)
        .unwrap()
        .object_hashes
        .contains(&remote_hash));

    drop(store);
    drop(cas);
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    establish_logical_common_base(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        0,
        &remote_bytes,
    )
    .expect("exact replay is idempotent after reopen");

    let mut different = remote;
    different.generation = "remote-other".to_owned();
    different.generation_sequence = "12".to_owned();
    let different_bytes = encode_logical_manifest(&different).unwrap();
    assert!(matches!(
        establish_logical_common_base(
            &mut store,
            &cas,
            "peer",
            "library",
            "local-0",
            0,
            &different_bytes,
        ),
        Err(PeerSyncError::ActivationConflict { .. })
    ));
}

#[test]
fn common_base_commit_intent_failure_rolls_back_the_new_common_base() {
    let (_directory, mut store, cas) = open_fixture();
    let local = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "local-0".to_owned(),
                generation_sequence: "7".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    let mut remote = local.manifest.clone();
    remote.generation = "remote-0".to_owned();
    remote.generation_sequence = "11".to_owned();
    remote.parent_generation = None;
    remote.source_revision = 99;
    let remote_bytes = encode_logical_manifest(&remote).unwrap();
    let callback_called = std::cell::Cell::new(false);

    let result = establish_logical_common_base_with_commit_intent(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        0,
        &remote_bytes,
        || {
            callback_called.set(true);
            Err(PeerSyncError::Storage(
                "commit intent write failed".to_owned(),
            ))
        },
    );

    assert!(
        matches!(result, Err(PeerSyncError::Storage(message)) if message == "commit intent write failed")
    );
    assert!(callback_called.get());
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM logical_peer_common_bases
                 WHERE peer_id = 'peer' AND library_id = 'library'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        0
    );
}

#[test]
fn first_common_base_rejects_local_content_or_revision_mismatch_without_a_row() {
    let (_directory, mut store, cas) = open_fixture();
    let local = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "local-0".to_owned(),
                generation_sequence: "0".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    let remote = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "remote-0".to_owned(),
        generation_sequence: "0".to_owned(),
        parent_generation: None,
        source_revision: 0,
        records: vec![ProjectedLogicalRecord::live(
            LogicalRecordLocator::Root,
            LogicalRecordEnvelope::Root {
                value: json!({"different":true}),
                owner_heads: vec![],
            },
            vec![],
        )],
    })
    .unwrap();

    assert!(matches!(
        establish_logical_common_base(
            &mut store,
            &cas,
            "peer",
            "library",
            "local-0",
            0,
            &remote.manifest_bytes,
        ),
        Err(PeerSyncError::Validation(_))
    ));
    let mut equivalent = local.manifest;
    equivalent.generation = "remote-0".to_owned();
    let equivalent_bytes = encode_logical_manifest(&equivalent).unwrap();
    assert!(matches!(
        establish_logical_common_base(
            &mut store,
            &cas,
            "peer",
            "library",
            "local-0",
            1,
            &equivalent_bytes,
        ),
        Err(PeerSyncError::ActivationConflict { .. })
    ));
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM logical_peer_common_bases",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        0
    );
}

#[test]
fn first_common_base_rejects_a_local_generation_id_with_a_different_manifest_hash() {
    let (_directory, mut store, cas) = open_fixture();
    let local = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "shared-generation".to_owned(),
                generation_sequence: "0".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    let mut remote = local.manifest;
    remote.source_revision = 1;
    let remote_bytes = encode_logical_manifest(&remote).unwrap();

    assert!(matches!(
        establish_logical_common_base(
            &mut store,
            &cas,
            "peer",
            "library",
            "shared-generation",
            0,
            &remote_bytes,
        ),
        Err(PeerSyncError::Validation(_))
    ));
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM logical_peer_common_bases",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        0
    );
}

#[test]
fn existing_payload_cas_reuse_rehashes_same_sized_content() {
    let (directory, mut store, cas) = open_fixture();
    let expected = b"expected payload".to_vec();
    let expected_hash = hash(&expected);
    let remote = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "remote-1".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: Some("remote-0".to_owned()),
        source_revision: 1,
        records: vec![ProjectedLogicalRecord::live(
            LogicalRecordLocator::Asset {
                logical_key: "remote-asset".to_owned(),
            },
            LogicalRecordEnvelope::Asset {
                object_hash: Some(expected_hash.clone()),
                size: expected.len() as u64,
                metadata: encode_asset_alias_metadata(&LogicalAssetAliasMetadata {
                    mime: "application/octet-stream".to_owned(),
                    name: "asset".to_owned(),
                    ext: "bin".to_owned(),
                    inlay_type: None,
                    width: None,
                    height: None,
                    metadata: json!({}),
                })
                .unwrap(),
            },
            vec![descriptor(&expected)],
        )],
    })
    .expect("build asset remote manifest");
    let prepared = cas
        .prepare_bytes(&expected)
        .expect("prepare expected payload");
    let mut corrupt = expected.clone();
    corrupt[0] ^= 0xff;
    fs::write(directory.path().join(prepared.physical_key), corrupt)
        .expect("corrupt existing CAS object with the same size");
    let target = PersistentLogicalDeltaTarget::new(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote.manifest_bytes,
        &directory.path().join("logical-delta-staging"),
    )
    .expect("open CAS verification target");

    assert!(matches!(
        target.promote_payload_object(&BTreeMap::new(), &expected_hash),
        Err(PeerSyncError::WholeObjectHashMismatch { object }) if object == expected_hash
    ));
}

#[test]
fn existing_payload_cas_reuse_is_pinned_before_durable_job_seal() {
    let (directory, mut store, cas) = open_fixture();
    let payload = b"existing remote payload".to_vec();
    let payload_hash = hash(&payload);
    let remote = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "remote-1".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: Some("remote-0".to_owned()),
        source_revision: 1,
        records: vec![ProjectedLogicalRecord::live(
            LogicalRecordLocator::Asset {
                logical_key: "remote-asset".to_owned(),
            },
            LogicalRecordEnvelope::Asset {
                object_hash: Some(payload_hash.clone()),
                size: payload.len() as u64,
                metadata: encode_asset_alias_metadata(&LogicalAssetAliasMetadata {
                    mime: "application/octet-stream".to_owned(),
                    name: "asset".to_owned(),
                    ext: "bin".to_owned(),
                    inlay_type: None,
                    width: None,
                    height: None,
                    metadata: json!({}),
                })
                .unwrap(),
            },
            vec![descriptor(&payload)],
        )],
    })
    .expect("build asset remote manifest");
    cas.prepare_bytes(&payload)
        .expect("prepare existing remote payload");
    let job = RefCell::new(
        DurableCasJob::begin(
            directory.path(),
            "logical-existing-payload",
            CasJobKind::LogicalDeltaTarget,
            0,
        )
        .expect("begin durable logical target job"),
    );
    let mut target = PersistentLogicalDeltaTarget::new_with_durable_job(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote.manifest_bytes,
        &directory.path().join("logical-delta-staging"),
        &job,
    )
    .expect("open durable CAS verification target");

    target
        .promote_payload_object(&BTreeMap::new(), &payload_hash)
        .expect("reuse verified existing payload");
    target
        .seal_durable_job()
        .expect("seal durable logical target job");

    let roots = job.borrow().root_set().expect("read sealed job roots");
    assert!(roots.object_hashes.contains(&payload_hash));
}

#[test]
fn structured_records_are_applied_to_invisible_staging_one_at_a_time() {
    let (directory, mut store, cas) = open_fixture();
    let local = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "local-0".to_owned(),
                generation_sequence: "0".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .expect("build local logical index");
    let (base, _bytes, base_hash) = store_remote_base_manifest(&cas, &local.manifest, "remote-0");
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('peer', 'library', 'remote-0', ?1, '0', 0)",
            [&base_hash],
        )
        .unwrap();
    let additions = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "remote-1".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: Some("remote-0".to_owned()),
        source_revision: 1,
        records: vec![
            ProjectedLogicalRecord::live(
                LogicalRecordLocator::Character {
                    character_id: "char".to_owned(),
                },
                LogicalRecordEnvelope::Character {
                    configured_index: 0,
                    detail: json!({"chaId":"char","name":"Character"}),
                    owner_heads: vec![LogicalOwnerHead::absent(
                        LogicalOwnerLocator::CharacterAdditional {
                            character_id: "char".to_owned(),
                        },
                    )],
                },
                vec![],
            ),
            ProjectedLogicalRecord::live(
                LogicalRecordLocator::Conversation {
                    character_id: "char".to_owned(),
                    conversation_id: "chat".to_owned(),
                },
                LogicalRecordEnvelope::Conversation {
                    configured_index: 0,
                    recent_at: 0,
                    detail: json!({"id":"wrong","name":"Malformed"}),
                    message_page_hashes: vec![],
                },
                vec![],
            ),
        ],
    })
    .expect("build target-invalid canonical records");
    let mut remote_manifest = base;
    remote_manifest.generation = "remote-1".to_owned();
    remote_manifest.generation_sequence = "1".to_owned();
    remote_manifest.parent_generation = Some("remote-0".to_owned());
    remote_manifest.source_revision = 1;
    remote_manifest
        .records
        .extend(additions.manifest.records.clone());
    remote_manifest
        .records
        .sort_by(|left, right| left.key().cmp(right.key()));
    remote_manifest
        .objects
        .extend(additions.manifest.objects.clone());
    remote_manifest
        .objects
        .sort_by(|left, right| left.hash.cmp(&right.hash));
    remote_manifest
        .objects
        .dedup_by(|left, right| left.hash == right.hash);
    let remote_bytes = encode_logical_manifest(&remote_manifest).unwrap();
    let remote_hash = hash(&remote_bytes);
    let apply = additions
        .manifest
        .records
        .iter()
        .map(|record| match record {
            LogicalManifestRecord::Live(record) => LogicalDeltaApplyOperation::Put {
                key: record.key.clone(),
                object_hash: record.object_hash.clone(),
                dependencies: record.dependencies.clone(),
            },
            LogicalManifestRecord::Tombstone(_) => unreachable!(),
        })
        .collect();
    let plan = ReadyLogicalDeltaPlan {
        expected_local_revision: 0,
        expected_base_manifest_hash: base_hash,
        expected_remote_generation: "remote-1".to_owned(),
        apply,
        preserve_local_keys: vec![],
        candidate_object_hashes: additions
            .manifest
            .objects
            .iter()
            .map(|object| object.hash.clone())
            .collect(),
        next_base_manifest_hash: remote_hash,
        next_base_generation_sequence: "1".to_owned(),
    };
    let staging_root = directory.path().join("peer-delta").join("staging");
    let mut target = PersistentLogicalDeltaTarget::new(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote_bytes,
        &staging_root,
    )
    .unwrap();
    let mut stage = target.begin(&plan).unwrap();
    for record in &additions.record_objects {
        target
            .stage_payload(
                &mut stage,
                &LogicalDeltaObject {
                    hash: record.object.hash.clone(),
                    size: record.object.size,
                },
                &mut Cursor::new(record.object.bytes.clone()),
            )
            .unwrap();
    }
    let active_stage = match &stage {
        PersistentLogicalDeltaStage::Changed {
            staging_directory, ..
        } => staging_directory.clone(),
        _ => unreachable!(),
    };
    assert_eq!(
        crate::peer_sync::maintenance::cleanup_temp(directory.path())
            .unwrap()
            .count,
        0
    );
    assert!(active_stage.exists());

    assert!(matches!(
        target.stage_database_changes(&mut stage, &plan),
        Err(PeerSyncError::Validation(_))
    ));
    let staging_id = match &stage {
        PersistentLogicalDeltaStage::Changed { staging_id, .. } => staging_id.clone(),
        _ => unreachable!(),
    };
    assert!(target
        .store
        .connection
        .query_row::<bool, _, _>(
            "SELECT EXISTS(
                SELECT 1 FROM characters
                WHERE generation = ?1 AND character_id = 'char'
             )",
            [staging_id],
            |row| row.get(0),
        )
        .unwrap());
    assert!(target.store.read_character("char", None).unwrap().is_none());
    target.abort(stage).unwrap();
}

#[test]
fn cloned_logical_target_preserves_dual_repository_authority() {
    let (_directory, store, _cas) = open_fixture();
    store
        .connection
        .execute(
            "UPDATE asset_repository_authority SET value = ?1 WHERE generation = 'revision-0'",
            [r#"{"format":"v2","migrationId":"migration-1","compatibilityHash":"abababababababababababababababababababababababababababababababab"}"#],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE cold_payload_authority SET value = ?1 WHERE generation = 'revision-0'",
            [r#"{"format":"v2","migrationId":"migration-2","compatibilityHash":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"}"#],
        )
        .unwrap();
    let transaction = store.connection.unchecked_transaction().unwrap();

    clone_generation(&transaction, "revision-0", "staging-logical-authority").unwrap();

    let authority: String = transaction
        .query_row(
            "SELECT value FROM asset_repository_authority
             WHERE generation = 'staging-logical-authority'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        authority,
        r#"{"format":"v2","migrationId":"migration-1","compatibilityHash":"abababababababababababababababababababababababababababababababab"}"#
    );
    let cold_authority: String = transaction
        .query_row(
            "SELECT value FROM cold_payload_authority
             WHERE generation = 'staging-logical-authority'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        cold_authority,
        r#"{"format":"v2","migrationId":"migration-2","compatibilityHash":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"}"#
    );
}

#[test]
fn configured_indices_are_unique_within_their_shared_ordering_scope() {
    let (_directory, store, _cas) = open_fixture();
    let transaction = store.connection.unchecked_transaction().unwrap();
    clone_generation(&transaction, "revision-0", "staging-logical-order").unwrap();

    transaction
        .execute_batch(
            "INSERT INTO bot_presets (
                generation, preset_id, configured_index, name, image, value
             ) VALUES
                ('staging-logical-order', 'a', 7, 'A', NULL, '{\"name\":\"A\"}'),
                ('staging-logical-order', 'b', 7, 'B', NULL, '{\"name\":\"B\"}');",
        )
        .unwrap();
    assert!(matches!(
        validate_configured_index_uniqueness(&transaction, "staging-logical-order"),
        Err(PeerSyncError::Validation(_))
    ));
    transaction
        .execute(
            "DELETE FROM bot_presets WHERE generation = 'staging-logical-order'",
            [],
        )
        .unwrap();

    transaction
        .execute_batch(
            "INSERT INTO characters (
                generation, character_id, configured_index, recent_at, trashed,
                name, image, conversation_count, type, creator_notes, trash_time, detail
             ) VALUES
                ('staging-logical-order', 'a', 8, 0, 0, 'A', NULL, 0, 'character', NULL, NULL,
                 '{\"chaId\":\"a\",\"name\":\"A\"}'),
                ('staging-logical-order', 'b', 8, 0, 0, 'B', NULL, 0, 'character', NULL, NULL,
                 '{\"chaId\":\"b\",\"name\":\"B\"}');",
        )
        .unwrap();
    assert!(matches!(
        validate_configured_index_uniqueness(&transaction, "staging-logical-order"),
        Err(PeerSyncError::Validation(_))
    ));
    transaction
        .execute(
            "DELETE FROM characters WHERE generation = 'staging-logical-order'",
            [],
        )
        .unwrap();

    transaction
        .execute_batch(
            "INSERT INTO conversations (
                generation, character_id, conversation_id, configured_index,
                recent_at, name, message_count, detail
             ) VALUES
                ('staging-logical-order', 'a', 'one', 9, 0, 'One', 0,
                 '{\"id\":\"one\",\"name\":\"One\"}'),
                ('staging-logical-order', 'a', 'two', 9, 0, 'Two', 0,
                 '{\"id\":\"two\",\"name\":\"Two\"}');",
        )
        .unwrap();
    assert!(matches!(
        validate_configured_index_uniqueness(&transaction, "staging-logical-order"),
        Err(PeerSyncError::Validation(_))
    ));
    transaction
        .execute(
            "DELETE FROM conversations WHERE generation = 'staging-logical-order'",
            [],
        )
        .unwrap();

    transaction
        .execute_batch(
            "INSERT INTO conversations (
                generation, character_id, conversation_id, configured_index,
                recent_at, name, message_count, detail
             ) VALUES
                ('staging-logical-order', 'a', 'one', 9, 0, 'One', 0,
                 '{\"id\":\"one\",\"name\":\"One\"}'),
                ('staging-logical-order', 'b', 'two', 9, 0, 'Two', 0,
                 '{\"id\":\"two\",\"name\":\"Two\"}');",
        )
        .unwrap();
    validate_configured_index_uniqueness(&transaction, "staging-logical-order").unwrap();
    transaction.rollback().unwrap();
}

#[test]
fn character_delete_rejects_a_remote_live_child_conversation() {
    let (_directory, store, _cas) = open_fixture();
    let character_key = encode_logical_record_key(&LogicalRecordLocator::Character {
        character_id: "char".to_owned(),
    })
    .unwrap();
    let conversation_key = encode_logical_record_key(&LogicalRecordLocator::Conversation {
        character_id: "char".to_owned(),
        conversation_id: "chat".to_owned(),
    })
    .unwrap();
    let plan = ReadyLogicalDeltaPlan {
        expected_local_revision: 0,
        expected_base_manifest_hash: "0".repeat(64),
        expected_remote_generation: "remote".to_owned(),
        apply: vec![
            LogicalDeltaApplyOperation::Delete {
                key: character_key,
                deleted_generation_sequence: "1".to_owned(),
            },
            LogicalDeltaApplyOperation::Put {
                key: conversation_key,
                object_hash: "1".repeat(64),
                dependencies: vec![],
            },
        ],
        preserve_local_keys: vec![],
        candidate_object_hashes: vec!["1".repeat(64)],
        next_base_manifest_hash: "2".repeat(64),
        next_base_generation_sequence: "1".to_owned(),
    };

    assert!(matches!(
        validate_character_deletes(&store.connection, "revision-0", &plan),
        Err(PeerSyncError::Validation(_))
    ));
}

#[test]
fn equal_plugin_ordinals_use_storage_key_as_the_deterministic_tie_breaker() {
    let (_directory, store, _cas) = open_fixture();
    store
        .connection
        .execute_batch(
            "INSERT INTO plugin_storage (
                generation, storage_key, byte_size, ordinal, value
             ) VALUES
                ('revision-0', 'zeta', 2, 4, '{}'),
                ('revision-0', 'alpha', 2, 4, '{}');",
        )
        .unwrap();

    assert_eq!(
        store
            .query_plugin_storage(None)
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.key)
            .collect::<Vec<_>>(),
        ["alpha".to_owned(), "zeta".to_owned()]
    );
}

#[test]
fn duplicate_configured_index_aborts_without_exposing_staging() {
    let (directory, mut store, cas) = open_fixture();
    let local = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "local-0".to_owned(),
                generation_sequence: "0".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    let (base, _bytes, base_hash) = store_remote_base_manifest(&cas, &local.manifest, "remote-0");
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('peer', 'library', 'remote-0', ?1, '0', 0)",
            [&base_hash],
        )
        .unwrap();
    let additions = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "additions".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: None,
        source_revision: 1,
        records: ["a", "b"]
            .into_iter()
            .map(|preset_id| {
                ProjectedLogicalRecord::live(
                    LogicalRecordLocator::Preset {
                        preset_id: preset_id.to_owned(),
                    },
                    LogicalRecordEnvelope::Preset {
                        configured_index: 7,
                        value: json!({"name":preset_id}),
                    },
                    vec![],
                )
            })
            .collect(),
    })
    .unwrap();
    let mut remote = base;
    remote.generation = "remote-1".to_owned();
    remote.generation_sequence = "1".to_owned();
    remote.parent_generation = Some("remote-0".to_owned());
    remote.source_revision = 1;
    remote.records.extend(additions.manifest.records.clone());
    remote
        .records
        .sort_by(|left, right| left.key().cmp(right.key()));
    remote.objects.extend(additions.manifest.objects.clone());
    remote
        .objects
        .sort_by(|left, right| left.hash.cmp(&right.hash));
    remote
        .objects
        .dedup_by(|left, right| left.hash == right.hash);
    let remote_bytes = encode_logical_manifest(&remote).unwrap();
    let remote_hash = hash(&remote_bytes);
    let plan = ReadyLogicalDeltaPlan {
        expected_local_revision: 0,
        expected_base_manifest_hash: base_hash,
        expected_remote_generation: "remote-1".to_owned(),
        apply: additions
            .manifest
            .records
            .iter()
            .map(|record| match record {
                LogicalManifestRecord::Live(record) => LogicalDeltaApplyOperation::Put {
                    key: record.key.clone(),
                    object_hash: record.object_hash.clone(),
                    dependencies: record.dependencies.clone(),
                },
                LogicalManifestRecord::Tombstone(_) => unreachable!(),
            })
            .collect(),
        preserve_local_keys: vec![],
        candidate_object_hashes: additions
            .manifest
            .objects
            .iter()
            .map(|object| object.hash.clone())
            .collect(),
        next_base_manifest_hash: remote_hash,
        next_base_generation_sequence: "1".to_owned(),
    };
    let remote_sizes = remote
        .objects
        .iter()
        .map(|object| (object.hash.clone(), object.size))
        .collect::<BTreeMap<_, _>>();
    let local_hashes = local
        .manifest
        .objects
        .iter()
        .map(|object| object.hash.clone())
        .collect::<BTreeSet<_>>();
    let mut source = MapSource {
        objects: additions
            .record_objects
            .iter()
            .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
            .collect(),
        content_gets: 0,
    };
    let staging_root = directory.path().join("logical-delta-staging");
    let mut target = PersistentLogicalDeltaTarget::new(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote_bytes,
        &staging_root,
    )
    .unwrap();

    assert!(matches!(
        execute_logical_delta_pull(
            &plan,
            &local_hashes,
            &cas,
            &remote_sizes,
            &mut source,
            &mut target,
        ),
        Err(PeerSyncError::Validation(_))
    ));
    drop(target);
    assert!(store.read_preset("a", None).unwrap().is_none());
    assert!(store.read_preset("b", None).unwrap().is_none());
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM root WHERE generation LIKE 'staging-logical-%'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        0
    );
    assert!(!staging_root.exists() || staging_root.read_dir().unwrap().next().is_none());
}

#[test]
fn p5_deferred_local_commit_leaves_shared_base_ack_and_proof_unchanged() {
    let (directory, mut store, cas) = open_fixture();
    store
        .connection
        .execute(
            "UPDATE root SET value = ?1 WHERE generation = 'revision-0'",
            [json!({"theme":"base"}).to_string()],
        )
        .unwrap();
    let base = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "shared-c".to_owned(),
                generation_sequence: "0".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('peer', 'library', 'shared-c', ?1, '0', 0)",
            [&base.manifest_hash],
        )
        .unwrap();
    let common = SyncGenerationIdentity {
        generation_id: "shared-c".to_owned(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: "0".to_owned(),
    };
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::for_test("library", "peer", common.clone(), 1),
            0,
        )
        .unwrap();
    let remote = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "remote-1".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: Some("shared-c".to_owned()),
        source_revision: 1,
        records: vec![ProjectedLogicalRecord::live(
            LogicalRecordLocator::Root,
            LogicalRecordEnvelope::Root {
                value: json!({"theme":"remote"}),
                owner_heads: vec![],
            },
            vec![],
        )],
    })
    .unwrap();
    let record = remote.record_objects.first().unwrap();
    let staging_root = directory.path().join("p5-deferred-staging");
    let mut target = PersistentLogicalDeltaTarget::new_p5_deferred(
        &mut store,
        &cas,
        "peer",
        "library",
        "shared-c",
        &remote.manifest_bytes,
        &staging_root,
        LogicalDeltaConflictPolicy::Reject,
    )
    .unwrap();
    let plan = target.build_ready_plan(0).unwrap();
    let mut source = MapSource {
        objects: [(record.object.hash.clone(), record.object.bytes.clone())]
            .into_iter()
            .collect(),
        content_gets: 0,
    };
    let remote_sizes = [(record.object.hash.clone(), record.object.size)]
        .into_iter()
        .collect();

    assert_eq!(
        execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &remote_sizes,
            &mut source,
            &mut target,
        )
        .unwrap(),
        LogicalDeltaActivation::Activated { revision: 1 }
    );
    drop(target);

    assert_eq!(store.read_root(None).unwrap().value["theme"], "remote");
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(
        store
            .connection
            .query_row::<(String, String, String), _, _>(
                "SELECT common_base.generation_id,
                        device.acknowledged_generation_id,
                        proof.local_generation_id
                 FROM logical_peer_common_bases AS common_base
                 JOIN logical_sync_devices AS device
                   ON device.library_id = common_base.library_id
                  AND device.device_id = common_base.peer_id
                 JOIN logical_sync_device_ack_proofs AS proof
                   ON proof.library_id = device.library_id
                  AND proof.device_id = device.device_id
                 WHERE common_base.peer_id = 'peer' AND common_base.library_id = 'library'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap(),
        (
            common.generation_id.clone(),
            common.generation_id.clone(),
            common.generation_id.clone(),
        )
    );
    let local = store
        .connection
        .query_row::<SyncGenerationIdentity, _, _>(
            "SELECT generation.generation_id, generation.manifest_hash,
                    generation.generation_sequence
             FROM logical_library_head AS head
             JOIN logical_sync_generations AS generation
               ON generation.library_id = head.library_id
              AND generation.generation_id = head.generation_id
             WHERE head.singleton = 1",
            [],
            |row| {
                Ok(SyncGenerationIdentity {
                    generation_id: row.get(0)?,
                    manifest_hash: row.get(1)?,
                    generation_sequence: row.get(2)?,
                })
            },
        )
        .unwrap();
    assert_ne!(local.generation_id, "shared-c");
    let shared = SyncGenerationIdentity {
        generation_id: remote.manifest.generation.clone(),
        manifest_hash: remote.manifest_hash,
        generation_sequence: remote.manifest.generation_sequence.clone(),
    };
    assert!(store
        .advance_active_sync_device_shared_ack(
            "library",
            "peer",
            0,
            &common,
            &common,
            &shared,
            &remote.manifest,
            &local,
        )
        .is_err());
    assert_eq!(
        store.sync_device_ack_state("library", "peer").unwrap(),
        SyncDeviceAckState {
            shared_identity: common.clone(),
            local_identity: common.clone(),
        }
    );
    store
        .advance_active_sync_device_shared_ack(
            "library",
            "peer",
            1,
            &common,
            &common,
            &shared,
            &remote.manifest,
            &local,
        )
        .unwrap();
    store
        .advance_active_sync_device_shared_ack(
            "library",
            "peer",
            1,
            &common,
            &common,
            &shared,
            &remote.manifest,
            &local,
        )
        .unwrap();
    assert_eq!(
        store.sync_device_ack_state("library", "peer").unwrap(),
        SyncDeviceAckState {
            shared_identity: shared.clone(),
            local_identity: local.clone(),
        }
    );

    let mut advanced_source = EmptySource { content_gets: 0 };
    let mut advanced_target = PersistentLogicalDeltaTarget::new_p5_deferred(
        &mut store,
        &cas,
        "peer",
        "library",
        "shared-c",
        &remote.manifest_bytes,
        &staging_root,
        LogicalDeltaConflictPolicy::Reject,
    )
    .unwrap();
    assert!(matches!(
        execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &remote_sizes,
            &mut advanced_source,
            &mut advanced_target,
        ),
        Err(PeerSyncError::ActivationConflict { .. })
    ));
    drop(advanced_target);
    assert_eq!(advanced_source.content_gets, 0);
    assert_eq!(
        store
            .connection
            .query_row::<(String, String, String), _, _>(
                "SELECT common_base.generation_id,
                        device.acknowledged_generation_id,
                        proof.local_generation_id
                 FROM logical_peer_common_bases AS common_base
                 JOIN logical_sync_devices AS device
                   ON device.library_id = common_base.library_id
                  AND device.device_id = common_base.peer_id
                 JOIN logical_sync_device_ack_proofs AS proof
                   ON proof.library_id = device.library_id
                  AND proof.device_id = device.device_id
                 WHERE common_base.peer_id = 'peer' AND common_base.library_id = 'library'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap(),
        (
            shared.generation_id.clone(),
            shared.generation_id,
            local.generation_id,
        )
    );
}

#[test]
fn p5_deferred_response_loss_recognizes_only_the_exact_committed_result() {
    let (directory, mut store, cas) = open_fixture();
    store
        .connection
        .execute(
            "UPDATE root SET value = ?1 WHERE generation = 'revision-0'",
            [json!({"theme":"base"}).to_string()],
        )
        .unwrap();
    let base = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "shared-c".to_owned(),
                generation_sequence: "0".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('peer', 'library', 'shared-c', ?1, '0', 0)",
            [&base.manifest_hash],
        )
        .unwrap();
    let common = SyncGenerationIdentity {
        generation_id: "shared-c".to_owned(),
        manifest_hash: base.manifest_hash,
        generation_sequence: "0".to_owned(),
    };
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::for_test("library", "peer", common.clone(), 1),
            0,
        )
        .unwrap();
    let remote = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "remote-1".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: Some("shared-c".to_owned()),
        source_revision: 1,
        records: vec![ProjectedLogicalRecord::live(
            LogicalRecordLocator::Root,
            LogicalRecordEnvelope::Root {
                value: json!({"theme":"remote"}),
                owner_heads: vec![],
            },
            vec![],
        )],
    })
    .unwrap();
    let record = remote.record_objects.first().unwrap();
    let remote_sizes = [(record.object.hash.clone(), record.object.size)]
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let staging_root = directory.path().join("p5-deferred-response-loss-staging");
    let mut target = PersistentLogicalDeltaTarget::new_p5_deferred(
        &mut store,
        &cas,
        "peer",
        "library",
        "shared-c",
        &remote.manifest_bytes,
        &staging_root,
        LogicalDeltaConflictPolicy::Reject,
    )
    .unwrap();
    let plan = target.build_ready_plan(0).unwrap();
    let mut source = MapSource {
        objects: [(record.object.hash.clone(), record.object.bytes.clone())]
            .into_iter()
            .collect(),
        content_gets: 0,
    };
    assert_eq!(
        execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &remote_sizes,
            &mut source,
            &mut target,
        )
        .unwrap(),
        LogicalDeltaActivation::Activated { revision: 1 }
    );
    drop(target);

    let mut retry_source = EmptySource { content_gets: 0 };
    let mut retry_target = PersistentLogicalDeltaTarget::new_p5_deferred(
        &mut store,
        &cas,
        "peer",
        "library",
        "shared-c",
        &remote.manifest_bytes,
        &staging_root,
        LogicalDeltaConflictPolicy::Reject,
    )
    .unwrap();
    assert_eq!(
        execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &remote_sizes,
            &mut retry_source,
            &mut retry_target,
        )
        .unwrap(),
        LogicalDeltaActivation::AlreadyActive { revision: 1 }
    );
    drop(retry_target);
    assert_eq!(retry_source.content_gets, 0);
    assert_eq!(
        store
            .connection
            .query_row::<SyncGenerationIdentity, _, _>(
                "SELECT generation_id, manifest_hash, generation_sequence
                 FROM logical_peer_common_bases
                 WHERE peer_id = 'peer' AND library_id = 'library'",
                [],
                |row| {
                    Ok(SyncGenerationIdentity {
                        generation_id: row.get(0)?,
                        manifest_hash: row.get(1)?,
                        generation_sequence: row.get(2)?,
                    })
                },
            )
            .unwrap(),
        common
    );
    assert_eq!(
        store.sync_device_ack_state("library", "peer").unwrap(),
        SyncDeviceAckState {
            shared_identity: common.clone(),
            local_identity: common.clone(),
        }
    );

    let mut root = store.read_root(None).unwrap().value;
    root["postDeferredCommit"] = json!(true);
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root_mutations: None,
            root: Some(root),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .unwrap();
    store.seal_active_logical_generation(&cas).unwrap();

    let mut stale_source = EmptySource { content_gets: 0 };
    let mut stale_target = PersistentLogicalDeltaTarget::new_p5_deferred(
        &mut store,
        &cas,
        "peer",
        "library",
        "shared-c",
        &remote.manifest_bytes,
        &staging_root,
        LogicalDeltaConflictPolicy::Reject,
    )
    .unwrap();
    assert!(matches!(
        execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &remote_sizes,
            &mut stale_source,
            &mut stale_target,
        ),
        Err(PeerSyncError::ActivationConflict { .. })
    ));
    drop(stale_target);
    assert_eq!(stale_source.content_gets, 0);
    assert_eq!(
        store.sync_device_ack_state("library", "peer").unwrap(),
        SyncDeviceAckState {
            shared_identity: common.clone(),
            local_identity: common,
        }
    );
}

#[test]
fn p5_target_requires_an_active_registered_device_before_staging() {
    let (directory, mut store, cas) = open_fixture();
    let remote = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "shared-a".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: Some("shared-c".to_owned()),
        source_revision: 1,
        records: vec![],
    })
    .unwrap();

    assert!(PersistentLogicalDeltaTarget::new_p5_deferred(
        &mut store,
        &cas,
        "peer",
        "library",
        "shared-c",
        &remote.manifest_bytes,
        &directory.path().join("p5-unregistered-staging"),
        LogicalDeltaConflictPolicy::Reject,
    )
    .is_err());
}

#[test]
fn p5_remote_activation_atomically_records_shared_ack_and_local_witness() {
    let (directory, mut store, cas) = open_fixture();
    store
        .connection
        .execute(
            "UPDATE root SET value = ?1 WHERE generation = 'revision-0'",
            [json!({"theme":"base"}).to_string()],
        )
        .unwrap();
    let base = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "shared-c".to_owned(),
                generation_sequence: "0".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('peer', 'library', 'shared-c', ?1, '0', 0)",
            [&base.manifest_hash],
        )
        .unwrap();
    let common = SyncGenerationIdentity {
        generation_id: "shared-c".to_owned(),
        manifest_hash: base.manifest_hash.clone(),
        generation_sequence: "0".to_owned(),
    };
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::for_test("library", "peer", common, 1),
            0,
        )
        .unwrap();
    let shared = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "shared-a".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: Some("shared-c".to_owned()),
        source_revision: 1,
        records: vec![ProjectedLogicalRecord::live(
            LogicalRecordLocator::Root,
            LogicalRecordEnvelope::Root {
                value: json!({"theme":"merged"}),
                owner_heads: vec![],
            },
            vec![],
        )],
    })
    .unwrap();
    let record = shared.record_objects.first().unwrap();
    let shared_bytes = shared.manifest_bytes.clone();
    let staging_root = directory.path().join("p5-remote-staging");
    let mut target = PersistentLogicalDeltaTarget::new_p5_remote_shared_ack(
        &mut store,
        &cas,
        "peer",
        "library",
        "shared-c",
        &shared_bytes,
        &staging_root,
        LogicalDeltaConflictPolicy::Reject,
    )
    .unwrap();
    let plan = target.build_ready_plan(0).unwrap();
    let mut source = MapSource {
        objects: [(record.object.hash.clone(), record.object.bytes.clone())]
            .into_iter()
            .collect(),
        content_gets: 0,
    };
    let remote_sizes = [(record.object.hash.clone(), record.object.size)]
        .into_iter()
        .collect();
    assert_eq!(
        execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &remote_sizes,
            &mut source,
            &mut target,
        )
        .unwrap(),
        LogicalDeltaActivation::Activated { revision: 1 }
    );
    drop(target);

    let committed: (String, String, String, String) = store
        .connection
        .query_row(
            "SELECT head.generation_id, common_base.generation_id,
                    device.acknowledged_generation_id, proof.local_generation_id
             FROM logical_library_head AS head
             JOIN logical_peer_common_bases AS common_base
               ON common_base.library_id = head.library_id
             JOIN logical_sync_devices AS device
               ON device.library_id = common_base.library_id
              AND device.device_id = common_base.peer_id
             JOIN logical_sync_device_ack_proofs AS proof
               ON proof.library_id = device.library_id
              AND proof.device_id = device.device_id
             WHERE head.singleton = 1 AND common_base.peer_id = 'peer'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_ne!(committed.0, "shared-a");
    assert_eq!(committed.1, "shared-a");
    assert_eq!(committed.2, "shared-a");
    assert_eq!(committed.3, committed.0);
    assert_eq!(store.read_root(None).unwrap().value["theme"], "merged");

    let mut retry_source = EmptySource { content_gets: 0 };
    let mut retry_target = PersistentLogicalDeltaTarget::new_p5_remote_shared_ack(
        &mut store,
        &cas,
        "peer",
        "library",
        "shared-c",
        &shared_bytes,
        &staging_root,
        LogicalDeltaConflictPolicy::Reject,
    )
    .unwrap();
    assert_eq!(
        execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &remote_sizes,
            &mut retry_source,
            &mut retry_target,
        )
        .unwrap(),
        LogicalDeltaActivation::AlreadyActive { revision: 1 }
    );
    assert_eq!(retry_source.content_gets, 0);
}

#[test]
fn p5_remote_no_op_advances_shared_ack_with_current_local_witness() {
    let (directory, mut store, cas) = open_fixture();
    let base = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "shared-c".to_owned(),
                generation_sequence: "0".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    let mut shared = base.manifest.clone();
    shared.generation = "shared-a".to_owned();
    shared.generation_sequence = "1".to_owned();
    shared.parent_generation = Some("shared-c".to_owned());
    let shared_bytes = encode_logical_manifest(&shared).unwrap();
    let shared_hash = hash(&shared_bytes);
    assert_eq!(
        cas.prepare_bytes(&shared_bytes).unwrap().content_hash,
        shared_hash
    );
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('peer', 'library', 'shared-c', ?1, '0', 0)",
            [&base.manifest_hash],
        )
        .unwrap();
    let common = SyncGenerationIdentity {
        generation_id: "shared-c".to_owned(),
        manifest_hash: base.manifest_hash,
        generation_sequence: "0".to_owned(),
    };
    store
        .attach_verified_sync_device_at_common_base(
            VerifiedSyncDeviceRegistration::for_test("library", "peer", common, 1),
            0,
        )
        .unwrap();
    let staging_root = directory.path().join("p5-remote-no-op-staging");
    let mut target = PersistentLogicalDeltaTarget::new_p5_remote_shared_ack(
        &mut store,
        &cas,
        "peer",
        "library",
        "shared-c",
        &shared_bytes,
        &staging_root,
        LogicalDeltaConflictPolicy::Reject,
    )
    .unwrap();
    let plan = target.build_ready_plan(0).unwrap();
    assert!(plan.apply.is_empty());
    let mut source = EmptySource { content_gets: 0 };

    assert_eq!(
        execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::new(),
            &mut source,
            &mut target,
        )
        .unwrap(),
        LogicalDeltaActivation::Activated { revision: 0 }
    );
    drop(target);

    assert_eq!(source.content_gets, 0);
    assert_eq!(store.revision().unwrap(), 0);
    assert_eq!(shared.generation_sequence, "1");
    assert_eq!(
        store
            .connection
            .query_row::<(String, String, String), _, _>(
                "SELECT common_base.generation_id, device.acknowledged_generation_id,
                        proof.local_generation_id
                 FROM logical_peer_common_bases AS common_base
                 JOIN logical_sync_devices AS device
                   ON device.library_id = common_base.library_id
                  AND device.device_id = common_base.peer_id
                 JOIN logical_sync_device_ack_proofs AS proof
                   ON proof.library_id = device.library_id
                  AND proof.device_id = device.device_id
                 WHERE common_base.peer_id = 'peer' AND common_base.library_id = 'library'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap(),
        (
            "shared-a".to_owned(),
            "shared-a".to_owned(),
            "shared-c".to_owned(),
        )
    );
    assert_eq!(shared_hash, hash(&shared_bytes));
}

#[test]
fn no_op_updates_only_the_durable_common_base_at_the_same_revision() {
    let (directory, mut store, cas) = open_fixture();
    let local = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "local-0".to_owned(),
                generation_sequence: "0".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .expect("build local logical index");
    let (base_manifest, _base_bytes, base_hash) =
        store_remote_base_manifest(&cas, &local.manifest, "remote-0");
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('peer', 'library', 'remote-0', ?1, '0', 0)",
            [&base_hash],
        )
        .expect("seed common base");

    let mut remote_manifest: LogicalManifest = base_manifest;
    remote_manifest.generation = "remote-1".to_owned();
    remote_manifest.generation_sequence = "1".to_owned();
    remote_manifest.parent_generation = Some("remote-0".to_owned());
    let remote_bytes = encode_logical_manifest(&remote_manifest).expect("encode remote manifest");
    let remote_hash = hash(&remote_bytes);
    let plan = ReadyLogicalDeltaPlan {
        expected_local_revision: 0,
        expected_base_manifest_hash: base_hash,
        expected_remote_generation: "remote-1".to_owned(),
        apply: Vec::new(),
        preserve_local_keys: Vec::new(),
        candidate_object_hashes: Vec::new(),
        next_base_manifest_hash: remote_hash.clone(),
        next_base_generation_sequence: "1".to_owned(),
    };
    let staging_root = directory.path().join("logical-delta-staging");
    let mut source = EmptySource { content_gets: 0 };
    let mut target = PersistentLogicalDeltaTarget::new(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote_bytes,
        &staging_root,
    )
    .expect("open staged target");
    assert_eq!(
        target
            .build_ready_plan(0)
            .expect("derive authoritative no-op plan"),
        plan
    );
    let mut missing_preserve = plan.clone();
    missing_preserve.preserve_local_keys =
        vec![encode_logical_record_key(&LogicalRecordLocator::Plugin {
            storage_key: "missing-local-key".to_owned(),
        })
        .unwrap()];
    assert!(matches!(
        target.begin(&missing_preserve),
        Err(PeerSyncError::Validation(_))
    ));

    let activation = execute_logical_delta_pull(
        &plan,
        &BTreeSet::new(),
        &cas,
        &BTreeMap::new(),
        &mut source,
        &mut target,
    )
    .expect("activate no-op pull");
    drop(target);

    assert_eq!(
        activation,
        LogicalDeltaActivation::Activated { revision: 0 }
    );
    assert_eq!(source.content_gets, 0);
    assert_eq!(store.revision().unwrap(), 0);
    assert_eq!(
        store
            .connection
            .query_row::<(String, String, String), _, _>(
                "SELECT generation_id, manifest_hash, generation_sequence
                 FROM logical_peer_common_bases
                 WHERE peer_id = 'peer' AND library_id = 'library'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap(),
        ("remote-1".to_owned(), remote_hash, "1".to_owned())
    );
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM root WHERE generation LIKE 'staging-logical-%'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        0
    );
    assert!(!staging_root.exists());

    drop(store);
    drop(cas);
    let mut store = PersistentStore::open(directory.path()).expect("reopen after committed no-op");
    let cas = PayloadCas::new(directory.path()).expect("reopen payload CAS");
    let mut retry_source = EmptySource { content_gets: 0 };
    let mut retry_target = PersistentLogicalDeltaTarget::new(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote_bytes,
        &staging_root,
    )
    .expect("reopen no-op target after simulated process kill");
    let retry = execute_logical_delta_pull(
        &plan,
        &BTreeSet::new(),
        &cas,
        &BTreeMap::new(),
        &mut retry_source,
        &mut retry_target,
    )
    .expect("reconcile committed no-op retry");
    drop(retry_target);
    assert_eq!(retry, LogicalDeltaActivation::AlreadyActive { revision: 0 });
    assert_eq!(retry_source.content_gets, 0);
    assert!(!staging_root.exists());

    store
        .connection
        .execute(
            "UPDATE meta SET value = '1' WHERE key = 'currentRevision'",
            [],
        )
        .expect("simulate a later local commit");
    let mut stale_source = EmptySource { content_gets: 0 };
    let mut stale_target = PersistentLogicalDeltaTarget::new(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote_bytes,
        &staging_root,
    )
    .expect("open stale retry target");
    assert!(matches!(
        execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::new(),
            &mut stale_source,
            &mut stale_target,
        ),
        Err(PeerSyncError::ActivationConflict {
            expected: Some(expected),
            actual: Some(actual),
        }) if expected == "0" && actual == "1"
    ));
    assert_eq!(stale_source.content_gets, 0);
    assert!(!staging_root.exists());
}

#[test]
fn divergent_local_and_remote_edits_are_rejected_before_staging() {
    let (directory, mut store, cas) = open_fixture();
    store
        .connection
        .execute(
            "UPDATE root SET value = ?1 WHERE generation = 'revision-0'",
            [json!({"theme":"base"}).to_string()],
        )
        .unwrap();
    let base_projection = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "base-projection".to_owned(),
                generation_sequence: "0".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    let (_base, _bytes, base_hash) =
        store_remote_base_manifest(&cas, &base_projection.manifest, "remote-0");
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('peer', 'library', 'remote-0', ?1, '0', 0)",
            [&base_hash],
        )
        .unwrap();
    store
        .connection
        .execute("DELETE FROM logical_library_head WHERE singleton = 1", [])
        .unwrap();
    store
        .prune_logical_generation("library", "base-projection")
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE root SET value = ?1 WHERE generation = 'revision-0'",
            [json!({"theme":"local"}).to_string()],
        )
        .unwrap();
    store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "local-0".to_owned(),
                generation_sequence: "0".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    let remote = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "remote-1".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: Some("remote-0".to_owned()),
        source_revision: 1,
        records: vec![ProjectedLogicalRecord::live(
            LogicalRecordLocator::Root,
            LogicalRecordEnvelope::Root {
                value: json!({"theme":"remote"}),
                owner_heads: vec![],
            },
            vec![],
        )],
    })
    .unwrap();
    let record = &remote.record_objects[0];
    let plan = ReadyLogicalDeltaPlan {
        expected_local_revision: 0,
        expected_base_manifest_hash: base_hash,
        expected_remote_generation: "remote-1".to_owned(),
        apply: vec![LogicalDeltaApplyOperation::Put {
            key: record.key.clone(),
            object_hash: record.object.hash.clone(),
            dependencies: vec![],
        }],
        preserve_local_keys: vec![],
        candidate_object_hashes: vec![record.object.hash.clone()],
        next_base_manifest_hash: remote.manifest_hash.clone(),
        next_base_generation_sequence: "1".to_owned(),
    };
    let staging_root = directory.path().join("logical-delta-staging");
    let mut target = PersistentLogicalDeltaTarget::new(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote.manifest_bytes,
        &staging_root,
    )
    .unwrap();

    assert_eq!(
        target.resolve_authoritative_plan(0).unwrap(),
        LogicalDeltaPlanResolution::Conflict {
            conflicts: vec![LogicalDeltaConflict {
                record: record.key.clone(),
                kind: LogicalDeltaConflictKind::SameRecord,
            }],
        }
    );
    expect_begin_merge_conflict(&mut target, &plan);
    assert!(!staging_root.exists());

    drop(target);
    let mut target = PersistentLogicalDeltaTarget::new_with_conflict_policy(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote.manifest_bytes,
        &staging_root,
        LogicalDeltaConflictPolicy::PreferRemote,
    )
    .unwrap();
    let resolution = target.resolve_authoritative_plan(0).unwrap();
    let (plan, conflicts) = match resolution {
        LogicalDeltaPlanResolution::Ready { plan, conflicts } => (plan, conflicts),
        LogicalDeltaPlanResolution::Conflict { .. } => {
            panic!("prefer-remote policy must produce an authoritative plan")
        }
    };
    assert_eq!(
        conflicts,
        vec![LogicalDeltaConflict {
            record: record.key.clone(),
            kind: LogicalDeltaConflictKind::SameRecord,
        }]
    );
    let mut source = MapSource {
        objects: [(record.object.hash.clone(), record.object.bytes.clone())]
            .into_iter()
            .collect(),
        content_gets: 0,
    };
    let remote_sizes = [(record.object.hash.clone(), record.object.size)]
        .into_iter()
        .collect();
    assert_eq!(
        execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &remote_sizes,
            &mut source,
            &mut target,
        )
        .unwrap(),
        LogicalDeltaActivation::Activated { revision: 1 }
    );
    drop(target);
    assert_eq!(
        store.read_root(None).unwrap().value,
        json!({"theme":"remote"})
    );
}

#[test]
fn changed_pull_applies_every_record_family_with_durable_cas_payloads() {
    let (directory, mut store, cas) = open_fixture();
    store
        .connection
        .execute(
            "UPDATE root SET value = ?1 WHERE generation = 'revision-0'",
            [json!({"theme":"base"}).to_string()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO bot_presets (
                generation, preset_id, configured_index, name, image, value
             ) VALUES ('revision-0', 'delete-me', 0, 'Delete', NULL, ?1)",
            [json!({"name":"Delete"}).to_string()],
        )
        .unwrap();
    let base_projection = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "base-projection".to_owned(),
                generation_sequence: "12".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    let (_base_manifest, _base_bytes, base_hash) =
        store_remote_base_manifest(&cas, &base_projection.manifest, "remote-0");
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('peer', 'library', 'remote-0', ?1, '12', 0)",
            [&base_hash],
        )
        .unwrap();
    store
        .connection
        .execute("DELETE FROM logical_library_head WHERE singleton = 1", [])
        .unwrap();
    store
        .prune_logical_generation("library", "base-projection")
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO plugin_storage (
                generation, storage_key, byte_size, ordinal, value
             ) VALUES ('revision-0', 'local-only', 14, 0, ?1)",
            [json!({"local":true}).to_string()],
        )
        .unwrap();
    let local = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "local-0".to_owned(),
                generation_sequence: "12".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();

    let owner_payload = b"owner payload exact bytes".to_vec();
    let owner_payload_hash = hash(&owner_payload);
    let owner_manifest = encode_owner_manifest(&[OwnerManifestEntry {
        tuple: [
            "label".to_owned(),
            "assets/owner.bin".to_owned(),
            "bin".to_owned(),
        ],
        payload_hash: Some(bytes_hash(&owner_payload_hash)),
    }])
    .unwrap();
    let owner_manifest_hash = hash(&owner_manifest);
    let root_heads = vec![
        LogicalOwnerHead::present(
            LogicalOwnerLocator::PersonaEmbeddedModule { index: 0 },
            owner_manifest_hash.clone(),
            1,
            1,
        )
        .unwrap(),
        LogicalOwnerHead::present(
            LogicalOwnerLocator::RootModule { index: 0 },
            owner_manifest_hash.clone(),
            1,
            0,
        )
        .unwrap(),
    ];
    let character_heads = vec![LogicalOwnerHead::present(
        LogicalOwnerLocator::CharacterAdditional {
            character_id: "char".to_owned(),
        },
        owner_manifest_hash.clone(),
        1,
        3,
    )
    .unwrap()];
    let owner_dependencies = vec![descriptor(&owner_manifest), descriptor(&owner_payload)];

    let page_a = encode_message_page(
        &(0..128)
            .map(|index| json!({"chatId":format!("m{index}"),"data":index}))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let page_b = encode_message_page(&[json!({"chatId":"m128","data":128})]).unwrap();
    let asset_bytes = b"ordinary asset exact bytes".to_vec();
    let inlay_bytes = b"inlay exact bytes".to_vec();
    let asset_hash = hash(&asset_bytes);
    let inlay_hash = hash(&inlay_bytes);
    let records = vec![
        ProjectedLogicalRecord::live(
            LogicalRecordLocator::Root,
            LogicalRecordEnvelope::Root {
                value: json!({
                    "theme":"remote",
                    "modules":[{
                        "assets":[["label","assets/owner.bin","bin","module-tail",{"rank":1}]],
                        "before":"module-before",
                        "name":"Module",
                        "after":"module-after"
                    }],
                    "personas":[{"embeddedModule":{
                        "before":"persona-before",
                        "assets":[["label","assets/owner.bin","bin","persona-tail",{"rank":2}]],
                        "name":"Embedded",
                        "after":"persona-after"
                    }}]
                }),
                owner_heads: root_heads,
            },
            owner_dependencies.clone(),
        ),
        ProjectedLogicalRecord::live(
            LogicalRecordLocator::Preset {
                preset_id: "remote-preset".to_owned(),
            },
            LogicalRecordEnvelope::Preset {
                configured_index: 4,
                value: json!({"name":"Remote preset","image":"preset.png"}),
            },
            vec![],
        ),
        ProjectedLogicalRecord::tombstone(
            LogicalRecordLocator::Preset {
                preset_id: "delete-me".to_owned(),
            },
            "13".to_owned(),
        ),
        ProjectedLogicalRecord::live(
            LogicalRecordLocator::Plugin {
                storage_key: "remote-plugin".to_owned(),
            },
            LogicalRecordEnvelope::Plugin {
                ordinal: 3,
                value: json!({"remote":true}),
            },
            vec![],
        ),
        ProjectedLogicalRecord::live(
            LogicalRecordLocator::Character {
                character_id: "char".to_owned(),
            },
            LogicalRecordEnvelope::Character {
                configured_index: 2,
                detail: json!({
                    "chaId":"char",
                    "name":"Remote character",
                    "unknown":"character-after"
                }),
                owner_heads: character_heads,
            },
            owner_dependencies,
        ),
        ProjectedLogicalRecord::live(
            LogicalRecordLocator::Conversation {
                character_id: "char".to_owned(),
                conversation_id: "chat".to_owned(),
            },
            LogicalRecordEnvelope::Conversation {
                configured_index: 1,
                recent_at: 123,
                detail: json!({"id":"chat","name":"Remote chat"}),
                message_page_hashes: vec![page_a.hash.clone(), page_b.hash.clone()],
            },
            vec![
                LogicalManifestObject {
                    hash: page_a.hash.clone(),
                    size: page_a.size,
                },
                LogicalManifestObject {
                    hash: page_b.hash.clone(),
                    size: page_b.size,
                },
            ],
        ),
        ProjectedLogicalRecord::live(
            LogicalRecordLocator::Asset {
                logical_key: "same-key".to_owned(),
            },
            LogicalRecordEnvelope::Asset {
                object_hash: Some(asset_hash.clone()),
                size: asset_bytes.len() as u64,
                metadata: encode_asset_alias_metadata(&LogicalAssetAliasMetadata {
                    mime: "application/octet-stream".to_owned(),
                    name: "asset".to_owned(),
                    ext: "bin".to_owned(),
                    inlay_type: None,
                    width: None,
                    height: None,
                    metadata: json!({"scope":"module"}),
                })
                .unwrap(),
            },
            vec![descriptor(&asset_bytes)],
        ),
        ProjectedLogicalRecord::live(
            LogicalRecordLocator::Inlay {
                logical_key: "same-key".to_owned(),
            },
            LogicalRecordEnvelope::Inlay {
                object_hash: Some(inlay_hash.clone()),
                size: inlay_bytes.len() as u64,
                metadata: encode_asset_alias_metadata(&LogicalAssetAliasMetadata {
                    mime: "image/webp".to_owned(),
                    name: "inlay".to_owned(),
                    ext: "webp".to_owned(),
                    inlay_type: Some("image".to_owned()),
                    width: Some(640),
                    height: Some(480),
                    metadata: json!({"animated":false}),
                })
                .unwrap(),
            },
            vec![descriptor(&inlay_bytes)],
        ),
        ProjectedLogicalRecord::live(
            LogicalRecordLocator::Cold {
                logical_key: "missing-cold".to_owned(),
            },
            LogicalRecordEnvelope::Cold {
                object_hash: None,
                size: 999,
                metadata: json!({"missing":true}),
            },
            vec![],
        ),
    ];
    let remote = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "remote-1".to_owned(),
        generation_sequence: "13".to_owned(),
        parent_generation: Some("remote-0".to_owned()),
        source_revision: 1,
        records,
    })
    .expect("build remote manifest with transitive owner payloads");
    let mut source_objects = BTreeMap::from([
        (page_a.hash.clone(), page_a.bytes.clone()),
        (page_b.hash.clone(), page_b.bytes.clone()),
        (owner_manifest_hash.clone(), owner_manifest.clone()),
        (owner_payload_hash.clone(), owner_payload.clone()),
        (asset_hash.clone(), asset_bytes.clone()),
        (inlay_hash.clone(), inlay_bytes.clone()),
    ]);
    for record in &remote.record_objects {
        source_objects.insert(record.object.hash.clone(), record.object.bytes.clone());
    }
    let apply = remote
        .manifest
        .records
        .iter()
        .map(|record| match record {
            LogicalManifestRecord::Live(record) => LogicalDeltaApplyOperation::Put {
                key: record.key.clone(),
                object_hash: record.object_hash.clone(),
                dependencies: record.dependencies.clone(),
            },
            LogicalManifestRecord::Tombstone(record) => LogicalDeltaApplyOperation::Delete {
                key: record.key.clone(),
                deleted_generation_sequence: record.deleted_generation_sequence.clone(),
            },
        })
        .collect::<Vec<_>>();
    let plan = ReadyLogicalDeltaPlan {
        expected_local_revision: 0,
        expected_base_manifest_hash: base_hash,
        expected_remote_generation: remote.manifest.generation.clone(),
        apply,
        preserve_local_keys: vec![encode_logical_record_key(&LogicalRecordLocator::Plugin {
            storage_key: "local-only".to_owned(),
        })
        .unwrap()],
        candidate_object_hashes: remote
            .manifest
            .objects
            .iter()
            .map(|object| object.hash.clone())
            .collect(),
        next_base_manifest_hash: remote.manifest_hash.clone(),
        next_base_generation_sequence: "13".to_owned(),
    };
    let remote_sizes = remote
        .manifest
        .objects
        .iter()
        .map(|object| (object.hash.clone(), object.size))
        .collect::<BTreeMap<_, _>>();
    let local_hashes = local
        .manifest
        .objects
        .iter()
        .map(|object| object.hash.clone())
        .collect::<BTreeSet<_>>();
    let staging_root = directory.path().join("logical-delta-staging");
    let mut source = MapSource {
        objects: source_objects,
        content_gets: 0,
    };
    let job = RefCell::new(
        DurableCasJob::begin(
            directory.path(),
            "logical-changed-payloads",
            CasJobKind::LogicalDeltaTarget,
            0,
        )
        .expect("begin durable changed logical target job"),
    );
    let mut target = PersistentLogicalDeltaTarget::new_with_durable_job(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote.manifest_bytes,
        &staging_root,
        &job,
    )
    .unwrap();

    let mut forged_no_op = plan.clone();
    forged_no_op.apply.clear();
    forged_no_op.preserve_local_keys.clear();
    forged_no_op.candidate_object_hashes.clear();
    expect_begin_validation(&mut target, &forged_no_op);

    let mut omitted_operation = plan.clone();
    omitted_operation.apply.pop();
    omitted_operation.candidate_object_hashes = omitted_operation
        .apply
        .iter()
        .filter_map(|operation| match operation {
            LogicalDeltaApplyOperation::Put {
                object_hash,
                dependencies,
                ..
            } => Some(std::iter::once(object_hash.clone()).chain(dependencies.iter().cloned())),
            LogicalDeltaApplyOperation::Delete { .. } => None,
        })
        .flatten()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    expect_begin_validation(&mut target, &omitted_operation);

    let mut missing_candidate = plan.clone();
    missing_candidate.candidate_object_hashes.pop();
    expect_begin_validation(&mut target, &missing_candidate);

    let mut extra_candidate = plan.clone();
    extra_candidate.candidate_object_hashes.push("f".repeat(64));
    extra_candidate.candidate_object_hashes.sort();
    expect_begin_validation(&mut target, &extra_candidate);

    let mut omitted_preserve = plan.clone();
    omitted_preserve.preserve_local_keys.clear();
    expect_begin_validation(&mut target, &omitted_preserve);

    let activation = execute_logical_delta_pull(
        &plan,
        &local_hashes,
        &cas,
        &remote_sizes,
        &mut source,
        &mut target,
    )
    .expect("activate changed logical pull");
    drop(target);

    assert_eq!(
        activation,
        LogicalDeltaActivation::Activated { revision: 1 }
    );
    let activated_logical_head = store
        .connection
        .query_row::<(String, String, i64, String, String), _, _>(
            "SELECT generation.generation_id, generation.pds_generation,
                    generation.source_revision, generation.state,
                    generation.manifest_hash
             FROM logical_library_head AS head
             JOIN logical_sync_generations AS generation
               ON generation.library_id = head.library_id
              AND generation.generation_id = head.generation_id
             WHERE head.singleton = 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_ne!(activated_logical_head.0, "local-0");
    assert_eq!(activated_logical_head.1, "revision-1");
    assert_eq!(activated_logical_head.2, 1);
    assert_eq!(activated_logical_head.3, "complete");
    let durable_roots = job.borrow().root_set().expect("read sealed job roots");
    assert!(durable_roots.object_hashes.contains(&asset_hash));
    assert!(durable_roots
        .object_hashes
        .contains(&activated_logical_head.4));
    assert!(source.content_gets > 0);
    let root = store.read_root(None).unwrap().value;
    assert_eq!(root["theme"], "remote");
    assert_eq!(
        root["modules"][0]["assets"],
        json!([["label", "assets/owner.bin", "bin", "module-tail", {"rank":1}]])
    );
    assert_eq!(
        root["modules"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["assets", "before", "name", "after"]
    );
    assert_eq!(
        root["personas"][0]["embeddedModule"]["assets"],
        json!([["label", "assets/owner.bin", "bin", "persona-tail", {"rank":2}]])
    );
    assert_eq!(
        root["personas"][0]["embeddedModule"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["before", "assets", "name", "after"]
    );
    let character = store.read_character("char", None).unwrap().unwrap().value;
    assert_eq!(
        character["additionalAssets"],
        json!([["label", "assets/owner.bin", "bin"]])
    );
    assert_eq!(
        character
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["chaId", "name", "unknown", "additionalAssets"]
    );
    let conversation = store
        .read_conversation("char", "chat", None)
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(conversation["message"].as_array().unwrap().len(), 129);
    assert!(store.read_preset("delete-me", None).unwrap().is_none());
    assert!(store.read_preset("remote-preset", None).unwrap().is_some());
    assert!(store
        .read_plugin_storage("local-only", None)
        .unwrap()
        .is_some());
    assert!(store
        .read_plugin_storage("remote-plugin", None)
        .unwrap()
        .is_some());
    let preserved_plugin_key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
        storage_key: "local-only".to_owned(),
    })
    .unwrap();
    assert!(store
        .connection
        .query_row::<bool, _, _>(
            "SELECT EXISTS(
                SELECT 1
                FROM logical_record_heads AS head
                JOIN logical_sync_generations AS generation
                  ON generation.library_id = head.library_id
                 AND generation.generation_id = head.generation_id
                WHERE generation.library_id = 'library'
                  AND generation.pds_generation = 'revision-1'
                  AND generation.state = 'complete'
                  AND head.record_key = ?1 AND head.state = 'live'
             )",
            [preserved_plugin_key],
            |row| row.get(0),
        )
        .unwrap());
    assert_eq!(
        cas.read_object(&owner_manifest_hash).unwrap().unwrap(),
        owner_manifest
    );
    assert_eq!(
        cas.read_object(&owner_payload_hash).unwrap().unwrap(),
        owner_payload
    );
    assert_eq!(cas.read_object(&asset_hash).unwrap().unwrap(), asset_bytes);
    assert_eq!(cas.read_object(&inlay_hash).unwrap().unwrap(), inlay_bytes);
    for record in &remote.record_objects {
        assert_eq!(
            cas.stat_object(&record.object.hash).unwrap(),
            Some(record.object.size)
        );
        assert!(durable_roots.object_hashes.contains(&record.object.hash));
    }
    assert_eq!(
        store
            .connection
            .query_row::<(Option<String>, i64), _, _>(
                "SELECT object_hash, size FROM cold_aliases
                 WHERE generation = 'revision-1' AND key = 'missing-cold'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap(),
        (None, 999)
    );
    let deleted_key = encode_logical_record_key(&LogicalRecordLocator::Preset {
        preset_id: "delete-me".to_owned(),
    })
    .unwrap();
    assert_eq!(
        store
            .connection
            .query_row::<String, _, _>(
                "SELECT head.deleted_generation_sequence
                 FROM logical_record_heads AS head
                 JOIN logical_sync_generations AS generation
                   ON generation.library_id = head.library_id
                  AND generation.generation_id = head.generation_id
                 WHERE generation.library_id = 'library'
                   AND generation.pds_generation = 'revision-1'
                   AND head.record_key = ?1 AND head.state = 'tombstone'",
                [deleted_key],
                |row| row.get(0),
            )
            .unwrap(),
        "13"
    );
    assert_eq!(
        store
            .connection
            .query_row::<String, _, _>(
                "SELECT generation_sequence
                 FROM logical_sync_generations
                 WHERE library_id = 'library' AND pds_generation = 'revision-1'
                   AND state = 'complete'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        "14"
    );
    assert_eq!(
        store
            .connection
            .query_row::<(String, String, String), _, _>(
                "SELECT generation_id, manifest_hash, generation_sequence
                 FROM logical_peer_common_bases
                 WHERE peer_id = 'peer' AND library_id = 'library'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap(),
        ("remote-1".to_owned(), remote.manifest_hash, "13".to_owned(),)
    );
    assert!(!staging_root.exists() || staging_root.read_dir().unwrap().next().is_none());

    let retry_hashes = remote
        .manifest
        .objects
        .iter()
        .map(|object| object.hash.clone())
        .collect::<BTreeSet<_>>();
    let retry_sizes = remote
        .manifest
        .objects
        .iter()
        .map(|object| (object.hash.clone(), object.size))
        .collect::<BTreeMap<_, _>>();
    let remote_bytes = remote.manifest_bytes.clone();
    drop(store);
    drop(cas);
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let mut retry_source = EmptySource { content_gets: 0 };
    let mut retry_target = PersistentLogicalDeltaTarget::new(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote_bytes,
        &staging_root,
    )
    .unwrap();
    assert_eq!(
        execute_logical_delta_pull(
            &plan,
            &retry_hashes,
            &cas,
            &retry_sizes,
            &mut retry_source,
            &mut retry_target,
        )
        .unwrap(),
        LogicalDeltaActivation::AlreadyActive { revision: 1 }
    );
    drop(retry_target);
    assert_eq!(retry_source.content_gets, 0);
    assert!(!staging_root.exists() || staging_root.read_dir().unwrap().next().is_none());

    let mut next_root = root.clone();
    next_root["postActivationCommit"] = json!(true);
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root_mutations: None,
            root: Some(next_root),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: Some(vec![
                AssetOwnerHead::present(
                    AssetOwnerLocator::RootModuleAssets { index: 0 },
                    owner_manifest_hash.clone(),
                    1,
                ),
                AssetOwnerHead::present(
                    AssetOwnerLocator::PersonaEmbeddedModuleAssets { index: 0 },
                    owner_manifest_hash,
                    1,
                ),
            ]),
        })
        .expect("ordinary commit follows logical target activation");
    let next_manifest = store
        .seal_active_logical_generation(&cas)
        .expect("seal logical child after ordinary commit");
    assert_eq!(next_manifest.manifest.source_revision, 2);
    assert_eq!(
        store
            .connection
            .query_row::<(String, String), _, _>(
                "SELECT generation.generation_id, generation.pds_generation
                 FROM logical_library_head AS head
                 JOIN logical_sync_generations AS generation
                   ON generation.library_id = head.library_id
                  AND generation.generation_id = head.generation_id
                 WHERE head.singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap(),
        (next_manifest.manifest.generation, "revision-2".to_owned())
    );

    store
        .connection
        .execute(
            "UPDATE meta SET value = '2' WHERE key = 'currentRevision'",
            [],
        )
        .unwrap();
    let mut stale_source = EmptySource { content_gets: 0 };
    let mut stale_target = PersistentLogicalDeltaTarget::new(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote_bytes,
        &staging_root,
    )
    .unwrap();
    assert!(matches!(
        execute_logical_delta_pull(
            &plan,
            &retry_hashes,
            &cas,
            &retry_sizes,
            &mut stale_source,
            &mut stale_target,
        ),
        Err(PeerSyncError::ActivationConflict {
            expected: Some(expected),
            actual: Some(actual),
        }) if expected == "1" && actual == "2"
    ));
    assert_eq!(stale_source.content_gets, 0);
}

#[test]
fn activation_conflict_aborts_staging_without_changing_the_active_generation() {
    let (directory, mut store, cas) = open_fixture();
    store
        .connection
        .execute(
            "UPDATE root SET value = ?1 WHERE generation = 'revision-0'",
            [json!({"theme":"local"}).to_string()],
        )
        .unwrap();
    let local = store
        .rebuild_logical_index(
            &cas,
            LogicalIndexBuildRequest {
                library_id: "library".to_owned(),
                generation_id: "local-0".to_owned(),
                generation_sequence: "0".to_owned(),
                parent_generation_id: None,
                lease: None,
            },
        )
        .unwrap();
    let (_base, _base_bytes, base_hash) =
        store_remote_base_manifest(&cas, &local.manifest, "remote-0");
    store
        .connection
        .execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES ('peer', 'library', 'remote-0', ?1, '0', 0)",
            [&base_hash],
        )
        .unwrap();
    let remote = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "remote-1".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: Some("remote-0".to_owned()),
        source_revision: 1,
        records: vec![ProjectedLogicalRecord::live(
            LogicalRecordLocator::Root,
            LogicalRecordEnvelope::Root {
                value: json!({"theme":"remote"}),
                owner_heads: vec![],
            },
            vec![],
        )],
    })
    .unwrap();
    let record = remote.record_objects.first().unwrap();
    let plan = ReadyLogicalDeltaPlan {
        expected_local_revision: 0,
        expected_base_manifest_hash: base_hash,
        expected_remote_generation: "remote-1".to_owned(),
        apply: vec![LogicalDeltaApplyOperation::Put {
            key: record.key.clone(),
            object_hash: record.object.hash.clone(),
            dependencies: vec![],
        }],
        preserve_local_keys: vec![],
        candidate_object_hashes: vec![record.object.hash.clone()],
        next_base_manifest_hash: remote.manifest_hash.clone(),
        next_base_generation_sequence: "1".to_owned(),
    };
    let staging_root = directory.path().join("peer-bidirectional").join("staging");
    let mut target = PersistentLogicalDeltaTarget::new(
        &mut store,
        &cas,
        "peer",
        "library",
        "local-0",
        &remote.manifest_bytes,
        &staging_root,
    )
    .unwrap();
    let mut stage = target.begin(&plan).unwrap();
    target
        .stage_payload(
            &mut stage,
            &LogicalDeltaObject {
                hash: record.object.hash.clone(),
                size: record.object.size,
            },
            &mut Cursor::new(record.object.bytes.clone()),
        )
        .unwrap();
    let active_stage = match &stage {
        PersistentLogicalDeltaStage::Changed {
            staging_directory, ..
        } => staging_directory.clone(),
        _ => unreachable!(),
    };
    assert_eq!(
        crate::peer_sync::maintenance::cleanup_temp(directory.path())
            .unwrap()
            .count,
        0
    );
    assert!(active_stage.exists());
    target.stage_database_changes(&mut stage, &plan).unwrap();
    let competing_hash = "f".repeat(64);
    target
        .store
        .connection
        .execute(
            "UPDATE logical_peer_common_bases SET manifest_hash = ?1
             WHERE peer_id = 'peer' AND library_id = 'library'",
            [&competing_hash],
        )
        .unwrap();

    assert_eq!(
        target
            .activate_database_and_base_if_current(
                &mut stage,
                0,
                &plan.expected_base_manifest_hash,
                &plan.next_base_manifest_hash,
                &plan.next_base_generation_sequence,
            )
            .unwrap(),
        LogicalDeltaActivation::Conflict {
            actual_revision: 0,
            actual_base_manifest_hash: competing_hash.clone(),
        }
    );
    target.abort(stage).unwrap();
    assert!(target.durable_abort_succeeded());
    drop(target);

    assert_eq!(store.revision().unwrap(), 0);
    assert_eq!(store.read_root(None).unwrap().value["theme"], "local");
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM root WHERE generation LIKE 'staging-logical-%'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM logical_sync_generations
                 WHERE library_id = 'library' AND generation_id != 'local-0'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .connection
            .query_row::<String, _, _>(
                "SELECT manifest_hash FROM logical_peer_common_bases
                 WHERE peer_id = 'peer' AND library_id = 'library'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        competing_hash
    );
    assert_eq!(
        store
            .connection
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM snapshot_leases
                 WHERE lease LIKE 'logical-delta-pin-%'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        0
    );
    assert!(!staging_root.exists());
    assert_eq!(cas.stat_object(&record.object.hash).unwrap(), None);
}

#[test]
fn retained_module_owner_property_rejects_noncanonical_or_mismatched_forms() {
    let head = ResolvedOwnerHead {
        head: LogicalOwnerHead::present(
            LogicalOwnerLocator::RootModule { index: 0 },
            "11".repeat(32),
            1,
            0,
        )
        .unwrap(),
        tuples: Some(vec![json!(["label", "assets/owner.bin", "bin"])]),
    };
    let cases = [
        (
            json!({"assets":[["label","assets/owner.bin","bin"]],"name":"Module"}),
            "exact-three logical owner property must use canonical stripping",
        ),
        (
            json!({"assets":[["different","assets/owner.bin","bin","tail"]],"name":"Module"}),
            "retained logical owner tuple differs from its manifest",
        ),
        (
            json!({"before":true,"assets":[["label","assets/owner.bin","bin","tail"]]}),
            "retained logical owner property index differs from its head",
        ),
    ];

    for (module, expected) in cases {
        let mut root = json!({"modules":[module]});
        let error = rehydrate_root_owners(&mut root, std::slice::from_ref(&head)).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
}
