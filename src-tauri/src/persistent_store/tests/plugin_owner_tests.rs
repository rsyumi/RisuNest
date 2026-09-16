use super::*;
use crate::persistent_store::plugin_owner::UNOWNED_OWNER;

fn store_with_rows(rows: &[(&str, &str, Value)]) -> (tempfile::TempDir, PersistentStore) {
    let directory = tempfile::tempdir().expect("create plugin owner directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let revision = store.revision().expect("read revision");
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(
                rows.iter()
                    .map(|(owner, key, value)| PluginStorageMutation::Set {
                        owner: (*owner).to_owned(),
                        key: (*key).to_owned(),
                        value: value.clone(),
                    })
                    .collect(),
            ),
            ..empty_working_set_commit(revision)
        })
        .expect("seed plugin rows");
    (directory, store)
}

/// Invariant 13 rests on the sentinel surviving SQLite untouched. `length()` on
/// this column stops at the first NUL, so the column is compared and never
/// measured.
#[test]
fn the_unowned_sentinel_round_trips_through_the_primary_key_and_change_log() {
    let (_directory, store) = store_with_rows(&[
        (UNOWNED_OWNER, "shared", json!("imported")),
        ("plugin-a", "shared", json!("owned")),
    ]);
    let generation = active_generation(&store.connection).expect("read generation");

    let owners: Vec<String> = store
        .connection
        .prepare("SELECT owner FROM plugin_storage WHERE generation=?1 ORDER BY owner")
        .expect("prepare owners")
        .query_map([&generation], |row| row.get(0))
        .expect("query owners")
        .collect::<Result<_, _>>()
        .expect("collect owners");
    assert_eq!(owners, vec![UNOWNED_OWNER.to_owned(), "plugin-a".to_owned()]);

    let stored: String = store
        .connection
        .query_row(
            "SELECT value FROM plugin_storage
             WHERE generation=?1 AND owner=?2 AND storage_key='shared'",
            params![&generation, UNOWNED_OWNER],
            |row| row.get(0),
        )
        .expect("read sentinel row by equality");
    assert_eq!(
        serde_json::from_str::<Value>(&stored).expect("parse sentinel value"),
        json!("imported")
    );
    assert!(store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM plugin_storage WHERE generation=?1 AND owner=''",
            [&generation],
            |row| row.get::<_, i64>(0),
        )
        .expect("count empty owners")
        == 0);

    let sentinel_changes: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM content_changes
             WHERE kind='plugin' AND key1=?1 AND key2='shared'",
            [UNOWNED_OWNER],
            |row| row.get(0),
        )
        .expect("count sentinel change rows");
    assert_eq!(sentinel_changes, 1);
    let named_changes: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM content_changes
             WHERE kind='plugin' AND key1='plugin-a' AND key2='shared'",
            [],
            |row| row.get(0),
        )
        .expect("count named change rows");
    assert_eq!(named_changes, 1);

    let round_tripped: Value =
        serde_json::from_str(&serde_json::to_string(&json!({ "owner": UNOWNED_OWNER })).unwrap())
            .expect("json round trip");
    assert_eq!(round_tripped["owner"], json!(UNOWNED_OWNER));
}

/// Invariant 11. One owner never observes another owner's rows.
#[test]
fn a_plugin_reads_and_clears_only_its_own_rows() {
    let (_directory, mut store) = store_with_rows(&[
        ("plugin-a", "a-key", json!("a")),
        ("plugin-b", "b-key", json!("b")),
    ]);

    assert!(store
        .read_plugin_storage("plugin-a", "b-key", None)
        .expect("read across owners")
        .is_none());

    let revision = store.revision().expect("read revision");
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Clear {
                owner: "plugin-a".to_owned(),
            }]),
            ..empty_working_set_commit(revision)
        })
        .expect("clear one owner");

    assert!(store
        .read_plugin_storage("plugin-a", "a-key", None)
        .expect("read cleared owner")
        .is_none());
    assert_eq!(
        store
            .read_plugin_storage("plugin-b", "b-key", None)
            .expect("read surviving owner")
            .expect("surviving row")
            .value,
        json!("b")
    );
}

/// Invariant 23. A write never moves a sentinel row to the writing plugin.
#[test]
fn writing_the_same_key_leaves_the_unowned_row_untouched() {
    let (_directory, mut store) =
        store_with_rows(&[(UNOWNED_OWNER, "pm_store", json!({ "apiKey": "imported" }))]);

    let revision = store.revision().expect("read revision");
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: "provider-manager".to_owned(),
                key: "pm_store".to_owned(),
                value: json!({ "apiKey": "default" }),
            }]),
            ..empty_working_set_commit(revision)
        })
        .expect("write the same key under a real owner");

    assert_eq!(
        store
            .read_plugin_storage(UNOWNED_OWNER, "pm_store", None)
            .expect("read sentinel row")
            .expect("sentinel row survives")
            .value,
        json!({ "apiKey": "imported" })
    );
    assert_eq!(
        store
            .read_plugin_storage("provider-manager", "pm_store", None)
            .expect("read owned row")
            .expect("owned row exists")
            .value,
        json!({ "apiKey": "default" })
    );
}

/// Invariant 26. The list answers with sizes and never with values.
#[test]
fn the_storage_list_reports_sizes_without_carrying_values() {
    let (_directory, store) = store_with_rows(&[
        ("plugin-a", "a-key", json!("aaaaaaaa")),
        ("plugin-b", "b-key", json!({ "nested": "bbbbbbbb" })),
    ]);

    let listed = store
        .list_plugin_storage(None)
        .expect("list plugin storage");
    let serialized = serde_json::to_string(&listed).expect("serialize listing");
    assert!(!serialized.contains("aaaaaaaa"));
    assert!(!serialized.contains("bbbbbbbb"));
    assert!(!serialized.contains("nested"));
    assert_eq!(listed.len(), 2);
    for item in &listed {
        assert!(item.byte_size > 0);
        assert!(item.value_type == "string" || item.value_type == "json");
    }
}

/// Invariant 14. A key two plugins hold leaves the upstream save entirely.
#[test]
fn exporting_drops_only_the_keys_two_plugins_both_hold() {
    let (_directory, store) = store_with_rows(&[
        ("plugin-a", "unique-a", json!("a")),
        ("plugin-b", "unique-b", json!("b")),
        ("plugin-a", "shared", json!("from a")),
        ("plugin-b", "shared", json!("from b")),
    ]);
    let generation = active_generation(&store.connection).expect("read generation");

    let flattened = super::super::export::flattened_plugin_storage(&store.connection, &generation)
        .expect("flatten plugin storage");
    assert_eq!(
        flattened.values.keys().collect::<Vec<_>>(),
        vec!["unique-a", "unique-b"]
    );
    assert_eq!(
        flattened.collisions,
        vec![(
            "shared".to_owned(),
            vec!["plugin-a".to_owned(), "plugin-b".to_owned()]
        )]
    );

    let meta = super::super::export::plugin_storage_meta_value(&flattened.owners);
    assert_eq!(meta["unique-a"]["plugin"], json!("plugin-a"));
    assert_eq!(meta["unique-b"]["plugin"], json!("plugin-b"));
    assert!(meta.get("shared").is_none());
}

/// The sidecar restores ownership; a save without one lands on the sentinel.
#[test]
fn an_import_restores_ownership_from_the_sidecar_and_otherwise_stays_unowned() {
    let directory = tempfile::tempdir().expect("create import directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let staging = store.replace_begin().expect("begin replacement");
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({
                "pluginCustomStorage": { "known": "value", "unknown": "value" },
                "pluginStorageMeta": {
                    "known": { "plugin": "plugin-a", "updatedAt": 17 },
                    "forged": { "plugin": "plugin-b", "updatedAt": 17 }
                }
            }),
        )
        .expect("stage imported plugin storage");
    let imported = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("activate import");

    assert!(store
        .read_root(Some(imported.revision))
        .expect("read root")
        .value
        .get("pluginStorageMeta")
        .is_none());
    assert_eq!(
        store
            .read_plugin_storage("plugin-a", "known", None)
            .expect("read restored row")
            .expect("restored row exists")
            .value,
        json!("value")
    );
    assert_eq!(
        store
            .read_plugin_storage(UNOWNED_OWNER, "unknown", None)
            .expect("read unowned row")
            .expect("unowned row exists")
            .value,
        json!("value")
    );
    assert!(store
        .read_plugin_storage("plugin-b", "forged", None)
        .expect("read sidecar only key")
        .is_none());
}
