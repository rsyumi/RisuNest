use super::*;
use crate::persistent_store::StorageCountBytes;

#[test]
fn storage_stats_reports_database_file_bytes_and_zero_global_catalogs_for_a_new_store() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open store");

    let stats = store.storage_stats().expect("storage stats");

    assert!(stats.database_bytes > 0);
    assert_eq!(stats.asset_objects.count, 0);
    assert_eq!(stats.asset_objects.bytes, 0);
    assert_eq!(stats.asset_bodies, StorageCountBytes { count: 0, bytes: 0 });
    assert_eq!(stats.missing_asset_bodies, StorageCountBytes { count: 0, bytes: 0 });
    assert_eq!(stats.inlay_bodies, StorageCountBytes { count: 0, bytes: 0 });
    assert_eq!(stats.plugin_storage.count, 0);
    assert_eq!(stats.characters.active.count, 0);
    assert_eq!(stats.characters.trashed_count, 0);
    assert_eq!(stats.conversations.count, 0);
}

#[test]
fn storage_stats_only_counts_active_generation_rows() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open store");
    store.connection.execute_batch(
        "INSERT INTO characters (generation, character_id, configured_index, recent_at, trashed, name, conversation_count, type, detail) VALUES
         ('revision-0', 'active', 0, 0, 0, 'active', 0, 'character', '\"가나\"'),
         ('revision-old', 'inactive', 0, 0, 0, 'inactive', 0, 'character', '{}');
         INSERT INTO conversations (generation, character_id, conversation_id, configured_index, recent_at, name, message_count, detail) VALUES
         ('revision-0', 'active', 'chat', 0, 0, 'chat', 3, '{}'),
         ('revision-old', 'inactive', 'chat', 0, 0, 'chat', 9, '{}');
         INSERT INTO plugin_storage (generation, owner, storage_key, byte_size, ordinal, value) VALUES
         ('revision-0', 'synthetic-plugin', 'plugin-active', 7, 0, '{}'), ('revision-old', 'synthetic-plugin', 'plugin-old', 13, 0, '{}');
         INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext, inlay_type, width, height, metadata) VALUES
         ('revision-0', 'asset-active', NULL, 'asset', 17, '', '', '', NULL, NULL, NULL, '{}'),
         ('revision-old', 'asset-old', NULL, 'asset', 19, '', '', '', NULL, NULL, NULL, '{}');
         INSERT INTO asset_objects (object_hash, byte_size, created_at_ms) VALUES ('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 23, 0);
         INSERT INTO asset_object_deletions (object_hash, byte_size, physical_key, state, created_at_ms) VALUES ('bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 29, 'assets/objects/bb/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'pending', 0);"
    ).expect("seed rows");

    let stats = store.storage_stats().expect("storage stats");
    assert_eq!(stats.characters.active, StorageCountBytes { count: 1, bytes: "\"가나\"".len() as u64 });
    assert_eq!(stats.conversations.count, 1);
    assert_eq!(stats.conversations.message_count, 3);
    assert_eq!(stats.plugin_storage.bytes, 7);
    assert_eq!(stats.asset_aliases[0].bytes, 17);
    assert_eq!(stats.asset_objects.bytes, 23);
    assert_eq!(stats.asset_bodies, StorageCountBytes { count: 0, bytes: 0 });
    assert_eq!(stats.missing_asset_bodies, StorageCountBytes { count: 1, bytes: 23 });
    assert_eq!(stats.asset_object_deletions[0].bytes, 29);
}

#[test]
fn storage_stats_does_not_wait_for_the_snapshot_archive_lock() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open store");
    // An open archive holds its lock, as a capture or rotation in progress does.
    let archive = crate::persistent_store::snapshot_archive::Archive::open(&store.snapshots_dir)
        .expect("open archive");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(store.storage_stats().map(|stats| stats.snapshot_bytes));
    });
    let snapshot_bytes = receiver
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("storage stats waited for the snapshot archive lock")
        .expect("storage stats");
    assert!(snapshot_bytes > 0);
    assert_eq!(snapshot_bytes, archive.bytes().unwrap());
}

fn put_body(store: &mut PersistentStore, key: &str, inlay: bool, bytes: &[u8]) -> String {
    let object = crate::asset_repository::PayloadCas::new(store.repository_root())
        .unwrap()
        .prepare_bytes(bytes)
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[asset_object_catalog::AssetObjectRegistration {
                object_hash: object.content_hash.clone(),
                byte_size: object.byte_size,
            }],
            1,
        )
        .unwrap();
    let alias = AssetAlias {
        key: key.into(),
        object_hash: Some(object.content_hash.clone()),
        kind: if inlay { "inlay" } else { "asset" }.into(),
        size: bytes.len() as i64,
        mime: "image/png".into(),
        name: "synthetic".into(),
        ext: "png".into(),
        inlay_type: inlay.then(|| "image".into()),
        width: None,
        height: None,
        metadata: serde_json::json!({}),
    };
    store.commit_asset_alias(&alias, store.revision().unwrap()).unwrap();
    object.content_hash
}

#[test]
fn storage_stats_counts_the_bodies_this_device_stores_apart_from_the_catalog() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open store");
    let asset = put_body(&mut store, "assets/synthetic-asset.png", false, &[1; 300]);
    let inlay = put_body(&mut store, "synthetic-inlay", true, &[2; 50]);
    // A second alias for the same inlay body counts it once.
    let shared = put_body(&mut store, "synthetic-inlay-copy", true, &[2; 50]);
    assert_eq!(shared, inlay);
    // A body with no catalog row is still stored here; other names are not bodies.
    let cas = crate::asset_repository::PayloadCas::new(store.repository_root()).unwrap();
    let orphan = cas.prepare_bytes(&[3; 7]).unwrap();
    let shard = store.repository_root().join("assets").join("objects").join(&asset[..2]);
    std::fs::write(shard.join("not-a-body.tmp"), [0; 11]).unwrap();

    let stats = store.storage_stats().expect("storage stats");
    assert_eq!(stats.asset_objects, StorageCountBytes { count: 2, bytes: 350 });
    assert_eq!(stats.asset_bodies, StorageCountBytes { count: 3, bytes: 357 });
    assert_eq!(stats.missing_asset_bodies, StorageCountBytes { count: 0, bytes: 0 });
    assert_eq!(stats.inlay_bodies, StorageCountBytes { count: 1, bytes: 50 });

    for (hash, size) in [(&inlay, 50), (&orphan.content_hash, 7)] {
        assert!(matches!(
            cas.unlink_exact_object(hash, size, &crate::asset_repository::object_physical_key(hash))
                .unwrap(),
            crate::asset_repository::ExactObjectUnlink::Removed { .. }
        ));
    }
    let stats = store.storage_stats().expect("storage stats");
    assert_eq!(stats.asset_objects, StorageCountBytes { count: 2, bytes: 350 });
    assert_eq!(stats.asset_bodies, StorageCountBytes { count: 1, bytes: 300 });
    assert_eq!(stats.missing_asset_bodies, StorageCountBytes { count: 1, bytes: 50 });
    assert_eq!(stats.inlay_bodies, StorageCountBytes { count: 0, bytes: 0 });
}
