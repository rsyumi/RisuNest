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
