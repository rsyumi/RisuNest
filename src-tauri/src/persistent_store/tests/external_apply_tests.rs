use super::*;
use crate::{
    asset_repository::owner_manifest_codec::{encode_owner_manifest, OwnerManifestEntry},
    logical_records::{
        encode_asset_alias_metadata, encode_logical_record, encode_logical_record_key,
        encode_message_page, LogicalAssetAliasMetadata, LogicalOwnerHead, LogicalOwnerLocator,
        LogicalRecordEnvelope, LogicalRecordLocator,
    },
    persistent_store::{external_storage_state, sync_selection},
};
use risunest_external_storage_format::format::{fingerprint, library_fingerprint_domain};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path};



fn open_store() -> (tempfile::TempDir, PersistentStore) {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let stage = store.replace_begin().unwrap();
    store
        .replace_put_root(
            &stage.staging_id,
            &json!({
                "marker": "local",
                "statics": { "messages": ["local-only"] }
            }),
        )
        .unwrap();
    store
        .replace_add_characters(
            &stage.staging_id,
            &[json!({
                "chaId": "character",
                "name": "Local character",
                "chatPage": 7,
                "lastInteraction": 700,
                "chats": []
            })],
        )
        .unwrap();
    store.replace_commit(&stage.staging_id, Some(0)).unwrap();
    (directory, store)
}

fn write_record(
    root: &Path,
    locator: LogicalRecordLocator,
    envelope: LogicalRecordEnvelope,
) -> (ExternalSnapshotRecord, [u8; 32]) {
    let key = encode_logical_record_key(&locator).unwrap();
    let encoded = encode_logical_record(&envelope).unwrap();
    let path = root.join(format!("record-{}", encoded.hash));
    fs::write(&path, &encoded.bytes).unwrap();
    (
        ExternalSnapshotRecord {
            key,
            content_hash: encoded.hash.clone(),
            byte_length: encoded.size,
            path,
        },
        hex::decode(encoded.hash).unwrap().try_into().unwrap(),
    )
}

fn write_object(root: &Path, bytes: &[u8]) -> ExternalSnapshotObject {
    let object =
        risunest_external_storage_format::logical_records::encoded_object(bytes.to_vec()).unwrap();
    let path = root.join(format!("object-{}", object.hash));
    fs::write(&path, bytes).unwrap();
    ExternalSnapshotObject {
        content_hash: object.hash,
        byte_length: object.size,
        path,
    }
}

fn application<'a>(
    root: &'a Path,
    scope_id: &'a [u8; 32],
    fingerprint: &'a [u8; 32],
    revision: i64,
) -> ExternalSnapshotApplication<'a> {
    ExternalSnapshotApplication {
        expected_revision: revision,
        staging_root: root,
        scope_id,
        fingerprint,
    }
}

fn root_record(root: &Path, marker: &str) -> (ExternalSnapshotRecord, [u8; 32]) {
    write_record(
        root,
        LogicalRecordLocator::Root,
        LogicalRecordEnvelope::Root {
            value: json!({
                "marker": marker,
                "statics": { "messages": ["remote-device-value"] }
            }),
            owner_heads: vec![],
        },
    )
}

fn stage_count(store: &PersistentStore) -> i64 {
    store
        .connection
        .query_row(
            "SELECT count(*) FROM root WHERE generation LIKE 'staging-%'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn external_snapshot_stages_streamed_records_and_preserves_local_view_fields() {
    let (directory, mut store) = open_store();
    let staging = directory.path().join("download");
    fs::create_dir(&staging).unwrap();
    let page = encode_message_page(&[json!({"chatId":"message","data":"remote"})]).unwrap();
    let page_object = write_object(&staging, &page.bytes);
    let owner_payload = write_object(&staging, b"synthetic owner payload");
    let owner_manifest_bytes = encode_owner_manifest(&[OwnerManifestEntry {
        tuple: ["asset".into(), "owner.bin".into(), "binary".into()],
        payload_hash: Some(
            hex::decode(&owner_payload.content_hash)
                .unwrap()
                .try_into()
                .unwrap(),
        ),
    }])
    .unwrap();
    let owner_manifest = write_object(&staging, &owner_manifest_bytes);
    let (conversation, conversation_hash) = write_record(
        &staging,
        LogicalRecordLocator::Conversation {
            character_id: "character".into(),
            conversation_id: "conversation".into(),
        },
        LogicalRecordEnvelope::Conversation {
            configured_index: 0,
            recent_at: 20,
            detail: json!({"id":"conversation","name":"Remote conversation"}),
            message_page_hashes: vec![page.hash],
        },
    );
    let (root, root_hash) = write_record(
        &staging,
        LogicalRecordLocator::Root,
        LogicalRecordEnvelope::Root {
            value: json!({
                "marker": "remote",
                "statics": { "messages": ["remote-device-value"] },
                "modules": [{"name":"Remote module"}]
            }),
            owner_heads: vec![LogicalOwnerHead::present(
                LogicalOwnerLocator::RootModule { index: 0 },
                owner_manifest.content_hash.clone(),
                1,
                1,
            )
            .unwrap()],
        },
    );
    let (character, character_hash) = write_record(
        &staging,
        LogicalRecordLocator::Character {
            character_id: "character".into(),
        },
        LogicalRecordEnvelope::Character {
            configured_index: 0,
            detail: json!({
                "chaId":"character",
                "name":"Remote character",
                "chatPage":1,
                "lastInteraction":10
            }),
            owner_heads: vec![LogicalOwnerHead::absent(
                LogicalOwnerLocator::CharacterAdditional {
                    character_id: "character".into(),
                },
            )],
        },
    );
    let records = vec![conversation, root, character];
    let mut hashes = BTreeMap::new();
    for (record, hash) in records
        .iter()
        .zip([conversation_hash, root_hash, character_hash])
    {
        hashes.insert(record.key.clone(), hash);
    }
    let scope_id = library_fingerprint_domain();
    let fingerprint = fingerprint(&scope_id, &hashes);
    let prepared = store
        .prepare_external_snapshot_application(
            &application(&staging, &scope_id, &fingerprint, 1),
            records.into_iter().map(Ok),
            [Ok(page_object), Ok(owner_manifest), Ok(owner_payload)],
        )
        .unwrap();

    let staged = store.materialize_staging(&prepared.staging_id).unwrap();
    assert_eq!(staged["marker"], "remote");
    assert_eq!(staged["statics"]["messages"], json!(["local-only"]));
    assert_eq!(
        staged["modules"][0]["assets"],
        json!([["asset", "owner.bin", "binary"]])
    );
    assert_eq!(staged["characters"][0]["name"], "Remote character");
    assert_eq!(staged["characters"][0]["chatPage"], 7);
    assert_eq!(staged["characters"][0]["lastInteraction"], 700);
    assert_eq!(
        staged["characters"][0]["chats"][0]["message"][0]["data"],
        "remote"
    );
    assert_eq!(store.finish_prepared_replace(prepared).unwrap().revision, 2);
}

#[test]
fn external_snapshot_stage_feeds_atomic_normal_receive_activation() {
    let (directory, mut store) = open_store();
    let staging = directory.path().join("download");
    fs::create_dir(&staging).unwrap();
    let epoch = sync_selection::read(&store.connection).unwrap().epoch;
    let tx = store.connection.transaction().unwrap();
    sync_selection::select(
        &tx,
        &epoch,
        &sync_selection::SyncTarget::External("connection".into()),
    )
    .unwrap();
    tx.commit().unwrap();
    let identity = sync_selection::identity(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    external_storage_state::prepare_receive(
        &tx,
        &external_storage_state::ReceiveIntent {
            job_id: "receive",
            connection_id: "connection",
            repository_id: "repository",
            snapshot_id: "snapshot",
            commit_id: "commit",
            authenticated_head: "head-token",
            identity: &identity,
        },
    )
    .unwrap();
    tx.commit().unwrap();

    let (root, root_hash) = root_record(&staging, "remote-sync");
    let hashes = BTreeMap::from([(root.key.clone(), root_hash)]);
    let scope_id = library_fingerprint_domain();
    let fingerprint = fingerprint(&scope_id, &hashes);
    let prepared = store
        .prepare_external_snapshot_application(
            &application(&staging, &scope_id, &fingerprint, identity.revision),
            [Ok(root)],
            std::iter::empty(),
        )
        .unwrap();
    store.finish_external_receive(prepared, "receive").unwrap();

    let after = sync_selection::identity(&store.connection).unwrap();
    assert_eq!(after.revision, identity.revision + 1);
    assert_eq!(after.library_epoch, identity.library_epoch);
    assert!(
        !sync_selection::read(&store.connection)
            .unwrap()
            .decision_required
    );
    let (snapshot, phase): (String, String) = store
        .connection
        .query_row(
            "SELECT snapshot_id,(SELECT phase FROM external_storage_jobs WHERE id='receive') FROM external_storage_bases",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (snapshot.as_str(), phase.as_str()),
        ("snapshot", "complete")
    );
}

#[test]
fn rejected_record_stream_rolls_back_the_whole_stage_and_preserves_live_library() {
    let (directory, mut store) = open_store();
    let staging = directory.path().join("download");
    fs::create_dir(&staging).unwrap();
    let (root, root_hash) = root_record(&staging, "rejected");
    let hashes = BTreeMap::from([(root.key.clone(), root_hash)]);
    let scope_id = library_fingerprint_domain();
    let fingerprint = fingerprint(&scope_id, &hashes);
    let error = match store.prepare_external_snapshot_application(
        &application(&staging, &scope_id, &fingerprint, 1),
        vec![
            Ok(root),
            Err(StoreError::Validation {
                message: "synthetic stream failure".into(),
            }),
        ],
        std::iter::empty(),
    ) {
        Ok(_) => panic!("stream failure must reject the stage"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("synthetic stream failure"));
    assert_eq!(stage_count(&store), 0);
    assert_eq!(store.materialize(None).unwrap()["marker"], "local");
}

#[test]
fn duplicate_key_key_mismatch_missing_payload_and_stale_revision_are_rejected() {
    for case in ["duplicate", "mismatch", "missing-payload", "stale"] {
        let (directory, mut store) = open_store();
        let staging = directory.path().join("download");
        fs::create_dir(&staging).unwrap();
        let (root, root_hash) = root_record(&staging, "rejected");
        let mut records = vec![root.clone()];
        let mut hashes = BTreeMap::from([(root.key.clone(), root_hash)]);
        if case == "duplicate" {
            records.push(root);
        } else if case == "mismatch" {
            let (mut mismatch, hash) = root_record(&staging, "mismatch");
            mismatch.key = encode_logical_record_key(&LogicalRecordLocator::Preset {
                preset_id: "preset".into(),
            })
            .unwrap();
            hashes.clear();
            hashes.insert(mismatch.key.clone(), hash);
            records = vec![mismatch];
        } else if case == "missing-payload" {
            let metadata = encode_asset_alias_metadata(&LogicalAssetAliasMetadata {
                mime: "application/octet-stream".into(),
                name: "payload.bin".into(),
                ext: "bin".into(),
                inlay_type: None,
                width: None,
                height: None,
                metadata: json!({}),
            })
            .unwrap();
            let (asset, hash) = write_record(
                &staging,
                LogicalRecordLocator::Asset {
                    logical_key: "asset".into(),
                },
                LogicalRecordEnvelope::Asset {
                    object_hash: Some("a".repeat(64)),
                    size: 4,
                    metadata,
                },
            );
            hashes.insert(asset.key.clone(), hash);
            records.push(asset);
        }
            let scope_id = library_fingerprint_domain();
        let fingerprint = fingerprint(&scope_id, &hashes);
        assert!(store
            .prepare_external_snapshot_application(
                &application(
                    &staging,
                    &scope_id,
                    &fingerprint,
                    if case == "stale" { 0 } else { 1 },
                ),
                records.into_iter().map(Ok),
                std::iter::empty(),
            )
            .is_err());
        assert_eq!(stage_count(&store), 0);
        assert_eq!(store.materialize(None).unwrap()["marker"], "local");
    }
}

#[test]
fn object_hash_and_scope_mismatches_are_rejected_before_activation() {
    let (directory, mut store) = open_store();
    let staging = directory.path().join("download");
    fs::create_dir(&staging).unwrap();
    let (root, root_hash) = root_record(&staging, "rejected");
    let hashes = BTreeMap::from([(root.key.clone(), root_hash)]);
    let scope_id = library_fingerprint_domain();
    let fingerprint = fingerprint(&scope_id, &hashes);
    let bytes = b"synthetic object";
    let mut object = write_object(&staging, bytes);
    object.content_hash = "b".repeat(64);
    assert!(store
        .prepare_external_snapshot_application(
            &application(&staging, &scope_id, &fingerprint, 1),
            [Ok(root.clone())],
            [Ok(object)],
        )
        .is_err());
    assert_eq!(stage_count(&store), 0);

    let wrong_scope = [9; 32];
    assert!(store
        .prepare_external_snapshot_application(
            &application(&staging, &wrong_scope, &fingerprint, 1),
            [Ok(root)],
            std::iter::empty(),
        )
        .is_err());
    assert_eq!(stage_count(&store), 0);
}

#[test]
fn connection_removal_cancels_local_jobs_releases_roots_and_deselects() {
    let (_directory, mut store) = open_store();
    let initial = sync_selection::read(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    let selected = sync_selection::select(
        &tx,
        &initial.epoch,
        &sync_selection::SyncTarget::External("connection".into()),
    )
    .unwrap();
    tx.commit().unwrap();
    let identity =
        serde_json::to_string(&sync_selection::identity(&store.connection).unwrap()).unwrap();
    store.connection.execute(
        "INSERT INTO external_storage_jobs VALUES('ready','connection','repository','capture-ready',?1,'backup',NULL,NULL,'point','ready')",
        [&identity],
    ).unwrap();
    store.connection.execute(
        "INSERT INTO external_storage_capture_refs VALUES('capture-ready','ready')",
        [],
    ).unwrap();
    store.connection.execute(
        "INSERT INTO external_storage_bases VALUES('connection','repository','snapshot','commit','observation',?1)",
        [&identity],
    ).unwrap();

    store
        .external_prepare_connection_removal("connection")
        .unwrap();

    let phases: Vec<String> = {
        let mut query = store.connection.prepare(
            "SELECT phase FROM external_storage_jobs WHERE connection_id='connection' ORDER BY id",
        ).unwrap();
        query
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(phases, ["cancelled"]);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT count(*) FROM external_storage_capture_refs",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT count(*) FROM external_storage_bases WHERE connection_id='connection'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    let after = sync_selection::read(&store.connection).unwrap();
    assert_eq!(after.target, sync_selection::SyncTarget::None);
    assert_ne!(after.epoch, selected.epoch);
}

#[test]
fn connection_removal_detaches_unknown_publication_but_keeps_its_recovery_root() {
    for phase in ["publishing", "publicationUnknown"] {
        let (_directory, mut store) = open_store();
        let initial = sync_selection::read(&store.connection).unwrap();
        let tx = store.connection.transaction().unwrap();
        sync_selection::select(
            &tx,
            &initial.epoch,
            &sync_selection::SyncTarget::External("connection".into()),
        )
        .unwrap();
        tx.commit().unwrap();
        let identity =
            serde_json::to_string(&sync_selection::identity(&store.connection).unwrap()).unwrap();
        store.connection.execute(
            "INSERT INTO external_storage_jobs VALUES('unsettled','connection','repository','capture',?1,?2,'cas','head','commit',?3)",
            rusqlite::params![identity, "sync", phase],
        ).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO external_storage_capture_refs VALUES('capture','unsettled')",
                [],
            )
            .unwrap();
        store.connection.execute(
            "INSERT INTO external_storage_bases VALUES('connection','repository','snapshot','commit','observation',?1)",
            [&identity],
        ).unwrap();

        store.external_prepare_connection_removal("connection").unwrap();

        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT phase FROM external_storage_jobs WHERE id='unsettled'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "publicationUnknown"
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT count(*) FROM external_storage_capture_refs",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT count(*) FROM external_storage_bases WHERE connection_id='connection'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        let after = sync_selection::read(&store.connection).unwrap();
        assert_eq!(after.target, sync_selection::SyncTarget::None);
    }

    let (_directory, mut store) = open_store();
    let initial = sync_selection::read(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    let selected = sync_selection::select(
        &tx,
        &initial.epoch,
        &sync_selection::SyncTarget::External("connection".into()),
    )
    .unwrap();
    tx.commit().unwrap();
    let identity = serde_json::to_string(&sync_selection::identity(&store.connection).unwrap()).unwrap();
    store.connection.execute(
        "INSERT INTO external_storage_jobs VALUES('unsettled','connection','repository','capture',?1,'restore','cas','head','commit','applying')",
        [&identity],
    ).unwrap();
    assert!(store.external_prepare_connection_removal("connection").is_err());
    assert_eq!(sync_selection::read(&store.connection).unwrap().epoch, selected.epoch);
}
