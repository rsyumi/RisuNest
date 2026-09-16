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
    store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("activate import");

    assert!(store
        .read_root(None)
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

/// Invariant 12. A plugin write is durable once the commit returns, so a fresh
/// process reading the same directory finds it without any further step.
#[test]
fn a_committed_plugin_write_survives_reopening_the_store() {
    let directory = tempfile::tempdir().expect("create durability directory");
    let generation;
    {
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let revision = store.revision().expect("read revision");
        store
            .commit(&WorkingSetCommit {
                plugin_storage: Some(vec![PluginStorageMutation::Set {
                    owner: "plugin-a".to_owned(),
                    key: "durable".to_owned(),
                    value: json!({ "kept": true }),
                }]),
                ..empty_working_set_commit(revision)
            })
            .expect("commit plugin write");
        generation = active_generation(&store.connection).expect("read generation");
    }

    let reopened = PersistentStore::open(directory.path()).expect("reopen persistent store");
    assert_eq!(
        active_generation(&reopened.connection).expect("read generation again"),
        generation
    );
    assert_eq!(
        reopened
            .read_plugin_storage("plugin-a", "durable", None)
            .expect("read durable row")
            .expect("durable row exists")
            .value,
        json!({ "kept": true })
    );
    assert!(reopened
        .read_plugin_storage(UNOWNED_OWNER, "durable", None)
        .expect("read unowned row")
        .is_none());
}

fn imported_store(
    values: Value,
    meta: Value,
) -> (tempfile::TempDir, PersistentStore) {
    let directory = tempfile::tempdir().expect("create claim directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let staging = store.replace_begin().expect("begin replacement");
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({ "pluginCustomStorage": values, "pluginStorageMeta": meta }),
        )
        .expect("stage imported plugin storage");
    store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("activate import");
    (directory, store)
}

/// Invariant 24. One window per import, opened once and never again.
#[test]
fn an_import_offers_each_plugin_one_claim_window_that_never_reopens() {
    let (_directory, mut store) = imported_store(
        json!({ "pm_store": { "apiKey": "imported" }, "other": "value" }),
        json!({}),
    );

    let session = store
        .begin_plugin_claim_session("provider-manager", "hash-one", "run-one")
        .expect("open claim session")
        .expect("a waiting import opens a window");
    assert_eq!(
        store
            .claim_plugin_storage_value(
                &session,
                "provider-manager",
                "hash-one",
                "run-one",
                "pm_store",
            )
            .expect("claim the unowned value"),
        Some(json!({ "apiKey": "imported" }))
    );
    assert_eq!(
        store
            .read_plugin_storage("provider-manager", "pm_store", None)
            .expect("read claimed row")
            .expect("claimed row exists")
            .value,
        json!({ "apiKey": "imported" })
    );
    assert!(store
        .read_plugin_storage(UNOWNED_OWNER, "pm_store", None)
        .expect("read unowned row")
        .is_none());

    let claimed = store
        .list_plugin_storage(None)
        .expect("list plugin storage")
        .into_iter()
        .find(|item| item.key == "pm_store")
        .expect("claimed item is listed");
    assert_eq!(claimed.claimed_from.as_deref(), Some("unowned"));
    assert!(claimed.assigned_at.is_some());
    assert!(claimed.import_batch_id.is_some());

    store
        .close_plugin_claim_session(&session)
        .expect("close the window");
    assert!(store
        .claim_plugin_storage_value(
            &session,
            "provider-manager",
            "hash-one",
            "run-one",
            "other",
        )
        .expect("claim after close")
        .is_none());
    // A later run of the same plugin gets no second window for this import.
    assert!(store
        .begin_plugin_claim_session("provider-manager", "hash-one", "run-two")
        .expect("reopen attempt")
        .is_none());
    assert!(store
        .read_plugin_storage(UNOWNED_OWNER, "other", None)
        .expect("read remaining unowned row")
        .is_some());
}

#[test]
fn a_claim_refuses_a_key_the_plugin_already_holds_and_a_foreign_caller() {
    let (_directory, mut store) =
        imported_store(json!({ "shared": "imported" }), json!({}));
    let revision = store.revision().expect("read revision");
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: "plugin-a".to_owned(),
                key: "shared".to_owned(),
                value: json!("own"),
            }]),
            ..empty_working_set_commit(revision)
        })
        .expect("write the plugin's own value");

    let session = store
        .begin_plugin_claim_session("plugin-a", "hash-one", "run-one")
        .expect("open claim session")
        .expect("a waiting import opens a window");
    assert!(store
        .claim_plugin_storage_value(&session, "plugin-a", "hash-one", "run-one", "shared")
        .expect("claim a held key")
        .is_none());
    assert_eq!(
        store
            .read_plugin_storage("plugin-a", "shared", None)
            .expect("read own row")
            .expect("own row exists")
            .value,
        json!("own")
    );
    assert!(store
        .read_plugin_storage(UNOWNED_OWNER, "shared", None)
        .expect("read unowned row")
        .is_some());

    // The window belongs to one plugin, one code and one run.
    for (owner, code_hash, runtime) in [
        ("plugin-b", "hash-one", "run-one"),
        ("plugin-a", "hash-two", "run-one"),
        ("plugin-a", "hash-one", "run-two"),
    ] {
        assert!(store
            .claim_plugin_storage_value(&session, owner, code_hash, runtime, "shared")
            .expect("claim from a foreign caller")
            .is_none());
    }
}

/// A full replacement written later must not mint a fresh window for values a
/// person deliberately left alone.
#[test]
fn a_later_full_replacement_keeps_the_import_a_waiting_value_arrived_in() {
    let (_directory, mut store) = imported_store(json!({ "waiting": "value" }), json!({}));
    let batch = store
        .list_plugin_storage(None)
        .expect("list plugin storage")
        .into_iter()
        .find(|item| item.key == "waiting")
        .expect("waiting item is listed")
        .import_batch_id
        .expect("waiting item carries its import");

    let session = store
        .begin_plugin_claim_session("plugin-a", "hash-one", "run-one")
        .expect("open claim session")
        .expect("a waiting import opens a window");
    store
        .close_plugin_claim_session(&session)
        .expect("close without claiming");

    let staging = store.replace_begin().expect("begin replacement");
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({ "pluginCustomStorage": { "waiting": "value" } }),
        )
        .expect("stage a full replacement");
    let revision = store.revision().expect("read revision");
    store
        .replace_commit(&staging.staging_id, Some(revision))
        .expect("activate the replacement");

    assert_eq!(
        store
            .list_plugin_storage(None)
            .expect("list plugin storage")
            .into_iter()
            .find(|item| item.key == "waiting")
            .expect("waiting item is still listed")
            .import_batch_id,
        Some(batch)
    );
    assert!(store
        .begin_plugin_claim_session("plugin-a", "hash-one", "run-three")
        .expect("reopen attempt after a replacement")
        .is_none());
}

#[test]
fn values_that_reached_the_store_outside_an_import_open_no_window() {
    let directory = tempfile::tempdir().expect("create claim directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let revision = store.revision().expect("read revision");
    store
        .commit(&WorkingSetCommit {
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: UNOWNED_OWNER.to_owned(),
                key: "stray".to_owned(),
                value: json!("value"),
            }]),
            ..empty_working_set_commit(revision)
        })
        .expect("write an unowned value outside an import");

    assert!(store
        .begin_plugin_claim_session("plugin-a", "hash-one", "run-one")
        .expect("open claim session")
        .is_none());
}
