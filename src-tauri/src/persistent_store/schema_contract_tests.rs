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
        "library_sync_selection",
        "local_library_identity",
        "external_storage_jobs",
        "external_storage_bases",
        "external_storage_backup_points",
        "external_storage_history_points",
        "external_storage_captures",
        "external_storage_capture_refs",
        "external_storage_capture_files",
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
        "server_sync_operation_sections",
        "server_sync_remote",
        "server_sync_remote_dirty",
        "server_sync_remote_cursor",
        "server_sync_remote_sections",
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

#[test]
fn content_change_schema_retains_live_protections_without_write_only_tables() {
    let mut db = rusqlite::Connection::open_in_memory().unwrap();
    super::schema::initialize(&mut db).unwrap();
    for name in [
        "content_capture_reservations",
        "external_storage_content_cache",
        "external_storage_conflicts",
    ] {
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get(0),
        ).unwrap();
        assert!(!exists, "retired table remains: {name}");
    }
    for name in [
        "content_changes", "content_change_context", "content_change_floor",
        "content_change_consumers", "external_storage_captures",
        "external_storage_capture_files", "external_storage_capture_refs", "server_sync_dirty",
    ] {
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get(0),
        ).unwrap();
        assert!(exists, "live protection table missing: {name}");
    }
    let primary_key = db.prepare(
        "SELECT name FROM pragma_table_info('content_changes') WHERE pk>0 ORDER BY pk",
    ).unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(primary_key, ["generation", "kind", "key1", "key2"]);
    assert!(db.execute("UPDATE content_change_floor SET revision=-1", []).is_err());
    assert!(db.execute("INSERT INTO content_change_consumers VALUES('negative','revision-0',-1,0)", []).is_err());
    assert!(db.execute("INSERT INTO content_change_consumers VALUES('invalid-rebuild','revision-0',0,2)", []).is_err());
    assert!(db.execute("INSERT INTO content_change_context VALUES(1,'revision-0',0,'unknown')", []).is_err());
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    assert_eq!(version, super::schema::SCHEMA_VERSION);
}
