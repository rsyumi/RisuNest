use super::*;

#[test]
fn storage_stats_reports_database_pages_and_zero_global_catalogs_for_a_new_store() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open store");

    let stats = store.storage_stats().expect("storage stats");

    assert!(stats.database_bytes > 0);
    assert_eq!(stats.asset_objects.count, 0);
    assert_eq!(stats.asset_objects.bytes, 0);
    assert_eq!(stats.cold_aliases.count, 0);
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
         ('revision-0', 'active', 0, 0, 0, 'active', 0, 'character', '{}'),
         ('revision-old', 'inactive', 0, 0, 0, 'inactive', 0, 'character', '{}');
         INSERT INTO conversations (generation, character_id, conversation_id, configured_index, recent_at, name, message_count, detail) VALUES
         ('revision-0', 'active', 'chat', 0, 0, 'chat', 3, '{}'),
         ('revision-old', 'inactive', 'chat', 0, 0, 'chat', 9, '{}');
         INSERT INTO cold_aliases (generation, key, object_hash, size, metadata) VALUES
         ('revision-0', 'cold-active', NULL, 5, '{}'), ('revision-old', 'cold-old', NULL, 11, '{}');
         INSERT INTO plugin_storage (generation, storage_key, byte_size, ordinal, value) VALUES
         ('revision-0', 'plugin-active', 7, 0, '{}'), ('revision-old', 'plugin-old', 13, 0, '{}');
         INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext, inlay_type, width, height, metadata) VALUES
         ('revision-0', 'asset-active', NULL, 'asset', 17, '', '', '', NULL, NULL, NULL, '{}'),
         ('revision-old', 'asset-old', NULL, 'asset', 19, '', '', '', NULL, NULL, NULL, '{}');
         INSERT INTO asset_objects (object_hash, byte_size, created_at_ms) VALUES ('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 23, 0);
         INSERT INTO asset_object_deletions (object_hash, byte_size, physical_key, state, created_at_ms) VALUES ('bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 29, 'assets-v2/objects/bb/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'pending', 0);"
    ).expect("seed rows");

    let stats = store.storage_stats().expect("storage stats");
    assert_eq!(stats.characters.active.count, 1);
    assert_eq!(stats.conversations.count, 1);
    assert_eq!(stats.conversations.message_count, 3);
    assert_eq!(stats.cold_aliases.bytes, 5);
    assert_eq!(stats.plugin_storage.bytes, 7);
    assert_eq!(stats.asset_aliases[0].bytes, 17);
    assert_eq!(stats.asset_objects.bytes, 23);
    assert_eq!(stats.asset_object_deletions[0].bytes, 29);
}
