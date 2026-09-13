//! Explicit ownership coverage for the current live PDS schema. This is not an archive whitelist.
use std::collections::BTreeSet;
#[test]
fn all_live_tables_have_an_explicit_preservation_owner() {
    let portable = [
        "root",
        "bot_presets",
        "characters",
        "conversations",
        "messages",
        "plugin_storage",
        "asset_aliases",
        "asset_owner_heads",
        "asset_repository_authority",
        "cold_aliases",
        "cold_payload_authority",
    ];
    let local_operations = [
        "meta",
        "app_kv",
        "snapshot_leases",
        "asset_objects",
        "asset_gc_maintenance_state",
        "asset_alias_replacement_candidates",
        "asset_object_deletions",
        "content_change_context",
        "content_changes",
        "content_change_consumers",
        "content_change_floor",
        "content_capture_reservations",
        "library_sync_selection",
        "local_library_identity",
        "external_storage_jobs",
        "external_storage_bases",
        "external_storage_captures",
        "external_storage_capture_refs",
        "external_storage_capture_files",
        "external_storage_content_cache",
    ];
    let server_operations = [
        "server_sync_state",
        "server_sync_context",
        "server_sync_dirty",
        "server_sync_base",
        "server_sync_scope_base",
        "server_sync_scope_clear_base",
        "server_sync_clears",
        "server_sync_clear_members",
        "server_sync_operation",
        "server_sync_objects",
        "server_sync_operation_records",
        "server_sync_operation_pages",
        "server_sync_operation_scopes",
        "server_sync_remote",
        "server_sync_remote_dirty",
        "server_sync_remote_cursor",
    ];
    let groups = [&portable[..], &local_operations[..], &server_operations[..]];
    let mut classified = BTreeSet::new();
    for group in groups {
        for name in group {
            assert!(
                classified.insert(name.to_string()),
                "table classified twice: {name}"
            );
        }
    }
    let mut db = rusqlite::Connection::open_in_memory().unwrap();
    super::schema::initialize(&mut db).unwrap();
    let actual = db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<BTreeSet<_>, _>>()
        .unwrap();
    assert_eq!(
        actual, classified,
        "Update preservation ownership when adding or removing a table"
    );
    assert!(!portable.contains(&"asset_alias_replacement_candidates"));
    assert!(!portable.contains(&"server_sync_operation"));
}
