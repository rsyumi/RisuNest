use super::{
    active_generation, asset_object_catalog, current_revision, hash_exact_file, schema, snapshot,
    AnchorOccurrence, AssetAlias, AssetAliasListQuery, AssetOwnerHead, AssetOwnerLocator,
    AssetRepositoryAuthorityState, CharacterQuery, CheckpointMode, ColdAlias,
    ColdPayloadAuthorityState, ColdPayloadMigrationInput, ConversationMutation, ConversationPage,
    ConversationQuery, ConversationWindowQuery, PersistentStore, PluginStorageMutation, QueryOrder,
    StoreError, Versioned, WorkingSetCommit, ASSET_GC_PRODUCT_MINIMUM_GRACE_MS,
    ASSET_GC_PRODUCT_PAGE_LIMIT, GENERATION_TABLES, JAVASCRIPT_MAX_SAFE_INTEGER,
};
use rusqlite::{params, Connection, TransactionBehavior};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Arc,
    },
    thread,
    time::Duration,
};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../fixtures/persistent-fixture.json"))
        .expect("parse persistent store fixture")
}

fn staged_root(database: &Value) -> Value {
    let mut root = database.clone();
    let root = root.as_object_mut().expect("fixture database object");
    root.remove("characters");
    root.remove("botPresets");
    Value::Object(root.clone())
}

fn root(database: &Value) -> Value {
    let mut root = staged_root(database);
    let root = root.as_object_mut().expect("fixture staged root object");
    root.remove("pluginCustomStorage");
    Value::Object(root.clone())
}

fn open_fixture() -> (tempfile::TempDir, PersistentStore, Value) {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let database = fixture();
    let staging = store.replace_begin().expect("begin staged replacement");
    let root = staged_root(&database);
    let characters = database["characters"]
        .as_array()
        .expect("fixture characters");
    store
        .replace_put_root(&staging.staging_id, &root)
        .expect("stage fixture root");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"].as_array().expect("fixture presets"),
        )
        .expect("stage fixture presets");
    store
        .replace_add_characters(&staging.staging_id, characters)
        .expect("stage fixture characters");
    assert_eq!(
        store
            .replace_commit(&staging.staging_id, Some(0))
            .expect("commit fixture")
            .revision,
        1
    );
    (directory, store, database)
}

fn empty_working_set_commit(expected_revision: i64) -> WorkingSetCommit {
    WorkingSetCommit {
        expected_revision,
        root_mutations: None,
        root: None,
        replace_presets: None,
        character: None,
        character_details: None,
        replace_character: None,
        add_character: None,
        conversations: None,
        delete_character_id: None,
        plugin_storage: None,
        asset_owner_heads: None,
    }
}

fn table_columns(
    connection: &rusqlite::Connection,
    table: &str,
) -> Vec<(String, String, bool, Option<String>, i64)> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info('{table}')"))
        .expect("prepare table column query");
    statement
        .query_map([], |row| {
            Ok((
                row.get(1)?,
                row.get(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("query table columns")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect table columns")
}

fn stage_root(store: &mut PersistentStore, username: &str) -> String {
    let staging = store.replace_begin().expect("begin staged replacement");
    store
        .replace_put_root(&staging.staging_id, &json!({ "username": username }))
        .expect("stage replacement root");
    staging.staging_id
}

#[path = "tests/asset_alias_tests.rs"]
mod asset_alias_tests;
#[path = "tests/asset_catalog_gc_tests.rs"]
mod asset_catalog_gc_tests;
#[path = "tests/cold_payload_tests.rs"]
mod cold_payload_tests;
#[path = "tests/display_name_tests.rs"]
mod display_name_tests;
#[path = "tests/replacement_tests.rs"]
mod replacement_tests;
#[path = "tests/schema_migration_tests.rs"]
mod schema_migration_tests;
mod server_sync_apply_tests;
mod server_sync_engine_tests;
#[path = "tests/server_sync_outbox_tests.rs"]
mod server_sync_outbox_tests;
#[path = "tests/server_sync_projection_tests.rs"]
mod server_sync_projection_tests;
#[path = "tests/snapshot_lease_tests.rs"]
mod snapshot_lease_tests;
#[path = "tests/storage_stats_tests.rs"]
mod storage_stats_tests;
#[path = "tests/working_set_tests.rs"]
mod working_set_tests;

fn reconstruct_snapshot(store: &PersistentStore, id: &str) -> (tempfile::TempDir, Connection) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reconstructed.db");
    fs::write(&path, []).unwrap();
    super::snapshot_archive::Archive::open(&store.snapshots_dir)
        .unwrap()
        .restore(id, &path)
        .unwrap();
    let connection = Connection::open(path).unwrap();
    (directory, connection)
}
