use super::*;
use crate::data_health::repair::RepairCandidate;
use crate::persistent_store::{active_generation, AssetRepositoryAuthorityState};
use serde_json::json;

fn candidate(action: RepairAction) -> RepairCandidate {
    RepairCandidate {
        id: "0:test".to_owned(),
        action,
        finding: 0,
        preferred: true,
        discards: false,
    }
}

/// A library whose root points at an asset it does not register and at one it does, so a repair
/// has something real to remove and something it must leave alone.
fn fixture() -> (tempfile::TempDir, PersistentStore) {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let staging = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(
            &staging,
            &json!({
                "userIcon": "assets/missing.png",
                "customBackground": "assets/kept.png",
                "enabledModules": ["module-a", "module-b"],
            }),
        )
        .unwrap();
    store
        .replace_put_asset_repository_authority(
            &staging,
            &AssetRepositoryAuthorityState::V2 {
                migration_id: "fixture".to_owned(),
                compatibility_hash: "1a".repeat(32),
            },
        )
        .unwrap();
    store.replace_commit(&staging, Some(0)).unwrap();
    (directory, store)
}

fn root_of(store: &PersistentStore) -> Value {
    store.read_root(None).unwrap().value
}

#[test]
fn a_reference_removal_rewrites_only_the_field_it_names() {
    let (_directory, mut store) = fixture();
    let (committed, journal) = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.userIcon".to_owned(),
                occurrence: 0,
            })],
            10,
        )
        .unwrap();
    assert_eq!(committed.revision, 2);
    let root = root_of(&store);
    assert!(root.get("userIcon").is_none());
    assert_eq!(
        root.get("customBackground").and_then(Value::as_str),
        Some("assets/kept.png"),
        "a repair changes only what was selected"
    );
    assert_eq!(journal.from_revision, 1);
    assert_eq!(journal.to_revision, 2);
    assert_eq!(journal.records.len(), 1);
    assert_eq!(journal.records[0].table, "root");
}

#[test]
fn several_removals_in_one_record_keep_naming_the_same_elements() {
    let (_directory, mut store) = fixture();
    let owner = crate::data_health::Owner {
        kind: "root".to_owned(),
        id: "database".to_owned(),
    };
    store
        .apply_repair(
            1,
            &[
                candidate(RepairAction::DropReference {
                    owner: owner.clone(),
                    source_path: "$.enabledModules[0]".to_owned(),
                    occurrence: 0,
                }),
                RepairCandidate {
                    id: "1:test".to_owned(),
                    ..candidate(RepairAction::DropReference {
                        owner,
                        source_path: "$.enabledModules[1]".to_owned(),
                        occurrence: 1,
                    })
                },
            ],
            10,
        )
        .unwrap();
    assert_eq!(
        root_of(&store).get("enabledModules"),
        Some(&json!([])),
        "removing the later element first keeps the earlier index meaning what it meant"
    );
}

#[test]
fn an_undo_restores_the_previous_image_and_raises_the_revision() {
    let (_directory, mut store) = fixture();
    let before = root_of(&store);
    let (_, journal) = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.userIcon".to_owned(),
                occurrence: 0,
            })],
            10,
        )
        .unwrap();
    let (undone, skipped) = store.undo_repair(&journal, 2).unwrap();
    assert_eq!(undone.revision, 3);
    assert!(skipped.is_empty());
    assert_eq!(root_of(&store), before);
}

#[test]
fn an_undo_leaves_a_record_the_reader_changed_after_the_repair() {
    let (_directory, mut store) = fixture();
    let (_, journal) = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.userIcon".to_owned(),
                occurrence: 0,
            })],
            10,
        )
        .unwrap();
    store
        .commit(&crate::persistent_store::WorkingSetCommit {
            expected_revision: 2,
            root: Some(json!({ "customBackground": "assets/kept.png", "note": "edited" })),
            root_mutations: None,
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
        .unwrap();

    let (_, skipped) = store.undo_repair(&journal, 3).unwrap();
    assert_eq!(skipped, ["root:"], "the later edit is reported, not discarded");
    assert_eq!(
        root_of(&store).get("note").and_then(Value::as_str),
        Some("edited")
    );
}

#[test]
fn a_repair_onto_another_revision_is_refused_before_anything_is_staged() {
    let (_directory, mut store) = fixture();
    let error = store
        .apply_repair(
            0,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.userIcon".to_owned(),
                occurrence: 0,
            })],
            10,
        )
        .unwrap_err();
    assert!(matches!(error, StoreError::RevisionConflict { .. }), "{error:?}");
    assert_eq!(store.revision().unwrap(), 1, "nothing was applied");
    assert_eq!(
        root_of(&store).get("userIcon").and_then(Value::as_str),
        Some("assets/missing.png")
    );
}

#[test]
fn a_repair_that_leaves_the_library_refused_is_not_applied() {
    let (_directory, mut store) = fixture();
    let generation = active_generation(&store.connection).unwrap();
    // An alias whose payload is absent blocks activation, and this repair does not answer it.
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext)
             VALUES (?1, 'assets/absent.png', ?2, 'asset', 4, 'image/png', 'absent.png', 'png')",
            rusqlite::params![generation, "3c".repeat(32)],
        )
        .unwrap();

    let error = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.userIcon".to_owned(),
                occurrence: 0,
            })],
            10,
        )
        .unwrap_err();
    assert!(matches!(error, StoreError::Validation { .. }), "{error:?}");
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(
        active_generation(&store.connection).unwrap(),
        generation,
        "the live generation is untouched"
    );
    assert_eq!(
        root_of(&store).get("userIcon").and_then(Value::as_str),
        Some("assets/missing.png"),
        "the change the reader selected is not half applied"
    );
}

#[test]
fn a_removed_alias_releases_its_object_into_the_journal() {
    let (_directory, mut store) = fixture();
    let cas = crate::asset_repository::PayloadCas::new(store.repository_root()).unwrap();
    let stored = cas.prepare_bytes(b"synthetic").unwrap();
    let generation = active_generation(&store.connection).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext)
             VALUES (?1, 'assets/kept.png', ?2, 'asset', 9, 'image/png', 'kept.png', 'png')",
            rusqlite::params![generation, stored.content_hash],
        )
        .unwrap();

    let (_, journal) = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropAlias {
                kind: "asset".to_owned(),
                key: "assets/kept.png".to_owned(),
            })],
            10,
        )
        .unwrap();
    assert!(journal.released_objects.contains(&stored.content_hash));
    assert!(
        crate::data_health::journal::roots(store.repository_root())
            .unwrap()
            .object_hashes
            .is_empty(),
        "the journal only holds what is written to disk, which the command does"
    );
}

#[test]
fn a_path_no_record_uses_changes_nothing() {
    let (_directory, mut store) = fixture();
    let before = root_of(&store);
    let error = store
        .apply_repair(
            1,
            &[candidate(RepairAction::DropReference {
                owner: crate::data_health::Owner {
                    kind: "root".to_owned(),
                    id: "database".to_owned(),
                },
                source_path: "$.neverPresent".to_owned(),
                occurrence: 0,
            })],
            10,
        )
        .map(|(committed, _)| committed);
    // The staged copy is identical, so the activation still succeeds and the library is the same.
    assert!(error.is_ok(), "{error:?}");
    assert_eq!(root_of(&store), before);
}

#[test]
fn parse_path_reads_the_shapes_the_reference_graph_emits() {
    assert_eq!(parse_path("$.userIcon"), Some(vec![Step::Key("userIcon".to_owned())]));
    assert_eq!(
        parse_path("$.enabledModules[2]"),
        Some(vec![Step::Key("enabledModules".to_owned()), Step::Index(2)])
    );
    assert_eq!(
        parse_path("$.message[0].data"),
        Some(vec![
            Step::Key("message".to_owned()),
            Step::Index(0),
            Step::Key("data".to_owned()),
        ])
    );
    assert_eq!(parse_path("userIcon"), None);
    assert_eq!(parse_path("$.a[x]"), None);
}
