use super::{
    logical_delta_source::LogicalDeltaSourceSession, logical_index::LogicalIndexBuildRequest,
    PersistentStore, WorkingSetCommit,
};
use crate::{
    asset_repository::PayloadCas,
    peer_sync::{
        logical_delta::{decode_logical_manifest, LogicalManifestRecord},
        LogicalDeltaObject, LogicalDeltaObjectSource, PeerSyncError,
    },
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::Read;

struct SourceFixture {
    directory: tempfile::TempDir,
    manifest_bytes: Vec<u8>,
    manifest_hash: String,
    root_object: LogicalDeltaObject,
    payload_object: LogicalDeltaObject,
    unrelated_object: LogicalDeltaObject,
}

impl SourceFixture {
    fn create() -> Self {
        let directory = tempfile::tempdir().expect("create source fixture");
        let mut store = PersistentStore::open(directory.path()).expect("open fixture store");
        let cas = PayloadCas::new(directory.path()).expect("open fixture payload CAS");
        let payload = cas
            .prepare_bytes(b"exact ordinary asset bytes")
            .expect("prepare fixture payload");
        let unrelated = cas
            .prepare_bytes(b"unrelated immutable object")
            .expect("prepare unrelated payload");
        store
            .connection
            .execute(
                "UPDATE root SET value = ?1 WHERE generation = 'revision-0'",
                [json!({ "username": "Pinned source" }).to_string()],
            )
            .expect("seed root");
        store
            .connection
            .execute(
                "INSERT INTO asset_aliases (
                    generation, logical_key, object_hash, kind, size, mime, name, ext,
                    inlay_type, width, height, metadata
                 ) VALUES (
                    'revision-0', 'asset.bin', ?1, 'asset', ?2,
                    'application/octet-stream', 'asset.bin', 'bin',
                    NULL, NULL, NULL, '{}'
                 )",
                rusqlite::params![
                    payload.content_hash,
                    i64::try_from(payload.byte_size).expect("payload size fits SQLite")
                ],
            )
            .expect("seed asset alias");
        let built = store
            .rebuild_logical_index(
                &cas,
                LogicalIndexBuildRequest {
                    library_id: "library".to_owned(),
                    generation_id: "generation-0".to_owned(),
                    generation_sequence: "0".to_owned(),
                    parent_generation_id: None,
                    lease: None,
                },
            )
            .expect("build complete logical generation");
        let root_object = built
            .manifest
            .records
            .iter()
            .find_map(|record| match record {
                LogicalManifestRecord::Live(record) if record.key == "r1:root" => {
                    Some(LogicalDeltaObject {
                        hash: record.object_hash.clone(),
                        size: built
                            .manifest
                            .objects
                            .iter()
                            .find(|object| object.hash == record.object_hash)
                            .expect("root object descriptor")
                            .size,
                    })
                }
                _ => None,
            })
            .expect("root record");

        Self {
            directory,
            manifest_bytes: built.manifest_bytes,
            manifest_hash: built.manifest_hash,
            root_object,
            payload_object: LogicalDeltaObject {
                hash: payload.content_hash,
                size: payload.byte_size,
            },
            unrelated_object: LogicalDeltaObject {
                hash: unrelated.content_hash,
                size: unrelated.byte_size,
            },
        }
    }

    fn open_source(&self) -> LogicalDeltaSourceSession {
        LogicalDeltaSourceSession::open(
            self.directory.path(),
            self.directory.path(),
            "library",
            "generation-0",
        )
        .expect("open pinned logical source")
    }
}

#[test]
fn pinned_source_serves_canonical_manifest_and_exact_manifest_objects() {
    let fixture = SourceFixture::create();
    let mut source = fixture.open_source();

    assert_eq!(source.manifest_hash(), fixture.manifest_hash);
    assert_eq!(
        source.manifest_size(),
        u64::try_from(fixture.manifest_bytes.len()).expect("manifest size fits u64")
    );
    let mut manifest_bytes = Vec::new();
    source
        .open_manifest()
        .expect("open manifest")
        .read_to_end(&mut manifest_bytes)
        .expect("read manifest");
    assert_eq!(manifest_bytes, fixture.manifest_bytes);
    assert_eq!(
        hex::encode(Sha256::digest(&manifest_bytes)),
        source.manifest_hash()
    );
    assert_eq!(
        decode_logical_manifest(&manifest_bytes)
            .expect("decode source manifest")
            .objects
            .len(),
        source.objects().len()
    );

    let mut root_bytes = Vec::new();
    source
        .open_object(&fixture.root_object)
        .expect("open reconstructed root record")
        .read_to_end(&mut root_bytes)
        .expect("read root record");
    assert_eq!(
        hex::encode(Sha256::digest(&root_bytes)),
        fixture.root_object.hash
    );
    assert_eq!(root_bytes.len() as u64, fixture.root_object.size);

    let mut payload_bytes = Vec::new();
    source
        .open_object(&fixture.payload_object)
        .expect("open immutable payload")
        .read_to_end(&mut payload_bytes)
        .expect("read immutable payload");
    assert_eq!(payload_bytes, b"exact ordinary asset bytes");
}

#[test]
fn source_rejects_unlisted_objects_and_manifest_size_mismatches_before_opening_storage() {
    let fixture = SourceFixture::create();
    let mut source = fixture.open_source();

    let outside = match source.open_object(&fixture.unrelated_object) {
        Ok(_) => panic!("unlisted CAS object must be rejected"),
        Err(error) => error,
    };
    assert!(
        matches!(outside, PeerSyncError::Validation(message) if message.contains("pinned manifest"))
    );

    let wrong_size = match source.open_object(&LogicalDeltaObject {
        hash: fixture.payload_object.hash.clone(),
        size: fixture.payload_object.size + 1,
    }) {
        Ok(_) => panic!("listed hash with the wrong size must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(wrong_size, PeerSyncError::Validation(message) if message.contains("size")));
}

#[test]
fn source_pin_survives_normal_writes_and_store_reopen_until_explicit_release() {
    let fixture = SourceFixture::create();
    let mut source = fixture.open_source();
    assert_eq!(logical_source_pin_count(fixture.directory.path()), 1);

    let mut writer = PersistentStore::open(fixture.directory.path()).expect("open writer");
    writer
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({ "username": "Changed after pin" })),
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
        .expect("commit while source is pinned");
    drop(writer);
    let reopened = PersistentStore::open(fixture.directory.path()).expect("reopen writer store");
    assert_eq!(reopened.revision().expect("read reopened revision"), 1);

    let mut bytes = Vec::new();
    source
        .open_object(&fixture.root_object)
        .expect("open old source after writer reopen")
        .read_to_end(&mut bytes)
        .expect("read pinned old root");
    assert_eq!(
        hex::encode(Sha256::digest(&bytes)),
        fixture.root_object.hash
    );

    source.release().expect("release source exactly once");
    assert_eq!(logical_source_pin_count(fixture.directory.path()), 0);
    assert!(matches!(
        source.open_object(&fixture.root_object),
        Err(PeerSyncError::Validation(message)) if message.contains("released")
    ));
    assert!(matches!(
        source.open_manifest(),
        Err(PeerSyncError::Validation(message)) if message.contains("released")
    ));
    source.release().expect("repeated release is a no-op");
    assert_eq!(logical_source_pin_count(fixture.directory.path()), 0);
}

#[test]
fn dropping_source_releases_its_logical_generation_session() {
    let fixture = SourceFixture::create();
    {
        let _source = fixture.open_source();
        assert_eq!(logical_source_pin_count(fixture.directory.path()), 1);
    }
    assert_eq!(logical_source_pin_count(fixture.directory.path()), 0);
}

#[test]
fn durable_logical_session_can_resume_after_store_reopen_without_duplicate_pins() {
    let fixture = SourceFixture::create();
    let mut store = PersistentStore::open(fixture.directory.path()).expect("open pin owner");
    let session_id = store
        .pin_logical_generation("library", "generation-0")
        .expect("create durable logical session pin");
    drop(store);
    assert_eq!(logical_source_pin_count(fixture.directory.path()), 1);

    let mut source = LogicalDeltaSourceSession::resume(
        fixture.directory.path(),
        fixture.directory.path(),
        "library",
        "generation-0",
        &session_id,
    )
    .expect("resume durable logical source");
    assert_eq!(source.session_id(), session_id);
    assert_eq!(logical_source_pin_count(fixture.directory.path()), 1);

    let mut bytes = Vec::new();
    source
        .open_object(&fixture.root_object)
        .expect("open object from resumed source")
        .read_to_end(&mut bytes)
        .expect("read resumed source object");
    assert_eq!(
        hex::encode(Sha256::digest(&bytes)),
        fixture.root_object.hash
    );

    source.release().expect("release resumed source");
    assert_eq!(logical_source_pin_count(fixture.directory.path()), 0);
}

#[test]
fn source_open_rejects_missing_or_incomplete_logical_generations_without_leaking_a_session() {
    let fixture = SourceFixture::create();
    let missing = match LogicalDeltaSourceSession::open(
        fixture.directory.path(),
        fixture.directory.path(),
        "library",
        "missing",
    ) {
        Ok(_) => panic!("missing logical generation must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(missing, PeerSyncError::Validation(_)));
    assert_eq!(logical_source_pin_count(fixture.directory.path()), 0);

    let store = PersistentStore::open(fixture.directory.path()).expect("open fixture store");
    store
        .connection
        .execute(
            "INSERT INTO logical_sync_generations (
                library_id, generation_id, generation_sequence, parent_generation_id,
                pds_generation, source_revision, state, manifest_hash, created_at, completed_at
             ) VALUES (
                'library', 'building', '1', 'generation-0',
                'revision-building', 1, 'building', NULL, 1, NULL
             )",
            [],
        )
        .expect("seed incomplete logical generation");
    drop(store);
    let incomplete = match LogicalDeltaSourceSession::open(
        fixture.directory.path(),
        fixture.directory.path(),
        "library",
        "building",
    ) {
        Ok(_) => panic!("incomplete logical generation must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(incomplete, PeerSyncError::Validation(_)));
    assert_eq!(logical_source_pin_count(fixture.directory.path()), 0);

    let mut source = fixture.open_source();
    source
        .release()
        .expect("source still opens after rejection");
}

#[test]
fn source_open_releases_its_pin_when_compact_manifest_validation_fails() {
    let corrupt = SourceFixture::create();
    let store = PersistentStore::open(corrupt.directory.path()).expect("open corrupt fixture");
    store
        .connection
        .execute(
            "UPDATE logical_record_heads
             SET object_size = object_size + 1
             WHERE library_id = 'library' AND generation_id = 'generation-0'
               AND record_key = 'r1:root'",
            [],
        )
        .expect("corrupt compact logical metadata");
    drop(store);
    let corrupt_open = match LogicalDeltaSourceSession::open(
        corrupt.directory.path(),
        corrupt.directory.path(),
        "library",
        "generation-0",
    ) {
        Ok(_) => panic!("corrupt complete logical generation must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(corrupt_open, PeerSyncError::Validation(_)));
    assert_eq!(logical_source_pin_count(corrupt.directory.path()), 0);
}

fn logical_source_pin_count(app_data_dir: &std::path::Path) -> i64 {
    let store = PersistentStore::open(app_data_dir).expect("open store to inspect source pins");
    store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM logical_generation_session_pins",
            [],
            |row| row.get(0),
        )
        .expect("count logical source pins")
}
